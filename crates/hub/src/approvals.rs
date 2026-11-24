//! Permission prompts waiting for the operator.
//!
//! A CLI stops and asks before it runs a command, edits a file or calls a
//! tool. When somebody is attached to its terminal that question is theirs.
//! When nobody is, the node used to answer the narrowest affirmative itself:
//! the fleet kept moving and nobody had authorised anything.
//!
//! Now the question arrives here. This module decides whether a standing
//! auto-approval covers it -- in which case it is answered at once and the
//! operator sees what was decided -- and otherwise puts it in the operator's
//! chat with the choices the CLI actually offered, and waits.
//!
//! Nothing in here times out into "allowed". A pending approval is a stopped
//! agent, which is the honest state: it is waiting for a person.

use anyhow::{bail, Result};
use nexmesh_proto::{now_ms, HubToNode, PromptOption};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::state::AppState;

/// Where a prompt came from, resolved once so everything below agrees.
struct Asker {
    session_id: String,
    slot_id: String,
    name: String,
    node_id: Option<String>,
    /// Is this seat, or its whole team, set to answer without asking?
    auto: Option<&'static str>,
}

fn asker(db: &Connection, agent_id: &str) -> Option<Asker> {
    let row: Option<(String, String, String, Option<String>, i64, i64)> = db
        .query_row(
            "SELECT s.session_id, s.id, COALESCE(s.display_name, s.id), a.node_id,
                    COALESCE(s.auto_approve, 0), COALESCE(se.auto_approve, 0)
               FROM agents a
               JOIN slots s ON s.id = a.slot_id
               JOIN sessions se ON se.id = s.session_id
              WHERE a.id = ?",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (session_id, slot_id, name, node_id, seat_auto, team_auto) = row?;
    Some(Asker {
        session_id,
        slot_id,
        name,
        node_id,
        // The seat's own setting is reported in preference to the team's, so
        // the chat line says which switch did it -- "this whole team is set
        // to auto-approve" is a different thing to go and look at.
        auto: if seat_auto == 1 {
            Some("auto:agent")
        } else if team_auto == 1 {
            Some("auto:session")
        } else {
            None
        },
    })
}

/// One line in the operator's chat, pre-claimed: a record to read, never a
/// message for an agent to answer.
fn chat(db: &Connection, session_id: &str, slot_id: &str, text: &str) -> Option<i64> {
    let now = now_ms();
    db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                               sent_at_ms, claimed_at_ms, claimed_by, record)
         VALUES (?1, ?2, NULL, 'approval', ?3, ?4, ?4, 'hub', 1)",
        params![session_id, slot_id, text, now],
    )
    .ok()?;
    Some(db.last_insert_rowid())
}

/// Retire every request waiting for this seat except `keep`.
///
/// One terminal shows one question. A seat that is asking something NEW has
/// moved on from whatever it was asking before -- answered at its own
/// terminal, or the agent stopped waiting -- so every other pending row for
/// that seat is stale by definition. This is what makes answering at the CLI
/// clear the dashboard even when the agent asks the next question
/// immediately: the node only ever reports the question it can SEE, and
/// without this the one before it stayed on screen, with live buttons, until
/// the operator answered it a second time.
///
/// Returns how many were retired, so the caller can say so out loud.
fn retire_others(db: &Connection, who: &Asker, keep: &str) -> usize {
    db.execute(
        "UPDATE approvals SET status = 'withdrawn', decided_at_ms = ?,
                              decided_by = 'superseded'
          WHERE slot_id = ? AND status = 'pending' AND COALESCE(fingerprint, '') <> ?",
        params![now_ms(), who.slot_id, keep],
    )
    .unwrap_or(0)
}

/// A prompt a node could not answer, arriving from the node.
///
/// Returns the decision to send straight back, when a standing auto-approval
/// covers it. The node is handed the KEYS it itself reported, so nothing in
/// between has interpreted the menu.
pub fn asked(
    st: &AppState,
    agent_id: &str,
    kind: &str,
    question: &str,
    options: &[PromptOption],
    fingerprint: &str,
) -> Option<HubToNode> {
    let db = st.db.lock().unwrap();
    let who = asker(&db, agent_id)?;
    // One dialog, one row: a node re-reports on reconnect, and an operator
    // must not come back to four copies of the same question.
    let existing: Option<String> = db
        .query_row(
            "SELECT id FROM approvals
              WHERE slot_id = ? AND fingerprint = ? AND status = 'pending'",
            params![who.slot_id, fingerprint],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();

    // The same dialog, read again through a screen that moved underneath it.
    //
    // A node fingerprints what it can see, and a seat under tmux has tmux's
    // clock on its screen; the node now ignores numbers, but any CLI that
    // redraws a counter or re-wraps a line can still change what the node
    // reads without the question changing. The choices on offer are what
    // settles it: same keys, same words, same keystrokes -- so this is the
    // request that is already on screen, refreshed in place.
    //
    // Keeping the row (and its id) is the point. A new row would be a new
    // card, a new notification, and another thing to answer, which is what
    // the operator was actually complaining about.
    let opts_json = serde_json::to_string(options).unwrap_or_else(|_| "[]".into());
    let same_choices: Option<String> = if existing.is_some() || options.is_empty() {
        None
    } else {
        db.query_row(
            "SELECT id FROM approvals
              WHERE slot_id = ? AND status = 'pending' AND options = ?
              ORDER BY asked_at_ms DESC LIMIT 1",
            params![who.slot_id, opts_json],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    };
    if let Some(id) = &same_choices {
        let _ = db.execute(
            "UPDATE approvals SET question = ?, fingerprint = ?, agent_id = ?, node_id = ?
              WHERE id = ?",
            params![question, fingerprint, agent_id, who.node_id, id],
        );
    }
    if retire_others(&db, &who, fingerprint) > 0 && same_choices.is_none() {
        // Said out loud: a card that vanishes with no explanation reads as a
        // broken dashboard, and the operator may have been about to click it.
        let _ = chat(
            &db,
            &who.session_id,
            &who.slot_id,
            &format!(
                "\u{1f511} {} is on a different question now \u{2014} the one before it was \
                 answered at its own terminal, or it stopped waiting, so it is no longer \
                 answerable from here.",
                who.name
            ),
        );
    }
    if existing.is_some() || same_choices.is_some() {
        return None;
    }

    let id = format!("ap-{}", &uuid::Uuid::new_v4().to_string()[..12]);

    // A standing auto-approval answers now. It may only ever pick the
    // narrowest affirmative: "answer this prompt for me" is not "let the
    // agent turn its own guardrails off", so an option that widens
    // permission is never chosen even when it is the only affirmative.
    let auto = who.auto.and_then(|by| {
        options
            .iter()
            .filter(|o| o.affirmative && !o.widens)
            .min_by_key(|o| o.key.clone())
            .map(|o| (by, o.clone()))
    });

    if let Some((by, opt)) = auto {
        let text = format!(
            "\u{1f511} {} was asked: {question}\nAnswered \"{}\" automatically \u{2014} {}. \
             Turn it off with /autoapprove {} off.",
            who.name,
            opt.label,
            if by == "auto:agent" { "this seat is set to auto-approve" }
            else { "this whole team is set to auto-approve" },
            who.name,
        );
        let msg = chat(&db, &who.session_id, &who.slot_id, &text);
        let now = now_ms();
        let _ = db.execute(
            "INSERT INTO approvals (id, session_id, slot_id, agent_id, node_id, kind,
                                    question, options, fingerprint, status, decision,
                                    decided_by, asked_at_ms, decided_at_ms, message_id)
             VALUES (?,?,?,?,?,?,?,?,?,'decided',?,?,?,?,?)",
            params![id, who.session_id, who.slot_id, agent_id, who.node_id, kind,
                    question, opts_json, fingerprint, opt.key, by, now, now, msg],
        );
        tracing::info!(%agent_id, slot = %who.slot_id, %by, choice = %opt.label,
                       "a permission prompt was auto-approved");
        return Some(HubToNode::ApprovalDecided {
            approval_id: id,
            agent_id: agent_id.to_string(),
            keys: opt.keys,
            label: opt.label,
            by: by.to_string(),
        });
    }

    // Nobody has pre-authorised this, so it waits. The chat line carries the
    // question; the dashboard turns the stored options into buttons.
    let text = if options.is_empty() {
        format!(
            "\u{1f511} {} is waiting for permission and I cannot read the choices it is \
             offering. Attach to its terminal to answer: {question}",
            who.name
        )
    } else {
        format!("\u{1f511} {} is asking permission: {question}", who.name)
    };
    let msg = chat(&db, &who.session_id, &who.slot_id, &text);
    let _ = db.execute(
        "INSERT INTO approvals (id, session_id, slot_id, agent_id, node_id, kind,
                                question, options, fingerprint, status, asked_at_ms, message_id)
         VALUES (?,?,?,?,?,?,?,?,?,'pending',?,?)",
        params![id, who.session_id, who.slot_id, agent_id, who.node_id, kind,
                question, opts_json, fingerprint, now_ms(), msg],
    );
    tracing::info!(%agent_id, slot = %who.slot_id, approval = %id,
                   options = options.len(), "a permission prompt is waiting for the operator");
    None
}

/// The prompt went away before anyone answered it: the agent gave up, or
/// somebody attached and took the decision at the terminal.
///
/// What the node is reporting is a fact about the SEAT -- it has no question
/// a keystroke from here could answer -- so everything waiting for that seat
/// is cleared, not only the one dialog the node remembers raising. The
/// fingerprint is kept for the record: a node that restarted lost its memory
/// of what it raised and sends none, and the row it is contradicting
/// outlived that process.
pub fn withdrawn(st: &AppState, agent_id: &str, fingerprint: &str) {
    let db = st.db.lock().unwrap();
    let Some(who) = asker(&db, agent_id) else { return };
    let n = db
        .execute(
            "UPDATE approvals SET status = 'withdrawn', decided_at_ms = ?,
                                  decided_by = 'attached'
              WHERE slot_id = ? AND status = 'pending'",
            params![now_ms(), who.slot_id],
        )
        .unwrap_or(0);
    if n > 0 {
        // Said out loud: buttons that stop working without explanation read
        // as a broken dashboard.
        let _ = chat(
            &db,
            &who.session_id,
            &who.slot_id,
            &format!(
                "\u{1f511} {}'s permission request went away before it was answered \u{2014} \
                 it was answered at the terminal, or the agent stopped waiting.",
                who.name
            ),
        );
        tracing::info!(%agent_id, %fingerprint, retired = n,
                       "pending approvals were withdrawn: the seat has no question up");
    }
}

/// Retire every request nobody can answer any more.
///
/// A pending approval is a stopped agent waiting for a person, so it only
/// means anything while that agent is there to be typed at. Three ways it
/// stops being there: the process exited, the seat has been through a new
/// process since, or the agent is not in the fleet at all. In every one of
/// them the buttons can only fail, and a card that can only fail is worse
/// than no card.
///
/// Swept when an agent exits and once when the hub starts. The second is what
/// clears what is already there: a hub holding 77 cards for three seats that
/// died days ago cannot be mended by anything the nodes say, because nothing
/// will ever mention those agents again.
///
/// A DISCONNECTED agent is deliberately kept: its node may come back with the
/// dialog still on the screen, and nothing has been lost meanwhile.
pub fn retire_gone(db: &Connection) -> usize {
    db.execute(
        "UPDATE approvals SET status = 'withdrawn', decided_at_ms = ?1, decided_by = 'gone'
          WHERE status = 'pending'
            AND NOT EXISTS (
                SELECT 1 FROM agents a
                 WHERE a.id = approvals.agent_id
                   AND a.exited_at_ms IS NULL
                   AND a.started_at_ms = (SELECT MAX(x.started_at_ms) FROM agents x
                                           WHERE x.slot_id = a.slot_id))",
        params![now_ms()],
    )
    .unwrap_or(0)
}

/// What the dashboard, the console and the XR view all read.
pub fn pending(db: &Connection, session_id: Option<&str>) -> Vec<serde_json::Value> {
    let sql = "SELECT a.id, a.session_id, a.slot_id, COALESCE(s.display_name, a.slot_id),
                      a.question, a.options, a.kind, a.asked_at_ms, a.agent_id
                 FROM approvals a LEFT JOIN slots s ON s.id = a.slot_id
                WHERE a.status = 'pending' AND (?1 IS NULL OR a.session_id = ?1)
                ORDER BY a.asked_at_ms";
    let Ok(mut stmt) = db.prepare(sql) else { return vec![] };
    stmt.query_map(params![session_id], |r| {
        let opts: String = r.get(5)?;
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "session_id": r.get::<_, String>(1)?,
            "slot_id": r.get::<_, String>(2)?,
            "agent": r.get::<_, String>(3)?,
            "question": r.get::<_, String>(4)?,
            "options": serde_json::from_str::<serde_json::Value>(&opts)
                           .unwrap_or_else(|_| json!([])),
            "kind": r.get::<_, String>(6)?,
            "asked_at_ms": r.get::<_, i64>(7)?,
            "agent_id": r.get::<_, Option<String>>(8)?,
        }))
    })
    .map(|m| m.filter_map(|x| x.ok()).collect())
    .unwrap_or_default()
}

/// The operator's answer. Returns what to send the node, if anything.
///
/// `also` optionally turns on a standing auto-approval at the same time,
/// which is how "allow, and stop asking me about this agent" is one click
/// rather than two screens.
pub fn decide(
    st: &AppState,
    approval_id: &str,
    choice: &str,
    also: Option<&str>,
) -> Result<(Option<HubToNode>, String)> {
    let db = st.db.lock().unwrap();
    let row: Option<(String, String, String, String, String, Option<String>)> = db
        .query_row(
            "SELECT session_id, slot_id, COALESCE(agent_id, ''), options, status, agent_id
               FROM approvals WHERE id = ?",
            [approval_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((session_id, slot_id, _agent, opts_json, status, agent_id)) = row else {
        bail!("no approval '{approval_id}'");
    };
    if status != "pending" {
        bail!("that request was already {status}");
    }
    let options: Vec<PromptOption> = serde_json::from_str(&opts_json).unwrap_or_default();
    let name: String = db
        .query_row("SELECT COALESCE(display_name, id) FROM slots WHERE id = ?", [&slot_id],
                   |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| slot_id.clone());

    // A standing auto-approval is set BEFORE the answer goes out, so the
    // next prompt from the same seat is covered even if it arrives while
    // this one is still being typed.
    let mut extra = String::new();
    match also {
        Some("agent") => {
            db.execute("UPDATE slots SET auto_approve = 1 WHERE id = ?", [&slot_id])?;
            extra = format!(
                " {name} will answer its own prompts from now on; /autoapprove {name} off stops that."
            );
        }
        Some("session") => {
            db.execute("UPDATE sessions SET auto_approve = 1 WHERE id = ?", [&session_id])?;
            extra = " every seat in this team will answer its own prompts from now on.".into();
        }
        Some(other) => bail!("'{other}' is not something to remember; use 'agent' or 'session'"),
        None => {}
    }

    // "deny" is not a button the CLI offered: it is the operator's word for
    // whichever option declines. Resolved here rather than in the dashboard,
    // so the console and the XR view mean the same thing by it.
    let picked = if choice == "deny" {
        // `!affirmative` alone is not enough: an option can be a yes that
        // the node could not classify, and declining must never be the
        // click that grants something. Both flags have to be clear.
        options.iter().find(|o| !o.affirmative && !o.widens).cloned()
    } else if choice == "allow" {
        options.iter().filter(|o| o.affirmative && !o.widens)
               .min_by_key(|o| o.key.clone()).cloned()
    } else {
        options.iter().find(|o| o.key == choice).cloned()
    };
    let Some(opt) = picked else {
        bail!(
            "'{choice}' is not one of the choices that prompt offered ({})",
            options.iter().map(|o| o.key.as_str()).collect::<Vec<_>>().join(", ")
        );
    };

    let now = now_ms();
    db.execute(
        "UPDATE approvals SET status = 'decided', decision = ?, decided_by = 'operator',
                              decided_at_ms = ? WHERE id = ?",
        params![opt.key, now, approval_id],
    )?;
    let _ = chat(
        &db,
        &session_id,
        &slot_id,
        &format!("\u{1f511} you answered {name}: \"{}\".{extra}", opt.label),
    );
    tracing::info!(%approval_id, slot = %slot_id, choice = %opt.label, ?also,
                   "the operator answered a permission prompt");
    let said = format!("Answered \"{}\" for {name}.{extra}", opt.label);
    let Some(agent_id) = agent_id else { return Ok((None, said)) };
    Ok((
        Some(HubToNode::ApprovalDecided {
            approval_id: approval_id.to_string(),
            agent_id,
            keys: opt.keys,
            label: opt.label,
            by: "operator".into(),
        }),
        said,
    ))
}

/// Turn a standing auto-approval on or off, for one seat or a whole team.
pub fn set_auto(db: &Connection, what: &str, id: &str, on: bool) -> Result<()> {
    let table = match what {
        "agent" | "slot" => "slots",
        "session" | "team" => "sessions",
        other => bail!("'{other}' is not something to auto-approve"),
    };
    let n = db.execute(
        &format!("UPDATE {table} SET auto_approve = ? WHERE id = ?"),
        params![if on { 1 } else { 0 }, id],
    )?;
    if n == 0 {
        bail!("no {what} '{id}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    fn opts() -> Vec<PromptOption> {
        // Codex's real tool dialog: one narrow yes, two that widen, one no.
        vec![
            PromptOption { key: "1".into(), label: "Allow".into(), keys: "1".into(),
                           affirmative: true, widens: false },
            PromptOption { key: "2".into(), label: "Allow for this session".into(),
                           keys: "2".into(), affirmative: true, widens: true },
            PromptOption { key: "3".into(), label: "Always allow".into(), keys: "3".into(),
                           affirmative: true, widens: true },
            PromptOption { key: "4".into(), label: "Cancel".into(), keys: "4".into(),
                           affirmative: false, widens: false },
        ]
    }

    /// A different question: not the same dialog read through a screen that
    /// moved, but the next thing the agent stopped on.
    fn other_opts() -> Vec<PromptOption> {
        vec![
            PromptOption { key: "1".into(), label: "Yes, run it".into(), keys: "1".into(),
                           affirmative: true, widens: false },
            PromptOption { key: "2".into(), label: "No".into(), keys: "2".into(),
                           affirmative: false, widens: false },
        ]
    }

    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        let now = now_ms();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',?,?)",
            params![now, now],
        )
        .unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,
                                last_seen_ms,lease_expires_ms)
             VALUES ('nd1','machine-a','linux','x86_64','0.1.0','online',?,?,?)",
            params![now, now, now + 60000],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,created_at_ms)
             VALUES ('sl-max','s1','Max',?)",
            params![now],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,status,provenance,started_at_ms,last_seen_ms)
             VALUES ('ag-1','sl-max','nd1','running','managed',?,?)",
            params![now, now],
        )
        .unwrap();
        drop(db);
        st
    }

    fn ask(st: &AppState, fp: &str) -> Option<HubToNode> {
        asked(st, "ag-1", "pty_prompt", "Allow Codex to run this tool?", &opts(), fp)
    }

    fn ask_next(st: &AppState, fp: &str) -> Option<HubToNode> {
        asked(st, "ag-1", "pty_prompt", "Run `rm -rf build`?", &other_opts(), fp)
    }

    fn pend(st: &AppState) -> Vec<serde_json::Value> {
        pending(&st.db.lock().unwrap(), None)
    }

    #[test]
    fn with_nothing_pre_authorised_the_prompt_waits_for_a_person() {
        // Nothing times out into "allowed". A stopped agent is the honest
        // state: it is waiting for the operator.
        let st = fleet();
        assert!(ask(&st, "aa").is_none(), "nothing should be decided yet");
        let p = pend(&st);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["agent"], "Max");
        assert_eq!(p[0]["options"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn it_reaches_the_operators_chat_as_a_record_not_as_a_message() {
        // `record = 1`: a line for whoever reads the room, never something
        // an agent is handed and asked to answer.
        let st = fleet();
        ask(&st, "aa");
        let db = st.db.lock().unwrap();
        let (kind, record, claimed): (String, i64, i64) = db
            .query_row("SELECT kind, record, claimed_at_ms IS NOT NULL FROM messages", [],
                       |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(kind, "approval");
        assert_eq!(record, 1);
        assert_eq!(claimed, 1);
    }

    #[test]
    fn one_dialog_is_raised_once_however_often_it_is_reported() {
        // A node re-reports on reconnect and on every idle tick. The
        // operator must not come back to four copies of one question.
        let st = fleet();
        for _ in 0..4 {
            ask(&st, "same-fingerprint");
        }
        assert_eq!(pend(&st).len(), 1);
    }

    #[test]
    fn one_terminal_shows_one_question_so_a_new_one_retires_the_last() {
        // A seat asking something NEW has moved on from what it asked
        // before. The old card's buttons would type a keystroke at whatever
        // replaced that dialog, so it is retired rather than left live.
        let st = fleet();
        ask(&st, "first");
        ask_next(&st, "second");
        let p = pend(&st);
        assert_eq!(p.len(), 1, "{p:#?}");
        let db = st.db.lock().unwrap();
        let (status, by): (String, String) = db
            .query_row("SELECT status, decided_by FROM approvals WHERE fingerprint = 'first'",
                       [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(status, "withdrawn");
        assert_eq!(by, "superseded");
    }

    #[test]
    fn answering_at_the_cli_clears_the_card_even_when_the_seat_asks_again_at_once() {
        // The reported bug. She answers the prompt in the agent's own
        // terminal; the agent proceeds and asks the NEXT permission straight
        // away, so its screen never passes through a moment with no dialog on
        // it. Nothing withdrew the first request, and it sat in the dashboard
        // with live buttons until she answered it a second time there.
        let st = fleet();
        ask(&st, "the-one-she-answered");
        ask_next(&st, "the-one-it-asked-next");
        let p = pend(&st);
        assert_eq!(p.len(), 1, "only the question actually on screen: {p:#?}");
        // ...and she is told why the first card went, rather than watching it
        // vanish.
        let db = st.db.lock().unwrap();
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM messages WHERE body LIKE '%different question now%'",
                       [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn one_dialog_read_through_a_moving_screen_stays_one_card() {
        // The node fingerprints what it can see, and a seat under tmux has
        // tmux's CLOCK on its screen. That is how 77 pending cards for three
        // seats came about: one per minute, each a "new" question, and only
        // the newest one could ever be retired. The choices on offer settle
        // it -- same keys, same words, same keystrokes, same dialog -- so the
        // card is refreshed in place and keeps its id, which is what stops
        // the dashboard notifying her about it again every minute.
        let st = fleet();
        ask(&st, "20:50");
        let first = pend(&st)[0]["id"].as_str().unwrap().to_string();
        for minute in ["20:51", "20:52", "20:53"] {
            ask(&st, minute);
        }
        let p = pend(&st);
        assert_eq!(p.len(), 1, "{p:#?}");
        assert_eq!(p[0]["id"].as_str().unwrap(), first, "it is the same card, not a new one");
        let db = st.db.lock().unwrap();
        // ...and she is not told three times that it moved on, because it
        // did not: only the screen under it did.
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM messages WHERE body LIKE '%different question now%'",
                       [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // The row now carries the fingerprint last reported, so the node's
        // withdrawal of THAT one still matches.
        let fp: String = db
            .query_row("SELECT fingerprint FROM approvals WHERE id = ?", [&first], |r| r.get(0))
            .unwrap();
        assert_eq!(fp, "20:53");
    }

    #[test]
    fn a_request_is_retired_when_the_seat_has_been_through_a_new_process() {
        // Nothing a node says will ever mention a dead agent again, so this
        // is the only thing that can clear it: a card for a process that is
        // not there has buttons that can only fail.
        let st = fleet();
        ask(&st, "asked-by-the-old-process");
        assert_eq!(pend(&st).len(), 1);
        {
            let db = st.db.lock().unwrap();
            // The seat is respawned: same slot, a newer agent.
            db.execute(
                "INSERT INTO agents (id,slot_id,node_id,status,provenance,started_at_ms,last_seen_ms)
                 VALUES ('ag-2','sl-max','nd1','running','managed',?,?)",
                params![now_ms() + 1000, now_ms() + 1000],
            )
            .unwrap();
            assert_eq!(retire_gone(&db), 1);
        }
        assert!(pend(&st).is_empty());
    }

    #[test]
    fn a_request_survives_a_node_that_is_merely_disconnected() {
        // Its node may come back with the dialog still on the screen, and
        // nothing has been lost meanwhile. Retiring it here would throw away
        // a question the agent is genuinely still stopped on.
        let st = fleet();
        ask(&st, "aa");
        let db = st.db.lock().unwrap();
        db.execute("UPDATE agents SET status = 'disconnected' WHERE id = 'ag-1'", []).unwrap();
        assert_eq!(retire_gone(&db), 0);
    }

    #[test]
    fn a_node_that_forgot_what_it_raised_can_still_clear_the_card() {
        // A node restart loses its memory of what it reported while the hub's
        // row outlives the process. It reports the SEAT as having nothing up,
        // with no fingerprint, and that has to be enough -- otherwise a
        // prompt answered at the terminal during a node restart keeps its
        // buttons for ever.
        let st = fleet();
        ask(&st, "raised-before-the-restart");
        withdrawn(&st, "ag-1", "");
        assert!(pend(&st).is_empty());
    }

    #[test]
    fn a_seat_set_to_auto_approve_is_answered_at_once_with_the_narrowest_yes() {
        let st = fleet();
        st.db.lock().unwrap()
            .execute("UPDATE slots SET auto_approve = 1 WHERE id = 'sl-max'", [])
            .unwrap();
        let d = ask(&st, "aa").expect("it should be decided immediately");
        match d {
            HubToNode::ApprovalDecided { keys, label, by, .. } => {
                assert_eq!(keys, "1");
                assert_eq!(label, "Allow");
                assert_eq!(by, "auto:agent");
            }
            other => panic!("{other:?}"),
        }
        assert!(pend(&st).is_empty(), "nothing should still be waiting");
    }

    #[test]
    fn a_whole_team_can_be_set_to_auto_approve() {
        let st = fleet();
        st.db.lock().unwrap()
            .execute("UPDATE sessions SET auto_approve = 1 WHERE id = 's1'", [])
            .unwrap();
        match ask(&st, "aa").expect("decided") {
            HubToNode::ApprovalDecided { by, keys, .. } => {
                assert_eq!(by, "auto:session");
                assert_eq!(keys, "1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_seats_own_setting_is_the_one_reported() {
        // Both on: the chat line should say which switch to go and look at,
        // and the seat's is the more specific.
        let st = fleet();
        {
            let db = st.db.lock().unwrap();
            db.execute("UPDATE slots SET auto_approve = 1 WHERE id = 'sl-max'", []).unwrap();
            db.execute("UPDATE sessions SET auto_approve = 1 WHERE id = 's1'", []).unwrap();
        }
        match ask(&st, "aa").unwrap() {
            HubToNode::ApprovalDecided { by, .. } => assert_eq!(by, "auto:agent"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_auto_approval_never_picks_an_option_that_widens_permission() {
        // THE rule that makes auto-approve safe to leave on. "Answer this
        // prompt for me" is not "let the agent turn its own guardrails off",
        // so when every affirmative widens, it waits like anything else.
        let st = fleet();
        st.db.lock().unwrap()
            .execute("UPDATE slots SET auto_approve = 1 WHERE id = 'sl-max'", [])
            .unwrap();
        let wide = vec![
            PromptOption { key: "1".into(), label: "Always trust this machine".into(),
                           keys: "1".into(), affirmative: true, widens: true },
            PromptOption { key: "2".into(), label: "No".into(), keys: "2".into(),
                           affirmative: false, widens: false },
        ];
        let d = asked(&st, "ag-1", "pty_prompt", "Trust this folder?", &wide, "aa");
        assert!(d.is_none(), "it must not answer this itself: {d:?}");
        assert_eq!(pend(&st).len(), 1, "and it must be waiting for the operator");
    }

    #[test]
    fn an_unreadable_prompt_is_raised_with_no_buttons_rather_than_guessed_at() {
        let st = fleet();
        let d = asked(&st, "ag-1", "pty_prompt", "Continue?", &[], "aa");
        assert!(d.is_none());
        let p = pend(&st);
        assert_eq!(p.len(), 1);
        assert!(p[0]["options"].as_array().unwrap().is_empty());
        let db = st.db.lock().unwrap();
        let body: String = db.query_row("SELECT body FROM messages", [], |r| r.get(0)).unwrap();
        assert!(body.contains("cannot read the choices"), "{body}");
    }

    #[test]
    fn the_operator_can_choose_any_option_the_cli_offered() {
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        let (frame, said) = decide(&st, &id, "2", None).unwrap();
        assert!(said.contains("Allow for this session"), "{said}");
        match frame.unwrap() {
            HubToNode::ApprovalDecided { keys, by, .. } => {
                assert_eq!(keys, "2");
                assert_eq!(by, "operator");
            }
            other => panic!("{other:?}"),
        }
        assert!(pend(&st).is_empty());
    }

    #[test]
    fn allow_means_the_narrowest_yes_and_deny_never_grants_anything() {
        // `deny` resolving onto "Always allow" was a real bug: that option
        // reads as a negative to a naive classifier because it does not
        // START with an affirmative word.
        let st = fleet();
        ask(&st, "a");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        match decide(&st, &id, "allow", None).unwrap().0.unwrap() {
            HubToNode::ApprovalDecided { keys, .. } => assert_eq!(keys, "1"),
            other => panic!("{other:?}"),
        }
        ask(&st, "b");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        match decide(&st, &id, "deny", None).unwrap().0.unwrap() {
            HubToNode::ApprovalDecided { keys, label, .. } => {
                assert_eq!(keys, "4", "deny must be Cancel, never an allow-always");
                assert_eq!(label, "Cancel");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn answering_can_turn_on_a_standing_approval_in_the_same_click() {
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        let (_, said) = decide(&st, &id, "allow", Some("agent")).unwrap();
        assert!(said.contains("will answer its own prompts"), "{said}");
        let on: i64 = st.db.lock().unwrap()
            .query_row("SELECT auto_approve FROM slots WHERE id='sl-max'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
        // ...and the next prompt from that seat no longer waits.
        assert!(ask(&st, "next").is_some());
    }

    #[test]
    fn or_for_the_whole_team() {
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        decide(&st, &id, "allow", Some("session")).unwrap();
        let on: i64 = st.db.lock().unwrap()
            .query_row("SELECT auto_approve FROM sessions WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn a_choice_the_prompt_did_not_offer_is_refused() {
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        let e = decide(&st, &id, "9", None).unwrap_err().to_string();
        assert!(e.contains("not one of the choices"), "{e}");
        assert_eq!(pend(&st).len(), 1, "and it is still waiting");
    }

    #[test]
    fn answering_twice_is_refused_rather_than_typed_twice() {
        // The second keystroke would land on whatever the agent did next.
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        decide(&st, &id, "allow", None).unwrap();
        let e = decide(&st, &id, "allow", None).unwrap_err().to_string();
        assert!(e.contains("already"), "{e}");
    }

    #[test]
    fn a_prompt_that_goes_away_stops_showing_live_buttons() {
        // Somebody attached and answered it, or the agent gave up waiting.
        let st = fleet();
        ask(&st, "aa");
        withdrawn(&st, "ag-1", "aa");
        assert!(pend(&st).is_empty());
        let db = st.db.lock().unwrap();
        let (status, by): (String, String) = db
            .query_row("SELECT status, decided_by FROM approvals", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(status, "withdrawn");
        assert_eq!(by, "attached");
        // And it is said out loud: buttons vanishing silently reads as a bug.
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM messages WHERE body LIKE '%went away%'", [],
                       |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn withdrawing_something_already_answered_changes_nothing() {
        let st = fleet();
        ask(&st, "aa");
        let id = pend(&st)[0]["id"].as_str().unwrap().to_string();
        decide(&st, &id, "allow", None).unwrap();
        withdrawn(&st, "ag-1", "aa");
        let status: String = st.db.lock().unwrap()
            .query_row("SELECT status FROM approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "decided");
    }

    #[test]
    fn a_standing_approval_can_be_turned_off_again() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        set_auto(&db, "agent", "sl-max", true).unwrap();
        set_auto(&db, "agent", "sl-max", false).unwrap();
        let on: i64 = db
            .query_row("SELECT auto_approve FROM slots WHERE id='sl-max'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 0);
        assert!(set_auto(&db, "agent", "sl-nope", true).is_err());
    }
}
