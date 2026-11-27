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
    /// The FLEET-WIDE flood limits: messages a seat may send a minute, and
    /// messages one pair may exchange in ten. The bottom of the usual three
    /// levels -- the seat's own, then its team's, then this -- and the only
    /// one that is not in the database, because it belongs to the hub's
    /// deployment rather than to anything in it.
    ///
    /// `(0, 0)` is off, which is what a `Default` and every test gets.
    pub flood: (i64, i64),
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

/// Does an ownership pattern cover this path?
///
/// The patterns teams actually write are directory prefixes and globs:
/// `verti/**`, `src/auth/*`, `*.sql`, `containers/ombia`. So this is a plain
/// glob -- `*` within one path segment, `**` across segments, `?` for one
/// character -- rather than a regex nobody would write by hand, and a bare
/// prefix with no wildcard covers everything under it, because that is what
/// somebody means by "this agent owns verti".
pub fn pattern_covers(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./").trim_start_matches('/');
    let path = path.trim().trim_start_matches("./").trim_start_matches('/');
    if pattern.is_empty() || path.is_empty() {
        return false;
    }
    if glob_match(pattern, path) {
        return true;
    }
    // "verti" owns "verti/src/main.rs". A pattern that is a plain name or
    // directory is a prefix; one with a wildcard in it means exactly what it
    // says and is not widened.
    if !pattern.contains(['*', '?']) {
        let prefix = pattern.trim_end_matches('/');
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    false
}

/// `*` stops at a separator, `**` does not, `?` is one character.
fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = path.chars().collect();
    fn go(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                // `**` crosses separators; a single `*` does not.
                let double = p.len() > 1 && p[1] == '*';
                let rest = if double {
                    // `**/` also matches nothing at all, so `a/**/b` covers `a/b`.
                    let after = &p[2..];
                    if go(after.strip_prefix(&['/']).unwrap_or(after), t) {
                        return true;
                    }
                    after
                } else {
                    &p[1..]
                };
                if go(rest, t) {
                    return true;
                }
                for i in 0..t.len() {
                    if !double && t[i] == '/' {
                        break;
                    }
                    if go(rest, &t[i + 1..]) {
                        return true;
                    }
                }
                false
            }
            '?' => !t.is_empty() && t[0] != '/' && go(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && go(&p[1..], &t[1..]),
        }
    }
    go(&p, &t)
}

/// Who owns a path in this workspace, if anybody: (slot, name, pattern).
///
/// The longest pattern wins, so `verti/db/**` beats `verti/**` -- a team
/// that carves a piece out of somebody's area means the carve-out.
pub fn owner_of(
    db: &Connection,
    workspace_id: &str,
    path: &str,
) -> Result<Option<(String, String, String)>> {
    let mut stmt = db.prepare(
        "SELECT o.slot_id, COALESCE(s.display_name, s.role, 'slot-' || s.id), o.pattern
           FROM ownership o JOIN slots s ON s.id = o.slot_id
          WHERE o.workspace_id = ?",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([workspace_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, pat)| pattern_covers(pat, path))
        .max_by_key(|(_, _, pat)| pat.trim().len()))
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

    /// What this seat may do to the fleet. Read fresh on every call, never
    /// cached and never taken from anything the caller sent.
    fn authority(&self) -> nexmesh_proto::authority::Authority {
        crate::fleet::authority_of(self.db, &self.slot_id)
    }

    /// WHO THIS SEAT MAY SEE AND ADDRESS, as one SQL predicate.
    ///
    /// Every peer query uses this, so what is LISTED and what can be
    /// ADDRESSED cannot drift apart. They had: `list_peers` carried its own
    /// copy of the predicate without the terminal exclusion, so an adopted
    /// terminal was offered as a teammate and then refused as a target --
    /// "no teammate matching 'term-Orchestrator'" for a name the same tool
    /// had just printed.
    ///
    /// A SUPERORCHESTRATOR'S ROOM IS THE FLEET. Its authority already
    /// reaches every team -- it may move, rename, remove and elect seats
    /// anywhere, and `list_seats` shows it all of them -- but its messaging
    /// stayed session-scoped, so it could reorganise a team it was unable to
    /// say one word to. The reach of the two has to be the same reach.
    ///
    /// An ordinary seat, and an orchestrator, see exactly what they always
    /// did: their own team, the teams they are a guest in, and those teams'
    /// guests.
    ///
    /// `me` is the placeholder holding this seat's slot id in the caller's
    /// query, because the two call sites number their parameters differently.
    fn peer_scope(&self, me: &str) -> String {
        use nexmesh_proto::authority::Authority;
        let room = if self.authority() == Authority::Super {
            "1 = 1".to_string()
        } else {
            crate::guests::shares_room("s", me)
        };
        // An adopted terminal is a window onto a seat, not a teammate. It
        // holds no model, answers nothing and acks nothing, and whatever is
        // IN it has a seat of its own that this query already returns.
        format!("({room}) AND COALESCE(s.cli_id,'') <> 'tmux' AND s.tmux_target IS NULL")
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

    /// Everyone this seat may address but me, with what they answer to.
    fn peers(&self) -> Result<Vec<Peer>> {
        let mut stmt = self.db.prepare(&format!(
            // Anyone I share a room with: my own team, plus any team I am a
            // guest in, plus the guests of those teams. Both directions
            // matter -- a room must see its guests, and a guest must see the
            // rooms it answers in, or it can be asked for something and have
            // no way to reply. For a superorchestrator, the fleet.
            "SELECT s.id, LOWER(COALESCE(s.display_name, '')),
                    LOWER(COALESCE(s.role, '')), s.capabilities
                 FROM slots s WHERE {scope} AND s.id != ?2",
            scope = self.peer_scope("?2"),
        ))?;
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

        // A seat's id is never ambiguous and never renamed.
        if let Some(p) = rows.iter().find(|p| p.id.to_lowercase() == n) {
            return Ok(p.id.clone());
        }
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
        // A NAME THE SEAT WAS RENAMED FROM. A teammate that learnt the old
        // handle keeps using it, and a send that bounced on it lost the
        // message: "my reply to 'Claude-host-5' bounced" after that seat
        // became 'reviewer'. Only when nobody holds the name now, and only a
        // seat in a room this one shares.
        if partial.is_empty() {
            let was: Option<String> = self
                .db
                .query_row(
                    "SELECT json_extract(detail, '$.slot') FROM audit
                      WHERE action = 'slot_renamed'
                        AND LOWER(json_extract(detail, '$.from')) = ?
                      ORDER BY at_ms DESC LIMIT 1",
                    [&n],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = was.filter(|id| rows.iter().any(|p| &p.id == id)) {
                return Ok(id);
            }
        }
        match partial.len() {
            1 => Ok(partial[0].id.clone()),
            // The guidance is the SAME whether or not a seat by this name
            // exists in another session -- deliberately. A bare "no teammate
            // matching X" reads as a typo and teaches nothing, so it is worth
            // saying why a send can fail and how cross-session reaching
            // actually works: two engineer seats that must collaborate hit
            // exactly this, with the name right and the room not shared. But
            // the message must NOT confirm whether X exists elsewhere, or it
            // becomes an oracle a plain seat could sweep to enumerate other
            // teams -- which `list_peers` refuses to reveal for the same
            // reason. So it names the mechanism, not the seat: the operator
            // or an orchestrator who runs the invite already knows where the
            // seat lives.
            0 => bail!(
                "no teammate matching '{needle}' in a room you share. You can only message \
                 seats in your own team or a team you are a guest in. If '{needle}' is a \
                 seat in another session, it has to share a room with you first -- ask the \
                 operator or an orchestrator to invite it into yours (guesting), after which \
                 send_message reaches it with you as the sender -- or raise the request with \
                 notify_operator."
            ),
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
    /// /srv/app blocked another locking `src/main.rs` under
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
        // FLOOD PROTECTION, on the one funnel every agent-sent message goes
        // through: `send_message`, `broadcast` and `reply` all arrive here,
        // so there is no way round it and no second place to remember.
        //
        // The message is REFUSED rather than dropped, and the refusal says
        // what to do instead. A message silently swallowed teaches an agent
        // nothing and it sends another; an error it can read is the only
        // thing that stops the loop at the end that is looping.
        if let Some(trip) = crate::watchdog::flood_check(
            self.db,
            &self.slot_id,
            to_slot,
            self.flood.0,
            self.flood.1,
        )? {
            let other = trip
                .other
                .as_deref()
                .map(|o| self.label_of(o))
                .unwrap_or_else(|| "that teammate".to_string());
            crate::watchdog::note_flood(
                self.db,
                &self.session_id,
                &self.slot_id,
                &trip,
                &self.my_label(),
            );
            bail!("{}", trip.explain(&other));
        }
        // A message belongs in the ROOM it is addressed into, which is not
        // always the sender's own. A guest seat -- a service answering in
        // several teams -- has one home session, and a message it sends to
        // somebody in another of its rooms has to land in THAT room, or the
        // recipient's team never sees it and the sender's team sees a note
        // about work it has nothing to do with.
        //
        // The return address stays this seat in ITS session, so `reply`
        // still comes back here wherever "here" is.
        let room = match to_slot {
            Some(t) => self
                .db
                .query_row("SELECT session_id FROM slots WHERE id = ?", [t], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .filter(|home| {
                    // Only when BOTH of us answer there. Otherwise it is
                    // this room: a seat must not be able to post into a team
                    // it has nothing to do with by addressing a guest of it.
                    //
                    // Unless it is a SUPERORCHESTRATOR, whose room is the
                    // fleet. The message has to land in the RECIPIENT'S
                    // room or it is never delivered at all: every delivery
                    // query -- `pending_for_slot`, `unclaimed_for_slot` --
                    // asks for messages in the rooms the RECIPIENT answers
                    // in, and the sender's room is not one of them. Written
                    // into the sender's own session it would sit there
                    // forever, reported as sent and read by nobody.
                    //
                    // `reaches` is false for an orchestrator here (its reach
                    // is its own session, which this arm has already
                    // excluded), so this widens exactly one level and
                    // nothing else.
                    home != &self.session_id
                        && (crate::guests::serves(self.db, &self.slot_id, home)
                            || self.authority().reaches(&self.session_id, home))
                })
                .unwrap_or_else(|| self.session_id.clone()),
            None => self.session_id.clone(),
        };
        self.db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms,
                                   reply_to_slot, reply_to_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?2, ?7)",
            params![
                room,
                self.slot_id,
                to_slot,
                serde_json::to_value(kind)?.as_str().unwrap_or("text"),
                body,
                now_ms(),
                self.session_id,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Answer one message, wherever its sender is.
    ///
    /// Three shapes, because a return address can be three things:
    ///
    ///   * a teammate in this session -- an ordinary addressed message;
    ///   * the OPERATOR, who has no seat: the answer goes into the session's
    ///     chat pre-claimed, so the person reading the chat sees it and no
    ///     agent is handed it as work. That is the case that used to be
    ///     answered with `broadcast`, which is why a note for one person was
    ///     read by the whole team;
    ///   * a seat in ANOTHER session, which is only possible for a message
    ///     that was forwarded here from the operator channel. That forward is
    ///     the one sanctioned way a reply crosses a session boundary.
    fn reply_to_message(&self, id: i64, text: &str) -> Result<String> {
        let found: Option<(String, Option<String>, Option<String>, Option<String>, Option<String>)> =
            self.db
                .query_row(
                    "SELECT session_id, to_slot, from_slot, reply_to_slot, reply_to_session
                       FROM messages WHERE id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
        let Some((session, to_slot, from_slot, reply_slot, reply_session)) = found else {
            bail!("no message #{id}");
        };
        // You may answer what was sent to you or to your team, in your own
        // session. Anything else is somebody else's conversation.
        if session != self.session_id {
            bail!("message #{id} is not in this session");
        }
        if let Some(to) = &to_slot {
            if to != &self.slot_id {
                bail!("message #{id} was not addressed to you");
            }
        }
        let target_slot = reply_slot.or(from_slot);
        let target_session = reply_session.unwrap_or_else(|| self.session_id.clone());
        let now = now_ms();
        match target_slot {
            // The operator: no seat, so the answer is a chat line, claimed on
            // arrival so no agent is handed it.
            None => {
                self.db.execute(
                    "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                           sent_at_ms, claimed_at_ms, claimed_by, record,
                                           reply_to_slot, reply_to_session)
                     VALUES (?1, ?2, NULL, 'text', ?3, ?4, ?4, 'operator', 1, ?2, ?1)",
                    params![self.session_id, self.slot_id, text, now],
                )?;
                Ok("Answered the operator in this chat. Your teammates were not sent it.".into())
            }
            Some(slot) if target_session == self.session_id => {
                self.db.execute(
                    "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                           sent_at_ms, reply_to_slot, reply_to_session)
                     VALUES (?1, ?2, ?3, 'text', ?4, ?5, ?2, ?1)",
                    params![self.session_id, self.slot_id, slot, text, now],
                )?;
                Ok(format!("Answered {}.", self.label_of(&slot)))
            }
            // The answer crosses a team boundary: back into the session it
            // came from, addressed to the seat that asked. Two things arrive
            // this way -- an answer to something the operator channel
            // forwarded here, and an answer to a superorchestrator that
            // addressed this seat from another team.
            //
            // FROM_SLOT IS THIS SEAT, not NULL. It was NULL, and a NULL
            // sender is what the whole stack reads as THE OPERATOR: the
            // recipient was told a person was talking to it when a peer was,
            // and the handshake text tells an agent to treat an operator
            // message as the person running the fleet. The one thing the
            // recipient cannot work out for itself is which TEAM answered --
            // a seat in another room is not in its roster -- so that is what
            // the prefix says, instead of repeating a name now attributed
            // properly.
            Some(slot) => {
                let team: String = self
                    .db
                    .query_row(
                        "SELECT COALESCE(name, id) FROM sessions WHERE id = ?",
                        [&self.session_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| self.session_id.clone());
                self.db.execute(
                    "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                           sent_at_ms, reply_to_slot, reply_to_session)
                     VALUES (?1, ?5, ?2, 'text', ?3, ?4, ?5, ?6)",
                    params![
                        target_session,
                        slot,
                        format!("[answering from the {team} team] {text}"),
                        now,
                        self.slot_id,
                        self.session_id
                    ],
                )?;
                Ok("Answered the seat that raised it, in its own session.".into())
            }
        }
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
         -- Every team this seat answers in: see `pending_for_slot`, which
         -- explains why a BROADCAST is only taken from its own team.
         WHERE (m.session_id = ?1
                OR m.session_id IN (SELECT g.session_id FROM slot_sessions g
                                     WHERE g.slot_id = ?2))
           AND (m.to_slot = ?2 OR (m.to_slot IS NULL AND m.session_id = ?1))
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
         -- THE ONE GATE. Every route that hands a message to an agent comes
         -- through here -- the push tick, the typed tick, the interactive
         -- tick, a shim asking, and the participant endpoint -- so a message
         -- the operator withdrew is excluded once, here, rather than in five
         -- queries of which one would eventually be missed.
         WHERE m.cancelled_at_ms IS NULL
         -- Every team this seat answers in, not just the one it lives in. A
         -- service seat -- a capability bridge, a reviewer -- is a guest in
         -- other rooms, and a message sent to it THERE has to arrive.
           AND (m.session_id = ?1
                OR m.session_id IN (SELECT g.session_id FROM slot_sessions g
                                     WHERE g.slot_id = ?2))
           AND m.from_slot IS NOT ?2
           -- A RECORD is not a message: the console's own housekeeping
           -- lines, the node's notes about what it did while nobody was
           -- attached, an answer addressed to the operator. They were being
           -- delivered to agents -- a seat that joined later was typed at
           -- with the whole backlog of them at once.
           --
           -- Filtering on the CLAIM instead looked equivalent and was not:
           -- a team message's state is per RECIPIENT, so one seat claiming
           -- it took it from all the others and `seen` never caught up with
           -- the audience.
           AND m.record = 0
           -- Addressed to me: the message's own state is the whole story.
           -- Addressed to the team: MY copy is, because one teammate acking
           -- must not take it from the rest.
           --
           -- A BROADCAST is only taken from this seat's OWN team (?1). A
           -- service seat visiting several teams -- which is what a
           -- capability bridge or a shared reviewer is -- would otherwise
           -- receive every one of those teams' announcements, and a bridge
           -- invited into seven rooms is then reading seven rooms' traffic
           -- to do a job it is asked for by name. In a room it visits it
           -- answers what it is ASKED; its own team is where it hears
           -- announcements. Address it directly if a visited team needs it
           -- to know something.
           AND ((m.to_slot = ?2 AND m.acked_at_ms IS NULL AND m.inflight_at_ms IS NULL)
                OR (m.to_slot IS NULL AND m.session_id = ?1 AND NOT EXISTS (
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

/// How long a message nobody has consumed waits before it is aged out. A day:
/// long enough that a seat offline overnight still gets its backlog, short
/// enough that a message is never typed at an agent so late it is only noise.
pub const UNDELIVERED_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Age out a message nobody has consumed in `older_than_ms`.
///
/// A teammate's message that has sat undelivered for a day should not suddenly
/// be typed at an agent long after it mattered. This stamps such a message
/// cancelled -- the ONE GATE (`pending_for_slot`) already excludes a cancelled
/// message from every delivery route -- but `by system:expired`, so the row
/// stays in the log and reads as "aged out", not "the operator withdrew it".
///
/// Only what was never consumed is touched; a message that reached its
/// audience is left exactly as it was:
///   - a direct message is unconsumed until it is acked;
///   - a team message is unconsumed while any non-sender slot in its session
///     still has no ack for it -- which also covers one nobody received at
///     all, whose recipient rows do not exist yet.
pub fn expire_undelivered(db: &Connection, older_than_ms: i64) -> Result<usize> {
    let now = now_ms();
    let cutoff = now - older_than_ms;
    Ok(db.execute(
        "UPDATE messages SET cancelled_at_ms = ?1, cancelled_by = 'system:expired'
          WHERE cancelled_at_ms IS NULL
            AND record = 0
            AND sent_at_ms <= ?2
            AND (
                 (to_slot IS NOT NULL AND acked_at_ms IS NULL)
              OR (to_slot IS NULL AND EXISTS (
                    SELECT 1 FROM slots sl
                     WHERE sl.session_id = messages.session_id
                       AND sl.id IS NOT messages.from_slot
                       AND NOT EXISTS (
                             SELECT 1 FROM message_recipients r
                              WHERE r.message_id = messages.id
                                AND r.slot_id = sl.id
                                AND r.acked_at_ms IS NOT NULL)))
            )",
        params![now, cutoff],
    )?)
}

// ------------------------------------------------------------- dispatch ----

pub fn dispatch(ctx: &Ctx, tool: &str, args: &Value) -> Result<String> {
    ctx.touch();
    // WHAT THIS SEAT MAY CALL, checked from the seat itself on every call.
    //
    // The tool list a seat is advertised already follows its authority, but
    // an advertisement is not an authorisation: a model that has seen a tool
    // once will call it again from memory after being demoted, and a model
    // told in a prompt not to do something routes around that the moment it
    // believes it is helping. For an OBSERVER -- a seat that exists to be
    // read and must not be able to steer the team -- that is the difference
    // between a restriction and a request.
    //
    // One indexed lookup per call, on the same slot row the rest of this
    // module already reads.
    let level = ctx.authority();
    if !nexmesh_proto::tools::allowed_for(level, tool) && nexmesh_proto::tools::is_known_tool(tool)
    {
        if level.is_read_only() {
            bail!(
                "'{tool}' is not something this seat can do. It OBSERVES: it reads the \
                 team's state and memory, answers whoever asks it, and can ask a \
                 teammate for its progress with `ask_progress` -- it cannot send a \
                 message of its own, change anything, or touch a seat. That is the \
                 point of the seat, not a misconfiguration; the operator can change \
                 its authority if it was meant to take part."
            );
        }
        // A tool that exists only for an observer, called by a seat that is
        // not one. Harmless if it ran -- `ask_progress` is a restricted
        // `send_message`, which this seat already has -- but a gate with a
        // hole in it is one somebody will add a tool behind.
        if !nexmesh_proto::tools::needs_authority(tool) {
            bail!(
                "'{tool}' is an OBSERVER's tool: the restricted substitute for \
                 `send_message` given to a seat that may only watch. This seat has \
                 `send_message` itself."
            );
        }
        // Otherwise it is a fleet tool, and that arm's own message is better
        // than this one -- it says where authority comes from and who can
        // grant it -- so leave it to answer, below.
    }
    // HOW OFTEN THIS SEAT ASKS FOR WHAT. A prompt can tell a seat to use the
    // code graph and the team's memory; only a count shows whether it did,
    // or went on reading the tree and forgetting. One upsert per call.
    if nexmesh_proto::tools::is_known_tool(tool) {
        let _ = ctx.db.execute(
            "INSERT INTO tool_counts (slot_id, tool, n, last_ms) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(slot_id, tool) DO UPDATE SET n = n + 1, last_ms = ?3",
            params![ctx.slot_id, tool, now_ms()],
        );
    }
    match tool {
        "list_peers" => list_peers(ctx, args),
        "send_message" => send_message(ctx, args),
        "broadcast" => broadcast(ctx, args),
        "reply" => {
            let id = args
                .get("to")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow::anyhow!("reply needs 'to': the message's number"))?;
            let text = arg_str(args, "text")?;
            ctx.reply_to_message(id, &text)
        }
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
        "who_owns" => who_owns(ctx, args),
        "store_knowledge" => store_knowledge(ctx, args),
        "query_knowledge" => query_knowledge(ctx, args),
        "store_memory" => store_memory(ctx, args),
        "recall" => recall(ctx, args),
        "forget" => forget_memory(ctx, args),
        "memory_history" => memory_history(ctx, args),
        "save_context" => save_context(ctx, args),
        "fold_context" => fold_context(ctx, args),
        "unfold_context" => unfold_context(ctx, args),
        "search_context" => search_context(ctx, args),
        "context_map" => context_map(ctx),
        "pending_asks" => pending_asks(ctx),
        "answer_ask" => answer_ask(ctx, args),
        "get_history" => get_history(ctx, args),
        "team_rules" => team_rules(ctx),
        "ask_progress" => ask_progress(ctx, args),

        // The fleet tools. Authority is checked HERE, from the calling slot,
        // and not inferred from anything the caller sent: these are only
        // advertised to a seat that holds it (see
        // `proto::tools::team_tools_for`), but a model that has seen a tool
        // once will call it again from memory after being demoted, and an
        // advertisement is not an authorisation.
        name if nexmesh_proto::tools::needs_authority(name) => {
            let caller = crate::fleet::authority_of(ctx.db, &ctx.slot_id);
            if !caller.is_orchestrating() {
                bail!(
                    "'{name}' is an orchestrator's tool, and this seat is a plain agent. \
                     Authority lives on the seat in the hub: the operator grants it from \
                     the dashboard, or an orchestrator of this session can with `elect`."
                );
            }
            let db = ctx.db;
            let (slot, sess) = (ctx.slot_id.as_str(), ctx.session_id.as_str());
            match name {
                "elect" => crate::fleet::elect(
                    db, slot, sess, &arg_str(args, "agent")?, &arg_str(args, "authority")?),
                "list_seats" => crate::fleet::list_seats(db, slot, sess),
                "invite_agent" => crate::fleet::invite_agent(
                    db, slot, sess, &arg_str(args, "agent")?, &arg_str(args, "session")?,
                    args.get("leave").and_then(|v| v.as_bool()).unwrap_or(false)),
                "add_agent" => crate::fleet::add_agent(db, slot, sess, args),
                "remove_agent" => crate::fleet::remove_agent(
                    db, slot, sess, &arg_str(args, "agent")?),
                "rename_agent" => crate::fleet::rename_agent(
                    db, slot, sess, &arg_str(args, "agent")?, &arg_str(args, "name")?),
                "move_agent" => crate::fleet::move_agent(
                    db, slot, sess, &arg_str(args, "agent")?, &arg_str(args, "session")?),
                "set_team_rules" => set_team_rules(ctx, args),
                // `needs_authority` said yes and this match did not: a tool
                // was added to the orchestrator surface without being wired
                // up here.
                _ => bail!("'{name}' is not implemented on this hub"),
            }
        }
        other => bail!("unknown tool '{other}'"),
    }
}

/// The team's standing rules, as they stand.
fn team_rules(ctx: &Ctx) -> Result<String> {
    let text: Option<String> = ctx
        .db
        .query_row(
            "SELECT instructions FROM sessions WHERE id = ?",
            [&ctx.session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    match text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok("This team has no standing rules set. Work to your own brief.".into()),
        Some(t) => Ok(format!(
            "THIS TEAM'S RULES, set for everybody working here. They outrank your own \
             preferences about how to work.\n\n{t}"
        )),
    }
}

/// Replace the team's standing rules.
///
/// Reachable only from a seat holding orchestrator or superorchestrator
/// authority -- `needs_authority` puts it behind the same gate as the rest of
/// the fleet tools. Rules an ordinary agent can rewrite are not rules, and an
/// agent that has misread them would be the first to edit them.
///
/// Everybody already running is TOLD, in the chat. Without that, a team
/// carries on to the old rules with nothing to distinguish the agents that
/// know from the agents that do not -- and the rules only reach a seat's
/// prompt when its next process starts, which may be days.
fn set_team_rules(ctx: &Ctx, args: &Value) -> Result<String> {
    let text = arg_str(args, "text").unwrap_or_default();
    let text = text.trim();
    if text.len() > MAX_TEAM_RULES {
        bail!(
            "that is {} KiB of rules; the limit is {} KiB. Rules nobody can hold in \
             their head are rules nobody follows -- put the detail in \
             `store_knowledge` and keep these short.",
            text.len() / 1024,
            MAX_TEAM_RULES / 1024
        );
    }
    let me = ctx.my_label();
    let before: Option<String> = ctx
        .db
        .query_row(
            "SELECT instructions FROM sessions WHERE id = ?",
            [&ctx.session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if before.as_deref().map(str::trim).unwrap_or("") == text {
        return Ok("Those are already this team's rules; nothing changed.".into());
    }
    ctx.db.execute(
        "UPDATE sessions SET instructions = ? WHERE id = ?",
        params![if text.is_empty() { None } else { Some(text) }, ctx.session_id],
    )?;
    ctx.db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![
            now_ms(),
            me,
            "team_rules_set",
            serde_json::json!({ "session": ctx.session_id, "bytes": text.len() })
                .to_string()
        ],
    )?;
    let said = if text.is_empty() {
        format!("{me} has cleared this team's standing rules.")
    } else {
        format!(
            "{me} has changed this team's standing rules. They apply to everybody here \
             from now on -- call `team_rules` to read them.\n\n{text}"
        )
    };
    ctx.db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, NULL, 'system', ?, ?)",
        params![ctx.session_id, said, now_ms()],
    )?;
    Ok(if text.is_empty() {
        "Cleared. Everybody here has been told.".to_string()
    } else {
        format!(
            "Set, {} characters. Everybody here has been told, and every seat started \
             from now on gets them in its first prompt.",
            text.len()
        )
    })
}

/// Rules longer than this are not rules. See `set_team_rules`.
const MAX_TEAM_RULES: usize = 8 * 1024;

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
    slot: String,
    /// The parts of the workspace this teammate is responsible for.
    zones: Vec<String>,
    /// Whose team this one actually lives in, when that is not the same as
    /// the caller's. A service seat visiting several teams is addressable
    /// here but does not belong to this room, and that changes what it is
    /// reasonable to ask of it.
    elsewhere: Option<String>,
}

fn list_peers(ctx: &Ctx, args: &Value) -> Result<String> {
    let role_filter = opt_str(args, "role");
    let cap_filter = opt_str(args, "capability").map(|c| c.trim().to_lowercase());
    let mut stmt = ctx.db.prepare(&format!(
        // Everyone I share a room with -- THE SAME PREDICATE `resolve_peer`
        // uses, from `peer_scope`, so what is LISTED and what can be
        // ADDRESSED cannot drift apart. It used to be a second copy of it
        // here, which drifted. A service seat visiting this team appears
        // here; so does a team this seat is visiting.
        "SELECT COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.task_state, s.summary, s.participant_kind,
                COALESCE(n.name, CASE WHEN a.provenance = 'external' THEN 'external' ELSE '-' END),
                COALESCE(a.status, 'disconnected'), s.capabilities, a.model, s.model, s.id,
                s.session_id <> (SELECT session_id FROM slots WHERE id = ?1),
                COALESCE(se.name, s.session_id)
         FROM slots s
         -- ONE process per seat. Joining every live agent listed a seat twice
         -- whenever it briefly had two -- a reattach before the old one was
         -- written off, a twin -- and a reader took it for two teammates.
         LEFT JOIN agents a ON a.id = (
            SELECT id FROM agents WHERE slot_id = s.id AND exited_at_ms IS NULL
             ORDER BY started_at_ms DESC LIMIT 1)
         LEFT JOIN nodes n ON n.id = a.node_id
         LEFT JOIN sessions se ON se.id = s.session_id
         WHERE {scope} AND s.id != ?1
         ORDER BY 1",
        scope = ctx.peer_scope("?1"),
    ))?;
    let rows: Vec<PeerRow> = stmt
        .query_map(params![ctx.slot_id], |r| {
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
                slot: r.get(10)?,
                zones: Vec::new(),
                elsewhere: if r.get::<_, bool>(11)? { r.get(12)? } else { None },
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);

    // Ownership zones, per teammate, in the workspace this seat is in.
    let mut rows = rows;
    {
        let mut z = ctx.db.prepare(
            "SELECT o.slot_id, o.pattern FROM ownership o
              WHERE o.workspace_id = (SELECT workspace_id FROM slots WHERE id = ?)
              ORDER BY o.pattern",
        )?;
        let owned: Vec<(String, String)> = z
            .query_map([&ctx.slot_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for p in rows.iter_mut() {
            p.zones = owned
                .iter()
                .filter(|(slot, _)| *slot == p.slot)
                .map(|(_, pat)| pat.clone())
                .collect();
        }
    }

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
        // A seat whose home is another team. Said out loud, because it is
        // addressable like a teammate and is not one: what it is reasonable
        // to ask of it -- and what it already knows from its other rooms --
        // is different.
        //
        // Two different things end up here and must not read the same. A
        // seat INVITED into this room is a service visiting it. A
        // superorchestrator sees every team whether or not anyone invited
        // anyone, and calling those "visiting from" told it twelve seats
        // were guests of its room when none of them were -- so it would
        // address another team's engineer as if that team had lent it out.
        if let Some(home) = &p.elsewhere {
            if crate::guests::serves(ctx.db, &p.slot, &ctx.session_id) {
                out.push_str(&format!(" [a service, visiting from {home}]"));
            } else {
                out.push_str(&format!(" [on the {home} team; you reach it as a superorchestrator]"));
            }
        }
        out.push('\n');
        // What this teammate is responsible for. Without it, a team that
        // divided a repository between its agents had no way for one of them
        // to know -- so the engineer with the whole checkout in front of it
        // edited somebody else's container and found out at review.
        if !p.zones.is_empty() {
            out.push_str(&format!("    owns {}\n", p.zones.join(", ")));
        }
        if !p.summary.is_empty() {
            out.push_str(&format!("    {}\n", p.summary));
        }
    }
    out.push_str(
        "Work inside somebody's zone belongs to them: ask with send_message rather than \
         editing it. who_owns <path> answers for one path.\n",
    );
    Ok(out)
}

/// Who is responsible for a path, and what to do about it.
///
/// The question an agent with a whole checkout in front of it has to be able
/// to ask before it edits anything: a team that divided a repository between
/// its agents gains nothing if each of them can only find that out by being
/// told off at review.
fn who_owns(ctx: &Ctx, args: &Value) -> Result<String> {
    let path = arg_str(args, "path")?;
    let workspace: Option<String> = ctx
        .db
        .query_row("SELECT workspace_id FROM slots WHERE id = ?", [&ctx.slot_id], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(ws) = workspace else {
        return Ok(format!(
            "This seat is not in a workspace, so nothing here owns {path}."
        ));
    };
    match owner_of(ctx.db, &ws, &path)? {
        None => Ok(format!(
            "Nobody owns {path}. It is unassigned: take a lock with acquire_file before \
             editing it, and say so with send_message if it is somebody's area in spirit."
        )),
        Some((slot, _who, pattern)) if slot == ctx.slot_id => Ok(format!(
            "You own {path}, through your zone `{pattern}`. Go ahead."
        )),
        Some((_, who, pattern)) => Ok(format!(
            "{who} owns {path}, through their zone `{pattern}`.\n\
             That work is theirs: send_message to {who} with what needs doing and why, \
             rather than editing it yourself. If it genuinely has to be you, say so to \
             {who} first and take a lock with acquire_file."
        )),
    }
}

fn send_message(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "to")?;
    let text = arg_str(args, "text")?;
    let target = ctx.resolve_peer(&to)?;
    ctx.insert_message(Some(&target), MessageKind::Text, &text)?;
    // Queued either way -- but a sender waiting on an answer from a seat that
    // cannot reach its model is told, rather than left to wait.
    let held: Option<(String, Option<i64>)> = ctx
        .db
        .query_row(
            "SELECT hold_reason, hold_until_ms FROM slots WHERE id = ? AND hold_reason IS NOT NULL",
            [&target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let who = ctx.label_of(&target);
    Ok(match held {
        Some((reason, until)) => format!(
            "Queued for {who}, who is HELD: {}. It gets this when it is back{}; if it is \
             urgent, give the work to someone else.",
            if reason == "quota" { "its account is out of model allowance" }
            else { "it cannot reach its model" },
            until.map(|t| format!(", at {}", crate::quota::when(t))).unwrap_or_default()
        ),
        None => format!("Sent to {who}."),
    })
}

/// An observer's one way of putting something in front of a teammate.
///
/// THE QUESTION IS FIXED. An observer with a free-text message is
/// `send_message` with extra steps, and watching a team turns into steering
/// it one helpful suggestion at a time. So the caller chooses WHICH of three
/// questions to ask and contributes no words of its own; the text below is
/// the hub's, and it says plainly that a short answer is the right answer.
///
/// Available to anybody in principle -- nothing breaks if an orchestrator
/// calls it -- but only advertised to an observer, which has no other way to
/// ask. See `proto::tools::observer_tools`.
fn ask_progress(ctx: &Ctx, args: &Value) -> Result<String> {
    let to = arg_str(args, "agent")?;
    let want = args
        .get("want")
        .and_then(|v| v.as_str())
        .unwrap_or("progress")
        .trim()
        .to_lowercase();
    let question = match want.as_str() {
        "tests" | "test" | "test_results" | "build" => {
            "what the last build or test run did: what you ran, and what it said. \
             The result, not an explanation of it"
        }
        "eta" | "when" | "estimate" => {
            "when you expect this to be done, and what would change that"
        }
        "progress" | "status" | "" => {
            "what state the work is in: what is finished, what you are on now, and \
             anything you are blocked on"
        }
        other => bail!(
            "'{other}' is not something to ask for. It is 'progress', 'tests' or 'eta'."
        ),
    };
    let target = ctx.resolve_peer(&to)?;
    let me = ctx.my_label();
    let text = format!(
        "{me} is watching this team on somebody's behalf and asks for {question}. \
         Answer it with `reply` in a line or two and carry on -- it cannot give you \
         instructions and nothing it says changes your work."
    );
    ctx.insert_message(Some(&target), MessageKind::Text, &text)?;
    Ok(format!(
        "Asked {} for {}. The answer will arrive as an ordinary message.",
        ctx.label_of(&target),
        if want.is_empty() { "progress" } else { want.as_str() }
    ))
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
                s.id, a.activity, s.paused, s.hold_reason, s.hold_until_ms
         FROM slots s
         LEFT JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE s.session_id = ? ORDER BY 1",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, String, String, String, String, Option<String>, i64,
                   Option<String>, Option<i64>)> = stmt
        .query_map([&ctx.session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?, r.get(8)?, r.get(9)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = String::from("Team:\n");
    for (name, role, state, summary, host, id, activity, paused, hold, until) in rows {
        let me = if id == ctx.slot_id { "  <- you" } else { "" };
        // What it is doing NOW, beside what it is working on. A seat that
        // cannot reach its model is said plainly, so nobody hands it work.
        let now = match (hold.as_deref(), paused, activity.as_deref()) {
            (Some("quota"), _, _) => format!(
                " -- HELD: its account is out of model allowance{}; do not assign it work",
                until.map(|t| format!(" until {}", crate::quota::when(t))).unwrap_or_default()
            ),
            (Some(_), _, _) => " -- HELD: it cannot reach its model; do not assign it work".into(),
            (None, 1, _) => " -- paused".into(),
            (None, _, Some("active")) => " -- working on a turn now".into(),
            (None, _, Some("blocked")) => " -- waiting on a question only a person can answer".into(),
            (None, _, Some("idle")) => " -- idle".into(),
            _ => String::new(),
        };
        out.push_str(&format!("- {name} [{role}] on {host}: {state}{now}{me}\n"));
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

    // And to any seat that SUPERVISES the fleet, in its own session, as an
    // ordinary message it can read and answer.
    //
    // The operator channel has no read tool for seats: a seat acting as
    // orchestrator could shout into it and never hear a thing, so a request
    // raised to the supervisors sat there unseen while three agents reported
    // that no orchestrator was reachable. The forward carries the asker as
    // its return address, which is what lets `reply` cross back.
    // supervisor = 1 is an OPT-IN flag a seat sets with `/supervisor`, and a
    // fleet can easily have none -- which is exactly how a superorchestrator
    // existed, saw the whole fleet, and still never received a single raised
    // ask: the forward looked only for the opt-in flag it had never set. A
    // seat with super authority supervises the fleet by definition, so it
    // hears these whether or not anyone flagged it.
    let supervisors: Vec<(String, String)> = {
        let mut q = ctx.db.prepare(
            "SELECT s.id, s.session_id FROM slots s
              WHERE (s.supervisor = 1 OR s.authority = 'super') AND s.session_id <> ?1",
        )?;
        let rows = q
            .query_map([&ctx.session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    let reached = supervisors.len();
    for (slot, session) in supervisors {
        let body = format!(
            "[raised to the supervisors] {me}, in session '{}', says: {text}\n\n\
             Answer it with reply(to: <this message's number>) -- that reaches {me} \
             in its own session. Nobody else is listening to this one.",
            ctx.session_id
        );
        ctx.db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms,
                                   reply_to_slot, reply_to_session)
             VALUES (?1, NULL, ?2, 'text', ?3, ?4, ?5, ?6)",
            params![session, slot, body, now, ctx.slot_id, ctx.session_id],
        )?;
    }

    // Tell the caller what actually happened, including the case the old
    // text papered over: a raise that reached nobody who can read it. The
    // operator_messages row is always written for the dashboard, so it is
    // never lost -- but "watch for a reply" is a promise only someone
    // listening can keep, and a seat left watching a channel no one is on
    // is the stall this path was meant to end.
    Ok(match (to_session, reached) {
        (Some(_), _) => format!(
            "Raised with this session's orchestrator ({why}). Watch for a reply with \
             check_messages, and carry on with anything that does not depend on it."
        ),
        (None, 0) => format!(
            "Logged for the operator to read on the dashboard ({why}). No supervisor seat \
             is currently listening, so do not wait on a reply here -- carry on, and raise \
             it again or ask the operator directly if it blocks you."
        ),
        (None, n) => format!(
            "Raised with {n} supervisor seat(s) who can see the whole fleet ({why}). Watch \
             for a reply with check_messages, and carry on with anything that does not \
             depend on it."
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
    // A lock is not permission. If this path is in somebody else's zone, the
    // work is theirs, and saying so here is the last moment anything will --
    // the next thing that happens is an edit.
    let workspace: Option<String> = ctx
        .db
        .query_row("SELECT workspace_id FROM slots WHERE id = ?", [&ctx.slot_id], |r| r.get(0))
        .optional()?
        .flatten();
    if let Some(ws) = workspace {
        if let Some((slot, who, pattern)) = owner_of(ctx.db, &ws, &path)? {
            if slot != ctx.slot_id {
                return Ok(format!(
                    "Locked {path} for 5 minutes.\n\
                     NOTE: {who} owns this, through their zone `{pattern}`. If this is their \
                     work, hand it to them with send_message and release the lock -- editing \
                     inside somebody's zone is how two agents undo each other."
                ));
            }
        }
    }
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
    // Session unless told otherwise, which is what it has always meant --
    // but a fact about the fleet rather than this team, or a note only this
    // seat should keep, no longer needs a different tool to say so.
    let scope = scope_of(ctx, args)?;
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
    // Everything this seat can see unless a scope is named. It used to read
    // the session and nothing else, so an agent that called it on arrival --
    // which the skill tells it to -- never saw a fleet-wide convention or its
    // own notes from its last run, and had no way to know they were there.
    render_entries(ctx, crate::memory::recall(
        ctx.db,
        &ctx.slot_id,
        &ctx.session_id,
        opt_str(args, "key").as_deref(),
        opt_str(args, "search").as_deref(),
        opt_str(args, "category").as_deref(),
        opt_str(args, "scope").as_deref(),
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

// -------------------------------------------------------------- folds ----
//
// What a stretch of context established, kept after the context is gone. See
// `folds.rs` for the shape of it; these four are the agent's side.

fn fold_context(ctx: &Ctx, args: &Value) -> Result<String> {
    // A fold defaults to the seat's OWN scope rather than the team's. Most
    // of what an agent folds is its own working context, and a team whose
    // every seat folded into the session memory would bury what the team
    // actually needs to know under everybody's housekeeping.
    let scope = crate::memory::Scope::parse(
        opt_str(args, "scope").as_deref().or(Some("agent")),
        opt_str(args, "group").as_deref(),
        &ctx.slot_id,
        &ctx.session_id,
    )?;
    crate::memory::may_write(ctx.db, &ctx.slot_id, &scope)?;
    let covers: Vec<String> = args
        .get("covers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let detail = opt_str(args, "detail").unwrap_or_default();
    let agent_id: Option<String> = ctx
        .db
        .query_row(
            "SELECT id FROM agents WHERE slot_id = ? AND exited_at_ms IS NULL
              ORDER BY started_at_ms DESC LIMIT 1",
            [&ctx.slot_id],
            |r| r.get(0),
        )
        .optional()?;
    let f = crate::folds::fold(
        ctx.db,
        &scope,
        &ctx.slot_id,
        agent_id.as_deref(),
        &arg_str(args, "topic")?,
        &arg_str(args, "summary")?,
        &detail,
        &covers,
        args.get("tokens_before").and_then(|v| v.as_i64()),
    )?;
    let mut said = format!("Folded as {} (tier {}, {} memory). ", f.id, f.tier, f.scope);
    if f.tokens_before > f.tokens_after && f.tokens_after > 0 {
        said.push_str(&format!(
            "About {} tokens of detail now cost you {} to carry. ",
            f.tokens_before, f.tokens_after
        ));
    } else if f.tokens_after >= f.tokens_before && f.covers.is_empty() {
        // Said plainly, because an agent folding small stretches is spending
        // context rather than saving it, and nothing else would tell it.
        said.push_str(&format!(
            "Note that this fold costs about {} tokens to carry and put away about {}:              folding a stretch this small gains nothing. Fold at the end of a piece of              work, not after every step. ",
            f.tokens_after, f.tokens_before
        ));
    }
    if !covers.is_empty() {
        said.push_str(&format!("{} earlier fold(s) now sit underneath it. ", covers.len()));
    }
    said.push_str(
        "You can let that detail go: `unfold_context` with that id gives it back in full, \
         and `search_context` finds it by its words.",
    );
    Ok(said)
}

fn unfold_context(ctx: &Ctx, args: &Value) -> Result<String> {
    let id = arg_str(args, "id")?;
    let allowed = crate::memory::readable(ctx.db, &ctx.slot_id, &ctx.session_id)?;
    let Some(f) = crate::folds::get(ctx.db, &id)? else {
        bail!("there is no fold '{id}'; `search_context` or `context_map` lists what there is");
    };
    // Checked against the readable list, the same way the memory store does
    // it: a scope that is not in that list cannot be reached by passing an id.
    let may = allowed.iter().any(|sc| sc.name() == f.scope && sc.reference() == f.scope_ref);
    if !may {
        bail!(
            "'{id}' is in another seat's private context. Only what you can see is \
             yours to unfold."
        );
    }
    crate::folds::note_unfolded(ctx.db, &id)?;
    let mut out = format!("{} -- {}\n{}\n", f.id, f.topic, f.summary);
    let kids = crate::folds::children(ctx.db, &id)?;
    if !kids.is_empty() {
        out.push_str(&format!(
            "\nThis fold covers {} earlier fold(s). Unfold one of these to go further down:\n",
            kids.len()
        ));
        for k in &kids {
            out.push_str(&format!("- {} (tier {}) {}: {}\n", k.id, k.tier, k.topic, k.summary));
        }
    }
    if !f.detail.trim().is_empty() {
        out.push_str("\nThe detail, as it was handed over:\n");
        out.push_str(&f.detail);
        out.push('\n');
    } else if kids.is_empty() {
        out.push_str("\nThis fold kept no detail beyond its summary.\n");
    }
    Ok(out)
}

fn search_context(ctx: &Ctx, args: &Value) -> Result<String> {
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10).clamp(1, 50);
    let scopes = match opt_str(args, "scope") {
        Some(_) => vec![scope_of(ctx, args)?],
        None => crate::memory::readable(ctx.db, &ctx.slot_id, &ctx.session_id)?,
    };
    let query = arg_str(args, "search")?;
    let hits = crate::folds::search(ctx.db, &scopes, &query, limit)?;
    if hits.is_empty() {
        return Ok(format!(
            "Nothing folded mentions '{query}'. It may never have been folded, in which \
             case it is still in your own context or in the files."
        ));
    }
    let mut out = format!("{} fold(s) mention '{query}':\n", hits.len());
    for f in &hits {
        out.push_str(&format!(
            "- {} (tier {}, {}) {}: {}\n",
            f.id, f.tier, f.scope, f.topic, f.summary
        ));
    }
    out.push_str("`unfold_context` with an id gives back the detail underneath it.\n");
    Ok(out)
}

fn context_map(ctx: &Ctx) -> Result<String> {
    let mut out = String::new();
    // How full it is, as the hub last measured it -- read off the CLI's own
    // conversation where the CLI reports one, never estimated here.
    let row: Option<(Option<i64>, Option<i64>, Option<String>)> = ctx
        .db
        .query_row(
            "SELECT context_used_tokens, context_limit_tokens, context_source
               FROM agents WHERE slot_id = ? AND exited_at_ms IS NULL
              ORDER BY started_at_ms DESC LIMIT 1",
            [&ctx.slot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        Some((Some(used), Some(limit), source)) if limit > 0 => {
            out.push_str(&format!(
                "Your context: {used} of {limit} tokens, {}% full (read from {}).\n",
                used.saturating_mul(100) / limit,
                source.unwrap_or_else(|| "your CLI".into())
            ));
        }
        _ => out.push_str(
            "How full your context is cannot be read for this CLI, so nothing here is \
             measured against a window. Fold by the rule instead: whatever the step you \
             are on no longer needs.\n",
        ),
    }

    let scopes = crate::memory::readable(ctx.db, &ctx.slot_id, &ctx.session_id)?;
    let mut any = false;
    for sc in &scopes {
        let shape = crate::folds::shape(ctx.db, sc)?;
        if shape.count() == 0 {
            continue;
        }
        any = true;
        out.push_str(&format!(
            "\n{} memory: {} fold(s), carrying about {} tokens of summary over about {} \
             tokens of detail.\n",
            sc.name(),
            shape.count(),
            shape.carried(),
            shape.folded()
        ));
        for (tier, n, carried, folded) in &shape.tiers {
            out.push_str(&format!(
                "  tier {tier}: {n} fold(s), {carried} carried, {folded} folded\n"
            ));
        }
    }
    if !any {
        out.push_str(
            "\nNothing has been folded yet. `fold_context` is how a stretch of work stops \
             costing you context without being lost.\n",
        );
    } else {
        let recent = crate::folds::visible(ctx.db, &scopes, 12)?;
        out.push_str("\nMost recent, newest first:\n");
        let now = nexmesh_proto::now_ms();
        for f in &recent {
            let mins = (now - f.created_at_ms) / 60_000;
            let when = if mins < 1 {
                "just now".to_string()
            } else if mins < 90 {
                format!("{mins}m ago")
            } else {
                format!("{}h ago", mins / 60)
            };
            out.push_str(&format!(
                "- {} (tier {}, {}, {when}) {}\n", f.id, f.tier, f.scope, f.topic
            ));
        }
        out.push_str(
            "\nWhen one scope carries many tier-1 folds, fold them into one with `covers`: \
             that is what keeps a long job down to a handful of summaries.\n",
        );
    }
    out.push_str(&format!("\n{}\n", crate::folds::DOCTRINE));
    Ok(out)
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
    // WHAT THIS SEAT COULD HAVE SEEN, not everything in the room.
    //
    // This used to return every message in the session, so any agent that
    // called it read the whole room: two other seats' private exchange, the
    // operator's replies to somebody else, and every `record` line -- the
    // console's housekeeping, the node's notes about what it did while
    // nobody was attached, what the operator decided on an approval. None
    // of that was ever delivered to this agent, and history is not a way
    // around that.
    //
    // Three things qualify: addressed to me, addressed to the whole team,
    // or sent by me. The same test `pending_for_slot` applies at delivery,
    // so what an agent can read later matches what it could be handed at
    // the time.
    let mut stmt = ctx.db.prepare(
        "SELECT COALESCE(s.display_name, s.role, 'orchestrator'), m.kind, m.body
         FROM messages m LEFT JOIN slots s ON s.id = m.from_slot
         WHERE (m.session_id = ?1
                OR m.session_id IN (SELECT g.session_id FROM slot_sessions g
                                     WHERE g.slot_id = ?2))
           AND m.record = 0
           AND (m.to_slot = ?2 OR m.to_slot IS NULL OR m.from_slot = ?2)
         ORDER BY m.id DESC LIMIT ?3",
    )?;
    let mut rows: Vec<(String, String, String)> = stmt
        .query_map(params![ctx.session_id, ctx.slot_id, limit], |r| {
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

    /// A send to a seat's id, or to a name it was renamed from, reaches it.
    #[test]
    fn a_seat_is_reached_by_its_id_and_by_a_name_it_used_to_have() {
        let db = fixture();
        let ctx = Ctx { db: &db, session_id: "s1".into(), slot_id: "sl-eng".into(),
                        flood: (0, 0) };
        assert_eq!(ctx.resolve_peer("sl-rev").unwrap(), "sl-rev");
        db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail)
             VALUES (1, 'operator', NULL, 'slot_renamed',
                     '{\"slot\":\"sl-rev\",\"from\":\"Claude-box-5\",\"to\":\"Iris\"}')",
            [],
        )
        .unwrap();
        assert_eq!(ctx.resolve_peer("claude-box-5").unwrap(), "sl-rev");
        // A name nobody ever had still bounces, with the guidance.
        assert!(ctx.resolve_peer("nobody-at-all").is_err());
    }

    /// One seat claiming a TEAM message must not take it from the others.
    ///
    /// Records are kept out of delivery by a flag of their own. Filtering on
    /// the CLAIM instead looked equivalent and was not: a team message's
    /// state is per recipient, and claiming is per message, so the first seat
    /// to claim one emptied it for everybody else and the audience never
    /// caught up. CI found it; this keeps it found.
    #[test]
    fn claiming_a_team_message_leaves_it_pending_for_the_other_seats() {
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', NULL, NULL, 'text', 'who is awake in here?', ?)",
            params![now],
        )
        .unwrap();
        let id = db.last_insert_rowid();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().iter().any(|m| m.id == id));
        assert!(pending_for_slot(&db, "s1", "sl-rev").unwrap().iter().any(|m| m.id == id));
        // The engineer deals with it...
        claim_messages(&db, "Max", &[id]).unwrap();
        // ...and the reviewer has still not seen it.
        assert!(
            pending_for_slot(&db, "s1", "sl-rev").unwrap().iter().any(|m| m.id == id),
            "one seat claiming a team message took it from the rest"
        );
    }

    /// History is what THIS SEAT could have seen, not the whole room.
    ///
    /// `get_history` used to return every message in the session, so any
    /// agent that called it read two other seats' private exchange, the
    /// operator's replies to somebody else, and every `record` line -- the
    /// console's housekeeping, the node's notes about what it did unattended,
    /// what the operator decided on an approval. None of that was ever
    /// delivered to it, and history must not be a way around delivery.
    #[test]
    fn history_shows_only_what_this_seat_could_have_seen() {
        let db = fixture();
        let now = now_ms();
        let say = |from: Option<&str>, to: Option<&str>, body: &str, record: i64| {
            db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                       sent_at_ms, record)
                 VALUES ('s1', ?1, ?2, 'text', ?3, ?4, ?5)",
                params![from, to, body, now, record],
            )
            .unwrap();
        };
        say(None, None, "everyone hear this", 0);              // team
        say(None, Some("sl-eng"), "for the engineer", 0);      // to me
        say(Some("sl-eng"), Some("sl-rev"), "from me to them", 0); // by me
        say(Some("sl-rev"), Some("sl-oth"), "between two others", 0);
        say(None, Some("sl-rev"), "for the reviewer alone", 0);
        say(None, None, "console housekeeping", 1);            // a record

        let ctx = Ctx { db: &db, session_id: "s1".into(), slot_id: "sl-eng".into(),
                        flood: (0, 0) };
        let out = get_history(&ctx, &json!({})).unwrap();

        assert!(out.contains("everyone hear this"), "{out}");
        assert!(out.contains("for the engineer"), "{out}");
        assert!(out.contains("from me to them"), "{out}");
        // The three it was never handed.
        assert!(!out.contains("between two others"), "it read another pair's exchange:\n{out}");
        assert!(!out.contains("for the reviewer alone"),
                "it read a message addressed to somebody else:\n{out}");
        assert!(!out.contains("console housekeeping"),
                "it read the operator's own housekeeping:\n{out}");
    }

    /// Answering is `reply`, and it goes to ONE place.
    ///
    /// Before this, delivery told an agent to answer the operator with
    /// `broadcast` -- "that is what they see" -- so every exchange with one
    /// agent was read by the whole team. A message now carries its own return
    /// address and `reply` uses it.
    #[test]
    fn replying_to_a_teammate_reaches_that_teammate_only() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "can you look at auth.rs?"})).unwrap();
        let id: i64 = db
            .query_row("SELECT id FROM messages WHERE body LIKE '%auth.rs%'", [], |r| r.get(0))
            .unwrap();
        let said = dispatch(&ctx(&db, "sl-eng"), "reply",
                            &json!({"to": id, "text": "looking now"})).unwrap();
        assert!(said.contains("Iris"), "{said}");
        let (to, claimed): (Option<String>, Option<i64>) = db
            .query_row("SELECT to_slot, claimed_at_ms FROM messages WHERE body = 'looking now'",
                       [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(to.as_deref(), Some("sl-rev"), "a reply is addressed, not broadcast");
        assert!(claimed.is_none(), "a teammate's reply is a real message for them");
    }

    #[test]
    fn replying_to_the_operator_lands_in_the_chat_and_not_on_the_team() {
        let db = fixture();
        let now = now_ms();
        // The operator has no seat: from_slot NULL is how they appear.
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', NULL, 'sl-eng', 'text', 'what is the status?', ?)",
            params![now],
        ).unwrap();
        let id: i64 = db
            .query_row("SELECT id FROM messages WHERE body = 'what is the status?'", [],
                       |r| r.get(0)).unwrap();
        let said = dispatch(&ctx(&db, "sl-eng"), "reply",
                            &json!({"to": id, "text": "auth is done"})).unwrap();
        assert!(said.contains("operator"), "{said}");
        let (to, claimed, by): (Option<String>, Option<i64>, Option<String>) = db
            .query_row("SELECT to_slot, claimed_at_ms, claimed_by FROM messages
                         WHERE body = 'auth is done'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert!(to.is_none(), "it goes in the chat the operator is reading");
        assert!(claimed.is_some() && by.as_deref() == Some("operator"),
                "and pre-claimed, so no agent is handed it: {claimed:?} {by:?}");
        // The proof of the complaint being fixed: it is not pending for a peer.
        let peers = pending_for_slot(&db, "s1", "sl-rev").unwrap();
        assert!(!peers.iter().any(|m| m.body == "auth is done"),
                "a reply to the operator must not reach the rest of the team");
    }

    #[test]
    fn you_cannot_answer_somebody_elses_message() {
        let db = fixture();
        dispatch(&ctx(&db, "sl-rev"), "send_message",
                 &json!({"to": "Max", "text": "for Max alone"})).unwrap();
        let id: i64 = db
            .query_row("SELECT id FROM messages WHERE body = 'for Max alone'", [], |r| r.get(0))
            .unwrap();
        let err = dispatch(&ctx(&db, "sl-ext"), "reply",
                           &json!({"to": id, "text": "me neither"})).unwrap_err();
        assert!(err.to_string().contains("not addressed to you"), "{err}");
    }

    /// A seat marked as a supervisor hears what agents raise to the
    /// supervisors, and can answer it across the session boundary -- the one
    /// sanctioned way a reply crosses one.
    #[test]
    fn what_is_raised_to_the_supervisors_reaches_a_supervisor_seat_and_can_be_answered() {
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO sessions (id, name, created_at_ms, last_active_ms)
             VALUES ('s2','supervisors',?,?)",
            params![now, now],
        ).unwrap();
        db.execute(
            "INSERT INTO slots (id, session_id, cli_id, participant_kind, display_name,
                                role, created_at_ms, supervisor)
             VALUES ('sl-boss','s2','claude','cli','Orchestrator','orchestrator',?,1)",
            params![now],
        ).unwrap();

        let said = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                            &json!({"text": "I need a credential only a person can issue"}))
            .unwrap();
        assert!(said.to_lowercase().contains("supervisor"), "{said}");

        let waiting = pending_for_slot(&db, "s2", "sl-boss").unwrap();
        let raised = waiting
            .iter()
            .find(|m| m.body.contains("only a person can issue"))
            .expect("the supervisor seat was not told");
        assert!(raised.body.contains("raised to the supervisors"), "{}", raised.body);
        assert!(raised.body.contains("reply(to:"), "it must say how to answer: {}", raised.body);

        // And the answer goes back into the asker's session, to the asker.
        let out = dispatch(&ctx2(&db, "sl-boss", "s2"), "reply",
                           &json!({"to": raised.id, "text": "minted, see your inbox"})).unwrap();
        assert!(out.contains("its own session"), "{out}");
        let back = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        assert!(back.iter().any(|m| m.body.contains("minted, see your inbox")),
                "the seat that raised it was not answered: {:?}",
                back.iter().map(|m| &m.body).collect::<Vec<_>>());
        // ...and nobody else in that session was.
        let others = pending_for_slot(&db, "s1", "sl-rev").unwrap();
        assert!(!others.iter().any(|m| m.body.contains("minted")),
                "the answer was sprayed at the team");
    }

    /// THE TRAP THAT LET ASKS SIT FOR HOURS.
    ///
    /// `supervisor = 1` is opt-in, set with `/supervisor`. A fleet with a
    /// superorchestrator but nobody flagged -- which is the ordinary case --
    /// forwarded every raised ask to no one, while telling the caller to
    /// watch for a reply. A seat that supervises the fleet by authority must
    /// receive these whether or not anyone flagged it, and the caller must
    /// be told the truth about who heard it.
    #[test]
    fn a_super_authority_seat_hears_raised_asks_without_the_opt_in_flag() {
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO sessions (id, name, created_at_ms, last_active_ms)
             VALUES ('s2','fleet',?,?)",
            params![now, now],
        ).unwrap();
        // authority = super, supervisor = 0: exactly the live fleet's shape.
        db.execute(
            "INSERT INTO slots (id, session_id, cli_id, participant_kind, display_name,
                                role, created_at_ms, supervisor, authority)
             VALUES ('sl-super','s2','claude','cli','Super','orchestrator',?,0,'super')",
            params![now],
        ).unwrap();

        let said = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                            &json!({"text": "fleet-wide thing only a super can settle"}))
            .unwrap();
        // The receipt names a real recipient count, not a hopeful "watch".
        assert!(said.contains("1 supervisor seat") && said.contains("see the whole fleet"),
                "receipt should say it reached the super: {said}");

        let waiting = pending_for_slot(&db, "s2", "sl-super").unwrap();
        assert!(waiting.iter().any(|m| m.body.contains("only a super can settle")),
                "the super seat never received the raised ask");
    }

    /// And when there is genuinely nobody to hear it, say so plainly rather
    /// than leave the caller watching a channel no one is on. The dashboard
    /// row is still written; the lie "watch for a reply" is what's removed.
    #[test]
    fn a_raise_with_no_listener_is_honest_about_it() {
        let db = fixture();
        // fixture() has no super and no supervisor seat, and sl-eng's session
        // has no orchestrator token, so this escalates to the supervisors and
        // finds none.
        let said = dispatch(&ctx(&db, "sl-eng"), "notify_operator",
                            &json!({"text": "anybody out there", "escalate": true}))
            .unwrap();
        assert!(said.to_lowercase().contains("no supervisor seat")
                    || said.contains("dashboard"),
                "a raise nobody can answer must not promise a reply: {said}");
        assert!(!said.contains("Watch for a reply"),
                "it still told the caller to wait: {said}");
    }

    /// A record is not a message. The console's housekeeping lines and the
    /// node's notes about what it did unattended are inserted pre-claimed,
    /// and the MCP floor has always skipped them -- but the PUSH and TYPED
    /// paths read a different query, which did not. The cost was a seat
    /// joining later and being typed at with the whole backlog of them at
    /// once, which killed it.
    #[test]
    fn a_record_is_never_delivered_to_an_agent() {
        let db = fixture();
        let now = now_ms();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                   sent_at_ms, claimed_at_ms, claimed_by, record)
             VALUES ('s1', NULL, NULL, 'system', 'answered a prompt for you', ?, ?, 'node', 1)",
            params![now, now],
        )
        .unwrap();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', NULL, NULL, 'text', 'a real message', ?)",
            params![now],
        )
        .unwrap();
        let pending = pending_for_slot(&db, "s1", "sl-eng").unwrap();
        let bodies: Vec<&str> = pending.iter().map(|e| e.body.as_str()).collect();
        assert!(bodies.contains(&"a real message"), "{bodies:?}");
        assert!(
            !bodies.iter().any(|b| b.contains("answered a prompt")),
            "a pre-claimed record was handed to an agent: {bodies:?}"
        );
    }

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
        Ctx { db, session_id: "s1".into(), slot_id: slot.into(), flood: (0, 0) }
    }

    /// A seat in some other session, for the cases that are about two teams.
    fn ctx2<'a>(db: &'a Connection, slot: &str, session: &str) -> Ctx<'a> {
        Ctx { db, session_id: session.into(), slot_id: slot.into(), flood: (0, 0) }
    }

    fn state(db: &Connection, slot: &str) -> String {
        db.query_row("SELECT task_state FROM slots WHERE id = ?", [slot], |r| r.get(0)).unwrap()
    }

    // ------------------------------------------------------------ folds ----

    #[test]
    fn folding_takes_the_detail_out_of_context_and_hands_it_back_on_request() {
        // The whole bargain: the agent stops carrying the raw material and
        // loses nothing by it.
        let db = fixture();
        let c = ctx(&db, "sl-eng");
        let out = dispatch(&c, "fold_context", &json!({
            "topic": "the tenant migration",
            "summary": "tenant_id was added in migrations/0041.sql:12; the backfill runs in \
                        batches of 5000 and is idempotent",
            "detail": format!("psql output, batch after batch:\n{}\n                               migrations/0041.sql:12 added tenant_id",
                              "BEGIN; UPDATE claims SET tenant_id = ... ; COMMIT;\n"
                                  .repeat(400)),
        })).unwrap();
        assert!(out.contains("Folded as fd-"), "{out}");
        let id = out.split("Folded as ").nth(1).unwrap()
                    .split_whitespace().next().unwrap().to_string();
        // It says what it bought, so an agent can tell whether folding is
        // worth doing again.
        assert!(out.contains("tokens of detail now cost you"), "{out}");
        let back = dispatch(&c, "unfold_context", &json!({ "id": id })).unwrap();
        assert!(back.contains("psql output, batch after batch"), "{back}");
        assert_eq!(back.matches("BEGIN; UPDATE claims").count(), 400,
                   "every line of it comes back, not a sample");
        assert!(back.contains("migrations/0041.sql:12"), "{back}");
    }

    #[test]
    fn a_fold_is_private_to_the_seat_unless_it_says_otherwise() {
        // An agent folding its own working context must not fill the team's
        // memory with its housekeeping -- and a teammate must not be able to
        // read it back by guessing an id.
        let db = fixture();
        let out = dispatch(&ctx(&db, "sl-eng"), "fold_context", &json!({
            "topic": "my own reading", "summary": "what I found", "detail": "the long version",
        })).unwrap();
        assert!(out.contains("agent memory"), "{out}");
        let id = out.split("Folded as ").nth(1).unwrap()
                    .split_whitespace().next().unwrap().to_string();
        let e = dispatch(&ctx(&db, "sl-rev"), "unfold_context", &json!({ "id": id }))
            .unwrap_err().to_string();
        assert!(e.contains("another seat's private context"), "{e}");
        // ...and the team scope is there when it IS for the team.
        let team = dispatch(&ctx(&db, "sl-eng"), "fold_context", &json!({
            "topic": "the api we settled on", "summary": "POST /v1/claims takes a batch",
            "scope": "session",
        })).unwrap();
        assert!(team.contains("session memory"), "{team}");
        let tid = team.split("Folded as ").nth(1).unwrap()
                      .split_whitespace().next().unwrap().to_string();
        assert!(dispatch(&ctx(&db, "sl-rev"), "unfold_context", &json!({ "id": tid }))
                .unwrap().contains("POST /v1/claims"));
    }

    #[test]
    fn what_was_folded_is_found_by_its_words_not_by_remembering_an_id() {
        // A fold nobody can find again is a deletion with extra steps.
        let db = fixture();
        let c = ctx(&db, "sl-eng");
        dispatch(&c, "fold_context", &json!({
            "topic": "the migration", "summary": "done in batches",
            "detail": "the column is tenant_id and the index is idx_claims_tenant",
        })).unwrap();
        let hit = dispatch(&c, "search_context", &json!({ "search": "idx_claims_tenant" }))
            .unwrap();
        assert!(hit.contains("the migration"), "{hit}");
        assert!(hit.contains("unfold_context"), "it has to say how to get the detail: {hit}");
        let miss = dispatch(&c, "search_context", &json!({ "search": "kubernetes" })).unwrap();
        assert!(miss.contains("Nothing folded mentions"), "{miss}");
    }

    #[test]
    fn folds_fold_into_folds_so_a_long_job_stays_a_handful_of_summaries() {
        let db = fixture();
        let c = ctx(&db, "sl-eng");
        let mut ids = Vec::new();
        for n in ["monday", "tuesday", "wednesday"] {
            let out = dispatch(&c, "fold_context", &json!({
                "topic": n, "summary": format!("what {n} established"),
                "detail": "x".repeat(2000),
            })).unwrap();
            ids.push(out.split("Folded as ").nth(1).unwrap()
                        .split_whitespace().next().unwrap().to_string());
        }
        let top = dispatch(&c, "fold_context", &json!({
            "topic": "the week", "summary": "the three days in one line",
            "covers": ids.clone(),
        })).unwrap();
        assert!(top.contains("tier 2"), "{top}");
        assert!(top.contains("3 earlier fold(s) now sit underneath it"), "{top}");
        // The map shows the parent, and the way down is in the unfold.
        let map = dispatch(&c, "context_map", &json!({})).unwrap();
        assert!(map.contains("tier 2:"), "{map}");
        let tid = top.split("Folded as ").nth(1).unwrap()
                     .split_whitespace().next().unwrap().to_string();
        let down = dispatch(&c, "unfold_context", &json!({ "id": tid })).unwrap();
        assert!(down.contains("covers 3 earlier fold(s)"), "{down}");
        assert!(down.contains("monday") && down.contains("wednesday"), "{down}");
    }

    #[test]
    fn the_map_says_how_full_it_is_and_what_to_do_about_it() {
        let db = fixture();
        db.execute(
            "UPDATE agents SET context_used_tokens = 150000, context_limit_tokens = 200000,
                               context_source = 'its conversation file' WHERE id = 'a-eng'",
            [],
        ).unwrap();
        let c = ctx(&db, "sl-eng");
        let map = dispatch(&c, "context_map", &json!({})).unwrap();
        assert!(map.contains("150000 of 200000"), "{map}");
        assert!(map.contains("75% full"), "{map}");
        assert!(map.contains("Nothing has been folded yet"), "{map}");
        // The rule travels with the map: an agent reading this has to know
        // what to keep, not just that it is full.
        assert!(map.contains("no longer needs it"), "{map}");
        assert!(map.contains("line numbers"), "{map}");
    }

    #[test]
    fn a_cli_whose_window_cannot_be_read_is_told_the_rule_instead_of_a_number() {
        // Only claude reports usage today. A seat on any other CLI must still
        // get something it can act on rather than a blank.
        let db = fixture();
        let map = dispatch(&ctx(&db, "sl-rev"), "context_map", &json!({})).unwrap();
        assert!(map.contains("cannot be read for this CLI"), "{map}");
        assert!(map.contains("Fold by the rule instead"), "{map}");
    }

    #[test]
    fn an_unknown_fold_says_so_rather_than_answering_nothing() {
        let db = fixture();
        let e = dispatch(&ctx(&db, "sl-eng"), "unfold_context", &json!({ "id": "fd-nope" }))
            .unwrap_err().to_string();
        assert!(e.contains("no fold 'fd-nope'"), "{e}");
        assert!(e.contains("search_context"), "it should say how to find the real one: {e}");
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
    fn a_pattern_covers_what_a_team_means_by_it() {
        // The patterns people write: a container name, a directory, a glob.
        assert!(pattern_covers("verti", "verti/src/main.rs"));
        assert!(pattern_covers("verti/", "verti/src/main.rs"));
        assert!(pattern_covers("verti", "verti"));
        assert!(!pattern_covers("verti", "vertigo/src/main.rs"));
        assert!(!pattern_covers("verti", "ombia/src/main.rs"));

        assert!(pattern_covers("verti/**", "verti/src/main.rs"));
        assert!(pattern_covers("verti/**", "verti/a/b/c.rs"));
        assert!(pattern_covers("src/auth/*", "src/auth/login.rs"));
        assert!(!pattern_covers("src/auth/*", "src/auth/deep/login.rs"));
        assert!(pattern_covers("*.sql", "schema.sql"));
        assert!(!pattern_covers("*.sql", "db/schema.sql"));
        assert!(pattern_covers("**/*.sql", "db/schema.sql"));
        assert!(pattern_covers("a/**/b.rs", "a/b.rs"), "** must also match nothing");

        // Leading ./ and / are noise, on either side.
        assert!(pattern_covers("./verti/**", "/verti/src/main.rs"));
        assert!(!pattern_covers("", "verti/x"));
        assert!(!pattern_covers("verti", ""));
    }

    #[test]
    fn the_longest_pattern_owns_the_path() {
        let db = fixture();
        db.execute(
            "INSERT INTO ownership (workspace_id, slot_id, pattern) VALUES
                ('w1','sl-eng','verti/**'), ('w1','sl-rev','verti/db/**')",
            [],
        )
        .unwrap();
        // A carve-out means the carve-out.
        let (slot, who, pat) = owner_of(&db, "w1", "verti/db/schema.sql").unwrap().unwrap();
        assert_eq!((slot.as_str(), pat.as_str()), ("sl-rev", "verti/db/**"));
        assert_eq!(who, "Iris", "the owner is named the way teammates address it");
        let (slot, _, _) = owner_of(&db, "w1", "verti/src/main.rs").unwrap().unwrap();
        assert_eq!(slot, "sl-eng");
        // Nothing covers it: nobody owns it, which is not an error.
        assert!(owner_of(&db, "w1", "ombia/src/main.rs").unwrap().is_none());
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
        // src/main.rs under /srv/app blocked another locking
        // src/main.rs under /storage/coderai -- different trees, and a
        // denial naming an agent in a project you were not touching.
        let db = fixture();
        let now = now_ms();
        for (wid, path) in [("w-app", "/srv/app"), ("w-coderai", "/storage/coderai")] {
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
        db.execute("UPDATE slots SET workspace_id = 'w-app' WHERE id = 'sl-eng'", []).unwrap();
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
        // It escalates to the supervisor layer (None target), and this
        // fixture has nobody flagged or super, so the receipt is honest
        // about that rather than promising a reply that cannot come.
        assert!(out.to_lowercase().contains("no supervisor seat"), "{out}");
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
        assert!(out.to_lowercase().contains("no supervisor seat"), "{out}");
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

    /// Every call through the one door is counted for the seat that made it,
    /// which is what lets the operator see whether a team uses its graph and
    /// its memory at all.
    #[test]
    fn each_tool_call_is_counted_for_the_seat_that_made_it() {
        let db = fixture();
        for _ in 0..3 {
            dispatch(&ctx(&db, "sl-eng"), "query_knowledge", &json!({})).unwrap();
        }
        dispatch(&ctx(&db, "sl-rev"), "list_peers", &json!({})).unwrap();
        let n: i64 = db
            .query_row(
                "SELECT n FROM tool_counts WHERE slot_id = 'sl-eng' AND tool = 'query_knowledge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
        let theirs: i64 = db
            .query_row("SELECT COUNT(*) FROM tool_counts WHERE slot_id = 'sl-rev'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(theirs, 1, "and only under the seat that called it");
    }

    const DAY_MS: i64 = 24 * 60 * 60 * 1000;

    /// A direct message nobody ever saw, two days old, is aged out: stamped
    /// `system:expired` so the log keeps it, and never handed to its agent.
    #[test]
    fn an_old_unseen_direct_message_is_aged_out() {
        let db = fixture();
        let old = now_ms() - 2 * DAY_MS;
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', 'sl-rev', 'sl-eng', 'text', 'still there?', ?)",
            params![old],
        )
        .unwrap();
        let id = db.last_insert_rowid();
        assert!(pending_for_slot(&db, "s1", "sl-eng").unwrap().iter().any(|m| m.id == id));
        assert_eq!(expire_undelivered(&db, DAY_MS).unwrap(), 1);
        let by: String = db
            .query_row("SELECT cancelled_by FROM messages WHERE id = ?", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(by, "system:expired");
        assert!(
            pending_for_slot(&db, "s1", "sl-eng").unwrap().iter().all(|m| m.id != id),
            "an aged-out message must never be delivered"
        );
    }

    /// A message that reached its agent is left exactly as it was: aging out
    /// must never rewrite a delivered message as withdrawn.
    #[test]
    fn a_delivered_message_is_never_aged_out() {
        let db = fixture();
        let old = now_ms() - 2 * DAY_MS;
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms, acked_at_ms)
             VALUES ('s1', 'sl-rev', 'sl-eng', 'text', 'got it', ?, ?)",
            params![old, old + 1],
        )
        .unwrap();
        assert_eq!(expire_undelivered(&db, DAY_MS).unwrap(), 0);
    }

    /// A recent message is untouched -- the timeout is a day, not a moment.
    #[test]
    fn a_recent_message_is_left_alone() {
        let db = fixture();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', 'sl-rev', 'sl-eng', 'text', 'just now', ?)",
            params![now_ms()],
        )
        .unwrap();
        assert_eq!(expire_undelivered(&db, DAY_MS).unwrap(), 0);
    }

    /// An old team broadcast still pending for a teammate is aged out; once
    /// every teammate has acked it, there is nothing left to age out.
    #[test]
    fn an_old_pending_broadcast_is_aged_out_but_a_consumed_one_is_not() {
        let db = fixture();
        let old = now_ms() - 2 * DAY_MS;
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', 'sl-eng', NULL, 'text', 'anyone about?', ?)",
            params![old],
        )
        .unwrap();
        let id = db.last_insert_rowid();
        assert!(pending_for_slot(&db, "s1", "sl-rev").unwrap().iter().any(|m| m.id == id));
        assert_eq!(expire_undelivered(&db, DAY_MS).unwrap(), 1, "nobody saw it");
        assert!(
            pending_for_slot(&db, "s1", "sl-rev").unwrap().iter().all(|m| m.id != id),
            "the straggler must not get a day-old broadcast"
        );

        // A second broadcast every non-sender slot has acked is fully
        // consumed, so it is not aged out.
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', 'sl-eng', NULL, 'text', 'all here', ?)",
            params![old],
        )
        .unwrap();
        let id2 = db.last_insert_rowid();
        for slot in ["sl-rev", "sl-ext"] {
            db.execute(
                "INSERT INTO message_recipients (message_id, slot_id, acked_at_ms)
                 VALUES (?, ?, ?)",
                params![id2, slot, old + 1],
            )
            .unwrap();
        }
        assert_eq!(expire_undelivered(&db, DAY_MS).unwrap(), 0, "a seen broadcast stays put");
    }
}

/// A SUPERORCHESTRATOR'S REACH OVER THE CHAT.
///
/// The authority model gave it the fleet for every management tool and left
/// its messaging in its own session, so the one seat on the fleet that could
/// move another team's agents could not say a word to them. These are about
/// the two reaches being one reach.
#[cfg(test)]
mod super_reach_tests {
    use super::*;
    use serde_json::json;

    /// Two teams, and a seat in each. `s1` holds the super.
    fn two_teams() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        let now = now_ms();
        for (id, name) in [("s1", "orchestrator"), ("s2", "team")] {
            db.execute(
                "INSERT INTO sessions (id, name, created_at_ms, last_active_ms) VALUES (?,?,?,?)",
                params![id, name, now, now],
            )
            .unwrap();
        }
        for (id, sess, name, auth, cli, tmux) in [
            ("sl-boss", "s1", "Orchestrator", "super", Some("claude"), None),
            ("sl-plain", "s1", "Plain", "agent", Some("claude"), None),
            // The adopted terminal that sits in the same room.
            ("sl-term", "s1", "term-Orchestrator", "agent", Some("tmux"), Some("nexmesh-orch")),
            ("sl-far", "s2", "alice", "agent", Some("codex"), None),
        ] {
            db.execute(
                "INSERT INTO slots (id, session_id, cli_id, participant_kind, display_name,
                                    role, authority, tmux_target, capabilities, created_at_ms)
                 VALUES (?,?,?, 'cli', ?, 'engineer', ?, ?, '[]', ?)",
                params![id, sess, cli, name, auth, tmux, now],
            )
            .unwrap();
        }
        db
    }

    fn c<'a>(db: &'a Connection, slot: &str, session: &str) -> Ctx<'a> {
        Ctx { db, session_id: session.into(), slot_id: slot.into(), flood: (0, 0) }
    }

    #[test]
    fn a_super_can_address_a_seat_on_another_team() {
        let db = two_teams();
        let out = dispatch(
            &c(&db, "sl-boss", "s1"),
            "send_message",
            &json!({"to": "alice", "text": "status?"}),
        )
        .unwrap();
        assert!(out.contains("alice"), "{out}");
    }

    /// The half that was easy to miss: resolving the name is not delivering
    /// the message. Written into the SENDER'S room it would be reported sent
    /// and read by nobody, because every delivery query asks for the rooms
    /// the RECIPIENT answers in.
    #[test]
    fn and_it_actually_arrives_in_the_other_team() {
        let db = two_teams();
        dispatch(
            &c(&db, "sl-boss", "s1"),
            "send_message",
            &json!({"to": "alice", "text": "status?"}),
        )
        .unwrap();
        let got = pending_for_slot(&db, "s2", "sl-far").unwrap();
        assert_eq!(got.len(), 1, "the far seat must be handed it: {got:?}");
        assert_eq!(got[0].body, "status?");
        // And it is NOT sitting in the sender's own room instead.
        let room: String =
            db.query_row("SELECT session_id FROM messages WHERE id = ?", [got[0].id], |r| r.get(0))
                .unwrap();
        assert_eq!(room, "s2", "the message belongs in the recipient's room");
    }

    /// The answer has to find its way back across the same boundary.
    #[test]
    fn the_far_seat_can_reply_back_across_the_boundary() {
        let db = two_teams();
        dispatch(&c(&db, "sl-boss", "s1"), "send_message",
                 &json!({"to": "alice", "text": "status?"})).unwrap();
        let got = pending_for_slot(&db, "s2", "sl-far").unwrap();
        dispatch(&c(&db, "sl-far", "s2"), "reply",
                 &json!({"to": got[0].id, "text": "two files left"})).unwrap();
        let back = pending_for_slot(&db, "s1", "sl-boss").unwrap();
        assert_eq!(back.len(), 1, "the super must get its answer: {back:?}");
        assert!(back[0].body.contains("two files left"), "{:?}", back[0].body);
    }

    /// A NULL sender is what the whole stack reads as the operator, and the
    /// handshake text tells an agent that an operator message is the person
    /// running the fleet. A teammate's answer arriving that way tells the
    /// recipient a person is talking to it.
    #[test]
    fn a_cross_team_answer_is_attributed_to_the_seat_that_sent_it() {
        let db = two_teams();
        dispatch(&c(&db, "sl-boss", "s1"), "send_message",
                 &json!({"to": "alice", "text": "status?"})).unwrap();
        let got = pending_for_slot(&db, "s2", "sl-far").unwrap();
        dispatch(&c(&db, "sl-far", "s2"), "reply",
                 &json!({"to": got[0].id, "text": "two files left"})).unwrap();
        let back = pending_for_slot(&db, "s1", "sl-boss").unwrap();
        assert_eq!(back.len(), 1, "{back:?}");
        assert_eq!(
            back[0].from_slot.as_deref(),
            Some("sl-far"),
            "a peer's answer must not arrive as the operator's"
        );
        assert_eq!(back[0].from_name.as_deref(), Some("alice"), "{:?}", back[0].from_name);
        // And it says which team, which is the one thing the recipient
        // cannot look up: a seat in another room is not in its roster.
        assert!(back[0].body.contains("team"), "{}", back[0].body);
    }

    #[test]
    fn a_super_sees_the_whole_fleet_in_list_peers() {
        let db = two_teams();
        let out = dispatch(&c(&db, "sl-boss", "s1"), "list_peers", &json!({})).unwrap();
        assert!(out.contains("alice"), "another team's seat must be listed: {out}");
        assert!(out.contains("Plain"), "{out}");
    }

    /// Fleet reach is not an invitation. Both read as "elsewhere", and
    /// calling them the same thing told a super that every seat on the fleet
    /// was a guest of its own room.
    #[test]
    fn a_seat_reached_by_authority_is_not_described_as_a_guest() {
        let db = two_teams();
        let out = dispatch(&c(&db, "sl-boss", "s1"), "list_peers", &json!({})).unwrap();
        let line = out.lines().find(|l| l.contains("alice")).unwrap_or_default();
        assert!(line.contains("on the team team"), "{line}");
        assert!(!line.contains("visiting"), "nobody invited it: {line}");

        // A seat genuinely invited into this room still reads as a visitor.
        db.execute(
            "INSERT INTO slot_sessions (slot_id, session_id, added_at_ms) VALUES ('sl-far','s1',1)",
            [],
        )
        .unwrap();
        let out = dispatch(&c(&db, "sl-boss", "s1"), "list_peers", &json!({})).unwrap();
        let line = out.lines().find(|l| l.contains("alice")).unwrap_or_default();
        assert!(line.contains("visiting from team"), "{line}");
    }

    /// Nothing else widens. A plain seat and an orchestrator see what they
    /// always saw, or this fix has quietly opened every team to everyone.
    #[test]
    fn a_plain_seat_still_sees_only_its_own_team() {
        let db = two_teams();
        let out = dispatch(&c(&db, "sl-plain", "s1"), "list_peers", &json!({})).unwrap();
        assert!(!out.contains("alice"), "another team must stay invisible: {out}");
        let err = dispatch(&c(&db, "sl-plain", "s1"), "send_message",
                           &json!({"to": "alice", "text": "x"})).unwrap_err().to_string();
        assert!(err.contains("no teammate matching"), "{err}");
    }

    /// The legible error must TEACH without becoming an oracle. A plain seat
    /// addressing 'alice' (a real seat in another team) and one addressing
    /// a name that exists nowhere must get the SAME message bar the needle --
    /// otherwise the difference is a yes/no probe a plain seat could sweep to
    /// map a team `list_peers` is built to hide. Both must still name the
    /// guest mechanism, so the one case this exists for -- two seats that
    /// should collaborate -- is told how.
    #[test]
    fn the_cross_session_hint_teaches_without_confirming_existence() {
        let db = two_teams();
        let real = dispatch(&c(&db, "sl-plain", "s1"), "send_message",
                            &json!({"to": "alice", "text": "x"})).unwrap_err().to_string();
        let ghost = dispatch(&c(&db, "sl-plain", "s1"), "send_message",
                             &json!({"to": "no-such-seat", "text": "x"})).unwrap_err().to_string();
        assert_eq!(
            real.replace("alice", "NEEDLE"),
            ghost.replace("no-such-seat", "NEEDLE"),
            "a real cross-session name and a nonexistent one must read identically, \
             or the error is an enumeration oracle"
        );
        assert!(real.contains("guesting") && real.contains("notify_operator"),
                "it must still point to the way across sessions: {real}");
    }

    #[test]
    fn an_orchestrator_reaches_its_own_team_and_no_further() {
        let db = two_teams();
        db.execute("UPDATE slots SET authority = 'orchestrator' WHERE id = 'sl-plain'", [])
            .unwrap();
        let err = dispatch(&c(&db, "sl-plain", "s1"), "send_message",
                           &json!({"to": "alice", "text": "x"})).unwrap_err().to_string();
        assert!(err.contains("no teammate matching"), "an orchestrator runs ONE team: {err}");
        // Its own team is unchanged.
        dispatch(&c(&db, "sl-plain", "s1"), "send_message",
                 &json!({"to": "Orchestrator", "text": "x"})).unwrap();
    }

    /// What is LISTED must be what can be ADDRESSED. `list_peers` carried its
    /// own copy of the room predicate, without the terminal exclusion, so it
    /// offered a name that `send_message` then refused.
    #[test]
    fn an_adopted_terminal_is_neither_listed_nor_addressable() {
        let db = two_teams();
        for slot in ["sl-boss", "sl-plain"] {
            let out = dispatch(&c(&db, slot, "s1"), "list_peers", &json!({})).unwrap();
            assert!(
                !out.contains("term-Orchestrator"),
                "a terminal is a window onto a seat, not a teammate ({slot}): {out}"
            );
            let err = dispatch(&c(&db, slot, "s1"), "send_message",
                               &json!({"to": "term-Orchestrator", "text": "x"}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("no teammate matching"), "{err}");
        }
    }

    /// A broadcast stays in the room it was spoken in, super or not. Fleet
    /// authority is for addressing a named seat; announcing to every agent
    /// on every team is the operator's call, and a budget.
    #[test]
    fn a_broadcast_does_not_follow_the_wider_reach() {
        let db = two_teams();
        dispatch(&c(&db, "sl-boss", "s1"), "broadcast", &json!({"text": "heads up"})).unwrap();
        assert!(pending_for_slot(&db, "s2", "sl-far").unwrap().is_empty(),
                "another team must not hear this team's announcements");
        assert_eq!(pending_for_slot(&db, "s1", "sl-plain").unwrap().len(), 1);
    }
}
