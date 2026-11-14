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
//! works unchanged on a roaming machine, behind NAT, or on a public host with no
//! inbound port.

mod cli_detect;
mod codegraph;
mod codex;
mod ctx;
mod driver;
mod gitsync;
mod localsock;
mod opencode;
mod plugin;
mod proc;
mod viewer;
mod spawn;
mod termlink;
mod tlstrust;
mod tmux;

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
    token: Option<String>,
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
    /// Directory holding a paired identity. Anything not passed explicitly is
    /// taken from here, so after `nexmesh-node pair` there is nothing to wire
    /// up by hand.
    #[arg(long, env = "NEXMESH_IDENTITY", default_value = nexmesh_proto::identity::DEFAULT_DIR)]
    identity_dir: String,
    /// Print the detected CLI inventory and exit.
    #[arg(long)]
    detect_only: bool,
    #[command(subcommand)]
    cmd: Option<NodeCmd>,
    /// PEM file with the hub's certificate (or its CA), for a self-signed hub.
    #[arg(long, env = "NEXMESH_CA_CERT")]
    ca_cert: Option<String>,
    /// This node's client certificate, when the hub requires mutual TLS.
    /// Issue one with `nexmesh-hub pki issue <node>`.
    #[arg(long, env = "NEXMESH_CLIENT_CERT", requires = "client_key")]
    client_cert: Option<String>,
    /// Private key for --client-cert.
    #[arg(long, env = "NEXMESH_CLIENT_KEY", requires = "client_cert")]
    client_key: Option<String>,
    /// Allow sending the enrolment token over plaintext ws:// to a remote hub.
    /// Only reasonable when the path is already encrypted, e.g. a WireGuard or
    /// OpenVPN tunnel.
    #[arg(long, env = "NEXMESH_INSECURE")]
    insecure: bool,
}

/// Refuse to leak the enrolment token onto an unencrypted network path.
///
/// The token authenticates this node and is sent in the first frame. Over
/// plaintext `ws://` to a remote host, anyone on the path can read it and
/// replay it -- and a node may spawn processes, so that token is worth a shell.
/// `wss://` is fine, loopback is fine, and a VPN is fine but we cannot detect
/// one, so that case needs an explicit `--insecure`.
fn check_transport(hub: &str, insecure: bool) -> Result<()> {
    let lower = hub.to_ascii_lowercase();
    if lower.starts_with("wss://") || lower.starts_with("https://") {
        return Ok(());
    }
    let host = lower
        .split("://")
        .nth(1)
        .unwrap_or(&lower)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| lower.split("://").nth(1).unwrap_or("").to_string());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host.starts_with("127.")
        || host.ends_with(".localhost");
    if loopback || insecure {
        if !loopback {
            tracing::warn!(
                hub,
                "sending the enrolment token over PLAINTEXT ws:// because --insecure was given; \
                 only do this inside a VPN or other encrypted tunnel"
            );
        }
        return Ok(());
    }
    anyhow::bail!(
        "refusing to send the enrolment token in cleartext to '{host}'.\n\
         The token authenticates this node and can spawn processes, so it must not \
         cross an unencrypted network.\n\
         Either use wss:// (terminate TLS in front of the hub), or pass --insecure \
         if the path is already encrypted, e.g. a WireGuard/OpenVPN tunnel."
    )
}

#[derive(clap::Subcommand)]
enum NodeCmd {
    /// Join a fleet using a one-time code from the dashboard. Generates this
    /// machine's key locally -- it never leaves -- and stores the certificate
    /// and token it gets back.
    Pair {
        /// The hub's pairing address, e.g. https://machine-a:7871
        #[arg(long)]
        hub: String,
        /// The code shown in the dashboard.
        #[arg(long)]
        code: String,
        /// The address this node will use afterwards, e.g. wss://machine-a:7870
        #[arg(long)]
        connect: Option<String>,
        /// Trust this PEM when contacting the pairing endpoint.
        #[arg(long)]
        ca_cert: Option<String>,
        /// Accept the hub's certificate without verifying it, for a first
        /// contact you have decided to trust.
        #[arg(long)]
        insecure: bool,
    },
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
    let mut args = Args::parse();

    if let Some(NodeCmd::Pair { hub, code, connect, ca_cert, insecure }) = &args.cmd {
        let p = nexmesh_proto::pair_client::pair(
            hub,
            code,
            &args.identity_dir,
            connect.as_deref(),
            ca_cert.as_deref(),
            *insecure,
        )
        .await?;
        println!("Paired as {} '{}'.", p.kind, p.name);
        println!("Identity written to {}", p.identity_file.display());
        println!("\nStart this node with:");
        match connect {
            Some(c) => println!("  nexmesh-node --hub {c}"),
            None => println!("  nexmesh-node --hub wss://<hub>:7870"),
        }
        println!("(the certificate, key and token are read from the identity directory)");
        return Ok(());
    }

    // Fill anything not given explicitly from the paired identity.
    let identity = nexmesh_proto::identity::load(&args.identity_dir);
    if let Some(id) = &identity {
        if args.token.is_none() {
            args.token = Some(id.identity.token.clone());
        }
        if args.ca_cert.is_none() {
            args.ca_cert = Some(id.ca_cert_path());
        }
        if args.client_cert.is_none() && args.client_key.is_none() {
            args.client_cert = Some(id.cert_path());
            args.client_key = Some(id.key_path());
        }
        if std::env::var("NEXMESH_NODE_NAME").is_err()
            && args.name == default_hostname()
            && id.identity.kind == "node"
        {
            // The certificate asserts this name; disagreeing with it would
            // only produce a confusing rejection later.
            args.name = id.identity.name.clone();
        }
        if let (Some(h), true) = (&id.identity.hub, args.hub == "ws://127.0.0.1:7870") {
            args.hub = h.clone();
        }
        tracing::info!(name = %args.name, dir = %args.identity_dir, "using a paired identity");
    }
    if args.token.is_none() && !args.detect_only {
        anyhow::bail!(
            "no enrolment token: pass --token, or pair this machine with \
             `nexmesh-node pair --hub https://<hub>:7871 --code <code>`"
        );
    }

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
    check_transport(&args.hub, args.insecure)?;

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
            // `{:#}` prints the whole anyhow chain. With plain Display a TLS
            // trust failure read as "connecting to wss://..." with no cause,
            // which is the least useful possible message.
            Err(e) => tracing::warn!(error = format!("{e:#}"), "session ended"),
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
    let (ws, _) = if url.starts_with("wss://") {
        let client = match (&args.client_cert, &args.client_key) {
            (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
            _ => None,
        };
        let tls = tlstrust::client_config(args.ca_cert.as_deref(), client)?;
        tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        )
        .await
        .with_context(|| format!("connecting to {url}"))?
    } else {
        tokio_tungstenite::connect_async(&url)
            .await
            .with_context(|| format!("connecting to {url}"))?
    };
    let (mut sink, mut stream) = ws.split();

    // What this node can still vouch for. On a hub restart the processes are
    // all still here and this list keeps their seats; on a node restart it is
    // empty, which is exactly what tells the hub those seats are free. A pid
    // alone would not do: it is checked together with its start time, so a
    // reused pid cannot pass for the agent that used to own it.
    let live_agents: Vec<String> = {
        let map = ctx.agents.lock().unwrap();
        map.values()
            .filter(|a| proc::is_same_process(a.proc.pid, a.proc.start_ticks))
            .map(|a| a.agent_id.clone())
            .collect()
    };
    let hello = NodeToHub::Hello {
        protocol: proto::PROTOCOL_VERSION,
        token: args.token.clone().unwrap_or_default(),
        name: args.name.clone(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        node_version: VERSION.to_string(),
        boot_id: boot_id(),
        labels: args.labels.clone(),
        clis: detected.to_vec(),
        live_agents: Some(live_agents),
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

        HubToNode::CloseAgent { agent_id, proc: id, grace_ms } => {
            if id.node_id != ctx.node_name {
                tracing::error!(%agent_id, "close names another node; ignoring");
                return;
            }
            // Closing the input is the polite signal: a stdio-driven CLI sees
            // EOF and shuts down cleanly, finishing whatever it was writing.
            ctx.close_agent_input(&agent_id);
            tracing::info!(%agent_id, grace_ms, "released; closed its input");
            let _ctx = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(grace_ms)).await;
                // Still there after the grace period: it is not going to leave
                // on its own.
                if proc::is_same_process(id.pid, id.start_ticks) {
                    tracing::info!(%agent_id, "still running after the grace period; terminating");
                    let _ = proc::signal_if_same(
                        id.pid, id.start_ticks, nix::sys::signal::Signal::SIGTERM);
                }
            });
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

        HubToNode::AttachOpen { attach_id, agent_id, cols, rows, replay_bytes } => {
            let Some(io) = ctx.agent_io(&agent_id) else {
                let _ = ctx
                    .send_hub(&NodeToHub::AttachRejected {
                        attach_id,
                        reason: format!("agent {agent_id} is not running on this node"),
                    })
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

            // Size the pty to the viewer's terminal before replaying, so a TUI
            // redraws at the right dimensions rather than wrapping oddly.
            io.resize(cols, rows);
            io.add_attach(&attach_id);
            let _ = ctx
                .send_hub(&NodeToHub::AttachOpened {
                    attach_id: attach_id.clone(),
                    agent_id: agent_id.clone(),
                    read_only: io.read_only(),
                    cols,
                    rows,
                })
                .await;
            // Replay scrollback so the viewer does not start at a blank screen.
            let back = io.scrollback(replay_bytes.min(256 * 1024));
            if !back.is_empty() {
                let _ = ctx
                    .send_hub(&NodeToHub::AttachData { attach_id, data: back })
                    .await;
            }
            tracing::info!(%agent_id, read_only = io.read_only(), "viewer attached");
        }

        HubToNode::AttachData { attach_id, data } => {
            // A terminal client opened for this viewer takes the keystrokes.
            if let Some(v) = ctx.viewer(&attach_id) {
                let _ = v.input.send(data).await;
                return;
            }
            let Some(io) = ctx.io_for_attach(&attach_id) else { return };
            match &io.input {
                Some(tx) => {
                    let _ = tx.send(data).await;
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

        HubToNode::BuildCodeGraph { req_id, workspace_id, path } => {
            // Parsing a large tree is CPU work; off the session loop so one
            // scan does not stall this machine's spawns and attach traffic.
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let root = std::path::PathBuf::from(&path);
                let mut graph = tokio::task::spawn_blocking(move || {
                    codegraph::scan(&root, &codegraph::Limits::default())
                })
                .await
                .unwrap_or_default();
                tracing::info!(%workspace_id, files = graph.files, nodes = graph.nodes.len(),
                    "parsed a workspace");
                // In pieces, because the whole thing in one message is
                // megabytes and the far end resets the connection rather
                // than accept it -- which loses the answer completely and
                // leaves the caller waiting for something that no longer
                // exists. Roughly two megabytes a piece.
                const PER_PIECE: usize = 8000;
                let nodes = std::mem::take(&mut graph.nodes);
                let edges = std::mem::take(&mut graph.edges);
                let pieces = std::cmp::max(
                    1,
                    std::cmp::max(nodes.len().div_ceil(PER_PIECE),
                                  edges.len().div_ceil(PER_PIECE)),
                );
                let mut nodes = nodes.into_iter();
                let mut edges = edges.into_iter();
                for seq in 0..pieces {
                    let last = seq + 1 == pieces;
                    let mut part = proto::codegraph::CodeGraph {
                        nodes: nodes.by_ref().take(PER_PIECE).collect(),
                        edges: edges.by_ref().take(PER_PIECE).collect(),
                        ..Default::default()
                    };
                    // The counts and the truncation notice describe the
                    // whole scan, so they ride on the piece that completes
                    // it -- there is nothing to describe until then.
                    if last {
                        part.files = graph.files;
                        part.skipped = std::mem::take(&mut graph.skipped);
                        part.truncated = graph.truncated;
                        part.truncated_reason = graph.truncated_reason.take();
                    }
                    let _ = ctx
                        .send_hub(&NodeToHub::CodeGraphResult {
                            req_id,
                            workspace_id: workspace_id.clone(),
                            graph: part,
                            seq: seq as u32,
                            last,
                        })
                        .await;
                }
            });
        }

        HubToNode::SyncWorkspace {
            req_id, workspace_id, path, repo_url, branch, op, author, message,
        } => {
            // git on a large repository can take a while, and this loop also
            // carries spawns, injections and attach traffic for the machine.
            // Run it to one side so one slow fetch does not stall everything.
            let ctx = ctx.clone();
            tokio::spawn(async move {
            use nexmesh_proto::SyncOp;
            let r = match op {
                SyncOp::Ensure => gitsync::ensure(&path, &repo_url, &branch).await,
                SyncOp::Refresh => gitsync::refresh(&path, &branch).await,
                SyncOp::Publish => gitsync::publish(&path, &branch, &author, &message).await,
                SyncOp::Status => gitsync::status(&path, &branch).await,
            };
            tracing::info!(%workspace_id, ?op, ok = r.ok, conflict = r.conflict, "{}", r.detail);
            let _ = ctx
                .send_hub(&NodeToHub::SyncResult {
                    req_id,
                    workspace_id,
                    ok: r.ok,
                    conflict: r.conflict,
                    head: r.head,
                    detail: r.detail,
                    changed: r.changed,
                })
                .await;
            });
        }

        HubToNode::AttachResize { attach_id, cols, rows } => {
            if let Some(v) = ctx.viewer(&attach_id) {
                v.resize(cols, rows);
            } else if let Some(io) = ctx.io_for_attach(&attach_id) {
                io.resize(cols, rows);
            }
        }

        HubToNode::AttachClose { attach_id } => {
            // Closing a window onto a server leaves the agent running.
            if let Some(v) = ctx.remove_viewer(&attach_id) {
                v.close();
            }
            if let Some(io) = ctx.io_for_attach(&attach_id) {
                io.remove_attach(&attach_id);
                tracing::info!(%attach_id, "viewer detached");
            }
        }

        HubToNode::Inject { agent_id, messages, expect_cli } => {
            // A typed delivery into an adopted terminal is allowed only on
            // the node's own evidence, re-gathered now. The hub's record was
            // true when it was made; by the time a message arrives the CLI
            // may have exited and left a shell in that pane, where typing
            // this would EXECUTE it.
            if let Some(cli) = &expect_cli {
                if let Err(why) = ctx.terminal_still_holds(&agent_id, cli) {
                    tracing::debug!(%agent_id, reason = %why, "not typing; requeueing");
                    let _ = ctx
                        .send_hub(&NodeToHub::InjectionsLost {
                            agent_id,
                            message_ids: messages.iter().map(|m| m.id).collect(),
                        })
                        .await;
                    return;
                }
            }
            let Some(io) = ctx.agent_io(&agent_id) else {
                tracing::warn!(%agent_id, "inject for an agent this node does not have");
                let _ = ctx
                    .send_hub(&NodeToHub::InjectionsLost {
                        agent_id,
                        message_ids: messages.iter().map(|m| m.id).collect(),
                    })
                    .await;
                return;
            };
            match driver::inject(&io, &messages).await {
                Err(e) => {
                    // Requeueing is the whole of the recovery here: a
                    // terminal that was mid-dialog is typeable a moment
                    // later, and until then the MCP floor is still there.
                    tracing::debug!(%agent_id, reason = %e, "not injecting; requeueing");
                    let _ = ctx
                        .send_hub(&NodeToHub::InjectionsLost {
                            agent_id,
                            message_ids: messages.iter().map(|m| m.id).collect(),
                        })
                        .await;
                }
                // Typed at a terminal: the write is the only confirmation
                // there is, so say so now rather than leave the batch in
                // flight for the stale sweep to deliver all over again.
                Ok(acked) if !acked.is_empty() => {
                    tracing::info!(%agent_id, count = acked.len(), "typed at the agent's terminal");
                    let _ = ctx
                        .send_hub(&NodeToHub::MessageAcked { agent_id, message_ids: acked })
                        .await;
                }
                Ok(_) => {
                    tracing::info!(%agent_id, count = messages.len(), "injected mid-turn");
                }
            }
        }

        HubToNode::Pending { .. } => {}

        HubToNode::Welcome { .. } | HubToNode::Rejected { .. } => {}
    }
}

/// Distinguishes "the node restarted" from "the machine rebooted" -- after a
/// reboot every pid the hub remembers is meaningless, start_ticks included.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{absolutize_mcpd, check_transport};

    #[test]
    fn plaintext_to_a_remote_host_is_refused() {
        let err = check_transport("ws://machine-a:7870", false).unwrap_err();
        assert!(err.to_string().contains("cleartext"), "{err}");
        let err = check_transport("ws://10.0.0.5:7870", false).unwrap_err();
        assert!(err.to_string().contains("cleartext"), "{err}");
        // A public host is the case that actually matters.
        assert!(check_transport("ws://hub.example.com/", false).is_err());
    }

    #[test]
    fn loopback_plaintext_is_fine() {
        for u in ["ws://127.0.0.1:7870", "ws://localhost:7870", "ws://[::1]:7870",
                  "ws://127.0.0.53:7870", "ws://hub.localhost:7870"] {
            assert!(check_transport(u, false).is_ok(), "{u} should be allowed");
        }
    }

    #[test]
    fn tls_is_always_fine() {
        assert!(check_transport("wss://hub.example.com", false).is_ok());
        assert!(check_transport("WSS://HUB.EXAMPLE.COM", false).is_ok());
    }

    #[test]
    fn insecure_is_an_explicit_opt_in_for_a_vpn_path() {
        assert!(check_transport("ws://machine-a:7870", true).is_ok());
    }

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
