//! Context that outlives the window it was learned in.
//!
//! A long-lived agent spends its context twice: once working, and then again
//! carrying what it did. The second cost is the one that ends the session. A
//! CLI's own compaction answers it by summarising everything into its own
//! context -- where the next compaction summarises the summary, and the
//! fourth generation of that is a paragraph that remembers the shape of the
//! work and none of its facts. Nothing of it survives a restart on a CLI that
//! cannot resume, and nothing of it is readable by a teammate.
//!
//! A **fold** is the alternative. The agent writes down what a stretch of its
//! work established -- the decision and why, the paths with their line
//! numbers, the exact error -- and nexmesh keeps it: the summary, which is
//! short enough to carry for ever, and the detail, which is kept out of
//! context until something asks for it back. Folds fold into folds, so a
//! month of work is a handful of summaries over a tree of detail, and the
//! tree is read downwards only where a question needs it.
//!
//! The design is billion-context's, reimplemented here rather than imported:
//! an LSM tree over prose, the model doing the summarising, the kernel owning
//! the layout. Its own form is a proxy in front of the model API, which
//! rewrites each request's history. nexmesh is not in that position and does
//! not want to be -- it drives CLIs that hold their own conversations, and
//! the seats that matter most here sign in with a subscription rather than an
//! API key, so there is no request to rewrite. What a proxy does in three
//! parts, this does in three places:
//!
//!   * **injecting** -- the tools below, their doctrine, and the nudge, all
//!     of which nexmesh already has a channel for: it is the MCP server
//!     inside every seat and it can type at them.
//!   * **retrieving** -- `search_context` and `unfold_context`, over this
//!     store. billion-context's own controlled pilot found this half carried
//!     the measured win: an arm that executed no compressions at all
//!     reproduced the drop in re-fetching.
//!   * **evicting** -- the CLI's own compaction, which the watchdog already
//!     asks for by the CLI's own command. The ordering is what matters: the
//!     fold is written BEFORE the compaction, so what the agent chose to keep
//!     survives in full while the CLI throws away its context.
//!
//! Scopes are the memory store's: `agent` for a seat's own working context,
//! `session` for what the team should know, `global` for what outlives every
//! team. A fold is read nearest-first like any other memory.

use anyhow::{bail, Result};
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

use crate::memory::Scope;

/// What a fold records.
#[derive(Debug, Clone)]
pub struct Fold {
    pub id: String,
    pub scope: String,
    pub scope_ref: String,
    pub tier: i64,
    pub topic: String,
    pub summary: String,
    pub detail: String,
    /// The folds this one folded, if any. A tier-2 fold covers tier-1 folds.
    pub covers: Vec<String>,
    pub tokens_before: i64,
    pub tokens_after: i64,
    pub created_at_ms: i64,
}

/// Rough size of a piece of text, when nobody has counted it properly.
///
/// Four characters to a token is the usual English approximation and is only
/// ever used for proportions here -- what was folded against what it folded
/// into. A real count comes from the CLI when the CLI reports one, which is
/// what `agents.context_used_tokens` holds.
pub fn estimate_tokens(text: &str) -> i64 {
    (text.chars().count() as i64 + 3) / 4
}

/// The doctrine, in one paragraph, for a tool description and a nudge.
///
/// It is one rule and two lists. The rule decides WHEN, and it is the only
/// question the agent has to answer about a stretch of its context: is this
/// still needed by the step I am on? The lists decide WHAT, and exist because
/// a summary written without them keeps the narrative and loses the facts --
/// which is the failure mode of every CLI's own compaction.
pub const DOCTRINE: &str = "Fold a stretch of context when, and only when, the step you are \
on no longer needs it. KEEP, verbatim, in the summary: paths with their line numbers, \
signatures and the lines that carry the logic, exact error text, decisions together with \
the reason they were taken, exact values and names, and how the goal has changed. DROP: \
the output of a command once you have taken the result out of it, a file you read twice, \
the route you took to a conclusion, and a dead end -- but keep what the dead end taught. \
Under pressure the order that survives is: the goal, then decisions and their reasons, \
then what was produced, then conclusions, then lessons.";

/// The most detail one fold may put away.
///
/// A limit because this is a tool a model drives: the honest failure is an
/// agent handing over its whole transcript as one fold, which is not folding
/// but copying, and it would be the hub's database that paid for it. Two
/// megabytes is several times the largest sensible stretch -- billion-context
/// measured a median source of 2.6K tokens and a 90th percentile of 40K,
/// which is about 160KB of text.
pub const MAX_DETAIL_BYTES: usize = 2 * 1024 * 1024;

/// And the most summary. A summary this long is not one, and it would be
/// carried in context for ever.
pub const MAX_SUMMARY_BYTES: usize = 16 * 1024;

/// Where a fold belongs, as a row pair.
fn scope_cols(scope: &Scope) -> (&'static str, String) {
    (scope.name(), scope.reference().to_string())
}

/// Write a fold.
///
/// `covers` names folds this one replaces, which is how a tier is built: the
/// covered folds are retired and this one takes their place in the map, with
/// their detail still on disk underneath it. A fold that covers nothing is
/// tier 1 and sits directly over the agent's own work.
pub fn fold(
    db: &Connection,
    scope: &Scope,
    author_slot: &str,
    agent_id: Option<&str>,
    topic: &str,
    summary: &str,
    detail: &str,
    covers: &[String],
    tokens_before: Option<i64>,
) -> Result<Fold> {
    if topic.trim().is_empty() {
        bail!("a fold needs a topic: one line that says what this stretch of work was");
    }
    if summary.trim().is_empty() {
        bail!("a fold needs a summary -- that is the part that survives");
    }
    if summary.len() > MAX_SUMMARY_BYTES {
        bail!(
            "that summary is {}KB. The summary is what you carry from now on, so it has to \
             be short: put the long version in `detail`, where it costs you nothing until \
             you ask for it back.",
            summary.len() / 1024
        );
    }
    if detail.len() > MAX_DETAIL_BYTES {
        bail!(
            "that is {}MB of detail in one fold, over the {}MB limit. Fold a stretch of the \
             work rather than all of it: several folds, each with its own summary, is also \
             what makes them findable afterwards.",
            detail.len() / (1024 * 1024),
            MAX_DETAIL_BYTES / (1024 * 1024)
        );
    }
    let (scope_name, scope_ref) = scope_cols(scope);

    // Everything covered has to exist, be in the same scope, and still be
    // active. Folding a fold that something else already folded would hide a
    // stretch of work under two parents, and the map would double-count it.
    let mut tier = 1;
    for id in covers {
        let row: Option<(String, String, i64, i64)> = db
            .query_row(
                "SELECT scope, scope_ref, tier, active FROM folds WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((s, sref, t, active)) = row else {
            bail!("there is no fold '{id}' to fold in");
        };
        if active == 0 {
            bail!("'{id}' has already been folded into something else");
        }
        if s != scope_name || sref != scope_ref {
            bail!(
                "'{id}' is in {s} memory and this fold is in {scope_name}: \
                 a fold can only cover folds of its own scope"
            );
        }
        tier = tier.max(t + 1);
    }

    let before = tokens_before.unwrap_or_else(|| {
        if covers.is_empty() {
            estimate_tokens(detail)
        } else {
            0
        }
    });
    // A tier above 1 folded summaries, so what it replaced is what those
    // summaries cost plus everything they stood for.
    let before = if covers.is_empty() {
        before.max(estimate_tokens(detail))
    } else {
        let mut sum = before;
        for id in covers {
            let (b, a): (i64, i64) = db.query_row(
                "SELECT tokens_before, tokens_after FROM folds WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            sum += b.max(a);
        }
        sum
    };

    let id = format!("fd-{}", &uuid::Uuid::new_v4().to_string()[..12]);
    let now = now_ms();
    let covers_json = serde_json::to_string(covers).unwrap_or_else(|_| "[]".into());
    db.execute(
        "INSERT INTO folds (id, scope, scope_ref, tier, topic, summary, detail, covers,
                            author_slot, agent_id, tokens_before, tokens_after,
                            active, created_at_ms)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,1,?)",
        params![id, scope_name, scope_ref, tier, topic.trim(), summary.trim(), detail,
                covers_json, author_slot, agent_id, before,
                estimate_tokens(summary), now],
    )?;
    for covered in covers {
        db.execute(
            "UPDATE folds SET active = 0, retired_at_ms = ?, retired_why = ?
              WHERE id = ?",
            params![now, format!("folded into {id}"), covered],
        )?;
    }
    Ok(Fold {
        id,
        scope: scope_name.to_string(),
        scope_ref,
        tier,
        topic: topic.trim().to_string(),
        summary: summary.trim().to_string(),
        detail: detail.to_string(),
        covers: covers.to_vec(),
        tokens_before: before,
        tokens_after: estimate_tokens(summary),
        created_at_ms: now,
    })
}

fn row_to_fold(r: &rusqlite::Row) -> rusqlite::Result<Fold> {
    let covers: String = r.get(7)?;
    Ok(Fold {
        id: r.get(0)?,
        scope: r.get(1)?,
        scope_ref: r.get(2)?,
        tier: r.get(3)?,
        topic: r.get(4)?,
        summary: r.get(5)?,
        detail: r.get(6)?,
        covers: serde_json::from_str(&covers).unwrap_or_default(),
        tokens_before: r.get(8)?,
        tokens_after: r.get(9)?,
        created_at_ms: r.get(10)?,
    })
}

const COLS: &str = "id, scope, scope_ref, tier, topic, summary, detail, covers,
                    tokens_before, tokens_after, created_at_ms";

/// One fold by id, whether or not it is still active.
pub fn get(db: &Connection, id: &str) -> Result<Option<Fold>> {
    Ok(db
        .query_row(&format!("SELECT {COLS} FROM folds WHERE id = ?"), [id], row_to_fold)
        .optional()?)
}

/// Every fold a reader may see, nearest scope first, newest first.
///
/// The same nearest-first rule the memory store reads by: a seat's own folds,
/// then its groups', then the team's, then global. `active` only -- a fold
/// that has been folded into a higher tier is reachable through its parent
/// and must not be in the map twice.
pub fn visible(db: &Connection, scopes: &[Scope], limit: i64) -> Result<Vec<Fold>> {
    let mut out = Vec::new();
    for scope in scopes {
        let (name, sref) = scope_cols(scope);
        let mut stmt = db.prepare(&format!(
            "SELECT {COLS} FROM folds
              WHERE scope = ? AND scope_ref = ? AND active = 1
              ORDER BY tier DESC, created_at_ms DESC LIMIT ?"
        ))?;
        let rows = stmt.query_map(params![name, sref, limit], row_to_fold)?;
        for r in rows {
            out.push(r?);
        }
        if out.len() as i64 >= limit {
            break;
        }
    }
    Ok(out)
}

/// Search folds by words, nearest scope first.
///
/// Over the topic, the summary AND the detail: the point of a fold is that
/// the detail is out of context but not out of reach, and a search that could
/// only see the summary would make everything the agent chose not to carry
/// unfindable -- which is the failure it exists to prevent.
pub fn search(db: &Connection, scopes: &[Scope], query: &str, limit: i64) -> Result<Vec<Fold>> {
    let pattern = format!("%{}%", query.trim().to_lowercase());
    let mut out = Vec::new();
    for scope in scopes {
        let (name, sref) = scope_cols(scope);
        let mut stmt = db.prepare(&format!(
            "SELECT {COLS} FROM folds
              WHERE scope = ? AND scope_ref = ? AND active = 1
                AND (LOWER(topic) LIKE ?3 OR LOWER(summary) LIKE ?3 OR LOWER(detail) LIKE ?3)
              ORDER BY created_at_ms DESC LIMIT ?4"
        ))?;
        let rows = stmt.query_map(params![name, sref, pattern, limit], row_to_fold)?;
        for r in rows {
            out.push(r?);
        }
        if out.len() as i64 >= limit {
            break;
        }
    }
    Ok(out)
}

/// What is underneath a fold: the folds it folded, in order.
pub fn children(db: &Connection, id: &str) -> Result<Vec<Fold>> {
    let Some(parent) = get(db, id)? else { return Ok(vec![]) };
    let mut out = Vec::new();
    for child in &parent.covers {
        if let Some(f) = get(db, child)? {
            out.push(f);
        }
    }
    Ok(out)
}

/// Record that a fold was read back, so the map can say which detail is being
/// reached for and the nudge can stop suggesting a seat fold more of it.
pub fn note_unfolded(db: &Connection, id: &str) -> Result<()> {
    db.execute(
        "UPDATE folds SET unfolded_count = COALESCE(unfolded_count, 0) + 1,
                          unfolded_at_ms = ? WHERE id = ?",
        params![now_ms(), id],
    )?;
    Ok(())
}

/// What one scope's folds add up to, per tier: how many, what they cost to
/// carry, and what they stand for.
#[derive(Debug, Clone, Default)]
pub struct Shape {
    /// `(tier, folds, tokens carried, tokens folded)`, lowest tier first.
    pub tiers: Vec<(i64, i64, i64, i64)>,
}

impl Shape {
    pub fn carried(&self) -> i64 {
        self.tiers.iter().map(|t| t.2).sum()
    }
    pub fn folded(&self) -> i64 {
        self.tiers.iter().map(|t| t.3).sum()
    }
    pub fn count(&self) -> i64 {
        self.tiers.iter().map(|t| t.1).sum()
    }
}

pub fn shape(db: &Connection, scope: &Scope) -> Result<Shape> {
    let (name, sref) = scope_cols(scope);
    let mut stmt = db.prepare(
        "SELECT tier, COUNT(*), SUM(tokens_after), SUM(tokens_before)
           FROM folds WHERE scope = ? AND scope_ref = ? AND active = 1
          GROUP BY tier ORDER BY tier",
    )?;
    let rows = stmt.query_map(params![name, sref], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        ))
    })?;
    let mut tiers = Vec::new();
    for r in rows {
        tiers.push(r?);
    }
    Ok(Shape { tiers })
}

/// Has this seat folded anything since a given moment?
///
/// What the nudge reads: a seat that has just folded is not asked again, and
/// a seat that has been asked and did nothing is the one the compaction has
/// to go ahead without.
pub fn folded_since(db: &Connection, slot_id: &str, since_ms: i64) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM folds WHERE author_slot = ? AND created_at_ms >= ?",
        params![slot_id, since_ms],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    fn db() -> AppState {
        AppState::in_memory().unwrap()
    }

    fn agent_scope() -> Scope {
        Scope::Agent("sl-1".into())
    }

    fn write(st: &AppState, topic: &str, detail: &str) -> Fold {
        let db = st.db.lock().unwrap();
        fold(&db, &agent_scope(), "sl-1", Some("ag-1"), topic,
             "the short version", detail, &[], None)
            .unwrap()
    }

    #[test]
    fn a_fold_keeps_the_detail_out_of_context_but_not_out_of_reach() {
        // The whole point: what the agent stops carrying is still there.
        let st = db();
        let f = write(&st, "the migration", "a very long transcript of a migration");
        assert_eq!(f.tier, 1);
        let db = st.db.lock().unwrap();
        let back = get(&db, &f.id).unwrap().unwrap();
        assert_eq!(back.detail, "a very long transcript of a migration");
        assert!(back.tokens_before > back.tokens_after, "{back:?}");
    }

    #[test]
    fn a_fold_needs_both_a_topic_and_a_summary() {
        let st = db();
        let db = st.db.lock().unwrap();
        for (topic, summary) in [("", "s"), ("t", "   ")] {
            let e = fold(&db, &agent_scope(), "sl-1", None, topic, summary, "d", &[], None)
                .unwrap_err()
                .to_string();
            assert!(e.contains("fold needs"), "{e}");
        }
    }

    #[test]
    fn a_fold_is_a_stretch_of_the_work_and_not_all_of_it() {
        // This is a tool a model drives, and the honest failure is an agent
        // handing over its whole transcript in one call: that is copying, not
        // folding, and the database would pay for it.
        let st = db();
        let db = st.db.lock().unwrap();
        let huge = "x".repeat(MAX_DETAIL_BYTES + 1);
        let e = fold(&db, &agent_scope(), "sl-1", None, "everything", "s", &huge, &[], None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("MB of detail in one fold"), "{e}");
        assert!(e.contains("Fold a stretch"), "it has to say what to do instead: {e}");
        let wordy = "y".repeat(MAX_SUMMARY_BYTES + 1);
        let e = fold(&db, &agent_scope(), "sl-1", None, "t", &wordy, "d", &[], None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("KB. The summary is what you carry"), "{e}");
    }

    #[test]
    fn folds_fold_into_folds_and_the_children_step_aside() {
        // The tier. Three tier-1 folds become one tier-2, which is what keeps
        // a month of work down to a handful of summaries -- and the three are
        // reachable underneath it rather than gone.
        let st = db();
        let a = write(&st, "one", "the first long stretch");
        let b = write(&st, "two", "the second long stretch");
        let db = st.db.lock().unwrap();
        let top = fold(&db, &agent_scope(), "sl-1", None, "the week",
                       "what the week established", "", &[a.id.clone(), b.id.clone()], None)
            .unwrap();
        assert_eq!(top.tier, 2);
        // The map holds the parent, not the children.
        let v = visible(&db, &[agent_scope()], 50).unwrap();
        assert_eq!(v.len(), 1, "{v:#?}");
        assert_eq!(v[0].id, top.id);
        // And the children are underneath it, with their detail.
        let kids = children(&db, &top.id).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].detail, "the first long stretch");
        // What it stands for is what they stood for, not nothing.
        assert!(top.tokens_before >= a.tokens_before + b.tokens_before,
                "{} vs {} + {}", top.tokens_before, a.tokens_before, b.tokens_before);
    }

    #[test]
    fn a_stretch_of_work_is_never_under_two_parents() {
        let st = db();
        let a = write(&st, "one", "a stretch");
        let db = st.db.lock().unwrap();
        fold(&db, &agent_scope(), "sl-1", None, "first parent", "s", "",
             &[a.id.clone()], None)
            .unwrap();
        let e = fold(&db, &agent_scope(), "sl-1", None, "second parent", "s", "",
                     &[a.id.clone()], None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already been folded"), "{e}");
    }

    #[test]
    fn a_fold_cannot_reach_into_another_scope() {
        // Otherwise a seat's private working notes could be folded into the
        // team's memory by a teammate, and nobody would be able to find them.
        let st = db();
        let mine = write(&st, "mine", "my own stretch");
        let db = st.db.lock().unwrap();
        let e = fold(&db, &Scope::Session("s1".into()), "sl-2", None, "theirs", "s", "",
                     &[mine.id], None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("can only cover folds of its own scope"), "{e}");
    }

    #[test]
    fn a_fold_that_does_not_exist_is_said_so_rather_than_ignored() {
        let st = db();
        let db = st.db.lock().unwrap();
        let e = fold(&db, &agent_scope(), "sl-1", None, "t", "s", "",
                     &["fd-nope".to_string()], None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no fold 'fd-nope'"), "{e}");
    }

    #[test]
    fn the_detail_is_searchable_even_though_it_is_not_carried() {
        // A fold whose detail could not be found again would be a deletion
        // with extra steps.
        let st = db();
        write(&st, "the migration", "the column was renamed to tenant_id at line 441");
        let db = st.db.lock().unwrap();
        let hits = search(&db, &[agent_scope()], "tenant_id", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert!(search(&db, &[agent_scope()], "nothing like this", 10).unwrap().is_empty());
    }

    #[test]
    fn the_shape_says_what_is_carried_and_what_it_stands_for() {
        let st = db();
        write(&st, "one", &"x".repeat(4000));
        write(&st, "two", &"y".repeat(4000));
        let db = st.db.lock().unwrap();
        let sh = shape(&db, &agent_scope()).unwrap();
        assert_eq!(sh.count(), 2);
        assert!(sh.folded() >= 2000, "{sh:?}");
        assert!(sh.carried() < sh.folded() / 10, "{sh:?}");
    }

    #[test]
    fn a_seat_that_has_just_folded_can_be_told_apart_from_one_that_has_not() {
        // What the nudge turns on, and what decides whether a compaction goes
        // ahead regardless.
        let st = db();
        let before = now_ms();
        {
            let db = st.db.lock().unwrap();
            assert_eq!(folded_since(&db, "sl-1", before).unwrap(), 0);
        }
        write(&st, "one", "a stretch");
        let db = st.db.lock().unwrap();
        assert_eq!(folded_since(&db, "sl-1", before).unwrap(), 1);
        assert_eq!(folded_since(&db, "sl-other", before).unwrap(), 0);
    }

    #[test]
    fn reading_a_fold_back_is_recorded() {
        let st = db();
        let f = write(&st, "one", "a stretch");
        let db = st.db.lock().unwrap();
        note_unfolded(&db, &f.id).unwrap();
        note_unfolded(&db, &f.id).unwrap();
        let n: i64 = db
            .query_row("SELECT unfolded_count FROM folds WHERE id = ?", [&f.id], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn a_nearer_scope_is_read_first() {
        let st = db();
        write(&st, "my own", "mine");
        {
            let db = st.db.lock().unwrap();
            fold(&db, &Scope::Session("s1".into()), "sl-2", None, "the team's",
                 "s", "theirs", &[], None)
                .unwrap();
        }
        let db = st.db.lock().unwrap();
        let v = visible(&db, &[agent_scope(), Scope::Session("s1".into())], 50).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].topic, "my own", "the seat's own fold comes first");
    }
}
