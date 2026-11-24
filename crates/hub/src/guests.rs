//! Seats that serve more than one team.
//!
//! A session is the isolation boundary and almost nothing should cross it.
//! But some seats are SERVICES rather than teammates: a capability bridge
//! offering OCR and transcription, a reviewer every team sends work to. One
//! copy per team means N processes, N models and N sets of warm context for
//! one job, and the operator maintaining N of them.
//!
//! So a seat can be a guest in other teams. Its HOME session is unchanged --
//! that is where it lives, whose workspace it holds, who owns it -- and each
//! guest membership is one more room it answers in.
//!
//! This is a hole in the isolation boundary, so it is explicit, recorded, and
//! takes reach over both teams: the operator, or a superorchestrator. An
//! orchestrator cannot, because it reaches one session and this needs two.

use anyhow::{bail, Result};
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

/// SQL fragment: is slot `s` in the session bound to `?N`?
///
/// Every query that used to say `s.session_id = ?` and means "serves this
/// team" has to say this instead, or a guest is invisible in the room it was
/// invited into -- which is the whole feature.
pub fn in_session(slot_col: &str, param: &str) -> String {
    format!(
        "({slot_col}.session_id = {param} OR EXISTS (
            SELECT 1 FROM slot_sessions g
             WHERE g.slot_id = {slot_col}.id AND g.session_id = {param}))"
    )
}

/// SQL fragment: does slot `other_col` share ANY room with the slot bound to
/// `me`?
///
/// The symmetric form of `in_session`, and both directions are needed. A room
/// has to see its guests, or an invited service is invisible in the room it
/// was invited into. A guest has to see its rooms, or it can be ASKED for
/// something and have no way to answer -- which is how the first version of
/// this went out: delivery into the guest worked and its reply came back "no
/// teammate matching".
///
/// It does mean a guest sees the union of every roster it answers in. That is
/// inherent in one seat serving several teams -- it is one conversation, so
/// what it is told in one room it knows in the others -- and is why opening
/// the hole takes reach over both teams.
pub fn shares_room(other_col: &str, me: &str) -> String {
    format!(
        "EXISTS (
            SELECT 1 FROM (
                SELECT session_id FROM slots WHERE id = {me}
                UNION SELECT session_id FROM slot_sessions WHERE slot_id = {me}
            ) mine
            WHERE {other_col}.session_id = mine.session_id
               OR EXISTS (SELECT 1 FROM slot_sessions g
                           WHERE g.slot_id = {other_col}.id
                             AND g.session_id = mine.session_id))"
    )
}

/// Every team this seat answers in: its home first, then its guest rooms.
pub fn sessions_of(db: &Connection, slot_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(Some(home)) = db
        .query_row("SELECT session_id FROM slots WHERE id = ?", [slot_id], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    {
        out.push(home);
    }
    if let Ok(mut stmt) =
        db.prepare("SELECT session_id FROM slot_sessions WHERE slot_id = ? ORDER BY added_at_ms")
    {
        if let Ok(rows) = stmt.query_map([slot_id], |r| r.get::<_, String>(0)) {
            for s in rows.flatten() {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Is this seat answering in this team at all? Home or guest.
pub fn serves(db: &Connection, slot_id: &str, session_id: &str) -> bool {
    sessions_of(db, slot_id).iter().any(|s| s == session_id)
}

/// The guests visiting one team, for the roster.
pub fn guests_of(db: &Connection, session_id: &str) -> Vec<serde_json::Value> {
    let sql = "SELECT s.id, COALESCE(s.display_name, s.id), COALESCE(s.role, ''),
                      s.session_id, COALESCE(hs.name, s.session_id), g.added_by, g.added_at_ms
                 FROM slot_sessions g
                 JOIN slots s ON s.id = g.slot_id
                 LEFT JOIN sessions hs ON hs.id = s.session_id
                WHERE g.session_id = ? ORDER BY g.added_at_ms";
    let Ok(mut stmt) = db.prepare(sql) else { return vec![] };
    stmt.query_map([session_id], |r| {
        Ok(json!({
            "slot_id": r.get::<_, String>(0)?,
            "name": r.get::<_, String>(1)?,
            "role": r.get::<_, String>(2)?,
            "home_session_id": r.get::<_, String>(3)?,
            "home": r.get::<_, String>(4)?,
            "added_by": r.get::<_, String>(5)?,
            "added_at_ms": r.get::<_, i64>(6)?,
        }))
    })
    .map(|m| m.filter_map(|x| x.ok()).collect())
    .unwrap_or_default()
}

/// Put a seat in another team.
pub fn join(db: &Connection, slot_id: &str, session_id: &str, by: &str) -> Result<String> {
    let name: String = db
        .query_row("SELECT COALESCE(display_name, id) FROM slots WHERE id = ?", [slot_id],
                   |r| r.get(0))
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no seat '{slot_id}'"))?;
    let (home, team): (String, String) = db
        .query_row(
            "SELECT s.session_id, COALESCE(se.name, ?2) FROM slots s
               LEFT JOIN sessions se ON se.id = ?2 WHERE s.id = ?1",
            params![slot_id, session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("no seat '{slot_id}'"))?;
    // Its own team is not a guest membership, and saying so beats recording
    // a row that means nothing.
    if home == session_id {
        bail!("{name} already lives in {team}; that is its own team");
    }
    let exists: bool = db
        .query_row("SELECT 1 FROM sessions WHERE id = ?", [session_id], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if !exists {
        bail!("no session '{session_id}'");
    }
    // A name is how everything addresses a seat, and it is only unique
    // within a team. A guest arriving under a name that room already uses
    // would make `send_message` ambiguous for everyone in it.
    let clash: bool = db
        .query_row(
            "SELECT 1 FROM slots WHERE session_id = ?1 AND display_name = ?2
              UNION SELECT 1 FROM slot_sessions g JOIN slots s ON s.id = g.slot_id
              WHERE g.session_id = ?1 AND s.display_name = ?2 AND s.id <> ?3",
            params![session_id, name, slot_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if clash {
        bail!("{team} already has a seat called '{name}'; rename one of them first");
    }
    let n = db.execute(
        "INSERT OR IGNORE INTO slot_sessions (slot_id, session_id, added_at_ms, added_by)
         VALUES (?,?,?,?)",
        params![slot_id, session_id, now_ms(), by],
    )?;
    if n == 0 {
        return Ok(format!("{name} already answers in {team}."));
    }
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,?,NULL,'slot_joined_session',?)",
        params![now_ms(), by,
                json!({ "slot": slot_id, "session": session_id, "home": home }).to_string()],
    )
    .ok();
    tracing::info!(slot = %slot_id, session = %session_id, %by, "a seat joined another team");
    Ok(format!(
        "{name} now answers in {team} as well as its own team. It is addressable there by \
         name, hears that team's messages, and keeps one conversation across all of them \
         \u{2014} which is the point, and also means anything it is told in one room it \
         knows in the others."
    ))
}

/// Take it out again. Its home team is not something it can leave.
pub fn leave(db: &Connection, slot_id: &str, session_id: &str, by: &str) -> Result<String> {
    let name: String = db
        .query_row("SELECT COALESCE(display_name, id) FROM slots WHERE id = ?", [slot_id],
                   |r| r.get(0))
        .optional()?
        .unwrap_or_else(|| slot_id.to_string());
    let home: Option<String> = db
        .query_row("SELECT session_id FROM slots WHERE id = ?", [slot_id], |r| r.get(0))
        .optional()?;
    if home.as_deref() == Some(session_id) {
        bail!(
            "{name} lives in that team \u{2014} it is not a guest there. Move it with \
             move_agent, or remove the seat."
        );
    }
    let n = db.execute(
        "DELETE FROM slot_sessions WHERE slot_id = ? AND session_id = ?",
        params![slot_id, session_id],
    )?;
    if n == 0 {
        bail!("{name} does not answer in that team");
    }
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,?,NULL,'slot_left_session',?)",
        params![now_ms(), by, json!({ "slot": slot_id, "session": session_id }).to_string()],
    )
    .ok();
    // Anything still queued for it in that room would never be delivered
    // now, and a message that can never arrive should not read as pending.
    let orphaned = db.execute(
        "UPDATE messages SET record = 1
          WHERE session_id = ?1 AND to_slot = ?2 AND acked_at_ms IS NULL",
        params![session_id, slot_id],
    )?;
    tracing::info!(slot = %slot_id, session = %session_id, %by, orphaned,
                   "a seat left a team it was visiting");
    Ok(if orphaned > 0 {
        format!("{name} no longer answers there. {orphaned} message(s) it had not read \
                 stayed behind as a record.")
    } else {
        format!("{name} no longer answers there.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// Two teams. `Cora` is a service seat living in `tools`; `Max` is an
    /// ordinary member of `app`.
    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        let now = now_ms();
        let db = st.db.lock().unwrap();
        for (id, name) in [("s-app", "app"), ("s-tools", "tools"), ("s-other", "other")] {
            db.execute(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES (?,?,?,?)",
                params![id, name, now, now],
            )
            .unwrap();
        }
        for (id, sess, name) in [
            ("sl-cora", "s-tools", "Cora"),
            ("sl-max", "s-app", "Max"),
            ("sl-dup", "s-app", "Cora2"),
        ] {
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,created_at_ms)
                 VALUES (?,?,?,?)",
                params![id, sess, name, now],
            )
            .unwrap();
        }
        drop(db);
        st
    }

    #[test]
    fn a_seat_answers_only_in_its_own_team_to_start_with() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        assert_eq!(sessions_of(&db, "sl-cora"), vec!["s-tools"]);
        assert!(serves(&db, "sl-cora", "s-tools"));
        assert!(!serves(&db, "sl-cora", "s-app"));
    }

    #[test]
    fn a_service_seat_can_answer_in_another_team_without_moving() {
        // The point: one process, one model, one set of warm context,
        // serving several teams instead of a private copy each.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let said = join(&db, "sl-cora", "s-app", "operator").unwrap();
        assert!(said.contains("answers in app as well as its own team"), "{said}");
        assert!(serves(&db, "sl-cora", "s-app"));
        // Its home is untouched: it did not move.
        let home: String = db
            .query_row("SELECT session_id FROM slots WHERE id='sl-cora'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(home, "s-tools");
        assert_eq!(sessions_of(&db, "sl-cora"), vec!["s-tools", "s-app"]);
    }

    #[test]
    fn it_shows_up_on_the_roster_of_the_team_it_visits() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        let g = guests_of(&db, "s-app");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0]["name"], "Cora");
        // Where it actually lives, so the room can tell a guest from a member.
        assert_eq!(g[0]["home"], "tools");
        assert!(guests_of(&db, "s-tools").is_empty(), "its home is not a visit");
    }

    #[test]
    fn inviting_it_twice_is_not_an_error_and_not_two_rows() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        let said = join(&db, "sl-cora", "s-app", "operator").unwrap();
        assert!(said.contains("already answers"), "{said}");
        assert_eq!(guests_of(&db, "s-app").len(), 1);
    }

    #[test]
    fn its_own_team_is_not_something_to_be_invited_into() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = join(&db, "sl-cora", "s-tools", "operator").unwrap_err().to_string();
        assert!(e.contains("that is its own team"), "{e}");
    }

    #[test]
    fn a_name_the_room_already_uses_is_refused() {
        // A name is only unique within a team, and it is how everything
        // addresses a seat. A guest arriving under a name that room already
        // uses would make `send_message` ambiguous for everyone in it.
        let st = fleet();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,created_at_ms)
             VALUES ('sl-clash','s-app','Cora',1)",
            [],
        )
        .unwrap();
        let e = join(&db, "sl-cora", "s-app", "operator").unwrap_err().to_string();
        assert!(e.contains("already has a seat called 'Cora'"), "{e}");
    }

    #[test]
    fn and_so_is_a_name_another_guest_of_that_room_already_uses() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,created_at_ms)
             VALUES ('sl-two','s-other','Cora',1)",
            [],
        )
        .unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        let e = join(&db, "sl-two", "s-app", "operator").unwrap_err().to_string();
        assert!(e.contains("already has a seat called"), "{e}");
    }

    #[test]
    fn a_team_that_does_not_exist_is_refused() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        assert!(join(&db, "sl-cora", "s-nope", "operator").is_err());
        assert!(join(&db, "sl-nope", "s-app", "operator").is_err());
    }

    #[test]
    fn it_can_be_taken_out_again() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        let said = leave(&db, "sl-cora", "s-app", "operator").unwrap();
        assert!(said.contains("no longer answers there"), "{said}");
        assert!(!serves(&db, "sl-cora", "s-app"));
    }

    #[test]
    fn but_never_out_of_the_team_it_lives_in() {
        // That is a move, or a removal, and both are different decisions.
        let st = fleet();
        let db = st.db.lock().unwrap();
        let e = leave(&db, "sl-cora", "s-tools", "operator").unwrap_err().to_string();
        assert!(e.contains("lives in that team"), "{e}");
    }

    #[test]
    fn leaving_retires_what_it_can_no_longer_be_given() {
        // A message queued for a seat in a room it no longer answers in can
        // never be delivered, and must not read as pending for ever.
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES ('s-app','sl-max','sl-cora','text','ocr this',1)",
            [],
        )
        .unwrap();
        let said = leave(&db, "sl-cora", "s-app", "operator").unwrap();
        assert!(said.contains("1 message(s) it had not read"), "{said}");
        let record: i64 = db
            .query_row("SELECT record FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(record, 1);
    }

    #[test]
    fn leaving_a_room_it_was_never_in_says_so() {
        let st = fleet();
        let db = st.db.lock().unwrap();
        assert!(leave(&db, "sl-cora", "s-app", "operator").is_err());
    }

    #[test]
    fn who_opened_the_hole_is_recorded() {
        // A seat reaching into another team is exactly the thing somebody
        // will need to account for afterwards.
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "seat Boss (sl-boss)").unwrap();
        let (actor, detail): (String, String) = db
            .query_row(
                "SELECT actor, detail FROM audit WHERE action = 'slot_joined_session'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(actor, "seat Boss (sl-boss)");
        assert!(detail.contains("s-app"), "{detail}");
        assert_eq!(guests_of(&db, "s-app")[0]["added_by"], "seat Boss (sl-boss)");
    }

    #[test]
    fn the_sql_fragment_matches_home_and_guest_alike() {
        // Everything that used to say `session_id = ?` and means "serves
        // this team" goes through this, so it is worth asserting directly.
        let st = fleet();
        let db = st.db.lock().unwrap();
        join(&db, "sl-cora", "s-app", "operator").unwrap();
        let sql = format!(
            "SELECT s.id FROM slots s WHERE {} ORDER BY s.id",
            in_session("s", "?1")
        );
        let mut stmt = db.prepare(&sql).unwrap();
        let ids: Vec<String> = stmt
            .query_map(["s-app"], |r| r.get(0))
            .unwrap()
            .filter_map(|x| x.ok())
            .collect();
        assert_eq!(ids, vec!["sl-cora", "sl-dup", "sl-max"]);
    }
}
