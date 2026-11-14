//! Operator attach: a terminal on one machine driving an agent on another.
//!
//! The hub is a relay. It never touches the pty -- only the agent's own node
//! can -- so an attach is three hops: operator WebSocket -> hub -> the node's
//! existing dial-out socket. That is why attaching works for a node behind NAT
//! without any inbound port.
//!
//! On this link, raw terminal bytes travel as binary frames and control
//! messages as JSON text, so keystrokes and screen output need no encoding.

use crate::state::{AppState, AttachFrame};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use nexmesh_proto::HubToNode;
use rusqlite::OptionalExtension;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct AttachQuery {
    /// Agent id. Resolve a name with /v1/sessions/:id/resolve/:who first.
    pub agent: String,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Bytes of scrollback to replay before live output.
    #[serde(default = "default_replay")]
    pub replay: usize,
}

fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    40
}
fn default_replay() -> usize {
    16 * 1024
}

pub async fn attach_ws(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<AttachQuery>,
) -> Response {
    // A browser that authenticated with the subprotocol must be answered with
    // the same one, or it closes the connection the instant it opens --
    // which looked exactly like the hub refusing the attach.
    let chosen = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| {
            raw.split(',')
                .map(|p| p.trim().to_string())
                .find(|p| p.starts_with(crate::auth::WS_TOKEN_PREFIX))
        });
    match chosen {
        Some(p) => ws.protocols([p]).on_upgrade(move |sock| session(sock, st, q)),
        None => ws.on_upgrade(move |sock| session(sock, st, q)),
    }
}

async fn session(socket: WebSocket, st: AppState, q: AttachQuery) {
    let (mut sink, mut stream) = socket.split();

    let fail = |msg: &str| serde_json::json!({ "t": "error", "message": msg }).to_string();

    // Which node is this agent on, and is it still running?
    let found: Option<(String, Option<i64>)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT node_id, exited_at_ms FROM agents WHERE id = ?",
            [&q.agent],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|(n, e): (Option<String>, Option<i64>)| n.map(|n| (n, e)))
    };
    let Some((node_id, exited)) = found else {
        let _ = sink
            .send(Message::Text(fail(
                "no such agent, or it is an external participant with no terminal",
            )))
            .await;
        return;
    };
    if exited.is_some() {
        let _ = sink
            .send(Message::Text(fail(
                "that agent has exited; read its transcript instead",
            )))
            .await;
        return;
    }
    if !st.node_is_connected(&node_id) {
        let _ = sink
            .send(Message::Text(fail("the agent's node is not connected")))
            .await;
        return;
    }

    let attach_id = format!("at-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AttachFrame>(1024);
    st.register_attach(&attach_id, &node_id, tx);

    if let Err(e) = st
        .send_to_node(
            &node_id,
            &HubToNode::AttachOpen {
                attach_id: attach_id.clone(),
                agent_id: q.agent.clone(),
                cols: q.cols,
                rows: q.rows,
                replay_bytes: q.replay,
            },
        )
        .await
    {
        st.drop_attach(&attach_id);
        let _ = sink.send(Message::Text(fail(&e.to_string()))).await;
        return;
    }
    tracing::info!(%attach_id, agent = %q.agent, node = %node_id, "attach opened");

    // Node -> operator.
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let msg = match frame {
                AttachFrame::Data(d) => Message::Binary(d),
                AttachFrame::Control(s) => Message::Text(s),
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Operator -> node.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            // Raw keystrokes.
            Message::Binary(data) => {
                let _ = st
                    .send_to_node(
                        &node_id,
                        &HubToNode::AttachData { attach_id: attach_id.clone(), data },
                    )
                    .await;
            }
            // Control: resize, or an explicit detach.
            Message::Text(t) => {
                let v: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match v["t"].as_str() {
                    Some("resize") => {
                        let cols = v["cols"].as_u64().unwrap_or(120) as u16;
                        let rows = v["rows"].as_u64().unwrap_or(40) as u16;
                        let _ = st
                            .send_to_node(
                                &node_id,
                                &HubToNode::AttachResize {
                                    attach_id: attach_id.clone(),
                                    cols,
                                    rows,
                                },
                            )
                            .await;
                    }
                    Some("close") => break,
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Detaching must never kill the agent: it keeps working unattended.
    st.drop_attach(&attach_id);
    let _ = st
        .send_to_node(&node_id, &HubToNode::AttachClose { attach_id: attach_id.clone() })
        .await;
    writer.abort();
    tracing::info!(%attach_id, "attach closed");
}
