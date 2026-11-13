//! Stopping a session that has spent its budget.
//!
//! Token counts were being collected and never used, which is the worst of
//! both: the cost is visible only after the fact. A session may now carry a
//! ceiling, and reaching it **pauses** the session.
//!
//! Pausing rather than killing, for three reasons. It is reversible, by an
//! operator who can look at what was bought and decide. Every agent already
//! understands it -- pause withholds work rather than yanking a process. And
//! the alternative, terminating agents mid-edit, destroys work to save money,
//! which is the wrong trade at almost any budget.
//!
//! The ceiling is per session, not per agent. With a mixed team the number
//! worth capping is what the task cost, not what one seat cost.

use crate::state::AppState;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// A session that has just gone over.
pub struct Hit {
    pub session_id: String,
    pub spent: i64,
    pub budget: i64,
}

/// Has the session this agent belongs to exceeded its budget?
///
/// Returns `Some` only on the FIRST crossing: `budget_hit_at_ms` is stamped
/// here, so re-pausing a session an operator has deliberately released does
/// not happen on the next token report.
pub fn exceeded(db: &Connection, agent_id: &str) -> Result<Option<Hit>> {
    let row: Option<(String, Option<i64>, Option<i64>)> = db
        .query_row(
            "SELECT se.id, se.token_budget, se.budget_hit_at_ms
               FROM agents a
               JOIN slots s   ON s.id = a.slot_id
               JOIN sessions se ON se.id = s.session_id
              WHERE a.id = ?",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((session_id, Some(budget), already)) = row else { return Ok(None) };
    if already.is_some() || budget <= 0 {
        return Ok(None);
    }
    // Cache reads are excluded: they are a discount, not a spend, and
    // counting them would pause a session for being efficient.
    let spent: i64 = db.query_row(
        "SELECT COALESCE(SUM(a.input_tokens + a.output_tokens), 0)
           FROM agents a JOIN slots s ON s.id = a.slot_id
          WHERE s.session_id = ?",
        [&session_id],
        |r| r.get(0),
    )?;
    if spent < budget {
        return Ok(None);
    }
    db.execute(
        "UPDATE sessions SET budget_hit_at_ms = ? WHERE id = ?",
        params![nexmesh_proto::now_ms(), session_id],
    )?;
    Ok(Some(Hit { session_id, spent, budget }))
}

/// Pause the session and say why, in the place an operator will look.
pub async fn enforce(st: &AppState, hit: Hit) {
    let reason = format!(
        "token budget reached: {} of {} spent. Raise or clear the budget, then release.",
        hit.spent, hit.budget
    );
    tracing::warn!(session = %hit.session_id, spent = hit.spent, budget = hit.budget,
        "token budget reached; pausing the session");
    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
            params![
                nexmesh_proto::now_ms(),
                "budget",
                "session_paused_on_budget",
                serde_json::json!({ "spent": hit.spent, "budget": hit.budget }).to_string()
            ],
        );
    }
    if let Err(e) = crate::control::set_session_paused(st, &hit.session_id, true, Some(&reason)).await
    {
        // Worth shouting about: the session is over budget and still running.
        tracing::error!(session = %hit.session_id, error = %e,
            "could not pause a session that reached its budget");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with(budget: Option<i64>, input: i64, output: i64, cache: i64) -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,token_budget,created_at_ms,last_active_ms)
             VALUES ('s1','T',?,1,1)",
            params![budget],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,created_at_ms) VALUES ('sl1','s1',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,provenance,status,input_tokens,output_tokens,
                                 cache_read_tokens,started_at_ms,last_seen_ms)
             VALUES ('ag1','sl1','managed','running',?,?,?,1,1)",
            params![input, output, cache],
        )
        .unwrap();
        db
    }

    #[test]
    fn no_budget_means_no_ceiling() {
        let db = db_with(None, 10_000_000, 10_000_000, 0);
        assert!(exceeded(&db, "ag1").unwrap().is_none());
    }

    #[test]
    fn under_budget_is_not_a_hit() {
        let db = db_with(Some(1000), 400, 300, 0);
        assert!(exceeded(&db, "ag1").unwrap().is_none());
    }

    #[test]
    fn reaching_the_budget_is_a_hit_once_and_once_only() {
        // Token reports arrive constantly; re-pausing a session the operator
        // has released would make `release` useless.
        let db = db_with(Some(1000), 700, 400, 0);
        let hit = exceeded(&db, "ag1").unwrap().expect("should be over");
        assert_eq!(hit.spent, 1100);
        assert_eq!(hit.budget, 1000);
        assert!(exceeded(&db, "ag1").unwrap().is_none(), "must not fire twice");
    }

    #[test]
    fn cache_reads_do_not_count_against_the_budget() {
        // They are a discount. Charging for them would pause a session for
        // being efficient.
        let db = db_with(Some(1000), 300, 200, 50_000);
        assert!(exceeded(&db, "ag1").unwrap().is_none());
    }

    #[test]
    fn spending_is_summed_across_the_whole_team() {
        let db = db_with(Some(1000), 600, 0, 0);
        db.execute(
            "INSERT INTO slots (id,session_id,created_at_ms) VALUES ('sl2','s1',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,provenance,status,input_tokens,output_tokens,
                                 cache_read_tokens,started_at_ms,last_seen_ms)
             VALUES ('ag2','sl2','managed','running',500,0,0,1,1)",
            [],
        )
        .unwrap();
        // 600 + 500 over a 1000 budget: neither agent alone is over.
        assert!(exceeded(&db, "ag1").unwrap().is_some());
    }

    #[test]
    fn an_unknown_agent_is_not_a_hit() {
        let db = db_with(Some(1), 5, 5, 0);
        assert!(exceeded(&db, "nope").unwrap().is_none());
    }

    #[test]
    fn a_nonsense_budget_is_ignored_rather_than_pausing_immediately() {
        let db = db_with(Some(0), 1, 1, 0);
        assert!(exceeded(&db, "ag1").unwrap().is_none());
        let db = db_with(Some(-5), 1, 1, 0);
        assert!(exceeded(&db, "ag1").unwrap().is_none());
    }
}
