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

#[derive(Parser)]
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

    fn next_req(&mut self) -> u64 {
        self.seq += 1;
        self.seq
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

    node.send(&McpdToNode::Register {
        cli_id: args.cli.clone(),
        pid,
        cwd,
        agent_hint: args.agent_id.clone(),
        slot_hint: args.slot.clone(),
        session_hint: args.session.clone(),
        role: args.role.clone(),
        name: args.name.clone(),
        cli_session: cli_session_id(),
    })
    .await?;

    let (agent_id, slot_id, tools): (String, String, Vec<ToolDef>) = match node.recv().await? {
        NodeToMcpd::Registered { agent_id, slot_id, tools, .. } => (agent_id, slot_id, tools),
        NodeToMcpd::Error { message, .. } => anyhow::bail!("the node refused registration: {message}"),
        other => anyhow::bail!("unexpected first frame from the node: {other:?}"),
    };
    tracing::info!(%agent_id, %slot_id, tools = tools.len(), "registered with the node");

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
                            "You are part of a nexmesh team (slot {slot_id}). Teammates may run on \
                             other machines, under other CLIs, or not be CLIs at all. Call \
                             query_knowledge and check_team_status before planning. Teammate \
                             messages arrive prefixed [PENDING MESSAGES] on any tool result."
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
                let req_id = node.next_req();
                if let Err(e) = node.send(&McpdToNode::Tool { req_id, tool: name.clone(), args: arguments }).await {
                    write_stdout(&rpc_err(id, -32603, format!("node unreachable: {e}"))).await?;
                    continue;
                }
                match node.recv().await {
                    Ok(NodeToMcpd::ToolReply { result, deliver, .. }) => {
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
                            if let Err(e) = node.send(&McpdToNode::Ack { message_ids: ids }).await {
                                tracing::warn!(error = %e, "could not ack delivered messages");
                            }
                        }
                        continue;
                    }
                    Ok(NodeToMcpd::Error { message, .. }) => rpc_err(id, -32603, message),
                    Ok(other) => rpc_err(id, -32603, format!("unexpected node frame: {other:?}")),
                    Err(e) => rpc_err(id, -32603, e),
                }
            }

            // Everything else: answer politely rather than hang the host.
            other => rpc_err(id, -32601, format!("method '{other}' is not supported")),
        };
        write_stdout(&response).await?;
    }

    let _ = node.send(&McpdToNode::Bye).await;
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
