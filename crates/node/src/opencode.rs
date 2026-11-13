//! The opencode `serve` driver.
//!
//! opencode is the one agent CLI here that already has a client/server split,
//! and it is the best of the three to drive. There is no stdio protocol to
//! parse and no pty to hold: the node starts `opencode serve`, learns the URL
//! it prints, and talks to a documented HTTP API.
//!
//! Verified against opencode 1.19.5, by reading its OpenAPI document at /doc
//! and calling the running server:
//!
//! - `POST /api/session { location: { directory } }` -> `data.id`, and
//!   `data.tokens` carries `{ input, output, reasoning, cache: { read, write } }`.
//! - `POST /api/session/{id}/prompt { prompt: { text }, delivery }` where
//!   **delivery is "steer" or "queue"**. Steering mid-turn is a first-class
//!   operation here rather than something bolted on.
//! - `GET /api/event` is a `text/event-stream` with 88 event types, including
//!   `permission.v2.asked`, `session.idle` and `session.error`.
//! - `POST /api/session/{id}/permission/{requestID}/reply { reply }` with
//!   `once | always | reject`.
//! - Auth is HTTP **Basic**, username `opencode`, password from
//!   `OPENCODE_SERVER_PASSWORD`. Unset, the server says so and is open to
//!   anyone with a shell on the machine.

use crate::ctx::NodeCtx;
use nexmesh_proto::{self as proto, Envelope};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub type InjectRequest = Vec<Envelope>;

/// What the node needs to talk to one agent's server.
#[derive(Clone)]
pub struct Server {
    pub base: String,
    user: Option<String>,
    password: Option<String>,
    client: reqwest::Client,
}

impl Server {
    pub fn new(base: String, user: Option<String>, password: Option<String>) -> Self {
        Self {
            base,
            user,
            password,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        }
    }

    fn auth(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.user, &self.password) {
            (Some(u), Some(p)) => r.basic_auth(u, Some(p)),
            _ => r,
        }
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        let r = self.auth(self.client.get(format!("{}{path}", self.base))).send().await?;
        anyhow::ensure!(r.status().is_success(), "GET {path} -> {}", r.status());
        Ok(r.json().await?)
    }

    pub async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let r = self
            .auth(self.client.post(format!("{}{path}", self.base)).json(&body))
            .send()
            .await?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        anyhow::ensure!(
            status.is_success(),
            "POST {path} -> {status}: {}",
            text.chars().take(200).collect::<String>()
        );
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// Pull the base URL out of the line `opencode serve` prints at startup.
///
/// The port is asked for as 0 so the OS picks a free one -- two agents on the
/// same machine must not fight over a fixed port -- which means the only way
/// to learn it is to read this line.
pub fn parse_server_url(line: &str) -> Option<String> {
    let idx = line.find("http://").or_else(|| line.find("https://"))?;
    let url: String = line[idx..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    let url = url.trim_end_matches(['.', ',', ')']).to_string();
    // A bare scheme with no host is not a usable URL.
    let rest = url.split_once("://")?.1;
    (!rest.is_empty() && rest.contains(':')).then_some(url)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    ctx: Arc<NodeCtx>,
    agent_id: String,
    slot_id: String,
    server: Server,
    mut inject_rx: mpsc::Receiver<InjectRequest>,
    first_prompt: String,
    cwd: String,
    resume: Option<String>,
    want_model: Option<String>,
) {
    // The server takes a moment to come up even after printing its URL.
    let mut ready = false;
    for _ in 0..60 {
        if server.get("/api/health").await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    if !ready {
        tracing::error!(%agent_id, base = %server.base, "the opencode server never became healthy");
        return;
    }

    let mut body = json!({ "location": { "directory": cwd } });
    if let Some(id) = &resume {
        // Reusing the id continues the previous conversation.
        body["id"] = json!(id);
    }
    // `POST /api/session` takes `model: ModelRef { providerID, id }` -- both
    // required -- so a model is requested as opencode writes it, as
    // `provider/model`. Anything without the provider is not a ModelRef and
    // would be silently ignored, so say so instead.
    if let Some(m) = &want_model {
        match m.split_once('/') {
            Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
                body["model"] = json!({ "providerID": provider, "id": id });
            }
            _ => tracing::warn!(model = %m,
                "opencode needs a model as 'provider/model' (e.g. anthropic/claude-sonnet-4); \
                 ignoring this one and using its default"),
        }
    }
    let session = match server.post("/api/session", body).await {
        Ok(v) => {
            // `Session.model` is `{ id, providerID, variant }` in opencode's
            // own OpenAPI, and `providerID/id` is how opencode itself writes a
            // model, so report it that way rather than inventing a format.
            let m = &v["data"]["model"];
            if let (Some(p), Some(i)) = (m["providerID"].as_str(), m["id"].as_str()) {
                let _ = ctx
                    .send_hub(&proto::NodeToHub::AgentModel {
                        agent_id: agent_id.clone(),
                        model: format!("{p}/{i}"),
                    })
                    .await;
            }
            v["data"]["id"].as_str().map(String::from)
        }
        Err(e) => {
            tracing::error!(%agent_id, error = %e, "could not create an opencode session");
            return;
        }
    };
    let Some(session) = session else {
        tracing::error!(%agent_id, "opencode returned no session id");
        return;
    };
    let _ = ctx
        .send_hub(&proto::NodeToHub::ResumeHandle {
            agent_id: agent_id.clone(),
            slot_id: slot_id.clone(),
            handle: session.clone(),
        })
        .await;
    tracing::info!(%agent_id, %session, resumed = resume.is_some(), "opencode session ready");

    // Events: permissions must be answered or the agent stalls, and idling is
    // when token totals are worth re-reading.
    {
        let server = server.clone();
        let ctx = ctx.clone();
        let agent_id = agent_id.clone();
        let session = session.clone();
        tokio::spawn(async move { events(ctx, agent_id, server, session).await });
    }

    if let Err(e) = server
        .post(
            &format!("/api/session/{session}/prompt"),
            json!({ "prompt": { "text": first_prompt }, "delivery": "queue" }),
        )
        .await
    {
        tracing::warn!(%agent_id, error = %e, "the first opencode prompt failed");
    }

    while let Some(messages) = inject_rx.recv().await {
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        let text = crate::driver::render_injection(&messages);
        // "steer" is opencode's own word for landing a message in the turn
        // that is already running, which is the entire point of a driver.
        let r = server
            .post(
                &format!("/api/session/{session}/prompt"),
                json!({ "prompt": { "text": text }, "delivery": "steer" }),
            )
            .await;
        match r {
            Ok(_) => {
                tracing::info!(%agent_id, count = ids.len(), "steered into the opencode session");
                let _ = ctx
                    .send_hub(&proto::NodeToHub::MessageAcked {
                        agent_id: agent_id.clone(),
                        message_ids: ids,
                    })
                    .await;
            }
            Err(e) => {
                tracing::warn!(%agent_id, error = %e, "opencode refused the delivery; requeueing");
                let _ = ctx
                    .send_hub(&proto::NodeToHub::InjectionsLost {
                        agent_id: agent_id.clone(),
                        message_ids: ids,
                    })
                    .await;
            }
        }
    }
}

/// Read the server-sent event stream, reconnecting if it drops.
async fn events(ctx: Arc<NodeCtx>, agent_id: String, server: Server, session: String) {
    use futures_util::StreamExt;
    let seen_tokens = Arc::new(Mutex::new((0i64, 0i64, 0i64)));

    loop {
        let req = server
            .auth(server.client.get(format!("{}/api/event", server.base)))
            .send()
            .await;
        let Ok(resp) = req else {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        };
        if !resp.status().is_success() {
            tracing::warn!(%agent_id, status = %resp.status(), "opencode event stream refused");
            return;
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(Ok(chunk)) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE frames are separated by a blank line; a chunk may hold part
            // of one, so only take whole frames.
            while let Some(cut) = buf.find("\n\n") {
                let frame: String = buf.drain(..cut + 2).collect();
                for line in frame.lines() {
                    let Some(payload) = line.strip_prefix("data:") else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(payload.trim()) else { continue };
                    handle_event(&ctx, &agent_id, &server, &session, &v, &seen_tokens).await;
                }
            }
        }
        tracing::debug!(%agent_id, "opencode event stream ended; reconnecting");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn handle_event(
    ctx: &Arc<NodeCtx>,
    agent_id: &str,
    server: &Server,
    session: &str,
    v: &Value,
    seen: &Arc<Mutex<(i64, i64, i64)>>,
) {
    match v["type"].as_str().unwrap_or("") {
        // The server blocks on these. An unattended agent has nobody to ask,
        // so approve for the session rather than stall the turn.
        "permission.v2.asked" | "permission.asked" => {
            let d = &v["data"];
            let sid = d["sessionID"].as_str().unwrap_or(session);
            let Some(req) = d["id"].as_str().or_else(|| d["requestID"].as_str()) else { return };
            let path = format!("/api/session/{sid}/permission/{req}/reply");
            if let Err(e) = server.post(&path, json!({ "reply": "always" })).await {
                tracing::warn!(%agent_id, error = %e, "could not answer an opencode permission");
            } else {
                tracing::debug!(%agent_id, "approved an opencode permission request");
            }
        }
        "session.error" => {
            tracing::warn!(%agent_id, error = %v["data"], "opencode reported a session error");
        }
        // Idle means a turn finished: a good moment to read the totals.
        "session.idle" | "session.updated" | "message.updated" => {
            if let Ok(s) = server.get(&format!("/api/session/{session}")).await {
                let t = &s["data"]["tokens"];
                let input = t["input"].as_i64().unwrap_or(0);
                let output = t["output"].as_i64().unwrap_or(0);
                let cache = t["cache"]["read"].as_i64().unwrap_or(0);
                let changed = {
                    let mut g = seen.lock().unwrap();
                    let c = (input, output, cache) != *g;
                    if c {
                        *g = (input, output, cache);
                    }
                    c
                };
                if changed {
                    let _ = ctx
                        .send_hub(&proto::NodeToHub::TokenUsage {
                            agent_id: agent_id.to_string(),
                            input,
                            output,
                            cache_read: cache,
                        })
                        .await;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_url_is_read_from_the_startup_line() {
        // The exact line opencode 1.19.5 prints.
        assert_eq!(
            parse_server_url("opencode server listening on http://127.0.0.1:17971").as_deref(),
            Some("http://127.0.0.1:17971")
        );
        assert_eq!(
            parse_server_url("listening on https://[::1]:4096 now").as_deref(),
            Some("https://[::1]:4096")
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_url() {
        assert_eq!(
            parse_server_url("ready at http://127.0.0.1:5000.").as_deref(),
            Some("http://127.0.0.1:5000")
        );
    }

    #[test]
    fn lines_without_a_usable_url_are_ignored() {
        for line in [
            "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.",
            "starting up",
            "see http:// for details",
            "",
        ] {
            assert!(parse_server_url(line).is_none(), "{line:?} should not parse");
        }
    }

    #[test]
    fn the_password_warning_line_does_not_look_like_a_url() {
        // It is printed first, so mistaking it for the URL would break startup.
        let warn = "Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured.";
        assert!(parse_server_url(warn).is_none());
    }
}
