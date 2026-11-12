// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! nexmesh node: the per-machine agent host.
//!
//! This is the component the JS original has no equivalent of, and the reason
//! it cannot leave one host: a driver holds stdio pipes to a child process, and
//! a pipe does not cross a machine boundary. So the thing that spawns and
//! drives an agent must live on the agent's own machine. The node is that
//! thing, and it is the ONLY component permitted to signal a process.
//!
//! It always dials OUT to the hub. The hub never connects to a node, so a node
//! works unchanged on a roaming laptop, behind NAT, or on a public host with no
//! inbound port.

mod cli_detect;
mod ctx;
mod localsock;
mod proc;
mod spawn;

use anyhow::{Context, Result};
use clap::Parser;
use ctx::{JoinOutcome, NodeCtx, ToolOutcome};
use futures_util::{SinkExt, StreamExt};
use nexmesh_proto::{self as proto, AgentHealth, HubToNode, NodeToHub};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "nexmesh-node", version, about = "nexmesh per-machine agent host")]
struct Args {
    #[arg(long, env = "NEXMESH_HUB", default_value = "ws://127.0.0.1:7870")]
    hub: String,
    #[arg(long, env = "NEXMESH_TOKEN")]
    token: String,
    #[arg(long, env = "NEXMESH_NODE_NAME", default_value_t = default_hostname())]
    name: String,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long, env = "NEXMESH_CLIS", default_value = "/etc/nexmesh/clis")]
    clis: String,
    /// Unix socket agent shims connect to.
    #[arg(long, env = "NEXMESH_SOCK", default_value_t = default_sock())]
    sock: String,
    /// Path to the nexmesh-mcpd shim. Defaults to a sibling of this binary.
    #[arg(long, env = "NEXMESH_MCPD")]
    mcpd: Option<String>,
    /// Print the detected CLI inventory and exit.
    #[arg(long)]
    detect_only: bool,
}

fn default_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// `/run` is not writable by a normal user, and a node is usually run as one.
fn default_sock() -> String {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return format!("{rt}/nexmesh/node.sock");
        }
    }
    format!("/tmp/nexmesh-{}/node.sock", current_uid())
}

fn current_uid() -> u32 {
    // Read it from /proc rather than take a libc dependency for one call.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

/// Turn an mcpd path into something an agent can resolve from any directory.
/// A path with a separator is made absolute; a bare name is left alone so the
/// child resolves it on PATH, which does not depend on cwd.
fn absolutize_mcpd(given: &str) -> Result<String> {
    if !given.contains('/') {
        if let Some(found) = cli_detect::resolve(given, &[]) {
            return Ok(found);
        }
        // Leave it for the child's PATH lookup, but say so now.
        tracing::warn!(mcpd = given, "not found on PATH; agents will fail to load the shim");
        return Ok(given.to_string());
    }
    let p = std::path::Path::new(given);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let abs = std::fs::canonicalize(&abs)
        .with_context(|| format!("{} does not exist", abs.display()))?;
    Ok(abs.to_string_lossy().to_string())
}

fn default_mcpd() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nexmesh-mcpd")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "nexmesh-mcpd".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let descriptors = cli_detect::load_descriptors(&args.clis)
        .with_context(|| format!("loading CLI descriptors from {}", args.clis))?;
    let detected = cli_detect::detect(&descriptors).await;

    if args.detect_only {
        println!("node: {}  ({} {})", args.name, std::env::consts::OS, std::env::consts::ARCH);
        println!("descriptors: {}", descriptors.len());
        if detected.is_empty() {
            println!("no agent CLIs found");
        }
        for d in &detected {
            println!(
                "  {:<10} {:<24} {}",
                d.cli_id,
                d.version.as_deref().unwrap_or("?"),
                d.path.as_deref().unwrap_or("")
            );
        }
        return Ok(());
    }

    // The path must be ABSOLUTE: it is written into the agent's MCP config, and
    // the agent runs with cwd set to its workspace, not to the node's cwd. A
    // relative path silently resolves to nothing there.
    let mcpd = absolutize_mcpd(&args.mcpd.clone().unwrap_or_else(default_mcpd))
        .context("resolving the nexmesh-mcpd path")?;
    let ctx = NodeCtx::new(args.name.clone(), descriptors, mcpd.clone(), args.sock.clone());

    tracing::info!(
        node = %args.name, clis = detected.len(), mcpd = %mcpd, sock = %args.sock,
        "starting; detected: {}",
        detected.iter().map(|d| d.cli_id.as_str()).collect::<Vec<_>>().join(", ")
    );

    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = localsock::serve(ctx).await {
                tracing::error!(error = %e, "agent socket died");
            }
        });
    }

    // Reconnect forever with capped backoff: a hub restart or a VPN flap must
    // not need a human to restart nodes.
    let mut backoff = 1u64;
    loop {
        match session(&args, &detected, ctx.clone()).await {
            Ok(()) => {
                tracing::warn!("hub closed the connection");
                backoff = 1;
            }
            Err(e) => tracing::warn!(error = %e, "session ended"),
        }
        *ctx.hub_tx.lock().unwrap() = None;
        ctx.fail_all_waiters();
        tracing::info!(retry_in_s = backoff, "reconnecting");
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn session(args: &Args, detected: &[proto::DetectedCli], ctx: Arc<NodeCtx>) -> Result<()> {
    let url = format!("{}/v1/node/ws", args.hub.trim_end_matches('/'));
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut stream) = ws.split();

    let hello = NodeToHub::Hello {
        protocol: proto::PROTOCOL_VERSION,
        token: args.token.clone(),
        name: args.name.clone(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        node_version: VERSION.to_string(),
        boot_id: boot_id(),
        labels: args.labels.clone(),
        clis: detected.to_vec(),
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?)).await?;

    let heartbeat_ms = match stream.next().await {
        Some(Ok(Message::Text(t))) => match serde_json::from_str::<HubToNode>(&t)? {
            HubToNode::Welcome { node_id, hub_version, heartbeat_ms, .. } => {
                tracing::info!(%node_id, %hub_version, "enrolled");
                *ctx.node_id.lock().unwrap() = node_id;
                heartbeat_ms
            }
            HubToNode::Rejected { reason } => anyhow::bail!("hub rejected this node: {reason}"),
            other => anyhow::bail!("expected welcome, got {other:?}"),
        },
        other => anyhow::bail!("no welcome frame: {other:?}"),
    };

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(512);
    *ctx.hub_tx.lock().unwrap() = Some(out_tx.clone());

    let writer = tokio::spawn(async move {
        while let Some(s) = out_rx.recv().await {
            if sink.send(Message::Text(s)).await.is_err() {
                break;
            }
        }
    });

    // Heartbeat. Agent liveness is verified against /proc here, so the hub
    // never has to guess from a pid it cannot see.
    let hb = {
        let ctx = ctx.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let mut seq = 0u64;
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(heartbeat_ms));
            loop {
                tick.tick().await;
                seq += 1;
                let agents: Vec<AgentHealth> = {
                    let map = ctx.agents.lock().unwrap();
                    map.values()
                        .map(|a| AgentHealth {
                            agent_id: a.agent_id.clone(),
                            alive: proc::is_same_process(a.proc.pid, a.proc.start_ticks),
                            idle_ms: 0,
                        })
                        .collect()
                };
                let frame = NodeToHub::Heartbeat { seq, agents };
                if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                    break;
                }
            }
        })
    };

    while let Some(frame) = stream.next().await {
        let text = match frame? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<HubToNode>(&text) {
            Ok(f) => handle(&ctx, f, &out_tx).await,
            Err(e) => tracing::warn!(error = %e, "bad hub frame"),
        }
    }

    hb.abort();
    writer.abort();
    Ok(())
}

async fn handle(ctx: &Arc<NodeCtx>, frame: HubToNode, out_tx: &tokio::sync::mpsc::Sender<String>) {
    match frame {
        HubToNode::Ping { seq } => {
            if let Ok(s) = serde_json::to_string(&NodeToHub::Pong { seq }) {
                let _ = out_tx.send(s).await;
            }
        }

        HubToNode::Spawn(spec) => {
            let agent_id = spec.agent_id.clone();
            let slot_id = spec.slot_id.clone();
            if let Err(e) = spawn::spawn_agent(ctx.clone(), spec).await {
                tracing::error!(%agent_id, error = %e, "spawn failed");
                // Report it as an immediate exit so the hub stops waiting.
                let _ = ctx
                    .send_hub(&NodeToHub::AgentExited { agent_id, code: None, signal: None })
                    .await;
                let _ = slot_id;
            }
        }

        HubToNode::Kill { agent_id, proc: id, grace_ms } => {
            // Identity-checked, and only ever for a pid on THIS machine.
            if id.node_id != ctx.node_name {
                tracing::error!(%agent_id, names = %id.node_id,
                    "refusing a kill that names another node");
                return;
            }
            tracing::info!(%agent_id, pid = id.pid, grace_ms, "kill requested");
            match proc::signal_if_same(id.pid, id.start_ticks, nix::sys::signal::Signal::SIGTERM) {
                Ok(true) => {}
                Ok(false) => tracing::warn!(%agent_id, "not signalled: identity mismatch"),
                Err(e) => tracing::warn!(%agent_id, error = %e, "signal failed"),
            }
        }

        HubToNode::ToolResult { call_id, result, deliver, .. } => {
            ctx.complete_tool(&call_id, ToolOutcome { result, deliver });
        }

        HubToNode::JoinGranted { req_id, slot_id, session_id, agent_id, display_name, role } => {
            ctx.complete_join(
                req_id,
                JoinOutcome { granted: Ok((slot_id, session_id, agent_id, display_name, role)) },
            );
        }

        HubToNode::JoinDenied { req_id, reason } => {
            ctx.complete_join(req_id, JoinOutcome { granted: Err(reason) });
        }

        // Milestone 4 (drivers) and milestone 3 (pty attach).
        HubToNode::Inject { .. }
        | HubToNode::Pending { .. }
        | HubToNode::AttachOpen { .. }
        | HubToNode::AttachData { .. }
        | HubToNode::AttachResize { .. }
        | HubToNode::AttachClose { .. } => {}

        HubToNode::Welcome { .. } | HubToNode::Rejected { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::absolutize_mcpd;

    #[test]
    fn a_relative_path_becomes_absolute() {
        // The bug this prevents: the node writes "./target/debug/nexmesh-mcpd"
        // into the config, the agent runs in its workspace, and the shim is
        // never found -- with the only clue on a stderr nobody reads.
        let dir = std::env::temp_dir().join(format!("nexmesh-mcpd-t-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("nexmesh-mcpd");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let got = absolutize_mcpd("./nexmesh-mcpd").unwrap();
        std::env::set_current_dir(prev).unwrap();
        assert!(got.starts_with('/'), "got {got}");
        assert!(got.ends_with("nexmesh-mcpd"), "got {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_path_fails_loudly_at_startup() {
        let err = absolutize_mcpd("/nonexistent/dir/nexmesh-mcpd").unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_bare_name_is_resolved_on_path_or_left_for_the_child() {
        // `sh` stands in for an installed shim.
        assert!(absolutize_mcpd("sh").unwrap().starts_with('/'));
        assert_eq!(absolutize_mcpd("nexmesh-definitely-absent").unwrap(), "nexmesh-definitely-absent");
    }
}

/// Distinguishes "the node restarted" from "the machine rebooted" -- after a
/// reboot every pid the hub remembers is meaningless, start_ticks included.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
