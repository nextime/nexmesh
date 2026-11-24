//! A CLI's own permission prompt, when nobody is attached.
//!
//! A fleet seat is driven, not watched. When a CLI stops to ask whether it may
//! run a tool, the agent is finished until something answers: the hub's next
//! message is not delivered either, because `driver::awaiting_answer` refuses
//! to type at a screen waiting for a choice -- an injected message must never
//! be the keystroke that approves a tool call nobody read.
//!
//! This module used to ANSWER those prompts itself, taking the narrowest
//! affirmative. That kept the fleet moving and quietly meant nobody had
//! authorised anything: the operator found out afterwards, from a note, if
//! they went looking.
//!
//! So the question goes to the operator instead. The node reads the menu,
//! reports it with the choices the CLI actually offered, and waits. The hub
//! puts it in the operator's chat as buttons and notifies them; if a standing
//! auto-approval covers this seat or its whole team, the hub answers at once
//! and the wait is invisible.
//!
//! Two rules survive from the old behaviour and now govern what an
//! AUTO-approval may pick:
//!
//!   * the narrowest affirmative, never the widest. `1. Yes` and `1. Allow`
//!     are takeable; `Yes, and don't ask again` and `Always allow` are not,
//!     because "answer this prompt for me" is not "let the agent turn its own
//!     guardrails off".
//!   * a shape the node cannot read is never guessed at. It is reported with
//!     no options, and the operator reads the screen themselves.
//!
//! Nothing here times out into "allowed". A pending approval is a stopped
//! agent, which is the honest state: it is waiting for a person.

use std::collections::{HashMap, HashSet};
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
    affirmative(text) && !widens(text)
}

/// Does this option grant more than the question asked?
///
/// Reported to the operator as well as used here: a button that reads "Yes"
/// but also means "and never ask me again" is the one mistake this whole
/// path exists to prevent, so it is labelled, not hidden.
fn widens(text: &str) -> bool {
    let low = text.to_lowercase();
    ["always", "don't ask", "dont ask", "do not ask", "for this session",
     "for future", "remember", "every", "all future", "all edits", "yolo"]
        .iter()
        .any(|w| low.contains(w))
}

/// Does picking this let the action proceed?
fn affirmative(text: &str) -> bool {
    let low = text.to_lowercase();
    // Punctuation is part of the word on a real menu: `No,` and `Yes,` are
    // how both of these actually appear.
    let word = |w: &str| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
    let words: Vec<String> = low.split_whitespace().map(|w| word(w)).collect();
    let first = words.first().map(String::as_str).unwrap_or("");
    if ["no", "cancel", "reject", "deny", "abort", "quit", "esc", "stop", "never"]
        .contains(&first)
    {
        return false;
    }
    // Anywhere in the option, not only at the front. `Always allow` and
    // `Allow for this session` are both affirmatives that happen not to
    // START with the affirmative word -- and reading the first of those as a
    // NEGATIVE was a real bug: it made "Always allow" the option that
    // `/deny` would have picked.
    let yes = ["yes", "allow", "approve", "accept", "continue", "proceed", "trust", "ok"];
    words.iter().any(|w| yes.contains(&w.as_str()))
}

/// Every choice this screen offers, in the CLI's own words.
///
/// This is what becomes the operator's buttons, so it is a reading of the
/// menu rather than a decision about it: the affirmative and the widening
/// ones are flagged and all of them are passed on. An empty list means the
/// node could not read the screen, which is reported as exactly that.
pub fn options_for(screen: &str) -> Vec<nexmesh_proto::PromptOption> {
    use nexmesh_proto::PromptOption;
    let lines = plain(screen);
    let tail: Vec<&String> = lines.iter().rev().take(12).collect();

    // A numbered menu: the LAST run of option lines on the screen, read in
    // screen order so the buttons come out the way the dialog does.
    //
    // The last run, not every numbered line in the tail, because a terminal's
    // scrollback is append-only: a dialog that has just been answered is
    // still up there, above the one the agent is stopped on now. Collecting
    // digits across the whole tail took the FIRST `1.` it found, which is the
    // old dialog's -- so the operator was offered "1. Yes" and "2. No" as
    // the choices for a question that was really offering "1. Run them" and
    // "2. Not now". The keystroke still landed on the live dialog; the words
    // next to it were the wrong dialog's, which is the one thing these
    // buttons must never be.
    let mut group: Vec<(u8, String)> = Vec::new();
    for line in &tail {
        match option(line) {
            Some(found) => group.push(found),
            // Anything else ENDS the run, but only once it has begun: what
            // comes between the end of the screen and the menu is the box
            // border, the hint line, and whatever the agent last printed.
            None if !group.is_empty() => break,
            None => {}
        }
    }
    let mut opts: Vec<PromptOption> = Vec::new();
    for (digit, text) in group.into_iter().rev() {
        let key = (digit as char).to_string();
        if opts.iter().any(|o| o.key == key) {
            continue;
        }
        opts.push(PromptOption {
            key: key.clone(),
            label: text.clone(),
            keys: key,
            affirmative: affirmative(&text),
            widens: widens(&text),
        });
    }
    if !opts.is_empty() {
        return opts;
    }

    // A yes/no question takes a letter and an Enter.
    for line in &tail {
        let low = line.to_lowercase();
        if low.contains("(y/n)") || low.contains("[y/n]") || low.contains("(yes/no)") {
            return vec![
                PromptOption { key: "y".into(), label: "Yes".into(), keys: "y".into(),
                               affirmative: true, widens: false },
                PromptOption { key: "n".into(), label: "No".into(), keys: "n".into(),
                               affirmative: false, widens: false },
            ];
        }
    }

    // "press enter to continue" wants exactly that and nothing else. There is
    // no way to decline it, so only one button is offered.
    for line in &tail {
        if line.to_lowercase().contains("press enter to") {
            return vec![PromptOption {
                key: "enter".into(), label: "Press Enter".into(), keys: "\r".into(),
                affirmative: true, widens: false,
            }];
        }
    }
    Vec::new()
}

/// What a standing auto-approval picks: the narrowest affirmative on offer.
///
/// `None` when nothing on the screen qualifies -- an auto-approval that could
/// not find a safe affirmative waits for the operator like any other prompt,
/// rather than pressing the only button it can see.
pub fn auto_choice(opts: &[nexmesh_proto::PromptOption]) -> Option<&nexmesh_proto::PromptOption> {
    opts.iter().filter(|o| o.affirmative && !o.widens).min_by_key(|o| o.key.clone())
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

/// When this agent's prompt was last answered.
///
/// A terminal's scrollback is append-only, so for a moment after the answer
/// the dialog is STILL the last thing on the screen -- the CLI has not
/// redrawn yet, and `awaiting_answer` looks at the tail. Without this the
/// same dialog is raised again the instant it is dealt with, and the operator
/// gets a second copy of a question they just answered.
static ANSWERED: Mutex<Option<HashMap<String, std::time::Instant>>> = Mutex::new(None);

/// How long to let a CLI redraw before believing its screen again.
const REDRAW_GRACE: std::time::Duration = std::time::Duration::from_secs(6);

fn just_answered(agent_id: &str) -> bool {
    ANSWERED
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(agent_id))
        .map(|t| t.elapsed() < REDRAW_GRACE)
        .unwrap_or(false)
}

fn mark_answered(agent_id: &str) {
    let mut g = ANSWERED.lock().unwrap();
    g.get_or_insert_with(HashMap::new)
        .insert(agent_id.to_string(), std::time::Instant::now());
}

/// Just the words of a line: letters, lowercased, one space between them.
///
/// What identifies a question is what it SAYS. Everything else on the line
/// moves by itself -- and a seat running under tmux has tmux's own status
/// line on its screen, with a CLOCK in it. Hashing the tail as it stood made
/// the fingerprint change every single minute, so one dialog was reported as
/// a new question sixty times an hour: that is where a pile of 77 pending
/// cards for three long-dead seats came from. Spinners, elapsed times and
/// token counters do the same thing faster.
///
/// The cost is that two questions differing ONLY in a number read as one
/// question here. They offer the same choices and the same keystrokes, so the
/// button still answers the dialog that is actually on screen; what the
/// operator reads is the wording of the first of them.
fn words(line: &str) -> String {
    let mut out = String::new();
    for c in line.chars() {
        if c.is_alphabetic() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(' ') {
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn fingerprint(screen: &str) -> u64 {
    // The last few meaningful lines are the prompt; the rest of the screen
    // moves on its own (clocks, spinners) and would defeat any memory.
    let lines = plain(screen);
    let tail = lines
        .iter()
        .rev()
        .take(6)
        .map(|l| words(l))
        .collect::<Vec<_>>()
        .join("\n");
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

/// Seats the hub has been told have nothing outstanding.
///
/// Separate from `SEEN` because "nothing is up" is a state worth reporting in
/// its own right, not just the absence of one. A node restart loses `SEEN`
/// while the hub's row outlives the process, so without this a prompt
/// answered at the terminal during a node restart would keep its live buttons
/// in the dashboard for ever: nothing would ever contradict it.
static CLEARED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn told_clear(agent_id: &str) -> bool {
    CLEARED.lock().unwrap().as_ref().map(|s| s.contains(agent_id)).unwrap_or(false)
}

fn mark_clear(agent_id: &str) {
    CLEARED.lock().unwrap().get_or_insert_with(HashSet::new).insert(agent_id.to_string());
}

/// This seat is asking something, so the hub's picture is no longer "clear".
fn unmark_clear(agent_id: &str) {
    if let Some(s) = CLEARED.lock().unwrap().as_mut() {
        s.remove(agent_id);
    }
}

/// Is there anything to tell the hub about this seat having no question up?
///
/// A seat we raised something for: yes, retire it. A seat we have already
/// reported clear: no, and that is what keeps a quiet fleet quiet. A seat we
/// know nothing about: yes, once -- this process may be a restarted node
/// looking at a seat whose card is still in the dashboard.
fn needs_clear(agent_id: &str) -> bool {
    raised_fingerprint(agent_id).is_some() || !told_clear(agent_id)
}

/// The shape currently with the operator, if any. Used to retire a request
/// that nobody can answer any more -- the prompt went away, or somebody
/// attached and took the decision at the terminal.
fn raised_fingerprint(agent_id: &str) -> Option<u64> {
    SEEN.lock().unwrap().as_ref().and_then(|m| m.get(agent_id).copied())
}

/// Idempotent: `already` has normally recorded it, and this makes the
/// intention legible at the call site that depends on it.
fn remember_raised(agent_id: &str, fp: u64) {
    let mut g = SEEN.lock().unwrap();
    g.get_or_insert_with(HashMap::new).insert(agent_id.to_string(), fp);
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
    // it out from under them would be worse than waiting. If we had already
    // raised it with the operator, retire that -- two people answering the
    // same dialog is how one of them answers whatever came after it.
    if !io.attach_ids().is_empty() {
        retire(ctx, agent_id).await;
        return;
    }
    let screen = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
    let Some(why) = awaiting_answer(&screen) else {
        // The prompt is gone -- answered at its own terminal, or the agent
        // gave up. Anything still showing buttons in the dashboard is stale.
        retire(ctx, agent_id).await;
        forget(agent_id);
        return;
    };
    // We answered something here a moment ago and the CLI has not redrawn
    // yet, so what is on the tail of the screen is the dialog we just dealt
    // with. Raising it again would hand the operator a second copy of a
    // question they have already answered.
    if just_answered(agent_id) {
        return;
    }
    let fp = fingerprint(&screen);
    // Already with the operator (or already reported as unreadable): one
    // dialog is raised once, however many idle ticks notice it.
    if already(agent_id, fp) {
        return;
    }
    let question = plain(&screen).into_iter().rev().take(8).collect::<Vec<_>>().join(" / ");
    let options = options_for(&screen);
    if options.is_empty() {
        // Not a shape the node can read. Reported with no options rather
        // than guessed at: the operator reads the screen themselves, and
        // `nexmesh attach` is how.
        let text = format!(
            "[nexmesh] this seat is waiting for an answer, nobody is attached, and I \
             cannot read the choices it is offering ({why}). Attach to answer it: {question}"
        );
        note(ctx, agent_id, io, &text).await;
        // Still raised, so it appears in the chat and can be notified on --
        // just with nothing to click.
        unmark_clear(agent_id);
        let _ = ctx
            .send_hub(&NodeToHub::ApprovalAsked {
                agent_id: agent_id.to_string(),
                kind: "pty_prompt".into(),
                question,
                options,
                fingerprint: format!("{fp:016x}"),
            })
            .await;
        return;
    }
    remember_raised(agent_id, fp);
    unmark_clear(agent_id);
    let _ = ctx
        .send_hub(&NodeToHub::ApprovalAsked {
            agent_id: agent_id.to_string(),
            kind: "pty_prompt".into(),
            question,
            options,
            fingerprint: format!("{fp:016x}"),
        })
        .await;
}

/// Type the answer the operator (or a standing auto-approval) chose.
///
/// The keys come back from the hub exactly as this node reported them, so
/// nothing in between has interpreted the menu. The screen is re-checked
/// before the Enter: a dialog that closed on the digit alone must not get an
/// empty line typed at whatever is behind it.
pub async fn apply_decision(
    ctx: &Arc<NodeCtx>,
    agent_id: &str,
    io: &Arc<AgentIo>,
    keys: &str,
    label: &str,
    by: &str,
) {
    forget(agent_id);
    mark_answered(agent_id);
    if keys.is_empty() {
        note(ctx, agent_id, io, &format!(
            "[nexmesh] the permission request was withdrawn ({by}); nothing was typed"
        )).await;
        return;
    }
    let Some(tx) = &io.input else { return };
    let before = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
    // Somebody attached and answered it in the meantime, or the agent moved
    // on: typing now would land on whatever is there instead.
    if awaiting_answer(&before).is_none() {
        note(ctx, agent_id, io, &format!(
            "[nexmesh] \"{label}\" arrived ({by}) but the prompt had already gone; \
             nothing was typed"
        )).await;
        return;
    }
    let keys_of = |s: &str| {
        options_for(s).into_iter().map(|o| o.key).collect::<Vec<_>>()
    };
    let was_offering = keys_of(&before);
    if tx.send(keys.as_bytes().to_vec()).await.is_err() {
        return;
    }
    // Some TUIs act on the digit; others say "enter to submit". An option
    // whose keystroke IS Enter has already submitted.
    let mut submitted = keys.contains('\r');
    if !submitted {
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        let after = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
        // Still the SAME dialog, so the Enter belongs to it.
        //
        // This used to compare a fingerprint of the whole screen, which the
        // terminal's own echo of the keystroke we just sent reliably broke:
        // the digit appears as a new line, the fingerprint changes, the
        // Enter is withheld, and a pty in canonical mode never delivers the
        // digit at all -- the agent sits on an answered dialog for ever.
        // What matters is not that the screen is unchanged but that the
        // question still is, so compare the options it is offering.
        if awaiting_answer(&after).is_some() && keys_of(&after) == was_offering {
            let _ = tx.send(vec![b'\r']).await;
            submitted = true;
        }
    }
    let who = match by {
        "operator" => "you chose it in the chat".to_string(),
        "auto:agent" => "this seat is set to auto-approve".to_string(),
        "auto:session" => "this whole team is set to auto-approve".to_string(),
        other => other.to_string(),
    };
    note(ctx, agent_id, io, &format!(
        "[nexmesh] answered the prompt with \"{label}\"{} -- {who}",
        if submitted { " and pressed Enter" } else { "" }
    )).await;
}

/// Type a literal line at a CLI, as a person would.
///
/// Guarded exactly like a delivered message: nothing is typed at a screen
/// that is waiting for an answer, because the Enter after it would answer
/// that question instead -- usually by accepting it.
pub async fn type_line(io: &Arc<AgentIo>, line: &str) -> Result<(), String> {
    if io.kind != IoKind::Pty {
        return Err("this seat has no terminal to type at".into());
    }
    let Some(tx) = &io.input else {
        return Err("this seat's terminal takes no input".into());
    };
    let screen = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
    if let Some(why) = awaiting_answer(&screen) {
        return Err(format!("its terminal is waiting for an answer ({why})"));
    }
    tx.send(line.as_bytes().to_vec())
        .await
        .map_err(|_| "its terminal is closed".to_string())?;
    // The Enter is its own keystroke a moment later, never the tail of the
    // same write: a fast burst reads as a PASTE, and a carriage return
    // inside a paste is a newline in the box rather than a submit.
    tokio::time::sleep(SUBMIT_PAUSE_AFTER_TYPING).await;
    tx.send(vec![b'\r'])
        .await
        .map_err(|_| "its terminal is closed".to_string())?;
    Ok(())
}

const SUBMIT_PAUSE_AFTER_TYPING: std::time::Duration = std::time::Duration::from_millis(300);

/// Say something in the agent's own scrollback and in the session's chat.
///
/// The same path `note` uses; public because the operator's own actions --
/// a compaction they asked for -- belong in the same place as the node's.
pub async fn say(ctx: &Arc<NodeCtx>, agent_id: &str, io: &Arc<AgentIo>, text: &str) {
    note(ctx, agent_id, io, text).await;
}

/// Tell the hub this seat has no question a keystroke from here could answer.
///
/// Sent at most once per transition, and with no fingerprint when this
/// process never raised anything -- the hub clears the seat, not one dialog,
/// precisely so that a node which has forgotten (or never knew) what was
/// raised can still retire a card nobody can answer any more.
async fn retire(ctx: &Arc<NodeCtx>, agent_id: &str) {
    if !needs_clear(agent_id) {
        return;
    }
    withdraw(ctx, agent_id, raised_fingerprint(agent_id)).await;
}

async fn withdraw(ctx: &Arc<NodeCtx>, agent_id: &str, fp: Option<u64>) {
    let _ = ctx
        .send_hub(&NodeToHub::ApprovalWithdrawn {
            agent_id: agent_id.to_string(),
            fingerprint: fp.map(|f| format!("{f:016x}")).unwrap_or_default(),
        })
        .await;
    forget(agent_id);
    mark_clear(agent_id);
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

    const CODEX: &str = "Called nexmesh.claim_messages\n\
                         Allow Codex to run this tool?\n\
                         \u{276f} 1. Allow  Run the tool and continue\n\
                         2. Allow for this session  remember this choice for this session\n\
                         3. Always allow  remember this choice for future tool calls\n\
                         4. Cancel  Cancel this tool call\n\
                         enter to submit | esc to cancel";

    #[test]
    fn the_operator_is_shown_every_choice_the_cli_offered() {
        // Not a decision about the menu: a reading of it. All four options
        // reach the operator, in screen order, in the CLI's own words.
        let opts = options_for(CODEX);
        assert_eq!(opts.len(), 4, "{opts:#?}");
        assert_eq!(opts.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
                   ["1", "2", "3", "4"]);
        assert!(opts[0].label.starts_with("Allow  Run the tool"), "{:?}", opts[0].label);
        assert_eq!(opts[0].keys, "1");
    }

    #[test]
    fn the_choices_are_the_live_dialogs_and_not_the_one_above_it() {
        // A terminal's scrollback is append-only, so a dialog answered at the
        // keyboard a moment ago is still on the screen above the one the
        // agent is stopped on now. The buttons have to belong to the live
        // one: offering the previous dialog's WORDS beside this dialog's
        // keystrokes is the single thing this screen must never do.
        let screen = "\u{256d}\u{2500}\u{256e}\n\
                      | Do you want to make this edit?\n\
                      | 1. Yes\n\
                      | 2. No\n\
                      \u{2570}\u{2500}\u{256f}\n\
                      Edit applied.\n\
                      \u{256d}\u{2500}\u{256e}\n\
                      | Do you want to run the tests?\n\
                      | 1. Run them\n\
                      | 2. Not now\n\
                      \u{2570}\u{2500}\u{256f}";
        let opts = options_for(screen);
        assert_eq!(opts.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
                   ["Run them", "Not now"], "{opts:#?}");
    }

    #[test]
    fn an_option_that_grants_more_than_the_question_is_flagged_as_such() {
        // So a button reading "Allow for this session" is never styled, or
        // auto-chosen, as if it were a plain yes.
        let opts = options_for(CODEX);
        let by = |k: &str| opts.iter().find(|o| o.key == k).unwrap();
        assert!(by("1").affirmative && !by("1").widens, "{:?}", by("1"));
        assert!(by("2").affirmative && by("2").widens, "{:?}", by("2"));
        assert!(by("3").affirmative && by("3").widens, "{:?}", by("3"));
        assert!(!by("4").affirmative, "{:?}", by("4"));
    }

    #[test]
    fn an_auto_approval_takes_the_narrowest_affirmative_and_nothing_wider() {
        let opts = options_for(CODEX);
        let pick = auto_choice(&opts).expect("there is a narrow yes here");
        assert_eq!(pick.key, "1");
        assert!(!pick.widens);
    }

    #[test]
    fn an_auto_approval_declines_to_choose_when_every_yes_widens() {
        // It waits for the operator rather than pressing the only button it
        // can see. This is the case the old auto-answering got right and is
        // the reason auto-approve can be left on safely.
        let screen = "Trust the files in this folder?\n\
                      1. Always trust every folder on this machine\n\
                      2. No";
        let opts = options_for(screen);
        assert_eq!(opts.len(), 2);
        assert!(auto_choice(&opts).is_none(), "{opts:#?}");
    }

    #[test]
    fn a_yes_no_question_offers_both_answers() {
        let opts = options_for("Overwrite the file? (y/n)");
        assert_eq!(opts.iter().map(|o| o.keys.as_str()).collect::<Vec<_>>(), ["y", "n"]);
        assert!(opts[0].affirmative && !opts[1].affirmative);
    }

    #[test]
    fn press_enter_to_continue_offers_the_one_thing_it_accepts() {
        let opts = options_for("press enter to continue");
        assert_eq!(opts.len(), 1, "{opts:#?}");
        assert_eq!(opts[0].keys, "\r");
    }

    #[test]
    fn a_screen_that_is_not_a_prompt_offers_nothing() {
        assert!(options_for("I wonder whether we should rebase?").is_empty());
    }

    #[test]
    fn a_shape_with_no_readable_choices_is_not_guessed_at() {
        // Reported to the operator with no buttons rather than answered.
        let opts = options_for("Do you want to continue with the migration");
        assert!(opts.is_empty(), "{opts:#?}");
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
    fn a_clock_on_the_screen_is_not_a_new_question() {
        // The real screen this came off: a tmux status line, with the time in
        // it, underneath a claude dialog. One question, read a minute apart,
        // used to be two -- and then 77, one per minute, none of which could
        // ever be retired because only the newest was remembered.
        let at = |t: &str| format!(
            "Do you want to make this edit?\n1. Yes\n2. No\n\
             [nexmesh-s0:claude* \"Claude Code\" {t} 19-Nov-26"
        );
        assert_eq!(fingerprint(&at("20:50")), fingerprint(&at("20:51")));
        // ...and a different question is still a different question.
        assert_ne!(fingerprint(&at("20:50")),
                   fingerprint("Do you want to run this command?\n1. Yes\n2. No"));
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
    fn the_hub_is_told_once_that_a_seat_has_nothing_up_and_once_more_after_a_restart() {
        // Three cases, in order: a seat this process knows nothing about --
        // which is every seat after a node restart, and the reason a stale
        // card can be cleared at all; the same seat once the hub has been
        // told, where saying it again every 1.2 seconds would be the whole
        // fleet chattering about nothing; and a seat with a question actually
        // raised, which must be retired when that question goes away.
        let id = "ag-test-clear";
        assert!(needs_clear(id), "a seat nothing is known about is reported once");
        mark_clear(id);
        assert!(!needs_clear(id), "and not again while nothing changes");
        remember_raised(id, 42);
        assert!(needs_clear(id), "something was raised, so it has to be retired");
        unmark_clear(id);
        forget(id);
        assert!(needs_clear(id));
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
