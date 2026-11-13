//! Asking a teammate for a capability, and waiting for the answer.
//!
//! Messages are the right shape for "here is something you should know" and
//! the wrong shape for "OCR this page and give me the text": the asker cannot
//! continue until it has the result. So a capability ask is a row with a reply
//! slot -- the asker waits on it, the holder fills it in.
//!
//! A row rather than an in-memory channel, because a holder restarted
//! mid-flight leaves the ask visibly pending instead of leaving the asker
//! waiting on something that no longer exists.
//!
//! This is deliberately generic. It knows nothing about any particular
//! service: whoever advertises a capability label can serve it, which is what
//! lets a bridge to an external service be an ordinary participant holding its
//! own credentials rather than something the hub has to know about.

use anyhow::{bail, Result};
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

/// How long an asker waits before giving up. Long enough for a model to run
/// on a GPU, short enough that a wedged holder does not hang a turn for ever.
pub const DEFAULT_TIMEOUT_S: i64 = 180;
pub const MAX_TIMEOUT_S: i64 = 900;
/// Requests up to this size are kept after answering, for readability; bigger
/// ones are replaced by a note of their size.
const KEEP_REQUEST_BYTES: i64 = 64 * 1024;

/// Who in this session can serve a capability, nearest to "idle" first.
pub fn holders(db: &Connection, session_id: &str, capability: &str, exclude: &str)
    -> Result<Vec<(String, String)>>
{
    let want = capability.trim().to_lowercase();
    let mut stmt = db.prepare(
        "SELECT s.id, COALESCE(s.display_name, s.role, s.id), s.capabilities
           FROM slots s
           JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
          WHERE s.session_id = ? AND s.id != ? AND s.paused = 0
          ORDER BY CASE s.task_state WHEN 'idle' THEN 0 ELSE 1 END, s.id",
    )?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![session_id, exclude], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, _, caps)| {
            serde_json::from_str::<Vec<String>>(caps)
                .unwrap_or_default()
                .iter()
                .any(|c| c == &want)
        })
        .map(|(id, label, _)| (id, label))
        .collect())
}

/// Record an ask and say who it went to.
pub fn create(
    db: &Connection,
    session_id: &str,
    from_slot: &str,
    capability: &str,
    request: &str,
    model: Option<&str>,
) -> Result<(String, String, String)> {
    let candidates = holders(db, session_id, capability, from_slot)?;
    let Some((to_slot, label)) = candidates.into_iter().next() else {
        bail!(
            "no teammate here offers '{}'. Use list_peers with a capability filter to see what \
             is available.",
            capability.trim().to_lowercase()
        );
    };
    let id = format!("ask-{}", &uuid::Uuid::new_v4().to_string()[..12]);
    db.execute(
        "INSERT INTO capability_asks
            (id, session_id, capability, from_slot, to_slot, request, model, asked_at_ms)
         VALUES (?,?,?,?,?,?,?,?)",
        params![
            id, session_id, capability.trim().to_lowercase(), from_slot, to_slot, request,
            model, now_ms()
        ],
    )?;
    Ok((id, to_slot, label))
}

/// Has it been answered?
pub fn poll(db: &Connection, id: &str) -> Result<Option<(String, Option<String>)>> {
    Ok(db
        .query_row(
            "SELECT status, result FROM capability_asks WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .filter(|(status, _): &(String, Option<String>)| status != "pending"))
}

/// What is waiting for one holder, oldest first.
pub fn pending_for(db: &Connection, slot_id: &str) -> Result<Vec<(String, String, String, Option<String>)>> {
    let mut stmt = db.prepare(
        "SELECT id, capability, request, model FROM capability_asks
          WHERE to_slot = ? AND status = 'pending' ORDER BY asked_at_ms LIMIT 20",
    )?;
    let rows = stmt
        .query_map([slot_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Fill in an answer. Only the holder it was given to may.
pub fn answer(db: &Connection, slot_id: &str, id: &str, result: &str, ok: bool) -> Result<String> {
    let to: Option<(String, String)> = db
        .query_row(
            "SELECT to_slot, status FROM capability_asks WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((to_slot, status)) = to else { bail!("no ask '{id}'") };
    if to_slot != slot_id {
        // Otherwise any teammate could answer for another, and the asker
        // would have no way to know it had been handed a stranger's work.
        bail!("ask '{id}' was not given to you");
    }
    if status != "pending" {
        bail!("ask '{id}' has already been answered");
    }
    db.execute(
        "UPDATE capability_asks SET status = ?, result = ?, answered_at_ms = ? WHERE id = ?",
        params![if ok { "done" } else { "failed" }, result, now_ms(), id],
    )?;
    // A served request can be a megabyte of base64. Keeping it for ever would
    // grow the database for no benefit, so a large one is replaced by a note
    // of what it was once it has been answered; a small one is kept, because
    // it is genuinely useful when reading back what a teammate was asked.
    db.execute(
        "UPDATE capability_asks
            SET request = '{\"note\":\"request body of ' || LENGTH(request) ||
                          ' bytes discarded after answering\"}'
          WHERE id = ? AND LENGTH(request) > ?",
        params![id, KEEP_REQUEST_BYTES],
    )?;
    Ok(format!("Answered {id}."))
}

/// Release anything a departing holder never answered, so an asker is told
/// rather than left waiting on a slot that has gone.
pub fn fail_outstanding(db: &Connection, slot_id: &str) -> Result<usize> {
    Ok(db.execute(
        "UPDATE capability_asks
            SET status = 'failed', answered_at_ms = ?,
                result = 'the teammate holding this request stopped before answering'
          WHERE to_slot = ? AND status = 'pending'",
        params![now_ms(), slot_id],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        for (id, name, caps, state) in [
            ("sl-ask", "Asker", "[]", "working"),
            ("sl-ocr", "Bridge", r#"["ocr","transcribe"]"#, "idle"),
            ("sl-none", "Plain", "[]", "idle"),
        ] {
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,capabilities,task_state,
                                    created_at_ms)
                 VALUES (?,'s1',?,?,?,1)",
                params![id, name, caps, state],
            )
            .unwrap();
            db.execute(
                "INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
                 VALUES (?,?,'external','running',1,1)",
                params![format!("ag-{id}"), id],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn an_ask_goes_to_a_teammate_that_offers_the_capability() {
        let db = db();
        let (_, to, label) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        assert_eq!(to, "sl-ocr");
        assert_eq!(label, "Bridge");
    }

    #[test]
    fn a_capability_nobody_offers_is_refused_with_advice() {
        let db = db();
        let err = create(&db, "s1", "sl-ask", "telepathy", "{}", None).unwrap_err().to_string();
        assert!(err.contains("no teammate here offers 'telepathy'"), "{err}");
        assert!(err.contains("list_peers"), "it must say how to find out: {err}");
    }

    #[test]
    fn the_capability_name_is_matched_case_insensitively() {
        let db = db();
        assert!(create(&db, "s1", "sl-ask", "OCR", "{}", None).is_ok());
    }

    #[test]
    fn you_are_never_given_your_own_ask() {
        let db = db();
        db.execute(
            "UPDATE slots SET capabilities = '[\"ocr\"]' WHERE id = 'sl-ask'",
            [],
        )
        .unwrap();
        let (_, to, _) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        assert_eq!(to, "sl-ocr", "not itself");
    }

    #[test]
    fn an_ask_is_pending_until_answered_then_carries_the_result() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{\"image\":\"x\"}", None).unwrap();
        assert!(poll(&db, &id).unwrap().is_none(), "pending is not an answer");
        let waiting = pending_for(&db, "sl-ocr").unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].1, "ocr");
        assert_eq!(waiting[0].2, "{\"image\":\"x\"}");
        answer(&db, "sl-ocr", &id, "the text", true).unwrap();
        let (status, result) = poll(&db, &id).unwrap().unwrap();
        assert_eq!(status, "done");
        assert_eq!(result.as_deref(), Some("the text"));
    }

    #[test]
    fn a_large_request_is_not_kept_for_ever_but_is_accounted_for() {
        let db = db();
        let big = format!("{{\"image\":\"{}\"}}", "A".repeat(100_000));
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", &big, None).unwrap();
        answer(&db, "sl-ocr", &id, "done", true).unwrap();
        let kept: String = db
            .query_row("SELECT request FROM capability_asks WHERE id = ?", [&id], |r| r.get(0))
            .unwrap();
        assert!(kept.contains("discarded after answering"), "{kept}");
        assert!(kept.contains("100"), "it must say how big it was: {kept}");
        // The answer itself is untouched.
        assert_eq!(poll(&db, &id).unwrap().unwrap().1.as_deref(), Some("done"));
    }

    #[test]
    fn a_small_request_is_kept_so_it_can_be_read_back() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{\"image\":\"tiny\"}", None).unwrap();
        answer(&db, "sl-ocr", &id, "done", true).unwrap();
        let kept: String = db
            .query_row("SELECT request FROM capability_asks WHERE id = ?", [&id], |r| r.get(0))
            .unwrap();
        assert!(kept.contains("tiny"), "{kept}");
    }

    #[test]
    fn a_failure_is_an_answer_too() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        answer(&db, "sl-ocr", &id, "the engine is not installed", false).unwrap();
        let (status, result) = poll(&db, &id).unwrap().unwrap();
        assert_eq!(status, "failed");
        assert!(result.unwrap().contains("not installed"));
    }

    #[test]
    fn only_the_holder_may_answer() {
        // Otherwise any teammate could answer for another and the asker would
        // have no way to know whose work it got.
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        let err = answer(&db, "sl-none", &id, "mine", true).unwrap_err().to_string();
        assert!(err.contains("not given to you"), "{err}");
        assert!(poll(&db, &id).unwrap().is_none(), "and it stays pending");
    }

    #[test]
    fn an_ask_cannot_be_answered_twice() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        answer(&db, "sl-ocr", &id, "first", true).unwrap();
        assert!(answer(&db, "sl-ocr", &id, "second", true).is_err());
        assert_eq!(poll(&db, &id).unwrap().unwrap().1.as_deref(), Some("first"));
    }

    #[test]
    fn a_model_choice_travels_with_the_ask() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{}", Some("surya")).unwrap();
        let waiting = pending_for(&db, "sl-ocr").unwrap();
        assert_eq!(waiting[0].3.as_deref(), Some("surya"));
        let _ = id;
    }

    #[test]
    fn a_holder_that_stops_does_not_leave_an_asker_waiting_for_ever() {
        let db = db();
        let (id, _, _) = create(&db, "s1", "sl-ask", "ocr", "{}", None).unwrap();
        assert_eq!(fail_outstanding(&db, "sl-ocr").unwrap(), 1);
        let (status, result) = poll(&db, &id).unwrap().unwrap();
        assert_eq!(status, "failed");
        assert!(result.unwrap().contains("stopped before answering"), "it must say why");
    }

    #[test]
    fn a_paused_teammate_is_not_given_work() {
        let db = db();
        db.execute("UPDATE slots SET paused = 1 WHERE id = 'sl-ocr'", []).unwrap();
        assert!(create(&db, "s1", "sl-ask", "ocr", "{}", None).is_err());
    }

    #[test]
    fn a_teammate_with_no_live_agent_is_not_given_work() {
        let db = db();
        db.execute("UPDATE agents SET exited_at_ms = 2 WHERE slot_id = 'sl-ocr'", [])
            .unwrap();
        assert!(create(&db, "s1", "sl-ask", "ocr", "{}", None).is_err());
    }

    #[test]
    fn an_idle_holder_is_preferred_over_a_busy_one() {
        let db = db();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,capabilities,task_state,created_at_ms)
             VALUES ('sl-busy','s1','Busy','[\"ocr\"]','working',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-busy','sl-busy','external','running',1,1)",
            [],
        )
        .unwrap();
        let found = holders(&db, "s1", "ocr", "sl-ask").unwrap();
        assert_eq!(found[0].0, "sl-ocr", "the idle one first: {found:?}");
    }
}
