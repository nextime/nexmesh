//! Skill enablement across scopes, and keeping seats' stored prompts in step.
//!
//! A seat's carried skills begin with its role template's own list. On top of
//! that the operator has three dials, widest to narrowest:
//!
//!   * **global** -- a fleet-wide default for a skill;
//!   * **session** -- a whole team's;
//!   * **seat** -- one seat's, which is the existing per-seat selection stored
//!     as the slot's `prompt_skills`, and which therefore always wins.
//!
//! A nearer scope wins over a wider one, exactly as a nearer memory scope
//! shadows a wider one -- the resolution itself is `roles::resolve_skills`,
//! kept pure in the proto crate; this module is only the part that needs the
//! database: reading the two wide dials, and, when one of them is turned,
//! re-rendering the stored prompt of every seat that was still following its
//! defaults so the change is actually visible.
//!
//! WHY SEAT SCOPE IS NOT A ROW HERE. The per-seat selection already lives on
//! the slot as the rendered-and-stored `prompt_skills`, and everything
//! downstream -- spawn, the byte count, the editor -- reads that one field.
//! Duplicating it into `skill_prefs` would be two sources of truth for the
//! same fact. So `skill_prefs` holds only the global and session dials; the
//! seat dial IS `prompt_skills`, and a seat that carries a selection of its
//! own has, by that fact, exercised seat scope and is left alone when a wider
//! dial moves.

use nexmesh_proto::now_ms;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::Path;

/// The enable/disable map set at one scope: skill name -> enabled.
pub fn prefs_at(db: &Connection, scope: &str, scope_ref: &str) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT skill, enabled FROM skill_prefs WHERE scope = ? AND scope_ref = ?")
    {
        if let Ok(rows) = stmt.query_map(params![scope, scope_ref], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        }) {
            for row in rows.flatten() {
                out.insert(row.0, row.1);
            }
        }
    }
    out
}

/// The wide overlays for a session, widest first: global, then that session.
/// Passed straight to `roles::resolve_skills` as the layers over a template's
/// own defaults. An empty session id contributes only the global layer.
fn overlays(db: &Connection, session_id: &str) -> Vec<BTreeMap<String, bool>> {
    let mut v = vec![prefs_at(db, "global", "")];
    if !session_id.is_empty() {
        v.push(prefs_at(db, "session", session_id));
    }
    v
}

/// A template's effective default skills in a session: its own list with the
/// global and session dials applied. This is what a seat carries when it has
/// made no selection of its own -- the base the per-seat toggles work from.
pub fn effective_defaults(
    db: &Connection,
    template_defaults: &[String],
    session_id: &str,
) -> Vec<String> {
    nexmesh_proto::roles::resolve_skills(template_defaults, &overlays(db, session_id))
}

/// Set or clear one skill's enablement at a scope. `enabled = None` removes the
/// row, reverting that skill to whatever the wider scopes or the template say.
pub fn set_pref(
    db: &Connection,
    scope: &str,
    scope_ref: &str,
    skill: &str,
    enabled: Option<bool>,
) -> rusqlite::Result<()> {
    match enabled {
        None => {
            db.execute(
                "DELETE FROM skill_prefs WHERE scope = ? AND scope_ref = ? AND skill = ?",
                params![scope, scope_ref, skill],
            )?;
        }
        Some(on) => {
            db.execute(
                "INSERT INTO skill_prefs (scope, scope_ref, skill, enabled, at_ms)
                 VALUES (?,?,?,?,?)
                 ON CONFLICT(scope, scope_ref, skill)
                 DO UPDATE SET enabled = excluded.enabled, at_ms = excluded.at_ms",
                params![scope, scope_ref, skill, on as i64, now_ms()],
            )?;
        }
    }
    Ok(())
}

/// One template-built seat, as propagation needs to see it.
struct Seat {
    id: String,
    session_id: String,
    template: String,
    skills: Vec<String>,
}

fn template_built_seats(db: &Connection, session_id: Option<&str>) -> Vec<Seat> {
    let sql = "SELECT id, session_id, prompt_template, COALESCE(prompt_skills, '[]')
                 FROM slots
                WHERE prompt_template IS NOT NULL AND prompt_template <> ''"
        .to_string();
    let sql = match session_id {
        Some(_) => format!("{sql} AND session_id = ?1"),
        None => sql,
    };
    let Ok(mut stmt) = db.prepare(&sql) else { return Vec::new() };
    let map = |r: &rusqlite::Row| {
        Ok(Seat {
            id: r.get(0)?,
            session_id: r.get(1)?,
            template: r.get(2)?,
            skills: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        })
    };
    let rows = match session_id {
        Some(s) => stmt.query_map(params![s], map),
        None => stmt.query_map([], map),
    };
    rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
}

/// Same set of names, order and case aside. Whether a seat's stored selection
/// is still exactly what its defaults would produce.
fn same_set(a: &[String], b: &[String]) -> bool {
    let norm = |v: &[String]| -> std::collections::BTreeSet<String> {
        v.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
    };
    norm(a) == norm(b)
}

/// After a global or session dial has moved, bring the SEATS THAT WERE
/// FOLLOWING THEIR DEFAULTS up to the new defaults, and leave every seat that
/// carries a selection of its own alone.
///
/// `before` is each affected seat's effective defaults computed BEFORE the
/// change (the caller captures it, because once the pref is written the old
/// value is gone). A seat whose stored skills match its own `before` set was
/// following defaults and is re-rendered to the new set; one that does not is
/// a per-seat choice -- seat scope -- and wins, so it is untouched.
///
/// Returns how many seats were re-rendered, for an honest receipt.
pub fn propagate(
    db: &Connection,
    dir: Option<&Path>,
    scope: &str,
    scope_ref: &str,
    before: &BTreeMap<String, Vec<String>>,
) -> usize {
    let session = (scope == "session").then_some(scope_ref);
    let mut changed = 0;
    for seat in template_built_seats(db, session) {
        let Some(old_defaults) = before.get(&seat.id) else { continue };
        if !same_set(&seat.skills, old_defaults) {
            continue; // a selection of its own: seat scope wins
        }
        let Some(t) = nexmesh_proto::roles::find(dir, &seat.template) else { continue };
        let new_defaults = effective_defaults(db, &t.skills, &seat.session_id);
        if same_set(&new_defaults, &seat.skills) {
            continue; // nothing actually moved for this seat
        }
        let rendered = nexmesh_proto::roles::render(&t, Some(&new_defaults), dir);
        let stored = serde_json::to_string(&new_defaults).unwrap_or_else(|_| "[]".into());
        if db
            .execute(
                "UPDATE slots SET system_prompt = ?, prompt_skills = ? WHERE id = ?",
                params![rendered, stored, seat.id],
            )
            .is_ok()
        {
            changed += 1;
        }
    }
    changed
}

/// Each affected seat's effective-defaults, captured before a dial is turned
/// so `propagate` can tell which seats were following them.
pub fn snapshot_defaults(
    db: &Connection,
    dir: Option<&Path>,
    scope: &str,
    scope_ref: &str,
) -> BTreeMap<String, Vec<String>> {
    let session = (scope == "session").then_some(scope_ref);
    let mut out = BTreeMap::new();
    for seat in template_built_seats(db, session) {
        if let Some(t) = nexmesh_proto::roles::find(dir, &seat.template) {
            out.insert(seat.id.clone(), effective_defaults(db, &t.skills, &seat.session_id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real DB is SCHEMA_SQL plus the column migrations; `system_prompt`
    // is one of the migrated columns, and propagate writes it, so the test
    // DB has to be built the same way the hub builds it.
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        crate::state::migrate(&c).unwrap();
        c
    }

    fn seat(c: &Connection, id: &str, session: &str, template: &str, skills: &[&str]) {
        c.execute(
            "INSERT INTO sessions (id, name, created_at_ms, last_active_ms)
             VALUES (?, ?, 0, 0) ON CONFLICT(id) DO NOTHING",
            params![session, session],
        )
        .unwrap();
        let list = serde_json::to_string(skills).unwrap();
        c.execute(
            "INSERT INTO slots (id, session_id, prompt_template, prompt_skills, created_at_ms)
             VALUES (?,?,?,?,0)",
            params![id, session, template, list],
        )
        .unwrap();
    }

    #[test]
    fn a_session_dial_moves_the_seats_that_followed_defaults_and_leaves_the_rest() {
        let c = db();
        // The reviewer template's own defaults are review-craft + independent-evidence.
        let defaults = nexmesh_proto::roles::find(None, "reviewer").unwrap().skills;
        // Two seats in team s1: one still on defaults, one with a selection of
        // its own (an extra skill). A third in s2 that must not be touched.
        seat(&c, "sl-plain", "s1", "reviewer", &defaults.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        seat(&c, "sl-custom", "s1", "reviewer", &["review-craft"]); // dropped one itself
        seat(&c, "sl-other", "s2", "reviewer", &defaults.iter().map(|s| s.as_str()).collect::<Vec<_>>());

        // Turn handover ON for the whole of s1.
        let before = snapshot_defaults(&c, None, "session", "s1");
        set_pref(&c, "session", "s1", "handover", Some(true)).unwrap();
        let n = propagate(&c, None, "session", "s1", &before);
        assert_eq!(n, 1, "only the default-following seat should move");

        let got = |id: &str| -> Vec<String> {
            let j: String = c
                .query_row("SELECT prompt_skills FROM slots WHERE id = ?", [id], |r| r.get(0))
                .unwrap();
            serde_json::from_str(&j).unwrap()
        };
        assert!(got("sl-plain").contains(&"handover".to_string()),
                "the default-following seat picked up the session skill");
        assert_eq!(got("sl-custom"), vec!["review-craft".to_string()],
                   "the seat with its own selection is untouched");
        assert!(!got("sl-other").contains(&"handover".to_string()),
                "a seat in another team is not affected by s1's dial");
    }

    #[test]
    fn a_global_dial_reaches_every_team_and_clearing_it_reverts() {
        let c = db();
        let defaults = nexmesh_proto::roles::find(None, "planner").unwrap().skills; // plan-shape, handover
        for (id, sess) in [("a", "s1"), ("b", "s2")] {
            seat(&c, id, sess, "planner",
                 &defaults.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        }
        // Disable handover fleet-wide.
        let before = snapshot_defaults(&c, None, "global", "");
        set_pref(&c, "global", "", "handover", Some(false)).unwrap();
        assert_eq!(propagate(&c, None, "global", "", &before), 2, "both teams' seats");
        let got = |id: &str| -> Vec<String> {
            let j: String = c
                .query_row("SELECT prompt_skills FROM slots WHERE id = ?", [id], |r| r.get(0))
                .unwrap();
            serde_json::from_str(&j).unwrap()
        };
        assert!(!got("a").contains(&"handover".to_string()));
        assert!(!got("b").contains(&"handover".to_string()));
        // Clear the dial: handover comes back for both.
        let before = snapshot_defaults(&c, None, "global", "");
        set_pref(&c, "global", "", "handover", None).unwrap();
        assert_eq!(propagate(&c, None, "global", "", &before), 2);
        assert!(got("a").contains(&"handover".to_string()));
    }
}
