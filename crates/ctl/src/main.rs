// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! `nexmesh` -- the operator CLI. Runs on any machine and talks to the hub over
//! HTTP, so it works from the laptop whether or not the laptop hosts anything.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "nexmesh", version, about = "nexmesh fleet control")]
struct Args {
    #[arg(long, env = "NEXMESH_HUB_HTTP", default_value = "http://127.0.0.1:7870")]
    hub: String,
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
    /// Live status of every agent in a session.
    Status { session: String },
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
    /// Send a message to the team or one agent, as the operator.
    Send {
        session: String,
        text: String,
        #[arg(long)]
        to: Option<String>,
    },
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?)
}

async fn get(c: &reqwest::Client, url: String) -> Result<Value> {
    let r = c.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    Ok(r.json().await?)
}

async fn post(c: &reqwest::Client, url: String, body: Value) -> Result<Value> {
    let r = c.post(&url).json(&body).send().await.with_context(|| format!("POST {url}"))?;
    Ok(r.json().await?)
}

fn bail_on_error(v: &Value) -> Result<()> {
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    Ok(())
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

async fn attach(base: &str, c: &reqwest::Client, session: &str, who: &str, replay: usize) -> Result<()> {
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
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("attaching to {name}"))?;
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
    let args = Args::parse();
    let base = args.hub.trim_end_matches('/').to_string();
    let c = client()?;

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
                "{:<12} {:<16} {:<10} {:<10} {:<20} {}",
                "NAME", "ROLE", "HOST", "KIND", "STATE", "SUMMARY"
            );
            for a in rows {
                println!(
                    "{:<12} {:<16} {:<10} {:<10} {:<20} {}",
                    a["name"].as_str().unwrap_or("?"),
                    a["role"].as_str().unwrap_or("-"),
                    a["host"].as_str().unwrap_or("-"),
                    a["provenance"].as_str().unwrap_or("-"),
                    a["task_state"].as_str().unwrap_or("-"),
                    a["summary"].as_str().unwrap_or(""),
                );
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
            attach(&base, &c, &session, &who, replay).await?;
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
