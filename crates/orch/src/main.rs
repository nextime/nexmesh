// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! `nexmesh-orch` -- the orchestrator, as an MCP server.
//!
//! Running a fleet should not require a particular CLI. This is a plain MCP
//! stdio server, so ANY MCP-capable client can load it -- Claude Code, Codex,
//! opencode, qwen, Gemini, or a program of your own -- and spin up teams from
//! whatever session you happen to be in.
//!
//! It holds an operator token and talks to the hub over HTTP. That is the one
//! real difference from `nexmesh-mcpd`, which deliberately holds no
//! credentials: these tools can start processes on every machine in the fleet.
//!
//! stdout carries JSON-RPC only; diagnostics go to stderr.

mod tools;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SUPPORTED_MCP: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const PREFERRED_MCP: &str = "2024-11-05";

#[derive(Parser)]
#[command(name = "nexmesh-orch", version, about = "nexmesh orchestrator as an MCP server")]
struct Args {
    #[arg(long, env = "NEXMESH_HUB_HTTP", default_value = "http://127.0.0.1:7870")]
    hub: String,
    /// Operator token. These tools can spawn processes fleet-wide.
    #[arg(long, env = "NEXMESH_OPERATOR_TOKEN")]
    token: Option<String>,
    /// PEM with the hub's certificate, for a self-signed hub.
    #[arg(long, env = "NEXMESH_CA_CERT")]
    ca_cert: Option<String>,
    /// Your client certificate, when the hub requires mutual TLS.
    #[arg(long, env = "NEXMESH_CLIENT_CERT", requires = "client_key")]
    client_cert: Option<String>,
    /// Private key for --client-cert.
    #[arg(long, env = "NEXMESH_CLIENT_KEY", requires = "client_cert")]
    client_key: Option<String>,
    /// Directory holding a paired identity.
    #[arg(long, env = "NEXMESH_IDENTITY", default_value = nexmesh_proto::identity::DEFAULT_DIR)]
    identity_dir: String,
}

struct Hub {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl Hub {
    fn new(a: &Args) -> Result<Self> {
        let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
        if let Some(path) = &a.ca_cert {
            let pem = std::fs::read(path).with_context(|| format!("reading CA file {path}"))?;
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        if let (Some(cert), Some(key)) = (&a.client_cert, &a.client_key) {
            let mut pem = std::fs::read(cert)
                .with_context(|| format!("reading client certificate {cert}"))?;
            pem.push(b'\n');
            pem.extend(std::fs::read(key).with_context(|| format!("reading client key {key}"))?);
            b = b.identity(reqwest::Identity::from_pem(&pem)
                .context("the client certificate and key could not be loaded together")?);
        }
        Ok(Self {
            base: a.hub.trim_end_matches('/').to_string(),
            token: a.token.clone(),
            client: b.build()?,
        })
    }

    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn body(&self, r: reqwest::Response) -> Result<Value> {
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(v);
        }
        // A 401 is answered in terms of THIS process, not the HTTP call. The
        // hub's own message tells the caller to send an Authorization header,
        // which is useless advice for a model: it is the orchestrator that
        // holds the credential, and the fix is an environment variable.
        if status.as_u16() == 401 {
            anyhow::bail!(
                "{} Fix it where the orchestrator runs: set NEXMESH_OPERATOR_TOKEN (mint one \
                 on the hub with `nexmesh-hub operator-token <label>`, then re-register with \
                 `nexmesh install-orchestrator --cli <your cli> --token <token>`). Nothing you \
                 can do from this conversation will help.",
                if self.token.is_some() {
                    "The hub rejected this orchestrator's operator token."
                } else {
                    "This orchestrator has no operator token, and the hub requires one."
                }
            )
        }
        let msg = v["error"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| text.chars().take(200).collect());
        anyhow::bail!("{msg}")
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let r = self
            .auth(self.client.get(format!("{}{path}", self.base)))
            .send()
            .await
            .with_context(|| format!("cannot reach the nexmesh hub at {}", self.base))?;
        self.body(r).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let r = self
            .auth(self.client.post(format!("{}{path}", self.base)).json(&body))
            .send()
            .await
            .with_context(|| format!("cannot reach the nexmesh hub at {}", self.base))?;
        self.body(r).await
    }

    /// Most tools take an optional session; default to the most recent so the
    /// caller does not have to carry an id around in conversation.
    async fn resolve_session(&self, given: Option<&str>) -> Result<String> {
        if let Some(s) = given.filter(|s| !s.is_empty()) {
            return Ok(s.to_string());
        }
        let v = self.get("/v1/sessions").await?;
        v["sessions"][0]["id"]
            .as_str()
            .map(String::from)
            .context("there are no sessions yet; create one with create_team")
    }
}

fn arg_str(a: &Value, k: &str) -> Option<String> {
    a.get(k).and_then(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn need(a: &Value, k: &str) -> Result<String> {
    arg_str(a, k).ok_or_else(|| anyhow::anyhow!("missing required argument '{k}'"))
}

async fn call_tool(hub: &Hub, name: &str, a: &Value) -> Result<String> {
    match name {
        "list_nodes" => {
            let v = hub.get("/v1/nodes").await?;
            let empty = vec![];
            let nodes = v["nodes"].as_array().unwrap_or(&empty);
            if nodes.is_empty() {
                return Ok("No nodes are enrolled. On each machine run `nexmesh-hub enrol <name>` \
                           on the hub, then start `nexmesh-node` there."
                    .into());
            }
            let mut out = format!("{} node(s):\n", nodes.len());
            for n in nodes {
                out.push_str(&format!(
                    "- {} [{}] {}/{} — CLIs: {}\n",
                    n["name"].as_str().unwrap_or("?"),
                    if n["connected"].as_bool() == Some(true) { "connected" } else { "offline" },
                    n["os"].as_str().unwrap_or("?"),
                    n["arch"].as_str().unwrap_or("?"),
                    n["clis"].as_array().map(|c| c.iter().filter_map(|x| x.as_str())
                        .collect::<Vec<_>>().join(", ")).unwrap_or_default(),
                ));
            }
            Ok(out)
        }

        "list_sessions" => {
            let v = hub.get("/v1/sessions").await?;
            let empty = vec![];
            let rows = v["sessions"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                return Ok("No sessions yet.".into());
            }
            Ok(rows
                .iter()
                .map(|s| {
                    format!(
                        "- {} ({}) {} — {} slot(s)",
                        s["name"].as_str().unwrap_or("?"),
                        s["id"].as_str().unwrap_or("?"),
                        s["status"].as_str().unwrap_or("?"),
                        s["slots"].as_i64().unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }

        "create_team" => create_team(hub, a).await,

        "add_agent" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let (slot, note) = add_one(hub, &sid, a).await?;
            Ok(format!("Added {} (slot {slot}). {note}", need(a, "name")?))
        }

        "team_status" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let v = hub.get(&format!("/v1/sessions/{sid}/status")).await?;
            let empty = vec![];
            let rows = v["agents"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                return Ok(format!("Session {sid} has no agents."));
            }
            let mut out = format!("Session {sid}:\n");
            for r in rows {
                out.push_str(&format!(
                    "- {} [{}] on {} — {} ({}){}\n",
                    r["name"].as_str().unwrap_or("?"),
                    r["role"].as_str().unwrap_or("-"),
                    r["host"].as_str().unwrap_or("-"),
                    r["task_state"].as_str().unwrap_or("-"),
                    r["status"].as_str().unwrap_or("-"),
                    match r["summary"].as_str().unwrap_or("") {
                        "" => String::new(),
                        s => format!("\n    {s}"),
                    }
                ));
            }
            Ok(out)
        }

        "read_log" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let limit = a["limit"].as_i64().unwrap_or(40).clamp(1, 500);
            let v = hub.get(&format!("/v1/sessions/{sid}/log?limit={limit}")).await?;
            let empty = vec![];
            let rows = v["messages"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                return Ok("No messages yet.".into());
            }
            let mut out = String::new();
            for m in rows {
                let mark = if m["delivered"].as_bool() == Some(true) {
                    " "
                } else if m["in_flight"].as_bool() == Some(true) {
                    "~"
                } else {
                    "!"
                };
                out.push_str(&format!(
                    "{mark} {} -> {} [{}] {}\n",
                    m["from"].as_str().unwrap_or("?"),
                    m["to"].as_str().unwrap_or("?"),
                    m["kind"].as_str().unwrap_or("text"),
                    m["body"].as_str().unwrap_or("")
                ));
            }
            out.push_str("\n( ! = the recipient has not seen it yet, ~ = delivery in flight )");
            Ok(out)
        }

        "message_team" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let text = need(a, "text")?;
            let mut body = json!({ "text": text });
            if let Some(to) = arg_str(a, "to") {
                body["to"] = json!(to);
            }
            hub.post(&format!("/v1/sessions/{sid}/messages"), body).await?;
            Ok(match arg_str(a, "to") {
                Some(to) => format!("Sent to {to}."),
                None => "Sent to the whole team.".into(),
            })
        }

        "pause_agents" | "resume_agents" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let mut body = json!({ "paused": name == "pause_agents" });
            if let Some(w) = arg_str(a, "who") { body["who"] = json!(w); }
            if let Some(r) = arg_str(a, "reason") { body["reason"] = json!(r); }
            let v = hub.post(&format!("/v1/sessions/{sid}/pause"), body).await?;
            Ok(v["detail"].as_str().unwrap_or("done").to_string())
        }

        "release_agents" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let mut body = json!({});
            if let Some(w) = arg_str(a, "who") { body["who"] = json!(w); }
            if let Some(n) = arg_str(a, "note") { body["note"] = json!(n); }
            let v = hub.post(&format!("/v1/sessions/{sid}/release"), body).await?;
            Ok(v["detail"].as_str().unwrap_or("done").to_string())
        }

        "adopt_terminal" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let v = hub
                .post(
                    &format!("/v1/sessions/{sid}/adopt"),
                    json!({
                        "node": need(a, "node")?,
                        "tmux_target": need(a, "tmux_target")?,
                        "name": need(a, "name")?,
                        "role": arg_str(a, "role"),
                    }),
                )
                .await?;
            Ok(format!(
                "Adopted {} on {}. Attach with: nexmesh attach {sid} {}\nNote: it is attachable, \
                 but not a team member unless the CLI inside it loads nexmesh-mcpd.",
                v["tmux_target"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?"),
                need(a, "name")?
            ))
        }

        "read_transcript" => {
            let sid = hub.resolve_session(arg_str(a, "session").as_deref()).await?;
            let who = need(a, "who")?;
            let tail = a["tail"].as_i64().unwrap_or(8000).clamp(200, 200_000);
            let r = hub.get(&format!("/v1/sessions/{sid}/resolve/{who}")).await?;
            let agent = r["agent_id"].as_str().context("no such agent")?;
            let v = hub.get(&format!("/v1/agents/{agent}/transcript?tail={tail}")).await?;
            let text = v["transcript"].as_str().unwrap_or("");
            Ok(if text.is_empty() {
                format!("No output recorded for {who}.")
            } else {
                format!("--- {who} ---\n{text}")
            })
        }

        other => anyhow::bail!("unknown tool '{other}'"),
    }
}

/// Add one agent to an existing session and start it.
async fn add_one(hub: &Hub, sid: &str, a: &Value) -> Result<(String, String)> {
    let mut body = json!({
        "cli_id": need(a, "cli")?,
        "display_name": need(a, "name")?,
        "mode": arg_str(a, "mode").unwrap_or_else(|| "headless".into()),
        "task": need(a, "task")?,
        "file_ownership": a.get("owns").cloned().unwrap_or(json!([])),
    });
    if let Some(r) = arg_str(a, "role") {
        body["role"] = json!(r);
    }
    if let Some(m) = arg_str(a, "model") {
        body["model"] = json!(m);
    }
    if let Some(c) = a.get("can").cloned() {
        body["capabilities"] = c;
    }
    if let Some(w) = arg_str(a, "workspace") {
        body["workspace"] = json!(w);
    }
    if let Some(n) = arg_str(a, "node") {
        body["placement"] = json!({ "node": n });
    } else {
        body["placement"] = json!({ "any": true });
    }
    let v = hub.post(&format!("/v1/sessions/{sid}/slots"), body).await?;
    let slot = v["slot_id"].as_str().context("hub returned no slot id")?.to_string();

    // Starting it is a separate step at the API level; from here it is one act.
    match hub.post(&format!("/v1/slots/{slot}/spawn"), json!({})).await {
        Ok(r) => Ok((
            slot,
            format!("Started on {}.", r["node"].as_str().unwrap_or("?")),
        )),
        // The slot exists and can be started later, so say so rather than
        // pretending the whole thing failed.
        Err(e) => Ok((slot, format!("NOT started: {e}"))),
    }
}

async fn create_team(hub: &Hub, a: &Value) -> Result<String> {
    let name = need(a, "name")?;
    let agents = a["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        anyhow::bail!("a team needs at least one agent");
    }

    let mut session_body = json!({ "name": name, "workspaces": [], "plan":
        a.get("plan").cloned().unwrap_or(json!([])) });
    if let Some(b) = a.get("token_budget").and_then(|v| v.as_i64()) {
        session_body["token_budget"] = json!(b);
    }
    let mut workspace_name = None;
    if let Some(w) = a.get("workspace").filter(|w| w.is_object()) {
        let wname = w["name"].as_str().unwrap_or("workspace").to_string();
        let kind = w["kind"].as_str().unwrap_or("node_local").to_string();
        let mut ws = json!({ "name": wname, "kind": kind, "mounts": {} });
        if let Some(n) = w["node"].as_str() {
            ws["node"] = json!(n);
            if let Some(p) = w["path"].as_str() {
                ws["mounts"] = json!({ n: p });
            }
        }
        if let Some(r) = w["repo"].as_str() {
            ws["repo_url"] = json!(r);
        }
        if let Some(m) = w.get("mounts").filter(|m| m.is_object()) {
            ws["mounts"] = m.clone();
        }
        session_body["workspaces"] = json!([ws]);
        workspace_name = Some(wname);
    }

    let v = hub.post("/v1/sessions", session_body).await?;
    let sid = v["session_id"].as_str().context("hub returned no session id")?.to_string();

    let mut lines = vec![format!("Created session \"{name}\" ({sid}).")];
    let mut started = 0;
    for agent in &agents {
        let mut spec = agent.clone();
        // Everyone shares the session's workspace unless told otherwise.
        if spec.get("workspace").is_none() {
            if let Some(w) = &workspace_name {
                spec["workspace"] = json!(w);
            }
        }
        let who = spec["name"].as_str().unwrap_or("agent").to_string();
        match add_one(hub, &sid, &spec).await {
            Ok((slot, note)) => {
                if note.starts_with("Started") {
                    started += 1;
                }
                lines.push(format!("- {who} ({slot}): {note}"));
            }
            // One agent failing should not hide the ones that worked.
            Err(e) => lines.push(format!("- {who}: FAILED — {e}")),
        }
    }
    lines.push(format!(
        "\n{started}/{} agent(s) running. Watch them with team_status and read_log, or open the \
         dashboard on the hub.",
        agents.len()
    ));
    Ok(lines.join("\n"))
}

async fn write_out(v: &Value) -> Result<()> {
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = Args::parse();
    // Pairing provisions the orchestrator too: one `nexmesh pair` and the
    // orchestrator in every CLI starts working.
    if let Some(id) = nexmesh_proto::identity::load(&args.identity_dir) {
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
    let hub = Hub::new(&args)?;
    tracing::info!(hub = %hub.base, authenticated = hub.token.is_some(), "orchestrator ready");

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = stdin.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_out(&rpc_err(Value::Null, -32700, format!("parse error: {e}"))).await?;
                continue;
            }
        };
        let method = req["method"].as_str().unwrap_or("").to_string();
        let Some(id) = req.get("id").cloned() else { continue };

        let response = match method.as_str() {
            "initialize" => {
                let asked = req["params"]["protocolVersion"].as_str().unwrap_or(PREFERRED_MCP);
                let version = if SUPPORTED_MCP.contains(&asked) { asked } else { PREFERRED_MCP };
                ok(id, json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "nexmesh-orchestrator", "version": VERSION },
                    "instructions":
                        "You can run a fleet of agent teams across several machines. Call \
                         list_nodes first to see which machines are connected and which agent \
                         CLIs each one has, then create_team to spin up a team. Agents work on \
                         their own and message each other; watch them with team_status and \
                         read_log. A message marked ! has not reached its recipient yet.",
                }))
            }
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, json!({
                "tools": tools::tools().iter().map(|t| json!({
                    "name": t.name, "description": t.description, "inputSchema": t.schema,
                })).collect::<Vec<_>>()
            })),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                let a = req["params"].get("arguments").cloned().unwrap_or(json!({}));
                match call_tool(&hub, &name, &a).await {
                    Ok(text) => ok(id, json!({ "content": [{ "type": "text", "text": text }] })),
                    Err(e) => ok(id, json!({
                        "content": [{ "type": "text", "text": format!("ERROR: {e:#}") }],
                        "isError": true
                    })),
                }
            }
            other => rpc_err(id, -32601, format!("method '{other}' is not supported")),
        };
        write_out(&response).await?;
    }
    Ok(())
}
