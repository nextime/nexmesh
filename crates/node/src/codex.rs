//! The Codex `app-server` driver.
//!
//! Codex speaks JSON-RPC over stdio with threads and turns, so unlike Claude's
//! stream-json there is no "write a line of text" injection: a message is
//! either steered into the turn that is running (`turn/steer`) or starts a new
//! one (`turn/start`).
//!
//! Every shape here came from `codex app-server generate-json-schema --out`
//! (codex-cli 0.167.2) and a live handshake, not from guesswork:
//!
//! - `initialize` takes `clientInfo { name, version }`.
//! - `thread/start` takes `cwd` and `approvalPolicy`; its result carries
//!   `thread.id`, which is the handle for `thread/resume { threadId }`.
//! - `turn/start { threadId, input: [UserInput] }` returns `{ turn }`, and
//!   `turn.id` is what `turn/steer { threadId, expectedTurnId, input }` needs.
//! - `thread/tokenUsage/updated` carries `tokenUsage.total` with
//!   `inputTokens`, `outputTokens`, `cachedInputTokens`.
//! - `turn/started` / `turn/completed` carry a `Turn` whose `status` is one of
//!   completed | interrupted | failed | inProgress.
//! - **Notifications omit the `jsonrpc` field** and add `emittedAtMs`, so a
//!   parser that insists on `jsonrpc: "2.0"` would drop all of them.
//! - The server asks US things, and it BLOCKS until answered. Approvals are
//!   accepted (an unattended agent cannot ask a human); requests that need a
//!   person -- user input, elicitation -- are declined rather than left to
//!   time out.

use crate::ctx::NodeCtx;
use nexmesh_proto::{self as proto, Envelope};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

/// A batch of teammate messages to get into the agent.
pub type InjectRequest = Vec<Envelope>;

struct Rpc {
    tx: mpsc::Sender<String>,
    next_id: Mutex<i64>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>,
}

impl Rpc {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let (done, wait) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, done);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.tx.send(frame.to_string()).await.is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err("the codex app-server is gone".into());
        }
        match tokio::time::timeout(std::time::Duration::from_secs(120), wait).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the app-server closed while a call was in flight".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("'{method}' timed out"))
            }
        }
    }

    fn resolve(&self, id: i64, result: Result<Value, String>) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(result);
        }
    }

    async fn reply(&self, id: &Value, result: Value) {
        let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.tx.send(frame.to_string()).await;
    }
}

fn text_input(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Answer a request the server is blocking on.
///
/// An unattended agent has nobody to ask, so the choice is approve or stall.
/// Commands and file changes are approved -- that is what the operator asked
/// for by launching an agent at all -- while anything needing a human's words
/// is declined so the turn can continue.
fn answer_server_request(method: &str) -> Option<Value> {
    match method {
        "execCommandApproval" | "applyPatchApproval" => Some(json!({ "decision": "approved" })),
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({ "decision": "acceptForSession" }))
        }
        // A permission profile we cannot reason about: decline the escalation
        // and let the sandbox do its job.
        "item/permissions/requestApproval" => None,
        "item/tool/requestUserInput" => Some(json!({ "answers": {} })),
        "mcpServer/elicitation/request" => Some(json!({ "action": "decline" })),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    ctx: Arc<NodeCtx>,
    agent_id: String,
    slot_id: String,
    io: Arc<crate::ctx::AgentIo>,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut inject_rx: mpsc::Receiver<InjectRequest>,
    first_prompt: String,
    cwd: String,
    resume: Option<String>,
) {
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err()
                || stdin.flush().await.is_err()
            {
                break;
            }
        }
    });

    let rpc = Arc::new(Rpc {
        tx: out_tx,
        next_id: Mutex::new(0),
        pending: Mutex::new(HashMap::new()),
    });

    // Shared turn state: the reader task learns it, the injector needs it.
    let thread_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let active_turn: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Reader: responses, notifications and the requests we must answer.
    {
        let rpc = rpc.clone();
        let ctx = ctx.clone();
        let io = io.clone();
        let agent_id = agent_id.clone();
        let active_turn = active_turn.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut seq = 0u64;
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                seq += 1;
                crate::spawn::emit_output(&ctx, &agent_id, &io, format!("{line}\n").as_bytes(), seq)
                    .await;
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };

                // A response to something we asked.
                if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                    if v.get("method").is_none() {
                        let r = if let Some(err) = v.get("error") {
                            Err(err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error")
                                .to_string())
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        rpc.resolve(id, r);
                        continue;
                    }
                }

                let Some(method) = v.get("method").and_then(|m| m.as_str()) else { continue };

                // A request FROM the server: it blocks until answered.
                if v.get("id").is_some() {
                    let id = v.get("id").cloned().unwrap_or(Value::Null);
                    match answer_server_request(method) {
                        Some(result) => {
                            tracing::debug!(%agent_id, method, "answering a server request");
                            rpc.reply(&id, result).await;
                        }
                        None => {
                            tracing::warn!(%agent_id, method,
                                "declining a server request we cannot answer unattended");
                            let frame = json!({
                                "jsonrpc": "2.0", "id": id,
                                "error": { "code": -32601,
                                           "message": "nexmesh runs this agent unattended" }
                            });
                            let _ = rpc.tx.send(frame.to_string()).await;
                        }
                    }
                    continue;
                }

                // Notifications. These carry no `jsonrpc` field.
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                match method {
                    "turn/started" => {
                        let id = p["turn"]["id"].as_str().map(|s| s.to_string());
                        *active_turn.lock().unwrap() = id;
                    }
                    "turn/completed" => {
                        *active_turn.lock().unwrap() = None;
                        if p["turn"]["status"] == "failed" {
                            tracing::warn!(%agent_id, "a codex turn failed");
                        }
                    }
                    "thread/tokenUsage/updated" => {
                        let t = &p["tokenUsage"]["total"];
                        let _ = ctx
                            .send_hub(&proto::NodeToHub::TokenUsage {
                                agent_id: agent_id.clone(),
                                input: t["inputTokens"].as_i64().unwrap_or(0),
                                output: t["outputTokens"].as_i64().unwrap_or(0),
                                cache_read: t["cachedInputTokens"].as_i64().unwrap_or(0),
                            })
                            .await;
                    }
                    "error" => {
                        tracing::warn!(%agent_id, error = %p["error"], will_retry = %p["willRetry"],
                            "codex reported an error");
                    }
                    _ => {}
                }
            }
            tracing::debug!(%agent_id, "codex app-server output ended");
        });
    }

    // Handshake.
    if let Err(e) = rpc
        .call(
            "initialize",
            json!({ "clientInfo": { "name": "nexmesh", "version": env!("CARGO_PKG_VERSION") } }),
        )
        .await
    {
        tracing::error!(%agent_id, error = %e, "codex initialize failed");
        return;
    }

    // Resume the previous thread if we have one, else start a fresh one.
    let started = match &resume {
        Some(tid) => {
            rpc.call("thread/resume", json!({ "threadId": tid, "cwd": cwd,
                                              "approvalPolicy": "never" }))
                .await
        }
        None => rpc.call("thread/start", json!({ "cwd": cwd, "approvalPolicy": "never" })).await,
    };
    let tid = match started {
        Ok(v) => v["thread"]["id"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| resume.clone()),
        Err(e) => {
            tracing::error!(%agent_id, error = %e, "could not start a codex thread");
            return;
        }
    };
    let Some(tid) = tid else {
        tracing::error!(%agent_id, "codex returned no thread id");
        return;
    };
    *thread_id.lock().unwrap() = Some(tid.clone());
    let _ = ctx
        .send_hub(&proto::NodeToHub::ResumeHandle {
            agent_id: agent_id.clone(),
            slot_id: slot_id.clone(),
            handle: tid.clone(),
        })
        .await;
    tracing::info!(%agent_id, thread = %tid, resumed = resume.is_some(), "codex thread ready");

    // The first prompt is just a turn.
    match rpc
        .call("turn/start", json!({ "threadId": tid, "input": text_input(&first_prompt) }))
        .await
    {
        Ok(v) => {
            if let Some(turn) = v["turn"]["id"].as_str() {
                *active_turn.lock().unwrap() = Some(turn.to_string());
            }
        }
        Err(e) => tracing::warn!(%agent_id, error = %e, "the first codex turn failed to start"),
    }

    // Injection loop: steer a live turn, or start a new one.
    while let Some(messages) = inject_rx.recv().await {
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        let text = crate::driver::render_injection(&messages);
        let current = active_turn.lock().unwrap().clone();

        let result = match &current {
            Some(turn) => {
                // Mid-turn: this is the whole point of the driver.
                rpc.call(
                    "turn/steer",
                    json!({ "threadId": tid, "expectedTurnId": turn, "input": text_input(&text) }),
                )
                .await
                .map(|_| ())
            }
            None => rpc
                .call("turn/start", json!({ "threadId": tid, "input": text_input(&text) }))
                .await
                .map(|v| {
                    if let Some(turn) = v["turn"]["id"].as_str() {
                        *active_turn.lock().unwrap() = Some(turn.to_string());
                    }
                }),
        };

        match result {
            Ok(()) => {
                // Codex accepted the input. That is weaker proof than Claude's
                // replay echo -- it means the runtime took it, not that the
                // model has read it -- but it is the strongest signal this
                // protocol offers.
                tracing::info!(%agent_id, count = ids.len(),
                    steered = current.is_some(), "delivered to codex");
                let _ = ctx
                    .send_hub(&proto::NodeToHub::MessageAcked {
                        agent_id: agent_id.clone(),
                        message_ids: ids,
                    })
                    .await;
            }
            Err(e) => {
                tracing::warn!(%agent_id, error = %e, "codex refused the delivery; requeueing");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_are_accepted_so_an_unattended_turn_can_proceed() {
        // Verified decision vocabularies: ReviewDecision uses `approved`,
        // the newer item/* requests use `accept`/`acceptForSession`. Sending
        // the wrong word for the wrong method leaves the turn blocked.
        assert_eq!(
            answer_server_request("execCommandApproval").unwrap()["decision"],
            "approved"
        );
        assert_eq!(
            answer_server_request("applyPatchApproval").unwrap()["decision"],
            "approved"
        );
        assert_eq!(
            answer_server_request("item/commandExecution/requestApproval").unwrap()["decision"],
            "acceptForSession"
        );
        assert_eq!(
            answer_server_request("item/fileChange/requestApproval").unwrap()["decision"],
            "acceptForSession"
        );
    }

    #[test]
    fn requests_needing_a_human_are_answered_emptily_not_left_hanging() {
        // The server blocks on these, so silence would wedge the agent.
        assert_eq!(
            answer_server_request("item/tool/requestUserInput").unwrap()["answers"],
            json!({})
        );
        assert_eq!(
            answer_server_request("mcpServer/elicitation/request").unwrap()["action"],
            "decline"
        );
    }

    #[test]
    fn an_unknown_server_request_is_refused_explicitly() {
        assert!(answer_server_request("something/new").is_none());
        assert!(answer_server_request("item/permissions/requestApproval").is_none());
    }

    #[test]
    fn user_input_matches_the_TextUserInput_shape() {
        let v = text_input("hello");
        assert_eq!(v[0]["type"], "text");
        assert_eq!(v[0]["text"], "hello");
        assert!(v.is_array(), "turn/start takes an ARRAY of UserInput");
    }
}
