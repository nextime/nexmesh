//! HTTP API: the operator surface, plus the participant front door.
//!
//! The participant endpoints are what make a non-CLI agent -- a hermes agent,
//! a Python daemon, anything -- a first-class team member. It joins with a
//! token and a URL, calls the same tools through the same dispatcher, and
//! appears in `list_peers` like any other teammate. It needs no node, no pty
//! and no MCP implementation.

use crate::{state::AppState, tools};
use axum::{
    extract::{Path, Query, State},
    Extension,
    http::{HeaderMap, StatusCode},
    Json,
};
use nexmesh_proto::{self as proto, now_ms, Placement};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.to_string() })))
}

/// A session's id, given its id OR its name.
///
/// Every command that takes a session takes a name -- that is what you read
/// on the dashboard and type at a terminal -- but only the handlers that
/// happened to spell out `id = ? OR name = ?` accepted one. `nexmesh status
/// payments` therefore matched no rows and reported "no agents in this
/// session" for a session with four seats in it: the worst kind of wrong
/// answer, because it looks like news about the fleet rather than a failure
/// to understand the question.
///
/// Unresolvable input is handed back unchanged, so handlers that check
/// existence themselves still produce their own error.
fn session_ref_to_id(db: &rusqlite::Connection, given: &str) -> String {
    db.query_row(
        "SELECT id FROM sessions WHERE id = ?1 OR name = ?1 ORDER BY (id = ?1) DESC LIMIT 1",
        [given],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or_else(|| given.to_string())
}

fn bad(msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err(StatusCode::BAD_REQUEST, msg)
}

/// Capability labels, tidied so addressing them is predictable.
///
/// Lower-cased and de-duplicated: a teammate asking for "Strong" and one
/// asking for "strong" must reach the same seats, or the whole point of
/// addressing by capability is lost to spelling.
pub(crate) fn normalise_capabilities(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in raw {
        let c = c.trim().to_lowercase();
        if !c.is_empty() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

// ------------------------------------------------------------- operator ----

pub async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn list_nodes(State(st): State<AppState>) -> Json<Value> {
    Json(st.nodes_json())
}

#[derive(Deserialize)]
pub struct NewWorkspace {
    pub name: String,
    /// "node_local" or "repo".
    pub kind: String,
    pub node: Option<String>,
    pub repo_url: Option<String>,
    pub repo_branch: Option<String>,
    /// node name -> absolute path on that machine.
    #[serde(default)]
    pub mounts: std::collections::BTreeMap<String, String>,
    /// Shorthand for a node_local workspace: the path on its own node, so the
    /// common case needs one field rather than a map naming the same node.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

#[derive(Deserialize)]
pub struct RoleRequest {
    pub role: String,
    /// What the role means for this seat, in its own words. Left alone when
    /// omitted.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct NewSession {
    pub name: String,
    #[serde(default)]
    pub workspaces: Vec<NewWorkspace>,
    #[serde(default)]
    pub plan: Vec<String>,
    /// Total tokens this session may spend before it is paused. None = no cap.
    pub token_budget: Option<i64>,
}

/// Remove a slot: the seat, and the agent rows that only describe who sat
/// in it.
///
/// You could always add a seat and never take one away, which is how three
/// seats called "Hermes" came to sit in one session with nobody in two of
/// them. A seat someone is actually using is refused unless `force` is set,
/// because removing it under a working agent is not a tidy-up.
pub async fn remove_slot(
    State(st): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let force = matches!(q.get("force").map(|s| s.as_str()), Some("1") | Some("true"));
    let db = st.db.lock().unwrap();
    let exists: bool = db
        .query_row("SELECT 1 FROM slots WHERE id = ?", [&id], |_| Ok(true))
        .optional()
        .map_err(bad)?
        .unwrap_or(false);
    if !exists {
        return Err(bad(format!("no slot '{id}'")));
    }
    // "Live" means a lease that has not run out, not merely a row that was
    // never marked exited: a node restart leaves agents looking busy forever.
    let cutoff = now_ms() - proto::NODE_LEASE_MS as i64;
    let live: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM agents
              WHERE slot_id = ? AND exited_at_ms IS NULL AND status <> 'disconnected'
                AND COALESCE(last_seen_ms, 0) > ?",
            params![id, cutoff],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if live > 0 && !force {
        return Err(bad(format!(
            "slot '{id}' still has a live agent; stop it first, or pass force=1"
        )));
    }
    let agents = db.execute("DELETE FROM agents WHERE slot_id = ?", [&id]).map_err(bad)?;
    db.execute("DELETE FROM slots WHERE id = ?", [&id]).map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now_ms(), "operator", "slot_removed",
                serde_json::json!({ "slot": id, "agents": agents }).to_string()],
    )
    .ok();
    Ok(Json(serde_json::json!({ "removed": id, "agent_rows": agents })))
}

/// Remove a session and everything that belonged to it.
///
/// There was no way to do this at all, so a session made to try something --
/// and every scratch session anybody has ever made -- stayed on the dashboard
/// for ever, in the list you scan to see what the fleet is doing. A tool that
/// can create a thing and not remove it teaches you not to create things.
///
/// Refuses while anybody is still sitting in it, like `remove-agent` does: a
/// live agent is somebody's work, and deleting the session under it would
/// leave a process talking to a team that does not exist.
pub async fn remove_session(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ApiResult {
    let force = matches!(q.get("force").map(|s| s.as_str()), Some("1") | Some("true"));
    let db = st.db.lock().unwrap();
    let session_id = session_ref_to_id(&db, &session_ref);
    let name: Option<String> = db
        .query_row("SELECT name FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
        .optional()
        .map_err(bad)?;
    let Some(name) = name else {
        return Err(bad(format!("no session '{session_ref}'")));
    };
    // Live means a lease that has not run out, exactly as a seat removal
    // reckons it: a row nobody has written off is not evidence of a process.
    let cutoff = now_ms() - proto::NODE_LEASE_MS as i64;
    let live: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE s.session_id = ? AND a.exited_at_ms IS NULL
                AND a.status <> 'disconnected' AND COALESCE(a.last_seen_ms, 0) > ?",
            params![session_id, cutoff],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if live > 0 && !force {
        return Err(bad(format!(
            "'{name}' still has {live} live agent(s); release or stop them first,              or pass force=1"
        )));
    }
    let slots: i64 = db
        .query_row("SELECT COUNT(*) FROM slots WHERE session_id = ?", [&session_id], |r| r.get(0))
        .unwrap_or(0);
    db.execute(
        "DELETE FROM agents WHERE slot_id IN (SELECT id FROM slots WHERE session_id = ?)",
        [&session_id],
    )
    .map_err(bad)?;
    db.execute(
        "DELETE FROM message_recipients WHERE message_id IN
            (SELECT id FROM messages WHERE session_id = ?)",
        [&session_id],
    )
    .ok();
    // Everything keyed on the session. Listed rather than cascaded, so adding
    // a table makes you think about what removing a session means for it.
    for table in [
        "slots", "workspaces", "messages", "plan_items", "knowledge", "locks",
        "capability_asks", "guardrails",
    ] {
        db.execute(&format!("DELETE FROM {table} WHERE session_id = ?"), [&session_id]).ok();
    }
    // Tokens scoped to this session can only ever have reached this session.
    for table in ["operator_tokens", "participant_tokens"] {
        db.execute(&format!("DELETE FROM {table} WHERE session_id = ?"), [&session_id]).ok();
    }
    db.execute("DELETE FROM sessions WHERE id = ?", [&session_id]).map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now_ms(), "operator", "session_removed",
                serde_json::json!({ "session": session_id, "name": name, "slots": slots })
                    .to_string()],
    )
    .ok();
    Ok(Json(serde_json::json!({
        "removed": session_id, "name": name, "seats": slots
    })))
}

#[derive(Deserialize)]
pub struct MoveSlot {
    /// Target session: its id or its name.
    pub session: String,
}

/// Move a seat -- and whoever is sitting in it -- to another session.
///
/// No restart. An agent's session is not something it carries: tools resolve
/// it by looking up the agent's slot, so moving the slot moves the live
/// agent with it, and its next tool call lands in the new session.
///
/// What does NOT come along: the workspace, the file locks and the ownership
/// zones. All three are claims about the session being left -- a workspace
/// belongs to one session, and a lock on `src/**` there says nothing about
/// anywhere else -- so they are released rather than carried into a session
/// where they would mean something different.
pub async fn move_slot(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MoveSlot>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let from: String = db
        .query_row("SELECT session_id FROM slots WHERE id = ?", [&id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no slot '{id}'")))?;
    let (to, to_name, status): (String, String, String) = db
        .query_row(
            "SELECT id, name, status FROM sessions WHERE id = ? OR name = ?",
            params![req.session, req.session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no session '{}'", req.session)))?;
    if to == from {
        return Err(bad(format!("that seat is already in '{to_name}'")));
    }
    if status != "active" {
        return Err(bad(format!("session '{to_name}' is {status}, not active")));
    }

    // Anything undelivered where it is now can never be read there, because
    // the reader is leaving. Say how much rather than discarding it silently.
    let stranded: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM messages
              WHERE session_id = ? AND to_slot = ? AND acked_at_ms IS NULL",
            params![from, id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let locks = db.execute("DELETE FROM locks WHERE held_by_slot = ?", [&id]).unwrap_or(0);
    let zones = db.execute("DELETE FROM ownership WHERE slot_id = ?", [&id]).unwrap_or(0);
    db.execute(
        "UPDATE slots SET session_id = ?, workspace_id = NULL WHERE id = ?",
        params![to, id],
    )
    .map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now_ms(), "operator", "slot_moved",
                serde_json::json!({ "slot": id, "from": from, "to": to }).to_string()],
    )
    .ok();

    Ok(Json(serde_json::json!({
        "slot": id,
        "from": from,
        "to": to,
        "session_name": to_name,
        "locks_released": locks,
        "zones_released": zones,
        "messages_left_unread": stranded,
    })))
}

#[derive(Deserialize)]
pub struct RenameSession {
    pub name: String,
}

/// Rename a session.
///
/// A fleet gets named before anyone knows what it is for, and the name is
/// what everybody then reads on the dashboard and in every message. Making
/// it permanent would mean recreating the session to change a word.
pub async fn rename_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RenameSession>,
) -> ApiResult {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("session name must not be empty"));
    }
    if name.chars().count() > 64 {
        return Err(bad("a session name must be 64 characters or fewer"));
    }
    // Renaming by the CURRENT name is the usual case: nobody reads a session
    // id off the screen to type it back in.
    let id = session_ref_to_id(&st.db.lock().unwrap(), &id);
    let db = st.db.lock().unwrap();
    let n = db
        .execute("UPDATE sessions SET name = ? WHERE id = ?", params![name, id])
        .map_err(bad)?;
    if n == 0 {
        return Err(bad(format!("no session '{id}'")));
    }
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now_ms(), "operator", "session_renamed",
                serde_json::json!({ "session": id, "name": name }).to_string()],
    )
    .ok();
    Ok(Json(serde_json::json!({ "session_id": id, "name": name })))
}

/// Create one workspace, with its mounts, inside a session.
///
/// Shared by session creation and by mounting one later, because a workspace
/// is the same thing whenever it arrives: a place work happens, and the paths
/// that place has on each machine.
fn insert_workspace(
    db: &rusqlite::Connection,
    session_id: &str,
    w: &NewWorkspace,
) -> std::result::Result<String, (axum::http::StatusCode, Json<Value>)> {
    if !["node_local", "repo"].contains(&w.kind.as_str()) {
        return Err(bad(format!("workspace kind must be node_local or repo, got '{}'", w.kind)));
    }
    let node_id: Option<String> = match &w.node {
        Some(n) => Some(
            db.query_row("SELECT id FROM nodes WHERE name = ?", [n], |r| r.get(0))
                .optional()
                .map_err(bad)?
                .ok_or_else(|| bad(format!("no node named '{n}'")))?,
        ),
        None => None,
    };
    if w.kind == "node_local" && node_id.is_none() {
        return Err(bad(format!("workspace '{}' is node_local but names no node", w.name)));
    }
    let ws_id = format!("ws-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO workspaces (id, session_id, name, kind, node_id, repo_url, repo_branch, sync)
         VALUES (?,?,?,?,?,?,?,?)",
        params![ws_id, session_id, w.name, w.kind, node_id, w.repo_url, w.repo_branch,
                if w.kind == "repo" { "git" } else { "none" }],
    )
    .map_err(bad)?;
    // A node_local workspace is mounted where it lives, so naming the node
    // twice is not required of the caller.
    let mut mounts = w.mounts.clone();
    if w.kind == "node_local" && mounts.is_empty() {
        if let (Some(n), Some(p)) = (w.node.as_deref(), w.path.as_deref()) {
            mounts.insert(n.to_string(), p.to_string());
        }
    }
    for (node_name, path) in &mounts {
        if !path.starts_with('/') {
            return Err(bad(format!(
                "mount path must be absolute, got '{path}' -- a relative path means \
                 nothing on a machine you are not standing on"
            )));
        }
        let nid: Option<String> = db
            .query_row("SELECT id FROM nodes WHERE name = ?", [node_name], |r| r.get(0))
            .optional()
            .map_err(bad)?;
        let nid = nid.ok_or_else(|| bad(format!("mount names unknown node '{node_name}'")))?;
        // Two sessions MAY mount the same directory on the same machine, and
        // two agents in one directory may belong to different teams. A path
        // says where files are, not who you work with -- so this is allowed,
        // and `session_for_cwd` treats a contested directory as something it
        // cannot answer rather than something nobody may ask.
        db.execute(
            "INSERT INTO workspace_mounts (workspace_id, node_id, path) VALUES (?,?,?)",
            params![ws_id, nid, path],
        )
        .map_err(bad)?;
    }
    Ok(ws_id)
}

/// Change what a seat is FOR.
///
/// A role is not decoration: `list_peers` and the capability router use it
/// to decide who to ask, and the review state machine uses it to decide who
/// may approve. A team's needs change -- the engineer seat becomes the
/// reviewer seat when the work moves from building to checking -- and
/// deleting the seat to change that would throw away its history and its
/// inbox with it.
pub async fn set_role(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RoleRequest>,
) -> ApiResult {
    let role = req.role.trim();
    if role.is_empty() {
        return Err(bad("a role must not be empty"));
    }
    if role.chars().count() > 64 {
        return Err(bad("a role must be 64 characters or fewer"));
    }
    let db = st.db.lock().unwrap();
    let was: Option<String> = db
        .query_row("SELECT role FROM slots WHERE id = ?", [&id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no seat '{id}'")))?;
    db.execute(
        "UPDATE slots SET role = ?, role_description = COALESCE(?, role_description)
          WHERE id = ?",
        params![role, req.description, id],
    )
    .map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,'role_changed',?)",
        params![now_ms(), "operator",
                json!({ "slot": id, "from": was, "to": role }).to_string()],
    )
    .ok();
    Ok(Json(json!({ "slot_id": id, "role": role, "was": was })))
}

/// Rename a seat.
///
/// A name is not decoration: it is how every command, every message and every
/// log line addresses an agent, and a seat is usually named before anyone
/// knows what it will do -- `Claude-machine-c-3` says where it runs and nothing
/// about the work. Renaming changes nothing else: same seat, same agent, same
/// history, same inbox.
///
/// Two seats with one name in a session would make both unaddressable, so a
/// collision is refused rather than resolved by guessing which was meant.
pub async fn rename_slot(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RenameRequest>,
) -> ApiResult {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(bad("an agent name must not be empty"));
    }
    if name.chars().count() > 64 {
        return Err(bad("an agent name must be 64 characters or fewer"));
    }
    let db = st.db.lock().unwrap();
    let (slot_id, session_id, was): (String, String, Option<String>) = db
        .query_row(
            "SELECT id, session_id, display_name FROM slots WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no seat '{id}'")))?;
    let taken: Option<String> = db
        .query_row(
            "SELECT id FROM slots WHERE session_id = ?1 AND display_name = ?2 AND id <> ?3",
            params![session_id, name, slot_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?;
    if let Some(other) = taken {
        return Err(bad(format!(
            "another seat in this session is already called '{name}' ({other}); \
             two with one name leaves neither addressable"
        )));
    }
    db.execute("UPDATE slots SET display_name = ? WHERE id = ?", params![name, slot_id])
        .map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,'slot_renamed',?)",
        params![now_ms(), "operator",
                json!({ "slot": slot_id, "from": was, "to": name }).to_string()],
    )
    .ok();
    Ok(Json(json!({ "slot_id": slot_id, "name": name, "was": was })))
}

/// Mount a workspace into a session that already exists.
///
/// Without this a session's directories were fixed at creation, so the only
/// way to give a running session a directory -- which is how an attached CLI
/// finds the session it belongs to -- was to build another session and move
/// everyone across.
pub async fn add_workspace(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(w): Json<NewWorkspace>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let session_id: String = db
        .query_row("SELECT id FROM sessions WHERE id = ? OR name = ?", params![id, id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no session '{id}'")))?;
    let ws_id = insert_workspace(&db, &session_id, &w)?;
    Ok(Json(json!({ "workspace_id": ws_id, "session_id": session_id, "name": w.name })))
}

pub async fn create_session(State(st): State<AppState>, Json(req): Json<NewSession>) -> ApiResult {
    if req.name.trim().is_empty() {
        return Err(bad("session name must not be empty"));
    }
    let db = st.db.lock().unwrap();
    let now = now_ms();
    let id = format!("se-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO sessions (id, name, token_budget, created_at_ms, last_active_ms)
         VALUES (?,?,?,?,?)",
        params![id, req.name, req.token_budget, now, now],
    )
    .map_err(bad)?;

    for w in &req.workspaces {
        insert_workspace(&db, &id, w)?;
    }

    for (i, label) in req.plan.iter().enumerate() {
        db.execute(
            "INSERT INTO plan_items (session_id, ord, label, updated_at_ms) VALUES (?,?,?,?)",
            params![id, i as i64 + 1, label, now],
        )
        .map_err(bad)?;
    }
    Ok(Json(json!({ "session_id": id, "name": req.name })))
}

pub async fn list_sessions(
    State(st): State<AppState>,
    op: Option<Extension<std::sync::Arc<crate::state::Operator>>>,
) -> Json<Value> {
    // A scoped orchestrator is told about its own session and no other. Not
    // merely barred from driving them -- it cannot learn that they exist,
    // because a list of names is how you would find something to ask for.
    let scope = op.and_then(|Extension(o)| o.session.clone());
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(
        // Seats you can talk to, and -- separately -- the windows onto them.
        // Counting both together said `10` for a team of five with a
        // keyboard each, which is the plumbing showing through: every other
        // listing already leaves adopted terminals out.
        &format!(
            // Ordered by what happened LAST, not by what was made last: the
            // team somebody said something to a minute ago is the one you
            // came to look at, and on a fleet with seven of them the one
            // created most recently is rarely it. "Last activity" is the
            // newest of: a message in it, an agent starting in it, and the
            // session's own creation -- so a quiet team sinks and a busy one
            // rises without anything having to touch a column.
            "SELECT s.id, s.name, s.status, s.created_at_ms,
                    (SELECT COUNT(*) FROM slots x
                      WHERE x.session_id = s.id AND NOT {term}),
                    (SELECT COUNT(*) FROM slots x
                      WHERE x.session_id = s.id AND {term}),
                    MAX(s.created_at_ms,
                        COALESCE((SELECT MAX(m.sent_at_ms) FROM messages m
                                   WHERE m.session_id = s.id), 0),
                        COALESCE((SELECT MAX(a.started_at_ms) FROM agents a
                                   JOIN slots y ON y.id = a.slot_id
                                  WHERE y.session_id = s.id), 0)) AS last_at_ms
             FROM sessions s WHERE (?1 IS NULL OR s.id = ?1)
             ORDER BY last_at_ms DESC",
            term = crate::seat::is_terminal("x")
        ),
    ) else {
        return Json(json!({ "sessions": [] }));
    };
    let rows: Vec<Value> = stmt
        .query_map([&scope], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                "status": r.get::<_, String>(2)?, "created_at_ms": r.get::<_, i64>(3)?,
                "slots": r.get::<_, i64>(4)?,
                "terminals": r.get::<_, i64>(5)?,
                "last_at_ms": r.get::<_, i64>(6)?,
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    Json(json!({ "sessions": rows }))
}

#[derive(Deserialize)]
pub struct NewSlot {
    pub cli_id: Option<String>,
    #[serde(default)]
    pub participant_kind: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub role_description: Option<String>,
    pub workspace: Option<String>,
    pub task: Option<String>,
    /// Which model this seat should run, if the CLI can be told. Kept on the
    /// slot so a respawn after a crash gets the same one.
    pub model: Option<String>,
    /// Capability labels teammates can address: "strong", "cheap", "ocr", ...
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub placement: Placement,
    #[serde(default)]
    pub file_ownership: Vec<String>,
}

#[derive(Deserialize)]
pub struct Upload {
    /// What to call it. A name only: any directory part is dropped.
    pub name: String,
    /// The bytes, base64 (a `data:` URI is accepted, which is what a browser
    /// hands over).
    pub data_base64: String,
    /// Which workspace, when the session has more than one.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Which machine's copy to write to. Any that holds it, by default.
    #[serde(default)]
    pub node: Option<String>,
    /// The agent this is FOR. When given, the file lands on the machine that
    /// agent runs on, in the workspace that seat works in -- which is what
    /// "send this to the thing I am attached to" means, from a console on
    /// one machine or a browser on another.
    #[serde(default)]
    pub to: Option<String>,
    /// Directory inside the workspace. `uploads` unless said otherwise.
    #[serde(default)]
    pub dir: Option<String>,
    /// A line for the team's chat. Absent means a plain one is written.
    #[serde(default)]
    pub say: Option<String>,
}

/// Put a file into a session's workspace, and tell the team where it is.
///
/// An agent reads files, not chat attachments: a screenshot, a PDF, a
/// recording has to BE somewhere before anybody can be asked to look at it.
/// So the operator's file goes into the workspace on a machine that holds it,
/// and the path is said in the chat, which is what agents read.
///
/// The hub writes nothing itself. It picks the workspace and the machine and
/// asks that node, which is the only thing with a file system here.
pub async fn upload(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<Upload>,
) -> ApiResult {
    // A name, not a path: `../../etc/passwd` is a name somebody typed.
    let base = req
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('.')
        .to_string();
    let safe: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._- ".contains(c) { c } else { '_' })
        .collect();
    let safe = safe.trim().to_string();
    if safe.is_empty() {
        return Err(bad("that file needs a name"));
    }
    let dir = req.dir.as_deref().unwrap_or("uploads").trim().trim_matches('/').to_string();
    if dir.contains("..") {
        return Err(bad("the directory must be inside the workspace"));
    }

    // An agent named means its machine and its workspace: a file sent to
    // something you are attached to belongs where that something can read
    // it, not on whichever machine answered first.
    let for_agent: Option<(String, String, String)> = match req.to.as_deref() {
        None => None,
        Some(who) => {
            let db = st.db.lock().unwrap();
            let sid = session_ref_to_id(&db, &session_ref);
            let found = db
                .query_row(
                    "SELECT COALESCE(a.node_id,''), COALESCE(s.workspace_id, ''),
                            COALESCE(s.display_name, s.role, s.id)
                       FROM slots s
                       JOIN agents a ON a.id = (
                            SELECT id FROM agents WHERE slot_id = s.id
                             ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
                      WHERE s.session_id = ?1
                        AND (s.id = ?2 OR LOWER(COALESCE(s.display_name,'')) = LOWER(?2)
                             OR LOWER(COALESCE(s.role,'')) = LOWER(?2))",
                    params![sid, who],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(bad)?;
            if found.is_none() {
                return Err(bad(format!("no agent called '{who}' in that session")));
            }
            found
        }
    };

    let (session_id, workspace_id, ws_name) = {
        let db = st.db.lock().unwrap();
        let sid = session_ref_to_id(&db, &session_ref);
        // The agent's own workspace, when one was named and it has one.
        let want = match &for_agent {
            Some((_, ws, _)) if !ws.is_empty() => Some(ws.clone()),
            _ => req.workspace.clone(),
        };
        let found: Option<(String, String)> = db
            .query_row(
                "SELECT id, name FROM workspaces
                  WHERE session_id = ?1 AND (?2 IS NULL OR name = ?2 OR id = ?2)
                  ORDER BY (?2 IS NOT NULL) DESC LIMIT 1",
                params![sid, want],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(bad)?;
        let Some((wid, wname)) = found else {
            return Err(bad(
                "this session has no workspace to put a file in. Mount one first:                  nexmesh add-workspace <session> <name> --node <machine> --path <dir>",
            ));
        };
        (sid, wid, wname)
    };

    // A machine that holds this workspace AND is connected. Asked for by
    // name when the caller said which; any that answers otherwise.
    let mounts = crate::sync::mounts(&st, &workspace_id);
    // The agent's machine wins over anything else, because that is the
    // machine that has to be able to open the file.
    let want_node = for_agent
        .as_ref()
        .map(|(node, _, _)| node.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| req.node.clone());
    let pick = mounts
        .iter()
        .find(|(node_id, name, _)| {
            st.node_is_connected(node_id)
                && want_node.as_deref().is_none_or(|want| want == name || want == node_id)
        })
        .cloned();
    let Some((node_id, node_name, root)) = pick else {
        return Err(bad(match (&for_agent, &want_node) {
            (Some((_, _, who)), _) => format!(
                "{who} is on a machine that is not connected, or one that does not hold \
                 workspace '{ws_name}'"
            ),
            (_, Some(n)) => format!("{n} is not connected, or does not hold '{ws_name}'"),
            _ => format!("no connected machine holds workspace '{ws_name}' right now"),
        }));
    };

    let rel = if dir.is_empty() { safe.clone() } else { format!("{dir}/{safe}") };
    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(
        &node_id,
        &proto::HubToNode::PutFile {
            req_id,
            workspace_id: workspace_id.clone(),
            root: root.clone(),
            rel: rel.clone(),
            data_b64: req.data_base64.clone(),
        },
    )
    .await
    .map_err(bad)?;
    // Generous: a node writing 60MB to a slow disk is not a node in trouble.
    let answer = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
        .await
        .map_err(|_| bad(format!("{node_name} did not answer in time")))?
        .map_err(|_| bad("that machine went away while writing it"))?;
    if answer["ok"].as_bool() != Some(true) {
        return Err(bad(format!(
            "{node_name} could not write it: {}",
            answer["detail"].as_str().unwrap_or("no reason given")
        )));
    }
    let path = answer["path"].as_str().unwrap_or(&rel).to_string();
    let bytes = answer["bytes"].as_i64().unwrap_or(0);

    // The team is TOLD. A file nobody knows about is a file nobody reads, and
    // this message is the whole point of putting it there.
    let line = match req.say.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(said) => format!("{said}

The file is at {path} on {node_name} ({bytes} bytes)."),
        None => format!(
            "I have put {safe} in the workspace: {path} on {node_name} ({bytes} bytes).              Read it from there."
        ),
    };
    {
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES (?, NULL, NULL, 'system', ?, ?)",
            params![session_id, line, now_ms()],
        )
        .map_err(bad)?;
    }
    Ok(Json(json!({
        "ok": true, "path": path, "node": node_name, "bytes": bytes,
        "workspace": ws_name, "rel": rel
    })))
}

#[derive(Deserialize)]
pub struct PlanItem {
    pub label: String,
    /// Which seat is to do it: a name, a role or a seat id.
    #[serde(default)]
    pub owner: Option<String>,
    /// Where in the list. Appended when absent.
    #[serde(default)]
    pub ord: Option<i64>,
}

/// Add a step to a session's plan, optionally owned by one seat.
///
/// The plan could only be written when the session was created, which makes
/// it a statement of intent rather than a record of work. A task handed to a
/// team -- by the operator, or by an orchestrator that can see the whole
/// fleet -- belongs in it: that is where everyone looks to see what is
/// outstanding and who has it.
pub async fn add_plan_item(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<PlanItem>,
) -> ApiResult {
    let label = req.label.trim().to_string();
    if label.is_empty() {
        return Err(bad("a plan step needs a label"));
    }
    let db = st.db.lock().unwrap();
    let session_id = session_ref_to_id(&db, &session_ref);
    let exists: Option<i64> = db
        .query_row("SELECT 1 FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
        .optional()
        .map_err(bad)?;
    if exists.is_none() {
        return Err(bad(format!("no session '{session_ref}'")));
    }
    // The owner is a seat in THAT session, resolved the way every other
    // reference to an agent is.
    let owner: Option<String> = match req.owner.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        None => None,
        Some(who) => Some(
            db.query_row(
                "SELECT id FROM slots
                  WHERE session_id = ?1
                    AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                         OR LOWER(COALESCE(role,'')) = LOWER(?2))",
                params![session_id, who],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(bad)?
            .ok_or_else(|| bad(format!("no agent called '{who}' in that session")))?,
        ),
    };
    let ord = match req.ord {
        Some(n) => n,
        None => db
            .query_row(
                "SELECT COALESCE(MAX(ord), 0) + 1 FROM plan_items WHERE session_id = ?",
                [&session_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1),
    };
    db.execute(
        "INSERT INTO plan_items (session_id, ord, label, assignee_slot, updated_at_ms)
         VALUES (?,?,?,?,?)",
        params![session_id, ord, label, owner, now_ms()],
    )
    .map_err(bad)?;
    // Answer with the NAME, since that is what the caller said and what the
    // team reads; the slot id is the hub's business.
    let owner_name: Option<String> = match &owner {
        None => None,
        Some(slot) => db
            .query_row(
                "SELECT COALESCE(display_name, role, id) FROM slots WHERE id = ?",
                [slot],
                |r| r.get(0),
            )
            .optional()
            .map_err(bad)?,
    };
    Ok(Json(json!({
        "session": session_id, "ord": ord, "label": label,
        "owner": owner_name, "owner_slot": owner
    })))
}

#[derive(Deserialize)]
pub struct OwnsRequest {
    /// Patterns this seat is responsible for: `verti/**`, `src/auth/*`, or a
    /// plain name like `ombia`, which covers everything under it.
    pub patterns: Vec<String>,
    /// Add to what it already owns rather than replacing it.
    #[serde(default)]
    pub add: bool,
}

/// Say which parts of the workspace a seat is responsible for.
///
/// Ownership zones existed only at creation, which is the wrong moment: a
/// team divides a repository once it knows what is in it, and a seat that
/// turned out to be the one looking after a container had no way to be told
/// so. Teammates read this through `list_peers` and `who_owns`, and that is
/// what turns "four agents in one checkout" into four agents that hand work
/// to each other.
pub async fn set_owns(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<OwnsRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let ws: Option<String> = db
        .query_row("SELECT workspace_id FROM slots WHERE id = ?", [&id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .flatten();
    let Some(ws) = ws else {
        return Err(bad(format!(
            "slot '{id}' is not in a workspace, and an ownership zone is a part of one.              Put it in one first: nexmesh add-workspace <session> <name> ..."
        )));
    };
    if !req.add {
        db.execute("DELETE FROM ownership WHERE slot_id = ? AND workspace_id = ?",
                   params![id, ws]).map_err(bad)?;
    }
    let mut kept: Vec<String> = Vec::new();
    for pat in &req.patterns {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        db.execute(
            "INSERT OR IGNORE INTO ownership (workspace_id, slot_id, pattern) VALUES (?,?,?)",
            params![ws, id, pat],
        )
        .map_err(bad)?;
        kept.push(pat.to_string());
    }
    // What it owns NOW, which is not always what was just sent.
    let all: Vec<String> = db
        .prepare("SELECT pattern FROM ownership WHERE slot_id = ? AND workspace_id = ? ORDER BY 1")
        .and_then(|mut q| {
            q.query_map(params![id, ws], |r| r.get::<_, String>(0)).and_then(|m| m.collect())
        })
        .unwrap_or_default();
    Ok(Json(json!({ "slot": id, "workspace": ws, "added": kept, "owns": all })))
}

pub async fn create_slot(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<NewSlot>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let db = st.db.lock().unwrap();
    let exists: Option<i64> = db
        .query_row("SELECT 1 FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
        .optional()
        .map_err(bad)?;
    if exists.is_none() {
        return Err(bad(format!("no session '{session_id}'")));
    }
    let kind = req.participant_kind.unwrap_or_else(|| "cli".into());
    if kind == "cli" && req.cli_id.is_none() {
        return Err(bad("a cli slot needs a cli_id"));
    }
    let mode = req.mode.unwrap_or_else(|| "headless".into());
    if !["headless", "interactive"].contains(&mode.as_str()) {
        return Err(bad("mode must be headless or interactive"));
    }
    let ws_id: Option<String> = match &req.workspace {
        Some(w) => Some(
            db.query_row(
                "SELECT id FROM workspaces WHERE session_id = ? AND (name = ? OR id = ?)",
                params![session_id, w, w],
                |r| r.get(0),
            )
            .optional()
            .map_err(bad)?
            .ok_or_else(|| bad(format!("no workspace '{w}' in this session")))?,
        ),
        None => None,
    };
    let slot_id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                            placement, display_name, role, role_description, model,
                            capabilities, created_at_ms)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            slot_id, session_id, ws_id, req.cli_id, kind, mode,
            serde_json::to_string(&req.placement).unwrap_or_else(|_| "{}".into()),
            req.display_name, req.role, req.role_description, req.model,
            serde_json::to_string(&normalise_capabilities(&req.capabilities))
                .unwrap_or_else(|_| "[]".into()),
            now_ms()
        ],
    )
    .map_err(bad)?;
    if let (Some(ws), false) = (&ws_id, req.file_ownership.is_empty()) {
        for pat in &req.file_ownership {
            db.execute(
                "INSERT OR IGNORE INTO ownership (workspace_id, slot_id, pattern) VALUES (?,?,?)",
                params![ws, slot_id, pat],
            )
            .map_err(bad)?;
        }
    }
    if let Some(t) = &req.task {
        db.execute(
            "UPDATE slots SET role_description = COALESCE(role_description, '') || ?
             WHERE id = ?",
            params![format!("\n\nTASK: {t}"), slot_id],
        )
        .map_err(bad)?;
    }
    Ok(Json(json!({ "slot_id": slot_id })))
}

/// Everything in one subsystem, for someone who clicked it.
///
/// The map can only carry a handful of names per circle; this is what is
/// actually in it, and what it depends on. Kept out of the dashboard
/// snapshot on purpose -- it is wanted for one subsystem at a time, and
/// putting it in the poll would mean sending all of them, every time, to
/// nobody.
pub async fn subsystem_detail(
    State(st): State<AppState>,
    Path((workspace_ref, community)): Path<(String, i64)>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let ws_id: String = db
        .query_row(
            "SELECT id FROM workspaces WHERE id = ?1 OR name = ?1 LIMIT 1",
            [&workspace_ref],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no workspace '{workspace_ref}'")))?;

    let members: Vec<Value> = db
        .prepare(
            "SELECT name, path, kind, line FROM code_graph_nodes
              WHERE workspace_id = ? AND community = ?
              ORDER BY CASE kind WHEN 'type' THEN 0 WHEN 'function' THEN 1 ELSE 2 END,
                       name LIMIT 500",
        )
        .and_then(|mut q| {
            q.query_map(params![ws_id, community], |r| {
                Ok(json!({ "name": r.get::<_, String>(0)?, "path": r.get::<_, String>(1)?,
                           "kind": r.get::<_, String>(2)?, "line": r.get::<_, i64>(3)? }))
            })
            .and_then(|m| m.collect())
        })
        .map_err(bad)?;

    let files: Vec<Value> = db
        .prepare(
            "SELECT path, COUNT(*) FROM code_graph_nodes
              WHERE workspace_id = ? AND community = ? AND path <> ''
              GROUP BY path ORDER BY 2 DESC, path LIMIT 100",
        )
        .and_then(|mut q| {
            q.query_map(params![ws_id, community], |r| {
                Ok(json!({ "path": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)? }))
            })
            .and_then(|m| m.collect())
        })
        .map_err(bad)?;

    // Which way a dependency runs is the whole point of looking: "what does
    // this need" and "what needs this" are different questions with
    // different consequences.
    let neighbours: Vec<Value> = db
        .prepare(
            "SELECT CASE WHEN a.community = ?2 THEN b.community ELSE a.community END AS other,
                    CASE WHEN a.community = ?2 THEN 'uses' ELSE 'used_by' END AS dir,
                    SUM(CASE WHEN e.kind IN ('defines','imports') THEN 1 ELSE 0 END) AS certain,
                    COUNT(*) AS n
               FROM code_graph_edges e
               JOIN code_graph_nodes a ON a.workspace_id = e.workspace_id AND a.id = e.from_id
               JOIN code_graph_nodes b ON b.workspace_id = e.workspace_id AND b.id = e.to_id
              WHERE e.workspace_id = ?1
                AND a.community IS NOT NULL AND b.community IS NOT NULL
                AND a.community <> b.community
                AND (a.community = ?2 OR b.community = ?2)
              GROUP BY other, dir ORDER BY n DESC LIMIT 40",
        )
        .and_then(|mut q| {
            q.query_map(params![ws_id, community], |r| {
                Ok(json!({ "id": r.get::<_, i64>(0)?, "dir": r.get::<_, String>(1)?,
                           "certain": r.get::<_, i64>(2)? > 0, "n": r.get::<_, i64>(3)? }))
            })
            .and_then(|m| m.collect())
        })
        .map_err(bad)?;

    let total: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM code_graph_nodes WHERE workspace_id = ? AND community = ?",
            params![ws_id, community],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(Json(json!({
        "workspace_id": ws_id, "id": community, "n": total,
        "members": members, "files": files, "neighbours": neighbours,
    })))
}

/// Place a slot on a node and ask that node to start it.
#[derive(Deserialize, Default)]
pub struct SpawnRequest {
    /// Start a different CLI in this seat than the one it has been running.
    ///
    /// A seat is a place on a team, not a commitment to one program: the
    /// work may be better done by codex today than by claude yesterday, and
    /// having to delete the seat and lose its history to try is a poor
    /// trade. The change is kept, because the seat really is running that
    /// CLI from now on.
    #[serde(default)]
    pub cli: Option<String>,
    /// `headless` or `interactive`, when the caller has an opinion.
    ///
    /// Kept on the seat like `cli` is, and for the same reason: it really is
    /// how that seat runs from now on, including when something respawns it
    /// after a crash. Absent leaves the seat's own mode alone, which is what
    /// a script and the orchestrator want -- a team declared headless must
    /// not become interactive because something restarted it.
    #[serde(default)]
    pub mode: Option<String>,
}

pub async fn spawn_slot(
    State(st): State<AppState>,
    Path(slot_id): Path<String>,
    body: Option<Json<SpawnRequest>>,
) -> ApiResult {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    if let Some(cli) = req.cli.as_deref().filter(|c| !c.trim().is_empty()) {
        let known: bool = {
            let db = st.db.lock().unwrap();
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM node_clis WHERE cli_id = ?)",
                [cli],
                |r| r.get(0),
            )
            .unwrap_or(false)
        };
        if !known {
            return Err(bad(format!(
                "no machine in this fleet has '{cli}'; `nexmesh nodes` lists what each one has"
            )));
        }
        let db = st.db.lock().unwrap();
        db.execute("UPDATE slots SET cli_id = ? WHERE id = ?", params![cli, slot_id])
            .map_err(bad)?;
    }
    if let Some(mode) = req.mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        if mode != "headless" && mode != "interactive" {
            return Err(bad(format!(
                "mode must be 'headless' or 'interactive', not '{mode}'"
            )));
        }
        let db = st.db.lock().unwrap();
        db.execute("UPDATE slots SET mode = ? WHERE id = ?", params![mode, slot_id])
            .map_err(bad)?;
    }
    match crate::launch::launch_slot(&st, &slot_id, None).await {
        Ok((agent_id, node)) => {
            let mode: String = {
                let db = st.db.lock().unwrap();
                db.query_row("SELECT mode FROM slots WHERE id = ?", [&slot_id], |r| r.get(0))
                    .unwrap_or_else(|_| "headless".into())
            };
            Ok(Json(json!({
                "slot_id": slot_id, "agent_id": agent_id, "node": node,
                "cli": req.cli, "mode": mode,
            })))
        }
        Err(e) => Err(bad(e)),
    }
}

#[derive(Deserialize)]
pub struct AdoptRequest {
    /// Node whose tmux server holds the session.
    pub node: String,
    /// tmux target: a session, window or pane (`mysession`, `work:1.0`, `%3`).
    pub tmux_target: String,
    pub name: String,
    pub role: Option<String>,
    pub workspace: Option<String>,
}

/// Adopt an already-running terminal so it can be watched and driven from
/// anywhere. See clis/tmux.toml for why this attaches a second tmux client
/// rather than plumbing pipe-pane and send-keys.
pub async fn adopt_slot(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<AdoptRequest>,
) -> ApiResult {
    if req.tmux_target.trim().is_empty() {
        return Err(bad("tmux_target must not be empty"));
    }
    // Named or identified, like every other session-scoped call.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    // Adopting the same terminal twice is one terminal, not two.
    //
    // Every adopt minted a fresh seat, so re-running it after a reboot, a
    // node restart or simply because you were not sure it had worked left a
    // row of `term-...` twins, all pointing at one pane, all but one of them
    // dead. The whole point of the seat machinery is that a thing you come
    // back to keeps its place.
    let existing: Option<String> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT id FROM slots
              WHERE session_id = ?1 AND cli_id = 'tmux' AND tmux_target = ?2
                AND COALESCE(task_state,'') <> 'released'
                AND COALESCE(placement,'') LIKE '%' || ?3 || '%'
              ORDER BY created_at_ms DESC LIMIT 1",
            params![session_id, req.tmux_target, req.node],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?
    };
    if let Some(slot_id) = existing {
        {
            let db = st.db.lock().unwrap();
            if !req.name.trim().is_empty() {
                let _ = db.execute(
                    "UPDATE slots SET display_name = ? WHERE id = ?",
                    params![req.name, slot_id],
                );
            }
            if let Some(r) = &req.role {
                let _ = db.execute("UPDATE slots SET role = ? WHERE id = ?", params![r, slot_id]);
            }
            // Live already? Then there is nothing to do but say so.
            let live: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM agents WHERE slot_id = ?
                                    AND exited_at_ms IS NULL)",
                    [&slot_id],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if live {
                return Ok(Json(json!({
                    "slot_id": slot_id, "reused": true, "tmux_target": req.tmux_target,
                    "note": "that terminal was already adopted; nothing to do"
                })));
            }
        }
        let (agent_id, node) = crate::launch::launch_slot(&st, &slot_id, None).await.map_err(bad)?;
        return Ok(Json(json!({
            "slot_id": slot_id, "agent_id": agent_id, "node": node, "reused": true,
            "tmux_target": req.tmux_target,
            "note": "the terminal is adopted again, in the seat it had before"
        })));
    }
    let slot_id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    {
        let db = st.db.lock().unwrap();
        let exists: Option<i64> = db
            .query_row("SELECT 1 FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
            .optional()
            .map_err(bad)?;
        if exists.is_none() {
            return Err(bad(format!("no session '{session_id}'")));
        }
        let ws_id: Option<String> = match &req.workspace {
            Some(w) => db
                .query_row(
                    "SELECT id FROM workspaces WHERE session_id = ? AND (name = ? OR id = ?)",
                    params![session_id, w, w],
                    |r| r.get(0),
                )
                .optional()
                .map_err(bad)?,
            None => None,
        };
        db.execute(
            "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                                placement, display_name, role, role_description, tmux_target,
                                created_at_ms)
             VALUES (?,?,?, 'tmux', 'cli', 'interactive', ?,?,?,?,?,?)",
            params![
                slot_id,
                session_id,
                ws_id,
                serde_json::json!({ "node": req.node }).to_string(),
                req.name,
                req.role,
                "You are an adopted terminal session.\n\nTASK: continue what you were doing.".to_string(),
                req.tmux_target,
                now_ms()
            ],
        )
        .map_err(bad)?;
    }
    match crate::launch::launch_slot(&st, &slot_id, None).await {
        Ok((agent_id, node)) => Ok(Json(json!({
            "slot_id": slot_id, "agent_id": agent_id, "node": node,
            "tmux_target": req.tmux_target,
            "note": "the terminal is now attachable; it is NOT a team member unless the CLI \
                     inside it loads nexmesh-mcpd"
        }))),
        Err(e) => {
            // Do not leave a slot behind pointing at something we failed to open.
            let db = st.db.lock().unwrap();
            let _ = db.execute("DELETE FROM slots WHERE id = ?", [&slot_id]);
            Err(bad(e))
        }
    }
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    /// The identity to revoke: a node or operator name.
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Revoke a name's certificate, its token, and the connection it holds.
pub async fn revoke_cert(
    State(st): State<AppState>,
    Json(req): Json<RevokeRequest>,
) -> ApiResult {
    match crate::certs::revoke(&st, &req.name, req.reason.as_deref()).await {
        Ok(r) => Ok(Json(json!({
            "ok": true, "name": req.name, "certs": r.certs, "tokens": r.tokens,
            "kicked": r.kicked, "on_crl": r.on_crl, "tls_reloaded": r.reloaded,
        }))),
        Err(e) => Err(bad(format!("{e:#}"))),
    }
}

pub async fn list_certs(State(st): State<AppState>) -> Json<Value> {
    Json(crate::certs::list(&st))
}

#[derive(Deserialize)]
pub struct SyncRequest {
    /// publish | refresh | status. Default: status.
    #[serde(default)]
    pub op: Option<String>,
    /// Workspace name or id; omitted means every shared workspace in the session.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Publish, refresh or report on a shared workspace across every machine that
/// holds it.
pub async fn sync_workspaces(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<SyncRequest>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let op = match req.op.as_deref().unwrap_or("status") {
        "publish" => proto::SyncOp::Publish,
        "refresh" => proto::SyncOp::Refresh,
        "status" => proto::SyncOp::Status,
        "ensure" => proto::SyncOp::Ensure,
        other => return Err(bad(format!("op must be publish, refresh, ensure or status, not '{other}'"))),
    };
    let ids: Vec<String> = {
        let db = st.db.lock().unwrap();
        let mut stmt = db
            .prepare(
                "SELECT id FROM workspaces
                 WHERE session_id = ?1 AND kind = 'repo'
                   AND (?2 IS NULL OR name = ?2 OR id = ?2)",
            )
            .map_err(bad)?;
        stmt.query_map(params![session_id, req.workspace], |r| r.get(0))
            .and_then(|m| m.collect())
            .map_err(bad)?
    };
    if ids.is_empty() {
        return Err(bad("no shared workspace in this session to sync"));
    }
    let mut out = vec![];
    for id in ids {
        match crate::sync::run_everywhere(&st, &id, op, "operator", "operator sync").await {
            Ok(v) => out.push(v),
            Err(e) => out.push(json!({ "workspace": id, "error": format!("{e:#}") })),
        }
    }
    Ok(Json(json!({ "workspaces": out })))
}

#[derive(Deserialize)]
pub struct PauseRequest {
    /// true to pause, false to resume.
    pub paused: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// Agent name, role or slot id; omitted means the whole team.
    #[serde(default)]
    pub who: Option<String>,
}

pub async fn pause(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<PauseRequest>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let r = match req.who.as_deref().filter(|w| !w.trim().is_empty()) {
        Some(who) => {
            crate::control::set_paused(&st, &session_id, who, req.paused, req.reason.as_deref()).await
        }
        None => {
            crate::control::set_session_paused(&st, &session_id, req.paused, req.reason.as_deref())
                .await
        }
    };
    match r {
        Ok(detail) => Ok(Json(json!({ "ok": true, "detail": detail }))),
        Err(e) => Err(bad(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct GroupRequest {
    /// Group name. Lower-cased, so membership and scope always agree.
    pub group: String,
    /// Agent name, role or slot id.
    pub who: String,
    /// True to add, false to remove.
    #[serde(default = "yes")]
    pub member: bool,
}

fn yes() -> bool {
    true
}

/// Put an agent in a memory group, or take it out.
///
/// Operator-driven on purpose: membership decides who can read a group's
/// memory, so an agent must not be able to add itself to one.
pub async fn set_group(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<GroupRequest>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let db = st.db.lock().unwrap();
    let slot: Option<String> = db
        .query_row(
            "SELECT id FROM slots
              WHERE session_id = ?1
                AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                     OR LOWER(COALESCE(role,'')) = LOWER(?2))
              LIMIT 1",
            params![session_id, req.who],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?;
    let slot = slot.ok_or_else(|| bad(format!("no agent '{}' in {session_id}", req.who)))?;
    crate::memory::set_group_member(&db, &req.group, &slot, req.member)
        .map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({
        "ok": true, "group": req.group.trim().to_lowercase(), "slot": slot,
        "member": req.member
    })))
}

#[derive(Deserialize)]
pub struct BudgetRequest {
    /// New ceiling in total tokens; null clears it.
    pub token_budget: Option<i64>,
}

/// Set or clear a session's token budget.
///
/// Changing it re-arms the guard. Without this the budget could only ever
/// fire once per session: the crossing is stamped so a released session is
/// not immediately re-paused, which would otherwise mean raising a budget had
/// no effect.
pub async fn set_budget(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<BudgetRequest>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    if let Some(b) = req.token_budget {
        if b <= 0 {
            return Err(bad("a token budget must be positive; omit it to remove the ceiling"));
        }
    }
    let db = st.db.lock().unwrap();
    let n = db
        .execute(
            "UPDATE sessions SET token_budget = ?, budget_hit_at_ms = NULL WHERE id = ?",
            params![req.token_budget, session_id],
        )
        .map_err(bad)?;
    if n == 0 {
        return Err(bad(format!("no session '{session_id}'")));
    }
    let spent: i64 = db
        .query_row(
            "SELECT COALESCE(SUM(a.input_tokens + a.output_tokens), 0)
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE s.session_id = ?",
            [&session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    tracing::info!(%session_id, budget = ?req.token_budget, "token budget changed");
    Ok(Json(json!({
        "ok": true, "token_budget": req.token_budget, "spent": spent,
        "detail": match req.token_budget {
            Some(b) => format!("budget set to {b}; {spent} already spent"),
            None => format!("budget removed; {spent} spent so far"),
        }
    })))
}

#[derive(Deserialize)]
pub struct ReleaseRequest {
    /// Agent name, role or slot id; omitted means everyone.
    #[serde(default)]
    pub who: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// How long to let it finish before terminating it.
    #[serde(default)]
    pub grace_ms: Option<u64>,
}

pub async fn release(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<ReleaseRequest>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let grace = req.grace_ms.unwrap_or(15_000).clamp(0, 600_000);
    let r = match req.who.as_deref().filter(|w| !w.trim().is_empty()) {
        Some(who) => {
            crate::control::release(&st, &session_id, who, req.note.as_deref(), grace).await
        }
        None => crate::control::release_all(&st, &session_id, req.note.as_deref(), grace).await,
    };
    match r {
        Ok(detail) => Ok(Json(json!({ "ok": true, "detail": detail }))),
        Err(e) => Err(bad(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct OperatorNote {
    pub text: String,
    /// A super addressing one session's orchestrator. Omitted by a scoped
    /// orchestrator, which can only address the supers.
    #[serde(default)]
    pub to: Option<String>,
}

/// The operator channel: send.
///
/// A scoped orchestrator cannot see that another session exists, so when it
/// needs something from outside its own team the honest thing it can do is
/// ask whoever can see the fleet. It does not have to know who they are --
/// "the supers" is an address, not a person -- which also means a super can
/// be added or revoked without telling anybody.
pub async fn operator_note(
    State(st): State<AppState>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
    Json(req): Json<OperatorNote>,
) -> ApiResult {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(bad("text must not be empty"));
    }
    let db = st.db.lock().unwrap();
    // A scoped orchestrator writes to the supers, whatever it asks for; a
    // super writes to a named session's orchestrator, or to the supers.
    let to_session: Option<String> = match (&op.session, req.to.as_deref()) {
        // Silently redirecting a scoped orchestrator's message to the supers
        // would have it believe it had reached a session it cannot address.
        // Better to say so.
        (Some(_), Some(t)) => {
            return Err(bad(format!(
                "this token drives one session and cannot address '{t}'; send without 'to' \
                 and the super orchestrators will see it"
            )))
        }
        (Some(_), None) => None,
        (None, Some(t)) => Some(
            db.query_row(
                "SELECT id FROM sessions WHERE id = ?1 OR name = ?1 ORDER BY (id = ?1) DESC LIMIT 1",
                [t],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(bad)?
            .ok_or_else(|| bad(format!("no session '{t}'")))?,
        ),
        (None, None) => None,
    };
    db.execute(
        "INSERT INTO operator_messages (from_label, from_session, to_session, body, sent_at_ms)
         VALUES (?,?,?,?,?)",
        params![op.label, op.session, to_session, text, now_ms()],
    )
    .map_err(bad)?;
    Ok(Json(json!({
        "sent": true,
        "to": match &to_session { Some(s) => json!(s), None => json!("supers") },
    })))
}

/// The operator channel: read, and advance this operator's cursor.
///
/// A super reads everything addressed to the supers plus anything addressed
/// to it; a scoped orchestrator reads only what was addressed to its own
/// session. Nothing here lets a scoped token learn another session's name.
pub async fn operator_inbox(
    State(st): State<AppState>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
    Query(q): Query<LogQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let db = st.db.lock().unwrap();
    let after = match q.after {
        Some(a) => a,
        None => db
            .query_row("SELECT last_id FROM operator_read_cursor WHERE label = ?", [&op.label],
                       |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0),
    };
    // One statement, three bindings, whichever kind of operator is asking.
    // Two statements with different parameter counts is how the super's inbox
    // came back empty: the extra binding made query_map fail, and an empty
    // list is indistinguishable from "no messages" to whoever is reading.
    let Ok(mut stmt) = db.prepare(
        "SELECT id, from_label, from_session, to_session, body, sent_at_ms
           FROM operator_messages
          WHERE id > ?1
            AND ((?3 = '' AND to_session IS NULL) OR (?3 <> '' AND to_session = ?3))
          ORDER BY id LIMIT ?2",
    ) else {
        return Json(json!({ "messages": [] }));
    };
    let scope = op.session.clone().unwrap_or_default();
    let rows: Vec<Value> = stmt
        .query_map(params![after, limit, scope], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "from": r.get::<_, String>(1)?,
                "from_session": r.get::<_, Option<String>>(2)?,
                "to_session": r.get::<_, Option<String>>(3)?,
                "text": r.get::<_, String>(4)?,
                "sent_at_ms": r.get::<_, i64>(5)?,
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    // Reading is what marks them read; a cursor per operator, so two supers
    // each see a message once rather than one of them seeing it at all.
    if let Some(last) = rows.last().and_then(|r| r["id"].as_i64()) {
        let _ = db.execute(
            "INSERT INTO operator_read_cursor (label, last_id) VALUES (?1, ?2)
             ON CONFLICT(label) DO UPDATE SET last_id = MAX(last_id, ?2)",
            params![op.label, last],
        );
    }
    Json(json!({ "messages": rows, "as": op.label,
                 "scope": match &op.session { Some(s) => json!(s), None => json!("fleet") } }))
}

/// Clear messages nobody claimed.
///
/// The other half of messages that no longer clear themselves: they pile up,
/// and somebody has to be able to say "this no longer matters" without
/// pretending an agent dealt with it. Claimed by `operator`, so the log
/// still says who closed it and when.
pub async fn clear_unclaimed(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let session_id = session_ref_to_id(&db, &session_ref);
    let ids: Vec<i64> = db
        .prepare("SELECT id FROM messages WHERE session_id = ? AND claimed_at_ms IS NULL")
        .and_then(|mut q| {
            q.query_map([&session_id], |r| r.get::<_, i64>(0)).and_then(|m| m.collect())
        })
        .map_err(bad)?;
    let who = format!("operator:{}", op.label);
    let n = crate::tools::claim_messages(&db, &who, &ids).map_err(bad)?;
    Ok(Json(json!({ "cleared": n, "session_id": session_id })))
}

#[derive(Deserialize)]
pub struct OperatorAsk {
    pub capability: String,
    /// Arguments for the capability, as the manifest's `accepts` names them.
    #[serde(default)]
    pub request: Value,
    /// Which team's providers to ask. A super may omit it and the fleet is
    /// searched; a scoped token is confined to its own session anyway.
    #[serde(default)]
    pub session: Option<String>,
}

/// Use a team capability as the operator.
///
/// The capability machinery was reachable only by agents, so a person at the
/// dashboard could not use the very services their own fleet advertises --
/// the transcription running on their GPU box was available to every agent
/// and to nobody holding a phone.
pub async fn operator_ask(
    State(st): State<AppState>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
    Json(req): Json<OperatorAsk>,
) -> ApiResult {
    if req.capability.trim().is_empty() {
        return Err(bad("name a capability"));
    }
    let db = st.db.lock().unwrap();
    // Where to look: what was asked for, else this token's own session,
    // else anywhere a provider is sitting.
    let sessions: Vec<String> = match (req.session.as_deref(), op.session.as_deref()) {
        (Some(s), _) => vec![session_ref_to_id(&db, s)],
        (None, Some(scope)) => vec![scope.to_string()],
        (None, None) => db
            .prepare("SELECT id FROM sessions WHERE status = 'active'")
            .and_then(|mut q| q.query_map([], |r| r.get::<_, String>(0)).and_then(|m| m.collect()))
            .map_err(bad)?,
    };
    let body = serde_json::to_string(&req.request).unwrap_or_else(|_| "{}".into());
    for sid in &sessions {
        // "operator" is not a seat, so it excludes nobody from answering.
        match crate::asks::create(&db, sid, "operator", &req.capability, &body, None) {
            Ok((id, to_slot, label)) => {
                return Ok(Json(json!({
                    "ask_id": id, "to": label, "to_slot": to_slot, "session_id": sid
                })))
            }
            Err(_) => continue,
        }
    }
    Err(bad(format!(
        "nothing in {} offers '{}'",
        if sessions.len() == 1 { "that session".to_string() } else { "the fleet".to_string() },
        req.capability.trim().to_lowercase()
    )))
}

/// Has it answered yet? The dashboard polls this.
pub async fn operator_ask_result(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let db = st.db.lock().unwrap();
    match crate::asks::poll(&db, &id) {
        Ok(Some((status, result))) => json!({
            "done": true, "status": status, "result": result
        })
        .into(),
        _ => json!({ "done": false }).into(),
    }
}

/// Parse a workspace and build its map, now.
///
/// The graph was only ever built as a side effect of an agent asking about
/// the code, so a person looking at the dashboard of a fresh fleet was told
/// "nothing has been parsed yet" with no way to change that. The map is for
/// reading a codebase you did not write; needing an agent to want it first
/// is backwards.
pub async fn build_graph(
    State(st): State<AppState>,
    Path(workspace_ref): Path<String>,
) -> ApiResult {
    let found: Option<(String, String, String)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT w.id, m.node_id, m.path
               FROM workspaces w JOIN workspace_mounts m ON m.workspace_id = w.id
              WHERE w.id = ?1 OR w.name = ?1
              ORDER BY length(m.path) LIMIT 1",
            [&workspace_ref],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?
    };
    let (ws_id, node_id, path) =
        found.ok_or_else(|| bad(format!("no workspace '{workspace_ref}', or it has no mount")))?;
    match crate::graph::build(&st, &ws_id, &node_id, &path).await {
        Ok(summary) => Ok(Json(json!({ "workspace_id": ws_id, "summary": summary }))),
        Err(e) => Err(bad(format!("could not parse {path}: {e}"))),
    }
}

/// Retire the seats nothing is sitting in any more.
///
/// A seat outlives its process on purpose: that is what crash recovery
/// stands on, and what lets a conversation come back to where it was. The
/// cost is that a conversation RESUMED under a new id -- which is what
/// `claude --continue` gives you -- leaves its predecessor behind, and a
/// session fills with teammates who will never answer.
///
/// The hub must not resolve that by guessing: from here a resumed
/// conversation and a brand new one in the same directory are the same
/// thing, and guessing let a second window inherit the first's seat, name
/// and inbox. So retiring is a judgement, and it is the operator's. Only
/// seats that are SUPERSEDED go: dead, with a live seat on the same machine
/// in the same directory doing the same job. Anything still unclaimed for
/// them is carried across rather than deleted, because a message nobody
/// claimed is work somebody still has to do.
pub async fn tidy_seats(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let session_id = session_ref_to_id(&db, &session_ref);
    let rows: Vec<(String, String, String, String, String)> = db
        .prepare(
            "SELECT s.id, COALESCE(s.display_name,''), COALESCE(s.cli_id,''),
                    COALESCE(a.node_id,''), COALESCE(a.cwd,'')
               FROM slots s
               LEFT JOIN agents a ON a.id = (
                   SELECT id FROM agents WHERE slot_id = s.id
                    ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
              WHERE s.session_id = ?1 AND s.participant_kind = 'cli'",
        )
        .and_then(|mut q| {
            q.query_map([&session_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .and_then(|m| m.collect())
        })
        .map_err(bad)?;

    let mut live_places: std::collections::HashSet<(String, String, String)> =
        std::collections::HashSet::new();
    let mut dead: Vec<(String, String, String, String, String)> = Vec::new();
    for row in rows {
        let alive: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE slot_id = ?
                                AND exited_at_ms IS NULL AND status <> 'disconnected')",
                [&row.0],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if alive {
            live_places.insert((row.2.clone(), row.3.clone(), row.4.clone()));
        } else {
            dead.push(row);
        }
    }

    let mut retired = Vec::new();
    for (slot, name, cli, node, cwd) in dead {
        if cwd.is_empty() || !live_places.contains(&(cli.clone(), node.clone(), cwd.clone())) {
            continue;
        }
        // Where its unfinished work goes: the live seat in the same place.
        let heir: Option<String> = db
            .query_row(
                "SELECT s.id FROM slots s
                   JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
                                AND a.status <> 'disconnected'
                  WHERE s.session_id = ?1 AND COALESCE(s.cli_id,'') = ?2
                    AND a.node_id = ?3 AND a.cwd = ?4 LIMIT 1",
                params![session_id, cli, node, cwd],
                |r| r.get(0),
            )
            .optional()
            .map_err(bad)?;
        if let Some(heir) = heir {
            db.execute(
                "UPDATE messages SET to_slot = ?1 WHERE to_slot = ?2 AND claimed_at_ms IS NULL",
                params![heir, slot],
            )
            .map_err(bad)?;
        }
        db.execute("DELETE FROM agents WHERE slot_id = ?", [&slot]).map_err(bad)?;
        db.execute("DELETE FROM slots WHERE id = ?", [&slot]).map_err(bad)?;
        retired.push(json!({ "slot_id": slot, "name": name, "cwd": cwd }));
    }
    Ok(Json(json!({ "retired": retired.len(), "seats": retired })))
}

pub async fn session_status(State(st): State<AppState>, Path(session_ref): Path<String>) -> Json<Value> {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(&format!(
        "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.participant_kind, COALESCE(s.cli_id, '-'), s.task_state,
                s.summary, COALESCE(a.provenance, '-'), {word},
                COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external' ELSE '-' END),
                COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0), s.paused,
                a.pid, COALESCE(a.io_kind, CASE WHEN s.terminal_via IS NOT NULL
                                                THEN 'tmux' END),
                a.id, a.exited_at_ms, s.terminal_via IS NOT NULL,
                COALESCE(a.status, 'disconnected'),
                s.cli_session, a.cwd, s.tmux_target,
                -- Which agent an attach should open: this seat's own
                -- process when it has a terminal, and otherwise the adopted
                -- terminal it is sitting in. The name you address is the
                -- name you attach to; which pty that turns out to be is the
                -- hub's business, not yours.
                COALESCE(
                    CASE WHEN a.io_kind IS NULL THEN
                        (SELECT v.id FROM agents v
                          WHERE v.id = s.terminal_via AND v.exited_at_ms IS NULL)
                    END, a.id)
         FROM slots s
         LEFT JOIN agents a ON a.id = (
             SELECT id FROM agents WHERE slot_id = s.id
             ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE s.session_id = ? ORDER BY 2",
        word = crate::seat::agent_word("s", "a"),
    )) else {
        return Json(json!({ "agents": [] }));
    };
    let rows: Vec<Value> = stmt
        .query_map([&session_id], |r| {
            Ok(json!({
                "slot_id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                "role": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                "cli": r.get::<_, String>(4)?, "task_state": r.get::<_, String>(5)?,
                "summary": r.get::<_, String>(6)?, "provenance": r.get::<_, String>(7)?,
                "status": r.get::<_, String>(8)?, "host": r.get::<_, String>(9)?,
                "input_tokens": r.get::<_, i64>(10)?, "output_tokens": r.get::<_, i64>(11)?,
                "paused": r.get::<_, i64>(12)? == 1,
                // The process behind the seat, and whether it has a terminal.
                // A seat reading `idle` with nothing running behind it is how
                // an attach fails for no visible reason: the task state
                // describes the WORK, which outlives the process doing it.
                "pid": r.get::<_, Option<i64>>(13)?,
                "io_kind": r.get::<_, Option<String>>(14)?,
                "agent_id": r.get::<_, Option<String>>(15)?,
                "exited": r.get::<_, Option<i64>>(16)?.is_some(),
                // A seat with no process of its own that is nonetheless
                // reachable: its CLI sits in a terminal a node has adopted,
                // so a message can be typed at it. IO reads `tmux`.
                "typed_via_terminal": r.get::<_, bool>(17)?,
                // The row's own word, for anything that must not be fooled
                // by the kinder one above: `waiting` is a seat, `exited` is
                // a process, and the two are answers to different questions.
                "agent_status": r.get::<_, String>(18)?,
                // Which conversation is in this seat, where it was running,
                // and the terminal it lives in if it has one. Enough to put
                // the same conversation back in the same place -- which is
                // what `nexmesh seat` does.
                "cli_session": r.get::<_, Option<String>>(19)?,
                "cwd": r.get::<_, Option<String>>(20)?,
                "tmux_target": r.get::<_, Option<String>>(21)?,
                "attach_agent_id": r.get::<_, Option<String>>(22)?,
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    Json(json!({ "session_id": session_id, "agents": rows }))
}

#[derive(Deserialize)]
pub struct LogQuery {
    /// Only messages with a higher id. Makes `--follow` cheap and gap-free.
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

/// The conversation between agents, as the operator sees it.
///
/// Every message is here: who sent it, who it was addressed to (null means the
/// whole team), what kind it was, and -- crucially -- whether it has actually
/// reached the recipient's model yet. A message that is sent but unacked is a
/// message the recipient has not seen, which is usually what you want to know
/// when a team looks stuck.
pub async fn session_log(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Query(q): Query<LogQuery>,
) -> Json<Value> {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let after = q.after.unwrap_or(0);
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(&format!(
        "SELECT m.id, m.sent_at_ms, m.kind, m.body,
                COALESCE(sf.display_name, sf.role, 'operator'),
                CASE WHEN m.to_slot IS NULL THEN '(team)'
                     ELSE COALESCE(st2.display_name, st2.role, m.to_slot) END,
                {delivered},
                m.inflight_at_ms IS NOT NULL,
                {seen}, {audience}
         FROM messages m
         LEFT JOIN slots sf ON sf.id = m.from_slot
         LEFT JOIN slots st2 ON st2.id = m.to_slot
         WHERE m.session_id = ?1 AND m.id > ?2
         ORDER BY m.id ASC LIMIT ?3",
        delivered = crate::seat::delivered("m"),
        seen = crate::seat::seen("m"),
        audience = crate::seat::audience("m"),
    )) else {
        return Json(json!({ "messages": [] }));
    };
    let rows: Vec<Value> = stmt
        .query_map(params![session_id, after, limit], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "at_ms": r.get::<_, i64>(1)?,
                "kind": r.get::<_, String>(2)?,
                "body": r.get::<_, String>(3)?,
                "from": r.get::<_, String>(4)?,
                "to": r.get::<_, String>(5)?,
                "delivered": r.get::<_, bool>(6)?,
                "in_flight": r.get::<_, bool>(7)?,
                // How many of the seats it was for have seen it, out of how
                // many could. For a team message these are the numbers that
                // matter: one teammate acking is not the team hearing you.
                "seen": r.get::<_, i64>(8)?,
                "audience": r.get::<_, i64>(9)?,
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    Json(json!({ "session_id": session_id, "messages": rows }))
}

#[derive(Deserialize)]
pub struct TranscriptQuery {
    pub tail: Option<usize>,
}

/// What an agent's process actually printed. Separate from the message log:
/// this is the agent's own output, the message log is the team conversation.
pub async fn agent_transcript(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> ApiResult {
    let tail = q.tail.unwrap_or(64 * 1024).min(2 * 1024 * 1024);
    let text = st.read_transcript(&agent_id, tail).map_err(bad)?;
    Ok(Json(json!({ "agent_id": agent_id, "transcript": text })))
}

/// Map a friendly name or slot id to the agent that last filled it, so the
/// operator can say `nexmesh transcript Max` instead of hunting an agent id.
pub async fn resolve_agent(
    State(st): State<AppState>,
    Path((session_ref, who)): Path<(String, String)>,
) -> ApiResult {
    // A session may be named, not only identified -- and `nexmesh attach
    // orchestrator Max` passes the name, because every other command takes
    // one. Without this the lookup compared a name against session ids,
    // matched nothing, and reported no agent for a seat that was listed on
    // screen a second earlier.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    let db = st.db.lock().unwrap();
    let found: Option<(String, String)> = db
        .query_row(
            "SELECT a.id, COALESCE(s.display_name, s.role, s.id)
             FROM slots s JOIN agents a ON a.slot_id = s.id
             WHERE s.session_id = ?1
               AND (s.id = ?2 OR a.id = ?2
                    OR LOWER(COALESCE(s.display_name,'')) = LOWER(?2)
                    OR LOWER(COALESCE(s.role,'')) = LOWER(?2))
             ORDER BY (a.exited_at_ms IS NULL) DESC, a.started_at_ms DESC LIMIT 1",
            params![session_id, who],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(bad)?;
    // "No agent matching" is true of a seat nobody is sitting in, and
    // unhelpful: the seat is in every listing, so the operator is left
    // doubting the name they just read. Say which of the two it is.
    let (agent_id, name) = match found {
        Some(v) => v,
        None => {
            let seat: Option<(String, String)> = db
                .query_row(
                    "SELECT id, COALESCE(display_name, role, id) FROM slots
                      WHERE session_id = ?1
                        AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                             OR LOWER(COALESCE(role,'')) = LOWER(?2))
                      ORDER BY created_at_ms DESC LIMIT 1",
                    params![session_id, who],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(bad)?;
            return Err(match seat {
                Some((slot_id, seat_name)) => bad(format!(
                    "seat {slot_id} ({seat_name}) has no process behind it: nothing has \
                     connected to it yet, or what did has gone. Start the CLI there again, \
                     or `nexmesh spawn` into the seat."
                )),
                None => bad(format!("no agent or seat matching '{who}' in this session")),
            });
        }
    };
    // Whether that agent is still there. The newest agent for a seat is the
    // right answer for `transcript`, which reads what a finished agent said,
    // and the wrong one for `attach`, which needs a process -- so the caller
    // is told rather than left to find out from a failure further on.
    let live: Option<(String, Option<i64>, Option<String>, Option<String>)> = db
        .query_row(
            "SELECT a.status, a.exited_at_ms, n.name, a.io_kind
               FROM agents a LEFT JOIN nodes n ON n.id = a.node_id
              WHERE a.id = ?",
            [&agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(bad)?;
    let (status, exited, host, io_kind) =
        live.unwrap_or_else(|| ("unknown".into(), None, None, None));

    // The terminal this seat is SITTING IN, when its own process has none.
    //
    // A CLI somebody started by hand has no pty the node owns -- the pty
    // belongs to the adopted tmux pane around it, which is a seat of its
    // own. So `nexmesh attach docs-site Docs-dev` used to be refused, and you
    // had to know to ask for `term-Docs-dev` instead, or go and find the
    // tmux session by hand. That is the plumbing showing through: the name
    // you address an agent by should open the screen it is on.
    //
    // The link is the one `termlink` proved: the node checked, through
    // /proc, that this seat's CLI is the process inside that pane. So an
    // attach that follows it is looking at the right agent, not merely at a
    // terminal that happens to be nearby.
    let via: Option<(String, String, Option<String>)> = if io_kind.is_none() {
        db.query_row(
            // `vs` is the TERMINAL's own seat, not the one being resolved:
            // the name wanted here is the terminal's, so a viewer can say
            // which screen it put you on.
            "SELECT v.id, COALESCE(vs.display_name, vs.role, vs.id), v.io_kind
               FROM slots s
               JOIN agents v ON v.id = s.terminal_via
               JOIN slots vs ON vs.id = v.slot_id
              WHERE s.id = (SELECT slot_id FROM agents WHERE id = ?1)
                AND v.exited_at_ms IS NULL",
            [&agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?
    } else {
        None
    };

    match via {
        Some((via_id, via_name, via_io)) => Ok(Json(json!({
            // The agent to attach to is the terminal's; the NAME stays the
            // one that was asked for, because that is who you are looking at.
            "agent_id": via_id, "name": name,
            "status": status, "exited": exited.is_some(),
            "host": host, "io_kind": via_io,
            // Said plainly, so a viewer can tell you whose screen this is.
            "via": via_name, "seat_agent_id": agent_id,
        }))),
        None => Ok(Json(json!({
            "agent_id": agent_id, "name": name,
            "status": status, "exited": exited.is_some(),
            // Which machine, and what kind of terminal -- so a viewer can say
            // whose screen you are looking at before it draws a byte of it.
            "host": host, "io_kind": io_kind,
        }))),
    }
}

#[derive(Deserialize)]
pub struct DirectAttach {
    /// The agent whose terminal is wanted -- its id, as `resolve` returns it.
    pub agent: String,
}

/// Authorise an attach that does not go through the hub.
///
/// The hub stays the only thing that decides WHO may attach to WHAT. What it
/// need not be is the wire: a keystroke typed on machine-b for an agent on machine-b
/// made two network hops and a relay on a hub elsewhere. So this mints a
/// one-time ticket, pushes it to the node holding that terminal over the
/// connection it already has, and returns it to the operator, who presents it
/// to the node directly. See `proto::attachsock`.
///
/// The ticket is worth nothing on its own: only the node the hub sent it to
/// will honour it, only for the agent it names, only once, and only for the
/// next few seconds. An operator who cannot reach the node falls back to
/// attaching through the hub, which is why that path remains.
pub async fn attach_ticket(
    State(st): State<AppState>,
    op: Option<Extension<std::sync::Arc<crate::state::Operator>>>,
    Json(req): Json<DirectAttach>,
) -> ApiResult {
    let who = op
        .as_ref()
        .map(|Extension(o)| o.label.clone())
        .unwrap_or_else(|| "operator".into());
    let scope = op.and_then(|Extension(o)| o.session.clone());
    let found: Option<(Option<String>, Option<i64>, String)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT a.node_id, a.exited_at_ms, s.session_id
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE a.id = ?",
            [&req.agent],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?
    };
    let Some((node_id, exited, session_id)) = found else {
        return Err(bad("no such agent, or it has no seat"));
    };
    // A scoped orchestrator may not reach outside its own session, here as
    // everywhere else.
    if let Some(only) = &scope {
        if only != &session_id {
            return Err(bad("that agent is in another session"));
        }
    }
    if exited.is_some() {
        return Err(bad("that agent has exited; read its transcript instead"));
    }
    let Some(node_id) = node_id else {
        return Err(bad("that agent has no machine: an external participant has no terminal"));
    };
    if !st.node_is_connected(&node_id) {
        return Err(bad("the agent's node is not connected, so it cannot be told about this"));
    }
    let node_name: String = {
        let db = st.db.lock().unwrap();
        db.query_row("SELECT name FROM nodes WHERE id = ?", [&node_id], |r| r.get(0))
            .optional()
            .map_err(bad)?
            .unwrap_or_default()
    };
    let ticket = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms() + nexmesh_proto::attachsock::TICKET_TTL_MS;
    st.send_to_node(
        &node_id,
        &nexmesh_proto::HubToNode::AttachTicket {
            ticket: ticket.clone(),
            agent_id: req.agent.clone(),
            expires_at_ms,
            operator: who,
        },
    )
    .await
    .map_err(bad)?;
    Ok(Json(json!({
        "ticket": ticket, "node": node_name, "agent": req.agent,
        "expires_at_ms": expires_at_ms,
    })))
}

#[derive(Deserialize)]
pub struct SessionForPath {
    /// Node name, as `nexmesh nodes` lists it.
    pub node: String,
    /// An absolute path on that node.
    pub path: String,
}

/// Which session owns a directory on a machine.
///
/// The same question an arriving CLI asks of itself -- a workspace mount is
/// how a conversation finds the team it belongs to -- asked from outside, so
/// `nexmesh run` in a directory can join the team that owns it rather than
/// making the operator name a session they already decided by cd'ing.
///
/// A directory two sessions both mount is not answered: a path says where
/// files are, not who you work with, and guessing between two teams is worse
/// than asking.
pub async fn session_for_path(
    State(st): State<AppState>,
    Query(q): Query<SessionForPath>,
) -> ApiResult {
    if !q.path.starts_with('/') {
        return Err(bad("path must be absolute: a relative path means nothing on a machine \
                        you are not standing on"));
    }
    let db = st.db.lock().unwrap();
    let node_id: Option<String> = db
        .query_row("SELECT id FROM nodes WHERE name = ?", [&q.node], |r| r.get(0))
        .optional()
        .map_err(bad)?;
    let node_id = node_id.ok_or_else(|| bad(format!("no node named '{}'", q.node)))?;
    let session = crate::nodes::session_for_cwd(&db, &node_id, &q.path).map_err(bad)?;
    let Some(session_id) = session else {
        return Ok(Json(json!({ "session_id": Value::Null })));
    };
    let name: String = db
        .query_row("SELECT name FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .unwrap_or_default();
    // The mount that actually contains it, which is the workspace a seat
    // working in this directory belongs to.
    let ws: Option<(String, String)> = db
        .prepare(
            "SELECT w.id, w.name FROM workspace_mounts m
               JOIN workspaces w ON w.id = m.workspace_id
              WHERE m.node_id = ?1 AND w.session_id = ?2
                AND (?3 = m.path OR ?3 LIKE m.path || '/%')
              ORDER BY LENGTH(m.path) DESC LIMIT 1",
        )
        .and_then(|mut q2| {
            q2.query_row(params![node_id, session_id, q.path.trim_end_matches('/')], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })
        .optional()
        .map_err(bad)?;
    Ok(Json(json!({
        "session_id": session_id, "session": name,
        "workspace_id": ws.as_ref().map(|w| w.0.clone()),
        "workspace": ws.map(|w| w.1),
    })))
}

#[derive(Deserialize)]
pub struct OperatorMessage {
    pub to: Option<String>,
    pub text: String,
}

/// The operator speaking to the team. `from_slot` is NULL, which every
/// transport renders as "orchestrator".
pub async fn operator_message(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<OperatorMessage>,
) -> ApiResult {
    // A session may be named, not only identified.
    let session_id = session_ref_to_id(&st.db.lock().unwrap(), &session_ref);
    if req.text.trim().is_empty() {
        return Err(bad("text must not be empty"));
    }
    // A line beginning with `/` is an instruction, not an utterance. Done
    // here rather than in each front end, so the console, the dashboard and
    // the phone all got it at once.
    let text = match crate::chatcmd::parse(&req.text).map_err(bad)? {
        crate::chatcmd::Parsed::Say(text) => text,
        crate::chatcmd::Parsed::Do(cmd) => {
            let said = run_command(&st, &session_id, &cmd).await?;
            // The outcome goes in the chat as a record: everyone watching
            // sees what was done, and it is pre-claimed so no agent is ever
            // handed the operator's housekeeping as a message to act on.
            let line = format!("{} \u{2192} {said}", req.text.trim());
            let db = st.db.lock().unwrap();
            let now = now_ms();
            db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body,
                                       sent_at_ms, claimed_at_ms, claimed_by)
                 VALUES (?, NULL, NULL, 'system', ?, ?, ?, 'console')",
                params![session_id, line, now, now],
            )
            .map_err(bad)?;
            return Ok(Json(json!({ "sent": true, "command": true, "said": said })));
        }
    };
    let db = st.db.lock().unwrap();
    let to_slot: Option<String> = match &req.to {
        Some(t) => Some(
            db.query_row(
                "SELECT id FROM slots WHERE session_id = ?1
                   AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                        OR LOWER(COALESCE(role,'')) = LOWER(?2))",
                params![session_id, t],
                |r| r.get(0),
            )
            .optional()
            .map_err(bad)?
            .ok_or_else(|| bad(format!("no slot matching '{t}'")))?,
        ),
        None => None,
    };
    db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, ?, 'system', ?, ?)",
        params![session_id, to_slot, text, now_ms()],
    )
    .map_err(bad)?;
    // Who it is for, and by what route it can reach them.
    //
    // "Sent" is not "heard". A seat the hub can drive, type at, or that polls
    // the hub itself gets a message in seconds. A CLI somebody started by
    // hand in a plain terminal has none of those: nothing can interrupt it,
    // and it will see this when it next calls a tool -- which, sitting at a
    // prompt with nothing to do, may be never. A question asked into a room
    // of those reads as ignored, and the operator had no way to tell. Now
    // the answer comes back with the send.
    let seats: Vec<Value> = db
        .prepare(&format!(
            "SELECT COALESCE(s.display_name, s.role, s.id),
                    CASE WHEN a.provenance = 'external' THEN 'polls'
                         WHEN a.push_capable = 1 THEN 'driven'
                         WHEN s.terminal_via IS NOT NULL THEN 'terminal'
                         ELSE 'next tool call' END,
                    {word}
               FROM slots s
               LEFT JOIN agents a ON a.id = (
                   SELECT id FROM agents WHERE slot_id = s.id
                   ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
              WHERE s.session_id = ?1 AND (?2 IS NULL OR s.id = ?2)
                AND COALESCE(s.task_state,'') <> 'released'
                AND a.id IS NOT NULL
                AND (a.exited_at_ms IS NULL OR ({waiting}))
              ORDER BY 1",
            word = crate::seat::agent_word("s", "a"),
            waiting = crate::seat::waiting("s", "a"),
        ))
        .and_then(|mut q| {
            q.query_map(params![session_id, to_slot], |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "route": r.get::<_, String>(1)?,
                    "status": r.get::<_, String>(2)?,
                }))
            })
            .and_then(|m| m.collect())
        })
        .unwrap_or_default();
    Ok(Json(json!({ "sent": true, "to": to_slot, "seats": seats })))
}

/// Carry out a chat command.
///
/// Every one of these is an existing endpoint, called as the handler it
/// already is: `/move` is `POST /v1/slots/:id/move`, `/remove` is the DELETE,
/// and so on. Nothing here reimplements an operation, so a command typed in
/// the chat and the same thing done with `nexmesh move-agent` take the same
/// path, including every refusal.
async fn run_command(
    st: &AppState,
    session_id: &str,
    cmd: &crate::chatcmd::Cmd,
) -> std::result::Result<String, (StatusCode, Json<Value>)> {
    use crate::chatcmd::Cmd;
    match cmd {
        Cmd::Help => Ok(crate::chatcmd::help()),
        Cmd::Rename { who, name } => {
            let (slot, was) = seat_by_ref(st, session_id, who)?;
            let v = rename_slot(
                State(st.clone()),
                Path(slot),
                Json(RenameRequest { name: name.clone() }),
            )
            .await?;
            let _ = v;
            Ok(format!("{was} is now called {name}"))
        }
        Cmd::Move { who, session } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let Json(v) = move_slot(
                State(st.clone()),
                Path(slot),
                Json(MoveSlot { session: session.clone() }),
            )
            .await?;
            let left = v["messages_left_unread"].as_i64().unwrap_or(0);
            let mut out = format!(
                "{name} is now in {}",
                v["session_name"].as_str().unwrap_or(session)
            );
            if left > 0 {
                out.push_str(&format!("; {left} unread message(s) stayed behind"));
            }
            Ok(out)
        }
        Cmd::Remove { who, force } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let mut q = std::collections::HashMap::new();
            if *force {
                q.insert("force".to_string(), "1".to_string());
            }
            let Json(v) =
                remove_slot(State(st.clone()), Path(slot), axum::extract::Query(q)).await?;
            Ok(format!(
                "{name} is gone ({} agent row(s) with it)",
                v["agent_rows"].as_i64().unwrap_or(0)
            ))
        }
        Cmd::Start { who, mode } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let Json(v) = spawn_slot(
                State(st.clone()),
                Path(slot),
                Some(Json(SpawnRequest { cli: None, mode: mode.clone() })),
            )
            .await?;
            Ok(format!(
                "{name} is starting on {} ({})",
                v["node"].as_str().unwrap_or("?"),
                v["mode"].as_str().unwrap_or("?")
            ))
        }
        Cmd::Owns { who, patterns, add } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let Json(v) = set_owns(
                State(st.clone()),
                Path(slot),
                Json(OwnsRequest { patterns: patterns.clone(), add: *add }),
            )
            .await?;
            let owns: Vec<&str> =
                v["owns"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
            Ok(format!(
                "{name} owns {}",
                if owns.is_empty() { "nothing".to_string() } else { owns.join(", ") }
            ))
        }
        Cmd::New { cli, name, mode } => {
            let Json(made) = create_slot(
                State(st.clone()),
                Path(session_id.to_string()),
                Json(NewSlot {
                    cli_id: Some(cli.clone()),
                    participant_kind: None,
                    // A seat made from a chat is one somebody means to talk
                    // to unless they said otherwise.
                    mode: Some(mode.clone().unwrap_or_else(|| "interactive".into())),
                    display_name: Some(name.clone()),
                    role: None,
                    role_description: None,
                    workspace: None,
                    task: None,
                    model: None,
                    capabilities: vec![],
                    // Wherever the CLI is actually installed.
                    placement: nexmesh_proto::Placement { node: None, labels: vec![], any: true },
                    file_ownership: vec![],
                }),
            )
            .await?;
            let slot = made["slot_id"].as_str().unwrap_or("").to_string();
            let Json(v) = spawn_slot(
                State(st.clone()),
                Path(slot.clone()),
                Some(Json(SpawnRequest {
                    cli: None,
                    mode: Some(mode.clone().unwrap_or_else(|| "interactive".into())),
                })),
            )
            .await?;
            Ok(format!(
                "{name} ({slot}) is starting on {} ({})",
                v["node"].as_str().unwrap_or("?"),
                v["mode"].as_str().unwrap_or("?")
            ))
        }
    }
}

/// A seat in this session, by name, role or id, with the name it goes by.
///
/// The same resolution the chat's `to:` already uses, and the same answer an
/// operator reads on the dashboard -- a person types "Dev", not "sl-4f1c".
fn seat_by_ref(
    st: &AppState,
    session_id: &str,
    who: &str,
) -> std::result::Result<(String, String), (StatusCode, Json<Value>)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT id, COALESCE(display_name, role, id) FROM slots
          WHERE session_id = ?1
            AND (id = ?2 OR LOWER(COALESCE(display_name,'')) = LOWER(?2)
                 OR LOWER(COALESCE(role,'')) = LOWER(?2))",
        params![session_id, who],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
    .map_err(bad)?
    .ok_or_else(|| {
        // What there is, rather than only what there is not: a name typed
        // from memory is usually nearly right.
        let names: Vec<String> = db
            .prepare("SELECT COALESCE(display_name, role, id) FROM slots WHERE session_id = ?
                      ORDER BY 1")
            .and_then(|mut q| {
                q.query_map([session_id], |r| r.get::<_, String>(0))
                    .and_then(|m| m.collect())
            })
            .unwrap_or_default();
        bad(format!(
            "no agent called '{who}' here. There is: {}",
            if names.is_empty() { "nobody".to_string() } else { names.join(", ") }
        ))
    })
}

// ---------------------------------------------------------- participant ----

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.trim().to_string())
}

/// Authenticate a participant token and return the session it is scoped to, if
/// any. A participant token can call team tools for its own slot and nothing
/// else -- it can never cause a process to be spawned on anyone's machine.
pub(crate) fn auth_participant(st: &AppState, headers: &HeaderMap) -> std::result::Result<Option<String>, (StatusCode, Json<Value>)> {
    let token = bearer(headers).ok_or_else(|| {
        err(
            StatusCode::UNAUTHORIZED,
            "missing participant bearer token; mint one with \
             `nexmesh-hub participant-token <label>` (an operator token will not work here)",
        )
    })?;
    let db = st.db.lock().unwrap();
    let row: Option<Option<String>> = db
        .query_row(
            "SELECT session_id FROM participant_tokens
             WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
            [crate::state::sha256_hex(&token)],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    row.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid or revoked participant token"))
}

#[derive(Deserialize)]
pub struct ParticipantJoin {
    pub session: Option<String>,
    /// Several sessions at once, for a participant that serves more than one
    /// team -- a capability bridge, typically, since OCR is OCR whoever is
    /// asking. `["*"]` or `["all"]` means every active session this token may
    /// join.
    ///
    /// A CLI cannot do this: one conversation is one seat, enforced when it
    /// registers. An external participant has no conversation and no process
    /// the hub can see, so nothing is ambiguous about it holding a seat in
    /// each of several teams -- each seat has its own inbox, its own asks and
    /// its own task state, which is exactly what a bridge wants.
    #[serde(default)]
    pub sessions: Option<Vec<String>>,
    pub name: String,
    pub role: Option<String>,
    pub role_description: Option<String>,
    /// Free-form note about what this is: "hermes", "python", ...
    pub kind_hint: Option<String>,
    pub workspace: Option<String>,
    /// Capability labels this participant offers, so teammates can address
    /// what it can do rather than its name. A bridge to an external service
    /// discovers these at startup and declares them here.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

pub async fn participant_join(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ParticipantJoin>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    let Some(asked) = req.sessions.take().filter(|v| !v.is_empty()) else {
        return Ok(Json(join_participant(&st, req, scoped)?));
    };

    // Which sessions. A scoped token gets its own and no more, whatever it
    // asked for -- the same rule as a single join, applied once rather than
    // per name so the refusal is not a surprise half way through.
    let wanted: Vec<String> = if let Some(sc) = &scoped {
        vec![sc.clone()]
    } else if asked.iter().any(|s| s == "*" || s.eq_ignore_ascii_case("all")) {
        let db = st.db.lock().unwrap();
        db.prepare("SELECT id FROM sessions WHERE status = 'active' ORDER BY created_at_ms")
            .and_then(|mut q| q.query_map([], |r| r.get::<_, String>(0)).and_then(|m| m.collect()))
            .map_err(bad)?
    } else {
        asked
    };
    if wanted.is_empty() {
        return Err(bad("no active session to join"));
    }

    // Seat it in each, and say which ones did not work rather than failing
    // the lot: a bridge that serves four teams should not be kept out of all
    // four because one session name was mistyped.
    let mut seats: Vec<Value> = Vec::new();
    let mut refused: Vec<Value> = Vec::new();
    let mut tools: Option<Value> = None;
    for sid in &wanted {
        let one = ParticipantJoin {
            session: Some(sid.clone()),
            sessions: None,
            name: req.name.clone(),
            role: req.role.clone(),
            role_description: req.role_description.clone(),
            kind_hint: req.kind_hint.clone(),
            workspace: req.workspace.clone(),
            capabilities: req.capabilities.clone(),
        };
        match join_participant(&st, one, scoped.clone()) {
            Ok(v) => {
                if tools.is_none() {
                    tools = v.get("tools").cloned();
                }
                seats.push(json!({
                    "session_id": v.get("session_id").cloned().unwrap_or(Value::Null),
                    "slot_id": v.get("slot_id").cloned().unwrap_or(Value::Null),
                    "agent_id": v.get("agent_id").cloned().unwrap_or(Value::Null),
                    "session": v.get("session").cloned().unwrap_or(Value::Null),
                }));
            }
            Err((_, Json(e))) => refused.push(json!({
                "session": sid,
                "error": e.get("error").cloned().unwrap_or(Value::Null),
            })),
        }
    }
    if seats.is_empty() {
        return Err(bad(format!(
            "could not join any of those sessions: {}",
            serde_json::to_string(&refused).unwrap_or_default()
        )));
    }
    // The first seat's fields are repeated at the top level, so a client
    // written against the single-session shape still works.
    let first = seats[0].clone();
    Ok(Json(json!({
        "agent_id": first["agent_id"], "slot_id": first["slot_id"],
        "session_id": first["session_id"],
        "tools": tools.unwrap_or(Value::Null),
        "seats": seats,
        "refused": refused,
    })))
}

/// Seat an external participant: a slot, an agent, and the tool set it may
/// call.
///
/// Shared with the MCP front door, which has to do exactly this when a client
/// sends `initialize`. One implementation, so the two doors cannot drift into
/// seating participants differently.
pub(crate) fn join_participant(
    st: &AppState,
    req: ParticipantJoin,
    scoped: Option<String>,
) -> std::result::Result<Value, (StatusCode, Json<Value>)> {
    if req.name.trim().is_empty() {
        return Err(bad("name must not be empty"));
    }
    let db = st.db.lock().unwrap();
    let wanted = req.session.or(scoped.clone());
    let session_id: String = match wanted {
        Some(s) => {
            // A token scoped to one session may not join another.
            if let Some(sc) = &scoped {
                let matches: Option<i64> = db
                    .query_row(
                        "SELECT 1 FROM sessions WHERE id = ?1 AND (id = ?2 OR name = ?2)",
                        params![sc, s],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(bad)?;
                if matches.is_none() {
                    return Err(err(StatusCode::FORBIDDEN, "token is scoped to another session"));
                }
                sc.clone()
            } else {
                db.query_row(
                    "SELECT id FROM sessions WHERE id = ?1 OR name = ?1",
                    [&s],
                    |r| r.get(0),
                )
                .optional()
                .map_err(bad)?
                .ok_or_else(|| bad(format!("no session '{s}'")))?
            }
        }
        None => {
            let ids: Vec<String> = db
                .prepare("SELECT id FROM sessions WHERE status = 'active'")
                .and_then(|mut s| s.query_map([], |r| r.get(0)).and_then(|m| m.collect()))
                .map_err(bad)?;
            match ids.len() {
                1 => ids[0].clone(),
                0 => return Err(bad("no active session to join")),
                k => return Err(bad(format!("{k} active sessions; name the one to join"))),
            }
        }
    };

    let ws_id: Option<String> = match &req.workspace {
        Some(w) => db
            .query_row(
                "SELECT id FROM workspaces WHERE session_id = ? AND (name = ? OR id = ?)",
                params![session_id, w, w],
                |r| r.get(0),
            )
            .optional()
            .map_err(bad)?,
        None => None,
    };

    let now = now_ms();
    let caps = serde_json::to_string(&normalise_capabilities(&req.capabilities))
        .unwrap_or_else(|_| "[]".into());

    // One name, one seat.
    //
    // An external participant is identified by its NAME: it has no
    // conversation id and no process the hub can see. This first looked only
    // for a seat whose agent was already GONE, so that a restart would
    // reclaim it while a concurrent second instance got its own -- and that
    // test can never work here. A service restarts in a second or two; the
    // lease deciding whether the old agent is live runs for twenty. The old
    // agent therefore still looked alive every single time, and every
    // restart of the capability bridge left another derelict CoderAI behind
    // -- three times in one afternoon, each cleared by hand.
    //
    // Two instances under one name are indistinguishable from a restart
    // anyway, so the name wins: the seat is taken and whoever was in it is
    // marked gone. An instance that wants its own seat asks for its own name.
    let existing: Option<String> = db
        .query_row(
            "SELECT s.id FROM slots s
              WHERE s.session_id = ?1 AND s.participant_kind = 'external'
                AND COALESCE(s.display_name, '') = ?2
              ORDER BY s.created_at_ms DESC LIMIT 1",
            params![session_id, req.name],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?;

    let slot_id = match existing {
        Some(id) => {
            // Whoever was in this seat is not any more: this is the same
            // participant, come back.
            db.execute(
                "UPDATE agents SET status = 'exited', exited_at_ms = COALESCE(exited_at_ms, ?)
                  WHERE slot_id = ? AND exited_at_ms IS NULL",
                params![now, id],
            )
            .map_err(bad)?;
            // What it can do may have changed since last time: a bridge
            // probes its service at startup, and a service gains and loses
            // endpoints.
            db.execute(
                "UPDATE slots SET capabilities = ?, role = COALESCE(?, role),
                                  role_description = COALESCE(?, role_description),
                                  workspace_id = COALESCE(?, workspace_id),
                                  status = 'connected'
                  WHERE id = ?",
                params![caps, req.role, req.role_description, ws_id, id],
            )
            .map_err(bad)?;
            tracing::info!(slot_id = %id, name = %req.name,
                "an external participant returned to its seat");
            id
        }
        None => {
            let id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            db.execute(
                "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                                    display_name, role, role_description, capabilities, status,
                                    created_at_ms)
                 VALUES (?,?,?, NULL, 'external', 'headless', ?,?,?,?, 'connected', ?)",
                params![
                    id, session_id, ws_id, req.name, req.role, req.role_description, caps, now
                ],
            )
            .map_err(bad)?;
            id
        }
    };
    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO agents (id, slot_id, node_id, provenance, status, kind_hint,
                             lease_expires_ms, started_at_ms, last_seen_ms)
         VALUES (?,?, NULL, 'external', 'running', ?, ?, ?, ?)",
        params![agent_id, slot_id, req.kind_hint, now + proto::NODE_LEASE_MS as i64, now, now],
    )
    .map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?, NULL, 'participant_joined', ?)",
        params![now, req.name, json!({ "slot": slot_id, "agent": agent_id }).to_string()],
    )
    .ok();
    tracing::info!(%slot_id, %agent_id, name = %req.name, "external participant joined");

    Ok(json!({
        "agent_id": agent_id,
        "slot_id": slot_id,
        "session_id": session_id,
        "heartbeat_ms": proto::HEARTBEAT_INTERVAL_MS,
        "tools": proto::tools::team_tools(),
    }))
}

#[derive(Deserialize)]
pub struct ParticipantTool {
    pub agent_id: String,
    pub tool: String,
    #[serde(default)]
    pub args: Value,
}

/// Same dispatcher as every CLI agent uses, reached over HTTP. Pending
/// messages ride back with the result exactly as they do for a CLI, and are
/// acked separately so nothing is lost if the participant dies mid-call.
pub async fn participant_tool(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ParticipantTool>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    verify_agent_scope(&st, &req.agent_id, &scoped)?;
    touch_participant(&st, &req.agent_id);
    match crate::nodes::run_tool_async(&st, &req.agent_id, &req.tool, &req.args).await {
        Ok((text, deliver)) => Ok(Json(json!({ "ok": true, "text": text, "messages": deliver }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e.to_string() }))),
    }
}

#[derive(Deserialize)]
pub struct ParticipantAck {
    pub agent_id: String,
    pub message_ids: Vec<i64>,
}

pub async fn participant_ack(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ParticipantAck>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    verify_agent_scope(&st, &req.agent_id, &scoped)?;
    let db = st.db.lock().unwrap();
    tools::ack_messages(&db, &req.agent_id, &req.message_ids).map_err(bad)?;
    Ok(Json(json!({ "acked": req.message_ids.len() })))
}

#[derive(Deserialize)]
pub struct AgentQuery {
    pub agent_id: String,
}

/// Fetch pending messages without calling a tool. Marks them in flight; the
/// caller must ack. An external agent polls this on whatever cadence suits it.
pub async fn participant_messages(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AgentQuery>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    verify_agent_scope(&st, &q.agent_id, &scoped)?;
    touch_participant(&st, &q.agent_id);
    let db = st.db.lock().unwrap();
    let (slot_id, session_id): (String, String) = db
        .query_row(
            "SELECT s.id, s.session_id FROM agents a JOIN slots s ON s.id = a.slot_id WHERE a.id = ?",
            [&q.agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad("unknown agent"))?;
    let pending = tools::pending_for_slot(&db, &session_id, &slot_id).map_err(bad)?;
    let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
    tools::mark_inflight(&db, &q.agent_id, &slot_id, &ids).map_err(bad)?;
    Ok(Json(json!({ "messages": pending })))
}

pub async fn participant_heartbeat(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<AgentQuery>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    verify_agent_scope(&st, &q.agent_id, &scoped)?;
    touch_participant(&st, &q.agent_id);
    Ok(Json(json!({ "ok": true })))
}

/// An external agent has no process we can inspect, so its liveness is a lease
/// it must keep renewing -- the same mechanism as a node, for the same reason.
pub(crate) fn touch_participant(st: &AppState, agent_id: &str) {
    let db = st.db.lock().unwrap();
    let now = now_ms();
    let _ = db.execute(
        "UPDATE agents SET last_seen_ms = ?, lease_expires_ms = ?,
            status = CASE WHEN status = 'disconnected' THEN 'running' ELSE status END
         WHERE id = ?",
        params![now, now + proto::NODE_LEASE_MS as i64, agent_id],
    );
}

/// A session-scoped token must not act for an agent in another session.
pub(crate) fn verify_agent_scope(
    st: &AppState,
    agent_id: &str,
    scoped: &Option<String>,
) -> std::result::Result<(), (StatusCode, Json<Value>)> {
    let db = st.db.lock().unwrap();
    let session: Option<String> = db
        .query_row(
            "SELECT s.session_id FROM agents a JOIN slots s ON s.id = a.slot_id WHERE a.id = ?",
            [agent_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let session = session.ok_or_else(|| bad("unknown agent"))?;
    if let Some(sc) = scoped {
        if sc != &session {
            return Err(err(StatusCode::FORBIDDEN, "token is scoped to another session"));
        }
    }
    Ok(())
}
