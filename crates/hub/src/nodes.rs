//! The node WebSocket session: authentication, then frame handling.

use crate::{
    state::{AppState, AttachFrame},
    tools,
};
use anyhow::Result;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    State,
};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nexmesh_proto::{self as proto, now_ms, HubToNode, NodeToHub};
use rusqlite::{params, OptionalExtension};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn node_ws(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |sock| session(sock, st))
}

fn reject(reason: impl Into<String>) -> String {
    serde_json::to_string(&HubToNode::Rejected { reason: reason.into() }).unwrap()
}

async fn session(socket: WebSocket, st: AppState) {
    let (mut sink, mut stream) = socket.split();

    // Nothing is honoured before authentication -- in particular, no frame
    // that could cause a process to be spawned or signalled.
    let Some(Ok(Message::Text(first))) = stream.next().await else { return };
    let hello: NodeToHub = match serde_json::from_str(&first) {
        Ok(h) => h,
        Err(e) => {
            let _ = sink.send(Message::Text(reject(format!("unparseable hello: {e}")))).await;
            return;
        }
    };
    let NodeToHub::Hello { token, name: claimed, protocol, .. } = &hello else {
        let _ = sink.send(Message::Text(reject("first frame must be hello"))).await;
        return;
    };
    if *protocol != proto::PROTOCOL_VERSION {
        let _ = sink
            .send(Message::Text(reject(format!(
                "protocol {protocol} unsupported, hub speaks {}",
                proto::PROTOCOL_VERSION
            ))))
            .await;
        return;
    }
    let Some(issued) = st.authenticate_node(token) else {
        tracing::warn!(node = %claimed, "rejected: bad token");
        let _ = sink.send(Message::Text(reject("bad token"))).await;
        return;
    };
    // The token decides identity, not the frame: a node cannot rename itself
    // into another machine's slot history.
    if &issued != claimed {
        tracing::warn!(claimed = %claimed, issued = %issued, "rejected: name mismatch");
        let _ = sink
            .send(Message::Text(reject(format!("token was issued for node '{issued}'"))))
            .await;
        return;
    }
    let node_id = match st.register_node(&issued, &hello) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "registering node");
            let _ = sink.send(Message::Text(reject("registration failed"))).await;
            return;
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(512);
    st.attach_node(&node_id, tx.clone());
    tracing::info!(node = %issued, id = %node_id, "online");

    let writer = tokio::spawn(async move {
        while let Some(s) = rx.recv().await {
            if sink.send(Message::Text(s)).await.is_err() {
                break;
            }
        }
    });

    let welcome = HubToNode::Welcome {
        protocol: proto::PROTOCOL_VERSION,
        node_id: node_id.clone(),
        hub_version: VERSION.to_string(),
        heartbeat_ms: proto::HEARTBEAT_INTERVAL_MS,
    };
    let _ = tx.send(serde_json::to_string(&welcome).unwrap()).await;

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let frame: NodeToHub = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(node = %issued, error = %e, "bad frame");
                continue;
            }
        };
        if let Err(e) = handle(&st, &node_id, &issued, frame, &tx).await {
            tracing::warn!(node = %issued, error = %e, "handling frame");
        }
    }

    st.detach_node(&node_id);
    st.mark_node_offline(&node_id);
    writer.abort();
    tracing::info!(node = %issued, "offline");
}

async fn handle(
    st: &AppState,
    node_id: &str,
    node_name: &str,
    frame: NodeToHub,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    match frame {
        NodeToHub::Heartbeat { agents, .. } => {
            st.renew_node_lease(node_id);
            let db = st.db.lock().unwrap();
            for a in agents {
                // A node reports liveness it verified against /proc, so the hub
                // never has to guess from a pid it cannot see.
                if a.alive {
                    db.execute(
                        "UPDATE agents SET last_seen_ms = ?, status =
                            CASE WHEN status = 'lost' THEN 'running' ELSE status END
                         WHERE id = ?",
                        params![now_ms(), a.agent_id],
                    )?;
                } else {
                    db.execute(
                        "UPDATE agents SET status = 'lost' WHERE id = ? AND exited_at_ms IS NULL",
                        [&a.agent_id],
                    )?;
                }
            }
        }

        NodeToHub::AgentUp {
            agent_id, slot_id, proc: id, provenance, cwd, resume_handle, push_capable,
        } => {
            let db = st.db.lock().unwrap();
            let now = now_ms();
            db.execute(
                "INSERT INTO agents (id, slot_id, node_id, pid, proc_start_ticks, provenance,
                                     status, cwd, started_at_ms, last_seen_ms, push_capable)
                 VALUES (?1,?2,?3,?4,?5,?6,'running',?7,?8,?8,?9)
                 ON CONFLICT(id) DO UPDATE SET status = 'running', last_seen_ms = ?8",
                params![
                    agent_id, slot_id, node_id, id.pid, id.start_ticks,
                    serde_json::to_value(provenance)?.as_str().unwrap_or("managed"),
                    cwd, now, push_capable as i64
                ],
            )?;
            db.execute("UPDATE slots SET status = 'connected' WHERE id = ?", [&slot_id])?;
            if let Some(h) = resume_handle {
                db.execute("UPDATE slots SET resume_handle = ? WHERE id = ?", params![h, slot_id])?;
            }
            tracing::info!(node = %node_name, %agent_id, %slot_id, pid = id.pid, "agent up");
        }

        NodeToHub::AgentExited { agent_id, code, signal } => {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'exited', exited_at_ms = ? WHERE id = ?",
                params![now_ms(), agent_id],
            )?;
            // Anything handed to it but never acked must go back in the queue.
            db.execute(
                "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                 WHERE inflight_agent = ? AND acked_at_ms IS NULL",
                [&agent_id],
            )?;
            tracing::info!(node = %node_name, %agent_id, ?code, ?signal, "agent exited");
        }

        NodeToHub::ResumeHandle { slot_id, handle, .. } => {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE slots SET resume_handle = ? WHERE id = ?",
                params![handle, slot_id],
            )?;
            tracing::debug!(%slot_id, "stored a resume handle");
        }

        NodeToHub::InjectionsLost { message_ids, .. } => {
            // Back in the queue, or they are never redelivered.
            let db = st.db.lock().unwrap();
            for id in message_ids {
                db.execute(
                    "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                     WHERE id = ? AND acked_at_ms IS NULL",
                    [id],
                )?;
            }
        }

        NodeToHub::TokenUsage { agent_id, input, output, cache_read } => {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET input_tokens = ?, output_tokens = ?, cache_read_tokens = ?
                 WHERE id = ?",
                params![input, output, cache_read, agent_id],
            )?;
        }

        NodeToHub::MessageAcked { agent_id, message_ids } => {
            let db = st.db.lock().unwrap();
            tools::ack_messages(&db, &agent_id, &message_ids)?;
        }

        NodeToHub::JoinRequest { req_id, session_hint, cli_id, role, name, cwd, proc: pid } => {
            let granted = join(st, node_id, session_hint, cli_id, role, name, cwd, pid);
            let frame = match granted {
                Ok((slot_id, session_id, agent_id, display_name, role)) => HubToNode::JoinGranted {
                    req_id, slot_id, session_id, agent_id, display_name, role,
                },
                Err(e) => {
                    tracing::warn!(node = %node_name, error = %e, "join denied");
                    HubToNode::JoinDenied { req_id, reason: e.to_string() }
                }
            };
            tx.send(serde_json::to_string(&frame)?).await.ok();
        }

        NodeToHub::ToolCall { agent_id, call_id, tool, args } => {
            let reply = run_tool(st, &agent_id, &tool, &args);
            let frame = match reply {
                Ok((text, deliver)) => HubToNode::ToolResult {
                    agent_id,
                    call_id,
                    result: serde_json::json!({ "ok": true, "text": text }),
                    deliver,
                },
                Err(e) => HubToNode::ToolResult {
                    agent_id,
                    call_id,
                    result: serde_json::json!({ "ok": false, "error": e.to_string() }),
                    deliver: vec![],
                },
            };
            tx.send(serde_json::to_string(&frame)?).await.ok();
        }

        NodeToHub::Output { agent_id, data, .. } => {
            // Persist it: this is the only record of what an agent actually did,
            // and it is gone for good once the process exits.
            st.append_transcript(&agent_id, &data);
        }
        NodeToHub::AttachOpened { attach_id, read_only, .. } => {
            let msg = serde_json::json!({ "t": "opened", "read_only": read_only });
            st.send_attach(&attach_id, AttachFrame::Control(msg.to_string())).await;
        }

        NodeToHub::AttachRejected { attach_id, reason } => {
            let msg = serde_json::json!({ "t": "error", "message": reason });
            st.send_attach(&attach_id, AttachFrame::Control(msg.to_string())).await;
            st.drop_attach(&attach_id);
        }

        NodeToHub::AttachData { attach_id, data } => {
            // If nobody is listening any more, tell the node to stop sending.
            if !st.send_attach(&attach_id, AttachFrame::Data(data)).await {
                st.drop_attach(&attach_id);
                let _ = tx
                    .send(serde_json::to_string(&HubToNode::AttachClose {
                        attach_id,
                    })?)
                    .await;
            }
        }

        NodeToHub::AttachClosed { attach_id, reason } => {
            let msg = serde_json::json!({ "t": "closed", "reason": reason });
            st.send_attach(&attach_id, AttachFrame::Control(msg.to_string())).await;
            st.drop_attach(&attach_id);
        }
        NodeToHub::Pong { .. } => {}
        NodeToHub::Hello { .. } => tracing::warn!(node = %node_name, "duplicate hello ignored"),
    }
    Ok(())
}

/// Run a tool for an agent and collect any messages to ride back with the
/// result. Marking them in flight here -- not when they were sent -- is what
/// makes delivery exactly-once-ish: an agent that dies before acking releases
/// them again.
pub fn run_tool(
    st: &AppState,
    agent_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<(String, Vec<proto::Envelope>)> {
    let db = st.db.lock().unwrap();
    let (slot_id, session_id): (String, String) = db
        .query_row(
            "SELECT s.id, s.session_id FROM agents a JOIN slots s ON s.id = a.slot_id
             WHERE a.id = ?",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_id}'"))?;

    let ctx = tools::Ctx { db: &db, session_id: session_id.clone(), slot_id: slot_id.clone() };
    let text = tools::dispatch(&ctx, tool, args)?;

    // `check_messages` already acked what it showed; don't hand it out twice.
    let deliver = if tool == "check_messages" {
        vec![]
    } else {
        let pending = tools::pending_for_slot(&db, &session_id, &slot_id)?;
        let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
        tools::mark_inflight(&db, agent_id, &ids)?;
        pending
    };
    let _ = db.execute(
        "UPDATE agents SET last_seen_ms = ? WHERE id = ?",
        params![now_ms(), agent_id],
    );
    Ok((text, deliver))
}

#[allow(clippy::too_many_arguments)]
fn join(
    st: &AppState,
    node_id: &str,
    session_hint: Option<String>,
    cli_id: Option<String>,
    role: Option<String>,
    name: Option<String>,
    cwd: Option<String>,
    pid: Option<proto::ProcIdentity>,
) -> Result<(String, String, String, Option<String>, Option<String>)> {
    let db = st.db.lock().unwrap();
    let now = now_ms();

    // Pick the session: the one named, else the only active one.
    let session_id: String = match session_hint {
        Some(s) => db
            .query_row("SELECT id FROM sessions WHERE id = ? OR name = ?", params![s, s], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no such session"))?,
        None => {
            let ids: Vec<String> = db
                .prepare("SELECT id FROM sessions WHERE status = 'active'")?
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            match ids.len() {
                1 => ids[0].clone(),
                0 => anyhow::bail!("no active session to join"),
                k => anyhow::bail!("{k} active sessions; specify which one"),
            }
        }
    };

    // Work out which workspace this agent is actually sitting in, by matching
    // its cwd against the mounts on this node. Without this an attached agent
    // gets no workspace, its locks fall back to session scope, and it would
    // NOT contend with a managed agent editing the very same tree.
    let mounts: Vec<(String, String)> = db
        .prepare(
            "SELECT w.id, m.path FROM workspaces w
             JOIN workspace_mounts m ON m.workspace_id = w.id
             WHERE w.session_id = ? AND m.node_id = ?",
        )?
        .query_map(params![session_id, node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let workspace_id = cwd.as_deref().and_then(|c| workspace_for_cwd(&mounts, c));
    if workspace_id.is_none() && cwd.is_some() {
        tracing::info!(
            cwd = ?cwd,
            "attached agent is not inside any workspace mount on this node; \
             its file locks will be session-scoped"
        );
    }

    let slot_id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                            display_name, role, status, created_at_ms)
         VALUES (?,?,?,?,'cli','interactive',?,?,'connected',?)",
        params![slot_id, session_id, workspace_id, cli_id, name, role, now],
    )?;
    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO agents (id, slot_id, node_id, pid, proc_start_ticks, provenance, status,
                             cwd, started_at_ms, last_seen_ms)
         VALUES (?,?,?,?,?, 'attached', 'running', ?, ?, ?)",
        params![
            agent_id, slot_id, node_id,
            pid.as_ref().map(|p| p.pid), pid.as_ref().map(|p| p.start_ticks),
            cwd, now, now
        ],
    )?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,?, 'agent_joined', ?)",
        params![now, name.clone().unwrap_or_default(), node_id,
                serde_json::json!({ "slot": slot_id, "provenance": "attached" }).to_string()],
    )?;
    tracing::info!(%slot_id, %agent_id, "attached agent joined");
    Ok((slot_id, session_id, agent_id, name, role))
}

/// Pick the workspace whose mount contains `cwd`, preferring the most
/// specific (longest) path so nested workspaces resolve correctly.
///
/// Matching is on path boundaries: `/srv/app` must not match `/srv/application`.
pub fn workspace_for_cwd(mounts: &[(String, String)], cwd: &str) -> Option<String> {
    let cwd = cwd.trim_end_matches('/');
    let mut best: Option<(&str, usize)> = None;
    for (ws_id, path) in mounts {
        let p = path.trim_end_matches('/');
        let contained = cwd == p || cwd.strip_prefix(p).map(|r| r.starts_with('/')).unwrap_or(false);
        if contained && best.map(|(_, len)| p.len() > len).unwrap_or(true) {
            best = Some((ws_id, p.len()));
        }
    }
    best.map(|(id, _)| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::workspace_for_cwd;

    fn mounts() -> Vec<(String, String)> {
        vec![
            ("ws-outer".into(), "/srv/app".into()),
            ("ws-inner".into(), "/srv/app/vendor/lib".into()),
            ("ws-other".into(), "/data/stuff/".into()),
        ]
    }

    #[test]
    fn an_exact_mount_path_matches() {
        assert_eq!(workspace_for_cwd(&mounts(), "/srv/app").as_deref(), Some("ws-outer"));
    }

    #[test]
    fn a_subdirectory_resolves_to_its_workspace() {
        assert_eq!(workspace_for_cwd(&mounts(), "/srv/app/src/http").as_deref(), Some("ws-outer"));
    }

    #[test]
    fn the_most_specific_nested_workspace_wins() {
        assert_eq!(
            workspace_for_cwd(&mounts(), "/srv/app/vendor/lib/sub").as_deref(),
            Some("ws-inner"),
            "a nested workspace must win over its parent"
        );
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_does_not_match() {
        // The bug this prevents: naive prefix matching puts an agent working in
        // /srv/application into the /srv/app workspace, so its locks arbitrate
        // against the wrong tree.
        assert_eq!(workspace_for_cwd(&mounts(), "/srv/application"), None);
        assert_eq!(workspace_for_cwd(&mounts(), "/srv/app-backup/src"), None);
    }

    #[test]
    fn trailing_slashes_on_either_side_are_tolerated() {
        assert_eq!(workspace_for_cwd(&mounts(), "/data/stuff").as_deref(), Some("ws-other"));
        assert_eq!(workspace_for_cwd(&mounts(), "/data/stuff/x/").as_deref(), Some("ws-other"));
    }

    #[test]
    fn a_cwd_outside_every_mount_matches_nothing() {
        assert_eq!(workspace_for_cwd(&mounts(), "/home/nextime"), None);
        assert_eq!(workspace_for_cwd(&[], "/srv/app"), None);
    }
}
