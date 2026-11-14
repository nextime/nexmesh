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

use crate::ctx::{AgentIo, Injected, IoKind, NodeCtx};
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

/// How a batch reads when it has to be TYPED at a terminal.
///
/// A TUI submits on the first newline, so the rendering above would arrive as
/// one message followed by several stray empty submits. The same wording goes
/// onto a single line instead, with no newline in it until the Enter that
/// sends it. Each body is collapsed the same way, because a teammate who
/// writes a paragraph should not be able to submit on your behalf.
pub fn render_injection_line(messages: &[Envelope]) -> String {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut body = String::from("[PENDING MESSAGES] ");
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            body.push_str(" | ");
        }
        body.push_str(&format!(
            "from {}: {}",
            m.from_name.as_deref().unwrap_or("orchestrator"),
            flat(&m.body)
        ));
    }
    body.push_str(" | respond to these before continuing.");
    body
}

/// Drop ANSI escapes so the tail of a screen can be matched as text.
///
/// A TUI's output is mostly cursor movement and colour; `❯ 1. Yes` arrives
/// wrapped in both. Only CSI and the two-byte escapes are handled, which is
/// everything a CLI emits in practice.
fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI and OSC run until a final byte; everything between is
            // parameters, not text.
            Some('[') => {
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() || n == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '\u{7}' || n == '\u{1b}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Lines with something on them, decoration dropped.
///
/// A TUI frames everything, so `│   2. No` and `╰────────╯` are the same line
/// of content and no line at all respectively. Judging a screen means judging
/// what it SAYS.
fn meaningful(text: &str) -> Vec<String> {
    const FRAME: &[char] = &[
        '│', '|', '╭', '╰', '╮', '╯', '┌', '└', '┐', '┘', '├', '┤', '─', '━', '═',
        '-', '=', '*', '•', ' ', '\t',
    ];
    text.lines()
        .map(|l| l.trim().trim_start_matches(FRAME).trim_end_matches(FRAME).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Does this line look like a CLI sitting at its own input prompt?
///
/// The strongest evidence there is that nothing is waiting for an answer:
/// a prompt is drawn when the question is over. Short on purpose -- `>`,
/// `❯`, `agent>` -- so that a sentence ending in a bracket is not mistaken
/// for one.
fn is_prompt(line: &str) -> bool {
    let l = line.trim_end();
    (l.ends_with('>') || l.ends_with('❯')) && l.chars().count() <= 12
}

/// Is this screen waiting for an ANSWER rather than for a message?
///
/// Typing at a permission prompt delivers nothing: the text is swallowed by a
/// menu that does not take text, and the Enter after it answers the question
/// -- usually by accepting it. An injected teammate message must never be the
/// thing that approves a tool call nobody read. So when the bottom of the
/// screen looks like a prompt awaiting a choice, nothing is typed; the batch
/// goes back in the queue and the next tick looks again.
///
/// Only the BOTTOM is examined, and a prompt on the last line clears it
/// outright. Scrollback is append-only -- a dialog that was answered minutes
/// ago is still in it -- so a rule that searched the whole tail would block
/// delivery for the rest of the agent's life the first time anyone was asked
/// anything. A question is the last thing on a screen; an answered one is not.
pub fn awaiting_answer(screen: &str) -> Option<String> {
    let lines = meaningful(&strip_ansi(screen));
    // A prompt means the CLI is done asking and is waiting to be typed at,
    // which is the whole point of looking.
    if lines.last().map(|l| is_prompt(l)).unwrap_or(false) {
        return None;
    }
    for line in lines.iter().rev().take(4) {
        let low = line.to_lowercase();
        for pat in [
            "(y/n)", "[y/n]", "(yes/no)", "do you want", "do you trust",
            "press enter to", "esc to cancel", "overwrite?",
        ] {
            if low.contains(pat) {
                return Some(pat.to_string());
            }
        }
        // A menu option, with or without the cursor on it: `❯ 1. Yes`, `2. No`.
        let opt = low.trim_start_matches(['❯', '>']).trim_start();
        let mut c = opt.chars();
        if let (Some(d), Some(sep)) = (c.next(), c.next()) {
            if d.is_ascii_digit() && (sep == '.' || sep == ')') && !opt[2..].trim().is_empty() {
                return Some("a numbered prompt".to_string());
            }
        }
    }
    None
}

/// Write a batch to the CLI's stdin, or type it at the CLI's terminal.
///
/// Returns the ids that are delivered as far as anything will ever say so.
/// For a structured driver that is empty: the CLI echoes the message back and
/// THAT is the ack, matched later. A terminal has no echo worth parsing, so a
/// successful write is the only evidence there will ever be and the caller
/// acks on it -- which is also what a human pressing Enter amounts to.
pub async fn inject(io: &Arc<AgentIo>, messages: &[Envelope]) -> anyhow::Result<Vec<i64>> {
    // Codex (JSON-RPC) and opencode (HTTP) take whole messages, not bytes on
    // stdin, and each acks itself once its call returns, so there is nothing
    // pending to record here.
    if let Some(push) = &io.push {
        push.send(messages.to_vec())
            .await
            .map_err(|_| anyhow::anyhow!("this agent's driver has stopped"))?;
        return Ok(vec![]);
    }
    let Some(tx) = &io.input else {
        anyhow::bail!("this agent has no input channel");
    };
    // A terminal is not a stdin: a stream-json frame typed at a TUI shows up
    // as a line of JSON in the chat box. Type what a person would type.
    if io.kind == IoKind::Pty {
        if let Some(why) = awaiting_answer(&String::from_utf8_lossy(&io.scrollback(4096))) {
            anyhow::bail!("the terminal is waiting for an answer ({why})");
        }
        let line = format!("{}\r", render_injection_line(messages));
        tx.send(line.into_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("the agent's terminal is closed"))?;
        return Ok(messages.iter().map(|m| m.id).collect());
    }
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
    Ok(vec![])
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
    let mut model_sent = false;

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
                // The init event names the model. It used to be dropped here,
                // which left the token counts beside it meaningless.
                if let Some(model) = v["model"].as_str() {
                    if !model_sent {
                        model_sent = true;
                        let _ = ctx
                            .send_hub(&proto::NodeToHub::AgentModel {
                                agent_id: agent_id.clone(),
                                model: model.to_string(),
                            })
                            .await;
                    }
                }
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
    fn a_typed_batch_is_one_line() {
        // A TUI submits on the first newline. A message containing newlines
        // would otherwise arrive as a fragment plus a handful of stray
        // submits -- and an empty submit at a Claude prompt is not harmless.
        let line = render_injection_line(&[
            env(1, "orchestrator", "ship it\nthen tell me"),
            env(2, "Max", "and\t rebase  first"),
        ]);
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains('\r'), "{line}");
        assert!(line.starts_with("[PENDING MESSAGES] "));
        assert!(line.contains("from orchestrator: ship it then tell me"));
        assert!(line.contains("from Max: and rebase first"));
        assert!(line.ends_with("respond to these before continuing."));
    }

    #[test]
    fn escapes_do_not_hide_a_prompt() {
        // The tail of a real screen is mostly colour and cursor movement.
        let screen = "\u{1b}[2K\u{1b}[36m╭────╮\u{1b}[0m\n\u{1b}[1m│ \u{1b}[32m❯ 1. Yes\u{1b}[0m\n";
        assert_eq!(awaiting_answer(screen).as_deref(), Some("a numbered prompt"));
    }

    #[test]
    fn a_dialog_is_never_typed_over() {
        // Each of these swallows the text and reads the Enter after it as the
        // ANSWER -- which for a permission prompt means approving a tool call
        // nobody looked at. Delivery waits; it does not answer.
        for screen in [
            "Do you want to make this edit to main.rs?\n  1. Yes\n  2. No\n",
            "Overwrite config.toml? (y/n) ",
            "  Do you trust the files in this folder?\n",
            "press enter to continue",
        ] {
            assert!(awaiting_answer(screen).is_some(), "{screen}");
        }
    }

    #[test]
    fn an_idle_prompt_is_typed_at() {
        // The other half: a guard that never lets anything through is just
        // the old behaviour with extra steps.
        for screen in [
            "\u{1b}[2m> \u{1b}[0m\n  ? for shortcuts\n",
            "",
            "● Done. Anything else?\n\n> ",
            // A question in the agent's own prose is not a dialog: it has no
            // choice to answer and the agent is waiting for input anyway.
            "Should I also update the tests?\n\n> ",
            // An agent that ends its turn with a numbered list is not asking
            // anything. The prompt under it says so.
            "Done:\n  1. parser fixed\n  2. test added\n\n❯ ",
        ] {
            assert_eq!(awaiting_answer(screen), None, "{screen:?}");
        }
    }

    #[test]
    fn an_answered_dialog_stops_blocking() {
        // Scrollback is append-only: the dialog is still in it after it has
        // been answered. A rule that searched the whole tail blocked every
        // message for the rest of the agent's life, the first time anyone was
        // asked anything -- which is strictly worse than not typing at all.
        let up = "│ Do you want to make this edit?\n│ ❯ 1. Yes\n│   2. No\n╰────╯\n";
        assert!(awaiting_answer(up).is_some());
        let answered = up.to_string() + "\n\u{1b}[2mEdit applied.\u{1b}[0m\n> ";
        assert_eq!(awaiting_answer(&answered), None, "{answered:?}");
    }

    #[test]
    fn a_three_option_dialog_is_caught_by_its_later_options() {
        // The real one puts the cursor on `1.` and two more options under it,
        // so the cursor line is already out of sight at the bottom of the
        // screen. Every option counts, not just the selected one.
        let screen = "│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. Yes, for this session\n\
                      │   3. No, and tell me what to do differently\n";
        assert_eq!(awaiting_answer(screen).as_deref(), Some("a numbered prompt"));
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
    fn a_plugin_acks_by_exact_id() {
        // Unlike a stream-json replay there is nothing to correlate: the
        // plugin says which ids got through.
        let io = io();
        io.push_pending(Injected { text: "a".into(), message_ids: vec![1, 2] });
        io.push_pending(Injected { text: "b".into(), message_ids: vec![3] });
        assert_eq!(io.ack_ids(&[3]), vec![3]);
        assert_eq!(io.ack_ids(&[1, 2]), vec![1, 2]);
        assert!(io.ack_ids(&[1, 2, 3]).is_empty(), "nothing left to ack");
    }

    #[test]
    fn a_plugin_cannot_ack_a_message_it_was_never_given() {
        // A plugin is an external binary. Inventing an id must not mark
        // somebody else's message delivered.
        let io = io();
        io.push_pending(Injected { text: "mine".into(), message_ids: vec![10] });
        assert!(io.ack_ids(&[999]).is_empty(), "an unknown id must ack nothing");
        assert_eq!(io.drain_pending(), vec![10], "and must not retire what is pending");
    }

    #[test]
    fn acking_the_same_id_twice_only_counts_once() {
        let io = io();
        io.push_pending(Injected { text: "a".into(), message_ids: vec![5] });
        assert_eq!(io.ack_ids(&[5]), vec![5]);
        assert!(io.ack_ids(&[5]).is_empty());
    }

    #[test]
    fn a_partly_acked_batch_keeps_the_rest_pending() {
        // Half-delivered must not count as delivered, or the remainder is
        // never redelivered.
        let io = io();
        io.push_pending(Injected { text: "batch".into(), message_ids: vec![1, 2, 3] });
        assert_eq!(io.ack_ids(&[2]), vec![2]);
        let mut left = io.drain_pending();
        left.sort();
        assert_eq!(left, vec![1, 3]);
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
