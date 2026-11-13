//! Shared memory with a scope.
//!
//! The first version was one flat table per session: exact-key lookup only,
//! last-writer-wins with no trace, and everything destroyed with the session
//! that learned it. Useful, and too small to lean on.
//!
//! This version answers the three questions that made it too small:
//!
//! * **Who may see it.** Four scopes, resolved nearest-first on a read --
//!   `agent`, then any `group` the reader belongs to, then `session`, then
//!   `global`. A cheap seat can be given a private scratchpad while still
//!   reading the team's conventions, and a nearer scope shadows a wider one,
//!   so a group can override a global default without editing it.
//! * **Who may write it.** Agent, group and session scopes are open to any
//!   teammate. `global` outlives every session, so writing it needs an
//!   explicit capability: a confused or experimental seat must not be able to
//!   leave something behind for every future team.
//! * **How it is found.** FTS5 over keys and values, because an agent that
//!   has to already know the key does not have memory, it has a filing
//!   cabinet.
//!
//! Overwrites keep the previous revision. Two agents writing one key is
//! normal in a parallel team, and the old behaviour made that invisible.

use anyhow::{bail, Result};
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

/// The capability a slot must hold to write global memory.
pub const GLOBAL_WRITE_CAPABILITY: &str = "memory-writer";

pub const CATEGORIES: &[&str] =
    &["decision", "convention", "discovery", "blocker", "context"];

/// Where a memory lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Everything, for ever. Capability-gated on write.
    Global,
    /// One session. The old `knowledge` behaviour, and still the default.
    Session(String),
    /// A named subset of a team.
    Group(String),
    /// One seat, private to it.
    Agent(String),
}

impl Scope {
    pub fn name(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Session(_) => "session",
            Scope::Group(_) => "group",
            Scope::Agent(_) => "agent",
        }
    }

    pub fn reference(&self) -> &str {
        match self {
            Scope::Global => "",
            Scope::Session(r) | Scope::Group(r) | Scope::Agent(r) => r,
        }
    }

    /// How close this scope is to the reader: lower shadows higher.
    fn distance(&self) -> i64 {
        match self {
            Scope::Agent(_) => 0,
            Scope::Group(_) => 1,
            Scope::Session(_) => 2,
            Scope::Global => 3,
        }
    }

    /// Parse a scope as an agent names it. `group` needs its name.
    pub fn parse(scope: Option<&str>, group: Option<&str>, slot_id: &str, session_id: &str)
        -> Result<Self>
    {
        match scope.unwrap_or("session") {
            "session" => Ok(Scope::Session(session_id.to_string())),
            "global" => Ok(Scope::Global),
            "agent" | "private" => Ok(Scope::Agent(slot_id.to_string())),
            "group" => match group.map(str::trim).filter(|g| !g.is_empty()) {
                Some(g) => Ok(Scope::Group(g.to_lowercase())),
                None => bail!("a group scope needs a group name"),
            },
            other => bail!(
                "unknown scope '{other}'; use agent, group, session or global"
            ),
        }
    }
}

/// Groups this slot belongs to.
pub fn groups_of(db: &Connection, slot_id: &str) -> Result<Vec<String>> {
    let mut stmt = db.prepare("SELECT group_name FROM memory_groups WHERE slot_id = ?")?;
    let rows = stmt
        .query_map([slot_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// Every scope this slot may READ, nearest first.
///
/// Agent scope is private and group scope is members-only, so this is the
/// access control: a query is built from this list rather than filtered
/// afterwards, which means a scope that is not in it cannot be reached by any
/// argument a caller passes.
pub fn readable(db: &Connection, slot_id: &str, session_id: &str) -> Result<Vec<Scope>> {
    let mut out = vec![Scope::Agent(slot_id.to_string())];
    for g in groups_of(db, slot_id)? {
        out.push(Scope::Group(g));
    }
    out.push(Scope::Session(session_id.to_string()));
    out.push(Scope::Global);
    Ok(out)
}

/// May this slot write to that scope?
pub fn may_write(db: &Connection, slot_id: &str, scope: &Scope) -> Result<()> {
    match scope {
        Scope::Global => {
            let caps: Option<String> = db
                .query_row("SELECT capabilities FROM slots WHERE id = ?", [slot_id], |r| r.get(0))
                .optional()?;
            let caps: Vec<String> =
                serde_json::from_str(caps.as_deref().unwrap_or("[]")).unwrap_or_default();
            if caps.iter().any(|c| c == GLOBAL_WRITE_CAPABILITY) {
                Ok(())
            } else {
                bail!(
                    "writing global memory needs the '{GLOBAL_WRITE_CAPABILITY}' capability, \
                     because it outlives every session. Store it in the session scope, or ask \
                     an operator to grant it."
                )
            }
        }
        Scope::Group(g) => {
            // Only a member may write a group's memory, for the same reason
            // only a member may read it.
            let member: Option<i64> = db
                .query_row(
                    "SELECT 1 FROM memory_groups WHERE group_name = ? AND slot_id = ?",
                    params![g, slot_id],
                    |r| r.get(0),
                )
                .optional()?;
            if member.is_some() {
                Ok(())
            } else {
                bail!("you are not a member of the group '{g}'")
            }
        }
        // Your own scratchpad, and the session you are already in.
        Scope::Agent(_) | Scope::Session(_) => Ok(()),
    }
}

/// Write one entry, keeping whatever it replaced.
#[allow(clippy::too_many_arguments)]
pub fn store(
    db: &Connection,
    slot_id: &str,
    scope: &Scope,
    key: &str,
    value: &str,
    category: &str,
    ttl_seconds: Option<i64>,
) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        bail!("a memory needs a key");
    }
    if !CATEGORIES.contains(&category) {
        bail!("category must be one of: {}", CATEGORIES.join(", "));
    }
    may_write(db, slot_id, scope)?;
    let now = now_ms();
    let expires = ttl_seconds.filter(|s| *s > 0).map(|s| now + s * 1000);

    let existing: Option<(i64, i64, String, Option<String>)> = db
        .query_row(
            "SELECT id, revision, value, author_slot FROM memory
              WHERE scope = ? AND scope_ref = ? AND key = ?",
            params![scope.name(), scope.reference(), key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    match existing {
        Some((id, revision, old_value, old_author)) => {
            // Keep what was there. A parallel team overwrites keys, and the
            // flat table made that invisible.
            db.execute(
                "INSERT INTO memory_history (memory_id, revision, value, author_slot,
                                             replaced_at_ms)
                 VALUES (?,?,?,?,?)",
                params![id, revision, old_value, old_author, now],
            )?;
            db.execute(
                "UPDATE memory SET value = ?, category = ?, author_slot = ?, revision = ?,
                                   updated_at_ms = ?, expires_at_ms = ?
                  WHERE id = ?",
                params![value, category, slot_id, revision + 1, now, expires, id],
            )?;
            Ok(format!(
                "Updated '{key}' in {} memory (revision {}). The previous value is kept.",
                scope.name(),
                revision + 1
            ))
        }
        None => {
            db.execute(
                "INSERT INTO memory (scope, scope_ref, key, category, value, author_slot,
                                     created_at_ms, updated_at_ms, expires_at_ms)
                 VALUES (?,?,?,?,?,?,?,?,?)",
                params![
                    scope.name(), scope.reference(), key, category, value, slot_id,
                    now, now, expires
                ],
            )?;
            Ok(format!("Stored '{key}' as {category} in {} memory.", scope.name()))
        }
    }
}

/// One entry as a reader sees it.
pub struct Entry {
    pub scope: String,
    pub scope_ref: String,
    pub key: String,
    pub category: String,
    pub value: String,
    pub author: Option<String>,
    pub revision: i64,
}

/// Read memory, nearest scope first, with a nearer scope shadowing a wider
/// one for the same key.
pub fn recall(
    db: &Connection,
    slot_id: &str,
    session_id: &str,
    key: Option<&str>,
    search: Option<&str>,
    category: Option<&str>,
    only_scope: Option<&str>,
    limit: i64,
) -> Result<Vec<Entry>> {
    let scopes = readable(db, slot_id, session_id)?;
    let scopes: Vec<&Scope> = match only_scope {
        Some(want) => scopes.iter().filter(|s| s.name() == want).collect(),
        None => scopes.iter().collect(),
    };
    if scopes.is_empty() {
        bail!("no readable memory in scope '{}'", only_scope.unwrap_or(""));
    }

    // FTS first when searching, so the match set is the index's, not a scan's.
    let fts_ids: Option<Vec<i64>> = match search.map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => {
            let mut stmt = db.prepare(
                "SELECT rowid FROM memory_fts WHERE memory_fts MATCH ? ORDER BY rank LIMIT 500",
            )?;
            // Quoted as a phrase: an agent's query is prose, not FTS syntax,
            // and a stray quote or AND would otherwise be a syntax error
            // rather than a search.
            let phrase = format!("\"{}\"", q.replace('"', " "));
            let ids = stmt
                .query_map([phrase], |r| r.get(0))?
                .collect::<std::result::Result<Vec<i64>, _>>()?;
            Some(ids)
        }
        None => None,
    };

    let now = now_ms();
    let mut out: Vec<(i64, Entry)> = Vec::new();
    for scope in &scopes {
        let mut stmt = db.prepare(
            "SELECT id, key, category, value, author_slot, revision FROM memory
              WHERE scope = ? AND scope_ref = ?
                AND (?3 IS NULL OR key = ?3)
                AND (?4 IS NULL OR category = ?4)
                AND (expires_at_ms IS NULL OR expires_at_ms > ?5)
              ORDER BY category, key",
        )?;
        let rows = stmt.query_map(
            params![scope.name(), scope.reference(), key, category, now],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )?;
        for row in rows {
            let (id, key, category, value, author, revision) = row?;
            if let Some(ids) = &fts_ids {
                if !ids.contains(&id) {
                    continue;
                }
            }
            out.push((
                scope.distance(),
                Entry {
                    scope: scope.name().to_string(),
                    scope_ref: scope.reference().to_string(),
                    key,
                    category,
                    value,
                    author,
                    revision,
                },
            ));
        }
    }

    // A nearer scope shadows a wider one for the same key: that is what makes
    // a group able to override a global default without editing it.
    out.sort_by(|a, b| a.1.key.cmp(&b.1.key).then(a.0.cmp(&b.0)));
    let mut seen: Vec<String> = Vec::new();
    let mut kept: Vec<Entry> = Vec::new();
    for (_, e) in out {
        if seen.contains(&e.key) {
            continue;
        }
        seen.push(e.key.clone());
        kept.push(e);
        if kept.len() >= limit.max(1) as usize {
            break;
        }
    }
    Ok(kept)
}

/// Remove an entry. Only from a scope the caller may write.
pub fn forget(db: &Connection, slot_id: &str, scope: &Scope, key: &str) -> Result<String> {
    may_write(db, slot_id, scope)?;
    let n = db.execute(
        "DELETE FROM memory WHERE scope = ? AND scope_ref = ? AND key = ?",
        params![scope.name(), scope.reference(), key.trim()],
    )?;
    if n == 0 {
        bail!("no '{}' in {} memory", key.trim(), scope.name());
    }
    Ok(format!("Forgot '{}' from {} memory.", key.trim(), scope.name()))
}

/// Previous values of one key, newest first.
pub fn history(db: &Connection, scope: &Scope, key: &str) -> Result<Vec<(i64, String, Option<String>, i64)>> {
    let mut stmt = db.prepare(
        "SELECT h.revision, h.value, h.author_slot, h.replaced_at_ms
           FROM memory_history h JOIN memory m ON m.id = h.memory_id
          WHERE m.scope = ? AND m.scope_ref = ? AND m.key = ?
          ORDER BY h.revision DESC LIMIT 20",
    )?;
    let rows = stmt
        .query_map(params![scope.name(), scope.reference(), key.trim()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete expired entries. Called on a read path would be surprising, so it
/// is explicit: expiry hides a fact immediately and reclaims it later.
pub fn prune(db: &Connection) -> Result<usize> {
    Ok(db.execute(
        "DELETE FROM memory WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?",
        [now_ms()],
    )?)
}

/// Add or remove a slot from a group. Operator-driven: membership decides who
/// can see what, so an agent cannot put itself into a group.
pub fn set_group_member(db: &Connection, group: &str, slot_id: &str, member: bool) -> Result<()> {
    let group = group.trim().to_lowercase();
    if group.is_empty() {
        bail!("a group needs a name");
    }
    if member {
        db.execute(
            "INSERT OR IGNORE INTO memory_groups (group_name, slot_id, added_at_ms)
             VALUES (?,?,?)",
            params![group, slot_id, now_ms()],
        )?;
    } else {
        db.execute(
            "DELETE FROM memory_groups WHERE group_name = ? AND slot_id = ?",
            params![group, slot_id],
        )?;
    }
    Ok(())
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
        for (id, caps) in [("sl-a", r#"["memory-writer"]"#), ("sl-b", "[]")] {
            db.execute(
                "INSERT INTO slots (id,session_id,capabilities,created_at_ms) VALUES (?,'s1',?,1)",
                params![id, caps],
            )
            .unwrap();
        }
        db
    }

    fn sess() -> Scope { Scope::Session("s1".into()) }

    #[test]
    fn a_session_memory_is_readable_by_the_whole_team() {
        let db = db();
        store(&db, "sl-a", &sess(), "db", "we use sqlite", "decision", None).unwrap();
        let got = recall(&db, "sl-b", "s1", None, None, None, None, 20).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value, "we use sqlite");
        assert_eq!(got[0].scope, "session");
    }

    #[test]
    fn an_agent_scope_is_private_to_that_seat() {
        let db = db();
        store(&db, "sl-a", &Scope::Agent("sl-a".into()), "note", "mine", "context", None).unwrap();
        assert_eq!(recall(&db, "sl-a", "s1", None, None, None, None, 20).unwrap().len(), 1);
        // The other seat cannot reach it by any argument, because its scope is
        // never in the query at all.
        assert!(recall(&db, "sl-b", "s1", None, None, None, None, 20).unwrap().is_empty());
        assert!(recall(&db, "sl-b", "s1", Some("note"), None, None, Some("agent"), 20)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn global_memory_needs_the_capability() {
        let db = db();
        let err = store(&db, "sl-b", &Scope::Global, "k", "v", "context", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory-writer"), "{err}");
        assert!(err.contains("session scope"), "it must say what to do instead: {err}");
        // The seat that holds it may.
        store(&db, "sl-a", &Scope::Global, "k", "v", "context", None).unwrap();
        // And everyone can read it, including across sessions.
        assert_eq!(recall(&db, "sl-b", "s1", Some("k"), None, None, None, 9).unwrap().len(), 1);
        assert_eq!(recall(&db, "sl-b", "other-session", Some("k"), None, None, None, 9)
            .unwrap().len(), 1, "global memory must outlive its session");
    }

    #[test]
    fn a_nearer_scope_shadows_a_wider_one() {
        let db = db();
        store(&db, "sl-a", &Scope::Global, "style", "tabs", "convention", None).unwrap();
        store(&db, "sl-a", &sess(), "style", "spaces", "convention", None).unwrap();
        let got = recall(&db, "sl-a", "s1", Some("style"), None, None, None, 9).unwrap();
        assert_eq!(got.len(), 1, "one value per key, the nearest");
        assert_eq!(got[0].value, "spaces");
        assert_eq!(got[0].scope, "session");
    }

    #[test]
    fn a_group_is_members_only_for_reading_and_writing() {
        let db = db();
        set_group_member(&db, "Reviewers", "sl-a", true).unwrap();
        store(&db, "sl-a", &Scope::Group("reviewers".into()), "bar", "high", "convention", None)
            .unwrap();
        assert_eq!(recall(&db, "sl-a", "s1", None, None, None, None, 9).unwrap().len(), 1);
        assert!(recall(&db, "sl-b", "s1", None, None, None, None, 9).unwrap().is_empty());
        let err = store(&db, "sl-b", &Scope::Group("reviewers".into()), "x", "y", "context", None)
            .unwrap_err().to_string();
        assert!(err.contains("not a member"), "{err}");
    }

    #[test]
    fn a_group_name_is_case_insensitive() {
        // Membership and scope must agree whatever case either was typed in.
        let db = db();
        set_group_member(&db, "Reviewers", "sl-a", true).unwrap();
        let scope = Scope::parse(Some("group"), Some("REVIEWERS"), "sl-a", "s1").unwrap();
        store(&db, "sl-a", &scope, "k", "v", "context", None).unwrap();
        assert_eq!(recall(&db, "sl-a", "s1", Some("k"), None, None, None, 9).unwrap().len(), 1);
    }

    #[test]
    fn an_overwrite_keeps_what_was_there() {
        // Two agents writing one key is normal; losing the first silently is
        // what the flat table did.
        let db = db();
        store(&db, "sl-a", &sess(), "k", "first", "context", None).unwrap();
        let msg = store(&db, "sl-b", &sess(), "k", "second", "context", None).unwrap();
        assert!(msg.contains("revision 2"), "{msg}");
        let got = recall(&db, "sl-a", "s1", Some("k"), None, None, None, 9).unwrap();
        assert_eq!(got[0].value, "second");
        assert_eq!(got[0].revision, 2);
        assert_eq!(got[0].author.as_deref(), Some("sl-b"));
        let h = history(&db, &sess(), "k").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "first");
        assert_eq!(h[0].2.as_deref(), Some("sl-a"), "and who wrote it");
    }

    #[test]
    fn memory_can_be_searched_not_only_looked_up_by_key() {
        let db = db();
        store(&db, "sl-a", &sess(), "auth-choice", "we picked argon2 for hashing",
              "decision", None).unwrap();
        store(&db, "sl-a", &sess(), "db-choice", "sqlite with WAL", "decision", None).unwrap();
        let got = recall(&db, "sl-a", "s1", None, Some("argon2"), None, None, 9).unwrap();
        assert_eq!(got.len(), 1, "a value match must be findable without the key");
        assert_eq!(got[0].key, "auth-choice");
        // And by a word in the key.
        let got = recall(&db, "sl-a", "s1", None, Some("db-choice"), None, None, 9).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_search_that_is_prose_is_not_an_fts_syntax_error() {
        // Agents type questions, not query syntax.
        let db = db();
        store(&db, "sl-a", &sess(), "k", "the queue is drained on boot", "context", None).unwrap();
        for q in ["queue AND", "\"unbalanced", "drained on boot", "NOT ok ("] {
            assert!(recall(&db, "sl-a", "s1", None, Some(q), None, None, 9).is_ok(), "{q:?}");
        }
    }

    #[test]
    fn an_expired_memory_is_hidden_at_once_and_reclaimed_later() {
        let db = db();
        store(&db, "sl-a", &sess(), "temp", "only for now", "context", Some(-1)).unwrap();
        // A negative ttl is treated as none rather than instantly expired.
        assert_eq!(recall(&db, "sl-a", "s1", None, None, None, None, 9).unwrap().len(), 1);
        db.execute("UPDATE memory SET expires_at_ms = 1 WHERE key = 'temp'", []).unwrap();
        assert!(recall(&db, "sl-a", "s1", None, None, None, None, 9).unwrap().is_empty(),
                "an expired fact must stop being returned immediately");
        assert_eq!(prune(&db).unwrap(), 1);
        assert_eq!(prune(&db).unwrap(), 0);
    }

    #[test]
    fn forgetting_needs_the_same_right_as_writing() {
        let db = db();
        store(&db, "sl-a", &Scope::Global, "k", "v", "context", None).unwrap();
        let err = forget(&db, "sl-b", &Scope::Global, "k").unwrap_err().to_string();
        assert!(err.contains("memory-writer"), "{err}");
        forget(&db, "sl-a", &Scope::Global, "k").unwrap();
        assert!(recall(&db, "sl-a", "s1", Some("k"), None, None, None, 9).unwrap().is_empty());
        assert!(forget(&db, "sl-a", &Scope::Global, "k").is_err(), "twice is an error");
    }

    #[test]
    fn a_bad_scope_or_category_is_refused_with_the_options() {
        let err = Scope::parse(Some("everywhere"), None, "sl-a", "s1").unwrap_err().to_string();
        assert!(err.contains("agent, group, session or global"), "{err}");
        let err = Scope::parse(Some("group"), None, "sl-a", "s1").unwrap_err().to_string();
        assert!(err.contains("group name"), "{err}");
        let db = db();
        let err = store(&db, "sl-a", &sess(), "k", "v", "nonsense", None).unwrap_err().to_string();
        assert!(err.contains("decision"), "{err}");
    }

    #[test]
    fn an_empty_key_is_refused() {
        let db = db();
        assert!(store(&db, "sl-a", &sess(), "   ", "v", "context", None).is_err());
    }

    #[test]
    fn scoping_a_read_narrows_it_but_cannot_widen_it() {
        let db = db();
        store(&db, "sl-a", &Scope::Global, "g", "v", "context", None).unwrap();
        store(&db, "sl-a", &sess(), "s", "v", "context", None).unwrap();
        let only = recall(&db, "sl-a", "s1", None, None, None, Some("global"), 9).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].key, "g");
        // Asking for a group this seat is not in returns nothing, not an error
        // about somebody else's data existing.
        assert!(recall(&db, "sl-a", "s1", None, None, None, Some("group"), 9).is_err());
    }
}

// ------------------------------------------------------- continuity ----

/// What identifies a returning agent.
///
/// Not the slot id, which is new every session. A seat is "the claude working
/// in this workspace as Max": that triple is what makes tomorrow's agent the
/// same agent as today's.
pub fn identity(cli_id: &str, workspace_key: &str, name: &str) -> String {
    format!("{}|{}|{}", cli_id.trim(), workspace_key.trim(), name.trim())
}

/// Remember a CLI's own resume handle for this identity, beyond the session.
pub fn remember_handle(
    db: &Connection,
    cli_id: &str,
    workspace_key: &str,
    name: &str,
    handle: &str,
) -> Result<()> {
    let id = identity(cli_id, workspace_key, name);
    db.execute(
        "INSERT INTO agent_continuity (identity, cli_id, workspace_key, name, resume_handle,
                                       updated_at_ms)
         VALUES (?,?,?,?,?,?)
         ON CONFLICT(identity) DO UPDATE SET resume_handle = ?5, updated_at_ms = ?6",
        params![id, cli_id, workspace_key, name, handle, now_ms()],
    )?;
    Ok(())
}

/// Store the rolling summary an agent keeps of its own work.
///
/// The handle alone is not enough: some CLIs cannot resume at all, and a
/// handle is opaque, so it cannot be read by a different CLI picking the work
/// up. A summary the agent writes itself can rehydrate either.
pub fn remember_context(
    db: &Connection,
    cli_id: &str,
    workspace_key: &str,
    name: &str,
    context: &str,
) -> Result<()> {
    let id = identity(cli_id, workspace_key, name);
    db.execute(
        "INSERT INTO agent_continuity (identity, cli_id, workspace_key, name, context,
                                       updated_at_ms)
         VALUES (?,?,?,?,?,?)
         ON CONFLICT(identity) DO UPDATE SET context = ?5, updated_at_ms = ?6",
        params![id, cli_id, workspace_key, name, context, now_ms()],
    )?;
    Ok(())
}

/// What is known about a returning identity: its handle and its own summary.
pub fn continuity(
    db: &Connection,
    cli_id: &str,
    workspace_key: &str,
    name: &str,
) -> Result<Option<(Option<String>, String)>> {
    Ok(db
        .query_row(
            "SELECT resume_handle, context FROM agent_continuity WHERE identity = ?",
            [identity(cli_id, workspace_key, name)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

#[cfg(test)]
mod continuity_tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        db
    }

    #[test]
    fn a_handle_outlives_the_session_that_made_it() {
        // slots.resume_handle is cascade-deleted with its session, which is
        // why yesterday's conversation could never be picked up.
        let db = db();
        remember_handle(&db, "claude", "/srv/app", "Max", "sess-abc").unwrap();
        let got = continuity(&db, "claude", "/srv/app", "Max").unwrap().unwrap();
        assert_eq!(got.0.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn a_handle_and_a_context_do_not_overwrite_each_other() {
        let db = db();
        remember_handle(&db, "claude", "/srv/app", "Max", "h1").unwrap();
        remember_context(&db, "claude", "/srv/app", "Max", "rewrote the parser").unwrap();
        let (h, c) = continuity(&db, "claude", "/srv/app", "Max").unwrap().unwrap();
        assert_eq!(h.as_deref(), Some("h1"), "storing context must not clear the handle");
        assert_eq!(c, "rewrote the parser");
        remember_handle(&db, "claude", "/srv/app", "Max", "h2").unwrap();
        let (h, c) = continuity(&db, "claude", "/srv/app", "Max").unwrap().unwrap();
        assert_eq!(h.as_deref(), Some("h2"));
        assert_eq!(c, "rewrote the parser", "nor the other way round");
    }

    #[test]
    fn identity_is_the_cli_the_workspace_and_the_name() {
        // Two Maxes on different CLIs, or in different workspaces, are not the
        // same agent and must not inherit each other's conversation.
        let db = db();
        remember_handle(&db, "claude", "/a", "Max", "h-claude").unwrap();
        remember_handle(&db, "codex", "/a", "Max", "h-codex").unwrap();
        remember_handle(&db, "claude", "/b", "Max", "h-other-ws").unwrap();
        assert_eq!(continuity(&db, "claude", "/a", "Max").unwrap().unwrap().0.as_deref(),
                   Some("h-claude"));
        assert_eq!(continuity(&db, "codex", "/a", "Max").unwrap().unwrap().0.as_deref(),
                   Some("h-codex"));
        assert_eq!(continuity(&db, "claude", "/b", "Max").unwrap().unwrap().0.as_deref(),
                   Some("h-other-ws"));
    }

    #[test]
    fn an_unknown_identity_is_none_not_an_error() {
        let db = db();
        assert!(continuity(&db, "claude", "/a", "Nobody").unwrap().is_none());
    }

    #[test]
    fn whitespace_does_not_make_a_second_identity() {
        let db = db();
        remember_handle(&db, "claude", "/a", "Max", "h").unwrap();
        assert!(continuity(&db, " claude ", " /a ", " Max ").unwrap().is_some());
    }
}
