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
    /// What to call the machine this bridge runs on. Defaults to the
    /// hostname, and is only ever a claim -- nothing verifies it.
    #[arg(long)]
    host: Option<String>,
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

    /// A multipart POST: one part is the file, the rest are text fields.
    ///
    /// Which field carries the bytes is the manifest's business, as is what
    /// to call it -- several services sniff the audio format from the
    /// filename, and answer about "unsupported format" when it is wrong.
    async fn post_multipart(&self, c: &Capability, body: &Value) -> Result<Value> {
        let field = c
            .file_field
            .clone()
            .context("this capability is multipart but names no file field")?;
        let raw = body
            .get(&field)
            .and_then(|v| v.as_str())
            .with_context(|| format!("'{}' needs '{field}': the file, as base64", c.label))?;
        let bytes = unb64(raw)?;
        let name = c.file_name.clone().unwrap_or_else(|| "upload.bin".to_string());
        // The part's name is the SERVICE's, which is not always the asker's.
        let part_name = c.file_part.clone().unwrap_or_else(|| field.clone());
        let mut form = reqwest::multipart::Form::new().part(
            part_name,
            reqwest::multipart::Part::bytes(bytes).file_name(name),
        );
        // Everything else goes as a text part, which is what an
        // OpenAI-shaped endpoint expects beside the file.
        if let Some(obj) = body.as_object() {
            for (k, v) in obj {
                if *k == field {
                    continue;
                }
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                form = form.text(k.clone(), text);
            }
        }
        let mut req = self.http.post(format!("{}{}", self.base, c.endpoint)).multipart(form);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await?;
        self.read(&c.endpoint, res, c.binary).await
    }

    /// What came back: JSON when it is JSON, and base64 when the body is the
    /// file itself.
    async fn read(&self, path: &str, res: reqwest::Response, binary: bool) -> Result<Value> {
        let status = res.status();
        let kind = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!("{path} -> {status}: {}", text.chars().take(400).collect::<String>());
        }
        let jsonish = kind.contains("json") || kind.starts_with("text/");
        if binary && !jsonish {
            let bytes = res.bytes().await?;
            return Ok(json!({
                "audio_base64": b64(&bytes),
                "content_type": if kind.is_empty() { "audio/wav" } else { &kind },
                "bytes": bytes.len(),
            }));
        }
        let text = res.text().await.unwrap_or_default();
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.post_as(path, body, false).await
    }

    /// `binary` says a non-JSON body is the file itself rather than an error
    /// message. The service's own words are passed through on a failure:
    /// they are far more useful than a status code, and are what told us the
    /// transcription endpoint wanted a multipart file in the first place.
    async fn post_as(&self, path: &str, body: &Value, binary: bool) -> Result<Value> {
        let mut req = self.http.post(format!("{}{}", self.base, path)).json(body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await?;
        self.read(path, res, binary).await
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
    // What the install answers with when the asker did not say. A service
    // that requires a model needs one even from an agent that has no
    // opinion about which, and the manifest is where that answer belongs.
    for (field, value) in &c.defaults {
        if !body.contains_key(field) {
            body.insert(field.clone(), manifest::toml_to_json(value));
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

/// How to actually ask, written for a reader that has no idea this is not a
/// teammate it can talk to.
///
/// The bridge has no language model -- deliberately, since every agent on the
/// team already is one -- so a sentence addressed to it cannot be acted on.
/// What it does have is a list of capabilities with a documented request
/// shape, and that is exactly what `ask_capability` takes. Saying so, with
/// the fields and one worked example per capability, is the difference
/// between a capability nobody can reach and one anybody can.
fn usage_text(name: &str, m: &Manifest, available: &[Available]) -> String {
    let mut out = format!(
        "{name} is a bridge to {}, not an agent: it has no language model, so it cannot read a \
         request written as a sentence. Reach it with the ask_capability tool, which takes the \
         capability and a request object:\n\n",
        m.service.name
    );
    // Two examples first, because an example is read and a schema is skimmed.
    for a in available.iter().take(2) {
        if let Some(c) = m.find(&a.label) {
            out.push_str(&format!(
                "  ask_capability(capability=\"{}\", request={{{}}}{})\n",
                a.label,
                c.required
                    .iter()
                    .map(|f| format!("\"{f}\": \"...\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                if a.models.is_empty() {
                    String::new()
                } else {
                    format!(", model=\"{}\"", a.models[0])
                }
            ));
        }
    }
    out.push_str(&format!("\nWhat {name} can do here:\n"));
    for a in available {
        out.push_str(&format!("- {}: {}\n", a.label, a.summary));
        if let Some(c) = m.find(&a.label) {
            let also: Vec<&String> =
                c.accepts.iter().filter(|f| !c.required.contains(f)).collect();
            out.push_str(&format!(
                "    needs {}{}{}\n",
                if c.required.is_empty() {
                    "nothing".to_string()
                } else {
                    c.required.join(", ")
                },
                if also.is_empty() {
                    String::new()
                } else {
                    format!(
                        " \u{b7} also takes {}",
                        also.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    )
                },
                if a.models.is_empty() {
                    String::new()
                } else {
                    format!(" \u{b7} models {}", a.models.join(", "))
                }
            ));
        }
    }
    out.push_str(
        "\nBinary input goes in as base64, a data URI, or a path this machine can read; anything \
         large belongs in the service's own store, passed by reference. What comes back is text, \
         or a file reference -- never the bytes of a video.\n\
         An ask BLOCKS until the work is done, so give slow work a generous timeout_seconds.\n",
    );
    out
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// base64 in, bytes out, accepting what a browser and an agent actually send:
/// a data URI, URL-safe characters, and newlines through the middle.
fn unb64(s: &str) -> Result<Vec<u8>> {
    let s = s.rsplit_once("base64,").map(|(_, b)| b).unwrap_or(s);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        let v = B64
            .iter()
            .position(|c| *c == ch)
            .or(match ch {
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            })
            .context("the file field was not base64")? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Split text into pieces a fixed-context model will accept, at the places a
/// reader would pause.
///
/// Sentence ends first, then clauses, then whitespace, and a word longer than
/// the whole budget is cut rather than dropped. Nothing is thrown away and
/// nothing is reordered: this is the same text, read in instalments.
/// Cut a piece in two at the best break near the middle.
///
/// Used only when the service has refused the piece, so the split has to
/// make progress: a piece that cannot be made smaller comes back as one
/// element and the caller reports the original failure rather than looping.
pub fn halve(piece: &str) -> Vec<String> {
    let piece = piece.trim();
    let n = piece.chars().count();
    if n < 24 {
        return vec![piece.to_string()];
    }
    let mid = piece.char_indices().nth(n / 2).map(|(i, _)| i).unwrap_or(piece.len() / 2);
    let at = piece[..mid]
        .rfind(['.', '!', '?', '\n', ';', ':', ','])
        .map(|i| i + 1)
        .or_else(|| piece[..mid].rfind(char::is_whitespace))
        .unwrap_or(mid);
    let (a, b) = piece.split_at(at.max(1));
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return vec![piece.to_string()];
    }
    vec![a.to_string(), b.to_string()]
}

pub fn chunk_text(text: &str, max: usize) -> Vec<String> {
    let max = max.max(40);
    let text = text.trim();
    if text.chars().count() <= max {
        return if text.is_empty() { Vec::new() } else { vec![text.to_string()] };
    }
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.chars().count() <= max {
            out.push(rest.trim().to_string());
            break;
        }
        // The byte index of the max'th character, so a multi-byte sentence
        // is never cut through the middle of a letter.
        let limit = rest
            .char_indices()
            .nth(max)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..limit];
        let at = window
            .rfind(['.', '!', '?', '\n'])
            .map(|i| i + 1)
            .or_else(|| window.rfind([';', ':', ',']).map(|i| i + 1))
            .or_else(|| window.rfind(char::is_whitespace))
            .unwrap_or(limit);
        let (head, tail) = rest.split_at(at.max(1));
        let head = head.trim();
        if !head.is_empty() {
            out.push(head.to_string());
        }
        rest = tail.trim_start();
    }
    out
}

/// Join several wav answers into one.
///
/// Each piece is a complete RIFF file; the result keeps the first one's
/// header and appends everybody's samples, with the two length fields
/// corrected. Anything that is not a wav is returned as the first piece
/// alone rather than spliced into nonsense.
fn join_wav(parts: &[Vec<u8>]) -> Option<Vec<u8>> {
    if parts.len() == 1 {
        return Some(parts[0].clone());
    }
    let data_of = |b: &[u8]| -> Option<(usize, usize)> {
        if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return None;
        }
        let mut i = 12;
        while i + 8 <= b.len() {
            let id = &b[i..i + 4];
            let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
            if id == b"data" {
                let start = i + 8;
                return Some((start, len.min(b.len().saturating_sub(start))));
            }
            i += 8 + len + (len % 2);
        }
        None
    };
    let (first_start, first_len) = data_of(parts.first()?)?;
    let mut out = parts[0][..first_start + first_len].to_vec();
    for p in &parts[1..] {
        let (s, l) = data_of(p)?;
        out.extend_from_slice(&p[s..s + l]);
    }
    // RIFF size (everything after the first 8 bytes) and the data chunk size.
    let riff = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    let data_len = (out.len() - first_start) as u32;
    out[first_start - 4..first_start].copy_from_slice(&data_len.to_le_bytes());
    Some(out)
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
    /// Teammates who have addressed this bridge in prose and are owed an
    /// answer. A message cannot be acted on here, but being acked and
    /// otherwise ignored is indistinguishable from the fleet not working.
    prose: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// This bridge's own seat, so a message addressed to it can be told
    /// apart from one said to the room.
    slot_id: String,
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
                let from = m["from_name"].as_str().unwrap_or("");
                let body = m["body"].as_str().unwrap_or("");
                // A BROADCAST is not a question for the bridge. It used to
                // be treated as one, so every line said to the team came
                // back as the capability sheet, once per sender -- which on
                // a busy channel is a storm, and on a restart starts again
                // because the "told them recently" map lives in memory.
                //
                // So the sheet is sent only when somebody has actually
                // addressed this bridge AND is asking what it can do, or
                // has tried to ask for something and got the shape wrong.
                // Everything else is acknowledged in silence: an ack is
                // what stops a message being handed out for ever, and it
                // costs nobody a line in the channel.
                let addressed = m["to_slot"].as_str().is_some_and(|t| t == self.slot_id);
                let wants_help = addressed && asking_for_help(body);
                tracing::info!(
                    from = %if from.is_empty() { "someone" } else { from },
                    addressed, wants_help,
                    "a message for the bridge; acknowledging it"
                );
                // The operator has no slot and is answered in the chat by a
                // person reading it; a teammate is answered directly.
                if wants_help && !from.is_empty() && from != "operator" {
                    if let Ok(mut q) = self.prose.lock() {
                        q.push(from.to_string());
                    }
                }
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
        // WHERE THIS RUNS. A bridge has no node row and no process the hub
        // can see, so without this the fleet can only say it is not a
        // machine -- and a service on a known host then appears under
        // "participants with no machine", which reads as nowhere.
        "host": args.host.clone().unwrap_or_else(default_host),
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
    // (agent, session, seat). The SEAT is how the bridge later tells a
    // message addressed to it from one said to the room.
    let seats: Vec<(String, String, String)> = match joined["seats"].as_array() {
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
                    v["slot_id"].as_str().unwrap_or("").to_string(),
                ))
            })
            .collect(),
        _ => vec![(
            joined["agent_id"]
                .as_str()
                .with_context(|| format!("the hub refused the join: {joined}"))?
                .to_string(),
            joined["session_id"].as_str().unwrap_or("?").to_string(),
            joined["slot_id"].as_str().unwrap_or("").to_string(),
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
        teams = %seats.iter().map(|(_, s, _)| s.clone()).collect::<Vec<_>>().join(", "),
        "joined"
    );

    let hubs: Vec<Hub> = seats
        .iter()
        .map(|(agent_id, _, slot_id)| Hub {
            http: http.clone(),
            base: base.clone(),
            token: args.token.clone(),
            agent_id: agent_id.clone(),
            prose: Default::default(),
            slot_id: slot_id.clone(),
        })
        .collect();


    // Say what is on offer AND how to ask for it, so teammates can read it
    // rather than guess -- a capability nobody knows the request shape of is
    // a capability nobody uses.
    let usage = usage_text(&args.name, &m, &available);
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
    lines.push_str("Reach these with ask_capability.\n\n");
    lines.push_str(&usage);
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

    serve(&hubs, &svc, &m, &available, args.max_inline_bytes, &args.name).await
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
    name: &str,
) -> Result<()> {
    let mut idle = 0u32;
    // Who has been told how to ask, and when. Told once and then left alone
    // for a while: an agent that keeps talking to the bridge in prose should
    // get the instructions, not a conversation with a wall.
    let mut told: BTreeMap<String, std::time::Instant> = BTreeMap::new();
    // A TERSE nudge, not the catalogue. A sentence addressed to a bridge gets
    // one line that says what it is and how to ask -- re-dumping the whole
    // ~2KB capability sheet into a shared room every time somebody speaks to
    // it in prose (four verbatim copies in one thread, on the live fleet)
    // buries real conversation in boilerplate. The full sheet, with every
    // request shape, is stored once in team memory at startup; this points
    // there rather than repeating it.
    let nudge = format!(
        "{name} is a bridge to {svc}, not an agent -- it has no language model, so it          cannot act on a message written as a sentence. Ask it with the ask_capability          tool: ask_capability(capability=\"...\", request={{...}}). Its {n} capabilities          and their request fields are listed in team memory -- recall \"{svc}-capabilities\".",
        name = name,
        svc = m.service.name,
        n = available.len(),
    );
    loop {
        answer_prose(hubs, &nudge, &mut told).await;
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

/// Is this somebody asking what the bridge can do, or getting an ask wrong?
///
/// A bridge has no model and cannot hold a conversation, so the only useful
/// thing it can say in prose is how to ask it properly -- and the only time
/// that is worth saying is when somebody is trying. Everything else said to
/// it is acknowledged and left alone.
fn asking_for_help(body: &str) -> bool {
    let b = body.to_lowercase();
    // Asking outright.
    const ASKING: &[&str] = &[
        "what can you", "what do you", "capabilit", "abilities", "/help", "help?",
        "how do i ask", "how should i ask", "usage", "what are you for",
        "can you do", "are you able",
    ];
    if ASKING.iter().any(|k| b.contains(k)) {
        return true;
    }
    // Or trying to ask for something it serves, in prose rather than with
    // `ask_capability` -- which is the other moment a correction helps.
    const WORK: &[&str] = &[
        "ocr", "transcrib", "diariz", "speech", "text to speech", "tts",
        "embed", "rerank", "moderat", "subtitle", "upscale", "image",
    ];
    const TRYING: &[&str] = &["can you", "could you", "please", "i need", "we need", "?"];
    WORK.iter().any(|k| b.contains(k)) && TRYING.iter().any(|k| b.contains(k))
}

/// Answer anybody who has spoken to the bridge in prose with how to ask it
/// properly. Once per sender per ten minutes, which is often enough to be
/// useful and rare enough not to be a loop of its own.
async fn answer_prose(
    hubs: &[Hub],
    usage: &str,
    told: &mut BTreeMap<String, std::time::Instant>,
) {
    const QUIET: std::time::Duration = std::time::Duration::from_secs(600);
    for hub in hubs {
        let waiting: Vec<String> = match hub.prose.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => continue,
        };
        for who in waiting {
            if told.get(&who).is_some_and(|t| t.elapsed() < QUIET) {
                continue;
            }
            told.insert(who.clone(), std::time::Instant::now());
            if let Err(e) = hub
                .tool("send_message", json!({ "to": who, "text": usage }))
                .await
            {
                tracing::warn!(%who, error = %e, "could not say how to ask");
            }
        }
    }
    // A name told long enough ago is forgotten, so the map cannot grow for
    // ever on a long-running bridge.
    told.retain(|_, t| t.elapsed() < QUIET * 2);
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

    // Too long for the model behind it? Say it in instalments and join the
    // sound back together. A teammate asking for a paragraph to be read
    // should not have to know this model's context is 510 phonemes.
    if let (Some(field), Some(max)) = (&c.chunk_field, c.chunk_chars) {
        let whole = body.get(field).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let pieces = chunk_text(&whole, max);
        if pieces.len() > 1 {
            tracing::info!(label = %c.label, pieces = pieces.len(),
                           "splitting a long request for a fixed-context model");
            let mut audio: Vec<Vec<u8>> = Vec::new();
            let mut text: Vec<String> = Vec::new();
            // A character count is a guess at a limit measured in phonemes.
            // Dense text packs more of them into the same width, so a chunk
            // that fits one paragraph overflows the next and the service
            // answers 500 -- "index 510 is out of bounds for axis 0 with
            // size 510" -- for the whole request. Rather than pick a smaller
            // number and hope, a piece that is refused is cut in half and
            // tried again. The guess only has to be close; this makes it
            // right.
            let mut queue: std::collections::VecDeque<String> = pieces.into_iter().collect();
            while let Some(piece) = queue.pop_front() {
                let mut part = body.clone();
                part[field] = json!(piece.clone());
                let attempt = match c.encoding.as_str() {
                    "multipart" => svc.post_multipart(c, &part).await,
                    _ => svc.post_as(&c.endpoint, &part, c.binary).await,
                };
                let answer = match attempt {
                    Ok(a) => a,
                    Err(e) => {
                        let halves = halve(&piece);
                        if halves.len() < 2 {
                            return Err(e);
                        }
                        tracing::info!(label = %c.label, chars = piece.chars().count(),
                            "a piece was refused; halving it and trying again");
                        for (i, h) in halves.into_iter().enumerate() {
                            queue.insert(i, h);
                        }
                        continue;
                    }
                };
                if c.binary {
                    let found = answer
                        .get("audio_base64")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            manifest::extract_first(&answer, &c.audio_at)
                                .into_iter()
                                .find(|s| s.len() > 64)
                        });
                    match found {
                        Some(b64) => audio.push(unb64(&b64)?),
                        None => anyhow::bail!(
                            "'{}' answered one piece with no sound in it",
                            c.label
                        ),
                    }
                } else {
                    text.push(render_answer(c, &answer));
                }
            }
            if c.binary {
                let joined = join_wav(&audio)
                    .context("the pieces came back in a format that cannot be joined")?;
                return Ok(serde_json::to_string(&json!({
                    "audio_base64": b64(&joined), "content_type": "audio/wav"
                }))
                .unwrap_or_default());
            }
            return Ok(text.join("\n"));
        }
    }

    let answer = match (c.method.to_uppercase().as_str(), c.encoding.as_str()) {
        ("GET", _) => svc.get(&c.endpoint).await?,
        (_, "multipart") => svc.post_multipart(c, &body).await?,
        _ => svc.post_as(&c.endpoint, &body, c.binary).await?,
    };
    // A capability that answers with SOUND is handed over as sound. It
    // arrives one of two ways -- as the response body, which `read` has
    // already turned into `audio_base64`, or inside JSON under whatever
    // this install calls it -- and the asker gets the same shape either
    // way, because the players on the other end read one shape.
    if c.binary {
        if answer.get("audio_base64").is_some() {
            return Ok(serde_json::to_string(&answer).unwrap_or_default());
        }
        let found = manifest::extract_first(&answer, &c.audio_at);
        if let Some(b64) = found.into_iter().find(|s| s.len() > 64) {
            let kind = answer
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("audio/wav")
                .to_string();
            return Ok(serde_json::to_string(&json!({
                "audio_base64": b64, "content_type": kind
            }))
            .unwrap_or_default());
        }
    }
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

    /// A refused piece has to come back smaller, or the retry is a loop.
    #[test]
    fn halving_makes_progress_and_knows_when_to_stop() {
        let long = "One sentence here. Another sentence follows it, longer than the first.";
        let halves = halve(long);
        assert_eq!(halves.len(), 2, "{halves:?}");
        assert!(halves[0].chars().count() < long.chars().count());
        assert!(halves[1].chars().count() < long.chars().count());
        assert!(halves[0].ends_with('.'), "split at the sentence: {:?}", halves[0]);
        // Nothing left to cut: one element, so the caller reports the real
        // failure instead of halving for ever.
        assert_eq!(halve("tiny").len(), 1);
        assert_eq!(halve("   ").len(), 1);
    }

    /// Joining the halves back up must lose no words, or a paragraph read
    /// aloud quietly drops a clause in the middle.
    #[test]
    fn halving_keeps_every_word() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let halves = halve(text);
        let rejoined = halves.join(" ");
        assert_eq!(rejoined.split_whitespace().collect::<Vec<_>>(),
                   text.split_whitespace().collect::<Vec<_>>());
    }

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

    fn manifest() -> Manifest {
        Manifest::parse(
            r#"
[service]
name = "svc"
[[capability]]
label = "ocr"
summary = "read text off a page"
endpoint = "/v1/ocr"
model_field = "engine"
accepts = ["image", "lang"]
required = ["image"]
text_at = ["text"]
[[capability]]
label = "image"
summary = "make a picture"
endpoint = "/v1/image"
accepts = ["prompt", "width"]
required = ["prompt"]
file_at = ["url"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn the_usage_text_says_how_to_ask_not_just_what_is_there() {
        // A capability nobody knows the request shape of is a capability
        // nobody can use -- which is what prose at a bridge with no model
        // comes down to.
        let m = manifest();
        let available = vec![
            Available {
                label: "ocr".into(),
                summary: "read text off a page".into(),
                models: vec!["surya".into()],
            },
            Available {
                label: "image".into(),
                summary: "make a picture".into(),
                models: vec![],
            },
        ];
        let t = usage_text("CoderAI", &m, &available);
        assert!(t.contains("no language model"), "{t}");
        assert!(t.contains("ask_capability(capability=\"ocr\""), "{t}");
        assert!(t.contains("\"image\": \"...\""), "the required field must be in the example: {t}");
        assert!(t.contains("model=\"surya\""), "{t}");
        // Every capability, with what it needs and what else it takes.
        assert!(t.contains("- ocr:") && t.contains("- image:"), "{t}");
        assert!(t.contains("needs image \u{b7} also takes lang"), "{t}");
        assert!(t.contains("needs prompt \u{b7} also takes width"), "{t}");
        assert!(t.contains("timeout_seconds"), "slow work needs saying: {t}");
    }

    #[test]
    fn long_text_is_split_where_a_reader_would_pause() {
        let text = "First sentence here. Second one, with a clause; and a third part. \
                    And a final sentence to finish it off.";
        let pieces = chunk_text(text, 45);
        assert!(pieces.len() > 1, "{pieces:?}");
        for p in &pieces {
            assert!(p.chars().count() <= 45, "{p}");
        }
        // Nothing is lost and nothing is reordered.
        let back: String = pieces.join(" ");
        for word in ["First", "clause;", "third", "finish"] {
            assert!(back.contains(word), "{word} missing from {back}");
        }
        assert!(pieces[0].ends_with('.'), "it breaks at a sentence: {:?}", pieces[0]);

        // Short enough already: one piece, unchanged.
        assert_eq!(chunk_text("just this", 100), vec!["just this".to_string()]);
        assert!(chunk_text("   ", 100).is_empty());
        // A word longer than the whole budget is cut, not dropped.
        let long = "x".repeat(300);
        let pieces = chunk_text(&long, 50);
        assert!(pieces.len() >= 6);
        assert_eq!(pieces.concat().len(), 300);
    }

    #[test]
    fn several_wavs_become_one() {
        // A minimal RIFF: 44-byte header, then samples.
        let wav = |samples: &[u8]| -> Vec<u8> {
            let mut v = b"RIFF".to_vec();
            v.extend_from_slice(&((36 + samples.len()) as u32).to_le_bytes());
            v.extend_from_slice(b"WAVEfmt ");
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&[1, 0, 1, 0]); // pcm, mono
            v.extend_from_slice(&24000u32.to_le_bytes());
            v.extend_from_slice(&48000u32.to_le_bytes());
            v.extend_from_slice(&[2, 0, 16, 0]);
            v.extend_from_slice(b"data");
            v.extend_from_slice(&(samples.len() as u32).to_le_bytes());
            v.extend_from_slice(samples);
            v
        };
        let a = wav(&[1, 2, 3, 4]);
        let b = wav(&[5, 6, 7, 8]);
        let joined = join_wav(&[a.clone(), b]).unwrap();
        assert_eq!(&joined[..4], b"RIFF");
        // Both halves of the sound, and the two length fields corrected.
        assert_eq!(&joined[44..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(u32::from_le_bytes(joined[4..8].try_into().unwrap()) as usize, joined.len() - 8);
        assert_eq!(u32::from_le_bytes(joined[40..44].try_into().unwrap()), 8);
        // One piece is itself.
        assert_eq!(join_wav(&[a.clone()]).unwrap(), a);
        // Something that is not a wav is refused rather than spliced.
        assert!(join_wav(&[a, b"not a wav at all".to_vec()]).is_none());
    }

    #[test]
    fn defaults_fill_in_what_the_service_insists_on() {
        // A service that REQUIRES a model gets one even from an agent with
        // no opinion about which -- that 422 is what sent us here.
        let m = Manifest::parse(
            r#"
[service]
name = "svc"
[[capability]]
label = "transcribe"
summary = "speech to text"
endpoint = "/v1/audio/transcriptions"
encoding = "multipart"
file_field = "audio_base64"
file_name = "speech.wav"
model_field = "model"
defaults = { model = "whisper3" }
accepts = ["audio_base64", "language"]
required = ["audio_base64"]
"#,
        )
        .unwrap();
        let c = &m.capabilities[0];
        assert_eq!(c.encoding, "multipart");
        assert_eq!(c.file_field.as_deref(), Some("audio_base64"));
        let body = build_request(c, &json!({ "audio_base64": "aGk=" }), None, 1_000_000).unwrap();
        assert_eq!(body["model"], "whisper3", "the default must be sent");
        // A model the asker NAMED still wins over the default -- that is
        // what the `model` argument of ask_capability is for.
        let body = build_request(c, &json!({ "audio_base64": "aGk=" }), Some("canary"),
                                 1_000_000).unwrap();
        assert_eq!(body["model"], "canary");
    }

    #[test]
    fn base64_round_trips_through_the_bridge() {
        for n in [0usize, 1, 2, 3, 100, 257] {
            let data: Vec<u8> = (0..n).map(|i| (i * 11 % 251) as u8).collect();
            assert_eq!(unb64(&b64(&data)).unwrap(), data, "{n}");
        }
        // What a browser and an agent actually send.
        assert_eq!(unb64("data:audio/wav;base64,aGVsbG8=").unwrap(), b"hello");
        assert_eq!(unb64("aGVs\nbG8=").unwrap(), b"hello");
        assert!(unb64("not base64!!").is_err());
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

/// What to call this machine when nobody said.
///
/// `hostname` is what an operator recognises, and it is what the fleet's
/// nodes are named after, so a bridge beside one of them lines up with it.
/// Falling back to the empty string rather than inventing a name: the hub
/// treats a blank as "did not say", which is honest, where a guess like
/// "unknown" would look like an answer.
fn default_host() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.trim().is_empty()))
        .unwrap_or_default()
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn it_names_this_machine_or_says_nothing() {
        let h = default_host();
        // Either a real name, or empty -- never a word that looks like one.
        assert!(h.is_empty() || !h.contains(char::is_whitespace), "{h:?}");
        assert_ne!(h, "unknown", "a guess must not look like an answer");
    }
}
