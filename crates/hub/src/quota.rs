//! Seats that cannot reach their model.
//!
//! A CLI whose account has used up its allowance -- claude's session or weekly
//! limit, codex's usage limit, a provider's final 429 -- fails every turn it
//! is given until the allowance resets, and a seat that keeps being handed
//! work in that state loses each message to a turn that never ran. So when a
//! node reports it (`NodeToHub::UsageLimited`, read from what the CLI itself
//! said), the seat is HELD: paused, with the reason and the time it lifts,
//! and every other seat signing in as the same account is held with it,
//! because they will all hit the same wall one turn later.
//!
//! A quota hold lifts ITSELF: at the reset time the CLI gave, or on a backoff
//! when it gave none, or at once when any seat on the account is seen to get
//! a call through. A lost login or a billing stop does not lift itself --
//! those need a person, and the operator is told what to do. Either way a
//! seat whose `quota_auto_resume` is 0 stays paused for the operator, and a
//! seat a PERSON paused is never touched by any of this.

use crate::state::AppState;
use anyhow::Result;
use nexmesh_proto::{now_ms, UsageBlock};
use rusqlite::{params, Connection, OptionalExtension};

/// The first retry for a used-up allowance with no reset time, and the most
/// a retry ever waits. Doubled each time it is still used up.
const RETRY_FIRST_MS: i64 = 15 * 60_000;
const RETRY_MAX_MS: i64 = 4 * 3_600_000;

/// Resume a minute after the reset the provider gave, not on it: its clock
/// and ours disagree by seconds, and a resume a moment early is a failed turn
/// and another hold.
const AFTER_RESET_MS: i64 = 60_000;

/// One seat, as the hold needs it.
#[derive(Debug, Clone)]
pub struct Seat {
    pub slot_id: String,
    pub name: String,
    pub session_id: String,
}

/// What a hold did.
#[derive(Debug)]
pub struct Held {
    pub account: String,
    pub kind: String,
    pub until_ms: Option<i64>,
    pub text: String,
    /// Seats that were not held before this.
    pub newly: Vec<Seat>,
    /// Of those, the ones that will stay paused when it lifts.
    pub stay_paused: Vec<String>,
}

/// The account a CLI on a node signs in as: the one its descriptor names,
/// or, when it names none, that CLI on that machine -- one config directory
/// is one login.
pub fn account_of(db: &Connection, node_id: &str, cli_id: &str) -> String {
    db.query_row(
        "SELECT account FROM node_clis WHERE node_id = ? AND cli_id = ?
            AND account IS NOT NULL AND account <> ''",
        params![node_id, cli_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or_else(|| format!("{cli_id} on {node_id}"))
}

/// The seat an agent is in, and the account it signs in as.
fn seat_of_agent(db: &Connection, agent_id: &str) -> Option<(Seat, String)> {
    let (slot_id, name, session_id, node_id, cli_id): (String, String, String, String, String) = db
        .query_row(
            "SELECT a.slot_id, COALESCE(s.display_name, s.role, a.slot_id), s.session_id,
                    COALESCE(a.node_id, ''), COALESCE(s.cli_id, '')
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE a.id = ?",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let account = account_of(db, &node_id, &cli_id);
    Some((Seat { slot_id, name, session_id }, account))
}

/// Every seat signing in as `account`: those with a live agent on it, and
/// those already held for it whose agent has since gone.
fn seats_on(db: &Connection, account: &str) -> Vec<Seat> {
    let mut out: Vec<Seat> = Vec::new();
    let mut stmt = match db.prepare(
        "SELECT s.id, COALESCE(s.display_name, s.role, s.id), s.session_id,
                COALESCE(a.node_id, ''), COALESCE(s.cli_id, ''), s.hold_account
           FROM slots s
           LEFT JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL",
    ) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let rows: Vec<(String, String, String, String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .and_then(|m| m.collect())
        .unwrap_or_default();
    for (slot_id, name, session_id, node_id, cli_id, held_for) in rows {
        let on = held_for.as_deref() == Some(account)
            || (!node_id.is_empty() && account_of(db, &node_id, &cli_id) == account);
        if on && !out.iter().any(|s| s.slot_id == slot_id) {
            out.push(Seat { slot_id, name, session_id });
        }
    }
    out
}

/// Hold every seat on the account of the agent that hit the wall.
pub fn hold(db: &Connection, agent_id: &str, b: &UsageBlock) -> Result<Option<Held>> {
    let Some((me, account)) = seat_of_agent(db, agent_id) else { return Ok(None) };
    let now = now_ms();
    let tries: i64 = db
        .query_row("SELECT COALESCE(hold_tries, 0) FROM slots WHERE id = ?", [&me.slot_id], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let known_reset = b.reset_at_ms.filter(|at| *at > now);
    let (until, tries) = match b.kind.as_str() {
        "quota" => match known_reset {
            Some(at) => (Some(at + AFTER_RESET_MS), tries),
            None => {
                let wait = RETRY_FIRST_MS.saturating_mul(1i64 << tries.clamp(0, 10));
                (Some(now + wait.min(RETRY_MAX_MS)), tries + 1)
            }
        },
        // A login or a bill does not come back on a timer.
        _ => (None, tries),
    };

    let mut seats = seats_on(db, &account);
    if !seats.iter().any(|s| s.slot_id == me.slot_id) {
        seats.push(me.clone());
    }
    let mut newly = Vec::new();
    let mut stay_paused = Vec::new();
    for seat in seats {
        let (paused, reason, auto): (i64, Option<String>, Option<i64>) = db
            .query_row(
                "SELECT paused, hold_reason, quota_auto_resume FROM slots WHERE id = ?",
                [&seat.slot_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((0, None, None));
        // A PERSON paused it. Not ours to hold, and never ours to release.
        if paused == 1 && reason.is_none() {
            continue;
        }
        db.execute(
            "UPDATE slots SET paused = 1, paused_at = COALESCE(paused_at, ?1),
                    hold_reason = ?2, hold_until_ms = ?3, hold_text = ?4,
                    hold_since_ms = COALESCE(hold_since_ms, ?1), hold_account = ?5,
                    hold_tries = ?6
              WHERE id = ?7",
            params![now, b.kind, until, b.text, account, tries, seat.slot_id],
        )?;
        if reason.is_none() {
            if auto == Some(0) {
                stay_paused.push(seat.name.clone());
            }
            newly.push(seat);
        }
    }
    if !newly.is_empty() {
        db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?, 'nexmesh', NULL,
                    'seats_held', ?)",
            params![
                now,
                serde_json::json!({
                    "account": account, "kind": b.kind, "until_ms": until, "text": b.text,
                    "window": b.window,
                    "seats": newly.iter().map(|s| &s.slot_id).collect::<Vec<_>>(),
                })
                .to_string()
            ],
        )?;
    }
    Ok(Some(Held { account, kind: b.kind.clone(), until_ms: until, text: b.text.clone(), newly, stay_paused }))
}

/// Lift the hold on every seat held for `account`. Seats set to stay paused
/// keep their pause and lose the hold, which leaves them exactly as if a
/// person had paused them. Returns (resumed, kept paused).
pub fn release(db: &Connection, account: &str, got_through: bool) -> Result<(Vec<Seat>, Vec<Seat>)> {
    let mut stmt = db.prepare(
        "SELECT id, COALESCE(display_name, role, id), session_id, COALESCE(quota_auto_resume, 1)
           FROM slots WHERE hold_account = ? AND hold_reason IS NOT NULL",
    )?;
    let held: Vec<(Seat, i64)> = stmt
        .query_map([account], |r| {
            Ok((Seat { slot_id: r.get(0)?, name: r.get(1)?, session_id: r.get(2)? }, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let (mut resumed, mut kept) = (Vec::new(), Vec::new());
    for (seat, auto) in held {
        let keep = auto == 0;
        db.execute(
            "UPDATE slots SET paused = ?1, paused_at = CASE WHEN ?1 = 1 THEN paused_at END,
                    hold_reason = NULL, hold_until_ms = NULL, hold_text = NULL,
                    hold_since_ms = NULL, hold_account = NULL,
                    hold_tries = CASE WHEN ?2 THEN 0 ELSE hold_tries END
              WHERE id = ?3",
            params![keep as i64, got_through, seat.slot_id],
        )?;
        if keep { kept.push(seat) } else { resumed.push(seat) }
    }
    Ok((resumed, kept))
}

/// `2026-11-25 22:00 UTC`, for a person.
pub fn when(ms: i64) -> String {
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(ms / 1000) else {
        return format!("{ms} ms");
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute()
    )
}

fn names(seats: &[Seat]) -> String {
    seats.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Tell the operator: in the fleet inbox an orchestrator reads, and as a
/// RECORD in the chat of every team the seats are in -- which is what the
/// dashboard shows, and what it raises a notification for. A record is never
/// delivered to an agent, and it is pre-claimed, so it is not one more
/// "unclaimed" for somebody to clear.
fn tell_operator(db: &Connection, seats: &[Seat], body: &str) {
    let now = now_ms();
    let first = seats.first().map(|s| s.session_id.as_str()).unwrap_or("");
    let _ = db.execute(
        "INSERT INTO operator_messages
            (from_label, from_kind, from_session, to_session, body, sent_at_ms)
         VALUES ('nexmesh', 'operator', ?, NULL, ?, ?)",
        params![first, body, now],
    );
    let mut sessions: Vec<&str> = seats.iter().map(|s| s.session_id.as_str()).collect();
    sessions.sort_unstable();
    sessions.dedup();
    for session in sessions {
        let _ = db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                   sent_at_ms, claimed_at_ms, claimed_by, record)
             VALUES (?1, NULL, NULL, 'system', ?2, ?3, ?3, 'hub', 1)",
            params![session, body, now],
        );
    }
}

fn tell_seat(db: &Connection, seat: &Seat, body: &str) {
    let _ = db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, ?, 'nudge', ?, ?)",
        params![seat.session_id, seat.slot_id, body, now_ms()],
    );
}

/// The words for a hold, for the operator.
pub fn hold_notice(h: &Held) -> String {
    let who = names(&h.newly);
    let what = match h.kind.as_str() {
        "quota" => "has used up its model allowance",
        "auth" => "is no longer signed in to its model",
        "billing" => "cannot use its subscription",
        _ => "cannot reach its model",
    };
    let next = match (h.kind.as_str(), h.until_ms) {
        ("quota", Some(at)) => format!(
            "They are held, and resume on their own at {}.", when(at)
        ),
        _ => "They are held until somebody fixes it -- sign the CLI in again, or settle \
              the subscription -- and then resume them from the dashboard or with `nexmesh \
              resume <team> <seat>`."
            .to_string(),
    };
    let kept = if h.stay_paused.is_empty() {
        String::new()
    } else {
        format!(" {} will stay paused when it lifts, as set for them.", h.stay_paused.join(", "))
    };
    format!(
        "[MODEL ACCESS] The account {} {what}: \"{}\". Held: {who}. {next}{kept}",
        h.account, h.text
    )
}

/// A node reported that an agent cannot reach its model, or can again.
pub fn report(st: &AppState, agent_id: &str, block: Option<UsageBlock>) {
    let db = st.db.lock().unwrap();
    match block {
        Some(b) => match hold(&db, agent_id, &b) {
            Ok(Some(h)) if !h.newly.is_empty() => {
                tracing::warn!(account = %h.account, kind = %h.kind, seats = h.newly.len(),
                    until = ?h.until_ms, "holding seats that cannot reach their model");
                tell_operator(&db, &h.newly, &hold_notice(&h));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%agent_id, error = %e, "could not hold a seat"),
        },
        // A call went through: whatever held this account is over.
        None => {
            let Some((_, account)) = seat_of_agent(&db, agent_id) else { return };
            lift(&db, &account, true, "a call on it went through again");
        }
    }
}

/// Lift one account's hold, and say so to the seats and the operator.
fn lift(db: &Connection, account: &str, got_through: bool, why: &str) {
    match release(db, account, got_through) {
        Ok((resumed, kept)) if !resumed.is_empty() || !kept.is_empty() => {
            for seat in &resumed {
                tell_seat(
                    db,
                    seat,
                    "[MODEL ACCESS RESTORED] Your account's allowance is back. The request you \
                     were working on when it ran out got no answer -- it is above you in your \
                     conversation: pick up where you left off.",
                );
            }
            let mut body = format!("[MODEL ACCESS] The account {account} is back ({why}).");
            if !resumed.is_empty() {
                body.push_str(&format!(" Resumed: {}.", names(&resumed)));
            }
            if !kept.is_empty() {
                body.push_str(&format!(" Still paused, as set for them: {}.", names(&kept)));
            }
            let all: Vec<Seat> = resumed.iter().chain(kept.iter()).cloned().collect();
            tell_operator(db, &all, &body);
            tracing::info!(%account, resumed = resumed.len(), kept = kept.len(), %why,
                "lifted a model-access hold");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%account, error = %e, "could not lift a hold"),
    }
}

/// Lift every quota hold whose time has come. On the housekeeping tick.
pub fn tick(db: &Connection) {
    let due: Vec<String> = db
        .prepare(
            "SELECT DISTINCT hold_account FROM slots
              WHERE hold_reason = 'quota' AND hold_until_ms IS NOT NULL
                AND hold_until_ms <= ? AND hold_account IS NOT NULL",
        )
        .and_then(|mut q| q.query_map([now_ms()], |r| r.get(0)).and_then(|m| m.collect()))
        .unwrap_or_default();
    for account in due {
        lift(db, &account, false, "its reset time has come");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two claude seats and a codex seat on one machine, and a claude seat on
    /// another: one config directory per CLI per machine is one login.
    fn fleet() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        crate::state::migrate(&db).unwrap();
        db.execute_batch(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);
             INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,lease_expires_ms)
               VALUES ('n-a','a','linux','x86_64','1','online',1,1,1),
                      ('n-b','b','linux','x86_64','1','online',1,1,1);
             INSERT INTO node_clis (node_id,cli_id) VALUES ('n-a','claude'),('n-a','codex'),('n-b','claude');
             INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms) VALUES
               ('sl-1','s1','Ada','claude',1),('sl-2','s1','Bo','claude',1),
               ('sl-3','s1','Cy','codex',1),('sl-4','s1','Di','claude',1);
             INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms) VALUES
               ('a-1','sl-1','n-a','managed','running',1,1),('a-2','sl-2','n-a','managed','running',1,1),
               ('a-3','sl-3','n-a','managed','running',1,1),('a-4','sl-4','n-b','managed','running',1,1);",
        )
        .unwrap();
        db
    }

    fn quota(reset: Option<i64>) -> UsageBlock {
        UsageBlock { kind: "quota".into(), reset_at_ms: reset, window: Some("five_hour".into()),
                     text: "You've hit your session limit".into() }
    }

    fn held(db: &Connection, slot: &str) -> (i64, Option<String>, Option<i64>) {
        db.query_row("SELECT paused, hold_reason, hold_until_ms FROM slots WHERE id = ?", [slot],
                     |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
    }

    #[test]
    fn every_seat_on_the_same_login_is_held_and_no_other() {
        let db = fleet();
        let reset = now_ms() + 3_600_000;
        let h = hold(&db, "a-1", &quota(Some(reset))).unwrap().unwrap();
        assert_eq!(h.account, "claude on n-a");
        let mut names: Vec<_> = h.newly.iter().map(|s| s.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["Ada", "Bo"], "the other claude seat on that machine too");
        assert_eq!(held(&db, "sl-2"), (1, Some("quota".into()), Some(reset + AFTER_RESET_MS)));
        assert_eq!(held(&db, "sl-3").0, 0, "codex is another login");
        assert_eq!(held(&db, "sl-4").0, 0, "claude on another machine is another login");
    }

    #[test]
    fn an_account_named_in_the_descriptor_spans_machines() {
        let db = fleet();
        db.execute("UPDATE node_clis SET account = 'anthropic:work' WHERE cli_id = 'claude'", [])
            .unwrap();
        let h = hold(&db, "a-1", &quota(Some(now_ms() + 60_000))).unwrap().unwrap();
        assert_eq!(h.newly.len(), 3, "Ada, Bo and Di share one account: {:?}", h.newly);
    }

    #[test]
    fn a_seat_a_person_paused_is_never_held_or_released() {
        let db = fleet();
        db.execute("UPDATE slots SET paused = 1 WHERE id = 'sl-2'", []).unwrap();
        let h = hold(&db, "a-1", &quota(Some(now_ms() + 60_000))).unwrap().unwrap();
        assert!(h.newly.iter().all(|s| s.slot_id != "sl-2"));
        release(&db, &h.account, true).unwrap();
        assert_eq!(held(&db, "sl-2"), (1, None, None), "still the person's pause");
        assert_eq!(held(&db, "sl-1").0, 0, "the held one is resumed");
    }

    #[test]
    fn a_quota_hold_lifts_itself_when_its_reset_has_come() {
        let db = fleet();
        hold(&db, "a-1", &quota(Some(now_ms() + 60_000))).unwrap();
        db.execute("UPDATE slots SET hold_until_ms = 1 WHERE hold_reason = 'quota'", []).unwrap();
        tick(&db);
        assert_eq!(held(&db, "sl-1"), (0, None, None));
        assert_eq!(held(&db, "sl-2"), (0, None, None));
        let told: i64 = db.query_row(
            "SELECT COUNT(*) FROM messages WHERE body LIKE '[MODEL ACCESS RESTORED]%'", [],
            |r| r.get(0)).unwrap();
        assert_eq!(told, 2, "each resumed seat is told to pick up where it left off");
        let (record, claimed): (i64, i64) = db.query_row(
            "SELECT record, claimed_at_ms IS NOT NULL FROM messages
              WHERE body LIKE '[MODEL ACCESS] %' LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((record, claimed), (1, 1),
                   "the operator's line is a record: shown, never delivered, nothing to clear");
    }

    #[test]
    fn with_no_reset_time_it_retries_on_a_growing_backoff() {
        let db = fleet();
        let t0 = now_ms();
        let first = hold(&db, "a-1", &quota(None)).unwrap().unwrap().until_ms.unwrap();
        assert!((first - t0 - RETRY_FIRST_MS).abs() < 5_000);
        release(&db, "claude on n-a", false).unwrap();
        let second = hold(&db, "a-1", &quota(None)).unwrap().unwrap().until_ms.unwrap();
        assert!(second - now_ms() > RETRY_FIRST_MS + 60_000, "it waits longer the second time");
        // And a call that gets through resets the backoff.
        release(&db, "claude on n-a", true).unwrap();
        let tries: i64 = db.query_row("SELECT hold_tries FROM slots WHERE id='sl-1'", [],
                                      |r| r.get(0)).unwrap();
        assert_eq!(tries, 0);
    }

    #[test]
    fn a_lost_login_is_held_until_a_person_fixes_it() {
        let db = fleet();
        let b = UsageBlock { kind: "auth".into(), reset_at_ms: None, window: None,
                             text: "Login expired".into() };
        let h = hold(&db, "a-1", &b).unwrap().unwrap();
        assert_eq!(h.until_ms, None);
        assert!(hold_notice(&h).contains("until somebody fixes it"), "{}", hold_notice(&h));
        // Not a quota hold: no time to lift at, so the tick leaves it.
        tick(&db);
        assert_eq!(held(&db, "sl-1").1.as_deref(), Some("auth"));
    }

    #[test]
    fn a_seat_set_to_stay_paused_keeps_its_pause_when_the_hold_lifts() {
        let db = fleet();
        db.execute("UPDATE slots SET quota_auto_resume = 0 WHERE id = 'sl-2'", []).unwrap();
        let h = hold(&db, "a-1", &quota(Some(now_ms() + 60_000))).unwrap().unwrap();
        assert_eq!(h.stay_paused, ["Bo"]);
        let (resumed, kept) = release(&db, &h.account, false).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(kept[0].name, "Bo");
        assert_eq!(held(&db, "sl-2"), (1, None, None), "paused, now as if by a person");
    }
}
