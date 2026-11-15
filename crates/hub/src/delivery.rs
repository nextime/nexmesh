//! Three background duties: pushing messages to driven agents, typing them at
//! the ones that can only be reached by keyboard, and bringing crashed agents
//! back.

use crate::{launch, state::AppState, tools};
use nexmesh_proto::{now_ms, HubToNode};
use rusqlite::params;

/// How long a slot may crash repeatedly before we stop trying.
const FLAP_WINDOW_MS: i64 = 5 * 60 * 1000;
const FLAP_LIMIT: i64 = 3;

/// Hand queued messages to agents whose driver can take them mid-turn.
///
/// For everyone else, delivery piggybacks on the next tool result, which is
/// correct but can wait minutes while an agent is deep in a long edit. This is
/// what makes a teammate's message land in seconds instead.
pub async fn push_tick(st: &AppState) {
    // (agent_id, node_id, slot_id, session_id) for live, driven agents.
    let targets: Vec<(String, String, String, String)> = {
        let db = st.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            // A paused agent is deliberately left alone: messages queue up
            // and arrive when it is resumed, which is what pause means.
            "SELECT a.id, a.node_id, s.id, s.session_id
             FROM agents a
             JOIN slots s ON s.id = a.slot_id
             JOIN sessions se ON se.id = s.session_id
             WHERE a.exited_at_ms IS NULL AND a.push_capable = 1 AND a.node_id IS NOT NULL
               AND s.task_state NOT IN ('released')
               AND s.paused = 0 AND se.status = 'active'",
        ) else {
            return;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|m| m.collect())
            .unwrap_or_default()
    };

    for (agent_id, node_id, slot_id, session_id) in targets {
        if !st.node_is_connected(&node_id) {
            continue;
        }
        let batch = {
            let db = st.db.lock().unwrap();
            match tools::pending_for_slot(&db, &session_id, &slot_id) {
                Ok(m) if !m.is_empty() => {
                    let ids: Vec<i64> = m.iter().map(|x| x.id).collect();
                    // Mark in flight before sending: a tick that overlaps the
                    // previous one must not inject the same message twice.
                    if tools::mark_inflight(&db, &agent_id, &slot_id, &ids).is_err() {
                        continue;
                    }
                    m
                }
                _ => continue,
            }
        };
        let count = batch.len();
        if let Err(e) = st
            .send_to_node(&node_id, &HubToNode::Inject {
                agent_id: agent_id.clone(),
                messages: batch.clone(),
                // A driven agent's own stdin: nothing to re-check.
                expect_cli: None,
            })
            .await
        {
            tracing::warn!(%agent_id, error = %e, "could not inject; requeueing");
            let db = st.db.lock().unwrap();
            for m in &batch {
                let _ = db.execute(
                    "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                     WHERE id = ? AND acked_at_ms IS NULL",
                    [m.id],
                );
            }
        } else {
            tracing::debug!(%agent_id, count, "pushed to a driven agent");
        }
    }
}

/// Hand queued messages to seats whose CLI is sitting in an adopted terminal.
///
/// These are the seats nothing drives: a CLI someone started by hand, whose
/// only route was its own next tool call. If its terminal happens to be a
/// tmux pane a node has adopted, there is a keyboard, and a task can be typed
/// at it in seconds rather than waiting for the agent to think of calling a
/// tool -- which, sitting at a prompt, it may never do.
///
/// Three things make this safe, and only the last is here:
///
///   - the link was made when the CLI's shim registered, which is the proof
///     that an agent CLI, not a shell, is in that pane (see `link_terminal`);
///   - the node re-checks the CLI's identity and its pane before typing a
///     single byte, because only the node shares a kernel with it;
///   - and the message goes to the ADOPTED agent, whose pty it is. If that
///     adoption has ended, the join below finds nothing and nothing is sent.
pub async fn typed_tick(st: &AppState) {
    // (via_agent_id, node_id, slot_id, session_id, cli pid, cli ticks)
    let targets: Vec<(String, String, String, String, i32, i64)> = {
        let db = st.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            // The via agent must still be alive and adopted; a paused seat
            // is deliberately left alone, as everywhere else.
            "SELECT v.id, v.node_id, s.id, s.session_id,
                    s.terminal_cli_pid, s.terminal_cli_ticks
             FROM slots s
             JOIN agents v ON v.id = s.terminal_via
             JOIN sessions se ON se.id = s.session_id
             WHERE s.terminal_via IS NOT NULL AND s.terminal_cli_pid IS NOT NULL
               AND v.exited_at_ms IS NULL AND v.node_id IS NOT NULL
               AND s.task_state NOT IN ('released')
               AND s.paused = 0 AND se.status = 'active'",
        ) else {
            return;
        };
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default()
    };

    for (via, node_id, slot_id, session_id, pid, ticks) in targets {
        if !st.node_is_connected(&node_id) {
            continue;
        }
        let batch = {
            let db = st.db.lock().unwrap();
            match tools::pending_for_slot(&db, &session_id, &slot_id) {
                Ok(m) if !m.is_empty() => {
                    let ids: Vec<i64> = m.iter().map(|x| x.id).collect();
                    // In flight against the TERMINAL, which is what will
                    // ack it -- the seat's own agent may not even be
                    // connected -- but recorded for the SEAT the message is
                    // for, which is not the terminal's own.
                    if tools::mark_inflight(&db, &via, &slot_id, &ids).is_err() {
                        continue;
                    }
                    m
                }
                _ => continue,
            }
        };
        let count = batch.len();
        let cli = nexmesh_proto::ProcIdentity {
            node_id: node_id.clone(),
            pid,
            start_ticks: ticks as u64,
        };
        if let Err(e) = st
            .send_to_node(&node_id, &HubToNode::Inject {
                agent_id: via.clone(),
                messages: batch.clone(),
                expect_cli: Some(cli),
            })
            .await
        {
            tracing::warn!(slot = %slot_id, error = %e, "could not type that; requeueing");
            let db = st.db.lock().unwrap();
            for m in &batch {
                let _ = db.execute(
                    "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                     WHERE id = ? AND acked_at_ms IS NULL",
                    [m.id],
                );
            }
        } else {
            tracing::debug!(slot = %slot_id, via = %via, count, "typing at an adopted terminal");
        }
    }
}

/// Hand queued messages to seats the fleet itself put in a terminal.
///
/// `spawn --mode interactive` on a CLI with no driver produced a seat that
/// could be watched and typed at by a person, and reached by nothing else:
/// no driver to push to, no adoption to type at, so its only route was its
/// own next tool call -- which a CLI sitting at a prompt with nothing to do
/// may never make. A message to it was accepted, queued, and never arrived.
///
/// The node already holds that pty and knows how to type at it; it was only
/// never asked. There is no pane to re-check here, unlike an adoption: the
/// node spawned the process, is its parent, and has no io for it once it is
/// gone -- so an `Inject` for a dead agent fails rather than typing into
/// somebody else's terminal.
pub async fn interactive_tick(st: &AppState) {
    // (agent_id, node_id, slot_id, session_id)
    let targets: Vec<(String, String, String, String)> = {
        let db = st.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            // Alive, at a prompt its own descriptor vouches for, and
            // driven by nothing. An adopted seat is excluded because
            // `typed_tick` owns it: there the keystrokes go to the pane's own
            // agent, not to this one.
            "SELECT a.id, a.node_id, s.id, s.session_id
             FROM agents a
             JOIN slots s ON s.id = a.slot_id
             JOIN sessions se ON se.id = s.session_id
             WHERE a.exited_at_ms IS NULL AND a.node_id IS NOT NULL
               AND a.push_capable = 0 AND a.typed_delivery = 1
               AND s.terminal_via IS NULL
               AND s.task_state NOT IN ('released')
               AND s.paused = 0 AND se.status = 'active'",
        ) else {
            return;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|m| m.collect())
            .unwrap_or_default()
    };

    for (agent_id, node_id, slot_id, session_id) in targets {
        if !st.node_is_connected(&node_id) {
            continue;
        }
        let batch = {
            let db = st.db.lock().unwrap();
            match tools::pending_for_slot(&db, &session_id, &slot_id) {
                Ok(m) if !m.is_empty() => {
                    let ids: Vec<i64> = m.iter().map(|x| x.id).collect();
                    if tools::mark_inflight(&db, &agent_id, &slot_id, &ids).is_err() {
                        continue;
                    }
                    m
                }
                _ => continue,
            }
        };
        let count = batch.len();
        if let Err(e) = st
            .send_to_node(&node_id, &HubToNode::Inject {
                agent_id: agent_id.clone(),
                messages: batch.clone(),
                // The node resolves this agent's own pty by id. A process
                // that has gone has no io, which is the check.
                expect_cli: None,
            })
            .await
        {
            tracing::warn!(%agent_id, error = %e, "could not type that; requeueing");
            let db = st.db.lock().unwrap();
            for m in &batch {
                let _ = db.execute(
                    "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                     WHERE id = ? AND acked_at_ms IS NULL",
                    [m.id],
                );
            }
        } else {
            tracing::debug!(slot = %slot_id, count, "typing at a seat's own terminal");
        }
    }
}

/// How long a message may go unacknowledged before its sender is told.
///
/// Long enough that a busy agent mid-turn is not reported as unreachable --
/// delivery happens on a tool result, which can be minutes on a long turn --
/// and short enough that nobody waits a whole afternoon on a seat that died
/// the moment they wrote to it.
const STALL_AFTER_MS: i64 = 180_000;

/// Tell senders about messages that are not getting through.
///
/// A message addressed to a seat whose process has died, or whose machine has
/// gone, waits for ever: the queue keeps it for whoever fills that seat next,
/// which is right, but the SENDER is never told and goes on believing it was
/// heard. An agent that asked a teammate for something and got silence cannot
/// tell "working on it" from "that seat has been dead for an hour".
///
/// So each stalled message is reported once, to whoever sent it, with the
/// reason -- which is a different sentence for a dead process, a disconnected
/// machine and a seat that simply has not picked it up yet.
pub async fn stalled_tick(st: &AppState) {
    let now = crate::now_ms();
    let stalled: Vec<(i64, String, Option<String>, String, String, Option<String>, Option<String>, i64)> = {
        let db = st.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            "SELECT m.id, m.session_id, m.from_slot, m.to_slot,
                    COALESCE(t.display_name, t.role, m.to_slot),
                    -- The machine's NAME, not its id: 'machine-a' tells a
                    -- reader where to look, 'n1' tells them nothing.
                    (SELECT n.name FROM agents a LEFT JOIN nodes n ON n.id = a.node_id
                      WHERE a.slot_id = m.to_slot
                      ORDER BY a.started_at_ms DESC LIMIT 1),
                    (SELECT n.status FROM agents a LEFT JOIN nodes n ON n.id = a.node_id
                      WHERE a.slot_id = m.to_slot
                      ORDER BY a.started_at_ms DESC LIMIT 1),
                    (SELECT COUNT(*) FROM agents a WHERE a.slot_id = m.to_slot
                      AND a.exited_at_ms IS NULL)
               FROM messages m JOIN slots t ON t.id = m.to_slot
              WHERE m.to_slot IS NOT NULL
                AND m.acked_at_ms IS NULL
                AND m.stalled_at_ms IS NULL
                AND m.claimed_at_ms IS NULL
                AND m.sent_at_ms < ?1
              ORDER BY m.id ASC LIMIT 20",
        ) else {
            return;
        };
        stmt.query_map([now - STALL_AFTER_MS], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                r.get(7)?))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default()
    };
    if stalled.is_empty() {
        return;
    }
    let db = st.db.lock().unwrap();
    for (id, session, from_slot, _to_slot, to_name, node, node_status, live) in stalled {
        let why = if live == 0 {
            match node {
                Some(n) if node_status.as_deref() != Some("online") => format!(
                    "{to_name} has no process running, and its machine '{n}' is not connected"
                ),
                _ => format!("{to_name} has no process running"),
            }
        } else if node_status.as_deref() == Some("online") {
            format!("{to_name} is running but has not picked it up")
        } else {
            let n = node.unwrap_or_else(|| "its machine".into());
            format!("{to_name} is running on '{n}', which is not connected")
        };
        let body = format!(
            "[undelivered] your message #{id} to {to_name} has not been delivered after              three minutes: {why}. It stays queued for whoever fills that seat, so it is              not lost -- but nothing has read it. Re-send it, address somebody else, or              raise it with notify_operator if it needs a person."
        );
        let told = match &from_slot {
            // An agent sent it: tell them as a real message, since they may
            // need to do something about it.
            Some(slot) => db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms,
                                       reply_to_slot, reply_to_session, claimed_at_ms,
                                       claimed_by)
                 VALUES (?1, NULL, ?2, 'system', ?3, ?4, NULL, NULL, NULL, NULL)",
                rusqlite::params![session, slot, body, now],
            ),
            // The operator sent it: a line in the chat they are reading,
            // pre-claimed so no agent is handed somebody else's postmark.
            None => db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms,
                                       claimed_at_ms, claimed_by)
                 VALUES (?1, NULL, NULL, 'system', ?2, ?3, ?3, 'operator')",
                rusqlite::params![session, body, now],
            ),
        };
        if told.is_ok() {
            let _ = db.execute(
                "UPDATE messages SET stalled_at_ms = ? WHERE id = ?",
                rusqlite::params![now, id],
            );
            tracing::info!(message = id, %to_name, "told the sender a message is not getting through");
        }
    }
}

/// Bring back managed agents that died with work still outstanding.
pub async fn recovery_tick(st: &AppState) {
    // A slot needs recovery when its last managed agent has exited, the work
    // is not finished, and nothing else is already bringing it back.
    let candidates: Vec<(String, String, i64, Option<i64>, Option<String>)> = {
        let db = st.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            // Respawn a slot that is still WORKING. A slot that has signalled
            // done is deliberately left alone unless feedback is waiting for
            // it -- restarting an agent with nothing to do would just burn
            // tokens and confuse the review loop.
            // The MOST RECENT agent must have been a managed one. `EXISTS`
            // was wrong and the fleet showed why: a seat that nexmesh had
            // spawned once, and that a person later attached their own CLI
            // to, was treated as the hub's to restart for ever. Closing that
            // CLI then produced a `claude` nobody asked for, launched with a
            // guess at how to resume a conversation only that person's
            // terminal knew about -- which failed instantly and was retried.
            //
            // A seat whose last process was attached belongs to whoever
            // started it. The hub does not know how they launched it and has
            // no business inventing a replacement.
            "SELECT s.id, s.session_id, s.restarts, s.last_crash_ms, s.display_name
             FROM slots s
             WHERE s.status != 'flapping'
               AND (SELECT a.provenance FROM agents a WHERE a.slot_id = s.id
                     ORDER BY a.started_at_ms DESC LIMIT 1) = 'managed'

               AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.slot_id = s.id
                                 AND a.exited_at_ms IS NULL)
               AND (
                 s.task_state IN ('idle', 'working', 'addressing_feedback')
                 OR (s.task_state = 'done_pending_review' AND EXISTS (
                      SELECT 1 FROM messages m
                      WHERE m.session_id = s.session_id AND m.to_slot = s.id
                        AND m.acked_at_ms IS NULL))
               )",
        ) else {
            return;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .and_then(|m| m.collect())
            .unwrap_or_default()
    };

    for (slot_id, session_id, restarts, last_crash, name) in candidates {
        if !st.begin_respawn(&slot_id) {
            continue; // already in progress
        }
        let now = now_ms();
        let within_window = last_crash.map(|t| now - t < FLAP_WINDOW_MS).unwrap_or(false);
        let streak = if within_window { restarts + 1 } else { 1 };

        if streak > FLAP_LIMIT {
            // Stop rather than hammer a broken agent forever, and say so where
            // the team and the operator will both see it.
            {
                let db = st.db.lock().unwrap();
                let _ = db.execute(
                    "UPDATE slots SET status = 'flapping', restarts = ?, last_crash_ms = ? WHERE id = ?",
                    params![streak, now, slot_id],
                );
                let _ = db.execute(
                    "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
                     VALUES (?, NULL, NULL, 'system', ?, ?)",
                    params![
                        session_id,
                        format!(
                            "{} has crashed {streak} times in five minutes and will not be \
                             restarted again. Work around it or ask the operator.",
                            name.clone().unwrap_or_else(|| slot_id.clone())
                        ),
                        now
                    ],
                );
            }
            tracing::error!(%slot_id, streak, "flapping: giving up on this slot");
            st.end_respawn(&slot_id);
            continue;
        }

        {
            let db = st.db.lock().unwrap();
            let _ = db.execute(
                "UPDATE slots SET restarts = ?, last_crash_ms = ?, status = 'disconnected' WHERE id = ?",
                params![streak, now, slot_id],
            );
        }

        let handoff = format!(
            "NOTE: you are being restarted after your previous process stopped \
             unexpectedly (attempt {streak}). Your earlier session is being resumed where \
             possible. Before doing anything else, call query_knowledge and get_plan to see \
             what has already been decided and done, and check_messages for anything waiting \
             for you. Do NOT redo work that is already finished."
        );

        match launch::launch_slot(st, &slot_id, Some(&handoff)).await {
            Ok((agent_id, node)) => {
                tracing::warn!(%slot_id, %agent_id, %node, streak, "respawned a crashed agent")
            }
            Err(e) => tracing::warn!(%slot_id, error = %e, "could not respawn yet"),
        }
        st.end_respawn(&slot_id);
    }
}

#[cfg(test)]
mod stalled_tests {
    use crate::state::AppState;
    use rusqlite::params;

    /// A session, two seats, and one of them dead.
    fn fleet(live_for_max: bool, node_online: bool) -> AppState {
        let st = AppState::in_memory().unwrap();
        let now = crate::now_ms();
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO sessions (id, name, created_at_ms, last_active_ms)
                 VALUES ('s1','T',?,?)",
                params![now, now],
            )
            .unwrap();
            db.execute(
                "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,
                                    last_seen_ms,lease_expires_ms)
                 VALUES ('n1','machine-a','linux','x86_64','0.1.0',?,?,?,?)",
                params![if node_online { "online" } else { "offline" }, now, now, now + 60000],
            )
            .unwrap();
            for (id, name) in [("sl-eng", "Max"), ("sl-rev", "Iris")] {
                db.execute(
                    "INSERT INTO slots (id, session_id, cli_id, participant_kind, display_name,
                                        role, created_at_ms)
                     VALUES (?,'s1','claude','cli',?,'engineer',?)",
                    params![id, name, now],
                )
                .unwrap();
            }
            // Max's agent: present, and alive or not.
            db.execute(
                "INSERT INTO agents (id, slot_id, node_id, provenance, status, started_at_ms,
                                     exited_at_ms, last_seen_ms)
                 VALUES ('a-eng','sl-eng','n1','managed','running',?,?,?)",
                params![now, if live_for_max { None } else { Some(now) }, now],
            )
            .unwrap();
        }
        st
    }

    fn insert(st: &AppState, from: Option<&str>, to: &str, body: &str, age_ms: i64) -> i64 {
        let db = st.db.lock().unwrap();
        let sent = crate::now_ms() - age_ms;
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s1', ?, ?, 'text', ?, ?)",
            params![from, to, body, sent],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn reports(st: &AppState) -> Vec<(String, Option<String>)> {
        let db = st.db.lock().unwrap();
        let mut q = db
            .prepare("SELECT body, to_slot FROM messages WHERE body LIKE '[undelivered]%'")
            .unwrap();
        let rows = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    #[tokio::test]
    async fn a_message_to_a_dead_seat_is_reported_to_whoever_sent_it() {
        let st = fleet(false, true);
        insert(&st, Some("sl-rev"), "sl-eng", "can you take this?", 200_000);
        super::stalled_tick(&st).await;
        let got = reports(&st);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0.contains("no process running"), "{}", got[0].0);
        assert_eq!(got[0].1.as_deref(), Some("sl-rev"), "the SENDER is told, nobody else");
    }

    #[tokio::test]
    async fn a_disconnected_machine_is_named_as_the_reason() {
        let st = fleet(false, false);
        insert(&st, Some("sl-rev"), "sl-eng", "still there?", 200_000);
        super::stalled_tick(&st).await;
        let got = reports(&st);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0.contains("machine-a") && got[0].0.contains("not connected"),
                "{}", got[0].0);
    }

    #[tokio::test]
    async fn a_live_seat_that_is_merely_busy_is_reported_as_such() {
        let st = fleet(true, true);
        insert(&st, Some("sl-rev"), "sl-eng", "when you can", 200_000);
        super::stalled_tick(&st).await;
        let got = reports(&st);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0.contains("has not picked it up"), "{}", got[0].0);
    }

    #[tokio::test]
    async fn a_fresh_message_is_left_alone() {
        let st = fleet(false, true);
        insert(&st, Some("sl-rev"), "sl-eng", "just sent", 1_000);
        super::stalled_tick(&st).await;
        assert!(reports(&st).is_empty(), "three minutes have not passed");
    }

    #[tokio::test]
    async fn the_operator_is_told_in_the_chat_and_no_agent_is_handed_it() {
        let st = fleet(false, true);
        insert(&st, None, "sl-eng", "status please", 200_000);
        super::stalled_tick(&st).await;
        let got = reports(&st);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1.is_none(), "it belongs in the chat, not in a seat's queue");
        let pending = {
            let db = st.db.lock().unwrap();
            crate::tools::pending_for_slot(&db, "s1", "sl-rev").unwrap()
        };
        assert!(!pending.iter().any(|m| m.body.starts_with("[undelivered]")),
                "somebody else's postmark was delivered to an agent");
    }

    #[tokio::test]
    async fn it_is_reported_once_and_not_every_tick() {
        let st = fleet(false, true);
        insert(&st, Some("sl-rev"), "sl-eng", "hello?", 200_000);
        super::stalled_tick(&st).await;
        super::stalled_tick(&st).await;
        super::stalled_tick(&st).await;
        assert_eq!(reports(&st).len(), 1, "a postmark per tick is a flood, not a warning");
    }
}
