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

/// The test that says a seat is between turns rather than empty, for a query
/// with the seat as `{s}` and its newest agent as `{a}`.
const WAITING_SQL: &str = "{a}.exited_at_ms IS NOT NULL
             AND {a}.provenance = 'attached'
             AND COALESCE({s}.cli_session,'') <> ''
             AND COALESCE({s}.task_state,'') <> 'released'
             AND EXISTS (SELECT 1 FROM agents p
                          WHERE p.slot_id = {s}.id AND p.started_at_ms < {a}.started_at_ms)
             AND (CAST(strftime('%s','now') AS INTEGER) * 1000) - {a}.exited_at_ms <
                 MAX(120000, MIN(3600000, 4 * COALESCE((
                     SELECT {a}.started_at_ms - p.started_at_ms FROM agents p
                      WHERE p.slot_id = {s}.id AND p.started_at_ms < {a}.started_at_ms
                      ORDER BY p.started_at_ms DESC LIMIT 1), 0)))";

/// Is this seat between turns? A condition, for a WHERE clause.
pub fn waiting(slots: &str, agents: &str) -> String {
    WAITING_SQL.replace("{s}", slots).replace("{a}", agents)
}

/// The seat's agent word, for a query with the seat as `slots` and its newest
/// agent as `agents`. Drops in where `COALESCE(a.status, 'disconnected')`
/// used to be.
pub fn agent_word(slots: &str, agents: &str) -> String {
    format!(
        "CASE WHEN {cond} THEN 'waiting' ELSE COALESCE({agents}.status, 'disconnected') END",
        cond = waiting(slots, agents)
    )
}

/// How many seats a team message is for: everyone in its session who could
/// still see it, bar whoever sent it.
///
/// This exists because "delivered" was a single flag on the message, set by
/// the FIRST ack -- and a capability bridge, which is handed team chat it
/// cannot answer, acks everything at once. So an operator's question to a
/// team of three read as delivered the moment the bridge took it, while the
/// two seats that could actually answer had never been handed it at all.
/// That is the worst kind of wrong: the fleet looked like it was ignoring
/// her, rather than like it had not heard.
///
/// A seat counts if something is there to read it: a live process, or a seat
/// between turns, which will read it on its next one. A seat with no process
/// and no rhythm cannot see anything, and counting it would mean a team
/// message was never delivered to anybody.
pub fn audience(m: &str) -> String {
    format!("(SELECT COUNT(*) {})", audience_seats(m, ""))
}

/// How many of those seats have actually seen it. Per seat, because one
/// teammate acking a broadcast must not count for the rest -- and counted
/// over the SAME seats as the audience, or a message read by seats that have
/// since been removed reads `4/3`, which is nobody's idea of an answer.
pub fn seen(m: &str) -> String {
    format!(
        "(SELECT COUNT(*) {})",
        audience_seats(
            m,
            &format!(
                "AND EXISTS (SELECT 1 FROM message_recipients r
                              WHERE r.message_id = {m}.id AND r.slot_id = s2.id
                                AND r.acked_at_ms IS NOT NULL)"
            )
        )
    )
}

/// The seats a team message is for: the FROM and WHERE of both counts above,
/// written once so they can never disagree.
fn audience_seats(m: &str, extra: &str) -> String {
    format!(
        "FROM slots s2
          LEFT JOIN agents a2 ON a2.id = (
              SELECT id FROM agents WHERE slot_id = s2.id
              ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
         WHERE s2.session_id = {m}.session_id
           AND ({m}.from_slot IS NULL OR s2.id <> {m}.from_slot)
           AND COALESCE(s2.task_state,'') <> 'released'
           -- An adopted terminal is a window, not a mind. It holds no model
           -- and can never ack, so counting it would mean every team message
           -- in a session with one stayed undelivered for ever. Whatever is
           -- SITTING in that terminal has a seat of its own, and that seat
           -- is counted.
           AND COALESCE(s2.cli_id,'') <> 'tmux'
           AND s2.tmux_target IS NULL
           AND a2.id IS NOT NULL
           AND (a2.exited_at_ms IS NULL OR ({cond}))
           {extra}",
        cond = waiting("s2", "a2")
    )
}

/// Whether everyone it was for has seen it. A message addressed to one seat
/// is its own business; a team message is not delivered until the team has
/// it, and a team nobody is in has not received anything.
pub fn delivered(m: &str) -> String {
    format!(
        "CASE WHEN {m}.to_slot IS NULL
              THEN ({a} > 0 AND {sn} >= {a})
              ELSE {m}.acked_at_ms IS NOT NULL END",
        a = audience(m),
        sn = seen(m)
    )
}

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
        let w = agent_word("s", "a");
        db.query_row(
            &format!(
                "SELECT {w} FROM slots s
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

    /// A session with a live CLI seat, a live external seat, a seat whose
    /// process is long gone, and one team message from the operator.
    fn room() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
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
        for (slot, name) in [("sl-cli", "Max"), ("sl-ext", "Bridge"), ("sl-dead", "Ghost")] {
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                 VALUES (?,'s1',?,'claude',1)",
                params![slot, name],
            )
            .unwrap();
        }
        let now = now_ms();
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-cli','sl-cli','nd-1','attached','running',?,?)",
            params![now, now],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-ext','sl-ext','external','running',?,?)",
            params![now, now],
        )
        .unwrap();
        // Written off, never pinned, so nothing says it is coming back.
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms,
                                 exited_at_ms)
             VALUES ('ag-dead','sl-dead','nd-1','attached','exited',?,?,?)",
            params![now - 9_000_000, now - 9_000_000, now - 8_000_000],
        )
        .unwrap();
        db.execute(
            "INSERT INTO messages (id,session_id,from_slot,to_slot,kind,body,sent_at_ms)
             VALUES (1,'s1',NULL,NULL,'system','are you connected?',?)",
            params![now],
        )
        .unwrap();
        db
    }

    fn tally(db: &Connection) -> (i64, i64, bool) {
        db.query_row(
            &format!(
                "SELECT {}, {}, {} FROM messages m WHERE m.id = 1",
                seen("m"),
                audience("m"),
                delivered("m")
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn a_team_message_is_for_the_seats_that_could_read_it() {
        let (seen_n, audience_n, delivered_f) = tally(&room());
        assert_eq!((seen_n, audience_n), (0, 2), "the dead seat is nobody's audience");
        assert!(!delivered_f);
    }

    #[test]
    fn one_seat_acking_is_not_the_team_hearing_you() {
        // The bridge takes everything, including team chat it cannot answer.
        let db = room();
        db.execute(
            "INSERT INTO message_recipients (message_id,slot_id,acked_at_ms) VALUES (1,'sl-ext',?)",
            params![now_ms()],
        )
        .unwrap();
        let (seen_n, audience_n, delivered_f) = tally(&db);
        assert_eq!((seen_n, audience_n), (1, 2));
        assert!(!delivered_f, "the seat that could answer has not seen it");

        db.execute(
            "INSERT INTO message_recipients (message_id,slot_id,acked_at_ms) VALUES (1,'sl-cli',?)",
            params![now_ms()],
        )
        .unwrap();
        let (seen_n, audience_n, delivered_f) = tally(&db);
        assert_eq!((seen_n, audience_n), (2, 2));
        assert!(delivered_f, "now the team really has it");
    }

    #[test]
    fn a_seat_that_has_since_gone_cannot_push_the_count_past_the_room() {
        // `(team 4/3)` on the live fleet: acks counted over all time, the
        // audience over seats that still exist.
        let db = room();
        for slot in ["sl-ext", "sl-cli", "sl-dead"] {
            db.execute(
                "INSERT INTO message_recipients (message_id,slot_id,acked_at_ms) VALUES (1,?,?)",
                params![slot, now_ms()],
            )
            .unwrap();
        }
        db.execute("DELETE FROM slots WHERE id = 'sl-dead'", []).unwrap();
        let (seen_n, audience_n, _) = tally(&db);
        assert_eq!((seen_n, audience_n), (2, 2));
    }

    #[test]
    fn an_adopted_terminal_is_not_part_of_the_audience() {
        // It is a window onto a pane. The CLI inside it has its own seat.
        let db = room();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,cli_id,tmux_target,created_at_ms)
             VALUES ('sl-term','s1','term-Max','tmux','nexmesh-max',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-term','sl-term','nd-1','managed','running',?,?)",
            params![now_ms(), now_ms()],
        )
        .unwrap();
        let (_, audience_n, _) = tally(&db);
        assert_eq!(audience_n, 2, "the terminal is not a teammate");
    }

    #[test]
    fn a_message_to_one_seat_is_its_own_business() {
        let db = room();
        db.execute("UPDATE messages SET to_slot = 'sl-cli' WHERE id = 1", []).unwrap();
        let (_, _, delivered_f) = tally(&db);
        assert!(!delivered_f, "not acked yet");
        db.execute("UPDATE messages SET acked_at_ms = ? WHERE id = 1", params![now_ms()])
            .unwrap();
        let (_, _, delivered_f) = tally(&db);
        assert!(delivered_f);
    }

    #[test]
    fn an_empty_room_has_not_received_anything() {
        let db = room();
        db.execute("UPDATE agents SET exited_at_ms = ?, status = 'exited'", params![now_ms()])
            .unwrap();
        let (_, audience_n, delivered_f) = tally(&db);
        assert_eq!(audience_n, 0);
        assert!(!delivered_f, "a team nobody is in has heard nothing");
    }

    #[test]
    fn the_patience_bounds_are_the_ones_the_sql_uses() {
        // The numbers are inlined in the SQL, where they can be read in
        // place; these constants are how everything else talks about them.
        let w = agent_word("s", "a");
        assert!(w.contains(&MIN_PATIENCE_MS.to_string()));
        assert!(w.contains(&MAX_PATIENCE_MS.to_string()));
    }
}
