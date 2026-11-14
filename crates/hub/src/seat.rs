//! What word a seat deserves when nothing is running in it.
//!
//! Most CLIs are one long-lived process: it registers, it sits there, and
//! when it goes the seat is empty until somebody starts another. For those,
//! `exited` says exactly what happened.
//!
//! Some are not. A CLI driven by a cron loop -- `hermes chat` is the one that
//! taught us this -- is a NEW process every turn: it registers, takes its
//! seat, says its piece and exits, and a minute later another one does the
//! same. The hub recorded all of that correctly and then reported the seat as
//! `exited` for the thirty seconds between turns, which reads as a dead
//! teammate on a fleet where that teammate is working perfectly well.
//!
//! So the word depends on whether the occupant has shown that it COMES BACK.
//! Recurrence is evidence, not a setting: a seat pinned to one conversation
//! (`cli_session`) that has already been taken more than once is a seat whose
//! occupant has a rhythm, and until that rhythm is broken the honest word is
//! `waiting` -- nobody is in it right now, and somebody will be.
//!
//! The patience is the occupant's own: four times the gap between its last
//! two turns, never less than two minutes, never more than an hour. A seat
//! taken once a minute stops waiting after four; one taken hourly is given
//! the hour. When the window passes, the seat goes back to saying `exited`,
//! because by then the rhythm really has stopped.
//!
//! Only the WORD changes. The agent row keeps saying `exited`, so delivery,
//! recovery and seat reclaim all behave exactly as they did -- the next turn
//! reclaims this seat rather than minting a twin beside it.

/// How long a seat waits at the very least, in ms.
pub const MIN_PATIENCE_MS: i64 = 120_000;
/// How long a seat waits at the very most, in ms.
pub const MAX_PATIENCE_MS: i64 = 3_600_000;

/// The seat's agent word, for a query that has `slots s` LEFT JOINed to the
/// seat's newest `agents a`. Drops in where `COALESCE(a.status,
/// 'disconnected')` used to be.
pub const AGENT_WORD_SQL: &str = "CASE WHEN a.exited_at_ms IS NOT NULL
             AND a.provenance = 'attached'
             AND COALESCE(s.cli_session,'') <> ''
             AND COALESCE(s.task_state,'') <> 'released'
             AND EXISTS (SELECT 1 FROM agents p
                          WHERE p.slot_id = s.id AND p.started_at_ms < a.started_at_ms)
             AND (CAST(strftime('%s','now') AS INTEGER) * 1000) - a.exited_at_ms <
                 MAX(120000, MIN(3600000, 4 * COALESCE((
                     SELECT a.started_at_ms - p.started_at_ms FROM agents p
                      WHERE p.slot_id = s.id AND p.started_at_ms < a.started_at_ms
                      ORDER BY p.started_at_ms DESC LIMIT 1), 0)))
        THEN 'waiting' ELSE COALESCE(a.status, 'disconnected') END";

#[cfg(test)]
mod tests {
    use super::*;
    use nexmesh_proto::now_ms;
    use rusqlite::{params, Connection};

    /// One seat, and as many turns in it as asked for. Each turn is a process
    /// that registered, ran and exited, `every` ms apart.
    fn seat(turns: usize, every: i64, ended_ago: i64, pinned: bool) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        // `cli_session` arrives by migration, and the word turns on it.
        crate::state::migrate(&db).unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,
                                lease_expires_ms)
             VALUES ('nd-1','machine-a','linux','x86_64','0.1.0','online',1,1,9999999999999)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,cli_id,cli_session,created_at_ms)
             VALUES ('sl-1','s1','Hermes','hermes',?,1)",
            params![if pinned { Some("hermes-lisa") } else { None }],
        )
        .unwrap();
        let last_exit = now_ms() - ended_ago;
        for i in 0..turns {
            // Turn 0 is the oldest; the newest ends `ended_ago` ago.
            let back = (turns - 1 - i) as i64 * every;
            db.execute(
                "INSERT INTO agents (id,slot_id,node_id,provenance,status,
                                     started_at_ms,last_seen_ms,exited_at_ms)
                 VALUES (?,'sl-1','nd-1','attached','exited',?,?,?)",
                params![
                    format!("ag-{i}"),
                    last_exit - back - every / 2,
                    last_exit - back,
                    last_exit - back
                ],
            )
            .unwrap();
        }
        db
    }

    fn word(db: &Connection) -> String {
        db.query_row(
            &format!(
                "SELECT {AGENT_WORD_SQL} FROM slots s
                 LEFT JOIN agents a ON a.id = (
                     SELECT id FROM agents WHERE slot_id = s.id
                     ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
                 WHERE s.id = 'sl-1'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_seat_between_turns_is_waiting_not_exited() {
        // A turn a minute, the last one half a minute ago: this is the live
        // hermes seat, and calling it `exited` is what sent us looking.
        let db = seat(6, 60_000, 30_000, true);
        assert_eq!(word(&db), "waiting");
    }

    #[test]
    fn a_rhythm_that_has_stopped_is_an_exit() {
        // Same seat, but nothing has come back for twenty minutes. Four
        // times a one-minute turn is the patience it earned; it is spent.
        let db = seat(6, 60_000, 20 * 60_000, true);
        assert_eq!(word(&db), "exited");
    }

    #[test]
    fn an_occupant_that_has_never_come_back_is_simply_gone() {
        // One turn and no second: a terminal somebody closed. Nothing here
        // says it recurs, so nothing may claim it will.
        let db = seat(1, 60_000, 5_000, true);
        assert_eq!(word(&db), "exited");
    }

    #[test]
    fn an_unpinned_seat_keeps_the_plain_word() {
        // Without a pinned conversation the next process is not provably the
        // same occupant, so the seat is not held open in words either.
        let db = seat(6, 60_000, 30_000, false);
        assert_eq!(word(&db), "exited");
    }

    #[test]
    fn a_released_seat_is_finished_with() {
        let db = seat(6, 60_000, 30_000, true);
        db.execute("UPDATE slots SET task_state = 'released' WHERE id = 'sl-1'", [])
            .unwrap();
        assert_eq!(word(&db), "exited");
    }

    #[test]
    fn a_slow_occupant_is_given_longer_but_not_for_ever() {
        // Twenty minutes between turns: eighty minutes of patience would be
        // more than the hour anyone wants to look at a waiting seat.
        let db = seat(4, 20 * 60_000, 50 * 60_000, true);
        assert_eq!(word(&db), "waiting");
        let db = seat(4, 20 * 60_000, 70 * 60_000, true);
        assert_eq!(word(&db), "exited", "an hour is the most a seat may wait");
    }

    #[test]
    fn a_live_process_is_still_running() {
        let db = seat(3, 60_000, 30_000, true);
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-live','sl-1','nd-1','attached','running',?,?)",
            params![now_ms(), now_ms()],
        )
        .unwrap();
        assert_eq!(word(&db), "running");
    }

    #[test]
    fn the_patience_bounds_are_the_ones_the_sql_uses() {
        // The numbers are inlined in the SQL, where they can be read in
        // place; these constants are how everything else talks about them.
        assert!(AGENT_WORD_SQL.contains(&MIN_PATIENCE_MS.to_string()));
        assert!(AGENT_WORD_SQL.contains(&MAX_PATIENCE_MS.to_string()));
    }
}
