// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! nexmesh-coderai -- a bridge that gives a team senses it does not have.
//!
//! Agents are language models. They cannot hear an audio file, read a scanned
//! page, or look at a diagram. A service that can do those things is therefore
//! worth more to a team than another model, and this joins the team as an
//! ordinary participant offering exactly that: OCR, transcription, speech,
//! embeddings, images, video.
//!
//! ## Why a separate process
//!
//! It holds the service credential, and nothing else in nexmesh does. The MCP
//! shim deliberately holds none so that a compromised agent gains no reach;
//! putting a service token into the hub or a node would undo that for every
//! machine in the fleet. Here it lives in one process that can do nothing but
//! answer requests for the capabilities it advertises.
//!
//! ## What it will not do
//!
//! Text generation. Agents already are language models, so routing their
//! thinking through another one is a loop with a bill attached. The chat and
//! completion endpoints are deliberately absent from the manifest.
//!
//! ## Discovery, not assumption
//!
//! The service's own published capability document is incomplete -- it omits
//! whole subsystems that are registered and serving -- and its written
//! documentation is further out of date still. So capabilities are *probed*:
//! what answers is what gets advertised, and the models each one offers come
//! from the install rather than from a list compiled here.

mod manifest;

use anyhow::{Context, Result};
use clap::Parser;
use manifest::{Capability, Manifest};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "nexmesh-coderai", version, about = "nexmesh bridge to a media/OCR service")]
struct Args {
    /// The nexmesh hub.
    #[arg(long, env = "NEXMESH_HUB_HTTP", default_value = "http://127.0.0.1:7870")]
    hub: String,
    /// A participant token, scoped to the session this should join.
    #[arg(long, env = "NEXMESH_PARTICIPANT_TOKEN")]
    token: String,
    /// Session to join. Omitted means the only active one.
    ///
    /// Repeatable, and `--session all` joins every active session: a bridge
    /// serves a capability, and OCR is OCR whoever is asking. An agent's
    /// `ask_capability` only reaches providers in its OWN session, so a
    /// bridge seated in one team is invisible to the rest -- which was the
    /// whole reason this takes a list.
    ///
    /// Resolved once, at startup. A session created later needs a restart of
    /// the bridge to be served; that is a second or two, and it keeps this
    /// from quietly re-seating itself on a timer.
    #[arg(long, env = "NEXMESH_SESSION")]
    session: Vec<String>,
    /// What teammates will call it.
    #[arg(long, default_value = "CoderAI")]
    name: String,
    #[arg(long, default_value = "media and document services")]
    role: String,
    /// The service's base URL.
    #[arg(long, env = "CODERAI_URL", default_value = "http://127.0.0.1:8000")]
    service: String,
    /// Bearer token for the service. Never logged.
    #[arg(long, env = "CODERAI_TOKEN")]
    service_token: Option<String>,
    /// Capability manifest.
    /// PEM holding the hub's certificate, or its CA, for a self-signed hub.
    ///
    /// Without this the bridge could not reach a hub that uses TLS at all --
    /// which is every hub reachable off its own machine. It is the one
    /// component that had no way to trust the fleet CA, so on a secured
    /// fleet it could only ever fail the handshake.
    #[arg(long, env = "NEXMESH_CA_CERT")]
    ca_cert: Option<String>,
    /// This bridge's own client certificate, when the hub requires mutual
    /// TLS. Its name must match the participant label, as for any operator.
    #[arg(long, env = "NEXMESH_CLIENT_CERT")]
    client_cert: Option<String>,
    #[arg(long, env = "NEXMESH_CLIENT_KEY")]
    client_key: Option<String>,
    #[arg(long, env = "NEXMESH_CAPABILITIES", default_value = "capabilities/coderai.toml")]
    manifest: String,
    /// Report what it would advertise, then exit. For checking an install
    /// without joining a team.
    #[arg(long)]
    probe_only: bool,
    /// Largest base64 payload accepted inline, in bytes. Anything bigger
    /// should be uploaded to the service and passed by reference instead.
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_inline_bytes: usize,
}

/// A capability that is actually present, and what it can be asked for.
struct Available {
    label: String,
    summary: String,
    models: Vec<String>,
}

struct Service {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Service {
    async fn get(&self, path: &str) -> Result<Value> {
        let (status, body) = self.raw_get(path).await?;
        if !status.is_success() {
            anyhow::bail!("{path} -> {status}");
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn raw_get(&self, path: &str) -> Result<(reqwest::StatusCode, String)> {
        let mut req = self.http.get(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await?;
        let status = res.status();
        Ok((status, res.text().await.unwrap_or_default()))
    }

    /// Does this route exist, without doing any work?
    ///
    /// A GET at a POST-only route is answered 405 by the framework serving
    /// this API, and 404 when the route is not registered at all -- so the
    /// wrong method is a read-only existence check. It matters because
    /// several capabilities (embeddings, rerank, diarization) have no listing
    /// endpoint of their own AND are missing from the service's capability
    /// document, so without this they would be invisible despite working.
    async fn route_exists(&self, path: &str) -> Option<bool> {
        let (status, _) = self.raw_get(path).await.ok()?;
        match status.as_u16() {
            // Registered, called wrongly on purpose.
            405 | 422 | 400 => Some(true),
            404 | 501 => Some(false),
            // 2xx means it answers GET too; anything else (401, 5xx) is not
            // evidence either way.
            s if (200..300).contains(&s) => Some(true),
            _ => None,
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let mut req = self.http.post(format!("{}{}", self.base, path)).json(body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            // The service's own message is far more useful than our status
            // code, so it is passed through to the asking agent.
            anyhow::bail!(
                "{path} -> {status}: {}",
                text.chars().take(400).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

/// Work out what this install can actually do.
async fn discover(svc: &Service, m: &Manifest) -> Vec<Available> {
    // The service's own document, where it has one. Treated as a hint: it is
    // known to omit subsystems that are present.
    let mut listed: Vec<String> = Vec::new();
    if let Some(path) = &m.service.capability_document {
        match svc.get(path).await {
            Ok(doc) => {
                for p in &m.service.document_endpoints {
                    listed.extend(manifest::extract(&doc, p));
                }
                tracing::info!(endpoints = listed.len(), "read the service capability document");
            }
            Err(e) => tracing::warn!(error = %e, "no capability document; probing only"),
        }
    }
    // Models the service is serving, for capabilities with no probe of their
    // own.
    let served: Vec<String> = match &m.service.models_endpoint {
        Some(p) => match svc.get(p).await {
            Ok(v) => manifest::extract_first(&v, &m.service.models_at),
            Err(e) => {
                tracing::warn!(error = %e, "could not list served models");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let mut out = Vec::new();
    for c in &m.capabilities {
        let mut models: Vec<String> = Vec::new();
        let mut probed = false;
        if let Some(probe) = &c.probe {
            match svc.get(probe).await {
                Ok(v) => {
                    probed = true;
                    models = manifest::extract_first(&v, &c.models_at);
                }
                Err(e) => tracing::debug!(label = %c.label, error = %e, "probe did not answer"),
            }
        }
        // Listed in the document counts as present, unless the manifest says
        // the probe is the only acceptable evidence.
        let in_document = listed.iter().any(|e| {
            let e = e.trim_start_matches('/');
            let want = c.endpoint.trim_start_matches('/');
            e == want
        });
        // Last resort: ask the route itself whether it exists. This is what
        // rescues the capabilities that have neither a listing endpoint nor a
        // mention in the document.
        let route = if probed || (in_document && !c.probe_required) {
            None
        } else if c.probe_required {
            None
        } else {
            svc.route_exists(&c.endpoint).await
        };
        if !probed && !(in_document && !c.probe_required) && route != Some(true) {
            tracing::info!(label = %c.label, "not available on this install");
            continue;
        }
        if models.is_empty() && !served.is_empty() {
            // No probe of its own: offer nothing rather than offer the whole
            // model list, most of which cannot do this job.
            tracing::debug!(label = %c.label, "no model list for this capability");
        }
        models.sort();
        models.dedup();
        tracing::info!(label = %c.label, models = models.len(), probed,
            by_route = route == Some(true), "available");
        out.push(Available {
            label: c.label.clone(),
            summary: c.summary.clone(),
            models,
        });
    }
    out
}

/// Build the request the service wants from the request the agent sent.
fn build_request(
    c: &Capability,
    asked: &Value,
    model: Option<&str>,
    max_inline: usize,
) -> Result<Value> {
    let mut body = BTreeMap::new();
    for field in &c.accepts {
        if let Some(v) = asked.get(field) {
            if let Some(s) = v.as_str() {
                // A base64 payload is passed straight through -- the service
                // takes data URIs and bare base64 natively -- but not an
                // unbounded one: that belongs in the service's own blob store,
                // passed by reference.
                if s.len() > max_inline {
                    anyhow::bail!(
                        "'{field}' is {} bytes, over the {max_inline}-byte inline limit. Upload \
                         it to the service and pass the reference instead.",
                        s.len()
                    );
                }
            }
            body.insert(field.clone(), v.clone());
        }
    }
    for field in &c.required {
        if !body.contains_key(field) {
            anyhow::bail!(
                "'{}' needs '{field}'. It accepts: {}.",
                c.label,
                c.accepts.join(", ")
            );
        }
    }
    if let (Some(f), Some(m)) = (&c.model_field, model) {
        body.insert(f.clone(), json!(m));
    }
    Ok(Value::Object(body.into_iter().collect()))
}

/// Turn the service's answer into something an agent can read.
fn render_answer(c: &Capability, v: &Value) -> String {
    let text = manifest::extract_first(v, &c.text_at);
    let files = manifest::extract_first(v, &c.file_at);
    let mut out = String::new();
    if !text.is_empty() {
        out.push_str(&text.join("\n"));
    }
    if !files.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        // A reference, never the bytes: a produced video has no business
        // travelling through a message.
        out.push_str(&format!("files: {}", files.join(", ")));
    }
    if out.is_empty() {
        // Nothing matched, so hand back what came rather than claim success
        // with an empty answer.
        let raw = serde_json::to_string(v).unwrap_or_default();
        out = format!(
            "no text or file field matched; the service answered: {}",
            raw.chars().take(1500).collect::<String>()
        );
    }
    out
}

#[derive(Clone)]
struct Hub {
    http: reqwest::Client,
    base: String,
    token: String,
    agent_id: String,
}

impl Hub {
    async fn tool(&self, tool: &str, args: Value) -> Result<Value> {
        let v: Value = self
            .http
            .post(format!("{}/v1/participant/tool", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "agent_id": self.agent_id, "tool": tool, "args": args }))
            .send()
            .await?
            .json()
            .await?;
        // Anything handed back with the result is a message addressed to
        // this bridge, taken in flight and waiting for an ack. A bridge
        // cannot act on one -- it answers asks, not conversation -- but it
        // must say it arrived: unacked, it is handed out again every couple
        // of minutes for ever, and the fleet looks like it is ignoring
        // somebody.
        if let Some(ids) = v["messages"].as_array().filter(|a| !a.is_empty()) {
            let ids: Vec<i64> = ids.iter().filter_map(|m| m["id"].as_i64()).collect();
            for m in v["messages"].as_array().into_iter().flatten() {
                tracing::info!(
                    from = %m["from_name"].as_str().unwrap_or("someone"),
                    body = %m["body"].as_str().unwrap_or(""),
                    "a message for the bridge; acknowledging it"
                );
            }
            if !ids.is_empty() {
                let _ = self
                    .http
                    .post(format!("{}/v1/participant/ack", self.base))
                    .bearer_auth(&self.token)
                    .json(&json!({ "agent_id": self.agent_id, "message_ids": ids }))
                    .send()
                    .await;
            }
        }
        Ok(v)
    }
}

/// The client that talks to the HUB, which on any fleet reachable off one
/// machine means TLS against a private CA, and often a client certificate
/// too. The service behind the bridge is a different matter: it is local,
/// and its client is built plainly.
fn hub_client(args: &Args) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60));
    if let Some(path) = args.ca_cert.as_deref() {
        let pem = std::fs::read(path).with_context(|| format!("reading CA file {path}"))?;
        b = b.add_root_certificate(
            reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parsing {path} as a PEM certificate"))?,
        );
    }
    match (args.client_cert.as_deref(), args.client_key.as_deref()) {
        (Some(cert), Some(key)) => {
            // reqwest wants one PEM holding both.
            let mut pem = std::fs::read(cert)
                .with_context(|| format!("reading client certificate {cert}"))?;
            pem.push(b'\n');
            pem.extend(std::fs::read(key).with_context(|| format!("reading client key {key}"))?);
            b = b.identity(
                reqwest::Identity::from_pem(&pem)
                    .context("the client certificate and key could not be loaded together")?,
            );
        }
        (None, None) => {}
        _ => anyhow::bail!("--client-cert and --client-key go together, or neither"),
    }
    Ok(b.build()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let text = std::fs::read_to_string(&args.manifest)
        .with_context(|| format!("reading {}", args.manifest))?;
    let m = Manifest::parse(&text).with_context(|| format!("parsing {}", args.manifest))?;

    let svc = Service {
        http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(600)).build()?,
        base: args.service.trim_end_matches('/').to_string(),
        token: args.service_token.clone(),
    };
    if args.service_token.is_none() {
        tracing::warn!(
            "no service token: every endpoint but the health check will refuse this bridge. \
             Set CODERAI_TOKEN."
        );
    }

    let available = discover(&svc, &m).await;
    if available.is_empty() {
        anyhow::bail!(
            "nothing on {} answered any probe, so there is nothing to offer a team. Check the \
             URL and the token.",
            args.service
        );
    }
    println!("{} capability(ies) on {}:", available.len(), args.service);
    for a in &available {
        println!(
            "  {:<12} {}{}",
            a.label,
            a.summary,
            if a.models.is_empty() {
                " [service default model]".to_string()
            } else {
                format!(" [models: {}]", a.models.join(", "))
            }
        );
    }
    if args.probe_only {
        return Ok(());
    }

    // Join as an ordinary participant, advertising what was found.
    let http = hub_client(&args).context("building the client that talks to the hub")?;
    let base = args.hub.trim_end_matches('/').to_string();
    let mut join = json!({
        "name": args.name,
        "role": args.role,
        "kind_hint": format!("{} bridge {VERSION}", m.service.name),
        "capabilities": available.iter().map(|a| a.label.clone()).collect::<Vec<_>>(),
    });
    // One session is sent the way it always was, so an older hub still
    // understands this bridge. Several -- or `all` -- go as a list, which is
    // the shape that hub answers with a seat per team.
    match args.session.len() {
        0 => {}
        1 => join["session"] = json!(args.session[0]),
        _ => join["sessions"] = json!(args.session),
    }
    if args.session.iter().any(|s| s == "all" || s == "*") {
        join["sessions"] = json!(["*"]);
        join.as_object_mut().map(|o| o.remove("session"));
    }
    let joined: Value = http
        .post(format!("{base}/v1/participant/join"))
        .bearer_auth(&args.token)
        .json(&join)
        .send()
        .await?
        .json()
        .await?;
    // `seats` when the hub seated it in several teams, and the single-session
    // fields otherwise -- which is also what an older hub answers with.
    let seats: Vec<(String, String)> = match joined["seats"].as_array() {
        Some(list) if !list.is_empty() => list
            .iter()
            .filter_map(|v| {
                Some((
                    v["agent_id"].as_str()?.to_string(),
                    v["session"]
                        .as_str()
                        .or_else(|| v["session_id"].as_str())
                        .unwrap_or("?")
                        .to_string(),
                ))
            })
            .collect(),
        _ => vec![(
            joined["agent_id"]
                .as_str()
                .with_context(|| format!("the hub refused the join: {joined}"))?
                .to_string(),
            joined["session_id"].as_str().unwrap_or("?").to_string(),
        )],
    };
    for r in joined["refused"].as_array().into_iter().flatten() {
        tracing::warn!(
            session = %r["session"].as_str().unwrap_or("?"),
            reason = %r["error"].as_str().unwrap_or("?"),
            "could not join that session; carrying on with the others"
        );
    }
    tracing::info!(
        seats = seats.len(), capabilities = available.len(),
        teams = %seats.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>().join(", "),
        "joined"
    );

    let hubs: Vec<Hub> = seats
        .iter()
        .map(|(agent_id, _)| Hub {
            http: http.clone(),
            base: base.clone(),
            token: args.token.clone(),
            agent_id: agent_id.clone(),
        })
        .collect();


    // Say what is on offer, so teammates can read it rather than guess.
    let mut lines = format!("{} is available for:\n", args.name);
    for a in &available {
        lines.push_str(&format!(
            "- {}: {}{}\n",
            a.label,
            a.summary,
            if a.models.is_empty() {
                String::new()
            } else {
                format!(" (models: {})", a.models.join(", "))
            }
        ));
    }
    lines.push_str("Reach these with ask_capability.\n");
    // Every team is told, not just the first: a seat whose summary is blank
    // reads like something that has not started.
    let summary = format!(
        "{} capabilities ready: {}",
        available.len(),
        available.iter().map(|a| a.label.clone()).collect::<Vec<_>>().join(", ")
    );
    for h in &hubs {
        let _ = h
            .tool("store_memory", json!({ "key": format!("{}-capabilities", m.service.name),
                                          "value": lines, "category": "context" }))
            .await;
        let _ = h
            .tool("set_summary", json!({ "summary": summary, "status": "in_progress" }))
            .await;
    }

    serve(&hubs, &svc, &m, &available, args.max_inline_bytes).await
}

/// Answer requests until stopped, for every seat this bridge holds.
///
/// A seat per team, polled in turn. The asks of one team are invisible to
/// another -- `pending_asks` is answered for the seat that calls it -- so
/// this is a loop over seats rather than anything cleverer, and the service
/// behind it is shared the way it already was.
async fn serve(
    hubs: &[Hub],
    svc: &Service,
    m: &Manifest,
    available: &[Available],
    max_inline: usize,
) -> Result<()> {
    let mut idle = 0u32;
    loop {
        let mut asks: Vec<(String, String, Value, Option<String>, &Hub)> = Vec::new();
        for hub in hubs {
            let waiting = hub.tool("pending_asks", json!({})).await;
            let text = waiting
                .as_ref()
                .ok()
                .and_then(|v| v["text"].as_str())
                .unwrap_or("")
                .to_string();
            for (id, label, request, model) in parse_pending(&text) {
                asks.push((id, label, request, model, hub));
            }
        }

        if asks.is_empty() {
            idle = (idle + 1).min(10);
            // Back off while nothing is happening, so an idle bridge is not a
            // busy loop on somebody's hub. Divided by the number of seats, so
            // four teams are not polled four times as often as one.
            let n = hubs.len().max(1) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(
                (300 + 200 * idle as u64) * n,
            ))
            .await;
            continue;
        }
        idle = 0;

        for (id, label, request, model, hub) in asks {
            let outcome = handle(svc, m, available, &label, &request, model.as_deref(), max_inline)
                .await;
            let (result, failed) = match outcome {
                Ok(text) => (text, false),
                Err(e) => {
                    tracing::warn!(%id, %label, error = %e, "request failed");
                    (format!("{e:#}"), true)
                }
            };
            if let Err(e) = hub
                .tool("answer_ask", json!({ "ask_id": id, "result": result, "failed": failed }))
                .await
            {
                tracing::error!(%id, error = %e, "could not return the answer");
            }
        }
    }
}

/// One request, end to end.
async fn handle(
    svc: &Service,
    m: &Manifest,
    available: &[Available],
    label: &str,
    request: &Value,
    model: Option<&str>,
    max_inline: usize,
) -> Result<String> {
    let Some(c) = m.find(label) else {
        anyhow::bail!("'{label}' is not a capability of this bridge");
    };
    let Some(have) = available.iter().find(|a| a.label == c.label) else {
        anyhow::bail!("'{label}' is in the manifest but not installed on this service");
    };
    // A model the install does not have would be a confusing error from the
    // service; refused here, with what it does have.
    if let Some(want) = model {
        if !have.models.is_empty() && !have.models.iter().any(|m| m == want) {
            anyhow::bail!(
                "'{label}' has no model '{want}' here. Available: {}.",
                have.models.join(", ")
            );
        }
    }
    let body = build_request(c, request, model, max_inline)?;
    let answer = match c.method.to_uppercase().as_str() {
        "GET" => svc.get(&c.endpoint).await?,
        _ => svc.post(&c.endpoint, &body).await?,
    };
    Ok(render_answer(c, &answer))
}

/// Read `pending_asks`'s rendering back into structure.
///
/// The tool answers in prose because its usual reader is a model. Parsing it
/// here keeps the bridge to the same public surface every other participant
/// uses, rather than needing a private channel of its own.
fn parse_pending(text: &str) -> Vec<(String, String, Value, Option<String>)> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("- ") else { continue };
        let Some((id, after)) = rest.split_once(' ') else { continue };
        if !id.starts_with("ask-") {
            continue;
        }
        let Some(open) = after.find('[') else { continue };
        let Some(close) = after.find(']') else { continue };
        let label = after[open + 1..close].to_string();
        let model = after[close + 1..]
            .split_whitespace()
            .find_map(|t| t.strip_prefix("model="))
            .map(|s| s.to_string());
        // The request is the indented line that follows.
        let body = lines.next().unwrap_or("").trim();
        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        out.push((id.to_string(), label, request, model));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap() -> Capability {
        let m = Manifest::parse(
            r#"
[service]
name = "svc"
[[capability]]
label = "ocr"
summary = "read text"
endpoint = "/v1/ocr"
model_field = "engine"
accepts = ["image", "lang"]
required = ["image"]
text_at = ["text", "data.text"]
file_at = ["url"]
"#,
        )
        .unwrap();
        m.capabilities.into_iter().next().unwrap()
    }

    #[test]
    fn only_declared_fields_are_passed_through() {
        // An agent inventing a field must not have it forwarded blindly.
        let asked = json!({"image": "b64", "lang": "en", "sneaky": "drop me"});
        let body = build_request(&cap(), &asked, None, 1_000_000).unwrap();
        assert_eq!(body["image"], "b64");
        assert_eq!(body["lang"], "en");
        assert!(body.get("sneaky").is_none());
    }

    #[test]
    fn a_missing_required_field_says_what_is_accepted() {
        let err = build_request(&cap(), &json!({"lang": "en"}), None, 1_000_000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs 'image'"), "{err}");
        assert!(err.contains("image, lang"), "it must list the options: {err}");
    }

    #[test]
    fn the_model_choice_lands_in_the_field_the_service_wants() {
        let body = build_request(&cap(), &json!({"image": "x"}), Some("surya"), 1_000).unwrap();
        assert_eq!(body["engine"], "surya", "named by model_field, not 'model'");
    }

    #[test]
    fn an_oversized_inline_payload_is_refused_with_the_alternative() {
        // base64 is passed through, but a 200MB video has no business inside
        // a request body.
        let big = "A".repeat(2048);
        let err = build_request(&cap(), &json!({"image": big}), None, 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("over the"), "{err}");
        assert!(err.contains("pass the reference"), "it must say what to do: {err}");
    }

    #[test]
    fn a_payload_at_the_limit_is_accepted() {
        let ok = "A".repeat(1024);
        assert!(build_request(&cap(), &json!({"image": ok}), None, 1024).is_ok());
    }

    #[test]
    fn the_answer_is_read_from_the_first_matching_path() {
        let c = cap();
        assert_eq!(render_answer(&c, &json!({"text": "hello"})), "hello");
        assert_eq!(render_answer(&c, &json!({"data": {"text": "nested"}})), "nested");
    }

    #[test]
    fn a_produced_file_comes_back_as_a_reference_not_bytes() {
        let out = render_answer(&cap(), &json!({"url": "/v1/files/a.png"}));
        assert!(out.contains("files: /v1/files/a.png"), "{out}");
    }

    #[test]
    fn an_unrecognised_answer_is_handed_over_rather_than_called_success() {
        // Claiming success with an empty answer is the one thing that would
        // make this untrustworthy.
        let out = render_answer(&cap(), &json!({"surprise": "shape"}));
        assert!(out.contains("no text or file field matched"), "{out}");
        assert!(out.contains("surprise"), "and it includes what came: {out}");
    }

    #[test]
    fn pending_asks_prose_parses_back_into_requests() {
        let text = "2 request(s) waiting on you:\n\
                    - ask-abc123 [ocr] model=surya\n  {\"image\":\"x\"}\n\
                    - ask-def456 [transcribe]\n  {\"file\":\"y\"}\n\
                    Answer each with answer_ask; the asker is blocked until you do.\n";
        let got = parse_pending(text);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].0, "ask-abc123");
        assert_eq!(got[0].1, "ocr");
        assert_eq!(got[0].2["image"], "x");
        assert_eq!(got[0].3.as_deref(), Some("surya"));
        assert_eq!(got[1].1, "transcribe");
        assert!(got[1].3.is_none(), "no model named");
    }

    #[test]
    fn nothing_waiting_parses_to_nothing() {
        assert!(parse_pending("Nothing is waiting on you.").is_empty());
        assert!(parse_pending("").is_empty());
    }

    #[test]
    fn a_line_that_is_not_an_ask_is_ignored() {
        assert!(parse_pending("- not-an-ask [ocr]\n  {}").is_empty());
    }
}
