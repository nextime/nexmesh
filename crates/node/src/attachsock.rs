// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Attaching to a terminal on this machine without the hub in the middle.
//!
//! `proto::attachsock` says why and describes the wire. This is the node end:
//! a socket beside the one the shims use, a ticket the hub minted and pushed
//! here, and then the same four operations an attach through the hub performs
//! -- which live here too, so both routes run exactly one implementation and
//! cannot drift apart.

use crate::{ctx::NodeCtx, viewer};
use anyhow::Result;
use nexmesh_proto::{
    self as proto,
    attachsock::{Control, Frame, Open},
    NodeToHub,
};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

/// Open an attach: the terminal, sized, with its scrollback replayed.
pub async fn open(
    ctx: &Arc<NodeCtx>,
    attach_id: String,
    agent_id: String,
    cols: u16,
    rows: u16,
    replay_bytes: usize,
) {

            let Some(io) = ctx.agent_io(&agent_id) else {
                // "not running on this node" was true of what the node has
                // and false of what the operator reads: the agent is very
                // often running perfectly well, in a terminal this node
                // never owned. An attach can only reach a terminal the node
                // holds, and the two cases want different advice -- wait,
                // versus adopt the pane it is sitting in.
                let known = ctx.agent(&agent_id).is_some();
                let reason = if known {
                    format!(
                        "no terminal here for {agent_id}: its CLI was started by hand, \
                         so only its own shim reaches it"
                    )
                } else {
                    format!(
                        "this node is not running {agent_id}: it may have restarted since \
                         that agent started, or the agent belongs to another machine"
                    )
                };
                let _ = ctx
                    .send_hub(&NodeToHub::AttachRejected { attach_id, reason })
                    .await;
                return;
            };
            // An agent with no pty of its own but a server to connect to gets
            // a terminal client started just for this viewer.
            if let (true, Some((bin, args))) = (!io.read_only() && !io.has_pty(),
                                                io.attach_command()) {
                let cwd = ctx
                    .agent(&agent_id)
                    .and_then(|_| std::env::var("PWD").ok())
                    .unwrap_or_else(|| "/".into());
                match viewer::open(ctx.clone(), attach_id.clone(), agent_id.clone(),
                                   bin, args, cwd, cols, rows).await
                {
                    Ok(()) => {
                        io.add_attach(&attach_id);
                        let _ = ctx
                            .send_hub(&NodeToHub::AttachOpened {
                                attach_id,
                                agent_id,
                                read_only: false,
                                cols,
                                rows,
                            })
                            .await;
                        return;
                    }
                    Err(e) => {
                        let _ = ctx
                            .send_hub(&NodeToHub::AttachRejected {
                                attach_id,
                                reason: format!("could not open a terminal: {e:#}"),
                            })
                            .await;
                        return;
                    }
                }
            }

            // Size the pty before replaying, so a TUI redraws at the right
            // dimensions rather than wrapping oddly -- but as THIS viewer's
            // contribution, reconciled with whoever else is watching. An
            // arriving viewer used to resize the pty to its own window,
            // which shrank somebody else's terminal mid-edit.
            io.add_attach(&attach_id);
            io.resize_from(&attach_id, cols, rows);
            let _ = ctx
                .send_hub(&NodeToHub::AttachOpened {
                    attach_id: attach_id.clone(),
                    agent_id: agent_id.clone(),
                    read_only: io.read_only(),
                    cols,
                    rows,
                })
                .await;
            // The modes the program set at STARTUP, which a viewer arriving
            // later never saw: they rolled out of the ring hours ago. Without
            // them the terminal in front of the operator is not in the
            // alternate screen and does not report the mouse, while the
            // program believes it is and does -- which is a TUI whose
            // scrolling has silently stopped working.
            let preamble = io.modes_preamble();
            if !preamble.is_empty() {
                let _ = ctx
                    .send_hub(&NodeToHub::AttachData {
                        attach_id: attach_id.clone(),
                        data: preamble,
                    })
                    .await;
            }
            // Replay scrollback so the viewer does not start at a blank screen.
            let back = io.scrollback(replay_bytes.min(256 * 1024));
            if !back.is_empty() {
                let _ = ctx
                    .send_hub(&NodeToHub::AttachData { attach_id, data: back })
                    .await;
            }
            // And for an adopted tmux pane, ask tmux to paint the screen.
            // The replay alone is not enough there: tmux writes only what
            // changed, and it rewrites its status line every second, so on a
            // pane that has been still the recent scrollback is all status
            // ticks and the screen itself has scrolled out of it. That is the
            // "it attaches but the terminal is black" this fixes.
            if let Some(view) = ctx.agent(&agent_id).and_then(|r| r.tmux_view.clone()) {
                crate::tmux::repaint(&view);
            }
            tracing::info!(%agent_id, read_only = io.read_only(), "viewer attached");
        }

/// Keystrokes from a viewer.
pub async fn input(ctx: &Arc<NodeCtx>, attach_id: String, data: Vec<u8>) {

            // A terminal client opened for this viewer takes the keystrokes.
            if let Some(v) = ctx.viewer(&attach_id) {
                let _ = v.input.send(data).await;
                return;
            }
            // Keystrokes with nowhere to go used to be dropped in silence,
            // which is the worst possible answer: "it does not take input"
            // with nothing on the screen to say why is a long hunt. Every
            // path now reports itself into the terminal the person is
            // looking at.
            let Some(io) = ctx.io_for_attach(&attach_id) else {
                let _ = ctx
                    .send_hub(&NodeToHub::AttachData {
                        attach_id,
                        data: b"\r\n[nexmesh] this attach is no longer wired to a terminal                                 here; detach and open it again\r\n"
                            .to_vec(),
                    })
                    .await;
                return;
            };
            match &io.input {
                Some(tx) => {
                    // Remember what the person is typing BEFORE it goes to the
                    // pty, so an injection arriving a moment later knows not to
                    // land in the middle of their input line.
                    io.note_human_input(&data);
                    if tx.send(data).await.is_err() {
                        let _ = ctx
                            .send_hub(&NodeToHub::AttachData {
                                attach_id,
                                data: b"\r\n[nexmesh] the terminal behind this agent has gone:                                         what you type is going nowhere\r\n"
                                    .to_vec(),
                            })
                            .await;
                    }
                }
                // Typing at a headless agent would go nowhere; say so once
                // rather than silently swallowing the keystrokes.
                None => {
                    let _ = ctx
                        .send_hub(&NodeToHub::AttachData {
                            attach_id,
                            data: b"\r\n[nexmesh] this agent is headless: view-only, input ignored\r\n"
                                .to_vec(),
                        })
                        .await;
                }
            }
        }

/// The viewer's window changed size.
pub fn resize(ctx: &Arc<NodeCtx>, attach_id: &str, cols: u16, rows: u16) {
    let attach_id = attach_id.to_string();

            if let Some(v) = ctx.viewer(&attach_id) {
                v.resize(cols, rows);
            } else if let Some(io) = ctx.io_for_attach(&attach_id) {
                // Per viewer: the pty's size is what every viewer can show,
                // not whichever one spoke last. See `AgentIo::resize_from`.
                io.resize_from(&attach_id, cols, rows);
            }
        }

/// The viewer has gone. The agent keeps working.
pub fn close(ctx: &Arc<NodeCtx>, attach_id: &str) {
    let attach_id = attach_id.to_string();

            // Closing a window onto a server leaves the agent running.
            if let Some(v) = ctx.remove_viewer(&attach_id) {
                v.close();
            }
            if let Some(io) = ctx.io_for_attach(&attach_id) {
                io.remove_attach(&attach_id);
                tracing::info!(%attach_id, "viewer detached");
            }
        }

// ------------------------------------------------------------- listener ----

/// How many frames may be waiting for a direct viewer before the terminal is
/// throttled. A screenful of repaints, not a backlog nobody is reading.
const QUEUE: usize = 256;

pub async fn serve(ctx: Arc<NodeCtx>) -> Result<()> {
    let path = proto::attachsock::socket_path_for(&ctx.sock_path);
    if let Some(dir) = Path::new(&path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Same rule as the shim socket: only this user. A ticket is the hub's
    // decision about WHO may attach, and a socket anybody could open would
    // let anybody spend one they overheard.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    tracing::info!(socket = %path, "listening for direct attaches");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(ctx, stream).await {
                        tracing::debug!(error = %e, "direct attach ended");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "accept failed on the attach socket"),
        }
    }
}

async fn connection(ctx: Arc<NodeCtx>, stream: UnixStream) -> Result<()> {
    let (mut read, mut write) = stream.into_split();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];

    // The first frame must be the ticket. Nothing else is read until it is.
    let opened: Open = loop {
        if let Some((frame, used)) = proto::attachsock::Frame::decode(&buf)
            .map_err(anyhow::Error::msg)?
        {
            buf.drain(..used);
            match frame {
                Frame::Open(o) => break o,
                _ => anyhow::bail!("the first frame must present a ticket"),
            }
        }
        let n = read.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let agent_id = match ctx.spend_ticket(&opened.ticket) {
        Ok(a) => a,
        Err(why) => {
            let f = Frame::Control(Control::Error { message: why.to_string() });
            let _ = write.write_all(&f.encode()).await;
            tracing::warn!(reason = why, "a direct attach was refused");
            return Ok(());
        }
    };
    // A ticket names the agent it was minted for. Presenting it for another
    // one is not a mistake worth being helpful about.
    if agent_id != opened.agent_id {
        let f = Frame::Control(Control::Error {
            message: "that ticket was not minted for this agent".into(),
        });
        let _ = write.write_all(&f.encode()).await;
        return Ok(());
    }

    // From here it is an ordinary attach, with this connection as its sink.
    let attach_id = format!("at-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(QUEUE);
    ctx.add_local_attach(attach_id.clone(), tx);

    let writer = tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            let ends = matches!(f, Frame::Control(Control::Closed { .. }) | Frame::Control(Control::Error { .. }));
            if write.write_all(&f.encode()).await.is_err() || write.flush().await.is_err() {
                break;
            }
            if ends {
                break;
            }
        }
    });

    open(&ctx, attach_id.clone(), agent_id.clone(), opened.cols, opened.rows, opened.replay_bytes)
        .await;

    // Keystrokes and window changes, until the viewer goes.
    let reason = loop {
        while let Some((frame, used)) =
            proto::attachsock::Frame::decode(&buf).map_err(anyhow::Error::msg)?
        {
            buf.drain(..used);
            match frame {
                Frame::Data(d) => input(&ctx, attach_id.clone(), d).await,
                Frame::Control(Control::Resize { cols, rows }) => {
                    resize(&ctx, &attach_id, cols, rows)
                }
                Frame::Control(Control::Close) => break,
                // Anything else from a viewer is the viewer talking nonsense;
                // it is not worth ending a terminal over.
                _ => {}
            }
        }
        match read.read(&mut chunk).await {
            Ok(0) => break "the viewer closed the connection",
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break "the connection was lost",
        }
    };

    close(&ctx, &attach_id);
    ctx.drop_local_attach(&attach_id);
    writer.abort();
    tracing::info!(%agent_id, %attach_id, reason, "direct attach closed");
    Ok(())
}

/// Tell the hub, once, that this node serves direct attaches -- part of the
/// node's own description rather than a thing the hub has to discover.
pub fn advertised_socket(ctx: &NodeCtx) -> String {
    proto::attachsock::socket_path_for(&ctx.sock_path)
}
