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
/// `newlines_safe` says whether the CLI has asked for bracketed paste (DEC
/// 2004). It decides the whole shape of this, because it decides what a
/// newline MEANS on the way in:
///
///   - With the markers, a newline inside the paste is a newline in the
///     composer. The bodies go in the way they were written -- a numbered
///     list, a diff, an indented code block all survive the trip -- and only
///     the Enter after the closing marker submits.
///   - Without them a TUI submits on the first newline, so the batch would
///     arrive as a fragment followed by a handful of stray empty submits.
///     Everything is flattened onto one line instead: lossy on purpose,
///     because a teammate who writes a paragraph must not be able to submit
///     on your behalf.
///
/// The flattening used to be unconditional. It was written before bracketed
/// paste was, and left behind: every list, every snippet and every indent an
/// agent was sent arrived as one run-on line at a CLI that would have taken
/// the real thing.
pub fn render_injection_line(messages: &[Envelope], newlines_safe: bool) -> String {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    // A body that keeps its newlines is indented under its own bullet. The
    // indent is uniform, so it shifts a code block without reflowing it,
    // and it is what makes the end of one message distinguishable from the
    // start of the next once several arrive together.
    let shaped = |s: &str| {
        if !newlines_safe {
            return flat(s);
        }
        s.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\n  ")
    };
    let mut body = String::from(if newlines_safe {
        "[PENDING MESSAGES]\n"
    } else {
        "[PENDING MESSAGES] "
    });
    let mut from_operator = false;
    for (i, m) in messages.iter().enumerate() {
        if newlines_safe {
            body.push_str("- ");
        } else if i > 0 {
            body.push_str(" | ");
        }
        // A message with no sending slot is from the person running the
        // fleet, and an agent told only to "respond" has no way to know that
        // a reply to a TEAMMATE never reaches them. The MCP paths say this
        // (see `proto::render_pending`); typing it at a terminal must say the
        // same thing, or the one route that arrives in seconds is the one
        // that gets answered to the wrong place.
        let who = match (&m.from_slot, &m.from_name) {
            // See `proto::render_pending`: the hub's own notes are not a person.
            (None, _) if m.kind == proto::MessageKind::Nudge => proto::AUTOMATIC.to_string(),
            (None, _) => {
                from_operator = true;
                "the OPERATOR".to_string()
            }
            (Some(_), Some(n)) => n.clone(),
            (Some(_), None) => "a teammate".to_string(),
        };
        body.push_str(&format!("from {}: {}", who, shaped(&m.body)));
        if newlines_safe {
            body.push('\n');
        }
    }
    if from_operator {
        body.push_str(if newlines_safe { "" } else { " | " });
        body.push_str(
            "the operator is a person reading this session's chat: answer with \
             `reply` and the message's own number, which reaches them there without \
             sending it to your teammates -- `broadcast` is for something the whole \
             team needs",
        );
        if newlines_safe {
            body.push('\n');
        }
    }
    body.push_str(if newlines_safe { "respond to these before continuing." } else { " | respond to these before continuing." });
    body
}

/// Drop ANSI escapes so the tail of a screen can be matched as text.
///
/// A TUI's output is mostly cursor movement and colour; `❯ 1. Yes` arrives
/// wrapped in both. Only CSI and the two-byte escapes are handled, which is
/// everything a CLI emits in practice.
pub(crate) fn strip_ansi(raw: &str) -> String {
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

/// What a terminal seat's model is doing, read from the bottom of its screen.
///
/// `None` when the screen says nothing either way -- which, for a terminal
/// whose CLI has never been seen working, is everything short of a question:
/// a shell, or a CLI whose furniture this does not know, is shown as unknown
/// rather than as idle. `known` is whether this seat has been seen working.
///
/// ACTIVE needs two things: the CLI's own "esc to interrupt" in the last few
/// lines, AND output in the last few seconds. Scrollback is append-only, so a
/// turn that has just ended can leave its last spinner frame at the tail; a
/// running turn redraws that spinner several times a second, so a quiet one
/// is over.
pub fn screen_activity(screen: &str, idle_ms: u64, known: bool) -> Option<proto::ActivityState> {
    if awaiting_answer(screen).is_some() {
        return Some(proto::ActivityState::Blocked);
    }
    let lines = meaningful(&strip_ansi(screen));
    let busy = lines.iter().rev().take(8).any(|l| {
        let l = l.to_lowercase();
        l.contains("esc to interrupt") || l.contains("ctrl+c to interrupt")
    });
    if busy && idle_ms < ACTIVE_QUIET_MS {
        Some(proto::ActivityState::Active)
    } else if known {
        Some(proto::ActivityState::Idle)
    } else {
        None
    }
}

/// How long a terminal may be silent and still be mid-turn. See
/// [`screen_activity`].
const ACTIVE_QUIET_MS: u64 = 3_000;

/// Write a batch to the CLI's stdin, or type it at the CLI's terminal.
///
/// Returns the ids that are delivered as far as anything will ever say so.
/// For a structured driver that is empty: the CLI echoes the message back and
/// THAT is the ack, matched later. A terminal has no echo worth parsing, so a
/// successful write is the only evidence there will ever be and the caller
/// acks on it -- which is also what a human pressing Enter amounts to.
/// How long to wait before pressing Enter at a TUI.
///
/// Long enough that the keystroke is not part of the burst that carried the
/// text -- paste detection keys on exactly that gap -- and short enough that
/// delivery still feels immediate.
const SUBMIT_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

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
        // Someone is typing at this terminal. Typing the message now would
        // splice it into the middle of their half-written line. Hold it back
        // and let the hub requeue it -- the human will send or clear what
        // they are writing, or go quiet long enough (COMPOSE_GRACE_MS) that
        // the message is delivered anyway.
        if io.human_composing() {
            anyhow::bail!("a human is typing at this terminal");
        }
        // A burst of bytes IS a paste as far as a TUI is concerned, and a
        // TUI that has asked for bracketed paste expects to be told where
        // one ends. Without the markers codex buffers the text as
        // "[Pasted Content 1024 chars]" and reads the Enter 300ms later as
        // part of it: the message is delivered, sits in the composer, and
        // waits for a human to press Enter. With them, the Enter is an
        // Enter. Verified against codex 0.169 at 1151 bytes, which is an
        // ordinary length for a teammate's message.
        let bracketed = io.bracketed();
        let line = render_injection_line(messages, bracketed);
        let typed = if bracketed {
            format!("\x1b[200~{line}\x1b[201~")
        } else {
            line
        };
        tx.send(typed.into_bytes())
            .await
            .map_err(|_| anyhow::anyhow!("the agent's terminal is closed"))?;
        // The Enter is its own keystroke, a moment later -- never the tail of
        // the same write.
        //
        // A TUI that sees a fast burst of bytes treats it as a PASTE, and a
        // carriage return inside a paste is a newline in the box, not a
        // submit. So the message was typed at the agent perfectly and then
        // sat there unsent, waiting for a human to press Enter -- which is
        // indistinguishable, from the outside, from an agent ignoring you.
        // Verified against claude 2.x in an adopted tmux pane.
        tokio::time::sleep(SUBMIT_PAUSE).await;
        tx.send(vec![b'\r'])
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
                // A query is starting: the model is about to work.
                ctx.report_activity(&agent_id, &io, proto::ActivityState::Active).await;
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
                // It has the message: a turn is running from here.
                ctx.report_activity(&agent_id, &io, proto::ActivityState::Active).await;
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
                ctx.report_activity(&agent_id, &io, proto::ActivityState::Active).await;
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
                // The turn is over, however it ended.
                ctx.report_activity(&agent_id, &io, proto::ActivityState::Idle).await;
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
        ], false);
        assert!(!line.contains('\n'), "{line}");
        assert!(!line.contains('\r'), "{line}");
        assert!(line.starts_with("[PENDING MESSAGES] "));
        assert!(line.contains("from orchestrator: ship it then tell me"));
        assert!(line.contains("from Max: and rebase first"));
        assert!(line.ends_with("respond to these before continuing."));
    }

    #[test]
    fn a_bracketed_batch_keeps_the_shape_it_was_written_in() {
        // Inside the paste markers a newline is a newline, so there is
        // nothing to protect against -- and a teammate who sends a numbered
        // list or an indented block meant it. Flattening this was silently
        // destroying every formatted message the fleet sent.
        let line = render_injection_line(
            &[env(1, "Max", "do these in order:\n  1. rebase\n  2. run the tests")],
            true,
        );
        assert!(line.contains("do these in order:\n    1. rebase\n    2. run the tests"),
                "{line:?}");
        // The body is indented under its own bullet, so two of them are
        // still tellable apart.
        assert!(line.contains("- from Max: do these in order:"), "{line:?}");
    }

    #[test]
    fn an_indented_block_keeps_its_relative_indent() {
        // A uniform shift is not a reflow: code sent to a CLI has to come
        // out of this with its own structure intact.
        let line = render_injection_line(&[env(1, "Max", "fn a() {\n    b();\n}")], true);
        assert!(line.contains("- from Max: fn a() {\n      b();\n  }"), "{line:?}");
    }

    #[test]
    fn the_note_to_the_operator_is_not_padded_with_source_whitespace() {
        // It used to carry runs of fourteen spaces from the way the literal
        // was wrapped in this file, and an agent read them verbatim.
        let mut e = env(1, "x", "hi");
        e.from_slot = None;
        e.from_name = None;
        for safe in [true, false] {
            let line = render_injection_line(&[e.clone()], safe);
            assert!(!line.contains("  `reply`"), "{line:?}");
        }
    }

    #[test]
    fn the_operator_is_named_as_the_operator_when_typed_at() {
        // The same thing the MCP paths say. A message with no sending slot is
        // the person running the fleet, and an agent that is not told so
        // answers a teammate instead -- which is what happened in the chat.
        let mut e = env(1, "orchestrator", "are you connected?");
        e.from_slot = None;
        e.from_name = None;
        let line = render_injection_line(&[e], false);
        assert!(line.contains("from the OPERATOR: are you connected?"), "{line}");
        assert!(line.contains("broadcast"), "{line}");
        assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
    }

    #[test]
    fn the_hubs_own_note_is_not_the_operator_and_asks_for_no_answer() {
        // A reminder labelled "the OPERATOR", with the line telling the agent
        // to `reply` to them, made every reminded seat write to the person.
        let mut e = env(1, "x", "a good moment to keep what you learned");
        e.from_slot = None;
        e.from_name = None;
        e.kind = proto::MessageKind::Nudge;
        for safe in [true, false] {
            let line = render_injection_line(&[e.clone()], safe);
            assert!(!line.contains("OPERATOR"), "{line}");
            assert!(!line.contains("`reply`"), "{line}");
            assert!(line.contains("automatic, not a person"), "{line}");
        }
    }

    #[test]
    fn a_teammates_line_says_nothing_about_the_operator() {
        let line = render_injection_line(&[env(1, "Max", "rebase first")], false);
        assert!(!line.contains("OPERATOR"), "{line}");
    }

    #[test]
    fn escapes_do_not_hide_a_prompt() {
        // The tail of a real screen is mostly colour and cursor movement.
        let screen = "\u{1b}[2K\u{1b}[36m╭────╮\u{1b}[0m\n\u{1b}[1m│ \u{1b}[32m❯ 1. Yes\u{1b}[0m\n";
        assert_eq!(awaiting_answer(screen).as_deref(), Some("a numbered prompt"));
    }

    #[tokio::test]
    async fn a_typed_message_is_bracketed_when_the_cli_asked_for_it() {
        // Codex buffers an unbracketed burst as "[Pasted Content 1024
        // chars]" and reads the Enter after it as part of the paste, so the
        // message is delivered, sits in the composer, and waits for a human.
        // The markers are what a real terminal sends, and make the Enter an
        // Enter.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let io = crate::ctx::AgentIo::new(crate::ctx::IoKind::Pty, Some(tx), None);
        io.note_modes(b"\x1b[?2004h");
        let body = "x".repeat(1200);
        inject(&io, &[env(1, "Ada", &body)]).await.unwrap();
        let first = String::from_utf8(rx.recv().await.unwrap()).unwrap();
        assert!(first.starts_with("\x1b[200~"), "no paste start: {:?}", &first[..20]);
        assert!(first.ends_with("\x1b[201~"), "no paste end");
        assert!(first.contains(&body), "the message itself must survive the wrapping");
        assert_eq!(rx.recv().await.unwrap(), vec![b'\r'], "the Enter is its own keystroke");
    }

    #[tokio::test]
    async fn a_cli_that_did_not_ask_is_typed_at_plainly() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let io = crate::ctx::AgentIo::new(crate::ctx::IoKind::Pty, Some(tx), None);
        inject(&io, &[env(1, "Ada", "hello")]).await.unwrap();
        let first = String::from_utf8(rx.recv().await.unwrap()).unwrap();
        assert!(!first.contains("\x1b[200~"), "markers a CLI never asked for are noise");
        assert!(first.contains("hello"));
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
    fn a_running_turn_is_active_while_its_spinner_moves() {
        let screen = "\u{23fa} Reading the parser\n\n\u{2733} Thinking\u{2026} (12s \u{b7} esc to interrupt)\n";
        assert_eq!(screen_activity(screen, 200, false), Some(proto::ActivityState::Active));
    }

    #[test]
    fn a_spinner_left_at_the_tail_of_a_quiet_screen_is_not_a_turn() {
        // The last frame of a finished turn, still in append-only scrollback.
        let screen = "\u{2733} Thinking\u{2026} (12s \u{b7} esc to interrupt)\n";
        assert_eq!(screen_activity(screen, 10_000, true), Some(proto::ActivityState::Idle));
    }

    #[test]
    fn a_question_on_screen_is_blocked_not_idle() {
        let screen = "\u{2502} Do you want to make this edit?\n\u{2502} \u{276f} 1. Yes\n\u{2502}   2. No\n";
        assert_eq!(screen_activity(screen, 10_000, true), Some(proto::ActivityState::Blocked));
    }

    #[test]
    fn a_terminal_never_seen_working_is_unknown_not_idle() {
        // A shell, or a CLI whose furniture this does not know.
        assert_eq!(screen_activity("$ ls\nREADME.md\n$ ", 10_000, false), None);
    }

    #[test]
    fn typing_then_sending_empties_the_box() {
        let io = io();
        io.note_human_input(b"what about this one");
        assert!(io.human_composing(), "mid-sentence is composing");
        io.note_human_input(b"\r");
        assert!(!io.human_composing(), "Enter submits the line, so the box is empty");
    }

    #[test]
    fn backspacing_to_empty_stops_composing() {
        let io = io();
        io.note_human_input(b"ab");
        io.note_human_input(&[0x7f, 0x7f]); // two deletes
        assert!(!io.human_composing());
    }

    #[test]
    fn ctrl_u_and_ctrl_c_clear_the_line() {
        for clear in [0x15u8, 0x03] {
            let io = io();
            io.note_human_input(b"a long half-written thing");
            io.note_human_input(&[clear]);
            assert!(!io.human_composing(), "{clear:#x} clears the line");
        }
    }

    #[test]
    fn moving_the_cursor_is_not_typing() {
        let io = io();
        // Up/Down/Left/Right: CSI sequences with no printable text in them.
        io.note_human_input(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert!(!io.human_composing());
    }

    #[tokio::test]
    async fn inject_waits_while_a_human_is_typing() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let io = AgentIo::new(crate::ctx::IoKind::Pty, Some(tx), None);
        io.note_human_input(b"half a sentence");
        let err = inject(&io, &[env(1, "Ada", "ping")]).await.unwrap_err();
        assert!(err.to_string().contains("typing"), "should defer: {err}");
    }

    #[tokio::test]
    async fn inject_proceeds_once_the_line_is_sent() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let io = AgentIo::new(crate::ctx::IoKind::Pty, Some(tx), None);
        io.note_human_input(b"half a sentence\r"); // typed, then Enter
        let acked = inject(&io, &[env(1, "Ada", "ping")]).await.unwrap();
        assert_eq!(acked, vec![1], "an empty box does not hold the message back");
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
