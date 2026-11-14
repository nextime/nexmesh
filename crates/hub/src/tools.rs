//! The team tool dispatcher. All tool logic lives here, in the hub, once.
//!
//! It is deliberately transport-agnostic: it takes a session, a slot and a tool
//! call, and returns text for the model. A `claude` process reaching it through
//! mcpd-over-stdio and a hermes agent reaching it over HTTP execute the same
//! code, so a non-CLI participant is a first-class team member rather than a
//! second-class guest.

use anyhow::{anyhow, bail, Result};
use nexmesh_proto::{now_ms, Envelope, MessageKind};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

pub const DEFAULT_LOCK_MS: i64 = 5 * 60 * 1000;

pub struct Ctx<'a> {
    pub db: &'a Connection,
    pub session_id: String,
    pub slot_id: String,
}

fn arg_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing required argument '{key}'"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Reviewers and QA approve their own completion: nobody reviews the reviewer.
fn is_reviewer(role: Option<&str>) -> bool {
    role.map(|r| {
        let r = r.to_lowercase();
        r.contains("review") || r.contains("qa") || r.contains("test")
    })
    .unwrap_or(false)
}

struct Peer {
    id: String,
    name: String,
    role: String,
    capabilities: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn my_label(&self) -> String {
        self.label_of(&self.slot_id)
    }

    fn label_of(&self, slot_id: &str) -> String {
        self.db
            .query_row(
                "SELECT COALESCE(display_name, role, 'slot-' || id) FROM slots WHERE id = ?",
                [slot_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| slot_id.to_string())
    }

    fn my_role(&self) -> Option<String> {
        self.db
            .query_row("SELECT role FROM slots WHERE id = ?", [&self.slot_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten()
    }

    /// Every teammate in this session but me, with what they answer to.
    fn peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self.db.prepare(
            // An adopted terminal is a window onto a seat, not a teammate.
            // It holds no model, answers nothing, and acks nothing -- and it
            // was being listed as a peer, so an agent looking for somebody
            // to hand work to found `term-Max` sitting there as if it could
            // take it. Whatever is IN that terminal has a seat of its own,
            // and that seat is listed.
            "SELECT id, LOWER(COALESCE(display_name, '')), LOWER(COALESCE(role, '')), capabilities
             FROM slots WHERE session_id = ? AND id != ?
               AND COALESCE(cli_id,'') <> 'tmux' AND tmux_target IS NULL",
        )?;
        let rows = stmt
            .query_map(params![self.session_id, self.slot_id], |r| {
                let caps: String = r.get(3)?;
                Ok(Peer {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    capabilities: serde_json::from_str(&caps).unwrap_or_default(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Find a teammate by display name, role, or capability label,
    /// case-insensitively. Ambiguity is an error rather than a silent pick --
    /// sending review feedback to the wrong agent is worse than a retry.
    ///
    /// Capability comes last on purpose. A label like "strong" is usually held
    /// by several teammates, so it is the right way to ask `list_peers` who
    /// can do something and the wrong way to address one of them; matching it
    /// here is a convenience for the case where exactly one seat has it.
    fn resolve_peer(&self, needle: &str) -> Result<String> {
        let n = needle.trim().to_lowercase();
        let rows = self.peers()?;

        let exact: Vec<&Peer> = rows.iter().filter(|p| p.name == n || p.role == n).collect();
        if exact.len() == 1 {
            return Ok(exact[0].id.clone());
        }
        if exact.len() > 1 {
            bail!(
                "'{needle}' matches {} teammates; use a unique name",
                exact.len()
            );
        }
        let by_cap: Vec<&Peer> =
            rows.iter().filter(|p| p.capabilities.iter().any(|c| c == &n)).collect();
        if by_cap.len() == 1 {
            return Ok(by_cap[0].id.clone());
        }
        if by_cap.len() > 1 {
            bail!(
                "'{needle}' is a capability held by {} teammates; name one of them, or use \
                 list_peers with capability='{needle}' to choose",
                by_cap.len()
            );
        }
        let partial: Vec<&Peer> = rows
            .iter()
            .filter(|p| p.name.contains(&n) || p.role.contains(&n))
            .collect();
        match partial.len() {
            1 => Ok(partial[0].id.clone()),
            0 => bail!("no teammate matching '{needle}'"),
            k => bail!("'{needle}' is ambiguous ({k} matches); use a unique name"),
        }
    }

    /// A lock's arbitration domain follows the slot's workspace: work on one
    /// machine's own files is node-scoped, a shared tree is workspace-scoped.
    /// Without this, two agents editing their own local `src/main.rs` on
    /// different machines would fight over one lock.
    /// Which lock domain this agent's files live in.
    ///
    /// Deliberately NOT the session: a lock is about a file, and two agents
    /// editing one file must contend whichever teams they are on. So the
    /// domain is the thing being edited -- a shared repo, or a directory on
    /// one machine -- and `acquire_file` matches on it without filtering by
    /// session.
    ///
    /// For a node_local workspace the domain is the machine AND THE MOUNT
    /// PATH. The machine alone was not enough: it made one domain of every
    /// local workspace on a host, so an agent locking `src/main.rs` under
    /// /AI/sentenze blocked another locking `src/main.rs` under
    /// /storage/coderai -- same relative path, different trees, a denial
    /// naming an agent in a project you were not touching.
    fn lock_scope(&self) -> Result<String> {
        let row: Option<(Option<String>, Option<String>, Option<String>, Option<String>)> = self
            .db
            .query_row(
                "SELECT w.kind, w.id, w.node_id,
                        (SELECT m.path FROM workspace_mounts m
                          WHERE m.workspace_id = w.id
                            AND (m.node_id = w.node_id OR w.node_id IS NULL)
                          ORDER BY m.path LIMIT 1)
                 FROM slots s
                 LEFT JOIN workspaces w ON w.id = s.workspace_id WHERE s.id = ?",
                [&self.slot_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        match row {
            Some((Some(kind), Some(ws_id), node_id, mount)) => match kind.as_str() {
                "node_local" => {
                    let node = node_id.unwrap_or_else(|| ws_id.clone());
                    // A workspace with no mount recorded keeps the old domain
                    // rather than inventing one: narrower than the truth
                    // would let two agents edit one file believing they each
                    // held it.
                    Ok(match mount {
                        Some(p) => format!("node:{node}:{}", p.trim_end_matches('/')),
                        None => format!("node:{node}"),
                    })
                }
                _ => Ok(format!("ws:{ws_id}")),
            },
            // No workspace: fall back to a session-wide domain so locks still
            // arbitrate rather than silently never conflicting.
            _ => Ok(format!("session:{}", self.session_id)),
        }
    }

    fn insert_message(&self, to_slot: Option<&str>, kind: MessageKind, body: &str) -> Result<i64> {
        self.db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                self.session_id,
                self.slot_id,
                to_slot,
                serde_json::to_value(kind)?.as_str().unwrap_or("text"),
                body,
                now_ms()
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    fn touch(&self) {
        let _ = self.db.execute(
            "UPDATE sessions SET last_active_ms = ? WHERE id = ?",
            params![now_ms(), self.session_id],
        );
    }

    fn set_state(&self, slot_id: &str, state: &str) -> Result<()> {
        self.db.execute(
            "UPDATE slots SET task_state = ? WHERE id = ?",
            params![state, slot_id],
        )?;
        Ok(())
    }
}

/// Messages waiting for a slot, excluding any already in flight to a live
/// agent. Used by every transport to piggyback delivery on a tool result.
/// What this seat has been sent and has not yet dealt with.
///
/// Distinct from `pending_for_slot`, which is about DELIVERY: that one finds
/// messages still to be handed over, and hands each over once. This one is
/// about ATTENTION, and keeps showing a message until somebody claims it.
/// Checking used to ack, so a glance was indistinguishable from an answer
/// and the message was gone; an operator watching the fleet had no way to
/// tell a message that had been acted on from one that had merely scrolled
/// past an agent.
pub fn unclaimed_for_slot(db: &Connection, session_id: &str, slot_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = db.prepare(
        "SELECT m.id, m.from_slot,
                COALESCE(s.display_name, s.role, 'orchestrator'),
                m.to_slot, m.kind, m.body, m.sent_at_ms
         FROM messages m LEFT JOIN slots s ON s.id = m.from_slot
         WHERE m.session_id = ?1
           AND (m.to_slot = ?2 OR m.to_slot IS NULL)
           AND m.from_slot IS NOT ?2
           AND m.claimed_at_ms IS NULL
         ORDER BY m.id ASC LIMIT 50",
    )?;
    let out = stmt
        .query_map(params![session_id, slot_id], |r| {
            Ok(Envelope {
                id: r.get(0)?,
                from_slot: r.get(1)?,
                from_name: r.get(2)?,
                to_slot: r.get(3)?,
                kind: serde_json::from_value(Value::String(r.get::<_, String>(4)?))
                    .unwrap_or(MessageKind::Text),
                body: r.get(5)?,
                sent_at_ms: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Mark messages dealt with. The only thing that clears them.
pub fn claim_messages(db: &Connection, who: &str, ids: &[i64]) -> Result<usize> {
    let now = now_ms();
    let mut n = 0;
    for id in ids {
        n += db.execute(
            "UPDATE messages SET claimed_at_ms = ?, claimed_by = ?
             WHERE id = ? AND claimed_at_ms IS NULL",
            params![now, who, id],
        )?;
    }
    Ok(n)
}

pub fn pending_for_slot(db: &Connection, session_id: &str, slot_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = db.prepare(
        "SELECT m.id, m.from_slot,
                COALESCE(s.display_name, s.role, 'orchestrator'),
                m.to_slot, m.kind, m.body, m.sent_at_ms
         FROM messages m LEFT JOIN slots s ON s.id = m.from_slot
         WHERE m.session_id = ?1
           AND m.from_slot IS NOT ?2
           -- Addressed to me: the message's own state is the whole story.
           -- Addressed to the team: MY copy is, because one teammate acking
           -- must not take it from the rest.
           AND ((m.to_slot = ?2 AND m.acked_at_ms IS NULL AND m.inflight_at_ms IS NULL)
                OR (m.to_slot IS NULL AND NOT EXISTS (
                      SELECT 1 FROM message_recipients r
                       WHERE r.message_id = m.id AND r.slot_id = ?2
                         AND (r.acked_at_ms IS NOT NULL OR r.inflight_at_ms IS NOT NULL))))
         ORDER BY m.id ASC LIMIT 50",
    )?;
    let out = stmt
        .query_map(params![session_id, slot_id], |r| {
            Ok(Envelope {
                id: r.get(0)?,
                from_slot: r.get(1)?,
                from_name: r.get(2)?,
                to_slot: r.get(3)?,
                kind: serde_json::from_value(Value::String(r.get::<_, String>(4)?))
                    .unwrap_or(MessageKind::Text),
                body: r.get(5)?,
                sent_at_ms: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Mark messages handed to a SEAT but not yet confirmed. An in-flight
/// message is never handed to that seat twice, and `reclaim_stale_inflight`
/// frees it if the agent dies before acking.
///
/// Per seat, not per message, because a broadcast has several recipients and
/// one of them taking it must not take it from the others. The message-level
/// columns are kept alongside, because they are what the log and the
/// dashboard show: "in flight" there means somebody has it, which is still
/// the useful thing to read.
pub fn mark_inflight(db: &Connection, agent_id: &str, slot_id: &str, ids: &[i64]) -> Result<()> {
    let now = now_ms();
    for id in ids {
        db.execute(
            "INSERT INTO message_recipients (message_id, slot_id, inflight_agent, inflight_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(message_id, slot_id) DO UPDATE SET
                inflight_agent = ?3, inflight_at_ms = ?4",
            params![id, slot_id, agent_id, now],
        )?;
        db.execute(
            "UPDATE messages SET inflight_agent = ?, inflight_at_ms = ? WHERE id = ?",
            params![agent_id, now, id],
        )?;
    }
    Ok(())
}

/// The seat an agent is sitting in, which is who a message was really for.
fn slot_of(db: &Connection, agent_id: &str) -> Option<String> {
    db.query_row("SELECT slot_id FROM agents WHERE id = ?", [agent_id], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

/// A message reached a model. Recorded for that SEAT.
///
/// This used to set one `acked_at_ms` on the message and be done, which
/// meant the first agent to answer a broadcast consumed it for the whole
/// team: everybody else's copy vanished unread. "I told the team and only
/// one of them knew" is a hard thing to notice and a worse thing to debug,
/// and a capability bridge -- which is handed team chat it cannot answer --
/// made it happen every time.
///
/// The message-level columns still record the FIRST ack, because that is
/// what "delivered" means in the log and on the dashboard.
pub fn ack_messages(db: &Connection, agent_id: &str, ids: &[i64]) -> Result<()> {
    let now = now_ms();
    let slot = slot_of(db, agent_id);
    for id in ids {
        // The seat that was HANDED it, which is not always the seat of the
        // agent reporting the ack.
        //
        // A message typed at an adopted terminal is written into the
        // TERMINAL's pty, by the terminal's agent, on behalf of the seat
        // whose CLI sits in that pane. `mark_inflight` recorded that seat
        // correctly; the ack used to be recorded against the terminal's own
        // seat instead -- so the recipient's copy stayed unacked for ever and
        // the hub typed the same message at it on every sweep. Hours-old
        // notices were still being typed at three agents when this was found,
        // and one of them reported it as a "stuck emitter", which is exactly
        // what it looked like from in there.
        //
        // So: ack the row this agent is holding, whoever it belongs to.
        let held = db.execute(
            "UPDATE message_recipients
                SET acked_at_ms = ?1, acked_by_agent = ?2,
                    inflight_agent = NULL, inflight_at_ms = NULL
              WHERE message_id = ?3 AND inflight_agent = ?2 AND acked_at_ms IS NULL",
            params![now, agent_id, id],
        )?;
        if held == 0 {
            if let Some(slot) = &slot {
                db.execute(
                    "INSERT INTO message_recipients
                        (message_id, slot_id, acked_at_ms, acked_by_agent)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(message_id, slot_id) DO UPDATE SET
                        acked_at_ms = ?3, acked_by_agent = ?4,
                        inflight_agent = NULL, inflight_at_ms = NULL",
                    params![id, slot, now, agent_id],
                )?;
            }
        }
        db.execute(
            "UPDATE messages SET acked_at_ms = ?, acked_by_agent = ?, inflight_agent = NULL,
                                 inflight_at_ms = NULL
             WHERE id = ? AND acked_at_ms IS NULL",
            params![now, agent_id, id],
        )?;
    }
    Ok(())
}

/// A message handed to an agent that then died must go back in the queue, or
/// it is lost forever. This is the whole point of acking separately.
///
/// `older_than_ms` is inclusive -- "in flight for at least this long" -- so
/// passing 0 reclaims everything outstanding, which is what a node going
/// offline needs.
pub fn reclaim_stale_inflight(db: &Connection, older_than_ms: i64) -> Result<usize> {
    let cutoff = now_ms() - older_than_ms;
    db.execute(
        "UPDATE message_recipients SET inflight_agent = NULL, inflight_at_ms = NULL
         WHERE acked_at_ms IS NULL AND inflight_at_ms IS NOT NULL AND inflight_at_ms <= ?",
        [cutoff],
    )?;
    Ok(db.execute(
        "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
         WHERE acked_at_ms IS NULL AND inflight_at_ms IS NOT NULL AND inflight_at_ms <= ?",
        [cutoff],
    )?)
}

// ------------------------------------------------------------- dispatch ----

pub fn dispatch(ctx: &Ctx, tool: &str, args: &Value) -> Result<String> {
    ctx.touch();
    match tool {
        "list_peers" => list_peers(ctx, args),
        "send_message" => send_message(ctx, args),
        "broadcast" => broadcast(ctx, args),
        "check_messages" => check_messages(ctx),
        "claim_messages" => claim_messages_tool(ctx, args),
        "set_summary" => set_summary(ctx, args),
        "check_team_status" => check_team_status(ctx),
        "get_plan" => get_plan(ctx),
        "update_plan" => update_plan(ctx, args),
        "signal_done" => signal_done(ctx, args),
        "escalate" => escalate(ctx, args),
        "notify_operator" => notify_operator(ctx, args),
        "submit_feedback" => submit_feedback(ctx, args),
        "approve" => approve(ctx, args),
        "acquire_file" => acquire_file(ctx, args),
        "release_file" => release_file(ctx, args),
        "view_file_locks" => view_file_locks(ctx),
        "store_knowledge" => store_knowledge(ctx, args),
        "query_knowledge" => query_knowledge(ctx, args),
        "store_memory" => store_memory(ctx, args),
        "recall" => recall(ctx, args),
        "forget" => forget_memory(ctx, args),
        "memory_history" => memory_history(ctx, args),
        "save_context" => save_context(ctx, args),
        "pending_asks" => pending_asks(ctx),
        "answer_ask" => answer_ask(ctx, args),
        "get_history" => get_history(ctx, args),
        other => bail!("unknown tool '{other}'"),
    }
}

struct PeerRow {
    name: String,
    role: String,
    state: String,
    summary: String,
    kind: String,
    host: String,
    status: String,
    capabilities: Vec<String>,
    /// What the agent reported, falling back to what the seat asked for. A
    /// reported model is the truth; a requested one is a hope.
    model: Option<String>,
}

fn list_peers(ctx: &Ctx, args: &Value) -> Result<String> {
    let role_filter = opt_str(args, "role");
    let cap_filter = opt_str(args, "capability").map(|c| c.trim().to_lowercase());
    let mut stmt = ctx.db.prepare(
        "SELECT COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.task_state, s.summary, s.participant_kind,
                COALESCE(n.name, CASE WHEN a.provenance = 'external' THEN 'external' ELSE '-' END),
                COALESCE(a.status, 'disconnected'), s.capabilities, a.model, s.model
         FROM slots s
         LEFT JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE s.session_id = ? AND s.id != ?
         ORDER BY 1",
    )?;
    let rows: Vec<PeerRow> = stmt
        .query_map(params![ctx.session_id, ctx.slot_id], |r| {
            let caps: String = r.get(7)?;
            let reported: Option<String> = r.get(8)?;
            let requested: Option<String> = r.get(9)?;
            Ok(PeerRow {
                name: r.get(0)?,
                role: r.get(1)?,
                state: r.get(2)?,
                summary: r.get(3)?,
                kind: r.get(4)?,
                host: r.get(5)?,
                status: r.get(6)?,
                capabilities: serde_json::from_str(&caps).unwrap_or_default(),
                model: reported.or(requested),
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    let rows: Vec<_> = rows
        .into_iter()
        .filter(|p| match &role_filter {
            Some(f) => p.role.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        })
        .filter(|p| match &cap_filter {
            // Exact on a label: "strong" must not also match "strongish".
            Some(f) => p.capabilities.iter().any(|c| c == f),
            None => true,
        })
        .collect();

    if rows.is_empty() {
        return Ok(match (&role_filter, &cap_filter) {
            (_, Some(c)) => format!("No teammate has the capability '{c}'."),
            (Some(r), _) => format!("No teammate has the role '{r}'."),
            _ => "No teammates yet.".into(),
        });
    }
    let mut out = format!("{} teammate(s):\n", rows.len());
    for p in rows {
        out.push_str(&format!(
            "- {} [{}] on {} ({}, {}) state={}",
            p.name, p.role, p.host, p.kind, p.status, p.state
        ));
        if let Some(m) = &p.model {
            out.push_str(&format!(" model={m}"));
        }
        if !p.capabilities.is_empty() {
            out.push_str(&format!(" can={}", p.capabilities.join(",")));
        }
        out.push('\n');
        if !p.summary.is_empty() {
            out.push_str(&format!("    {}\n", p.summary));
        }
    }
    Ok(out)
}

fn send_message(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "to")?;
    let text = arg_str(args, "text")?;
    let target = ctx.resolve_peer(&to)?;
    ctx.insert_message(Some(&target), MessageKind::Text, &text)?;
    Ok(format!("Sent to {}.", ctx.label_of(&target)))
}

fn broadcast(ctx: &Ctx, args: &Value) -> Result<String> {
    let text = arg_str(args, "text")?;
    ctx.insert_message(None, MessageKind::Text, &text)?;
    Ok("Broadcast to the team.".into())
}

fn check_messages(ctx: &Ctx) -> Result<String> {
    let msgs = unclaimed_for_slot(ctx.db, &ctx.session_id, &ctx.slot_id)?;
    if msgs.is_empty() {
        return Ok("Nothing waiting on you.".into());
    }
    // Reading is not answering. This used to ack everything it showed, so a
    // message glanced at was a message gone -- and nobody, agent or
    // operator, could tell the difference afterwards.
    let mut out = format!(
        "{} message(s) waiting on you. They stay here until you claim them \
         with claim_messages:\n",
        msgs.len()
    );
    for m in msgs {
        out.push_str(&format!(
            "- [{}] from {}: {}\n",
            m.id,
            m.from_name.unwrap_or_else(|| "orchestrator".into()),
            m.body
        ));
    }
    Ok(out)
}

/// Say you have dealt with what you were sent.
fn claim_messages_tool(ctx: &Ctx, args: &Value) -> Result<String> {
    let waiting = unclaimed_for_slot(ctx.db, &ctx.session_id, &ctx.slot_id)?;
    if waiting.is_empty() {
        return Ok("Nothing was waiting on you.".into());
    }
    let ids: Vec<i64> = match args.get("ids").and_then(|v| v.as_array()) {
        Some(a) => {
            let wanted: Vec<i64> = a.iter().filter_map(|v| v.as_i64()).collect();
            // Only ever your own: an id from another seat's inbox is not
            // yours to close.
            waiting.iter().map(|m| m.id).filter(|id| wanted.contains(id)).collect()
        }
        None => waiting.iter().map(|m| m.id).collect(),
    };
    if ids.is_empty() {
        bail!("none of those ids are waiting on you; check_messages lists what is");
    }
    let n = claim_messages(ctx.db, &ctx.my_label(), &ids)?;
    Ok(format!("Claimed {n} message(s). They will not be shown to you again."))
}

fn set_summary(ctx: &Ctx, args: &Value) -> Result<String> {
    let summary = arg_str(args, "summary")?;
    ctx.db.execute(
        "UPDATE slots SET summary = ? WHERE id = ?",
        params![summary, ctx.slot_id],
    )?;
    ctx.db.execute(
        "UPDATE agents SET last_seen_ms = ? WHERE slot_id = ? AND exited_at_ms IS NULL",
        params![now_ms(), ctx.slot_id],
    )?;
    // First sign of life moves a slot out of idle.
    ctx.db.execute(
        "UPDATE slots SET task_state = 'working' WHERE id = ? AND task_state = 'idle'",
        [&ctx.slot_id],
    )?;
    Ok("Summary updated.".into())
}

fn check_team_status(ctx: &Ctx) -> Result<String> {
    let mut stmt = ctx.db.prepare(
        "SELECT COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.task_state, s.summary,
                COALESCE(n.name, CASE WHEN a.provenance = 'external' THEN 'external' ELSE '-' END),
                s.id
         FROM slots s
         LEFT JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE s.session_id = ? ORDER BY 1",
    )?;
    let rows: Vec<(String, String, String, String, String, String)> = stmt
        .query_map([&ctx.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = String::from("Team:\n");
    for (name, role, state, summary, host, id) in rows {
        let me = if id == ctx.slot_id { "  <- you" } else { "" };
        out.push_str(&format!("- {name} [{role}] on {host}: {state}{me}\n"));
        if !summary.is_empty() {
            out.push_str(&format!("    {summary}\n"));
        }
    }
    Ok(out)
}

fn get_plan(ctx: &Ctx) -> Result<String> {
    let mut stmt = ctx.db.prepare(
        "SELECT p.ord, p.label, p.status, COALESCE(s.display_name, s.role, '')
         FROM plan_items p LEFT JOIN slots s ON s.id = p.assignee_slot
         WHERE p.session_id = ? ORDER BY p.ord",
    )?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map([&ctx.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        return Ok("No plan has been set.".into());
    }
    let done = rows.iter().filter(|(_, _, s, _)| s == "done").count();
    let mut out = format!("Plan ({done}/{} done):\n", rows.len());
    for (ord, label, status, who) in rows {
        let who = if who.is_empty() { String::new() } else { format!(" ({who})") };
        out.push_str(&format!("{ord}. [{status}] {label}{who}\n"));
    }
    Ok(out)
}

fn update_plan(ctx: &Ctx, args: &Value) -> Result<String> {
    let item = arg_str(args, "item")?;
    let status = arg_str(args, "status")?;
    if !["pending", "in_progress", "done", "blocked"].contains(&status.as_str()) {
        bail!("status must be pending, in_progress, done or blocked");
    }
    // Accept an index or a label substring.
    let changed = if let Ok(ord) = item.parse::<i64>() {
        ctx.db.execute(
            "UPDATE plan_items SET status = ?, updated_at_ms = ? WHERE session_id = ? AND ord = ?",
            params![status, now_ms(), ctx.session_id, ord],
        )?
    } else {
        ctx.db.execute(
            "UPDATE plan_items SET status = ?, updated_at_ms = ?
             WHERE session_id = ? AND LOWER(label) LIKE '%' || LOWER(?) || '%'",
            params![status, now_ms(), ctx.session_id, item],
        )?
    };
    if changed == 0 {
        bail!("no plan item matching '{item}'");
    }
    Ok(format!("{changed} plan item(s) set to {status}."))
}

fn signal_done(ctx: &Ctx, args: &Value) -> Result<String> {
    let notes = opt_str(args, "notes").unwrap_or_default();
    let role = ctx.my_role();
    let me = ctx.my_label();
    if is_reviewer(role.as_deref()) {
        ctx.set_state(&ctx.slot_id, "approved")?;
        ctx.insert_message(
            None,
            MessageKind::System,
            &format!("{me} (reviewer) finished. {notes}"),
        )?;
        return Ok("Marked approved: reviewer roles do not need external review.".into());
    }
    ctx.set_state(&ctx.slot_id, "done_pending_review")?;
    ctx.insert_message(
        None,
        MessageKind::Handoff,
        &format!("{me} signalled done and is awaiting review. {notes}"),
    )?;
    Ok("Marked done, pending review. A reviewer will approve or send feedback.".into())
}

/// Hand a task up to a teammate better equipped for it.
///
/// The mirror of `submit_feedback`, which hands work BACK down for rework.
/// Both exist because the alternative is an agent that is stuck and silent:
/// with a mixed team, the cheap seat hitting its limit is the normal case, not
/// an error, and it needs a move that is neither "done" nor "failed".
///
/// It is deliberately not a scheduler. The hub does not decide who is more
/// capable -- the agent asks for a capability and the hub routes it, the same
/// as every other message here.
fn escalate(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "to")?;
    let reason = arg_str(args, "reason")?;
    let target = ctx.resolve_peer(&to)?;
    if target == ctx.slot_id {
        bail!("you cannot escalate to yourself");
    }
    let me = ctx.my_label();
    // Their task: what the escalator named, or whatever this seat was doing.
    let task = match opt_str(args, "task") {
        Some(t) => t,
        None => ctx
            .db
            .query_row(
                "SELECT COALESCE(NULLIF(summary, ''), COALESCE(role_description, ''))
                 FROM slots WHERE id = ?",
                [&ctx.slot_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default(),
    };
    let body = if task.trim().is_empty() {
        format!("Escalated from {me}. Reason: {reason}")
    } else {
        format!("Escalated from {me}. Reason: {reason}\n\nTask: {task}")
    };
    ctx.insert_message(Some(&target), MessageKind::Handoff, &body)?;
    // Both ends move: the escalator is no longer the one to chase, and the
    // receiver is working. Leaving either state alone is how a task gets
    // silently dropped between two agents that each think the other has it.
    ctx.set_state(&ctx.slot_id, "escalated")?;
    ctx.set_state(&target, "working")?;
    let them = ctx.label_of(&target);
    tracing::info!(from = %me, to = %them, "task escalated");
    Ok(format!(
        "Escalated to {them}. Your state is 'escalated' and theirs is 'working'; they have \
         your reason and the task. Do not keep working on it."
    ))
}

/// How long an orchestrator may be silent before it counts as absent.
///
/// A driving orchestrator talks to the hub constantly -- resolving a session,
/// reading status, delivering a message -- so silence this long means nobody
/// is holding the wheel.
const ORCHESTRATOR_SILENT_MS: i64 = 10 * 60 * 1000;

/// Raise something with whoever is driving this team.
///
/// The team's own orchestrator first, always: it knows the work, and an agent
/// that goes over its head for every question makes the scoping pointless.
/// The supervisors get it only when this session has no orchestrator of its
/// own, when the one it has has gone quiet, or when the agent says the
/// orchestrator is itself the problem.
///
/// Note the asymmetry this closes: a message from an operator is pushed into
/// an agent's next tool result, while an agent's signals -- summaries, review
/// requests -- sat until somebody looked. This is the agent's way to make
/// something arrive.
fn notify_operator(ctx: &Ctx, args: &Value) -> Result<String> {
    let text = arg_str(args, "text")?;
    let escalate = args.get("escalate").and_then(|v| v.as_bool()).unwrap_or(false);
    let me = ctx.my_label();
    let now = now_ms();

    // Who, if anyone, drives this session: a live token scoped to it.
    let own: Option<(String, Option<i64>)> = ctx
        .db
        .query_row(
            "SELECT label, last_used_ms FROM operator_tokens
              WHERE session_id = ? AND revoked_at_ms IS NULL
              ORDER BY COALESCE(last_used_ms, 0) DESC LIMIT 1",
            [&ctx.session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let (to_session, why) = match (&own, escalate) {
        (_, true) => (None, "you asked for the supervisors directly".to_string()),
        (None, _) => (None, "this session has no orchestrator of its own".to_string()),
        (Some((label, last)), _) => {
            let silent = now - last.unwrap_or(0) > ORCHESTRATOR_SILENT_MS;
            if silent {
                (
                    None,
                    format!("'{label}' drives this session but has not been heard from in a while"),
                )
            } else {
                (Some(ctx.session_id.clone()), format!("'{label}' drives this session"))
            }
        }
    };

    ctx.db.execute(
        "INSERT INTO operator_messages
            (from_label, from_kind, from_session, to_session, body, sent_at_ms)
         VALUES (?, 'agent', ?, ?, ?, ?)",
        params![me, ctx.session_id, to_session, text, now],
    )?;

    Ok(match to_session {
        Some(_) => format!(
            "Raised with this session's orchestrator ({why}). Watch for a reply with \
             check_messages, and carry on with anything that does not depend on it."
        ),
        None => format!(
            "Raised with the supervisors who can see the whole fleet ({why}). Watch for a \
             reply with check_messages, and carry on with anything that does not depend on it."
        ),
    })
}

fn submit_feedback(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "to")?;
    let text = arg_str(args, "text")?;
    let actionable = args
        .get("actionable")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow!("missing required argument 'actionable'"))?;
    let target = ctx.resolve_peer(&to)?;
    let me = ctx.my_label();
    ctx.insert_message(
        Some(&target),
        MessageKind::Feedback,
        &format!("Review feedback from {me}{}: {text}",
            if actionable { " (ACTION REQUIRED)" } else { " (informational)" }),
    )?;
    if actionable {
        ctx.set_state(&target, "addressing_feedback")?;
        Ok(format!("Actionable feedback sent to {}; they are now addressing it.", ctx.label_of(&target)))
    } else {
        Ok(format!("Informational feedback sent to {}.", ctx.label_of(&target)))
    }
}

fn approve(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "to")?;
    let notes = opt_str(args, "notes").unwrap_or_default();
    let target = ctx.resolve_peer(&to)?;
    let state: String = ctx
        .db
        .query_row("SELECT task_state FROM slots WHERE id = ?", [&target], |r| r.get(0))
        .unwrap_or_else(|_| "idle".into());
    if state == "idle" || state == "working" {
        bail!(
            "{} has not signalled done yet (state: {state}); nothing to approve",
            ctx.label_of(&target)
        );
    }
    ctx.set_state(&target, "approved")?;
    let me = ctx.my_label();
    ctx.insert_message(
        Some(&target),
        MessageKind::Approval,
        &format!("{me} approved your work. {notes}"),
    )?;
    Ok(format!("Approved {}.", ctx.label_of(&target)))
}

fn acquire_file(ctx: &Ctx, args: &Value) -> Result<String> {
    let path = arg_str(args, "path")?;
    let purpose = opt_str(args, "purpose").unwrap_or_default();
    let scope = ctx.lock_scope()?;
    let now = now_ms();

    // Expire stale locks first: an agent that crashed holding a lock must not
    // block the team forever.
    ctx.db.execute("DELETE FROM locks WHERE expires_at_ms < ?", [now])?;

    let holder: Option<String> = ctx
        .db
        .query_row(
            "SELECT held_by_slot FROM locks WHERE scope = ? AND path = ?",
            params![scope, path],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(h) = holder {
        if h == ctx.slot_id {
            ctx.db.execute(
                "UPDATE locks SET expires_at_ms = ? WHERE scope = ? AND path = ?",
                params![now + DEFAULT_LOCK_MS, scope, path],
            )?;
            return Ok(format!("You already hold {path}; lock extended."));
        }
        return Ok(format!(
            "DENIED: {path} is locked by {}. Coordinate with them or work elsewhere.",
            ctx.label_of(&h)
        ));
    }
    ctx.db.execute(
        "INSERT INTO locks (session_id, scope, path, held_by_slot, purpose, acquired_at_ms, expires_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![ctx.session_id, scope, path, ctx.slot_id, purpose, now, now + DEFAULT_LOCK_MS],
    )?;
    Ok(format!("Locked {path} for 5 minutes."))
}

fn release_file(ctx: &Ctx, args: &Value) -> Result<String> {
    let path = arg_str(args, "path")?;
    let scope = ctx.lock_scope()?;
    let n = ctx.db.execute(
        "DELETE FROM locks WHERE scope = ? AND path = ? AND held_by_slot = ?",
        params![scope, path, ctx.slot_id],
    )?;
    if n == 0 {
        bail!("you do not hold a lock on {path}");
    }
    Ok(format!("Released {path}."))
}

fn view_file_locks(ctx: &Ctx) -> Result<String> {
    let scope = ctx.lock_scope()?;
    let mut stmt = ctx.db.prepare(
        "SELECT l.path, COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(l.purpose, '')
         FROM locks l JOIN slots s ON s.id = l.held_by_slot
         WHERE l.scope = ? AND l.expires_at_ms > ? ORDER BY l.path",
    )?;
    let locks: Vec<(String, String, String)> = stmt
        .query_map(params![scope, now_ms()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut stmt2 = ctx.db.prepare(
        "SELECT o.pattern, COALESCE(s.display_name, s.role, 'slot-' || s.id)
         FROM ownership o JOIN slots s ON s.id = o.slot_id
         WHERE o.workspace_id = (SELECT workspace_id FROM slots WHERE id = ?) ORDER BY o.pattern",
    )?;
    let zones: Vec<(String, String)> = stmt2
        .query_map([&ctx.slot_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut out = format!("Lock domain: {scope}\n");
    if locks.is_empty() {
        out.push_str("No active locks.\n");
    } else {
        out.push_str("Active locks:\n");
        for (p, who, why) in locks {
            out.push_str(&format!("- {p} held by {who} {why}\n"));
        }
    }
    if !zones.is_empty() {
        out.push_str("Ownership zones:\n");
        for (pat, who) in zones {
            out.push_str(&format!("- {pat} -> {who}\n"));
        }
    }
    Ok(out)
}

/// The original two tools, now a session-scoped path through `memory`.
///
/// Kept because agents, prompts and descriptors all name them, and a tool
/// that disappears is a broken team rather than a migration.
fn store_knowledge(ctx: &Ctx, args: &Value) -> Result<String> {
    let scope = crate::memory::Scope::Session(ctx.session_id.to_string());
    crate::memory::store(
        ctx.db,
        &ctx.slot_id,
        &scope,
        &arg_str(args, "key")?,
        &arg_str(args, "value")?,
        &opt_str(args, "category").unwrap_or_else(|| "context".into()),
        None,
    )
}

fn query_knowledge(ctx: &Ctx, args: &Value) -> Result<String> {
    render_entries(ctx, crate::memory::recall(
        ctx.db,
        &ctx.slot_id,
        &ctx.session_id,
        opt_str(args, "key").as_deref(),
        None,
        opt_str(args, "category").as_deref(),
        Some("session"),
        100,
    )?)
}

fn scope_of(ctx: &Ctx, args: &Value) -> Result<crate::memory::Scope> {
    crate::memory::Scope::parse(
        opt_str(args, "scope").as_deref(),
        opt_str(args, "group").as_deref(),
        &ctx.slot_id,
        &ctx.session_id,
    )
}

fn store_memory(ctx: &Ctx, args: &Value) -> Result<String> {
    let scope = scope_of(ctx, args)?;
    crate::memory::store(
        ctx.db,
        &ctx.slot_id,
        &scope,
        &arg_str(args, "key")?,
        &arg_str(args, "value")?,
        &opt_str(args, "category").unwrap_or_else(|| "context".into()),
        args.get("ttl_seconds").and_then(|v| v.as_i64()),
    )
}

fn recall(ctx: &Ctx, args: &Value) -> Result<String> {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(40).clamp(1, 200);
    render_entries(ctx, crate::memory::recall(
        ctx.db,
        &ctx.slot_id,
        &ctx.session_id,
        opt_str(args, "key").as_deref(),
        opt_str(args, "search").as_deref(),
        opt_str(args, "category").as_deref(),
        opt_str(args, "scope").as_deref(),
        limit,
    )?)
}

fn render_entries(ctx: &Ctx, rows: Vec<crate::memory::Entry>) -> Result<String> {
    if rows.is_empty() {
        return Ok("Nothing remembered that matches.".into());
    }
    let mut out = format!("{} entr(ies):\n", rows.len());
    for e in rows {
        // The scope is shown because it tells the reader how far the fact
        // travels: a global convention and somebody's private note deserve
        // different weight.
        out.push_str(&format!(
            "- [{}] {} ({}): {}",
            e.category, e.key, e.scope, e.value
        ));
        // Who wrote the value you are reading. With a shared key this is the
        // difference between a convention and one teammate's opinion.
        if let Some(a) = &e.author {
            out.push_str(&format!("  -- by {}", ctx.label_of(a)));
        }
        if e.revision > 1 {
            out.push_str(&format!(" (rev {})", e.revision));
        }
        out.push('\n');
    }
    Ok(out)
}

fn forget_memory(ctx: &Ctx, args: &Value) -> Result<String> {
    let scope = scope_of(ctx, args)?;
    crate::memory::forget(ctx.db, &ctx.slot_id, &scope, &arg_str(args, "key")?)
}

fn pending_asks(ctx: &Ctx) -> Result<String> {
    let rows = crate::asks::pending_for(ctx.db, &ctx.slot_id)?;
    if rows.is_empty() {
        return Ok("Nothing is waiting on you.".into());
    }
    let mut out = format!("{} request(s) waiting on you:\n", rows.len());
    for (id, capability, request, model) in rows {
        out.push_str(&format!("- {id} [{capability}]"));
        if let Some(m) = model {
            out.push_str(&format!(" model={m}"));
        }
        out.push_str(&format!("\n  {request}\n"));
    }
    out.push_str("Answer each with answer_ask; the asker is blocked until you do.\n");
    Ok(out)
}

fn answer_ask(ctx: &Ctx, args: &Value) -> Result<String> {
    let id = arg_str(args, "ask_id")?;
    let result = arg_str(args, "result")?;
    let failed = args.get("failed").and_then(|v| v.as_bool()).unwrap_or(false);
    crate::asks::answer(ctx.db, &ctx.slot_id, &id, &result, !failed)
}

/// Keep a summary against the AGENT's identity, not the session.
fn save_context(ctx: &Ctx, args: &Value) -> Result<String> {
    let summary = arg_str(args, "summary")?;
    let who: Option<(Option<String>, Option<String>, Option<String>)> = ctx
        .db
        .query_row(
            "SELECT s.cli_id, s.display_name,
                    (SELECT a.cwd FROM agents a WHERE a.slot_id = s.id
                      ORDER BY a.started_at_ms DESC LIMIT 1)
               FROM slots s WHERE s.id = ?",
            [&ctx.slot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match who {
        Some((Some(cli), Some(name), Some(cwd))) => {
            crate::memory::remember_context(ctx.db, &cli, &cwd, &name, &summary)?;
            Ok("Saved. If you are restarted you will be told this.".into())
        }
        // An external participant has no CLI and no working directory, so
        // there is no identity to key continuity on. Say so rather than
        // pretend it was stored.
        _ => bail!(
            "this seat has no CLI identity to remember against (an external participant \
             has no CLI or working directory); use store_memory with scope 'agent' instead"
        ),
    }
}

fn memory_history(ctx: &Ctx, args: &Value) -> Result<String> {
    let scope = scope_of(ctx, args)?;
    let key = arg_str(args, "key")?;
    let rows = crate::memory::history(ctx.db, &scope, &key)?;
    if rows.is_empty() {
        return Ok(format!("'{key}' has never been overwritten in {} memory.", scope.name()));
    }
    let mut out = format!("{} earlier revision(s) of '{key}':\n", rows.len());
    for (rev, value, author, _at) in rows {
        out.push_str(&format!(
            "- rev {rev} by {}: {value}\n",
            author.as_deref().map(|a| ctx.label_of(a)).unwrap_or_else(|| "?".into())
        ));
    }
    Ok(out)
}

fn get_history(ctx: &Ctx, args: &Value) -> Result<String> {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(30).clamp(1, 200);
    let mut stmt = ctx.db.prepare(
        "SELECT COALESCE(s.display_name, s.role, 'orchestrator'), m.kind, m.body
         FROM messages m LEFT JOIN slots s ON s.id = m.from_slot
         WHERE m.session_id = ? ORDER BY m.id DESC LIMIT ?",
    )?;
    let mut rows: Vec<(String, String, String)> = stmt
        .query_map(params![ctx.session_id, limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    rows.reverse();
    if rows.is_empty() {
        return Ok("No messages yet.".into());
    }
    let mut out = format!("Last {} message(s):\n", rows.len());
    for (who, kind, body) in rows {
        out.push_str(&format!("- [{kind}] {who}: {body}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A session with a shared-repo workspace and three slots: an engineer, a
    /// reviewer, and an EXTERNAL (non-CLI) participant standing in for a
    /// hermes agent.
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        let now = now_ms();
        db.execute(
            "INSERT INTO sessions (id, name, created_at_ms, last_active_ms) VALUES ('s1','T',?,?)",
            params![now, now],
        ).unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,lease_expires_ms)
             VALUES ('n-machine-a','machine-a','linux','x86_64','0.1.0','online',?,?,?)",
            params![now, now, now + 60000],
        ).unwrap();
        db.execute(
            "INSERT INTO workspaces (id, session_id, name, kind, repo_url, sync)
             VALUES ('w1','s1','main','repo','git@git:x.git','git')",
            [],
        ).unwrap();
        for (id, name, role, kind, cli, caps, model) in [
            ("sl-eng", "Max", "engineer", "cli", Some("claude"), r#"["cheap"]"#, None),
            ("sl-rev", "Iris", "code reviewer", "cli", Some("opencode"), r#"["strong","vision"]"#,
             Some("anthropic/claude-opus-4")),
            ("sl-ext", "Hermes", "archivist", "external", None, r#"["ocr"]"#, None),
        ] {
            db.execute(
                "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind,
                                    display_name, role, capabilities, model, created_at_ms)
                 VALUES (?,?, 'w1', ?, ?, ?, ?, ?, ?, ?)",
                params![id, "s1", cli, kind, name, role, caps, model, now],
            ).unwrap();
        }
        for (aid, slot, prov, node) in [
            ("a-eng", "sl-eng", "managed", Some("n-machine-a")),
            ("a-rev", "sl-rev", "managed", Some("n-machine-a")),
            ("a-ext", "sl-ext", "external", None),
        ] {
            db.execute(
                "INSERT INTO agents (id, slot_id, node_id, provenance, status, started_at_ms, last_seen_ms)
                 VALUES (?,?,?,?, 'running', ?, ?)",
                params![aid, slot, node, prov, now, now],
            ).unwrap();
        }
        db
    }

    fn ctx<'a>(db: &'a Connection, slot: &str) -> Ctx<'a> {
        Ctx { db, session_id: "s1".into(), slot_id: slot.into() }
    }

    /// A seat in some other session, for the cases that are about two teams.
    fn ctx2<'a>(db: &'a Connection, slot: &str, session: &str) -> Ctx<'a> {
        Ctx { db, session_id: session.into(), slot_id: slot.into() }
    }

    fn state(db: &Connection, slot: &str) -> String {
        db.query_row("SELECT task_state FROM slots WHERE id = ?", [slot], |r| r.get(0)).unwrap()
    }

    #[test]
    fn an_external_agent_is_a_full_peer() {
        let db = fixture();
        // The engineer sees the hermes agent as a teammate...
        let out = dispatch(&ctx(&db, "sl-eng"), "list_peers", &json!({})).unwrap();
        assert!(out.contains("Hermes"), "{out}");
        assert!(out.contains("external"), "{out}");
        // ...and the hermes agent can use the same tools to reach the others.
        let out = dispatch(&ctx(&db, "sl-ext"), "send_message",
            &json!({"to": "Max", "text": "indexed the corpus"})).unwrap();
        assert!(out.contains("Max"), "{out}");
        let got = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from_name.as_deref(), Some("Hermes"));
    }

    #[test]
    fn the_review_loop_walks_the_state_machine() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "set_summary", &json!({"summary": "building"})).unwrap();
        assert_eq!(state(&db, "sl-eng"), "working");

        dispatch(&ctx(&db, "sl-eng"), "signal_done", &json!({})).unwrap();
        assert_eq!(state(&db, "sl-eng"), "done_pending_review");

        dispatch(&ctx(&db, "sl-rev"), "submit_feedback",
            &json!({"to": "Max", "text": "null deref", "actionable": true})).unwrap();
        assert_eq!(state(&db, "sl-eng"), "addressing_feedback");

        dispatch(&ctx(&db, "sl-eng"), "signal_done", &json!({"notes": "fixed"})).unwrap();
        assert_eq!(state(&db, "sl-eng"), "done_pending_review");

        dispatch(&ctx(&db, "sl-rev"), "approve", &json!({"to": "Max"})).unwrap();
        assert_eq!(state(&db, "sl-eng"), "approved");
    }

    #[test]
    fn list_peers_shows_capabilities_and_the_model() {
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "list_peers", &json!({})).unwrap();
        assert!(out.contains("can=strong,vision"), "{out}");
        assert!(out.contains("model=anthropic/claude-opus-4"), "{out}");
        assert!(out.contains("can=ocr"), "{out}");
    }

    #[test]
    fn list_peers_filters_by_capability() {
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "list_peers",
            &json!({"capability": "OCR"})).unwrap();
        assert!(out.contains("Hermes"), "{out}");
        assert!(!out.contains("Iris"), "case must not matter: {out}");
    }

    #[test]
    fn a_capability_nobody_has_says_so_rather_than_listing_everyone() {
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "list_peers",
            &json!({"capability": "quantum"})).unwrap();
        assert!(out.contains("No teammate has the capability 'quantum'"), "{out}");
    }

    #[test]
    fn a_capability_label_is_matched_whole_not_as_a_prefix() {
        // "strong" must not match a seat labelled "strongish".
        let db = fixture();
        db.execute("UPDATE slots SET capabilities = '[\"strongish\"]' WHERE id = 'sl-ext'", [])
            .unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "list_peers",
            &json!({"capability": "strong"})).unwrap();
        assert!(out.contains("Iris") && !out.contains("Hermes"), "{out}");
    }

    #[test]
    fn a_teammate_can_be_addressed_by_a_capability_only_one_holds() {
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "send_message",
            &json!({"to": "ocr", "text": "scan this"})).unwrap();
        assert!(out.contains("Hermes"), "{out}");
    }

    #[test]
    fn a_capability_several_hold_is_refused_rather_than_guessed() {
        let db = fixture();
        db.execute("UPDATE slots SET capabilities = '[\"strong\"]' WHERE id = 'sl-ext'", [])
            .unwrap();
        let err = dispatch(&ctx(&db, "sl-eng"), "send_message",
            &json!({"to": "strong", "text": "help"})).unwrap_err().to_string();
        assert!(err.contains("capability held by 2"), "{err}");
        assert!(err.contains("list_peers"), "it must say how to choose: {err}");
    }

    #[test]
    fn a_name_wins_over_a_capability_of_the_same_spelling() {
        // Otherwise naming a teammate could be hijacked by a label.
        let db = fixture();
        db.execute("UPDATE slots SET capabilities = '[\"iris\"]' WHERE id = 'sl-ext'", [])
            .unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "send_message",
            &json!({"to": "Iris", "text": "hi"})).unwrap();
        assert!(out.contains("Iris") && !out.contains("Hermes"), "{out}");
    }

    #[test]
    fn escalating_moves_both_ends_and_carries_the_reason() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "set_summary",
            &json!({"summary": "wiring the parser", "status": "in_progress"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "escalate",
            &json!({"to": "strong", "reason": "the grammar is beyond me"})).unwrap();
        assert!(out.contains("Iris"), "{out}");
        // Neither side may be left thinking the other has it.
        assert_eq!(state(&db, "sl-eng"), "escalated");
        assert_eq!(state(&db, "sl-rev"), "working");
        let body: String = db
            .query_row("SELECT body FROM messages WHERE to_slot = 'sl-rev'", [], |r| r.get(0))
            .unwrap();
        assert!(body.contains("the grammar is beyond me"), "{body}");
        assert!(body.contains("wiring the parser"), "it should carry the task too: {body}");
        let kind: String = db
            .query_row("SELECT kind FROM messages WHERE to_slot = 'sl-rev'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "handoff");
    }

    #[test]
    fn escalating_can_name_a_different_task() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "escalate",
            &json!({"to": "Iris", "reason": "needs vision", "task": "read the scanned table"}))
            .unwrap();
        let body: String = db
            .query_row("SELECT body FROM messages WHERE to_slot = 'sl-rev'", [], |r| r.get(0))
            .unwrap();
        assert!(body.contains("read the scanned table"), "{body}");
    }

    #[test]
    fn you_cannot_escalate_to_yourself() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-eng"), "escalate",
            &json!({"to": "Max", "reason": "stuck"})).unwrap_err().to_string();
        assert!(err.contains("yourself") || err.contains("no teammate"), "{err}");
    }

    #[test]
    fn escalating_to_nobody_is_an_error_not_a_silent_drop() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-eng"), "escalate",
            &json!({"to": "nobody-here", "reason": "stuck"})).unwrap_err().to_string();
        assert!(err.contains("no teammate"), "{err}");
        assert_eq!(state(&db, "sl-eng"), "idle", "my state must not move if it failed");
    }

    #[test]
    fn a_reviewer_self_approves_on_done() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "signal_done", &json!({})).unwrap();
        assert_eq!(state(&db, "sl-rev"), "approved", "nobody reviews the reviewer");
    }

    #[test]
    fn approving_work_that_was_never_submitted_is_refused() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-rev"), "approve", &json!({"to": "Max"})).unwrap_err();
        assert!(err.to_string().contains("not signalled done"), "{err}");
    }

    #[test]
    fn locks_are_exclusive_and_reentrant_for_the_holder() {
        let db = fixture();
        let a = dispatch(&ctx(&db, "sl-eng"), "acquire_file",
            &json!({"path": "Cargo.toml", "purpose": "add dep"})).unwrap();
        assert!(a.contains("Locked"), "{a}");
        let b = dispatch(&ctx(&db, "sl-rev"), "acquire_file", &json!({"path": "Cargo.toml"})).unwrap();
        assert!(b.contains("DENIED"), "{b}");
        let c = dispatch(&ctx(&db, "sl-eng"), "acquire_file", &json!({"path": "Cargo.toml"})).unwrap();
        assert!(c.contains("already hold"), "{c}");
        dispatch(&ctx(&db, "sl-eng"), "release_file", &json!({"path": "Cargo.toml"})).unwrap();
        let d = dispatch(&ctx(&db, "sl-rev"), "acquire_file", &json!({"path": "Cargo.toml"})).unwrap();
        assert!(d.contains("Locked"), "{d}");
    }

    #[test]
    fn releasing_a_lock_you_do_not_hold_is_an_error() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-eng"), "release_file", &json!({"path": "nope"})).unwrap_err();
        assert!(err.to_string().contains("do not hold"), "{err}");
    }

    #[test]
    fn an_expired_lock_does_not_block_the_team_forever() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "acquire_file", &json!({"path": "x.rs"})).unwrap();
        // Simulate the holder having crashed five minutes ago.
        db.execute("UPDATE locks SET expires_at_ms = ?", [now_ms() - 1000]).unwrap();
        let out = dispatch(&ctx(&db, "sl-rev"), "acquire_file", &json!({"path": "x.rs"})).unwrap();
        assert!(out.contains("Locked"), "{out}");
    }

    #[test]
    fn node_local_workspaces_do_not_share_a_lock_domain() {
        // The bug this prevents: two agents editing their OWN local
        // src/main.rs on different machines must not contend for one lock.
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,lease_expires_ms)
             VALUES ('n-machine-c','machine-c','linux','x86_64','0.1.0','online',?,?,?)",
            params![now, now, now + 60000],
        ).unwrap();
        for (wid, node) in [("w-z", "n-machine-a"), ("w-l", "n-machine-c")] {
            db.execute(
                "INSERT INTO workspaces (id, session_id, name, kind, node_id) VALUES (?,'s1',?, 'node_local', ?)",
                params![wid, wid, node],
            ).unwrap();
        }
        db.execute("UPDATE slots SET workspace_id = 'w-z' WHERE id = 'sl-eng'", []).unwrap();
        db.execute("UPDATE slots SET workspace_id = 'w-l' WHERE id = 'sl-rev'", []).unwrap();

        let a = dispatch(&ctx(&db, "sl-eng"), "acquire_file", &json!({"path": "src/main.rs"})).unwrap();
        let b = dispatch(&ctx(&db, "sl-rev"), "acquire_file", &json!({"path": "src/main.rs"})).unwrap();
        assert!(a.contains("Locked"), "{a}");
        assert!(b.contains("Locked"), "both must get it: different machines, different files");
    }

    #[test]
    fn two_local_workspaces_on_one_machine_do_not_share_a_lock_domain() {
        // The bug this prevents: the domain was the machine alone, so every
        // node_local workspace on a host shared one. An agent locking
        // src/main.rs under /AI/sentenze blocked another locking
        // src/main.rs under /storage/coderai -- different trees, and a
        // denial naming an agent in a project you were not touching.
        let db = fixture();
        let now = now_ms();
        for (wid, path) in [("w-sentenze", "/AI/sentenze"), ("w-coderai", "/storage/coderai")] {
            db.execute(
                "INSERT INTO workspaces (id, session_id, name, kind, node_id)
                 VALUES (?,'s1',?,'node_local','n-machine-a')",
                params![wid, wid],
            )
            .unwrap();
            db.execute(
                "INSERT INTO workspace_mounts (workspace_id, node_id, path) VALUES (?,?,?)",
                params![wid, "n-machine-a", path],
            )
            .unwrap();
        }
        let _ = now;
        db.execute("UPDATE slots SET workspace_id = 'w-sentenze' WHERE id = 'sl-eng'", []).unwrap();
        db.execute("UPDATE slots SET workspace_id = 'w-coderai' WHERE id = 'sl-rev'", []).unwrap();

        let a = dispatch(&ctx(&db, "sl-eng"), "acquire_file", &json!({"path": "src/main.rs"})).unwrap();
        let b = dispatch(&ctx(&db, "sl-rev"), "acquire_file", &json!({"path": "src/main.rs"})).unwrap();
        assert!(a.contains("Locked"), "{a}");
        assert!(b.contains("Locked"),
                "same machine, same relative path, DIFFERENT directories: {b}");
    }

    #[test]
    fn one_directory_contends_even_across_sessions() {
        // The other half, and the reason the domain is not the session: two
        // sessions may mount the same directory on the same machine, and two
        // agents editing one file there must contend whichever team they are
        // on. A lock is about the file.
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s2','other',?,?)",
            params![now, now],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id, session_id, cli_id, display_name, role, created_at_ms)
             VALUES ('sl-other','s2','claude','Theirs','engineer',?)",
            params![now],
        )
        .unwrap();
        for (wid, sess) in [("w-mine", "s1"), ("w-theirs", "s2")] {
            db.execute(
                "INSERT INTO workspaces (id, session_id, name, kind, node_id)
                 VALUES (?,?,?,'node_local','n-machine-a')",
                params![wid, sess, wid],
            )
            .unwrap();
            db.execute(
                "INSERT INTO workspace_mounts (workspace_id, node_id, path)
                 VALUES (?, 'n-machine-a', '/srv/shared')",
                [wid],
            )
            .unwrap();
        }
        db.execute("UPDATE slots SET workspace_id = 'w-mine' WHERE id = 'sl-eng'", []).unwrap();
        db.execute("UPDATE slots SET workspace_id = 'w-theirs' WHERE id = 'sl-other'", []).unwrap();

        let a = dispatch(&ctx(&db, "sl-eng"), "acquire_file", &json!({"path": "src/main.rs"})).unwrap();
        assert!(a.contains("Locked"), "{a}");
        let b = dispatch(&ctx2(&db, "sl-other", "s2"), "acquire_file",
                         &json!({"path": "src/main.rs"})).unwrap();
        assert!(b.contains("DENIED"),
                "one directory, one file, two sessions: the second must be refused: {b}");
    }

    fn note_target(db: &Connection) -> (Option<String>, String, String) {
        db.query_row(
            "SELECT to_session, from_kind, from_label FROM operator_messages
              ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn give_session_an_orchestrator(db: &Connection, label: &str, last_used_ms: i64) {
        db.execute(
            "INSERT INTO operator_tokens (token_sha256, label, session_id, issued_at_ms, last_used_ms)
             VALUES (?,?, 's1', ?, ?)",
            params![format!("hash-{label}"), label, now_ms(), last_used_ms],
        )
        .unwrap();
    }

    #[test]
    fn an_agent_raises_things_with_its_own_orchestrator_first() {
        // The team's driver knows the work. An agent going over its head for
        // every question would make scoping pointless.
        let db = fixture();
        give_session_an_orchestrator(&db, "alpha-lead", now_ms());
        let out = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                           &json!({"text": "which database do we target?"})).unwrap();
        assert!(out.contains("this session's orchestrator"), "{out}");
        let (to, kind, from) = note_target(&db);
        assert_eq!(to.as_deref(), Some("s1"), "it must be addressed to this session");
        assert_eq!(kind, "agent", "an orchestrator must be able to tell this from a peer's note");
        assert_eq!(from, "Max");
    }

    #[test]
    fn with_no_orchestrator_of_its_own_an_agent_reaches_the_supervisors() {
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                           &json!({"text": "nobody is driving this team"})).unwrap();
        assert!(out.contains("supervisors"), "{out}");
        assert!(out.contains("no orchestrator of its own"), "{out}");
        assert_eq!(note_target(&db).0, None, "None is the supers' address");
    }

    #[test]
    fn an_orchestrator_that_has_gone_quiet_is_treated_as_absent() {
        // "Present" cannot mean "a token exists": a revoked-in-practice
        // orchestrator that nobody is running would swallow every request an
        // agent made, silently, for as long as the token existed.
        let db = fixture();
        give_session_an_orchestrator(&db, "asleep", now_ms() - ORCHESTRATOR_SILENT_MS - 1);
        let out = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                           &json!({"text": "still blocked"})).unwrap();
        assert!(out.contains("supervisors"), "{out}");
        assert!(out.contains("not been heard from"), "{out}");
        assert_eq!(note_target(&db).0, None);
    }

    #[test]
    fn an_agent_can_say_the_orchestrator_itself_is_the_problem() {
        let db = fixture();
        give_session_an_orchestrator(&db, "alpha-lead", now_ms());
        let out = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                           &json!({"text": "it keeps telling me to do the impossible",
                                   "escalate": true})).unwrap();
        assert!(out.contains("supervisors"), "{out}");
        assert_eq!(note_target(&db).0, None, "escalate goes past the session's own driver");
    }

    #[test]
    fn a_revoked_orchestrator_does_not_count_as_present() {
        let db = fixture();
        give_session_an_orchestrator(&db, "gone", now_ms());
        db.execute("UPDATE operator_tokens SET revoked_at_ms = ? WHERE label = 'gone'",
                   [now_ms()]).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                           &json!({"text": "hello?"})).unwrap();
        assert!(out.contains("no orchestrator of its own"), "{out}");
    }

    #[test]
    fn checking_messages_does_not_clear_them() {
        // Reading is not answering. This acked everything it showed, so a
        // message glanced at was a message gone, and nobody -- agent or
        // operator -- could tell afterwards what had actually been dealt
        // with.
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "look at the loader"})).unwrap();
        let first = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(first.contains("look at the loader"), "{first}");
        let again = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(again.contains("look at the loader"),
                "a second check must still show it: {again}");
    }

    #[test]
    fn claiming_is_what_clears_a_message() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "ping"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "claim_messages", &json!({})).unwrap();
        assert!(out.contains("Claimed 1"), "{out}");
        let after = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(after.contains("Nothing waiting"), "{after}");
        let who: String = db
            .query_row("SELECT claimed_by FROM messages WHERE body = 'ping'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(who, "Max", "the log must say who closed it");
    }

    #[test]
    fn a_seat_cannot_claim_another_seats_messages() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "for max only"})).unwrap();
        let id: i64 = db
            .query_row("SELECT id FROM messages WHERE body = 'for max only'", [], |r| r.get(0))
            .unwrap();
        // The reviewer tries to close a message addressed to the engineer.
        let _ = dispatch(&ctx(&db, "sl-rev"), "claim_messages", &json!({"ids": [id]}));
        let claimed: Option<i64> = db
            .query_row("SELECT claimed_at_ms FROM messages WHERE id = ?", [id], |r| r.get(0))
            .unwrap();
        assert!(claimed.is_none(), "another seat must not be able to close your message");
        let still = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(still.contains("for max only"), "{still}");
    }

    #[test]
    fn an_operator_can_clear_what_nobody_claimed() {
        // The other half of messages that no longer clear themselves: they
        // pile up, and somebody has to be able to say "this no longer
        // matters" without pretending an agent dealt with it.
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "stale"})).unwrap();
        let ids: Vec<i64> = db
            .prepare("SELECT id FROM messages WHERE claimed_at_ms IS NULL").unwrap()
            .query_map([], |r| r.get(0)).unwrap()
            .collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(claim_messages(&db, "operator:me", &ids).unwrap(), 1);
        let after = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(after.contains("Nothing waiting"), "{after}");
    }

    #[test]
    fn an_ambiguous_peer_name_is_refused_rather_than_guessed() {
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO slots (id, session_id, cli_id, display_name, role, created_at_ms)
             VALUES ('sl-2','s1','claude','Maxine','engineer',?)",
            params![now],
        ).unwrap();
        // "Max" now partially matches Max and Maxine; sending review feedback
        // to the wrong agent is worse than making the caller retry.
        let err = dispatch(&ctx(&db, "sl-rev"), "send_message",
            &json!({"to": "Max", "text": "hi"}));
        // Exact match on "Max" wins over the partial "Maxine".
        assert!(err.is_ok(), "exact match should win: {err:?}");
        let err = dispatch(&ctx(&db, "sl-rev"), "send_message",
            &json!({"to": "Ma", "text": "hi"})).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn broadcast_reaches_everyone_but_the_sender() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "broadcast", &json!({"text": "standup"})).unwrap();
        assert_eq!(pending_for_slot(&db, "s1", "sl-rev").unwrap().len(), 1);
        assert_eq!(pending_for_slot(&db, "s1", "sl-ext").unwrap().len(), 1);
        assert_eq!(pending_for_slot(&db, "s1", "sl-eng").unwrap().len(), 0, "not back to sender");
    }

    #[test]
    fn one_teammate_reading_a_broadcast_does_not_take_it_from_the_others() {
        // The bug this exists for: `messages` carries one `acked_at_ms`, so
        // the first agent to answer consumed the message for the whole team
        // and nobody could see that it had happened. On this fleet a
        // capability bridge -- handed team chat it cannot answer -- ate an
        // operator's question twice before anyone noticed.
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "broadcast", &json!({"text": "standup"})).unwrap();
        let mine = pending_for_slot(&db, "s1", "sl-rev").unwrap();
        assert_eq!(mine.len(), 1);
        mark_inflight(&db, "a-rev", "sl-rev", &[mine[0].id]).unwrap();
        ack_messages(&db, "a-rev", &[mine[0].id]).unwrap();

        assert!(pending_for_slot(&db, "s1", "sl-rev").unwrap().is_empty(),
                "the one who read it does not get it again");
        assert_eq!(pending_for_slot(&db, "s1", "sl-ext").unwrap().len(), 1,
                   "everyone else still has it waiting");
    }

    #[test]
    fn a_seat_that_takes_a_broadcast_in_flight_blocks_only_itself() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "broadcast", &json!({"text": "standup"})).unwrap();
        let mine = pending_for_slot(&db, "s1", "sl-rev").unwrap();
        mark_inflight(&db, "a-rev", "sl-rev", &[mine[0].id]).unwrap();
        assert!(pending_for_slot(&db, "s1", "sl-rev").unwrap().is_empty(), "no double delivery");
        assert_eq!(pending_for_slot(&db, "s1", "sl-ext").unwrap().len(), 1,
                   "and the others are not waiting on it");
        // An agent that died holding it must not keep it from itself for ever.
        assert!(reclaim_stale_inflight(&db, 0).unwrap() >= 1);
        assert_eq!(pending_for_slot(&db, "s1", "sl-rev").unwrap().len(), 1, "it comes back");
    }

    #[test]
    fn a_message_addressed_to_one_agent_is_still_acked_once() {
        // The per-seat table must not change what a DIRECTED message means:
        // it has one recipient, and the message's own ack is the whole story.
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message", &json!({"to": "Max", "text": "x"})).unwrap();
        let m = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert_eq!(m.len(), 1);
        ack_messages(&db, "a-eng", &[m[0].id]).unwrap();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().is_empty());
        let acked: Option<i64> = db
            .query_row("SELECT acked_at_ms FROM messages WHERE id = ?", [m[0].id], |r| r.get(0))
            .unwrap();
        assert!(acked.is_some(), "the log still shows it as delivered");
    }

    #[test]
    fn an_inflight_message_is_not_handed_out_twice_but_is_reclaimed_if_lost() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message", &json!({"to": "Max", "text": "x"})).unwrap();
        let first = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert_eq!(first.len(), 1);
        mark_inflight(&db, "a-eng", "sl-eng", &[first[0].id]).unwrap();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().is_empty(), "no double delivery");

        // The agent died before acking: the message must come back.
        assert_eq!(reclaim_stale_inflight(&db, 0).unwrap(), 1);
        assert_eq!(pending_for_slot(&db, "s1", "sl-eng").unwrap().len(), 1, "must not be lost");

        // Once acked it is gone for good.
        mark_inflight(&db, "a-eng", "sl-eng", &[first[0].id]).unwrap();
        ack_messages(&db, "a-eng", &[first[0].id]).unwrap();
        assert_eq!(reclaim_stale_inflight(&db, 0).unwrap(), 0);
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().is_empty());
    }

    #[test]
    fn knowledge_upserts_and_filters_by_category() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-eng"), "store_knowledge",
            &json!({"key": "auth", "value": "JWT", "category": "decision"})).unwrap();
        dispatch(&ctx(&db, "sl-eng"), "store_knowledge",
            &json!({"key": "auth", "value": "JWT + refresh rotation", "category": "decision"})).unwrap();
        dispatch(&ctx(&db, "sl-eng"), "store_knowledge",
            &json!({"key": "style", "value": "tabs", "category": "convention"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-ext"), "query_knowledge", &json!({"category": "decision"})).unwrap();
        assert!(out.contains("refresh rotation"), "{out}");
        assert!(!out.contains("tabs"), "category filter leaked: {out}");
        assert!(out.contains("1 entr"), "upsert should not duplicate: {out}");
    }

    #[test]
    fn a_bad_category_is_rejected() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-eng"), "store_knowledge",
            &json!({"key": "k", "value": "v", "category": "nonsense"})).unwrap_err();
        assert!(err.to_string().contains("category must be"), "{err}");
    }

    #[test]
    fn missing_required_arguments_are_reported_by_name() {
        let db = fixture();
        let err = dispatch(&ctx(&db, "sl-eng"), "send_message", &json!({"to": "Iris"})).unwrap_err();
        assert!(err.to_string().contains("'text'"), "{err}");
        let err = dispatch(&ctx(&db, "sl-rev"), "submit_feedback",
            &json!({"to": "Max", "text": "x"})).unwrap_err();
        assert!(err.to_string().contains("'actionable'"), "{err}");
    }

    #[test]
    fn unknown_tools_are_rejected() {
        let db = fixture();
        assert!(dispatch(&ctx(&db, "sl-eng"), "rm_minus_rf", &json!({})).is_err());
    }

    #[test]
    fn check_messages_shows_without_clearing_and_delivery_still_acks() {
        // This test used to assert the opposite -- that a check acked what
        // it showed. That is the behaviour being reversed: reading is not
        // answering. What must NOT change is delivery: a message handed
        // over and acked is still not handed over twice, which is what
        // keeps a respawned agent from re-reading its whole inbox.
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message", &json!({"to": "Max", "text": "ping"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(out.contains("ping"), "{out}");
        let again = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(again.contains("ping"), "a check must not clear it: {again}");

        // Delivery is a separate axis and is untouched.
        let handed = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert_eq!(handed.len(), 1);
        ack_messages(&db, "a-eng", &[handed[0].id]).unwrap();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().is_empty(),
                "an acked message must not be handed over again");
        let still = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(still.contains("ping"),
                "delivered is not dealt with: it still waits on the agent: {still}");
    }

    #[test]
    fn the_plan_tracks_progress() {
        let db = fixture();
        let now = now_ms();
        for (i, label) in ["design schema", "write hub", "write node"].iter().enumerate() {
            db.execute(
                "INSERT INTO plan_items (session_id, ord, label, updated_at_ms) VALUES ('s1',?,?,?)",
                params![i as i64 + 1, label, now],
            ).unwrap();
        }
        dispatch(&ctx(&db, "sl-eng"), "update_plan", &json!({"item": "1", "status": "done"})).unwrap();
        dispatch(&ctx(&db, "sl-eng"), "update_plan", &json!({"item": "write hub", "status": "in_progress"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "get_plan", &json!({})).unwrap();
        assert!(out.contains("1/3 done"), "{out}");
        assert!(out.contains("[in_progress] write hub"), "{out}");
        let err = dispatch(&ctx(&db, "sl-eng"), "update_plan",
            &json!({"item": "nonexistent", "status": "done"})).unwrap_err();
        assert!(err.to_string().contains("no plan item"), "{err}");
    }
}
