// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! `nexmesh` -- the operator CLI. Runs on any machine and talks to the hub over
//! HTTP, so it works from any machine, whether or not that machine hosts anything.

mod install;
mod tlstrust;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "nexmesh", version, about = "nexmesh fleet control")]
struct Args {
    #[arg(long, global = true, env = "NEXMESH_HUB_HTTP", default_value = "http://127.0.0.1:7870")]
    hub: String,
    /// Operator token. Mint one with `nexmesh-hub operator-token <label>`.
    #[arg(long, global = true, env = "NEXMESH_OPERATOR_TOKEN")]
    token: Option<String>,
    /// PEM file with the hub's certificate (or its CA), for a self-signed hub.
    #[arg(long, global = true, env = "NEXMESH_CA_CERT")]
    ca_cert: Option<String>,
    /// Your client certificate, when the hub requires mutual TLS.
    #[arg(long, global = true, env = "NEXMESH_CLIENT_CERT", requires = "client_key")]
    client_cert: Option<String>,
    /// Private key for --client-cert.
    #[arg(long, global = true, env = "NEXMESH_CLIENT_KEY", requires = "client_cert")]
    client_key: Option<String>,
    /// Directory holding a paired identity; anything not passed explicitly is
    /// taken from here.
    #[arg(long, global = true, env = "NEXMESH_IDENTITY", default_value = nexmesh_proto::identity::DEFAULT_DIR)]
    identity_dir: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hub health and version.
    Health,
    /// Nodes, their CLIs and whether they are connected.
    Nodes,
    /// List sessions.
    Sessions,
    /// Create a session, optionally with a workspace on one node.
    NewSession {
        name: String,
        /// Workspace name.
        #[arg(long)]
        workspace: Option<String>,
        /// "node_local" or "repo".
        #[arg(long, default_value = "node_local")]
        kind: String,
        /// Node the workspace lives on (required for node_local).
        #[arg(long)]
        node: Option<String>,
        /// Absolute path on that node.
        #[arg(long)]
        path: Option<String>,
        /// Git remote, for a repo workspace.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Add a slot to a session.
    AddAgent {
        session: String,
        /// CLI id from a node's inventory, e.g. claude, opencode, qwen.
        #[arg(long)]
        cli: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Pin to a node by name.
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value = "headless")]
        mode: String,
        /// Glob this agent owns, repeatable.
        #[arg(long = "owns")]
        owns: Vec<String>,
    },
    /// Ask the placed node to start a slot's agent.
    Spawn { slot: String },
    /// Adopt an already-running tmux session so you can watch and drive it
    /// from anywhere. Your own terminal stays attached.
    Adopt {
        session: String,
        /// Node whose tmux server holds it.
        #[arg(long)]
        node: String,
        /// tmux target: session, window or pane (e.g. work, work:1.0, %3).
        #[arg(long = "tmux")]
        tmux_target: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Live status of every agent in a session.
    Status { session: String },
    /// Stop sending work to an agent, or to the whole team. Reversible; a
    /// turn already running finishes.
    Pause {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Start sending work again.
    Resume {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
    },
    /// Accept an agent's work and let it stop. Omit the name to release the
    /// whole team.
    Release {
        session: String,
        who: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// How long to let it finish before terminating it.
        #[arg(long, default_value_t = 15000)]
        grace_ms: u64,
    },
    /// Publish, pull or report on a shared workspace, on every machine at once.
    Sync {
        session: String,
        /// publish | refresh | status
        #[arg(long, default_value = "status")]
        op: String,
        /// Workspace name; omitted means all shared ones in the session.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Read the conversation between the agents.
    Log {
        session: String,
        /// Keep printing new messages as they are sent.
        #[arg(short, long)]
        follow: bool,
        /// How many past messages to show first.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Read what one agent's process actually printed.
    Transcript {
        session: String,
        /// Agent name, role, slot id or agent id.
        who: String,
        /// Bytes from the end.
        #[arg(long, default_value_t = 65536)]
        tail: usize,
    },
    /// Attach to an agent's terminal, wherever it is running.
    ///
    /// An interactive agent can be watched AND typed into; a headless one is
    /// view-only. Detach with Ctrl-] -- the agent keeps working either way.
    Attach {
        session: String,
        /// Agent name, role, slot id or agent id.
        who: String,
        /// Bytes of scrollback to replay first.
        #[arg(long, default_value_t = 16384)]
        replay: usize,
    },
    /// Join a fleet as an operator, using a one-time code from the dashboard.
    /// Your key is generated here and never leaves this machine.
    Pair {
        /// The hub's pairing address, e.g. https://machine-a:7871
        #[arg(long = "pair-hub")]
        pair_hub: String,
        /// The code shown in the dashboard.
        #[arg(long)]
        code: String,
        /// The address to use afterwards, e.g. https://machine-a:7870
        #[arg(long)]
        connect: Option<String>,
        /// Accept the hub's certificate without verifying it, for a first
        /// contact you have decided to trust.
        #[arg(long)]
        insecure: bool,
    },
    /// Register the orchestrator as an MCP server in one of your agent CLIs,
    /// so you can spin up teams from whatever session you are in.
    InstallOrchestrator {
        /// CLI to install into: claude, opencode, qwen, codex, gemini.
        #[arg(long = "cli")]
        cli_id: String,
        /// Directory of CLI descriptors.
        #[arg(long, env = "NEXMESH_CLIS", default_value = "/etc/nexmesh/clis")]
        clis: String,
        /// Name the server appears under inside that CLI.
        #[arg(long, default_value = "nexmesh")]
        name: String,
        /// Path to nexmesh-orch; defaults to a sibling of this binary.
        #[arg(long)]
        orch: Option<String>,
        /// Print what would be done without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Send a message to the team or one agent, as the operator.
    Send {
        session: String,
        text: String,
        #[arg(long)]
        to: Option<String>,
    },
}

/// The operator token, read once from args/env and attached to every request.
/// Kept in a process-local rather than threaded through every call site, so a
/// new subcommand cannot forget to authenticate.
static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn token() -> Option<&'static String> {
    TOKEN.get().and_then(|t| t.as_ref())
}

fn client(ca_cert: Option<&str>, identity: Option<(&str, &str)>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
    if let Some(path) = ca_cert {
        let pem = std::fs::read(path).with_context(|| format!("reading CA file {path}"))?;
        b = b.add_root_certificate(
            reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parsing {path} as a PEM certificate"))?,
        );
    }
    if let Some((cert, key)) = identity {
        // reqwest wants one PEM containing both.
        let mut pem = std::fs::read(cert)
            .with_context(|| format!("reading client certificate {cert}"))?;
        pem.push(b'\n');
        pem.extend(std::fs::read(key).with_context(|| format!("reading client key {key}"))?);
        b = b.identity(
            reqwest::Identity::from_pem(&pem)
                .context("the client certificate and key could not be loaded together")?,
        );
    }
    Ok(b.build()?)
}

fn authed(r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token() {
        Some(t) => r.bearer_auth(t),
        None => r,
    }
}

/// Turn an HTTP error into the hub's own message, so an auth failure says what
/// to do instead of surfacing as a JSON parse error.
async fn body_or_error(r: reqwest::Response, what: &str) -> Result<Value> {
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(v);
    }
    let msg = v
        .get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| text.chars().take(200).collect());
    anyhow::bail!("{what} failed ({status}): {msg}")
}

async fn get(c: &reqwest::Client, url: String) -> Result<Value> {
    let r = authed(c.get(&url)).send().await.with_context(|| format!("GET {url}"))?;
    body_or_error(r, "request").await
}

async fn post(c: &reqwest::Client, url: String, body: Value) -> Result<Value> {
    let r = authed(c.post(&url).json(&body))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    body_or_error(r, "request").await
}

fn bail_on_error(v: &Value) -> Result<()> {
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    Ok(())
}

/// Token counts get large; 12400 reads better as 12.4k in a table.
fn human(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn hhmmss(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::offset::LocalResult::Single(t) => t.format("%H:%M:%S").to_string(),
        _ => "--:--:--".to_string(),
    }
}

/// One line per message. The delivery marker matters: a message that was sent
/// but not acked is one the recipient's model has NOT seen yet, which is
/// usually the answer to "why is this team stuck?".
fn print_message(m: &Value) {
    let mark = if m["delivered"].as_bool() == Some(true) {
        " "
    } else if m["in_flight"].as_bool() == Some(true) {
        "~" // handed over, not yet confirmed
    } else {
        "!" // queued, recipient has not seen it
    };
    let kind = m["kind"].as_str().unwrap_or("text");
    let kind = if kind == "text" { String::new() } else { format!("[{kind}] ") };
    println!(
        "{} {} {:>12} -> {:<12} {}{}",
        hhmmss(m["at_ms"].as_i64().unwrap_or(0)),
        mark,
        m["from"].as_str().unwrap_or("?"),
        m["to"].as_str().unwrap_or("?"),
        kind,
        m["body"].as_str().unwrap_or("").replace('\n', " / "),
    );
}

/// Puts the terminal back the way we found it, even on an early return or a
/// panic. Leaving a shell in raw mode is a genuinely hostile thing to do.
struct RawGuard;

impl RawGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("entering raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        println!("\r");
    }
}

/// Ctrl-] detaches, the way telnet does. Ctrl-C cannot: in raw mode it belongs
/// to the agent, which is the whole point of a takeover.
const DETACH_BYTE: u8 = 0x1d;

async fn attach(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    replay: usize,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let r = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
    bail_on_error(&r)?;
    let agent = r["agent_id"].as_str().context("no agent id returned")?.to_string();
    let name = r["name"].as_str().unwrap_or(who).to_string();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!("{ws_base}/v1/attach/ws?agent={agent}&cols={cols}&rows={rows}&replay={replay}");
    // The attach upgrade needs the operator token too. It goes in a header,
    // never the query string: URLs land in proxy logs and shell history.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.as_str().into_client_request().context("building the attach request")?;
    if let Some(t) = token() {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {t}").parse().context("building the auth header")?,
        );
    }
    let (ws, _) = if url.starts_with("wss://") {
        let tls = tlstrust::client_config(ca_cert, identity)?;
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

    println!("attached to {name} ({agent}) -- Ctrl-] to detach");

    // Raw mode only after the connection is up, so a failure prints normally.
    let _guard = RawGuard::enter()?;

    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::channel::<String>(4);

    // Hub -> our terminal.
    let reader = {
        let quit_tx = quit_tx.clone();
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    WsMessage::Binary(data) => {
                        if out.write_all(&data).await.is_err() || out.flush().await.is_err() {
                            break;
                        }
                    }
                    WsMessage::Text(t) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        match v["t"].as_str() {
                            Some("opened") => {
                                if v["read_only"].as_bool() == Some(true) {
                                    let _ = out.write_all(
                                        b"\r\n[nexmesh] headless agent: view-only.\r\n").await;
                                    let _ = out.flush().await;
                                }
                            }
                            Some("closed") | Some("error") => {
                                let why = v["reason"].as_str()
                                    .or(v["message"].as_str())
                                    .unwrap_or("session ended");
                                let _ = quit_tx.send(why.to_string()).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
            let _ = quit_tx.send("connection closed".into()).await;
        })
    };

    // Window changes must reach the pty or a TUI redraws at the wrong size.
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
    tokio::spawn(async move {
        let Ok(mut sigwinch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while sigwinch.recv().await.is_some() {
            if let Ok((c, r)) = crossterm::terminal::size() {
                if resize_tx.send((c, r)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Our keyboard -> hub.
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    let reason = loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                let n = match n { Ok(0) | Err(_) => break "input ended".to_string(), Ok(n) => n };
                if let Some(pos) = buf[..n].iter().position(|b| *b == DETACH_BYTE) {
                    // Send anything typed before the detach key, then leave.
                    if pos > 0 {
                        let _ = sink.send(WsMessage::Binary(buf[..pos].to_vec())).await;
                    }
                    break "detached".to_string();
                }
                if sink.send(WsMessage::Binary(buf[..n].to_vec())).await.is_err() {
                    break "connection lost".to_string();
                }
            }
            Some((c, r)) = resize_rx.recv() => {
                let msg = json!({ "t": "resize", "cols": c, "rows": r }).to_string();
                let _ = sink.send(WsMessage::Text(msg)).await;
            }
            Some(why) = quit_rx.recv() => break why,
        }
    };

    let _ = sink.send(WsMessage::Text(json!({ "t": "close" }).to_string())).await;
    let _ = sink.close().await;
    reader.abort();
    drop(_guard);
    println!("{reason}. The agent is still running.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    // A paired identity supplies anything not given explicitly.
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
        if let (Some(h), true) = (&id.identity.hub, args.hub == "http://127.0.0.1:7870") {
            args.hub = h.clone();
        }
    }

    let base = args.hub.trim_end_matches('/').to_string();
    let _ = TOKEN.set(args.token.clone());
    let identity = match (&args.client_cert, &args.client_key) {
        (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
        _ => None,
    };
    let c = client(args.ca_cert.as_deref(), identity)?;

    match args.cmd {
        Cmd::Health => println!("{:#}", get(&c, format!("{base}/healthz")).await?),

        Cmd::Nodes => {
            let v = get(&c, format!("{base}/v1/nodes")).await?;
            let empty = vec![];
            let nodes = v["nodes"].as_array().unwrap_or(&empty);
            if nodes.is_empty() {
                println!("no nodes enrolled");
                return Ok(());
            }
            println!("{:<12} {:<11} {:<14} {}", "NAME", "STATE", "ARCH", "CLIS");
            for n in nodes {
                let state = if n["connected"].as_bool() == Some(true) {
                    "connected"
                } else {
                    n["status"].as_str().unwrap_or("?")
                };
                println!(
                    "{:<12} {:<11} {:<14} {}",
                    n["name"].as_str().unwrap_or("?"),
                    state,
                    format!("{}/{}", n["os"].as_str().unwrap_or("?"), n["arch"].as_str().unwrap_or("?")),
                    n["clis"].as_array().map(|a| a.iter().filter_map(|x| x.as_str())
                        .collect::<Vec<_>>().join(",")).unwrap_or_default(),
                );
            }
        }

        Cmd::Sessions => {
            let v = get(&c, format!("{base}/v1/sessions")).await?;
            let empty = vec![];
            let rows = v["sessions"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no sessions");
                return Ok(());
            }
            println!("{:<14} {:<22} {:<10} {}", "ID", "NAME", "STATUS", "SLOTS");
            for s in rows {
                println!(
                    "{:<14} {:<22} {:<10} {}",
                    s["id"].as_str().unwrap_or("?"),
                    s["name"].as_str().unwrap_or("?"),
                    s["status"].as_str().unwrap_or("?"),
                    s["slots"].as_i64().unwrap_or(0)
                );
            }
        }

        Cmd::NewSession { name, workspace, kind, node, path, repo } => {
            let mut body = json!({ "name": name, "workspaces": [] });
            if let Some(ws) = workspace {
                if kind == "node_local" && (node.is_none() || path.is_none()) {
                    anyhow::bail!("a node_local workspace needs --node and --path");
                }
                let mut w = json!({ "name": ws, "kind": kind, "mounts": {} });
                if let Some(n) = &node {
                    w["node"] = json!(n);
                    if let Some(p) = &path {
                        w["mounts"] = json!({ n.clone(): p });
                    }
                }
                if let Some(r) = &repo {
                    w["repo_url"] = json!(r);
                }
                body["workspaces"] = json!([w]);
            }
            let v = post(&c, format!("{base}/v1/sessions"), body).await?;
            bail_on_error(&v)?;
            println!("session {} created", v["session_id"].as_str().unwrap_or("?"));
        }

        Cmd::AddAgent { session, cli, name, role, task, workspace, node, mode, owns } => {
            let mut body = json!({
                "cli_id": cli, "display_name": name, "mode": mode,
                "file_ownership": owns,
            });
            if let Some(r) = role { body["role"] = json!(r); }
            if let Some(t) = task { body["task"] = json!(t); }
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            if let Some(n) = node { body["placement"] = json!({ "node": n }); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/slots"), body).await?;
            bail_on_error(&v)?;
            println!("slot {} created", v["slot_id"].as_str().unwrap_or("?"));
        }

        Cmd::Spawn { slot } => {
            let v = post(&c, format!("{base}/v1/slots/{slot}/spawn"), json!({})).await?;
            bail_on_error(&v)?;
            println!(
                "spawning {} on {}",
                v["agent_id"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?")
            );
        }

        Cmd::Adopt { session, node, tmux_target, name, role, workspace } => {
            let mut body = json!({ "node": node, "tmux_target": tmux_target, "name": name });
            if let Some(r) = role { body["role"] = json!(r); }
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/adopt"), body).await?;
            bail_on_error(&v)?;
            println!(
                "adopted {} on {} as slot {}",
                v["tmux_target"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?"),
                v["slot_id"].as_str().unwrap_or("?")
            );
            println!("attach with:  nexmesh attach {session} {name}");
            if let Some(n) = v["note"].as_str() {
                println!("note: {n}");
            }
        }

        Cmd::Status { session } => {
            let v = get(&c, format!("{base}/v1/sessions/{session}/status")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let rows = v["agents"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no agents in this session");
                return Ok(());
            }
            println!(
                "{:<12} {:<14} {:<9} {:<9} {:<20} {:>9} {}",
                "NAME", "ROLE", "HOST", "KIND", "STATE", "TOKENS", "SUMMARY"
            );
            for a in rows {
                let tin = a["input_tokens"].as_i64().unwrap_or(0);
                let tout = a["output_tokens"].as_i64().unwrap_or(0);
                let tokens = if tin + tout == 0 {
                    "-".to_string()
                } else {
                    format!("{}/{}", human(tin), human(tout))
                };
                println!(
                    "{:<12} {:<14} {:<9} {:<9} {:<20} {:>9} {}",
                    a["name"].as_str().unwrap_or("?"),
                    a["role"].as_str().unwrap_or("-"),
                    a["host"].as_str().unwrap_or("-"),
                    a["provenance"].as_str().unwrap_or("-"),
                    if a["paused"].as_bool() == Some(true) {
                        format!("{} (paused)", a["task_state"].as_str().unwrap_or("-"))
                    } else {
                        a["task_state"].as_str().unwrap_or("-").to_string()
                    },
                    tokens,
                    a["summary"].as_str().unwrap_or(""),
                );
            }
            println!("\n  (TOKENS = input/output)");
        }

        Cmd::Pause { session, who, reason } => {
            let mut body = json!({ "paused": true });
            if let Some(w) = who { body["who"] = json!(w); }
            if let Some(r) = reason { body["reason"] = json!(r); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/pause"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("paused"));
        }

        Cmd::Resume { session, who } => {
            let mut body = json!({ "paused": false });
            if let Some(w) = who { body["who"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/pause"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("resumed"));
        }

        Cmd::Release { session, who, note, grace_ms } => {
            let mut body = json!({ "grace_ms": grace_ms });
            if let Some(w) = who { body["who"] = json!(w); }
            if let Some(n) = note { body["note"] = json!(n); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/release"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("released"));
        }

        Cmd::Sync { session, op, workspace } => {
            let mut body = json!({ "op": op });
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/sync"), body).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            for ws in v["workspaces"].as_array().unwrap_or(&empty) {
                if let Some(e) = ws["error"].as_str() {
                    println!("{}: {e}", ws["workspace"].as_str().unwrap_or("?"));
                    continue;
                }
                println!("{} ({})", ws["workspace"].as_str().unwrap_or("?"),
                         ws["branch"].as_str().unwrap_or("?"));
                for r in ws["results"].as_array().unwrap_or(&empty) {
                    println!("  {:<12} {}{}", r["node"].as_str().unwrap_or("?"),
                             r["detail"].as_str().unwrap_or(""),
                             if r["conflict"].as_bool() == Some(true) { "   [CONFLICT]" } else { "" });
                }
            }
        }

        Cmd::Log { session, follow, limit } => {
            // Seed with the tail of the history, then stream forward by id so
            // nothing is missed or repeated between polls.
            let v = get(&c, format!("{base}/v1/sessions/{session}/log?limit={limit}")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let msgs = v["messages"].as_array().unwrap_or(&empty).clone();
            if msgs.is_empty() && !follow {
                println!("no messages yet in {session}");
                return Ok(());
            }
            println!("{:<8} {:<1} {:>12}    {:<12} {}", "TIME", "", "FROM", "TO", "MESSAGE");
            let mut last = 0i64;
            for m in &msgs {
                print_message(m);
                last = last.max(m["id"].as_i64().unwrap_or(0));
            }
            if !follow {
                println!("\n  ( ! = recipient has not seen it yet, ~ = delivery in flight )");
                return Ok(());
            }
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let v = get(&c, format!("{base}/v1/sessions/{session}/log?after={last}")).await?;
                for m in v["messages"].as_array().unwrap_or(&empty) {
                    print_message(m);
                    last = last.max(m["id"].as_i64().unwrap_or(0));
                }
            }
        }

        Cmd::Transcript { session, who, tail } => {
            let r = get(&c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
            bail_on_error(&r)?;
            let agent = r["agent_id"].as_str().context("no agent id returned")?;
            let name = r["name"].as_str().unwrap_or(&who);
            let v = get(&c, format!("{base}/v1/agents/{agent}/transcript?tail={tail}")).await?;
            bail_on_error(&v)?;
            let text = v["transcript"].as_str().unwrap_or("");
            if text.is_empty() {
                println!("no output recorded for {name} ({agent})");
            } else {
                println!("--- {name} ({agent}) ---");
                print!("{text}");
            }
        }

        Cmd::Attach { session, who, replay } => {
            attach(&base, &c, &session, &who, replay, args.ca_cert.as_deref(), identity).await?;
        }

        Cmd::Pair { pair_hub, code, connect, insecure } => {
            let p = nexmesh_proto::pair_client::pair(
                &pair_hub, &code, &args.identity_dir,
                connect.as_deref(), args.ca_cert.as_deref(), insecure,
            )
            .await?;
            println!("Paired as {} '{}'.", p.kind, p.name);
            println!("Identity written to {}", p.identity_file.display());
            println!("\nTry it:  nexmesh nodes");
            println!("Then give your CLIs the orchestrator:");
            println!("  nexmesh install-orchestrator --cli claude");
            return Ok(());
        }

        Cmd::InstallOrchestrator { cli_id, clis, name, orch, dry_run } => {
            let msg = install::install(install::Opts {
                cli_id: &cli_id,
                clis_dir: &clis,
                server_name: &name,
                orch: orch.as_deref(),
                hub: &base,
                token: args.token.as_deref(),
                ca_cert: args.ca_cert.as_deref(),
                dry_run,
            })?;
            println!("{msg}");
            if !dry_run {
                println!("Restart {cli_id} to pick it up, then ask it to list your nodes.");
                if args.token.is_none() {
                    println!(
                        "NOTE: no operator token was passed, so the orchestrator will only work \
                         against a hub that has none. Re-run with --token once you mint one."
                    );
                }
            }
        }

        Cmd::Send { session, text, to } => {
            let mut body = json!({ "text": text });
            if let Some(t) = to { body["to"] = json!(t); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/messages"), body).await?;
            bail_on_error(&v)?;
            println!("sent");
        }
    }
    Ok(())
}
