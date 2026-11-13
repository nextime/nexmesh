//! The node WebSocket session: authentication, then frame handling.

use crate::{
    state::{AppState, AttachFrame},
    tools,
};
use anyhow::Result;
use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Extension, State,
};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use nexmesh_proto::{self as proto, now_ms, HubToNode, NodeToHub};
use rusqlite::{params, OptionalExtension};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn node_ws(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    own: Option<Extension<std::sync::Arc<crate::peer::PeerIdentity>>>,
    conn: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Captured before the upgrade: once this is a socket, the handshake that
    // produced it -- and the certificate presented during it -- is out of
    // reach. Resolved exactly as every other route resolves it, so a node
    // arriving through a nominated proxy is bound to its certificate too.
    let peer = crate::peer::resolve_parts(
        &st,
        own.map(|Extension(p)| p),
        conn.map(|c| c.0.ip()),
        &headers,
    );
    ws.on_upgrade(move |sock| session(sock, st, peer))
}

fn reject(reason: impl Into<String>) -> String {
    serde_json::to_string(&HubToNode::Rejected { reason: reason.into() }).unwrap()
}

async fn session(
    socket: WebSocket,
    st: AppState,
    peer: std::sync::Arc<crate::peer::PeerIdentity>,
) {
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
    // Nor can one machine's token be used from another machine's connection:
    // under mutual TLS the certificate has to name the same node.
    if st.identity_binding() {
        if let Err(why) = peer.agrees_with(&issued) {
            tracing::warn!(node = %issued, certificate = ?peer.common_name,
                "rejected: certificate does not match the token");
            let _ = sink.send(Message::Text(reject(why))).await;
            return;
        }
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
    // A revoked machine must lose the connection it already has, not just be
    // refused the next one.
    let kick = st.kick_handle(&issued);
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

    loop {
        let msg = tokio::select! {
            biased;
            _ = kick.notified() => {
                tracing::warn!(node = %issued, "credentials revoked; closing the connection");
                break;
            }
            m = stream.next() => match m {
                Some(Ok(m)) => m,
                _ => break,
            },
        };
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
        // A tool call may itself have to ask this same node something -- a
        // workspace sync, say -- and the answer arrives as another frame on
        // this connection. Handling it inline would block the very loop that
        // must read that answer, so tool calls run concurrently. They are
        // independent of each other (each carries its own call id), and this
        // also stops one slow tool holding up heartbeats.
        if matches!(frame, NodeToHub::ToolCall { .. }) {
            let st = st.clone();
            let node_id = node_id.clone();
            let issued = issued.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(&st, &node_id, &issued, frame, &tx).await {
                    tracing::warn!(node = %issued, error = %e, "handling a tool call");
                }
            });
            continue;
        }
        if let Err(e) = handle(&st, &node_id, &issued, frame, &tx).await {
            tracing::warn!(node = %issued, error = %e, "handling frame");
        }
    }

    st.detach_node(&node_id);
    st.drop_kick_handle(&issued);
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
            // And anyone blocked on a request it was serving must be told,
            // rather than waiting out a timeout on a slot that has gone.
            if let Some(slot) = db
                .query_row("SELECT slot_id FROM agents WHERE id = ?", [&agent_id], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
            {
                match crate::asks::fail_outstanding(&db, &slot) {
                    Ok(n) if n > 0 => tracing::warn!(%agent_id, requests = n,
                        "agent exited holding capability requests; askers told"),
                    Err(e) => tracing::error!(error = %e, "releasing capability requests"),
                    _ => {}
                }
            }
            tracing::info!(node = %node_name, %agent_id, ?code, ?signal, "agent exited");
        }

        NodeToHub::ResumeHandle { slot_id, handle, agent_id } => {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE slots SET resume_handle = ? WHERE id = ?",
                params![handle, slot_id],
            )?;
            // And durably, against the identity rather than the slot, so the
            // conversation can be picked up in a later session.
            let who: Option<(Option<String>, Option<String>, Option<String>)> = db
                .query_row(
                    "SELECT s.cli_id, s.display_name, a.cwd
                       FROM slots s LEFT JOIN agents a ON a.id = ?
                      WHERE s.id = ?",
                    params![agent_id, slot_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((Some(cli), Some(name), Some(cwd))) = who {
                let _ = crate::memory::remember_handle(&db, &cli, &cwd, &name, &handle);
            }
            tracing::debug!(%slot_id, "stored a resume handle");
        }

        NodeToHub::CodeGraphResult { req_id, graph, .. } => {
            // Hand it to whoever asked, through the same waiter the workspace
            // sync round-trip uses.
            st.complete_sync(req_id, serde_json::to_value(graph).unwrap_or_default());
        }

        NodeToHub::AgentModel { agent_id, model } => {
            let db = st.db.lock().unwrap();
            db.execute("UPDATE agents SET model = ? WHERE id = ?", params![model, agent_id])?;
            tracing::info!(%agent_id, %model, "agent reported its model");
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

        NodeToHub::SyncResult {
            req_id, workspace_id, ok, conflict, head, detail, changed,
        } => {
            let v = serde_json::json!({
                "ok": ok, "conflict": conflict, "head": head,
                "detail": detail, "changed": changed,
            });
            crate::sync::record(st, &workspace_id, node_id, &v);
            st.complete_sync(req_id, v);
        }

        NodeToHub::TokenUsage { agent_id, input, output, cache_read } => {
            let over = {
                let db = st.db.lock().unwrap();
                db.execute(
                    "UPDATE agents SET input_tokens = ?, output_tokens = ?, cache_read_tokens = ?
                     WHERE id = ?",
                    params![input, output, cache_read, agent_id],
                )?;
                crate::budget::exceeded(&db, &agent_id)?
            };
            // Checked here because this is the only place spending is learnt,
            // and acted on outside the lock: pausing talks to every node.
            if let Some(hit) = over {
                crate::budget::enforce(st, hit).await;
            }
        }

        NodeToHub::MessageAcked { agent_id, message_ids } => {
            let db = st.db.lock().unwrap();
            tools::ack_messages(&db, &agent_id, &message_ids)?;
        }

        NodeToHub::JoinRequest {
            req_id, session_hint, cli_id, role, name, cwd, proc: pid, cli_session,
        } => {
            let granted = join(st, node_id, session_hint, cli_id, role, name, cwd, pid, cli_session);
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
            let reply = run_tool_async(st, &agent_id, &tool, &args).await;
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

/// Tools that have to talk to a node, so cannot run inside the synchronous
/// dispatcher.
/// Ask a teammate and wait for the answer.
///
/// Polled rather than notified: the holder may be any kind of participant on
/// any machine, and a poll on one indexed table is cheap next to the work
/// being waited for.
async fn ask_capability_tool(
    st: &AppState,
    agent_id: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let (slot_id, _) = slot_and_name(st, agent_id)
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_id}'"))?;
    let capability = args
        .get("capability")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required argument 'capability'"))?;
    let request = args.get("request").cloned().unwrap_or(serde_json::json!({}));
    let model = args.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
    let timeout = args
        .get("timeout_seconds")
        .and_then(|v| v.as_i64())
        .unwrap_or(crate::asks::DEFAULT_TIMEOUT_S)
        .clamp(1, crate::asks::MAX_TIMEOUT_S);

    let session_id: String = {
        let db = st.db.lock().unwrap();
        db.query_row("SELECT session_id FROM slots WHERE id = ?", [&slot_id], |r| r.get(0))?
    };
    let (id, _to_slot, label) = {
        let db = st.db.lock().unwrap();
        crate::asks::create(
            &db,
            &session_id,
            &slot_id,
            capability,
            &serde_json::to_string(&request).unwrap_or_else(|_| "{}".into()),
            model.as_deref(),
        )?
    };
    tracing::info!(%id, %capability, holder = %label, "capability asked");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout as u64);
    loop {
        {
            let db = st.db.lock().unwrap();
            if let Some((status, result)) = crate::asks::poll(&db, &id)? {
                let body = result.unwrap_or_default();
                return if status == "done" {
                    Ok(format!("{label} answered:\n{body}"))
                } else {
                    // A failure the asker can read beats a timeout it cannot
                    // explain.
                    Ok(format!("{label} could not do it: {body}"))
                };
            }
        }
        if std::time::Instant::now() >= deadline {
            return Ok(format!(
                "{label} has not answered within {timeout}s. The request is still pending as \
                 {id}; ask again or try another holder."
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

fn is_workspace_tool(tool: &str) -> bool {
    matches!(
        tool,
        "publish_work" | "sync_workspace" | "workspace_status" | "query_code_graph"
    )
}

/// Run a tool, including the ones that need a round trip to a machine.
pub async fn run_tool_async(
    st: &AppState,
    agent_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<(String, Vec<proto::Envelope>)> {
    if tool == "ask_capability" {
        return ask_capability_tool(st, agent_id, args).await.map(|t| (t, vec![]));
    }
    if is_workspace_tool(tool) {
        return workspace_tool(st, agent_id, tool, args).await;
    }
    let out = run_tool(st, agent_id, tool, args)?;
    // Finishing a piece of work is exactly the moment teammates need to see
    // it, so do not make the agent remember a second call.
    if tool == "signal_done" {
        if let Some(extra) = auto_publish(st, agent_id).await {
            return Ok((format!("{}\n\n{extra}", out.0), out.1));
        }
    }
    Ok(out)
}

fn slot_and_name(st: &AppState, agent_id: &str) -> Option<(String, String)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT s.id, COALESCE(s.display_name, s.role, s.id)
         FROM agents a JOIN slots s ON s.id = a.slot_id WHERE a.id = ?",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

/// Answer a code-graph question, parsing the workspace first if need be.
async fn code_graph_tool(
    st: &AppState,
    ws_id: &str,
    node_id: &str,
    path: &str,
    args: &serde_json::Value,
) -> Result<String> {
    let what = args
        .get("what")
        .and_then(|v| v.as_str())
        .unwrap_or("overview")
        .to_string();
    let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rebuild = args.get("rebuild").and_then(|v| v.as_bool()).unwrap_or(false);

    // Built on first use rather than requiring anyone to remember: an agent
    // asking a question should get an answer, not an instruction.
    let have = { crate::graph::exists(&st.db.lock().unwrap(), ws_id) };
    let mut preamble = String::new();
    if rebuild || !have {
        preamble = format!("{}\n", crate::graph::build(st, ws_id, node_id, path).await?);
    }
    let answer = {
        let db = st.db.lock().unwrap();
        crate::graph::query(&db, ws_id, &what, &target, &to)?
    };
    Ok(format!("{preamble}{answer}"))
}

async fn workspace_tool(
    st: &AppState,
    agent_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<(String, Vec<proto::Envelope>)> {
    let (slot_id, who) = slot_and_name(st, agent_id)
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_id}'"))?;

    // The code graph needs a directory, not a git remote, so it is answered
    // for any workspace -- including a node-local one.
    if tool == "query_code_graph" {
        let Some((ws_id, node_id, path)) = crate::sync::workspace_host(st, &slot_id) else {
            return Ok((
                "You have no workspace on a connected machine, so there are no files to parse."
                    .into(),
                vec![],
            ));
        };
        return code_graph_tool(st, &ws_id, &node_id, &path, args).await.map(|t| (t, vec![]));
    }

    let Some((ws_id, node_id, path)) = crate::sync::slot_workspace(st, &slot_id) else {
        return Ok((
            "You are not in a shared workspace: your files are local to this machine, so there \
             is nothing to publish or pull. Tell teammates what you found with send_message and \
             store_knowledge instead."
                .into(),
            vec![],
        ));
    };
    let ws = crate::sync::load(st, &ws_id)?;
    let op = match tool {
        "publish_work" => proto::SyncOp::Publish,
        "sync_workspace" => proto::SyncOp::Refresh,
        _ => proto::SyncOp::Status,
    };
    let message = args
        .get("summary")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("{who}: {s}"))
        .unwrap_or_else(|| format!("{who}: work in progress"));

    let v = crate::sync::run_on(st, &ws, &node_id, &path, op, &who, &message).await?;
    crate::sync::record(st, &ws_id, &node_id, &v);

    let detail = v["detail"].as_str().unwrap_or("done").to_string();
    if v["conflict"].as_bool() == Some(true) {
        // A conflict is the agent's to resolve; say so plainly rather than
        // reporting a vague failure.
        return Ok((format!("CONFLICT: {detail}"), vec![]));
    }
    if v["ok"].as_bool() != Some(true) {
        return Ok((format!("Could not {tool}: {detail}"), vec![]));
    }
    // When work is shared, the team should hear about it.
    if op == proto::SyncOp::Publish && detail.starts_with("published") {
        let db = st.db.lock().unwrap();
        let session: Option<String> = db
            .query_row("SELECT session_id FROM slots WHERE id = ?", [&slot_id], |r| r.get(0))
            .optional()?;
        if let Some(session) = session {
            let _ = db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
                 VALUES (?, ?, NULL, 'system', ?, ?)",
                rusqlite::params![
                    session,
                    slot_id,
                    format!("{who} published work to {}. Call sync_workspace to pull it in.", ws.branch),
                    now_ms()
                ],
            );
        }
    }
    Ok((detail, vec![]))
}

/// Publish on the agent's behalf when it signals done, reporting what happened
/// but never failing the signal itself.
async fn auto_publish(st: &AppState, agent_id: &str) -> Option<String> {
    let (slot_id, who) = slot_and_name(st, agent_id)?;
    let (ws_id, node_id, path) = crate::sync::slot_workspace(st, &slot_id)?;
    let ws = crate::sync::load(st, &ws_id).ok()?;
    let v = crate::sync::run_on(
        st,
        &ws,
        &node_id,
        &path,
        proto::SyncOp::Publish,
        &who,
        &format!("{who}: finished work"),
    )
    .await
    .ok()?;
    crate::sync::record(st, &ws_id, &node_id, &v);
    let detail = v["detail"].as_str().unwrap_or("").to_string();
    Some(if v["conflict"].as_bool() == Some(true) {
        format!("Your work could NOT be shared: {detail}")
    } else if v["ok"].as_bool() == Some(true) {
        format!("Your work was shared with the team ({detail}).")
    } else {
        format!("Your work could not be shared: {detail}")
    })
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
    cli_session: Option<String>,
) -> Result<(String, String, String, Option<String>, Option<String>)> {
    let db = st.db.lock().unwrap();
    let now = now_ms();

    // Pick the session: the one named, else the one that owns the directory
    // you are standing in, else the only active one.
    //
    // The directory matters because that is how a person actually works: you
    // cd into a project and start a CLI, or resume one there. Having to
    // remember a session id to do that is a worse interface than the
    // filesystem, which already knows which project this is.
    let session_id: String = match session_hint {
        Some(s) => db
            .query_row("SELECT id FROM sessions WHERE id = ? OR name = ?", params![s, s], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no such session '{s}'"))?,
        None => {
            let by_cwd = cwd.as_deref().and_then(|c| session_for_cwd(&db, node_id, c).ok()).flatten();
            match by_cwd {
                Some(id) => {
                    tracing::info!(session = %id, cwd = ?cwd, "session taken from the directory");
                    id
                }
                None => {
                    let ids: Vec<(String, String)> = db
                        .prepare("SELECT id, name FROM sessions WHERE status = 'active'")?
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<std::result::Result<_, _>>()?;
                    match ids.len() {
                        1 => ids[0].0.clone(),
                        0 => anyhow::bail!("no active session to join"),
                        _ => {
                            // Name them. "3 active sessions" leaves the reader
                            // to go and look up what they are called.
                            let names: Vec<String> =
                                ids.iter().map(|(_, n)| n.clone()).collect();
                            anyhow::bail!(
                                "this directory belongs to no workspace, and there are {} \
                                 active sessions ({}). Say which: NEXMESH_SESSION=<name>, \
                                 or give the session a workspace mounted here",
                                ids.len(),
                                names.join(", ")
                            )
                        }
                    }
                }
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

    // Reuse the seat this identity already holds, if whatever was in it has
    // gone. An attached agent is a process somebody runs by hand, and it is
    // run repeatedly -- restarted, or probed by its own tooling. Taking a
    // fresh seat each time fills a session with teammates that never answer:
    // `hermes mcp add` followed by `hermes mcp test` made two before anyone
    // had started a conversation.
    //
    // A seat whose agent is still LIVE is left alone: that is a genuinely
    // concurrent second instance, and it deserves its own.
    // The conversation, when the CLI publishes one, is a better identity than
    // anything we could infer. It is stable across a resume -- where the pid
    // is not -- and different between two windows open on the same folder,
    // which is precisely the case a directory cannot tell apart.
    //
    // A seat matched this way is reclaimed even if its last agent still looks
    // live: one conversation cannot be in two places, so whatever is recorded
    // there has gone whether or not anybody said so.
    let by_conversation: Option<String> = match cli_session.as_deref() {
        Some(cs) if !cs.is_empty() => db
            .query_row(
                "SELECT id FROM slots
                  WHERE session_id = ?1 AND participant_kind = 'cli'
                    AND cli_session = ?2
                  ORDER BY created_at_ms DESC LIMIT 1",
                params![session_id, cs],
                |r| r.get(0),
            )
            .optional()?,
        _ => None,
    };
    // One pinned identity is one process, so joining here means it is no
    // longer sitting anywhere else. This is what lets an agent MOVE between
    // sessions: point it at another session and it takes (or returns to) its
    // seat there, while the seat it left stops claiming to be occupied
    // instead of showing a teammate who will never answer.
    if let Some(cs) = cli_session.as_deref().filter(|c| !c.is_empty()) {
        db.execute(
            "UPDATE agents SET exited_at_ms = COALESCE(exited_at_ms, ?1)
              WHERE exited_at_ms IS NULL
                AND slot_id IN (SELECT id FROM slots WHERE cli_session = ?2)",
            params![now, cs],
        )?;
    }

    let existing: Option<String> = match by_conversation {
        Some(id) => Some(id),
        None => db
        .query_row(
            "SELECT s.id FROM slots s
              WHERE s.session_id = ?1 AND s.participant_kind = 'cli'
                AND COALESCE(s.cli_id,'') = COALESCE(?2,'')
                AND COALESCE(s.display_name,'') = COALESCE(?3,'')
                AND COALESCE(s.cli_session,'') = ''
                AND NOT EXISTS (
                    SELECT 1 FROM agents a
                     WHERE a.slot_id = s.id AND a.exited_at_ms IS NULL
                       AND a.status <> 'lost'
                       AND a.provenance = 'attached')
                AND EXISTS (
                    SELECT 1 FROM agents a
                     WHERE a.slot_id = s.id AND a.node_id = ?4
                       AND a.provenance = 'attached')
              ORDER BY s.created_at_ms DESC LIMIT 1",
            params![session_id, cli_id, name, node_id],
            |r| r.get(0),
        )
        .optional()?,
    };

    let slot_id = match existing {
        Some(id) => {
            db.execute(
                "UPDATE slots SET status = 'connected', workspace_id = COALESCE(?, workspace_id),
                                  role = COALESCE(?, role)
                  WHERE id = ?",
                params![workspace_id, role, id],
            )?;
            // Adopt: an identity that arrives for the first time and finds its
            // own seat by name takes that seat over, rather than leaving an
            // unclaimed twin behind. Without this, pinning NEXMESH_CLI_SESSION
            // for an agent that already had a seat makes a SECOND one, which
            // looks exactly like the duplication the pin was meant to stop.
            if let Some(cs) = cli_session.as_deref().filter(|c| !c.is_empty()) {
                db.execute(
                    "UPDATE slots SET cli_session = ? WHERE id = ? AND cli_session IS NULL",
                    params![cs, id],
                )?;
            }
            tracing::info!(slot_id = %id, "an attached agent returned to its seat");
            id
        }
        None => {
            let id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            db.execute(
                "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                                    display_name, role, status, created_at_ms, cli_session)
                 VALUES (?,?,?,?,'cli','interactive',?,?,'connected',?,?)",
                params![id, session_id, workspace_id, cli_id, name, role, now, cli_session],
            )?;
            id
        }
    };
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
/// Which session owns the directory an agent started in.
///
/// Only workspaces mounted on THIS node are considered: a path is a claim
/// about one machine, and `/srv/app` on one host is not `/srv/app` on
/// another. The longest matching mount wins, so a workspace nested inside
/// another is not shadowed by its parent.
///
/// Two active sessions mounting the same directory on the same machine is a
/// genuine ambiguity, and this refuses rather than picking one: joining the
/// wrong team is worse than being asked which.
fn session_for_cwd(
    db: &rusqlite::Connection,
    node_id: &str,
    cwd: &str,
) -> Result<Option<String>> {
    let rows: Vec<(String, String)> = db
        .prepare(
            "SELECT m.path, w.session_id FROM workspace_mounts m
               JOIN workspaces w ON w.id = m.workspace_id
               JOIN sessions sx ON sx.id = w.session_id
              WHERE m.node_id = ? AND sx.status = 'active'",
        )?
        .query_map([node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let cwd_t = cwd.trim_end_matches('/');
    let mut best_len = 0usize;
    let mut best: Vec<String> = Vec::new();
    for (path, session_id) in rows {
        let p = path.trim_end_matches('/');
        let contained =
            cwd_t == p || cwd_t.strip_prefix(p).map(|r| r.starts_with('/')).unwrap_or(false);
        if !contained {
            continue;
        }
        if p.len() > best_len {
            best_len = p.len();
            best = vec![session_id];
        } else if p.len() == best_len && !best.contains(&session_id) {
            best.push(session_id);
        }
    }
    match best.len() {
        0 => Ok(None),
        1 => Ok(Some(best.remove(0))),
        _ => anyhow::bail!(
            "{} active sessions have a workspace mounted at this directory; \
             set NEXMESH_SESSION to say which",
            best.len()
        ),
    }
}

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
