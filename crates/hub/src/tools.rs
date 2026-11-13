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
            "SELECT id, LOWER(COALESCE(display_name, '')), LOWER(COALESCE(role, '')), capabilities
             FROM slots WHERE session_id = ? AND id != ?",
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
    fn lock_scope(&self) -> Result<String> {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = self
            .db
            .query_row(
                "SELECT w.kind, w.id, w.node_id FROM slots s
                 LEFT JOIN workspaces w ON w.id = s.workspace_id WHERE s.id = ?",
                [&self.slot_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            Some((Some(kind), Some(ws_id), node_id)) => match kind.as_str() {
                "node_local" => Ok(format!(
                    "node:{}",
                    node_id.unwrap_or_else(|| ws_id.clone())
                )),
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
pub fn pending_for_slot(db: &Connection, session_id: &str, slot_id: &str) -> Result<Vec<Envelope>> {
    let mut stmt = db.prepare(
        "SELECT m.id, m.from_slot,
                COALESCE(s.display_name, s.role, 'orchestrator'),
                m.to_slot, m.kind, m.body, m.sent_at_ms
         FROM messages m LEFT JOIN slots s ON s.id = m.from_slot
         WHERE m.session_id = ?1
           AND (m.to_slot = ?2 OR m.to_slot IS NULL)
           AND m.from_slot IS NOT ?2
           AND m.acked_at_ms IS NULL
           AND m.inflight_at_ms IS NULL
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

/// Mark messages handed to an agent but not yet confirmed. An in-flight
/// message is never handed to a second agent, and `reclaim_stale_inflight`
/// frees it if the agent dies before acking.
pub fn mark_inflight(db: &Connection, agent_id: &str, ids: &[i64]) -> Result<()> {
    let now = now_ms();
    for id in ids {
        db.execute(
            "UPDATE messages SET inflight_agent = ?, inflight_at_ms = ? WHERE id = ?",
            params![agent_id, now, id],
        )?;
    }
    Ok(())
}

pub fn ack_messages(db: &Connection, agent_id: &str, ids: &[i64]) -> Result<()> {
    let now = now_ms();
    for id in ids {
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
        "set_summary" => set_summary(ctx, args),
        "check_team_status" => check_team_status(ctx),
        "get_plan" => get_plan(ctx),
        "update_plan" => update_plan(ctx, args),
        "signal_done" => signal_done(ctx, args),
        "escalate" => escalate(ctx, args),
        "submit_feedback" => submit_feedback(ctx, args),
        "approve" => approve(ctx, args),
        "acquire_file" => acquire_file(ctx, args),
        "release_file" => release_file(ctx, args),
        "view_file_locks" => view_file_locks(ctx),
        "store_knowledge" => store_knowledge(ctx, args),
        "query_knowledge" => query_knowledge(ctx, args),
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
    let msgs = pending_for_slot(ctx.db, &ctx.session_id, &ctx.slot_id)?;
    if msgs.is_empty() {
        return Ok("No new messages.".into());
    }
    // An explicit check is itself proof of delivery, so ack immediately.
    let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
    ack_messages(ctx.db, "self-check", &ids)?;
    let mut out = format!("{} new message(s):\n", msgs.len());
    for m in msgs {
        out.push_str(&format!(
            "- from {}: {}\n",
            m.from_name.unwrap_or_else(|| "orchestrator".into()),
            m.body
        ));
    }
    Ok(out)
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

fn store_knowledge(ctx: &Ctx, args: &Value) -> Result<String> {
    let key = arg_str(args, "key")?;
    let value = arg_str(args, "value")?;
    let category = opt_str(args, "category").unwrap_or_else(|| "context".into());
    if !["decision", "convention", "discovery", "blocker", "context"].contains(&category.as_str()) {
        bail!("category must be decision, convention, discovery, blocker or context");
    }
    ctx.db.execute(
        "INSERT INTO knowledge (session_id, key, category, value, author_slot, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id, key) DO UPDATE SET
            category = ?3, value = ?4, author_slot = ?5, updated_at_ms = ?6",
        params![ctx.session_id, key, category, value, ctx.slot_id, now_ms()],
    )?;
    Ok(format!("Stored '{key}' as {category}."))
}

fn query_knowledge(ctx: &Ctx, args: &Value) -> Result<String> {
    let key = opt_str(args, "key");
    let category = opt_str(args, "category");
    let mut stmt = ctx.db.prepare(
        "SELECT k.key, k.category, k.value, COALESCE(s.display_name, s.role, '?')
         FROM knowledge k LEFT JOIN slots s ON s.id = k.author_slot
         WHERE k.session_id = ?1
           AND (?2 IS NULL OR k.key = ?2)
           AND (?3 IS NULL OR k.category = ?3)
         ORDER BY k.category, k.key",
    )?;
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map(params![ctx.session_id, key, category], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        return Ok("No knowledge recorded yet.".into());
    }
    let mut out = format!("{} entr(ies):\n", rows.len());
    for (k, c, v, who) in rows {
        out.push_str(&format!("- [{c}] {k}: {v}  (by {who})\n"));
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
    fn an_inflight_message_is_not_handed_out_twice_but_is_reclaimed_if_lost() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message", &json!({"to": "Max", "text": "x"})).unwrap();
        let first = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert_eq!(first.len(), 1);
        mark_inflight(&db, "a-eng", &[first[0].id]).unwrap();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().is_empty(), "no double delivery");

        // The agent died before acking: the message must come back.
        assert_eq!(reclaim_stale_inflight(&db, 0).unwrap(), 1);
        assert_eq!(pending_for_slot(&db, "s1", "sl-eng").unwrap().len(), 1, "must not be lost");

        // Once acked it is gone for good.
        mark_inflight(&db, "a-eng", &[first[0].id]).unwrap();
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
    fn check_messages_acks_what_it_shows() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message", &json!({"to": "Max", "text": "ping"})).unwrap();
        let out = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(out.contains("ping"), "{out}");
        let again = dispatch(&ctx(&db, "sl-eng"), "check_messages", &json!({})).unwrap();
        assert!(again.contains("No new messages"), "{again}");
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
