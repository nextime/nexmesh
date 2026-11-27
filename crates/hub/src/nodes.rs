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

    // Before anything else on this connection: settle what this node still
    // has. A node that restarted lists nothing, which retires the agents its
    // predecessor spawned instead of leaving their seats held by processes
    // that no longer exist. Done here, ahead of the first `AgentUp`, so a
    // reconnecting node's own agents are never caught by it.
    if let NodeToHub::Hello { live_agents: Some(live), .. } = &hello {
        let gone = st.reconcile_node_agents(&node_id, live);
        if !gone.is_empty() {
            tracing::info!(node = %issued, retired = gone.len(), kept = live.len(),
                "node reconnected without these agents; their seats are free");
        }
    }

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
                    // A process that is HERE has not exited. A node restart
                    // writes its attached agents off -- rightly, nothing was
                    // left to reach them through -- and then the shims
                    // reconnect and the node vouches for them again. Without
                    // clearing the exit time those rows read `running` with
                    // an exit beside them, and everything that asks "is there
                    // a process?" answered no: `attach` refused a CLI that was
                    // sitting right there, the seat read as between turns, and
                    // seat reclaim thought the chair was free.
                    db.execute(
                        "UPDATE agents SET last_seen_ms = ?,
                            exited_at_ms = CASE WHEN status = 'disconnected'
                                                THEN NULL ELSE exited_at_ms END,
                            status = CASE WHEN status = 'disconnected' THEN 'running'
                                          ELSE status END
                         WHERE id = ?",
                        params![now_ms(), a.agent_id],
                    )?;
                    // A lost `Activity` frame is repaired here, a heartbeat
                    // later. The time is kept when the state has not moved.
                    if let Some(act) = a.activity {
                        note_turn(st, &db, &a.agent_id, act.as_str());
                        db.execute(
                            "UPDATE agents SET activity = ?1,
                                    activity_at_ms = CASE WHEN activity IS ?1
                                                          THEN activity_at_ms ELSE ?2 END
                              WHERE id = ?3",
                            params![act.as_str(), now_ms(), a.agent_id],
                        )?;
                    }
                } else {
                    db.execute(
                        "UPDATE agents SET status = 'disconnected' WHERE id = ? AND exited_at_ms IS NULL",
                        [&a.agent_id],
                    )?;
                }
            }
        }

        NodeToHub::AgentUp {
            agent_id, slot_id, proc: id, provenance, cwd, resume_handle, push_capable, io_kind,
            typed_delivery,
        } => {
            let db = st.db.lock().unwrap();
            let now = now_ms();
            // THE SAME PROCESS, UNDER A NEW AGENT ID.
            //
            // tmux supervision made that ordinary: the CLI outlives the node,
            // and after a node restart the node adopts the pane and registers
            // it as a fresh agent -- same machine, same pid, same start
            // ticks. `agents_identity` is UNIQUE on exactly that triple and
            // `ON CONFLICT(id)` below does not cover it, so the whole insert
            // failed, the frame was rejected with "UNIQUE constraint failed",
            // and the hub went on believing the seat was empty. `attach`
            // starts a seat with no process, so it started it again, and
            // again: four rounds on the live fleet with the CLI sitting right
            // there, and the only way out was killing a tmux session by hand.
            //
            // The triple is what says "this is the same process", so an older
            // row carrying it describes THIS process under an id nothing will
            // ever update again. Its bookkeeping ends here: the pid moves to
            // the row that is actually tracking the process, and the old row
            // keeps its times and its token counts as history.
            let superseded: Vec<String> = db
                .prepare(
                    "SELECT id FROM agents
                      WHERE node_id = ?1 AND pid = ?2 AND proc_start_ticks = ?3 AND id <> ?4",
                )
                .and_then(|mut q| {
                    q.query_map(params![node_id, id.pid, id.start_ticks, agent_id], |r| r.get(0))
                        .and_then(|m| m.collect())
                })
                .unwrap_or_default();
            for old in &superseded {
                // pid cleared, not just the status: the unique index is
                // partial (WHERE pid IS NOT NULL), so leaving the pid there
                // would keep the conflict and the insert would fail again.
                db.execute(
                    "UPDATE agents SET pid = NULL, status = 'exited',
                                       exited_at_ms = COALESCE(exited_at_ms, ?2)
                      WHERE id = ?1",
                    params![old, now],
                )?;
                tracing::info!(old = %old, new = %agent_id, pid = id.pid, node = %node_id,
                    "the same process has re-registered under a new agent id; \
                     the earlier row is now history");
            }
            db.execute(
                "INSERT INTO agents (id, slot_id, node_id, pid, proc_start_ticks, provenance,
                                     status, cwd, started_at_ms, last_seen_ms, push_capable,
                                     io_kind, typed_delivery, resumed)
                 VALUES (?1,?2,?3,?4,?5,?6,'running',?7,?8,?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET status = 'running', last_seen_ms = ?8,
                                               -- Up again is not exited.
                                               exited_at_ms = NULL,
                                               io_kind = COALESCE(?10, io_kind),
                                               typed_delivery = ?11",
                params![
                    agent_id, slot_id, node_id, id.pid, id.start_ticks,
                    serde_json::to_value(provenance)?.as_str().unwrap_or("managed"),
                    cwd, now, push_capable as i64, io_kind, typed_delivery as i64,
                    // Did the hub hand this start a conversation to resume?
                    // A start that dies seconds after being given one died
                    // on OUR argument, and recovery treats that differently
                    // from a seat exiting on its own account.
                    i64::from(resume_handle.is_some())
                ],
            )?;
            db.execute("UPDATE slots SET status = 'connected' WHERE id = ?", [&slot_id])?;
            if let Some(h) = resume_handle {
                db.execute("UPDATE slots SET resume_handle = ? WHERE id = ?", params![h, slot_id])?;
            }
            tracing::info!(node = %node_name, %agent_id, %slot_id, pid = id.pid, "agent up");
        }

        // An agent is stopped at a permission prompt with nobody attached.
        // The node did NOT answer it: it read the menu and is waiting.
        NodeToHub::ApprovalAsked { agent_id, kind, question, options, fingerprint } => {
            // `asked` returns a decision only when a standing auto-approval
            // covers this seat, in which case the wait is invisible. The
            // reply goes back on this node's own connection, since only this
            // node can type at that terminal.
            if let Some(decided) = crate::approvals::asked(
                st, &agent_id, &kind, &question, &options, &fingerprint)
            {
                if let Ok(s) = serde_json::to_string(&decided) {
                    let _ = tx.send(s).await;
                }
            }
        }
        NodeToHub::ApprovalWithdrawn { agent_id, fingerprint } => {
            crate::approvals::withdrawn(st, &agent_id, &fingerprint);
        }
        NodeToHub::Noted { agent_id, text } => {
            // Pre-claimed, like the console's own housekeeping lines: this is
            // a record for whoever reads the room later, never a message an
            // agent should answer.
            let db = st.db.lock().unwrap();
            let found: Option<(String, String)> = db
                .query_row(
                    "SELECT s.session_id, s.id FROM agents a JOIN slots s ON s.id = a.slot_id
                      WHERE a.id = ?",
                    [&agent_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap_or(None);
            if let Some((session_id, slot_id)) = found {
                let now = crate::now_ms();
                let _ = db.execute(
                    "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                           sent_at_ms, claimed_at_ms, claimed_by, record)
                     VALUES (?1, ?2, NULL, 'system', ?3, ?4, ?4, 'node', 1)",
                    rusqlite::params![session_id, slot_id, text, now],
                );
            } else {
                tracing::debug!(%agent_id, "a note for an agent with no seat");
            }
        }

        NodeToHub::AgentExited { agent_id, code, signal } => {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE agents SET status = 'exited', exited_at_ms = ? WHERE id = ?",
                params![now_ms(), agent_id],
            )?;
            // Whatever it was stopped on cannot be answered now: there is no
            // terminal left to type the keystroke at.
            crate::approvals::retire_gone(&db);
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
                // A seat a hand-run CLI took by itself has nothing to come
                // back to it: the operator closed a terminal. Left behind it
                // is a teammate in the roster that is not there. Only a seat
                // carrying nothing goes -- see `retire_if_throwaway`, which
                // keeps every seat an operator configured, because a hand-run
                // conversation returns to its own seat.
                if let Err(e) = crate::seat::retire_if_throwaway(&db, &slot) {
                    tracing::error!(error = %e, slot = %slot,
                        "could not decide whether to retire this seat; leaving it");
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
            // BOUND TO THE PLACE IT EXISTS IN.
            //
            // A conversation is a local file on one machine, in one
            // directory, belonging to one CLI. Recorded per place, so a seat
            // that moves to another machine is not offered a conversation
            // that cannot be there -- and a seat that moves BACK finds the
            // one it left. The key is the seat, so renaming it or moving it
            // between teams changes nothing.
            let place: Option<(String, String, String)> = db
                .query_row(
                    "SELECT a.node_id, COALESCE(a.cwd, ''), COALESCE(s.cli_id, '')
                       FROM agents a JOIN slots s ON s.id = a.slot_id
                      WHERE a.id = ?",
                    [&agent_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .unwrap_or(None);
            if let Some((node, cwd, cli)) = place {
                db.execute(
                    "INSERT INTO slot_conversations
                        (slot_id, node_id, cwd, cli_id, handle, updated_at_ms)
                     VALUES (?,?,?,?,?,?)
                     ON CONFLICT(slot_id, node_id, cwd, cli_id) DO UPDATE SET
                        handle = excluded.handle,
                        updated_at_ms = excluded.updated_at_ms",
                    params![slot_id, node, cwd, cli, handle, crate::now_ms()],
                )
                .ok();
                tracing::info!(%slot_id, %node, %cwd, %cli, %handle,
                               "bound this seat's conversation to where it lives");
            }
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

        NodeToHub::CodeGraphResult { req_id, workspace_id, graph, seq: _, last } => {
            // A graph arrives in pieces; there is nothing to do with one
            // until the last lands.
            let Some(graph) = st.take_graph_piece(req_id, graph, last) else {
                return Ok(());
            };
            // Hand it to whoever asked, through the same waiter the workspace
            // sync round-trip uses.
            let taken =
                st.complete_sync(req_id, serde_json::to_value(&graph).unwrap_or_default());
            if !taken {
                // Nobody is waiting any more: the parse outlived the caller's
                // patience. Store it anyway. The expensive work is done, and
                // throwing it away leaves the workspace with no graph at all
                // -- so the next ask starts from nothing and times out in the
                // same place, for ever.
                match crate::graph::store_off_thread(st, &workspace_id, graph).await {
                    Ok(summary) => tracing::info!(%workspace_id, %summary,
                        "stored a code graph that arrived after its caller had gone"),
                    Err(e) => tracing::warn!(%workspace_id, error = %e,
                        "could not store a late code graph"),
                }
            }
        }

        NodeToHub::TreeFingerprintResult {
            req_id: _,
            workspace_id,
            fingerprint,
            files,
            partial,
        } => {
            let db = st.db.lock().unwrap();
            crate::graph::fingerprint_arrived(&db, &workspace_id, &fingerprint, files, partial);
        }

        NodeToHub::AgentModel { agent_id, model } => {
            let db = st.db.lock().unwrap();
            db.execute("UPDATE agents SET model = ? WHERE id = ?", params![model, agent_id])?;
            tracing::info!(%agent_id, %model, "agent reported its model");
        }

        NodeToHub::InjectionsLost { agent_id, message_ids } => {
            // Back in the queue, or they are never redelivered.
            //
            // Per recipient as well as per message: the row the agent was
            // holding is the one that has to be freed. Clearing only the
            // message-level columns left that row in flight for ever, and a
            // row in flight is never handed over again -- so a terminal that
            // was busy for one moment would have dropped that seat's copy
            // silently, which is the opposite failure to the one that made
            // this obvious.
            let db = st.db.lock().unwrap();
            for id in message_ids {
                db.execute(
                    "UPDATE message_recipients SET inflight_agent = NULL, inflight_at_ms = NULL
                      WHERE message_id = ?1 AND inflight_agent = ?2 AND acked_at_ms IS NULL",
                    params![id, agent_id],
                )?;
                db.execute(
                    "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                     WHERE id = ? AND acked_at_ms IS NULL",
                    [id],
                )?;
            }
        }

        NodeToHub::FileBytes { req_id, ok, data_b64, media_type, detail } => {
            st.complete_sync(
                req_id,
                serde_json::json!({
                    "ok": ok, "data_b64": data_b64, "media_type": media_type, "detail": detail
                }),
            );
        }

        NodeToHub::TerminalMouseSet { req_id, ok, detail } => {
            st.complete_sync(req_id, serde_json::json!({ "ok": ok, "detail": detail }));
        }

        NodeToHub::TicketStored { req_id } => {
            st.complete_sync(req_id, serde_json::json!({ "ok": true }));
        }

        NodeToHub::PutFileResult { req_id, ok, path, bytes, detail } => {
            st.complete_sync(
                req_id,
                serde_json::json!({ "ok": ok, "path": path, "bytes": bytes, "detail": detail }),
            );
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

        // WHAT THE MODEL IS DOING. The node's clock says when; it is capped at
        // ours so a node running fast cannot date it in the future.
        NodeToHub::Activity { agent_id, state, since_ms } => {
            let db = st.db.lock().unwrap();
            note_turn(st, &db, &agent_id, state.as_str());
            db.execute(
                "UPDATE agents SET activity = ?, activity_at_ms = ? WHERE id = ?",
                params![state.as_str(), since_ms.min(now_ms()), agent_id],
            )?;
        }

        // The agent cannot reach its model, or can again.
        NodeToHub::UsageLimited { agent_id, block } => {
            crate::quota::report(st, &agent_id, block);
        }

        NodeToHub::FileReads { agent_id, n } => {
            let db = st.db.lock().unwrap();
            let (_, graph) = st.fleet_reminders();
            crate::reminders::deliver(
                &db,
                crate::reminders::files_read(&db, &agent_id, n as i64, graph),
            );
        }

        // HOW FULL this agent's context is -- not what it has spent. See
        // `agents.context_used_tokens` for why those are different numbers.
        NodeToHub::ContextUsage { agent_id, used, limit, source } => {
            let next = {
                let db = st.db.lock().unwrap();
                crate::watchdog::record_context(
                    &db,
                    st.fleet_compact_at_pct(),
                    &agent_id,
                    used,
                    limit,
                    &source,
                )?
            };
            // Outside the lock, like a budget: asking a seat to compact
            // talks to a node, and holding the database while waiting on a
            // socket stops the whole fleet.
            match next {
                // A fold comes FIRST, and the compaction follows on a later
                // reading if the seat does nothing about it: what the agent
                // writes down survives the compaction, and what the
                // compaction leaves behind is a summary of a summary.
                crate::watchdog::Next::Fold(n) => crate::watchdog::nudge(st, n).await,
                crate::watchdog::Next::Compact(full) => {
                    crate::watchdog::compact(st, full).await
                }
                crate::watchdog::Next::Nothing => {}
            }
        }

        // The same turn, over and over. The node reports the cycle; whether
        // that many repeats is a loop is this fleet's setting.
        NodeToHub::LoopSuspected { agent_id, period, repeats } => {
            let looping = {
                let db = st.db.lock().unwrap();
                crate::watchdog::record_loop(
                    &db,
                    st.fleet_loop_repeats(),
                    &agent_id,
                    period,
                    repeats,
                )?
            };
            if let Some(it) = looping {
                crate::watchdog::halt(st, it).await;
            }
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

        NodeToHub::TerminalLink { agent_id, slot_id, via_agent_id, cli } => {
            let db = st.db.lock().unwrap();
            // Recorded on the SEAT, not the agent, because that is what it
            // is about: whoever is in this seat is reachable by keyboard for
            // as long as that terminal is adopted. The shim that proved it
            // may be gone by tomorrow; the CLI it was a child of is what the
            // node will check against.
            db.execute(
                "UPDATE slots SET terminal_via = ?, terminal_cli_pid = ?,
                                  terminal_cli_ticks = ?, terminal_at_ms = ?
                  WHERE id = ?",
                params![via_agent_id, cli.pid, cli.start_ticks as i64, now_ms(), slot_id],
            )?;
            tracing::info!(
                node = %node_name, %agent_id, %slot_id, via = %via_agent_id, pid = cli.pid,
                "seat is reachable through an adopted terminal"
            );
        }

        NodeToHub::MessageAcked { agent_id, message_ids } => {
            let db = st.db.lock().unwrap();
            tools::ack_messages(&db, &agent_id, &message_ids)?;
        }

        NodeToHub::JoinRequest {
            req_id, session_hint, cli_id, role, name, cwd, proc: pid, cli_session,
            cli_session_is_own,
        } => {
            let granted = join(st, node_id, session_hint, cli_id, role, name, cwd, pid,
                               cli_session, cli_session_is_own);
            let frame = match granted {
                Ok((slot_id, session_id, agent_id, display_name, role)) => {
                    // The seat's authority decides the tool list its shim
                    // advertises, and only the hub knows it.
                    let authority = {
                        let db = st.db.lock().unwrap();
                        crate::fleet::authority_of(&db, &slot_id).as_str().to_string()
                    };
                    HubToNode::JoinGranted {
                        req_id, slot_id, session_id, agent_id, display_name, role,
                        authority: Some(authority),
                    }
                }
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

/// The longest a `pending_asks` call may be held, asked for or not.
const ASKS_WAIT_MAX_S: i64 = 30;

/// How long a PROGRAM's `pending_asks` is held when it did not say.
///
/// A capability bridge is a program that asks "anything for me?" in a loop,
/// and on the live fleet one did so 50,000 times in thirty hours -- twice a
/// second, every answer "nothing". Holding an empty answer for a few seconds
/// and returning the moment a request lands cuts that tenfold without making
/// any request wait longer. Only for an external participant: a model seat
/// that calls it wants its answer now, and gets the hold only by asking for
/// it with `wait_seconds`. Short, so a client with a short timeout of its own
/// does not see its calls fail.
const ASKS_HOLD_PROGRAM_S: i64 = 4;

/// Wait, if this call should, until a request is waiting or the time is up.
/// The call itself runs afterwards as usual, so it is counted once.
async fn hold_for_asks(st: &AppState, agent_id: &str, args: &serde_json::Value) {
    let (slot, session, external) = {
        let db = st.db.lock().unwrap();
        match db
            .query_row(
                "SELECT s.id, s.session_id, COALESCE(s.participant_kind, '') = 'external'
                   FROM agents a JOIN slots s ON s.id = a.slot_id WHERE a.id = ?",
                [agent_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)),
            )
            .optional()
        {
            Ok(Some(row)) => row,
            _ => return,
        }
    };
    let wait = args
        .get("wait_seconds")
        .and_then(|v| v.as_i64())
        .unwrap_or(if external { ASKS_HOLD_PROGRAM_S } else { 0 })
        .clamp(0, ASKS_WAIT_MAX_S);
    let until = now_ms() + wait * 1000;
    while now_ms() < until {
        let waiting = {
            let db = st.db.lock().unwrap();
            !crate::asks::pending_for(&db, &slot).map(|v| v.is_empty()).unwrap_or(true)
                // A message for it is a reason to answer too: it rides back
                // with the result, and should not wait out the hold.
                || tools::pending_for_slot(&db, &session, &slot)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
        };
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

/// A turn ENDED when an agent goes from active to idle. Read before the
/// activity is written, from either the Activity frame or the heartbeat that
/// repairs a lost one -- whichever arrives first sees the transition, and the
/// other finds it already idle.
fn note_turn(st: &AppState, db: &rusqlite::Connection, agent_id: &str, new: &str) {
    if new != "idle" {
        return;
    }
    let was: Option<String> = db
        .query_row("SELECT activity FROM agents WHERE id = ?", [agent_id], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    if was.as_deref() == Some("active") {
        let (memory, _) = st.fleet_reminders();
        crate::reminders::deliver(db, crate::reminders::turn_ended(db, agent_id, memory));
    }
}

/// Run a tool, including the ones that need a round trip to a machine.
pub async fn run_tool_async(
    st: &AppState,
    agent_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<(String, Vec<proto::Envelope>)> {
    // Counted here for the tools that never reach `tools::dispatch` (which
    // counts the rest): without this, every code-graph query on the fleet
    // went unrecorded and the dashboard said no seat had ever used it.
    if tool == "ask_capability" || is_workspace_tool(tool) {
        let db = st.db.lock().unwrap();
        if let Ok(slot) = db.query_row(
            "SELECT slot_id FROM agents WHERE id = ?",
            [agent_id],
            |r| r.get::<_, String>(0),
        ) {
            let _ = db.execute(
                "INSERT INTO tool_counts (slot_id, tool, n, last_ms) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(slot_id, tool) DO UPDATE SET n = n + 1, last_ms = ?3",
                params![slot, tool, now_ms()],
            );
        }
    }
    if tool == "pending_asks" {
        hold_for_asks(st, agent_id, args).await;
    }
    let out = if tool == "ask_capability" {
        ask_capability_tool(st, agent_id, args).await.map(|t| (t, vec![]))
    } else if is_workspace_tool(tool) {
        workspace_tool(st, agent_id, tool, args).await
    } else {
        run_tool(st, agent_id, tool, args)
    };
    // What a successful call answers, for the reminders: keeping something
    // resets the turn count, a graph query the read count.
    if out.is_ok() {
        let db = st.db.lock().unwrap();
        let _ = crate::reminders::tool_called(&db, agent_id, tool);
        if tool == "signal_done" {
            let (memory, _) = st.fleet_reminders();
            crate::reminders::deliver(&db, crate::reminders::work_done(&db, agent_id, memory));
        }
    }
    let out = out?;
    if is_workspace_tool(tool) || tool == "ask_capability" {
        return Ok(out);
    }
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
    session_id: &str,
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
    let scope = crate::graph::GraphScope::parse(args.get("scope").and_then(|v| v.as_str()))?;

    // A WIDER question answers from what has already been parsed, and never
    // parses on behalf of somebody else's workspace: that is minutes of
    // another machine's time, and the agent asking it is not the one who
    // decided to run an agent there.
    if scope != crate::graph::GraphScope::Workspace {
        if rebuild {
            anyhow::bail!(
                "rebuild parses one workspace on one machine, so ask it of your own: \
                 drop the scope. A wider question answers from the workspaces already \
                 parsed."
            );
        }
        let db = st.db.lock().unwrap();
        let places = crate::graph::parsed_in_scope(&db, scope, session_id, ws_id)?;
        if places.is_empty() {
            return Ok(format!(
                "No workspace {} has been parsed yet. Ask about your own workspace first \
                 (drop the scope), which parses it.",
                if scope == crate::graph::GraphScope::Global { "on the fleet" } else { "in this team" }
            ));
        }
        return crate::graph::query_across(&db, &places, &what, &target, &to);
    }

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
        let session_id: String = {
            let db = st.db.lock().unwrap();
            db.query_row("SELECT session_id FROM slots WHERE id = ?", [&slot_id], |r| r.get(0))
                .optional()
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let Some((ws_id, node_id, path)) = crate::sync::workspace_host(st, &slot_id) else {
            // A seat with no workspace of its own can still ask a WIDER
            // question: the fleet's parsed workspaces are there whether this
            // seat has files or not, and refusing left an agent on a machine
            // with nothing checked out unable to ask where anything is.
            let wider = crate::graph::GraphScope::parse(
                args.get("scope").and_then(|v| v.as_str()),
            )?;
            if wider == crate::graph::GraphScope::Workspace {
                return Ok((
                    "You have no workspace on a connected machine, so there are no files to \
                     parse. Ask with scope 'session' or 'global' to search the workspaces \
                     this fleet has already parsed."
                        .into(),
                    vec![],
                ));
            }
            return code_graph_tool(st, "", "", "", &session_id, args)
                .await
                .map(|t| (t, vec![]));
        };
        return code_graph_tool(st, &ws_id, &node_id, &path, &session_id, args)
            .await
            .map(|t| (t, vec![]));
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

    let ctx = tools::Ctx {
        db: &db,
        session_id: session_id.clone(),
        slot_id: slot_id.clone(),
        flood: (st.fleet_msg_rate(), st.fleet_msg_pair()),
    };
    let text = tools::dispatch(&ctx, tool, args)?;

    // `check_messages` already acked what it showed; don't hand it out twice.
    let deliver = if tool == "check_messages" {
        vec![]
    } else {
        let pending = tools::pending_for_slot(&db, &session_id, &slot_id)?;
        let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
        tools::mark_inflight(&db, agent_id, &slot_id, &ids)?;
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
    cli_session_is_own: bool,
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
            // Where this conversation already sits, before anything is
            // inferred from the filesystem.
            //
            // A directory resolves to ONE session per machine; a conversation
            // does not have to. Two CLIs open in the same folder can belong
            // to different teams, and the one thing that knows which is the
            // seat the conversation already holds. Without this the directory
            // won every restart, so an agent MOVED to another session came
            // back to the session its folder names -- in a new seat, leaving
            // the one it was moved into empty. A move that does not survive a
            // restart is not a move.
            let by_seat: Option<String> = match cli_session.as_deref() {
                Some(cs) if !cs.is_empty() => db
                    .query_row(
                        "SELECT s.session_id FROM slots s
                           JOIN sessions sx ON sx.id = s.session_id
                          WHERE s.cli_session = ?1 AND sx.status = 'active'
                          ORDER BY s.created_at_ms DESC LIMIT 1",
                        [cs],
                        |r| r.get(0),
                    )
                    .optional()?,
                _ => None,
            };
            if let Some(id) = by_seat {
                tracing::info!(session = %id, "session taken from the seat this conversation holds");
                id
            } else {
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
            // The word goes with the time. A row left saying `running` with
            // an exit time beside it is a seat that reads as occupied by a
            // process nobody will ever find: `nexmesh status` showed a pid
            // that had been gone for minutes, because the row still claimed
            // to be live. `disconnected` is left alone -- that one means the
            // hub lost touch, which is a different thing it should keep
            // saying.
            "UPDATE agents SET exited_at_ms = COALESCE(exited_at_ms, ?1),
                               status = CASE WHEN status = 'running' THEN 'exited'
                                             ELSE status END
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
                       AND a.status <> 'disconnected'
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

    // A conversation that has ENDED, in a seat that is still somebody's.
    //
    // A CLI started by hand gives no name, and its conversation id is new
    // every time one is started rather than resumed. So closing `claude` and
    // opening another in the same folder left the old seat empty and minted
    // `Claude-machine-b-2` beside it: the team gains a stranger every morning,
    // and the name everyone addresses belongs to the seat nobody is in.
    //
    // This is a judgement, and the surrounding code is right to be wary of
    // guessing which empty seat was a restart. So it is narrowed until the
    // guess is nearly forced:
    //
    //   - **no name was given.** A registration that names itself says which
    //     seat it wants, and two named conversations deliberately get two
    //     seats -- that is what `NEXMESH_NAME` is for.
    //   - **the last agent was written off.** It has an exit time: either
    //     it reported an ending, or the node it lived on went and the hub
    //     recorded that there is no process left to come back to that seat.
    //     An agent that is merely out of touch has no exit time and counts
    //     as live, so its seat is never taken from under it.
    //   - **it was an attached agent.** A managed seat belongs to whoever
    //     spawned it, and recovery may be about to bring it back.
    //   - **same session, same CLI, same directory**, and nothing live in
    //     the seat now.
    //   - **not released.** Released means the work was accepted and the
    //     seat is finished with, not waiting for its next occupant.
    let mut reclaimed = false;
    let existing = match (existing, name.as_deref().filter(|n| !n.trim().is_empty())) {
        (Some(id), _) => Some(id),
        (None, Some(_)) => None,
        (None, None) => {
            let found: Option<String> = db
                .query_row(
                    "SELECT s.id FROM slots s
                      WHERE s.session_id = ?1 AND s.participant_kind = 'cli'
                        AND COALESCE(s.cli_id,'') = COALESCE(?2,'')
                        AND s.task_state <> 'released'
                        AND NOT EXISTS (
                            SELECT 1 FROM agents a
                             WHERE a.slot_id = s.id AND a.exited_at_ms IS NULL)
                        AND EXISTS (
                            SELECT 1 FROM agents a
                             WHERE a.slot_id = s.id
                               -- Written off, by whichever route. `exited`
                               -- is an ending somebody reported; a seat
                               -- emptied by a NODE restart keeps the word
                               -- `disconnected` and gets an exit TIME --
                               -- the hub saying there is no process left to
                               -- come back to it. Requiring the word was
                               -- too strict, and on a fleet where deploys
                               -- restart nodes it is the common case: the
                               -- seat everyone addresses stayed empty and
                               -- the CLI that meant to be in it took a
                               -- sibling. An agent that is merely out of
                               -- touch has no exit time, and is treated as
                               -- live by the clause above.
                               AND a.exited_at_ms IS NOT NULL
                               AND a.provenance = 'attached'
                               AND COALESCE(a.cwd,'') = COALESCE(?3,'')
                               AND a.started_at_ms = (
                                   SELECT MAX(b.started_at_ms) FROM agents b
                                    WHERE b.slot_id = s.id))
                      -- Where you last were, not the newest seat: a folder
                      -- with two empty seats usually has the one you have
                      -- been using and the twin a restart minted beside it,
                      -- and coming back to the twin would keep the wrong
                      -- name alive.
                      ORDER BY (SELECT MAX(b.started_at_ms) FROM agents b
                                 WHERE b.slot_id = s.id) DESC LIMIT 1",
                    params![session_id, cli_id, cwd],
                    |r| r.get(0),
                )
                .optional()?;
            reclaimed = found.is_some();
            found
        }
    };

    // A person starting a CLI by hand has no name to give, and a nameless
    // seat is useless to read: every view shows a blank where a teammate
    // should be, and two conversations on one machine are indistinguishable.
    // So name it after what it is and where it is -- `Claude-machine-b` -- and
    // number it only when that is taken, which is what happens when the same
    // CLI is open twice on one machine.
    let name = match name.filter(|n| !n.trim().is_empty()) {
        Some(n) => Some(n),
        None => mint_seat_name(&db, &session_id, cli_id.as_deref(), node_id).ok(),
    };

    // The machine's NAME, because that is what a placement records.
    let node_name: String = db
        .query_row("SELECT name FROM nodes WHERE id = ?", [node_id], |r| r.get(0))
        .optional()?
        .unwrap_or_default();

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
                if reclaimed {
                    // Re-pin: this conversation is the seat's now, so a
                    // resume of THIS one comes straight back to it rather
                    // than going round the same reclaim again.
                    db.execute(
                        "UPDATE slots SET cli_session = ?, cli_session_own = ? WHERE id = ?",
                        params![cs, cli_session_is_own as i64, id],
                    )?;
                } else {
                    db.execute(
                        "UPDATE slots SET cli_session = ?, cli_session_own = ?
                          WHERE id = ? AND cli_session IS NULL",
                        params![cs, cli_session_is_own as i64, id],
                    )?;
                }
            }
            if reclaimed {
                tracing::info!(
                    slot_id = %id,
                    "a new conversation took the seat the last one finished in"
                );
            } else {
                tracing::info!(slot_id = %id, "an attached agent returned to its seat");
            }
            id
        }
        None => {
            let id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            db.execute(
                // Remember WHERE this seat lives. An attached agent is a
                // process on one machine, and if the hub is later asked to
                // restart it -- so it can be attached to and detached from
                // like any managed agent -- a seat with no placement is
                // placed by fallback: the first online node with that CLI,
                // in name order. A seat closed on one machine would quietly
                // come back on another.
                "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                                    display_name, role, status, created_at_ms, cli_session,
                                    cli_session_own, placement)
                 VALUES (?,?,?,?,'cli','interactive',?,?,'connected',?,?,?,?)",
                params![id, session_id, workspace_id, cli_id, name, role, now, cli_session,
                        cli_session_is_own as i64,
                        serde_json::json!({ "node": node_name }).to_string()],
            )?;
            id
        }
    };
    // Whatever the seat is actually called is what this agent is called. It
    // matters for a seat that was already named: reporting back the name that
    // was asked for would have the node, the shim and every log line using a
    // name no command can address.
    let name: Option<String> = db
        .query_row("SELECT display_name FROM slots WHERE id = ?", [&slot_id], |r| r.get(0))
        .optional()?
        .flatten();

    // The same process may come back. A node restart does not end the CLI
    // that node was serving -- the conversation is still there, and the shim
    // reconnects -- and the identity triple, machine plus pid plus process
    // start, is what says it is the same process rather than a new one. So
    // the agent row is retaken. Inserting a second one hits the unique index
    // on that triple, and the shim is told its registration was refused for
    // as long as the conversation lasts, which is the exact failure the
    // reconnect exists to prevent.
    let returning: Option<String> = match pid.as_ref() {
        Some(p) => db
            .query_row(
                "SELECT id FROM agents WHERE node_id = ? AND pid = ? AND proc_start_ticks = ?",
                params![node_id, p.pid, p.start_ticks],
                |r| r.get(0),
            )
            .optional()?,
        None => None,
    };
    let agent_id = match returning {
        Some(existing) => {
            db.execute(
                // Same pid, same start ticks: the same process, back in its
                // seat. It has not exited, whatever was written off while it
                // was out of touch -- and the exit time matters more than the
                // word, because every "is there a process here?" reads it.
                // `attach` refused a CLI that was sitting right there, for
                // the whole life of a node restart.
                "UPDATE agents SET slot_id = ?, status = 'running', cwd = ?, last_seen_ms = ?,
                                   exited_at_ms = NULL
                  WHERE id = ?",
                params![slot_id, cwd, now, existing],
            )?;
            tracing::info!(%slot_id, agent_id = %existing, "attached agent rejoined");
            existing
        }
        None => {
            let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            db.execute(
                "INSERT INTO agents (id, slot_id, node_id, pid, proc_start_ticks, provenance,
                                     status, cwd, started_at_ms, last_seen_ms)
                 VALUES (?,?,?,?,?, 'attached', 'running', ?, ?, ?)",
                params![
                    agent_id, slot_id, node_id,
                    pid.as_ref().map(|p| p.pid), pid.as_ref().map(|p| p.start_ticks),
                    cwd, now, now
                ],
            )?;
            agent_id
        }
    };
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,?, 'agent_joined', ?)",
        params![now, name.clone().unwrap_or_default(), node_id,
                serde_json::json!({ "slot": slot_id, "provenance": "attached" }).to_string()],
    )?;
    tracing::info!(%slot_id, %agent_id, "attached agent joined");
    Ok((slot_id, session_id, agent_id, name, role))
}

/// A readable name for a seat nobody named: the CLI, on this machine.
///
/// `claude-alice` on `machine-b` becomes `Claude-machine-b`, matching how these
/// seats are named by hand, and a second one on the same machine becomes
/// `Claude-machine-b-2` rather than a duplicate -- a duplicate name is the one
/// thing that makes a session impossible to reason about, because every
/// command that addresses an agent addresses it by name.
fn mint_seat_name(
    db: &rusqlite::Connection,
    session_id: &str,
    cli_id: Option<&str>,
    node_id: &str,
) -> Result<String> {
    let node: String = db
        .query_row("SELECT name FROM nodes WHERE id = ?", [node_id], |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| "node".to_string());
    // The part before the first dash is the CLI itself; what follows names
    // one of several accounts for it, which is not interesting in a seat name.
    let family = cli_id.unwrap_or("agent").split('-').next().unwrap_or("agent");
    let mut c = family.chars();
    let pretty = match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => "Agent".to_string(),
    };
    let base = format!("{pretty}-{node}");
    for n in 1..100 {
        let candidate = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let taken: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM slots WHERE session_id = ? AND display_name = ?)",
            params![session_id, candidate],
            |r| r.get(0),
        )?;
        if !taken {
            return Ok(candidate);
        }
    }
    Ok(format!("{base}-{}", &uuid::Uuid::new_v4().to_string()[..4]))
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
pub(crate) fn session_for_cwd(
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
        assert_eq!(workspace_for_cwd(&mounts(), "/home/alice"), None);
        assert_eq!(workspace_for_cwd(&[], "/srv/app"), None);
    }
}

/// A process that comes back under a new agent id.
#[cfg(test)]
mod reregister_tests {
    use super::*;
    use crate::state::AppState;

    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        {
            let db = st.db.lock().unwrap();
            db.execute_batch(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);
                 INSERT INTO nodes (id,name,os,arch,node_version,labels,status,
                                    first_seen_ms,last_seen_ms,lease_expires_ms)
                   VALUES ('n1','machine-b','linux','x86_64','0','[]','online',1,1,9999999999999);
                 INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                   VALUES ('sl-1','s1','Dev','claude',1);",
            )
            .unwrap();
        }
        st
    }

    fn up(agent: &str) -> proto::NodeToHub {
        proto::NodeToHub::AgentUp {
            agent_id: agent.to_string(),
            slot_id: "sl-1".to_string(),
            proc: proto::ProcIdentity {
                node_id: "machine-b".to_string(),
                pid: 4242,
                start_ticks: 777,
            },
            provenance: proto::Provenance::Managed,
            cwd: "/tmp".to_string(),
            resume_handle: None,
            push_capable: false,
            io_kind: Some("tmux".to_string()),
            typed_delivery: true,
        }
    }

    /// A CLI held in tmux outlives its node. The node restarts, adopts the
    /// pane, and registers it as a FRESH agent -- same machine, same pid,
    /// same start ticks. `agents_identity` is UNIQUE on that triple, so the
    /// insert failed, the frame was rejected, and the hub went on believing
    /// the seat was empty. attach then started it again, four times over,
    /// with the CLI sitting right there.
    #[tokio::test]
    async fn the_same_process_may_re_register_under_a_new_id() {
        let st = fleet();
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
        handle(&st, "n1", "machine-b", up("ag-first"), &tx)
            .await
            .expect("first registration");
        // The node restarts and adopts the same pane.
        handle(&st, "n1", "machine-b", up("ag-second"), &tx)
            .await
            .expect("a reattached process must be allowed to register again");

        let db = st.db.lock().unwrap();
        // THE SYMPTOM THAT MATTERED: the seat has a live process the hub
        // knows about, so attach has something to open.
        let live: Vec<(String, Option<i64>)> = db
            .prepare("SELECT id, pid FROM agents WHERE slot_id='sl-1' AND exited_at_ms IS NULL")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(live.len(), 1, "exactly one live agent per process: {live:?}");
        assert_eq!(live[0].0, "ag-second", "the new id is the live one");
        assert_eq!(live[0].1, Some(4242), "and it carries the pid");

        // The earlier row is history, not a second live agent -- and it has
        // given up the pid, because the unique index is partial on pid and
        // would otherwise still conflict.
        let (status, pid): (String, Option<i64>) = db
            .query_row("SELECT status, pid FROM agents WHERE id='ag-first'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "exited");
        assert_eq!(pid, None);
    }

    /// And the ordinary case is untouched: the same agent id coming up again
    /// is an update, not a supersede, and must not retire itself.
    #[tokio::test]
    async fn the_same_agent_coming_up_again_is_still_just_an_update() {
        let st = fleet();
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(8);
        handle(&st, "n1", "machine-b", up("ag-one"), &tx).await.unwrap();
        handle(&st, "n1", "machine-b", up("ag-one"), &tx).await.unwrap();
        let db = st.db.lock().unwrap();
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM agents WHERE slot_id='sl-1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let (status, pid): (String, Option<i64>) = db
            .query_row("SELECT status, pid FROM agents WHERE id='ag-one'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "running");
        assert_eq!(pid, Some(4242), "it must not retire its own pid");
    }
}
