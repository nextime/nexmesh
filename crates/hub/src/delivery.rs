//! Two background duties: pushing messages to driven agents, and bringing
//! crashed agents back.

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
                    if tools::mark_inflight(&db, &agent_id, &ids).is_err() {
                        continue;
                    }
                    m
                }
                _ => continue,
            }
        };
        let count = batch.len();
        if let Err(e) = st
            .send_to_node(&node_id, &HubToNode::Inject { agent_id: agent_id.clone(), messages: batch.clone() })
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
            "SELECT s.id, s.session_id, s.restarts, s.last_crash_ms, s.display_name
             FROM slots s
             WHERE s.status != 'flapping'
               AND EXISTS (SELECT 1 FROM agents a WHERE a.slot_id = s.id
                             AND a.provenance = 'managed')
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
