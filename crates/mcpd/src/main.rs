// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! nexmesh-mcpd -- the MCP shim an agent CLI loads.
//!
//! It speaks MCP over stdio to its CLI and forwards every tool call to the
//! LOCAL node over a unix socket. All tool logic lives in the hub, so mcpd
//! holds no credentials and never talks to the hub directly: a compromised
//! agent gains no fleet access, and a hub blip cannot wedge an agent's stdio.
//!
//! It is also how an attached agent joins. A human runs `claude` or `opencode`
//! by hand, anywhere; its mcpd finds the local node, the node asks the hub for
//! a seat, and the agent becomes a full team member nobody spawned.
//!
//! NOTE: stdout carries JSON-RPC only. Every diagnostic goes to stderr, or the
//! host CLI will fail to parse the stream.

use anyhow::{Context, Result};
use clap::Parser;
use nexmesh_proto::{
    localsock::{McpdToNode, NodeToMcpd, DEFAULT_SOCKET},
    Envelope, ToolDef,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP revisions this shim is known to work with.
const SUPPORTED_MCP: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const PREFERRED_MCP: &str = "2024-11-05";

#[derive(Parser, Clone)]
#[command(name = "nexmesh-mcpd", version, about = "nexmesh MCP shim")]
struct Args {
    #[arg(long, env = "NEXMESH_SOCK", default_value = DEFAULT_SOCKET)]
    sock: String,
    #[arg(long, env = "NEXMESH_AGENT_ID")]
    agent_id: Option<String>,
    #[arg(long, env = "NEXMESH_SLOT")]
    slot: Option<String>,
    #[arg(long, env = "NEXMESH_SESSION")]
    session: Option<String>,
    #[arg(long, env = "NEXMESH_CLI", default_value = "unknown")]
    cli: String,
    #[arg(long, env = "NEXMESH_ROLE")]
    role: Option<String>,
    #[arg(long, env = "NEXMESH_NAME")]
    name: Option<String>,
}

/// The link to the local node: one connection, used sequentially.
struct Node {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
    #[allow(dead_code)] // the Link owns sequencing once the keeper takes over
    seq: u64,
}

impl Node {
    async fn connect(path: &str) -> Result<Self> {
        // The node may still be starting; a few quick retries beat failing the
        // whole agent.
        let mut last = None;
        for attempt in 0..10 {
            match UnixStream::connect(path).await {
                Ok(s) => {
                    let (r, w) = s.into_split();
                    return Ok(Self { lines: BufReader::new(r).lines(), write: w, seq: 1 });
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt + 1))).await;
                }
            }
        }
        Err(anyhow::anyhow!(
            "cannot reach the nexmesh node at {path}: {}. Is nexmesh-node running on this machine?",
            last.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

    async fn send(&mut self, frame: &McpdToNode) -> Result<()> {
        let mut s = serde_json::to_string(frame)?;
        s.push('\n');
        self.write.write_all(s.as_bytes()).await?;
        self.write.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<NodeToMcpd> {
        loop {
            let line = self
                .lines
                .next_line()
                .await?
                .context("the nexmesh node closed the connection")?;
            if line.trim().is_empty() {
                continue;
            }
            return Ok(serde_json::from_str(&line)?);
        }
    }

}

/// Say who we are and take (or retake) a seat.
async fn register(
    node: &mut Node,
    args: &Args,
    pid: i32,
    cwd: &str,
) -> Result<(String, String, Vec<ToolDef>)> {
    node.send(&McpdToNode::Register {
        cli_id: args.cli.clone(),
        pid,
        cwd: cwd.to_string(),
        agent_hint: args.agent_id.clone(),
        slot_hint: args.slot.clone(),
        session_hint: args.session.clone(),
        role: args.role.clone(),
        name: args.name.clone(),
        cli_session: cli_session_id(),
    })
    .await?;
    match node.recv().await? {
        NodeToMcpd::Registered { agent_id, slot_id, tools, .. } => Ok((agent_id, slot_id, tools)),
        NodeToMcpd::Error { message, .. } => {
            anyhow::bail!("the node refused registration: {message}")
        }
        other => anyhow::bail!("unexpected first frame from the node: {other:?}"),
    }
}

/// The connection to the node, kept alive by somebody whose only job it is.
///
/// It used to be owned by the loop that reads the CLI's stdin, which meant it
/// was only ever looked at when the model called a tool. A node restart
/// therefore left the seat reading `disconnected` until the model happened to
/// use one -- so an operator talking to their CLI watched it stay
/// disconnected while they typed, which is indistinguishable from broken.
///
/// Now a reader task owns the socket. It notices the moment the node goes,
/// redials, registers again with the same conversation id -- which is what
/// takes the same seat back -- and forwards frames to whoever is waiting.
struct Link {
    args: Args,
    pid: i32,
    cwd: String,
    /// Replaced on every reconnect; `None` while there is no node.
    write: tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>,
    frames: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<NodeToMcpd>>,
    slot: std::sync::Mutex<String>,
    seq: std::sync::atomic::AtomicU64,
    /// Woken when a reconnect finishes, so a tool call that arrived in the
    /// gap waits for the link rather than failing into the model's lap.
    back: tokio::sync::Notify,
}

impl Link {
    fn next_req(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    fn slot_id(&self) -> String {
        self.slot.lock().unwrap().clone()
    }

    async fn send(&self, frame: &McpdToNode) -> Result<()> {
        let mut g = self.write.lock().await;
        let w = g.as_mut().context("not connected to the node")?;
        let mut s = serde_json::to_string(frame)?;
        s.push('\n');
        w.write_all(s.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    /// The next frame for `req_id`. Anything else is a late answer to a call
    /// nobody is waiting for any more, and is dropped rather than returned
    /// to the wrong caller.
    async fn reply(&self, req_id: u64, wait: std::time::Duration) -> Result<NodeToMcpd> {
        let mut rx = self.frames.lock().await;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(f)) => match &f {
                    NodeToMcpd::ToolReply { req_id: got, .. } if *got == req_id => return Ok(f),
                    NodeToMcpd::Error { req_id: Some(got), .. } if *got == req_id => return Ok(f),
                    NodeToMcpd::Error { req_id: None, .. } => return Ok(f),
                    other => {
                        tracing::debug!(?other, "a late frame for nobody; dropped");
                        continue;
                    }
                },
                Ok(None) => anyhow::bail!("the link to the node closed"),
                Err(_) => anyhow::bail!("the node did not answer in time"),
            }
        }
    }
}

/// Dial and register, for ever, with a bounded backoff.
///
/// For ever on purpose: this runs in the background of a conversation that
/// may last hours, and a node being restarted for a deploy should cost the
/// seat a few seconds, not the rest of the session.
async fn relink(link: &Link) -> (tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
                                 tokio::net::unix::OwnedWriteHalf,
                                 String) {
    let mut wait = std::time::Duration::from_millis(250);
    loop {
        match UnixStream::connect(&link.args.sock).await {
            Ok(stream) => {
                let (r, w) = stream.into_split();
                let mut node = Node { lines: BufReader::new(r).lines(), write: w, seq: 0 };
                match register(&mut node, &link.args, link.pid, &link.cwd).await {
                    Ok((agent_id, slot_id, _tools)) => {
                        tracing::info!(%agent_id, %slot_id, "registered with the node again");
                        return (node.lines, node.write, slot_id);
                    }
                    // A node that refuses the registration will keep refusing
                    // it until something changes, so this waits like any
                    // other failure rather than spinning. Dropping `node`
                    // closes the socket we just opened.
                    Err(e) => tracing::warn!(error = %e, "the node refused the registration"),
                }
            }
            Err(e) => tracing::debug!(error = %e, "the node is not there yet"),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(std::time::Duration::from_secs(5));
    }
}

/// Forward what the node says, and put the link back when it goes.
async fn keeper(
    link: std::sync::Arc<Link>,
    tx: tokio::sync::mpsc::Sender<NodeToMcpd>,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) {
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<NodeToMcpd>(&line) {
                    Ok(f) => {
                        if tx.send(f).await.is_err() {
                            return; // the shim is shutting down
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "unparseable frame from the node"),
                }
            }
            // EOF or a broken socket: the node has gone. This is the whole
            // point of the task -- noticing now, rather than the next time
            // the model happens to call a tool.
            _ => {
                tracing::warn!("the node went away; reconnecting");
                *link.write.lock().await = None;
                let (new_lines, w, slot) = relink(&link).await;
                *link.write.lock().await = Some(w);
                *link.slot.lock().unwrap() = slot;
                lines = new_lines;
                link.back.notify_waiters();
            }
        }
    }
}

async fn write_stdout(v: &Value) -> Result<()> {
    let mut out = tokio::io::stdout();
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    out.write_all(s.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: Value, code: i64, message: impl std::fmt::Display) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.to_string() } })
}

/// A tool result carrying teammate messages. Prepending them is the universal
/// delivery path: it needs nothing from the host CLI beyond MCP itself, which
/// is what lets an unknown CLI join a team with no bespoke code.
fn tool_content(text: &str, deliver: &[Envelope]) -> Value {
    let mut body = String::new();
    if !deliver.is_empty() {
        body.push_str("[PENDING MESSAGES]\n");
        for m in deliver {
            body.push_str(&format!(
                "- from {}: {}\n",
                m.from_name.as_deref().unwrap_or("orchestrator"),
                m.body
            ));
        }
        body.push_str("[END PENDING MESSAGES]\nRespond to these before continuing.\n\n");
    }
    body.push_str(text);
    json!({ "content": [ { "type": "text", "text": body } ] })
}

/// Environment variables through which a CLI tells an MCP server which of
/// its own conversations is running.
///
/// Verified by dumping the environment each CLI actually hands a server, not
/// read off documentation: claude sets `CLAUDE_CODE_SESSION_ID`, codex sets
/// `CODEX_THREAD_ID`. opencode publishes nothing of the kind today, so it
/// falls back to identity by directory and name -- which is why this is a
/// list and not an assumption.
const CLI_SESSION_VARS: &[&str] = &[
    // An explicit override wins: a wrapper that knows better can say so.
    "NEXMESH_CLI_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    // Not observed in the wild yet; harmless, and ready if they appear.
    "OPENCODE_SESSION_ID",
    "QWEN_SESSION_ID",
    "GEMINI_SESSION_ID",
];

/// Which conversation this shim belongs to, if the CLI says.
fn cli_session_id() -> Option<String> {
    for key in CLI_SESSION_VARS {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout is JSON-RPC only
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let mut node = Node::connect(&args.sock).await?;
    let pid = std::process::id() as i32;
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let (agent_id, slot_id, tools) = register(&mut node, &args, pid, &cwd).await?;
    tracing::info!(%agent_id, %slot_id, tools = tools.len(), "registered with the node");

    // From here the socket belongs to the keeper: it forwards what the node
    // says and puts the link back on its own when the node restarts, so a
    // seat does not sit there reading `disconnected` until the model next
    // calls a tool.
    let (tx, rx) = tokio::sync::mpsc::channel::<NodeToMcpd>(256);
    let link = std::sync::Arc::new(Link {
        args: args.clone(),
        pid,
        cwd: cwd.clone(),
        write: tokio::sync::Mutex::new(Some(node.write)),
        frames: tokio::sync::Mutex::new(rx),
        slot: std::sync::Mutex::new(slot_id.clone()),
        seq: std::sync::atomic::AtomicU64::new(1),
        back: tokio::sync::Notify::new(),
    });
    {
        let link = link.clone();
        tokio::spawn(keeper(link, tx, node.lines));
    }

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = stdin.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = write_stdout(&rpc_err(Value::Null, -32700, format!("parse error: {e}"))).await;
                continue;
            }
        };
        let method = req["method"].as_str().unwrap_or("").to_string();
        let id = req.get("id").cloned();

        // A JSON-RPC notification has no id and must never be answered.
        let Some(id) = id else {
            tracing::debug!(%method, "notification");
            continue;
        };

        let response = match method.as_str() {
            "initialize" => {
                let asked = req["params"]["protocolVersion"].as_str().unwrap_or(PREFERRED_MCP);
                let version = if SUPPORTED_MCP.contains(&asked) { asked } else { PREFERRED_MCP };
                ok(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "nexmesh", "version": VERSION },
                        "instructions": format!(
                            "You are part of a nexmesh team (slot {}). Teammates may run on \
                             other machines, under other CLIs, or not be CLIs at all. Call \
                             query_knowledge and check_team_status before planning. Teammate \
                             messages arrive prefixed [PENDING MESSAGES] on any tool result.",
                            link.slot_id()
                        ),
                    }),
                )
            }

            "ping" => ok(id, json!({})),

            "tools/list" => ok(
                id,
                json!({
                    "tools": tools.iter().map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })).collect::<Vec<_>>()
                }),
            ),

            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                let arguments = req["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                // The node may have been restarted under us: an upgrade, a
                // reboot. Which half of the exchange notices depends on
                // timing -- a write into a socket whose far end has gone is
                // as often buffered and accepted, leaving the read to fail
                // -- so both halves lead here, and both are worth one
                // reconnect. The model is waiting on this call, and an
                // error is the one answer it cannot act on.
                let mut reply = None;
                let mut trouble = String::new();
                for attempt in 0..2 {
                    if attempt > 0 {
                        // The keeper is already putting the link back. Wait
                        // for it rather than racing it with a second dial:
                        // two registrations for one conversation is how a
                        // seat ends up with a twin.
                        let waited = tokio::time::timeout(
                            std::time::Duration::from_secs(20),
                            link.back.notified(),
                        )
                        .await;
                        if waited.is_err() {
                            trouble = "the node is not answering".into();
                            break;
                        }
                    }
                    let req_id = link.next_req();
                    if let Err(e) = link
                        .send(&McpdToNode::Tool {
                            req_id,
                            tool: name.clone(),
                            args: arguments.clone(),
                        })
                        .await
                    {
                        trouble = format!("node unreachable: {e}");
                        continue;
                    }
                    // Long, not unbounded: a capability ask can take minutes
                    // and a lost answer should still end in an error rather
                    // than a conversation that never comes back.
                    match link.reply(req_id, std::time::Duration::from_secs(900)).await {
                        Ok(frame) => {
                            reply = Some(frame);
                            break;
                        }
                        Err(e) => trouble = format!("node unreachable: {e}"),
                    }
                }
                let Some(frame) = reply else {
                    write_stdout(&rpc_err(id, -32603, trouble)).await?;
                    continue;
                };
                match frame {
                    NodeToMcpd::ToolReply { result, deliver, .. } => {
                        let text = if result["ok"].as_bool() == Some(true) {
                            result["text"].as_str().unwrap_or("").to_string()
                        } else {
                            format!("ERROR: {}", result["error"].as_str().unwrap_or("tool failed"))
                        };
                        let is_error = result["ok"].as_bool() != Some(true);
                        let mut content = tool_content(&text, &deliver);
                        if is_error {
                            content["isError"] = Value::Bool(true);
                        }
                        write_stdout(&ok(id, content)).await?;

                        // Only now have the messages reached the model, so only
                        // now may they be marked delivered. If we died before
                        // this line, the hub would hand them to the next agent
                        // in this slot rather than lose them.
                        if !deliver.is_empty() {
                            let ids: Vec<i64> = deliver.iter().map(|m| m.id).collect();
                            if let Err(e) = link.send(&McpdToNode::Ack { message_ids: ids }).await {
                                tracing::warn!(error = %e, "could not ack delivered messages");
                            }
                        }
                        continue;
                    }
                    NodeToMcpd::Error { message, .. } => rpc_err(id, -32603, message),
                    other => rpc_err(id, -32603, format!("unexpected node frame: {other:?}")),
                }
            }

            // Everything else: answer politely rather than hang the host.
            other => rpc_err(id, -32601, format!("method '{other}' is not supported")),
        };
        write_stdout(&response).await?;
    }

    let _ = link.send(&McpdToNode::Bye).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CLI_SESSION_VARS;

    #[test]
    fn the_explicit_override_is_consulted_first() {
        assert_eq!(CLI_SESSION_VARS[0], "NEXMESH_CLI_SESSION");
    }

    #[test]
    fn the_two_cli_variables_we_verified_are_there() {
        assert!(CLI_SESSION_VARS.contains(&"CLAUDE_CODE_SESSION_ID"));
        assert!(CLI_SESSION_VARS.contains(&"CODEX_THREAD_ID"));
    }
}
