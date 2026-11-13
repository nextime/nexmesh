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
/// digesta` therefore matched no rows and reported "no agents in this
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
              WHERE slot_id = ? AND exited_at_ms IS NULL AND status <> 'lost'
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
/// knows what it will do -- `Claude-zeiss-3` says where it runs and nothing
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
        "SELECT s.id, s.name, s.status, s.created_at_ms,
                (SELECT COUNT(*) FROM slots WHERE session_id = s.id)
         FROM sessions s WHERE (?1 IS NULL OR s.id = ?1) ORDER BY s.created_at_ms DESC",
    ) else {
        return Json(json!({ "sessions": [] }));
    };
    let rows: Vec<Value> = stmt
        .query_map([&scope], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                "status": r.get::<_, String>(2)?, "created_at_ms": r.get::<_, i64>(3)?,
                "slots": r.get::<_, i64>(4)?,
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
    match crate::launch::launch_slot(&st, &slot_id, None).await {
        Ok((agent_id, node)) => Ok(Json(json!({
            "slot_id": slot_id, "agent_id": agent_id, "node": node,
            "cli": req.cli,
        }))),
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
    Path(session_id): Path<String>,
    Json(req): Json<AdoptRequest>,
) -> ApiResult {
    if req.tmux_target.trim().is_empty() {
        return Err(bad("tmux_target must not be empty"));
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
                                AND exited_at_ms IS NULL AND status <> 'lost')",
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
                                AND a.status <> 'lost'
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
    let Ok(mut stmt) = db.prepare(
        "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.participant_kind, COALESCE(s.cli_id, '-'), s.task_state,
                s.summary, COALESCE(a.provenance, '-'), COALESCE(a.status, 'disconnected'),
                COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external' ELSE '-' END),
                COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0), s.paused
         FROM slots s
         LEFT JOIN agents a ON a.id = (
             SELECT id FROM agents WHERE slot_id = s.id
             ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE s.session_id = ? ORDER BY 2",
    ) else {
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
    let Ok(mut stmt) = db.prepare(
        "SELECT m.id, m.sent_at_ms, m.kind, m.body,
                COALESCE(sf.display_name, sf.role, 'operator'),
                CASE WHEN m.to_slot IS NULL THEN '(team)'
                     ELSE COALESCE(st2.display_name, st2.role, m.to_slot) END,
                m.acked_at_ms IS NOT NULL,
                m.inflight_at_ms IS NOT NULL
         FROM messages m
         LEFT JOIN slots sf ON sf.id = m.from_slot
         LEFT JOIN slots st2 ON st2.id = m.to_slot
         WHERE m.session_id = ?1 AND m.id > ?2
         ORDER BY m.id ASC LIMIT ?3",
    ) else {
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
    Path((session_id, who)): Path<(String, String)>,
) -> ApiResult {
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
    let (agent_id, name) = found.ok_or_else(|| bad(format!("no agent matching '{who}'")))?;
    Ok(Json(json!({ "agent_id": agent_id, "name": name })))
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
        params![session_id, to_slot, req.text, now_ms()],
    )
    .map_err(bad)?;
    Ok(Json(json!({ "sent": true, "to": to_slot })))
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
    Json(req): Json<ParticipantJoin>,
) -> ApiResult {
    let scoped = auth_participant(&st, &headers)?;
    Ok(Json(join_participant(&st, req, scoped)?))
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
    tools::mark_inflight(&db, &q.agent_id, &ids).map_err(bad)?;
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
            status = CASE WHEN status = 'lost' THEN 'running' ELSE status END
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
