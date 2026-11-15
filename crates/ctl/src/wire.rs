// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! How an attach's bytes travel, with the viewer not having to care.
//!
//! Two routes. Through the hub, over the same websocket everything else uses:
//! works from anywhere, including from outside the network the fleet is on.
//! Or straight to the node that holds the terminal, which is what you want
//! whenever you can reach it -- attaching from machine-b to an agent on machine-b had
//! every keystroke crossing the network twice and waiting on a hub on a
//! Celeron in between.
//!
//! Which one is a matter of what works: the direct route is tried, and the hub
//! is what happens when it does not. Both arrive here as the same pair of
//! channels, so the viewer loop is written once.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// What the viewer says.
#[derive(Debug)]
pub enum Out {
    /// Keystrokes.
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// What comes back.
#[derive(Debug)]
pub enum In {
    /// Terminal bytes.
    Data(Vec<u8>),
    /// The attach is live. `read_only` is a headless agent: watch, not type.
    Opened { read_only: bool },
    /// It is over, and why.
    Ended(String),
}

pub struct Wire {
    pub tx: mpsc::Sender<Out>,
    pub rx: mpsc::Receiver<In>,
    /// Whether the bytes are going straight to the node. Said out loud in the
    /// viewer's bar, because "why is this slow" is a question worth being able
    /// to answer by looking.
    pub direct: bool,
}

/// Straight to the node on this machine, with a ticket the hub minted.
pub async fn open_local(
    socket: &str,
    ticket: &str,
    agent: &str,
    cols: u16,
    rows: u16,
    replay: usize,
) -> Result<Wire> {
    use nexmesh_proto::attachsock::{Frame, Open};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {socket}"))?;
    let (mut read, mut write) = stream.into_split();
    let hello = Frame::Open(Open {
        ticket: ticket.to_string(),
        agent_id: agent.to_string(),
        cols,
        rows,
        replay_bytes: replay,
    })
    .encode();
    write.write_all(&hello).await.context("presenting the ticket")?;
    write.flush().await?;

    let (in_tx, rx) = mpsc::channel::<In>(1024);
    let (tx, mut out_rx) = mpsc::channel::<Out>(256);

    tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut chunk = [0u8; 16384];
        loop {
            // Whole frames first, then more bytes: a pty stream arrives in
            // pieces that have nothing to do with frame boundaries.
            loop {
                match Frame::decode(&buf) {
                    Ok(Some((frame, used))) => {
                        buf.drain(..used);
                        let msg = match frame {
                            Frame::Data(d) => In::Data(d),
                            Frame::Control(c) => match c {
                                nexmesh_proto::attachsock::Control::Opened { read_only, .. } => {
                                    In::Opened { read_only }
                                }
                                nexmesh_proto::attachsock::Control::Error { message } => {
                                    In::Ended(message)
                                }
                                nexmesh_proto::attachsock::Control::Closed { reason } => {
                                    In::Ended(reason)
                                }
                                // The node does not send the viewer's own
                                // control frames back.
                                _ => continue,
                            },
                            Frame::Open(_) => continue,
                        };
                        if in_tx.send(msg).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = in_tx.send(In::Ended(e)).await;
                        return;
                    }
                }
            }
            match read.read(&mut chunk).await {
                Ok(0) | Err(_) => {
                    let _ = in_tx.send(In::Ended("connection closed".into())).await;
                    return;
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    });

    tokio::spawn(async move {
        use nexmesh_proto::attachsock::Control;
        while let Some(o) = out_rx.recv().await {
            let frame = match o {
                Out::Data(d) => Frame::Data(d),
                Out::Resize { cols, rows } => Frame::Control(Control::Resize { cols, rows }),
                Out::Close => Frame::Control(Control::Close),
            };
            if write.write_all(&frame.encode()).await.is_err() || write.flush().await.is_err() {
                return;
            }
        }
    });

    Ok(Wire { tx, rx, direct: true })
}

/// Through the hub, which can be reached from anywhere.
pub async fn open_hub(
    url: &str,
    token: Option<String>,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
    name: &str,
) -> Result<Wire> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};

    let mut req = url.into_client_request().context("building the attach request")?;
    // The upgrade needs the operator token too. It goes in a header, never the
    // query string: URLs land in proxy logs and shell history.
    if let Some(t) = token {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {t}").parse().context("building the auth header")?,
        );
    }
    let (ws, _) = if url.starts_with("wss://") {
        let tls = crate::tlstrust::client_config(ca_cert, identity)?;
        tokio_tungstenite::connect_async_tls_with_config(
            req,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        )
        .await
        .with_context(|| format!("attaching to {name}"))?
    } else {
        tokio_tungstenite::connect_async(req)
            .await
            .with_context(|| format!("attaching to {name}"))?
    };
    let (mut sink, mut stream) = ws.split();
    let (in_tx, rx) = mpsc::channel::<In>(1024);
    let (tx, mut out_rx) = mpsc::channel::<Out>(256);

    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let send = match msg {
                WsMessage::Binary(data) => In::Data(data),
                WsMessage::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    match v["t"].as_str() {
                        Some("opened") => {
                            In::Opened { read_only: v["read_only"].as_bool() == Some(true) }
                        }
                        Some("closed") | Some("error") => In::Ended(
                            v["reason"]
                                .as_str()
                                .or(v["message"].as_str())
                                .unwrap_or("session ended")
                                .to_string(),
                        ),
                        _ => continue,
                    }
                }
                WsMessage::Close(_) => break,
                _ => continue,
            };
            if in_tx.send(send).await.is_err() {
                return;
            }
        }
        let _ = in_tx.send(In::Ended("connection closed".into())).await;
    });

    tokio::spawn(async move {
        while let Some(o) = out_rx.recv().await {
            let last = matches!(o, Out::Close);
            let msg = match o {
                Out::Data(d) => WsMessage::Binary(d),
                Out::Resize { cols, rows } => {
                    WsMessage::Text(json!({ "t": "resize", "cols": cols, "rows": rows }).to_string())
                }
                Out::Close => WsMessage::Text(json!({ "t": "close" }).to_string()),
            };
            if sink.send(msg).await.is_err() {
                return;
            }
            if last {
                let _ = sink.close().await;
                return;
            }
        }
    });

    Ok(Wire { tx, rx, direct: false })
}

/// Whether a direct attach is even worth trying: the agent is on this machine
/// and this machine's node is listening for one.
///
/// Deliberately a question about THIS host only. Reaching another machine's
/// node directly is the same ticket and the same framing over TLS, and wants
/// the node to advertise an address the hub can pass on -- which is the next
/// piece of this, not a thing to guess at from a hostname.
pub fn local_socket_for(host: &str, this_node: &str) -> Option<String> {
    if host.is_empty() || host != this_node {
        return None;
    }
    let shim = std::env::var("NEXMESH_SOCK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/nexmesh/node.env").ok().and_then(|t| {
                t.lines()
                    .find_map(|l| l.trim().strip_prefix("NEXMESH_SOCK=").map(|v| v.trim().trim_matches('"').to_string()))
            })
        })
        .unwrap_or_else(|| "/run/nexmesh/node.sock".to_string());
    let path = nexmesh_proto::attachsock::socket_path_for(&shim);
    // A socket that is not there is an older node, or none: say no rather
    // than fail a connection the operator did not ask for.
    std::fs::metadata(&path).ok().map(|_| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_attach_is_only_offered_for_this_machine() {
        assert_eq!(local_socket_for("machine-c", "machine-b"), None);
        assert_eq!(local_socket_for("", "machine-b"), None);
    }

    #[test]
    fn and_only_when_the_node_here_is_listening_for_one() {
        // No socket on this test machine at the default path means no offer,
        // which is how an older node is handled: the hub route still works.
        let answer = local_socket_for("samehost", "samehost");
        if let Some(p) = answer {
            assert!(p.ends_with("attach.sock"), "{p}");
        }
    }
}
