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

mod attachsock;
mod putfile;
mod cli_detect;
mod codegraph;
mod context;
mod codex;
mod ctx;
mod driver;
mod gitsync;
mod localsock;
mod loopguard;
mod opencode;
mod plugin;
mod prompts;
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

/// A log sink that never reports an I/O error.
///
/// The node is built `panic = "abort"`, and its stdout and stderr are the one
/// logfile a service writes to (`… >> node.log 2>&1`). The fmt layer, when a
/// write to its writer fails, falls back to `eprintln!` to say so -- and that
/// write lands on the same failing file, where `_eprint` panics ("failed
/// printing to stderr"). So a filesystem filling under the logfile would abort
/// the whole node, and every pty seat on the host with it -- which is exactly
/// how workstation went down when one seat's model download filled root. Dropping the
/// line is the right thing to lose when the disk is full; the node keeps running.
struct NeverErr(std::io::Stdout);

impl std::io::Write for NeverErr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut self.0, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut self.0);
        Ok(())
    }
}

struct SafeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SafeWriter {
    type Writer = NeverErr;
    fn make_writer(&'a self) -> Self::Writer {
        NeverErr(std::io::stdout())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(SafeWriter)
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

    // And the socket an operator on this machine attaches through, so a
    // keystroke typed on this host never leaves it. The hub still decides who
    // may: nothing is served here without a ticket it minted.
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = attachsock::serve(ctx).await {
                tracing::error!(error = %e, "attach socket died");
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
            // How full each agent's context is, and whether it is going
            // round in circles. Both come from following the same file, so
            // one watcher answers both and reads it once. It belongs to this
            // task rather than to `NodeCtx`: it is a cache keyed by agent
            // id, and a cache in shared state is one that outlives the
            // agents and leaks between tests. The mutex is only so the
            // blocking read can borrow it -- nothing else touches it.
            let watcher = std::sync::Arc::new(std::sync::Mutex::new(context::Watcher::default()));
            // Not on every heartbeat. A heartbeat is seconds; a conversation
            // changes on the scale of a turn, and the first look at a seat
            // reads megabytes.
            let mut look = tokio::time::interval(context_every());
            look.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        seq += 1;
                        // Read the agent table, LET IT GO, then read each one's
                        // I/O: holding one lock while taking another is how
                        // two tasks that take them in the other order hang.
                        let live: Vec<(String, bool)> = {
                            let map = ctx.agents.lock().unwrap();
                            map.values()
                                .map(|a| (
                                    a.agent_id.clone(),
                                    proc::is_same_process(a.proc.pid, a.proc.start_ticks),
                                ))
                                .collect()
                        };
                        ctx.prune_shims(&live.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>());
                        let agents: Vec<AgentHealth> = live
                            .into_iter()
                            .map(|(agent_id, alive)| {
                                let io = ctx.agent_io(&agent_id);
                                AgentHealth {
                                    idle_ms: io.as_ref().map(|io| io.idle_ms()).unwrap_or(0),
                                    activity: io.as_ref().and_then(|io| io.activity().map(|(s, _)| s)),
                                    mcp_link: if alive { ctx.mcp_link(&agent_id) } else { None },
                                    agent_id,
                                    alive,
                                }
                            })
                            .collect();
                        let frame = NodeToHub::Heartbeat { seq, agents };
                        if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                            break;
                        }
                    }
                    _ = look.tick() => {
                        // Off the async thread: this reads files, and a
                        // blocking read in the heartbeat task is a node that
                        // stops answering while a 450MB transcript is
                        // opened for the first time.
                        let c = ctx.clone();
                        let w = watcher.clone();
                        let found = tokio::task::spawn_blocking(move || {
                            w.lock().unwrap().poll(&c)
                        })
                        .await;
                        let Ok(found) = found else { continue };
                        for (agent_id, a) in found.reads {
                            let frame = NodeToHub::FileReads {
                                agent_id,
                                n: a.n,
                                paths: a.paths,
                                edited: a.edited,
                            };
                            if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                                break;
                            }
                        }
                        for (agent_id, block) in found.blocks {
                            tracing::info!(agent = %agent_id, blocked = block.is_some(),
                                kind = block.as_ref().map(|b| b.kind.as_str()).unwrap_or("-"),
                                "model access changed");
                            let frame = NodeToHub::UsageLimited { agent_id, block };
                            if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                                break;
                            }
                        }
                        for (agent_id, r) in found.usage {
                            tracing::debug!(agent = %agent_id, used = r.used, limit = r.limit,
                                "context");
                            let frame = NodeToHub::ContextUsage {
                                agent_id,
                                used: r.used,
                                limit: r.limit,
                                source: r.source,
                            };
                            if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                                break;
                            }
                        }
                        // WHICH CONVERSATION a seat is really in. Reported
                        // through the same frame a fresh conversation uses,
                        // so the hub's one handler does all three things it
                        // already does with a handle: the seat's record, the
                        // per-machine binding, and the durable one kept
                        // against the seat's identity.
                        for (agent_id, slot_id, handle) in found.conversations {
                            let frame = NodeToHub::ResumeHandle {
                                agent_id: agent_id.clone(),
                                slot_id,
                                handle: handle.clone(),
                            };
                            if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                                break;
                            }
                            // And locally, so the next pass agrees with the
                            // hub and says nothing further.
                            ctx.note_conversation(&agent_id, &handle);
                        }
                        for (agent_id, c) in found.loops {
                            tracing::info!(agent = %agent_id, period = c.period,
                                repeats = c.repeats,
                                "this agent has repeated the same turns back to back");
                            let frame = NodeToHub::LoopSuspected {
                                agent_id,
                                period: c.period as i64,
                                repeats: c.repeats as i64,
                            };
                            if out_tx.send(serde_json::to_string(&frame).unwrap()).await.is_err() {
                                break;
                            }
                        }
                    }
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

/// How often each agent's conversation is looked at.
///
/// Not the heartbeat. A heartbeat is about liveness and is measured in
/// seconds; a conversation grows by a turn at a time and the first look at a
/// seat reads megabytes off disk. Twenty seconds is far finer than any
/// threshold worth setting -- a turn cannot take a seat from under a limit
/// to over it and back in that time -- and it is cheap, because after the
/// first look only what has been appended is read.
const CONTEXT_EVERY: std::time::Duration = std::time::Duration::from_secs(20);

/// The same, overridable -- for tests, which otherwise spend twenty seconds
/// waiting for each assertion about a conversation that was written in a
/// millisecond. A knob rather than a shorter default: twenty seconds is the
/// right cadence on a real fleet, and a suite that needs a faster one should
/// say so rather than making every node on the fleet poll harder.
fn context_every() -> std::time::Duration {
    match std::env::var("NEXMESH_CONTEXT_EVERY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(ms) if ms >= 100 => std::time::Duration::from_millis(ms),
        _ => CONTEXT_EVERY,
    }
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
                // It never registered, so nothing it wanted to say about how
                // it was started has anywhere to go.
                ctx.forget_notes(&agent_id);
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

        HubToNode::JoinGranted {
            req_id, slot_id, session_id, agent_id, display_name, role, authority,
        } => {
            ctx.complete_join(
                req_id,
                JoinOutcome {
                    granted: Ok(ctx::Granted {
                        slot_id, session_id, agent_id, display_name, role, authority,
                    }),
                },
            );
        }

        HubToNode::JoinDenied { req_id, reason } => {
            ctx.complete_join(req_id, JoinOutcome { granted: Err(reason) });
        }

        HubToNode::AttachOpen { attach_id, agent_id, cols, rows, replay_bytes } => {
            attachsock::open(&ctx, attach_id, agent_id, cols, rows, replay_bytes).await;
        }

        HubToNode::AttachTicket { req_id, ticket, agent_id, expires_at_ms, operator } => {
            // The hub has decided this operator may attach to this agent. The
            // node holds the ticket until it is spent or it expires; the data
            // then goes straight between them. See `proto::attachsock`.
            tracing::info!(%agent_id, %operator, "a direct attach is authorised");
            ctx.add_ticket(ticket, agent_id, expires_at_ms);
            // Said only after it is in the map, so the operator who is told
            // "here is your ticket" can spend it on the next line.
            let _ = ctx.send_hub(&NodeToHub::TicketStored { req_id }).await;
        }

        HubToNode::AttachData { attach_id, data } => {
            attachsock::input(&ctx, attach_id, data).await;
        }

        HubToNode::TreeFingerprint { req_id, workspace_id, path } => {
            // Cheap, but still a walk of somebody's tree: off the session
            // loop like the parse, so a slow disk does not stall spawns.
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let root = std::path::PathBuf::from(&path);
                let (fingerprint, files, partial) = tokio::task::spawn_blocking(move || {
                    codegraph::fingerprint(&root, &codegraph::Limits::default())
                })
                .await
                .unwrap_or_else(|_| (String::new(), 0, true));
                let _ = ctx
                    .send_hub(&NodeToHub::TreeFingerprintResult {
                        req_id,
                        workspace_id,
                        fingerprint,
                        files,
                        partial,
                    })
                    .await;
            });
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

        HubToNode::PutFile { req_id, workspace_id, root, rel, data_b64 } => {
            // Writing somebody's file is I/O on a thread of its own: a 20MB
            // video must not stall spawns and attach traffic for the machine.
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let (ok, path, bytes, detail) =
                    match putfile::write(&root, &rel, &data_b64).await {
                        Ok((p, n)) => (true, p, n, String::new()),
                        Err(e) => (false, String::new(), 0, format!("{e:#}")),
                    };
                if ok {
                    tracing::info!(%workspace_id, %path, bytes, "wrote a file for the operator");
                } else {
                    tracing::warn!(%workspace_id, %rel, "could not write it: {detail}");
                }
                let _ = ctx
                    .send_hub(&NodeToHub::PutFileResult { req_id, ok, path, bytes, detail })
                    .await;
            });
        }

        HubToNode::GetFile { req_id, root, rel } => {
            // Off the session loop, like PutFile: reading 20MB off a slow
            // disk must not stall spawns and attach traffic for the machine.
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let (ok, data_b64, media_type, detail) =
                    match putfile::read(&root, &rel, proto::MAX_FETCH_BYTES).await {
                        Ok((bytes, kind)) => {
                            (true, proto::b64::encode(&bytes), kind, String::new())
                        }
                        Err(e) => (false, String::new(), String::new(), format!("{e:#}")),
                    };
                if !ok {
                    tracing::warn!(%rel, "could not read it for the hub: {detail}");
                }
                let _ = ctx
                    .send_hub(&NodeToHub::FileBytes { req_id, ok, data_b64, media_type, detail })
                    .await;
            });
        }

        HubToNode::TerminalMouse { req_id, slot_id, on } => {
            // The node's own view of that pane, not the operator's session:
            // mouse is a per-session option, so this changes what the FLEET
            // sees and leaves anybody else's tmux alone.
            let view = crate::spawn::tmux_view(&slot_id);
            let (ok, detail) = crate::tmux::set_mouse(&view, on);
            tracing::info!(%view, on, ok, "tmux mouse");
            let _ = ctx
                .send_hub(&NodeToHub::TerminalMouseSet { req_id, ok, detail })
                .await;
        }

        HubToNode::AttachResize { attach_id, cols, rows } => {
            attachsock::resize(&ctx, &attach_id, cols, rows);
        }

        HubToNode::AttachClose { attach_id } => {
            attachsock::close(&ctx, &attach_id);
        }

        // STOP what this agent is doing, without ending it.
        //
        // Raw keys straight at the terminal, and deliberately NOT through
        // `type_line`: that writes a line and presses Enter, and an agent
        // mid-turn is not reading a line. An interrupt has to land as a
        // keystroke while the CLI is busy, which is the only moment it is
        // ever wanted.
        HubToNode::Interrupt { agent_id, by } => {
            let Some(io) = ctx.agent_io(&agent_id) else {
                tracing::warn!(%agent_id, "interrupt for an agent this node does not have");
                return;
            };
            let keys = ctx
                .agent(&agent_id)
                .map(|rec| rec.cli_id.clone())
                .and_then(|cli| {
                    ctx.descriptor(&cli)
                        .and_then(|d| d.modes.get("interactive"))
                        .and_then(|m| m.interrupt_keys.clone())
                });
            match (keys, &io.input) {
                (Some(keys), Some(tx)) if !keys.is_empty() => {
                    let sent = tx.send(keys.as_bytes().to_vec()).await.is_ok();
                    tracing::info!(%agent_id, %by, sent, "interrupting this agent");
                    if sent {
                        crate::prompts::say(ctx, &agent_id, &io, &format!(
                            "[nexmesh] interrupted \u{2014} {by}. Nothing has been lost: this \
                             conversation is intact and your seat is where it was."
                        )).await;
                    }
                }
                (None, _) => {
                    tracing::info!(%agent_id, %by,
                        "nothing to interrupt with: this CLI declares no interrupt keys");
                    crate::prompts::say(ctx, &agent_id, &io, &format!(
                        "[nexmesh] {by} \u{2014} but this CLI declares no way to be \
                         interrupted, so nothing was typed. Add `interrupt_keys` to its \
                         descriptor if it has one."
                    )).await;
                }
                _ => {
                    tracing::warn!(%agent_id, "this seat has no terminal to interrupt");
                }
            }
        }

        // Compact this agent's context, now, because the operator said so.
        //
        // The node resolves WHAT to type, from the descriptor it holds: the
        // hub does not know the slash commands of every CLI and should not
        // have to. Refused when the screen is waiting for an answer, like
        // any other keystroke -- a compaction request must not be the Enter
        // that approves a tool call nobody read.
        HubToNode::Compact { agent_id, by } => {
            let Some(io) = ctx.agent_io(&agent_id) else {
                tracing::warn!(%agent_id, "compact for an agent this node does not have");
                return;
            };
            // A PROTOCOL DRIVER compacts through its own protocol: codex has
            // `thread/compact/start`, and typing a slash command at a JSON-RPC
            // pipe would be a line of garbage in its input.
            if let Some(ctl) = io.control() {
                let ok = ctl.send(crate::ctx::DriverControl::Compact).await.is_ok();
                tracing::info!(%agent_id, %by, ok, "asked the driver to compact");
                return;
            }
            // A HEADLESS claude reads stream-json on stdin and takes a slash
            // command as a user message, the way the Agent SDK sends one. It
            // has no terminal, which is why typing it failed before.
            if io.kind == crate::ctx::IoKind::Pipes {
                if let Some(tx) = &io.input {
                    let line = format!("{}\n", crate::driver::user_message("/compact"));
                    let ok = tx.send(line.into_bytes()).await.is_ok();
                    tracing::info!(%agent_id, %by, ok, "sent /compact as a stream-json message");
                    return;
                }
            }
            // The agent's CLI, then that CLI's interactive mode, then what it
            // says to type. Any of the three being absent means "this CLI
            // does not declare one", which is reported rather than guessed.
            let cmd = ctx
                .agent(&agent_id)
                .map(|rec| rec.cli_id.clone())
                .and_then(|cli| {
                    ctx.descriptor(&cli)
                        .and_then(|d| d.modes.get("interactive"))
                        .and_then(|m| m.compact_command.clone())
                });
            match cmd {
                None => {
                    crate::prompts::say(ctx, &agent_id, &io, &format!(
                        "[nexmesh] {by} asked this seat to compact its context, but its \
                         CLI declares no compaction command, so nothing was typed. Add \
                         `compact_command` to its descriptor if it has one."
                    )).await;
                }
                Some(cmd) => {
                    match crate::prompts::type_line(&io, &cmd).await {
                        Ok(()) => {
                            crate::prompts::say(ctx, &agent_id, &io, &format!(
                                "[nexmesh] typed {cmd:?} to compact this context \u{2014} {by} \
                                 asked for it."
                            )).await;
                        }
                        Err(why) => {
                            crate::prompts::say(ctx, &agent_id, &io, &format!(
                                "[nexmesh] {by} asked this seat to compact, but {why}; \
                                 nothing was typed."
                            )).await;
                        }
                    }
                }
            }
        }
        // A permission prompt the operator (or a standing auto-approval) has
        // answered. Only this node can type at that terminal, and only this
        // node read the menu, so the hub is handing back one of the options
        // this node reported rather than a decision of its own.
        HubToNode::ApprovalDecided { approval_id, agent_id, keys, label, by } => {
            let Some(io) = ctx.agent_io(&agent_id) else {
                tracing::warn!(%agent_id, %approval_id,
                               "an approval came back for an agent this node does not have");
                return;
            };
            tracing::info!(%agent_id, %approval_id, %by, %label, "answering a permission prompt");
            crate::prompts::apply_decision(ctx, &agent_id, &io, &keys, &label, &by).await;
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
