//! Driver fast paths: getting a teammate's message to a model MID-TURN.
//!
//! Without a driver, a message waits for the agent's next MCP tool call. That
//! is the universal floor and it always works, but an agent deep in a long
//! edit may not call a tool for minutes. A driver writes into the CLI's own
//! input stream so the message is read at the next tool-call boundary instead.
//!
//! `stream_json` is Claude Code's shape. Everything below was verified against
//! claude 2.1.322 rather than assumed:
//!   - `--output-format=stream-json` REQUIRES `--verbose`, or it exits at once.
//!   - `system`/`init` carries `session_id`: the handle for `--resume`.
//!   - `--replay-user-messages` echoes each stdin message back as a `user`
//!     event with `isReplay: true`. That echo is the proof it reached the
//!     model, and it is the ack.
//!   - Custom fields added to an input message are STRIPPED from the replay,
//!     so acks correlate on the message text.
//!   - `assistant` events carry `message.usage` per API call; the turn's
//!     `result` carries the totals.

use crate::ctx::{AgentIo, Injected, NodeCtx};
use nexmesh_proto::{self as proto, Envelope};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Frame a message as a stream-json user message, the way Claude expects.
pub fn user_message(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] }
    })
    .to_string()
}

/// How a batch of teammate messages is presented to the model. The same
/// wording the MCP floor uses, so an agent sees one convention either way.
pub fn render_injection(messages: &[Envelope]) -> String {
    let mut body = String::from("[PENDING MESSAGES]\n");
    for m in messages {
        body.push_str(&format!(
            "- from {}: {}\n",
            m.from_name.as_deref().unwrap_or("orchestrator"),
            m.body
        ));
    }
    body.push_str("[END PENDING MESSAGES]\nRespond to these before continuing.");
    body
}

/// Write a batch to the CLI's stdin and remember it until it is echoed back.
pub async fn inject(io: &Arc<AgentIo>, messages: &[Envelope]) -> anyhow::Result<()> {
    // Codex (JSON-RPC) and opencode (HTTP) take whole messages, not bytes on
    // stdin, and each acks itself once its call returns, so there is nothing
    // pending to record here.
    if let Some(push) = &io.push {
        return push
            .send(messages.to_vec())
            .await
            .map_err(|_| anyhow::anyhow!("this agent's driver has stopped"));
    }
    let Some(tx) = &io.input else {
        anyhow::bail!("this agent has no input channel");
    };
    let text = render_injection(messages);
    let line = format!("{}\n", user_message(&text));
    // Record BEFORE writing: if the echo arrived first we would have nothing
    // to match it against.
    io.push_pending(Injected {
        text,
        message_ids: messages.iter().map(|m| m.id).collect(),
    });
    tx.send(line.into_bytes())
        .await
        .map_err(|_| anyhow::anyhow!("the agent's input channel is closed"))?;
    Ok(())
}

#[derive(Default)]
struct Usage {
    input: i64,
    output: i64,
    cache_read: i64,
}

/// Read a Claude stream-json stdout: transcript, acks, tokens, resume handle.
pub async fn run_stream_json<R>(
    ctx: Arc<NodeCtx>,
    agent_id: String,
    slot_id: String,
    io: Arc<AgentIo>,
    reader: R,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut usage = Usage::default();
    let mut seq = 0u64;
    let mut resume_sent = false;

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        // Everything goes to the transcript, parseable or not: a line we do not
        // understand is exactly what someone will need to read later.
        seq += 1;
        crate::spawn::emit_output(&ctx, &agent_id, &io, format!("{line}\n").as_bytes(), seq).await;

        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                if let (false, Some(sid)) = (resume_sent, v["session_id"].as_str()) {
                    resume_sent = true;
                    let _ = ctx
                        .send_hub(&proto::NodeToHub::ResumeHandle {
                            agent_id: agent_id.clone(),
                            slot_id: slot_id.clone(),
                            handle: sid.to_string(),
                        })
                        .await;
                    tracing::info!(%agent_id, session = sid, "captured the resume handle");
                }
            }

            // The echo of something we wrote: proof it reached the model.
            "user" if v["isReplay"].as_bool() == Some(true) => {
                let text = v["message"]["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let ids = io.ack_replay(&text);
                if !ids.is_empty() {
                    tracing::info!(%agent_id, acked = ids.len(), "driver delivery confirmed");
                    let _ = ctx
                        .send_hub(&proto::NodeToHub::MessageAcked {
                            agent_id: agent_id.clone(),
                            message_ids: ids,
                        })
                        .await;
                }
            }

            // Per-API-call metering.
            "assistant" => {
                let u = &v["message"]["usage"];
                usage.input += u["input_tokens"].as_i64().unwrap_or(0);
                usage.cache_read += u["cache_read_input_tokens"].as_i64().unwrap_or(0);
                let _ = ctx
                    .send_hub(&proto::NodeToHub::TokenUsage {
                        agent_id: agent_id.clone(),
                        input: usage.input,
                        output: usage.output,
                        cache_read: usage.cache_read,
                    })
                    .await;
            }

            // End of a turn: authoritative output count.
            "result" => {
                usage.output += v["usage"]["output_tokens"].as_i64().unwrap_or(0);
                let _ = ctx
                    .send_hub(&proto::NodeToHub::TokenUsage {
                        agent_id: agent_id.clone(),
                        input: usage.input,
                        output: usage.output,
                        cache_read: usage.cache_read,
                    })
                    .await;
                if v["is_error"].as_bool() == Some(true) {
                    tracing::warn!(%agent_id, subtype = ?v["subtype"].as_str(),
                        "the agent's turn ended in an error");
                }
            }
            _ => {}
        }
    }
    tracing::debug!(%agent_id, "stream-json output ended");
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexmesh_proto::MessageKind;

    fn env(id: i64, from: &str, body: &str) -> Envelope {
        Envelope {
            id,
            from_slot: Some("sl-x".into()),
            from_name: Some(from.into()),
            to_slot: Some("sl-y".into()),
            kind: MessageKind::Text,
            body: body.into(),
            sent_at_ms: 0,
        }
    }

    #[test]
    fn a_user_message_matches_the_shape_claude_accepts() {
        let v: serde_json::Value = serde_json::from_str(&user_message("hi")).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "hi");
    }

    #[test]
    fn the_injection_is_framed_like_the_mcp_floor() {
        let text = render_injection(&[env(1, "Iris", "please review"), env(2, "Hermes", "indexed")]);
        assert!(text.starts_with("[PENDING MESSAGES]"));
        assert!(text.contains("from Iris: please review"));
        assert!(text.contains("from Hermes: indexed"));
        assert!(text.contains("[END PENDING MESSAGES]"));
    }

    #[test]
    fn a_message_must_survive_a_round_trip_through_json() {
        // Awkward text is the norm: quotes, newlines, unicode.
        let nasty = "he said \"stop\"\nnext line\tτέλος 🎉";
        let line = user_message(nasty);
        assert!(!line.contains('\n'), "the frame must be one line for NDJSON");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["message"]["content"][0]["text"], nasty);
    }

    fn io() -> Arc<AgentIo> {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        AgentIo::new(crate::ctx::IoKind::Pipes, Some(tx), None)
    }

    #[test]
    fn an_exact_replay_acks_the_matching_injection() {
        let io = io();
        io.push_pending(Injected { text: "first".into(), message_ids: vec![1, 2] });
        io.push_pending(Injected { text: "second".into(), message_ids: vec![3] });
        // Out of order on purpose: match on content, not position.
        assert_eq!(io.ack_replay("second"), vec![3]);
        assert_eq!(io.ack_replay("first"), vec![1, 2]);
        assert!(io.ack_replay("first").is_empty(), "nothing left to ack");
    }

    #[test]
    fn an_unmatched_replay_still_retires_the_oldest_injection() {
        // Otherwise that message is in flight forever: never acked, never
        // redelivered, silently lost.
        let io = io();
        io.push_pending(Injected { text: "mine".into(), message_ids: vec![9] });
        assert_eq!(io.ack_replay("something the user typed"), vec![9]);
    }

    #[test]
    fn a_replay_with_nothing_pending_acks_nothing() {
        assert!(io().ack_replay("anything").is_empty());
    }

    #[test]
    fn outstanding_injections_are_drained_when_an_agent_dies() {
        let io = io();
        io.push_pending(Injected { text: "a".into(), message_ids: vec![1] });
        io.push_pending(Injected { text: "b".into(), message_ids: vec![2, 3] });
        let mut got = io.drain_pending();
        got.sort();
        assert_eq!(got, vec![1, 2, 3], "all must be released for redelivery");
        assert!(io.drain_pending().is_empty());
    }
}
