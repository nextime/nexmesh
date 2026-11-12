//! Pausing and releasing agents.
//!
//! Two different things, often confused:
//!
//! **Pause** is reversible and leaves the agent alive. New work stops reaching
//! it and it is told to stand by. A turn already in flight finishes -- nothing
//! here reaches into a model mid-thought -- so pause means "take on nothing
//! further", not "stop this instant".
//!
//! **Release** is the end of the review loop. Agents deliberately cannot
//! disconnect on their own, which is what stops one wandering off before its
//! work is reviewed; release is how that hold is lifted. The agent's input is
//! closed so a stdio-driven CLI exits of its own accord, and only if it is
//! still there after a grace period is it terminated.

use crate::state::AppState;
use anyhow::{Context, Result};
use nexmesh_proto::{now_ms, HubToNode, ProcIdentity};
use rusqlite::{params, OptionalExtension};

/// Resolve a name, role or id within a session to one slot.
pub fn find_slot(st: &AppState, session_id: &str, who: &str) -> Result<(String, String)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT id, COALESCE(display_name, role, id) FROM slots
         WHERE session_id = ?1
           AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                OR LOWER(COALESCE(role,'')) = LOWER(?2))",
        params![session_id, who],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .with_context(|| format!("no agent matching '{who}' in this session"))
}

fn tell(st: &AppState, session_id: &str, slot_id: Option<&str>, body: &str) {
    let db = st.db.lock().unwrap();
    let _ = db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, ?, 'system', ?, ?)",
        params![session_id, slot_id, body, now_ms()],
    );
}

/// The live agent filling a slot, if there is one.
fn live_agent(st: &AppState, slot_id: &str) -> Option<(String, String, Option<ProcIdentity>)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT a.id, a.node_id, a.pid, a.proc_start_ticks, n.name
         FROM agents a LEFT JOIN nodes n ON n.id = a.node_id
         WHERE a.slot_id = ? AND a.exited_at_ms IS NULL
         ORDER BY a.started_at_ms DESC LIMIT 1",
        [slot_id],
        |r| {
            let agent: String = r.get(0)?;
            let node: Option<String> = r.get(1)?;
            let pid: Option<i32> = r.get(2)?;
            let ticks: Option<i64> = r.get(3)?;
            let node_name: Option<String> = r.get(4)?;
            let proc = match (pid, ticks, node_name) {
                (Some(pid), Some(t), Some(n)) => Some(ProcIdentity {
                    node_id: n,
                    pid,
                    start_ticks: t as u64,
                }),
                _ => None,
            };
            Ok((agent, node.unwrap_or_default(), proc))
        },
    )
    .optional()
    .ok()
    .flatten()
}

pub async fn set_paused(
    st: &AppState,
    session_id: &str,
    who: &str,
    paused: bool,
    reason: Option<&str>,
) -> Result<String> {
    let (slot_id, name) = find_slot(st, session_id, who)?;
    {
        let db = st.db.lock().unwrap();
        db.execute(
            "UPDATE slots SET paused = ?, paused_at = ? WHERE id = ?",
            params![paused as i64, paused.then(now_ms), slot_id],
        )?;
    }
    let note = reason.filter(|r| !r.trim().is_empty());
    if paused {
        tell(
            st,
            session_id,
            Some(&slot_id),
            &format!(
                "PAUSED by the operator{}. Finish what you are doing, then stop and wait: no \
                 further work will be sent to you until you are resumed.",
                note.map(|r| format!(" ({r})")).unwrap_or_default()
            ),
        );
    } else {
        tell(
            st,
            session_id,
            Some(&slot_id),
            "RESUMED by the operator. Anything that arrived while you were paused is on its way; \
             carry on.",
        );
    }
    tracing::info!(%slot_id, %name, paused, "slot pause state changed");
    Ok(format!(
        "{name} is {}.",
        if paused { "paused" } else { "running again" }
    ))
}

/// Pause or resume everyone in a session at once.
pub async fn set_session_paused(
    st: &AppState,
    session_id: &str,
    paused: bool,
    reason: Option<&str>,
) -> Result<String> {
    // One transaction: a failure part way through would leave the session
    // marked paused while its agents carried on, which is the worst of both.
    let n = {
        let mut db = st.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE sessions SET status = ?, pause_reason = ?, paused_at = ? WHERE id = ?",
            params![
                if paused { "paused" } else { "active" },
                reason,
                paused.then(now_ms),
                session_id
            ],
        )?;
        // A released slot has finished; leave it alone.
        let n = tx.execute(
            "UPDATE slots SET paused = ?, paused_at = ?
             WHERE session_id = ? AND task_state != 'released'",
            params![paused as i64, paused.then(now_ms), session_id],
        )?;
        tx.commit()?;
        n
    };
    tell(
        st,
        session_id,
        None,
        &if paused {
            format!(
                "The whole team is PAUSED by the operator{}. Finish your current step, then stop \
                 and wait.",
                reason.map(|r| format!(" ({r})")).unwrap_or_default()
            )
        } else {
            "The team is RESUMED. Carry on.".to_string()
        },
    );
    Ok(format!(
        "{n} agent(s) {}.",
        if paused { "paused" } else { "resumed" }
    ))
}

/// Release one agent: its work is accepted and it may stop.
pub async fn release(
    st: &AppState,
    session_id: &str,
    who: &str,
    note: Option<&str>,
    grace_ms: u64,
) -> Result<String> {
    let (slot_id, name) = find_slot(st, session_id, who)?;
    {
        let db = st.db.lock().unwrap();
        db.execute(
            "UPDATE slots SET task_state = 'released', paused = 0 WHERE id = ?",
            [&slot_id],
        )?;
    }
    tell(
        st,
        session_id,
        Some(&slot_id),
        &format!(
            "RELEASED by the operator{}. Your work is accepted and you are free to stop; you do \
             not need to wait for review any more.",
            note.filter(|n| !n.trim().is_empty())
                .map(|n| format!(" ({n})"))
                .unwrap_or_default()
        ),
    );

    let detail = match live_agent(st, &slot_id) {
        Some((agent_id, node_id, Some(proc))) if st.node_is_connected(&node_id) => {
            match st
                .send_to_node(
                    &node_id,
                    &HubToNode::CloseAgent { agent_id, proc, grace_ms },
                )
                .await
            {
                Ok(()) => "its process is being closed".to_string(),
                Err(e) => format!("could not reach its machine to close it: {e}"),
            }
        }
        Some(_) => "it has no process this hub can close".to_string(),
        None => "it was not running".to_string(),
    };
    tracing::info!(%slot_id, %name, "released");
    Ok(format!("Released {name}; {detail}."))
}

/// Release every agent in a session: the team is finished.
pub async fn release_all(
    st: &AppState,
    session_id: &str,
    note: Option<&str>,
    grace_ms: u64,
) -> Result<String> {
    let slots: Vec<String> = {
        let db = st.db.lock().unwrap();
        db.prepare("SELECT id FROM slots WHERE session_id = ? AND task_state != 'released'")
            .and_then(|mut s| s.query_map([session_id], |r| r.get(0)).and_then(|m| m.collect()))?
    };
    if slots.is_empty() {
        return Ok("everyone was already released".into());
    }
    let mut done = 0;
    for slot in &slots {
        if release(st, session_id, slot, note, grace_ms).await.is_ok() {
            done += 1;
        }
    }
    Ok(format!("Released {done} agent(s)."))
}
