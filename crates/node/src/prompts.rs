//! Answering a CLI's own permission prompt when nobody is attached.
//!
//! A fleet seat is driven, not watched. When a CLI stops to ask whether it may
//! run a tool, the agent is finished until something answers: the hub's next
//! message is not delivered either, because `driver::awaiting_answer` refuses
//! to type at a screen waiting for a choice -- an injected message must never
//! be the keystroke that approves a tool call nobody read.
//!
//! So when there is nobody who COULD read it -- no viewer attached to that
//! terminal -- the node answers it, and says so. Two rules make that safe
//! enough to do unattended:
//!
//!   * the narrowest affirmative, never the widest. `1. Yes` and `1. Allow`
//!     are taken; `Yes, and don't ask again` and `Always allow` are not,
//!     because a decision taken for somebody must not also be a decision
//!     taken for every future case.
//!   * a shape it does not recognise is left alone and reported instead. A
//!     guessed keystroke at an unknown prompt is how an agent ends up having
//!     approved something nobody chose.
//!
//! Every answer is written into the agent's own transcript and sent to the hub
//! as a note, so it appears in the session's chat: a decision made on your
//! behalf while you were not looking is only acceptable if you can find it
//! afterwards.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nexmesh_proto::NodeToHub;

use crate::ctx::{AgentIo, IoKind, NodeCtx};
use crate::driver::awaiting_answer;

/// What to press, and what that choice says.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The keystrokes, in order. A menu takes its digit; a dialog that says
    /// "enter to submit" needs the Enter afterwards, which is sent only if
    /// the prompt is still up.
    pub keys: Vec<u8>,
    /// The option's own words, for the record.
    pub label: String,
}

/// Strip ANSI so the text can be read, keeping the lines.
fn plain(screen: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in crate::driver::strip_ansi(screen).lines() {
        let t = line.trim().trim_start_matches(['│', '┃', '|']).trim();
        let t = t.trim_start_matches(['❯', '>', '*']).trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out
}

/// Is this option's text an affirmative, and is it the NARROW one?
///
/// `Yes` and `Allow` are narrow. `Always`, `don't ask again`, `for this
/// session` and `for future` widen the answer beyond the question asked, so
/// they are never chosen here even though they are affirmative.
fn narrow_affirmative(text: &str) -> bool {
    let low = text.to_lowercase();
    let widens = ["always", "don't ask", "dont ask", "do not ask", "for this session",
                  "for future", "remember", "every", "all future"];
    if widens.iter().any(|w| low.contains(w)) {
        return false;
    }
    let negatives = ["no", "cancel", "reject", "deny", "abort", "quit", "esc"];
    let first = low.split_whitespace().next().unwrap_or("");
    if negatives.contains(&first) {
        return false;
    }
    let yes = ["yes", "allow", "approve", "accept", "continue", "proceed", "trust", "ok"];
    yes.iter().any(|y| first == *y || low.starts_with(&format!("{y} ")))
}

/// Read a numbered menu option: `1. Yes`, `2) No`.
fn option(line: &str) -> Option<(u8, String)> {
    let mut c = line.chars();
    let d = c.next()?;
    let sep = c.next()?;
    if !d.is_ascii_digit() || !(sep == '.' || sep == ')') {
        return None;
    }
    let rest = line[2..].trim();
    if rest.is_empty() {
        return None;
    }
    Some((d as u8, rest.to_string()))
}

/// What to press at this screen, if anything.
pub fn answer_for(screen: &str) -> Option<Answer> {
    // The same detector the typing guard uses, so the two can never disagree
    // about whether a screen is waiting for an answer.
    awaiting_answer(screen)?;
    let lines = plain(screen);
    let tail: Vec<&String> = lines.iter().rev().take(10).collect();

    // A numbered menu: take the lowest-numbered narrow affirmative.
    let mut best: Option<(u8, String)> = None;
    for line in &tail {
        if let Some((digit, text)) = option(line) {
            if narrow_affirmative(&text) && best.as_ref().map(|(d, _)| digit < *d).unwrap_or(true) {
                best = Some((digit, text));
            }
        }
    }
    if let Some((digit, text)) = best {
        return Some(Answer { keys: vec![digit], label: format!("{}. {text}", digit as char) });
    }

    // A yes/no question, which takes a letter and an Enter.
    for line in &tail {
        let low = line.to_lowercase();
        if low.contains("(y/n)") || low.contains("[y/n]") || low.contains("(yes/no)") {
            return Some(Answer { keys: vec![b'y'], label: "y".into() });
        }
    }

    // "press enter to continue" wants exactly that and nothing else.
    for line in &tail {
        if line.to_lowercase().contains("press enter to") {
            return Some(Answer { keys: vec![b'\r'], label: "Enter".into() });
        }
    }
    None
}

/// What has already been dealt with, so a prompt is never answered twice and
/// an unknown one is reported once rather than every second.
static SEEN: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn fingerprint(screen: &str) -> u64 {
    // The last few meaningful lines are the prompt; the rest of the screen
    // moves on its own (clocks, spinners) and would defeat any memory.
    let lines = plain(screen);
    let tail = lines.iter().rev().take(6).cloned().collect::<Vec<_>>().join("\n");
    let mut h: u64 = 1469598103934665603;
    for b in tail.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn already(agent_id: &str, fp: u64) -> bool {
    let mut g = SEEN.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    if map.get(agent_id) == Some(&fp) {
        return true;
    }
    map.insert(agent_id.to_string(), fp);
    false
}

/// The screen has moved on, so whatever was remembered about it is stale.
///
/// Without this the memory would be a memory of SHAPES, and the second time
/// an agent asked the same question -- which is the normal case, since a CLI
/// asks the same way every time -- it would go unanswered for ever.
fn forget(agent_id: &str) {
    let mut g = SEEN.lock().unwrap();
    if let Some(map) = g.as_mut() {
        map.remove(agent_id);
    }
}

/// Answer a prompt on this agent's screen, if there is one and nobody is
/// attached to answer it themselves.
pub async fn manage_if_unattached(ctx: &Arc<NodeCtx>, agent_id: &str, io: &Arc<AgentIo>) {
    if io.kind != IoKind::Pty {
        return;
    }
    // Somebody is looking at this terminal: the choice is theirs, and taking
    // it out from under them would be worse than waiting.
    if !io.attach_ids().is_empty() {
        return;
    }
    let screen = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
    let Some(why) = awaiting_answer(&screen) else {
        forget(agent_id);
        return;
    };
    let fp = fingerprint(&screen);
    if already(agent_id, fp) {
        return;
    }
    let question = plain(&screen).into_iter().rev().take(6).collect::<Vec<_>>().join(" / ");
    let Some(answer) = answer_for(&screen) else {
        let text = format!(
            "[nexmesh] this seat is waiting for an answer and nobody is attached, and the \
             prompt is not one I will answer for you ({why}): {question}"
        );
        note(ctx, agent_id, io, &text).await;
        return;
    };
    let Some(tx) = &io.input else { return };
    if tx.send(answer.keys.clone()).await.is_err() {
        return;
    }
    // Some TUIs act on the digit; others say "enter to submit". Look again
    // before pressing Enter, so a dialog that has already closed does not get
    // an empty line typed at whatever is behind it.
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let after = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
    let mut submitted = false;
    if awaiting_answer(&after).is_some() && fingerprint(&after) == fp {
        let _ = tx.send(vec![b'\r']).await;
        submitted = true;
    }
    let text = format!(
        "[nexmesh] answered a prompt for you: chose \"{}\"{} because nobody was attached. \
         The question was: {question}",
        answer.label,
        if submitted { " and pressed Enter" } else { "" }
    );
    note(ctx, agent_id, io, &text).await;
}

/// Into the agent's own transcript, into any attached viewer, and into the
/// session's chat. The first two go through the same path the agent's own
/// output goes through, so the note sits in the scrollback where it happened.
async fn note(ctx: &Arc<NodeCtx>, agent_id: &str, io: &Arc<AgentIo>, text: &str) {
    let line = format!("\r\n{text}\r\n");
    crate::spawn::emit_output(ctx, agent_id, io, line.as_bytes(), 0).await;
    let _ = ctx
        .send_hub(&NodeToHub::Noted {
            agent_id: agent_id.to_string(),
            text: text.to_string(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(screen: &str) -> Option<String> {
        answer_for(screen).map(|a| a.label)
    }

    #[test]
    fn a_codex_tool_dialog_is_allowed_once_and_not_for_ever() {
        // The real thing, as it arrived off a live seat.
        let screen = "Called nexmesh.claim_messages\n\
                      Allow Codex to run this tool?\n\
                      \u{276f} 1. Allow  Run the tool and continue\n\
                      2. Allow for this session  remember this choice for this session\n\
                      3. Always allow  remember this choice for future tool calls\n\
                      4. Cancel  Cancel this tool call\n\
                      enter to submit | esc to cancel";
        assert_eq!(label(screen).as_deref(), Some("1. Allow  Run the tool and continue"));
    }

    #[test]
    fn a_claude_edit_dialog_takes_the_plain_yes() {
        let screen = "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}\n\
                      | Do you want to make this edit?\n\
                      | \u{276f} 1. Yes\n\
                      |   2. Yes, and don't ask again this session\n\
                      |   3. No, tell Claude what to do differently\n\
                      \u{2570}\u{2500}\u{2500}\u{2500}\u{256f}";
        assert_eq!(label(screen).as_deref(), Some("1. Yes"));
    }

    #[test]
    fn a_menu_whose_only_affirmative_widens_the_answer_is_left_alone() {
        let screen = "Trust the files in this folder?\n\
                      1. Always trust every folder on this machine\n\
                      2. No";
        assert_eq!(label(screen), None);
    }

    #[test]
    fn a_yes_no_question_takes_a_letter() {
        assert_eq!(label("Overwrite the file? (y/n)").as_deref(), Some("y"));
    }

    #[test]
    fn press_enter_to_continue_is_answered_with_enter() {
        assert_eq!(label("Update available.\npress enter to continue").as_deref(), Some("Enter"));
    }

    #[test]
    fn a_question_in_the_agents_own_prose_is_not_a_prompt() {
        // No menu, no (y/n): the agent is talking, not asking the terminal.
        assert_eq!(label("Do you want me to deploy it? I can do either.\n> "), None);
    }

    #[test]
    fn the_same_prompt_is_answered_once() {
        let screen = "Do you want to make this edit?\n1. Yes\n2. No";
        let fp = fingerprint(screen);
        assert!(!already("ag-test-once", fp));
        assert!(already("ag-test-once", fp), "a second look must not answer it again");
        let moved = "Do you want to make this edit?\n1. Yes\n2. No\nEdit applied.\n> ";
        assert!(!already("ag-test-once", fingerprint(moved)),
                "a screen that has moved on is a new question");
    }

    #[test]
    fn the_same_question_asked_again_is_answered_again() {
        // A CLI asks the same way every time, so a memory of shapes would
        // answer the first permission prompt of an agent's life and none of
        // the others.
        let screen = "Do you want to make this edit?\n1. Yes\n2. No";
        let fp = fingerprint(screen);
        assert!(!already("ag-test-again", fp));
        forget("ag-test-again");
        assert!(!already("ag-test-again", fp),
                "once the screen moved on, the next identical question is a new one");
    }
}
