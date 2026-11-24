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
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
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
pub struct AuthorityRequest {
    /// agent | orchestrator | super. Anything else is refused rather than
    /// quietly read as 'agent': setting authority is deliberate, and a typo
    /// that silently demoted a seat would be found much later.
    pub authority: String,
}

#[derive(serde::Deserialize)]
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

#[derive(serde::Deserialize)]
pub struct DecideRequest {
    /// An option key the prompt offered, or `allow` / `deny` for "whichever
    /// of its options does that".
    pub choice: String,
    /// Also turn on a standing auto-approval: `agent` or `session`. This is
    /// how "allow, and stop asking me about this one" is a single click.
    #[serde(default)]
    pub remember: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct AutoApproveRequest {
    /// Defaults to ON: `POST .../auto-approve` with no body turns it on, and
    /// turning it off is the deliberate `{"on": false}`.
    #[serde(default = "yes")]
    pub on: bool,
}

/// What is waiting for the operator to decide.
///
/// Fleet-wide by default so that nothing can be stopped on a question the
/// operator never saw because they were looking at another team.
pub async fn list_approvals(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let db = st.db.lock().unwrap();
    let session = q.get("session").map(|s| session_ref_to_id(&db, s));
    let rows = crate::approvals::pending(&db, session.as_deref());
    Json(json!({ "approvals": rows }))
}

/// Answer one.
pub async fn decide_approval(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DecideRequest>,
) -> ApiResult {
    // The node id is read before the decision so that a failure to reach the
    // node is reported WITH the decision that was recorded: the operator has
    // answered, and a node that is offline will not have typed it.
    let node = {
        let db = st.db.lock().unwrap();
        db.query_row("SELECT node_id FROM approvals WHERE id = ?", [&id],
                     |r| r.get::<_, Option<String>>(0))
            .optional()
            .ok()
            .flatten()
            .flatten()
    };
    let (frame, said) =
        crate::approvals::decide(&st, &id, req.choice.trim(), req.remember.as_deref())
            .map_err(bad)?;
    let mut typed = false;
    if let (Some(frame), Some(node)) = (frame, node) {
        typed = st.send_to_node(&node, &frame).await.is_ok();
    }
    Ok(Json(json!({
        "ok": true, "said": said,
        // Whether the keystroke actually went out. A decision recorded
        // against an offline node is not an answered prompt, and saying so
        // is the difference between "done" and "the agent is still stuck".
        "typed": typed,
    })))
}

#[derive(serde::Deserialize)]
pub struct ResumeHandleRequest {
    /// The CLI's own id for the conversation this seat should continue.
    /// Empty or absent clears it, so the next spawn starts fresh.
    #[serde(default)]
    pub handle: Option<String>,
}

/// Pin which of its CLI's conversations a seat continues.
///
/// Normally the node reports this itself -- it named the conversation, or
/// learned the id over the CLI's own protocol. This is the operator's door,
/// for the cases the node cannot work out: moving an existing conversation
/// into a seat's new store, or pointing a seat at a conversation that was
/// started by hand.
///
/// It is NOT validated against the CLI, because the hub cannot see the CLI's
/// store -- a wrong id means the next spawn starts fresh, which is why the
/// previous value is reported back.
pub async fn set_resume_handle(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ResumeHandleRequest>,
) -> ApiResult {
    let want = req.handle.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let db = st.db.lock().unwrap();
    let was: Option<Option<String>> = db
        .query_row("SELECT resume_handle FROM slots WHERE id = ?", [&id], |r| r.get(0))
        .optional()
        .map_err(bad)?;
    let Some(was) = was else {
        return Err(bad(format!("no seat '{id}'")));
    };
    db.execute("UPDATE slots SET resume_handle = ? WHERE id = ?", params![want, id])
        .map_err(bad)?;
    // AND the binding for where this seat actually is.
    //
    // A spawn reads `slot_conversations` for the machine it is launching on
    // and only falls back to `slots.resume_handle` for a seat with no
    // bindings at all. So setting the handle alone works until the node
    // reports a binding and then silently stops working -- a pin that does
    // nothing is worse than no pin. Clearing the handle clears the binding
    // for that place too, for the same reason.
    let place: Option<(String, String, String)> = db
        .query_row(
            "SELECT a.node_id, COALESCE(a.cwd, ''), COALESCE(s.cli_id, '')
               FROM slots s
               JOIN agents a ON a.id = (
                 SELECT id FROM agents WHERE slot_id = s.id AND node_id IS NOT NULL
                  ORDER BY started_at_ms DESC LIMIT 1)
              WHERE s.id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(bad)?;
    let mut bound_at = None;
    if let Some((node, cwd, cli)) = place {
        match want {
            Some(h) => {
                db.execute(
                    "INSERT INTO slot_conversations
                        (slot_id, node_id, cwd, cli_id, handle, updated_at_ms)
                     VALUES (?,?,?,?,?,?)
                     ON CONFLICT(slot_id, node_id, cwd, cli_id) DO UPDATE SET
                        handle = excluded.handle, updated_at_ms = excluded.updated_at_ms",
                    params![id, node, cwd, cli, h, now_ms()],
                )
                .map_err(bad)?;
            }
            None => {
                db.execute(
                    "DELETE FROM slot_conversations
                      WHERE slot_id = ? AND node_id = ? AND cwd = ? AND cli_id = ?",
                    params![id, node, cwd, cli],
                )
                .map_err(bad)?;
            }
        }
        bound_at = Some(json!({ "node": node, "cwd": cwd, "cli": cli }));
    }
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,'operator',NULL,'resume_handle',?)",
        params![now_ms(), json!({ "slot": id, "from": was, "to": want }).to_string()],
    )
    .ok();
    tracing::info!(slot = %id, from = ?was, to = ?want, "resume handle set by the operator");
    // `bound_at` says WHERE it was pinned, or null when the seat has never
    // run anywhere the hub knows about -- in which case only the fallback
    // handle was set, and the first spawn on any machine will use it.
    Ok(Json(json!({ "slot_id": id, "resume_handle": want, "was": was,
                    "bound_at": bound_at })))
}

/// Whether the hub brings this seat back by itself when its process exits.
///
/// A respawn PREPARES the seat -- the process is there and can be attached
/// to -- and never reattaches anybody's terminal: whose terminal that would
/// be is not the hub's to decide.
pub async fn slot_autorespawn(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AutoApproveRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let n = db
        .execute(
            "UPDATE slots SET autorespawn = ? WHERE id = ?",
            params![i64::from(req.on), id],
        )
        .map_err(bad)?;
    if n == 0 {
        return Err(bad(format!("no seat '{id}'")));
    }
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,'operator',NULL,'autorespawn',?)",
        params![now_ms(), json!({ "slot": id, "on": req.on }).to_string()],
    )
    .ok();
    Ok(Json(json!({ "slot_id": id, "autorespawn": req.on })))
}

/// What a seat tells its agent, over and above the CLI's own prompt.
///
/// Two separate things, because they answer different questions:
///
///  * `system_prompt` is WHO the agent in this seat is, and goes to the CLI
///    as a system prompt -- appended to the CLI's own, or replacing it.
///    It is set once and lasts: every agent that ever fills this seat gets
///    it, including a respawn at 4am that nobody watched.
///  * `instructions` is WHAT it should do on arrival, and goes in as the
///    first thing said to it. A new occupant reads it; the agent already
///    sitting there does not get it again.
///
/// A field left out is left alone. A field present and empty clears it --
/// which is how "stop doing that" is one call rather than a delete verb.
#[derive(serde::Deserialize)]
pub struct SeatPromptRequest {
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Use the template for this role as the system prompt, instead of
    /// writing one by hand. `GET /v1/roles` lists them.
    ///
    /// Rendered and STORED, not referenced: what a seat tells its agent is
    /// then visible in one place, editable afterwards, and does not change
    /// under a running team because somebody improved a template.
    #[serde(default)]
    pub template: Option<String>,
    /// Which skill fragments the brief carries, overriding the template's
    /// own list. An empty list means none; absent means the template's.
    ///
    /// Without `template`, this re-renders from the template the seat
    /// already has -- which is how a whole selection is replaced.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Select these, leaving the rest of the seat's selection alone.
    #[serde(default)]
    pub skills_on: Option<Vec<String>>,
    /// Deselect these.
    ///
    /// Separate from `skills` because a checkbox is a toggle: a surface that
    /// has to send the whole list to turn one thing off races every other
    /// surface looking at the same seat, and loses whatever it had not
    /// loaded.
    #[serde(default)]
    pub skills_off: Option<Vec<String>>,
    /// `append` (the default) or `replace`. Replacing throws away the CLI's
    /// own system prompt, which is what you want for a seat that is not
    /// meant to behave like that CLI at all, and a trap otherwise.
    #[serde(default)]
    pub system_prompt_mode: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// A system prompt is handed to the CLI as one argv entry, and Linux caps a
/// single argument at 128 KiB (MAX_ARG_STRLEN). Refusing at 64 KiB leaves
/// room for the rest of the command line and fails here, with a sentence,
/// rather than as E2BIG inside a spawn nobody is watching.
const PROMPT_MAX: usize = 64 * 1024;

fn prompt_field(
    name: &str,
    given: Option<String>,
    current: Option<String>,
) -> std::result::Result<(Option<String>, bool), (StatusCode, Json<Value>)> {
    let Some(text) = given else {
        return Ok((current, false));
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok((None, current.is_some()));
    }
    if text.len() > PROMPT_MAX {
        return Err(bad(format!(
            "that {name} is {} KiB; the limit is {} KiB, because it is handed \
             to the CLI as a single argument",
            text.len() / 1024,
            PROMPT_MAX / 1024
        )));
    }
    let changed = current.as_deref() != Some(text);
    Ok((Some(text.to_string()), changed))
}

/// One team, in detail: what it is, what it is set to, and its rules.
///
/// There was no such endpoint -- only a listing and a delete -- so anything
/// wanting a single team's settings had to find it in the list. The settings
/// that now live on a session (a budget, a compaction threshold, a loop
/// guard, the standing rules) need somewhere to be read from.
pub async fn session_detail(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let id = session_ref_to_id(&db, &session_ref);
    let row: Option<(String, String, String, Option<i64>, Option<i64>, Option<i64>,
                     Option<i64>, Option<String>, i64)> = db
        .query_row(
            "SELECT id, name, status, token_budget, budget_hit_at_ms,
                    compact_at_pct, loop_guard_repeats, instructions, auto_approve
               FROM sessions WHERE id = ?",
            [&id],
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?,
                ))
            },
        )
        .optional()
        .map_err(bad)?;
    let Some((id, name, status, budget, hit, pct, repeats, rules, auto)) = row else {
        return Err(bad(format!("no team '{session_ref}'")));
    };
    Ok(Json(json!({
        "session_id": id, "name": name, "status": status,
        "token_budget": budget, "budget_hit_at_ms": hit,
        // NULL means "inherit": a team that has set neither follows the
        // fleet. 0 is deliberately off. See `watchdog::setting`.
        "compact_at_pct": pct,
        "loop_guard_repeats": repeats,
        "auto_approve": auto == 1,
        "instructions": rules,
    })))
}

/// Set or clear a team's standing rules.
///
/// The operator's way in. A seat holding orchestrator authority does the
/// same thing through the `set_team_rules` tool, which shares the rule about
/// who may: nobody else.
pub async fn set_session_instructions(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Json(req): Json<SessionRulesRequest>,
) -> ApiResult {
    let text = req.instructions.unwrap_or_default();
    let text = text.trim().to_string();
    if text.len() > TEAM_RULES_MAX {
        return Err(bad(format!(
            "that is {} KiB of rules; the limit is {} KiB. Rules nobody can hold in              their head are rules nobody follows -- keep these short and put the              detail in the team's knowledge.",
            text.len() / 1024,
            TEAM_RULES_MAX / 1024
        )));
    }
    let (id, changed) = {
        let db = st.db.lock().unwrap();
        let id = session_ref_to_id(&db, &session_ref);
        let before: Option<String> = db
            .query_row("SELECT instructions FROM sessions WHERE id = ?", [&id], |r| r.get(0))
            .optional()
            .map_err(bad)?
            .ok_or_else(|| bad(format!("no team '{session_ref}'")))?;
        let changed = before.as_deref().map(str::trim).unwrap_or("") != text;
        if changed {
            db.execute(
                "UPDATE sessions SET instructions = ? WHERE id = ?",
                params![if text.is_empty() { None } else { Some(&text) }, id],
            )
            .map_err(bad)?;
            db.execute(
                "INSERT INTO audit (at_ms, actor, node_id, action, detail)
                 VALUES (?,'operator',NULL,'team_rules_set',?)",
                params![now_ms(), json!({ "session": id, "bytes": text.len() }).to_string()],
            )
            .ok();
            // EVERYBODY ALREADY RUNNING IS TOLD. The rules reach a seat's
            // prompt only when its next process starts, which may be days;
            // without this the team works to the old ones and nothing
            // distinguishes the agents that know from the ones that do not.
            let said = if text.is_empty() {
                "The operator has cleared this team's standing rules.".to_string()
            } else {
                format!(
                    "The operator has set this team's standing rules. They apply to                      everybody here from now on, and outrank your own preferences about                      how to work -- `team_rules` reads them back at any                      time.\n\n{text}"
                )
            };
            db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
                 VALUES (?, NULL, NULL, 'system', ?, ?)",
                params![id, said, now_ms()],
            )
            .ok();
        }
        (id, changed)
    };
    tracing::info!(session = %id, bytes = text.len(), changed, "the team's standing rules");
    Ok(Json(json!({
        "session_id": id, "instructions": if text.is_empty() { None } else { Some(&text) },
        "changed": changed,
        "detail": if !changed {
            "those are already this team's rules".to_string()
        } else if text.is_empty() {
            "cleared; everybody here has been told".to_string()
        } else {
            format!("set, {} characters; everybody here has been told, and every seat \
                     started from now on gets them in its first prompt", text.len())
        },
    })))
}

#[derive(serde::Deserialize)]
pub struct SessionRulesRequest {
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Rules longer than this are not rules. Mirrors `tools::MAX_TEAM_RULES`.
const TEAM_RULES_MAX: usize = 8 * 1024;

/// The role templates this hub has: what a seat in each role is for.
///
/// Listed rather than documented, because a fleet can add its own with
/// `--prompts-dir` and nothing written down elsewhere would know about them.
pub async fn roles(State(st): State<AppState>) -> Json<Value> {
    let dir = st.prompts_dir();
    let roles: Vec<Value> = nexmesh_proto::roles::all(dir)
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id, "title": t.title, "summary": t.summary,
                "skills": t.skills,
                "source": nexmesh_proto::roles::role_source(dir, &t.id),
                // The size, not the text: a listing is for choosing, and a
                // fourteen-role listing with every brief in it is a page
                // nobody reads. The text comes back when one is applied.
                "prompt_bytes": t.prompt.len(),
            })
        })
        .collect();
    Json(json!({
        "roles": roles,
        "skills": nexmesh_proto::roles::builtin_skill_names(),
        "local": dir.map(|d| d.display().to_string()),
    }))
}

/// One template's full text, as it would be given to a seat.
///
/// So an operator can read what they are about to apply, and so a surface
/// can offer it as the starting point of an edit.
pub async fn role(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<RoleQuery>,
) -> ApiResult {
    let dir = st.prompts_dir();
    let Some(t) = nexmesh_proto::roles::find(dir, &id) else {
        return Err(bad(format!("no role template '{id}'")));
    };
    // `?skills=a,b` previews a different set, and `?skills=` previews none:
    // the same choice `POST /v1/slots/:id/prompt` takes, so what is read
    // here is what would be stored there.
    let chosen: Option<Vec<String>> = q.skills.as_ref().map(|s| {
        s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
    });
    let text = nexmesh_proto::roles::render(&t, chosen.as_deref(), dir);
    Ok(Json(json!({
        "id": t.id, "title": t.title, "summary": t.summary,
        "skills": chosen.unwrap_or(t.skills), "prompt": text,
    })))
}

#[derive(serde::Deserialize)]
pub struct RoleQuery {
    #[serde(default)]
    pub skills: Option<String>,
}

// ----------------------------------------------------- editing the library ----
//
// Every write below needs somewhere to write: a prompts directory this fleet
// owns. Built-ins live in the binary and are never touched -- a write creates
// or replaces a LOCAL file that shadows the built-in of the same name, which
// is what makes "edit a built-in" and "reset it" the same two operations as
// "add a local one" and "remove it". With no store configured there is
// nowhere to put a local file, so these say so rather than failing obscurely.
fn store_dir(st: &AppState) -> Result<std::path::PathBuf, (StatusCode, Json<Value>)> {
    st.prompts_dir().map(|d| d.to_path_buf()).ok_or_else(|| {
        bad("this hub has no editable prompts store. It is normally provisioned at \
             /var/lib/nexmesh/prompts when the package is installed; set --prompts-dir \
             (or NEXMESH_PROMPTS_DIR) and restart the hub, or run `nexmesh-hub` with it.")
    })
}

/// The whole skill library: every skill, where it comes from, its text, and
/// its fleet-wide (global) enable state. Session and seat state are read with
/// the prefs endpoint, which takes a reference.
pub async fn list_skills(State(st): State<AppState>) -> Json<Value> {
    let dir = st.prompts_dir();
    let global = {
        let db = st.db.lock().unwrap();
        crate::skills::prefs_at(&db, "global", "")
    };
    let skills: Vec<Value> = nexmesh_proto::roles::all_skill_names(dir)
        .into_iter()
        .map(|name| {
            let body = nexmesh_proto::roles::skill(dir, &name).unwrap_or_default();
            json!({
                "name": name,
                "source": nexmesh_proto::roles::skill_source(dir, &name),
                "body": body,
                // absent => follows the template/default; true/false => a
                // fleet-wide dial the operator set.
                "global": global.get(&name),
            })
        })
        .collect();
    Json(json!({
        "skills": skills,
        "editable": dir.is_some(),
        "local": dir.map(|d| d.display().to_string()),
    }))
}

/// One skill's text, with the built-in underneath it when a local copy is
/// shadowing one -- so an editor can offer "reset to the built-in".
pub async fn read_skill(State(st): State<AppState>, Path(name): Path<String>) -> ApiResult {
    let dir = st.prompts_dir();
    let Some(body) = nexmesh_proto::roles::skill(dir, &name) else {
        return Err(bad(format!("no skill '{name}'")));
    };
    // The built-in text, shown only when a local file is on top of one.
    let builtin = nexmesh_proto::roles::builtin_skills()
        .iter()
        .find(|(n, _)| *n == name.trim().to_lowercase())
        .map(|(_, t)| (*t).to_string());
    let src = nexmesh_proto::roles::skill_source(dir, &name);
    Ok(Json(json!({
        "name": name.trim().to_lowercase(),
        "source": src,
        "body": body,
        "builtin": if src == "override" { builtin } else { None },
    })))
}

#[derive(serde::Deserialize)]
pub struct SkillWrite {
    pub body: String,
}

pub async fn write_skill(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<SkillWrite>,
) -> ApiResult {
    let dir = store_dir(&st)?;
    let saved = nexmesh_proto::roles::write_skill(&dir, &name, &req.body).map_err(bad)?;
    audit_prompt(&st, "skill_write", json!({ "skill": saved }));
    Ok(Json(json!({ "name": saved, "source": nexmesh_proto::roles::skill_source(Some(&dir), &saved) })))
}

pub async fn delete_skill(State(st): State<AppState>, Path(name): Path<String>) -> ApiResult {
    let dir = store_dir(&st)?;
    let reverted = nexmesh_proto::roles::delete_skill(&dir, &name).map_err(bad)?;
    audit_prompt(&st, "skill_delete", json!({ "skill": name, "reverted_to_builtin": reverted }));
    Ok(Json(json!({
        "name": name.trim().to_lowercase(),
        // Said plainly, because the two outcomes differ: a built-in is back
        // to its shipped text; a local-only skill is gone.
        "outcome": if reverted { "reverted to the built-in" } else { "removed" },
    })))
}

#[derive(serde::Deserialize)]
pub struct RoleWrite {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub skills: Vec<String>,
    pub prompt: String,
}

pub async fn write_role(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RoleWrite>,
) -> ApiResult {
    let dir = store_dir(&st)?;
    let t = nexmesh_proto::roles::RoleTemplate {
        id: id.trim().to_lowercase(),
        title: req.title,
        summary: req.summary,
        skills: req.skills,
        prompt: req.prompt,
    };
    let saved = nexmesh_proto::roles::write_role(&dir, &t).map_err(bad)?;
    audit_prompt(&st, "role_write", json!({ "role": saved }));
    Ok(Json(json!({ "id": saved, "source": nexmesh_proto::roles::role_source(Some(&dir), &saved) })))
}

pub async fn delete_role(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let dir = store_dir(&st)?;
    let reverted = nexmesh_proto::roles::delete_role(&dir, &id).map_err(bad)?;
    audit_prompt(&st, "role_delete", json!({ "role": id, "reverted_to_builtin": reverted }));
    Ok(Json(json!({
        "id": id.trim().to_lowercase(),
        "outcome": if reverted { "reverted to the built-in" } else { "removed" },
    })))
}

#[derive(serde::Deserialize)]
pub struct SkillPrefQuery {
    pub scope: Option<String>,
    pub reference: Option<String>,
}

/// The enable/disable dial at one scope, as a map of skill -> bool.
pub async fn get_skill_prefs(
    State(st): State<AppState>,
    Query(q): Query<SkillPrefQuery>,
) -> ApiResult {
    let scope = q.scope.as_deref().unwrap_or("global");
    let (scope, reference) = normalise_scope(scope, q.reference.as_deref())?;
    let db = st.db.lock().unwrap();
    let map = crate::skills::prefs_at(&db, &scope, &reference);
    Ok(Json(json!({ "scope": scope, "reference": reference, "prefs": map })))
}

#[derive(serde::Deserialize)]
pub struct SkillPrefSet {
    pub scope: String,
    #[serde(default)]
    pub reference: Option<String>,
    pub skill: String,
    /// true/false set the dial; null clears it, reverting to the wider scope.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Turn one skill on or off at a scope, and bring the seats that were
/// following their defaults into line so the change is actually visible.
pub async fn set_skill_pref(
    State(st): State<AppState>,
    Json(req): Json<SkillPrefSet>,
) -> ApiResult {
    let (scope, reference) = normalise_scope(&req.scope, req.reference.as_deref())?;
    let skill = nexmesh_proto::roles::valid_name(&req.skill).map_err(bad)?;
    let dir = st.prompts_dir().map(|d| d.to_path_buf());
    let db = st.db.lock().unwrap();
    // The seat scope IS the seat's own stored selection, set through the
    // prompt endpoint -- there is no row for it here, and routing it through
    // this dial would be a second source of truth. Say so.
    if scope == "seat" {
        return Err(bad(
            "a seat's own skills are set on the seat, not here: POST /v1/slots/<id>/prompt \
             with skills_on/skills_off. This endpoint is the global and session dials.",
        ));
    }
    let before = crate::skills::snapshot_defaults(&db, dir.as_deref(), &scope, &reference);
    crate::skills::set_pref(&db, &scope, &reference, &skill, req.enabled).map_err(bad)?;
    let moved = crate::skills::propagate(&db, dir.as_deref(), &scope, &reference, &before);
    drop(db);
    audit_prompt(&st, "skill_pref",
        json!({ "scope": scope, "reference": reference, "skill": skill,
                "enabled": req.enabled, "seats_rerendered": moved }));
    Ok(Json(json!({
        "scope": scope, "reference": reference, "skill": skill, "enabled": req.enabled,
        // What actually happened to running seats, so the surface can say it.
        "seats_rerendered": moved,
    })))
}

/// A scope and its reference, checked together: global takes none, session
/// needs a session id. (Seat is refused in the prefs endpoint above.)
fn normalise_scope(
    scope: &str,
    reference: Option<&str>,
) -> Result<(String, String), (StatusCode, Json<Value>)> {
    let scope = scope.trim().to_lowercase();
    match scope.as_str() {
        "global" => Ok(("global".into(), String::new())),
        "session" | "seat" => {
            let r = reference.map(str::trim).filter(|s| !s.is_empty());
            match r {
                Some(r) => Ok((scope, r.to_string())),
                None => Err(bad(format!("the {scope} scope needs a reference"))),
            }
        }
        other => Err(bad(format!(
            "'{other}' is not a scope; it is 'global' or 'session'"
        ))),
    }
}

fn audit_prompt(st: &AppState, action: &str, detail: Value) {
    if let Ok(db) = st.db.lock() {
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,'operator',NULL,?,?)",
            params![now_ms(), action, detail.to_string()],
        );
    }
}

/// Apply a select/deselect pair to a list of skill names.
///
/// Case-folded, de-duplicated and order-preserving, so a surface can send
/// what a person typed and two surfaces toggling the same seat cannot
/// produce a list that depends on who asked last.
fn apply_toggles(
    base: Vec<String>,
    on: &Option<Vec<String>>,
    off: &Option<Vec<String>>,
) -> Vec<String> {
    let norm = |s: &str| s.trim().to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for s in base.iter().map(|s| norm(s)).filter(|s| !s.is_empty()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    for s in on.iter().flatten().map(|s| norm(s)).filter(|s| !s.is_empty()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if let Some(off) = off {
        let drop: Vec<String> = off.iter().map(|s| norm(s)).collect();
        out.retain(|s| !drop.contains(s));
    }
    out
}

/// Set, change or clear what this seat tells its agent.
pub async fn set_slot_prompt(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SeatPromptRequest>,
) -> ApiResult {
    let mode = match req.system_prompt_mode.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(m @ ("append" | "replace")) => Some(m.to_string()),
        Some(other) => {
            return Err(bad(format!(
                "'{other}' is not a system prompt mode; it is 'append' (add to \
                 the CLI's own prompt) or 'replace' (use this instead of it)"
            )))
        }
    };
    let db = st.db.lock().unwrap();
    #[allow(clippy::type_complexity)]
    let row: Option<(Option<String>, String, Option<String>, Option<String>, Option<String>, String)> = db
        .query_row(
            "SELECT system_prompt, COALESCE(system_prompt_mode, 'append'), instructions,
                    prompt_template, prompt_skills, COALESCE(session_id, '')
               FROM slots WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(bad)?;
    let Some((had_prompt, had_mode, had_instr, had_template, had_skills, session_id)) = row else {
        return Err(bad(format!("no seat '{id}'")));
    };
    let had_skills: Vec<String> = had_skills
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    // A TEMPLATE IS A WAY OF WRITING THE SYSTEM PROMPT, not a third field.
    //
    // Rendered here and stored as the prompt, so everything downstream --
    // the spawn, the dashboard, the byte count, the editor -- sees one
    // thing. Given both a template and a prompt, this refuses rather than
    // choosing: two sources for one field is a question for the operator.
    //
    // WHICH TEMPLATE, when none is named: the one this seat already has.
    // That is what makes a skill something an operator can select and
    // deselect -- without it, turning one off meant restating the template
    // and the whole list from memory.
    let wants_skill_change =
        req.skills.is_some() || req.skills_on.is_some() || req.skills_off.is_some();
    // Noted before the field is consumed below: whether the operator wrote
    // the prompt themselves is what decides if this seat keeps a template.
    let prompt_by_hand = req.system_prompt.is_some();
    let named_template = req.template.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let template: Option<String> = match named_template {
        Some(name) => Some(name.to_string()),
        None if wants_skill_change && req.system_prompt.is_none() => {
            let Some(t) = had_template.clone() else {
                return Err(bad(
                    "this seat's prompt was written by hand, so it has no skills to                      select. Apply a template first and they become editable:                      `--template <role>`, or GET /v1/roles for the list.",
                ));
            };
            Some(t)
        }
        None => None,
    };
    // The selection this change produces. An explicit list replaces it
    // outright; the toggles work from whatever the seat already carries --
    // or, when a template is also named, from THAT template's own defaults,
    // which is what "this template, without that one skill" means.
    let chosen: Option<Vec<String>> = if !wants_skill_change {
        None
    } else {
        let base = match (&req.skills, named_template) {
            (Some(list), _) => list.clone(),
            // A named template's base is its defaults AS THE DIALS LEAVE
            // THEM: a skill the fleet or this team has turned on or off is
            // already reflected, so applying a template gives the same set a
            // seat already on it carries, not the raw compiled list.
            (None, Some(name)) => nexmesh_proto::roles::find(st.prompts_dir(), name)
                .map(|t| crate::skills::effective_defaults(&db, &t.skills, &session_id))
                .unwrap_or_default(),
            (None, None) => had_skills.clone(),
        };
        Some(apply_toggles(base, &req.skills_on, &req.skills_off))
    };
    let from_template = match template.as_deref() {
        None => None,
        Some(name) => {
            if req.system_prompt.is_some() {
                return Err(bad(
                    "a template and a system prompt are two ways of filling the same                      field; send one of them",
                ));
            }
            let dir = st.prompts_dir();
            let Some(t) = nexmesh_proto::roles::find(dir, name) else {
                let have: Vec<String> =
                    nexmesh_proto::roles::all(dir).into_iter().map(|t| t.id).collect();
                return Err(bad(format!(
                    "no role template '{name}'. There is: {}",
                    have.join(", ")
                )));
            };
            // A skill this hub does not have is refused rather than
            // quietly rendering a note about it: a checkbox that silently
            // does nothing is worse than an error.
            if let Some(list) = &chosen {
                for want in list {
                    if nexmesh_proto::roles::skill(dir, want).is_none() {
                        return Err(bad(format!(
                            "no skill '{want}'. This hub has: {}",
                            nexmesh_proto::roles::builtin_skill_names().join(", ")
                        )));
                    }
                }
            }
            // WHAT WAS RENDERED is what gets recorded. Applying a
            // template without naming any skills renders the template's own
            // -- so that is the selection, not an empty one. Recording
            // nothing here showed a seat carrying three sections and
            // reporting none, which is the sort of disagreement that makes
            // a checkbox untrustworthy.
            let effective = chosen
                .clone()
                .unwrap_or_else(|| crate::skills::effective_defaults(&db, &t.skills, &session_id));
            Some((nexmesh_proto::roles::render(&t, Some(&effective), dir), effective))
        }
    };
    let (rendered, effective_skills) = match from_template {
        Some((text, skills)) => (Some(text), Some(skills)),
        None => (None, None),
    };
    let (prompt, prompt_changed) = prompt_field(
        "system prompt",
        rendered.or(req.system_prompt),
        had_prompt.clone(),
    )?;
    let (instr, instr_changed) =
        prompt_field("instruction", req.instructions, had_instr.clone())?;
    let mode = mode.unwrap_or_else(|| had_mode.clone());
    // WHAT IT WAS BUILT FROM, kept beside the text.
    //
    // A prompt written by hand keeps no template: once somebody has edited
    // the words, re-rendering from a template would throw their edit away,
    // so the link is dropped and the skills stop being editable until a
    // template is applied again. That is the honest behaviour -- the
    // alternative is a checkbox that silently discards an operator's
    // writing.
    // `system_prompt: ""` is how a prompt is cleared, and it is a hand
    // edit like any other: both land here.
    let (store_template, store_skills) = if prompt_by_hand {
        (None, None)
    } else if let Some(t) = &template {
        let list = effective_skills.clone().unwrap_or_default();
        (Some(t.clone()), Some(serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())))
    } else {
        (had_template.clone(), Some(serde_json::to_string(&had_skills).unwrap_or_default()))
    };
    db.execute(
        "UPDATE slots SET system_prompt = ?, system_prompt_mode = ?, instructions = ?,
                          prompt_template = ?, prompt_skills = ?
          WHERE id = ?",
        params![prompt, mode, instr, store_template, store_skills, id],
    )
    .map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,'operator',NULL,'seat_prompt',?)",
        params![
            now_ms(),
            json!({ "slot": id, "system_prompt_bytes": prompt.as_deref().map(str::len),
                    "system_prompt_mode": mode,
                    "instruction_bytes": instr.as_deref().map(str::len) })
            .to_string()
        ],
    )
    .ok();
    // The text itself is not in the audit line or the log: a seat's system
    // prompt can hold anything the operator wants to tell it privately, and
    // a byte count says what changed without repeating it everywhere.
    tracing::info!(slot = %id, mode = %mode,
                   prompt = prompt.as_deref().map(str::len), instr = instr.as_deref().map(str::len),
                   "the operator set what this seat tells its agent");
    // What takes effect WHEN, said plainly, because the two halves differ:
    // the system prompt needs the next process, the instruction needs the
    // next occupant, and a running agent gets neither until then.
    let mut takes_effect = Vec::new();
    if prompt_changed || (mode != had_mode && prompt.is_some()) {
        // Said differently for a prompt that is GONE: "applies to the next
        // agent" reads as though something was added, and clearing one is
        // the case where an operator most wants to know nothing is left.
        takes_effect.push(if prompt.is_some() {
            "the system prompt applies to the next agent started in this seat"
        } else {
            "this seat's own system prompt is gone; the next agent starts with \
             only what its CLI says"
        });
    }
    if instr_changed {
        takes_effect.push(if instr.is_some() {
            "the instruction is given to the next agent that fills this seat"
        } else {
            "nothing is said to a new agent on arrival any more"
        });
    }
    // The same shape the GET returns, so a surface that just changed
    // something can redraw from the answer instead of asking again -- and so
    // `nexmesh seat-prompt --no-skill x` can print the resulting selection
    // rather than only the prose.
    let now_template = store_template.clone();
    let now_skills: Vec<String> = store_skills
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    let dir = st.prompts_dir();
    let suggests: Vec<String> = now_template
        .as_deref()
        .and_then(|t| nexmesh_proto::roles::find(dir, t))
        .map(|t| t.skills)
        .unwrap_or_default();
    Ok(Json(json!({
        "slot_id": id,
        "system_prompt": prompt,
        "system_prompt_mode": mode,
        "instructions": instr,
        "template": now_template,
        "skills": now_skills,
        "template_skills": suggests,
        "skills_available": nexmesh_proto::roles::all_skill_names(dir),
        "takes_effect": takes_effect,
    })))
}

/// Read back what a seat tells its agent, text and all.
///
/// Separate from the seat's status, which carries only whether these are set
/// and how long they are: a status poll should not drag 64 KiB of prompt
/// through every surface four times a second.
pub async fn slot_prompt(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let dir = st.prompts_dir();
    let db = st.db.lock().unwrap();
    let row: Option<(Option<String>, String, Option<String>, Option<String>, Option<String>)> =
        db.query_row(
            "SELECT system_prompt, COALESCE(system_prompt_mode, 'append'), instructions,
                    prompt_template, prompt_skills
               FROM slots WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(bad)?;
    let Some((prompt, mode, instr, template, skills)) = row else {
        return Err(bad(format!("no seat '{id}'")));
    };
    let chosen: Vec<String> = skills
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    // Enough for a surface to draw a checkbox per skill without asking
    // anything else: which are on, which this role suggests, and everything
    // this hub has to offer. A seat with no template has no skills to
    // select -- `template` is null and the list is empty, which is what a
    // UI should show as "apply a template first".
    let suggests: Vec<String> = template
        .as_deref()
        .and_then(|t| nexmesh_proto::roles::find(dir, t))
        .map(|t| t.skills)
        .unwrap_or_default();
    let available: Vec<String> = nexmesh_proto::roles::all_skill_names(dir);
    Ok(Json(json!({
        "slot_id": id,
        "system_prompt": prompt,
        "system_prompt_mode": mode,
        "instructions": instr,
        "template": template,
        "skills": chosen,
        "template_skills": suggests,
        "skills_available": available,
    })))
}

/// Turn a standing auto-approval on or off for one seat.
pub async fn slot_auto_approve(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AutoApproveRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    crate::approvals::set_auto(&db, "agent", &id, req.on).map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,'operator',NULL,'auto_approve',?)",
        params![now_ms(), json!({ "slot": id, "on": req.on }).to_string()],
    )
    .ok();
    Ok(Json(json!({ "slot_id": id, "auto_approve": req.on })))
}

/// ...or a whole team.
pub async fn session_auto_approve(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AutoApproveRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let sid = session_ref_to_id(&db, &id);
    crate::approvals::set_auto(&db, "session", &sid, req.on).map_err(bad)?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,'operator',NULL,'auto_approve',?)",
        params![now_ms(), json!({ "session": sid, "on": req.on }).to_string()],
    )
    .ok();
    Ok(Json(json!({ "session_id": sid, "auto_approve": req.on })))
}

#[derive(serde::Deserialize)]
pub struct GuestRequest {
    /// The seat, by slot id or by name.
    pub agent: String,
    /// Take it out again instead of putting it in.
    #[serde(default)]
    pub leave: bool,
}

/// Put a seat in this team as well as its own, or take it out again.
///
/// The operator's door. A seat serving several teams is a hole in the session
/// boundary, and opening one needs reach over both teams -- so by a seat it
/// is a superorchestrator's tool, and here it is the person running the
/// fleet, who outranks both.
pub async fn session_guest(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<GuestRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let sid = session_ref_to_id(&db, &id);
    // By slot id, or by name anywhere in the fleet -- the operator is not
    // confined to one team, and the seat being invited is by definition
    // somewhere else.
    let slot: String = db
        .query_row(
            "SELECT id FROM slots WHERE id = ?1 OR display_name = ?1 LIMIT 1",
            [req.agent.trim()],
            |r| r.get(0),
        )
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no seat '{}'", req.agent)))?;
    let said = if req.leave {
        crate::guests::leave(&db, &slot, &sid, "operator").map_err(bad)?
    } else {
        crate::guests::join(&db, &slot, &sid, "operator").map_err(bad)?
    };
    Ok(Json(json!({ "ok": true, "said": said, "slot_id": slot, "session_id": sid })))
}

/// Which teams a seat answers in, and which guests a team has.
pub async fn session_guests(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Json<Value> {
    let db = st.db.lock().unwrap();
    let sid = session_ref_to_id(&db, &id);
    Json(json!({ "session_id": sid, "guests": crate::guests::guests_of(&db, &sid) }))
}

/// Set what a seat is allowed to do to the fleet.
///
/// This is the operator's door -- the dashboard and `/authority` in the chat
/// -- so it is not itself subject to the election rules in
/// `proto::authority`: the person running the fleet outranks every level in
/// it. A seat electing another seat goes through the `elect` TOOL, which does
/// apply them.
///
/// `role` is next door and is a different thing entirely: that is the seat's
/// JOB and confers nothing. This is the authority.
pub async fn set_authority(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AuthorityRequest>,
) -> ApiResult {
    use nexmesh_proto::authority::Authority;
    // Refused rather than parsed loosely: `parse` fails closed to `agent`,
    // which is right when READING a column and wrong when a person is
    // trying to grant something.
    let want = match req.authority.trim().to_lowercase().as_str() {
        // Below `agent`: a seat that reads and reports and cannot steer. See
        // `Authority::Observer`.
        "observer" | "watcher" | "readonly" | "read-only" => Authority::Observer,
        "agent" => Authority::Agent,
        "orchestrator" | "orch" => Authority::Orchestrator,
        "super" | "superorchestrator" | "super-orchestrator" | "superorch" => Authority::Super,
        other => {
            return Err(bad(format!(
                "'{other}' is not an authority. Use 'observer', 'agent', \
                 'orchestrator' or 'super'."
            )))
        }
    };
    let db = st.db.lock().unwrap();
    let was: Option<String> = db
        .query_row("SELECT authority FROM slots WHERE id = ?", [&id], |r| r.get(0))
        .optional()
        .map_err(bad)?
        .ok_or_else(|| bad(format!("no seat '{id}'")))?;
    db.execute(
        "UPDATE slots SET authority = ? WHERE id = ?",
        params![want.as_str(), id],
    )
    .map_err(bad)?;
    // Audited, always. A change in who can command the fleet is the one
    // thing a log has to be able to answer afterwards.
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail)
         VALUES (?,?,NULL,'authority_changed',?)",
        params![now_ms(), "operator",
                json!({ "slot": id, "from": was, "to": want.as_str() }).to_string()],
    )
    .ok();
    tracing::info!(slot = %id, from = ?was, to = %want.as_str(), "authority changed by operator");
    // The grant is in force at once, but the seat's MCP client fixed its tool
    // list when the CLI started, so the model is not yet SHOWN the fleet
    // tools -- and a tool a model was not shown is one it cannot call. Say
    // so here, or granting authority looks like it did nothing.
    let note = if want.is_orchestrating() && was.as_deref() != Some(want.as_str()) {
        Some(
            "In force from the next call. Restart this seat for its CLI to be offered \
             the fleet tools: an MCP client asks for the tool list once, at startup.",
        )
    } else {
        None
    };
    Ok(Json(json!({ "slot_id": id, "authority": want.as_str(), "was": was, "note": note })))
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
                             ORDER BY (exited_at_ms IS NULL) DESC, (io_kind IS NOT NULL) DESC, started_at_ms DESC LIMIT 1)
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
    // Remembered, so the chat can show the picture later rather than the
    // path to it. The bytes stay on the machine that holds them; this is the
    // note that says which machine, and where. See `upload_bytes`.
    let upload_id = format!("up-{}", &uuid::Uuid::new_v4().to_string()[..12]);
    let media_type = media_type_of(&safe);
    {
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO uploads (id, session_id, workspace_id, node_id, node_name, root, rel,
                                  path, name, media_type, bytes, created_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            params![upload_id, session_id, workspace_id, node_id, node_name, root, rel,
                    path, safe, media_type, bytes, now_ms()],
        )
        .map_err(bad)?;
        db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms,
                                   attachment)
             VALUES (?, NULL, NULL, 'system', ?, ?, ?)",
            params![session_id, line, now_ms(), upload_id],
        )
        .map_err(bad)?;
    }
    Ok(Json(json!({
        "ok": true, "path": path, "node": node_name, "bytes": bytes,
        "workspace": ws_name, "rel": rel, "upload_id": upload_id, "media_type": media_type
    })))
}

/// What a file is, from its name, for a chat deciding whether to show it.
///
/// The node sniffs the real bytes when they are fetched and its answer wins;
/// this is only so the chat knows whether to try. Guessing wrong here costs
/// a thumbnail that does not render, not a wrong content type on the wire.
fn media_type_of(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" | "md" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// The bytes of an upload, fetched from the machine that holds them.
///
/// The hub stores no files. A picture in the chat is this: the row says which
/// machine and where, the node reads it back, and the browser gets it with
/// the type the node sniffed from the bytes themselves. Served inline so an
/// `<img>` renders it, and never as anything a browser would run: a file
/// somebody uploaded is not script on the hub's own origin.
pub async fn upload_bytes(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let found: Option<(String, String, String, String)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT node_id, root, rel, name FROM uploads WHERE id = ?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(bad)?
    };
    let Some((node_id, root, rel, name)) = found else {
        return Err(bad("no such upload"));
    };
    if !st.node_is_connected(&node_id) {
        return Err(bad("the machine holding that file is not connected"));
    }
    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(&node_id, &nexmesh_proto::HubToNode::GetFile { req_id, root, rel })
        .await
        .map_err(bad)?;
    let answer = tokio::time::timeout(std::time::Duration::from_secs(60), rx)
        .await
        .map_err(|_| bad("that machine did not answer in time"))?
        .map_err(|_| bad("that machine went away while reading it"))?;
    if answer["ok"].as_bool() != Some(true) {
        return Err(bad(format!(
            "could not read it: {}",
            answer["detail"].as_str().unwrap_or("no reason given")
        )));
    }
    let bytes = nexmesh_proto::b64::decode(answer["data_b64"].as_str().unwrap_or(""))
        .map_err(|e| bad(format!("that machine sent something unreadable: {e}")))?;
    let kind = answer["media_type"].as_str().unwrap_or("application/octet-stream");
    // Only shapes a browser renders are served as themselves. Anything else
    // downloads, so an uploaded .html or .svg cannot run as a page on this
    // origin and read the dashboard's own session.
    let inline = matches!(kind, "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                                | "application/pdf" | "audio/wav" | "audio/mpeg"
                                | "video/mp4" | "video/webm");
    let safe_name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._- ".contains(c) { c } else { '_' })
        .collect();
    let disposition = if inline {
        format!("inline; filename=\"{safe_name}\"")
    } else {
        format!("attachment; filename=\"{safe_name}\"")
    };
    let kind = if inline { kind.to_string() } else { "application/octet-stream".to_string() };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, kind),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
            (
                axum::http::header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
            (axum::http::header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        bytes,
    )
        .into_response())
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
    /// Start one anyway, with something already in the seat.
    ///
    /// Only for the case where the hub's record is wrong -- a process it
    /// believes is alive and is not. It is not a way to run two agents in
    /// one seat: see the guard in `spawn_slot` for what that costs.
    #[serde(default)]
    pub force: bool,
}

pub async fn spawn_slot(
    State(st): State<AppState>,
    Path(slot_id): Path<String>,
    body: Option<Json<SpawnRequest>>,
) -> ApiResult {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    // ONE AGENT PER SEAT.
    //
    // Spawning into an occupied seat gives two processes the same identity
    // and -- because a seat's conversation is resumed by id -- the SAME
    // CONVERSATION. Two CLIs then append to one transcript file. That is
    // what happened on the live fleet: recovery brought a seat back, the
    // operator spawned it again a few seconds later because the first one
    // could not be attached to, and two claudes spent four minutes writing
    // the same 123MB conversation.
    //
    // So it is refused, and the message says what to do instead. `force`
    // exists for the one honest case: the hub believes something is alive
    // that is not.
    if !req.force {
        let live: Option<(String, Option<i64>)> = {
            let db = st.db.lock().unwrap();
            db.query_row(
                "SELECT id, pid FROM agents
                  WHERE slot_id = ? AND exited_at_ms IS NULL
                  ORDER BY started_at_ms DESC LIMIT 1",
                [&slot_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(bad)?
        };
        if let Some((agent, pid)) = live {
            return Err(bad(format!(
                "that seat already has {agent}{} in it. Two agents in one seat resume the \
                 same conversation and write over each other -- attach to it instead, or \
                 stop it first. Pass force=true only if you know the hub's record is stale.",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            )));
        }
    }
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

/// WHAT THE AGENT JUST SAID, out of the screen it also draws itself on.
///
/// "Read that back to me" is a question about one reply, and a terminal is
/// not a list of replies: it is a screen, with the reply somewhere between
/// an input box, a status row, a spinner and a list of files the agent read.
/// The dashboard used to read all of it, line by line, with a full stop
/// inserted between every line -- so a wrapped paragraph came out as a dozen
/// one-word sentences and then recited the input box.
///
/// The screen is in the BROWSER (the terminal is drawn there), and the rule
/// for what is furniture is in `nexmesh_proto::screenread`, shared with the
/// console. So the page sends the screen and gets the reply back, rather
/// than there being two implementations of the same judgement -- one of
/// which would quietly rot.
#[derive(Deserialize)]
pub struct ScreenRead {
    /// The terminal as a person sees it: lines, no escape sequences.
    pub screen: String,
    /// The descriptor id of whatever is drawing it. Unknown is fine: the
    /// generic rules still take the boxes and the status rows out.
    #[serde(default)]
    pub cli: Option<String>,
}

pub async fn screen_reply(Json(req): Json<ScreenRead>) -> ApiResult {
    let cli = req.cli.unwrap_or_default();
    let text = nexmesh_proto::screenread::last_reply(&req.screen, &cli);
    Ok(Json(json!({
        "text": text,
        // Said plainly, so a caller can offer the whole screen instead
        // rather than going silent on a screen with nothing on it.
        "found": !text.is_empty(),
    })))
}

/// WHAT THE FLEET REMEMBERS, ALL OF IT.
///
/// The dashboard only ever saw memory through a session's snapshot, which
/// shows what that team can see -- so on a fleet whose seats had only ever
/// written session-scoped notes the page looked as though session scope was
/// the only one there was. This answers the operator's question instead:
/// every scope, labelled, nothing shadowed, with the counts so an empty
/// scope says it is empty.
#[derive(Deserialize)]
pub struct MemoryQuery {
    /// global, session, group, agent -- or absent for all of them.
    pub scope: Option<String>,
    /// One session id, group name or slot id.
    #[serde(alias = "ref")]
    pub reference: Option<String>,
    /// Matches a key or a value, case-insensitively.
    #[serde(alias = "search")]
    pub q: Option<String>,
    pub limit: Option<i64>,
}

pub async fn list_memory(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryQuery>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let rows = crate::memory::all(
        &db,
        q.scope.as_deref(),
        q.reference.as_deref(),
        q.q.as_deref(),
        q.limit.unwrap_or(500),
    )
    .map_err(|e| bad(format!("{e:#}")))?;
    let counts = crate::memory::counts(&db).unwrap_or_default();
    Ok(Json(json!({
        "memory": rows.iter().map(|r| json!({
            "scope": r.scope, "scope_ref": r.scope_ref, "scope_label": r.scope_label,
            "key": r.key, "category": r.category, "value": r.value, "who": r.author,
            "revision": r.revision, "created_at_ms": r.created_at_ms,
            "at_ms": r.updated_at_ms, "expires_at_ms": r.expires_at_ms,
            "revisions_kept": r.revisions_kept,
        })).collect::<Vec<Value>>(),
        "counts": counts.iter().map(|(s, n)| json!({ "scope": s, "n": n }))
                        .collect::<Vec<Value>>(),
        "categories": crate::memory::CATEGORIES,
    })))
}

#[derive(Deserialize)]
pub struct MemoryWrite {
    /// global, session, group or agent. A session or agent scope needs its
    /// reference; global needs none.
    pub scope: String,
    #[serde(alias = "ref")]
    pub reference: Option<String>,
    pub key: String,
    pub value: Option<String>,
    pub category: Option<String>,
    /// Seconds. Absent or zero means it does not expire.
    pub ttl_seconds: Option<i64>,
    /// True to remove it instead of writing it.
    #[serde(default)]
    pub forget: bool,
}

/// The operator writing, or removing, a memory by hand.
///
/// Global memory is capability-gated for AGENTS -- one seat must not be able
/// to give itself fleet-wide reach -- and the person who grants that
/// capability is the person calling this. Attributed to `operator`, so a
/// convention somebody typed is never mistaken for something a model
/// concluded.
pub async fn write_memory(
    State(st): State<AppState>,
    Json(req): Json<MemoryWrite>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let scope = match req.scope.trim() {
        "global" => crate::memory::Scope::Global,
        "session" => {
            let r = req
                .reference
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("a session memory needs the session it belongs to"))?;
            crate::memory::Scope::Session(session_ref_to_id(&db, r))
        }
        "group" => crate::memory::Scope::Group(
            req.reference
                .as_deref()
                .map(|g| g.trim().to_lowercase())
                .filter(|g| !g.is_empty())
                .ok_or_else(|| bad("a group memory needs the group's name"))?,
        ),
        "agent" | "private" => crate::memory::Scope::Agent(
            req.reference
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("an agent memory needs the seat it belongs to"))?
                .to_string(),
        ),
        other => return Err(bad(format!(
            "unknown scope '{other}'; use agent, group, session or global"
        ))),
    };
    let said = if req.forget {
        crate::memory::operator_forget(&db, &scope, &req.key)
            .map_err(|e| bad(format!("{e:#}")))?
    } else {
        crate::memory::operator_store(
            &db,
            &scope,
            &req.key,
            req.value.as_deref().unwrap_or(""),
            req.category.as_deref().unwrap_or("context"),
            req.ttl_seconds,
        )
        .map_err(|e| bad(format!("{e:#}")))?
    };
    Ok(Json(json!({ "ok": true, "said": said })))
}

/// What a key used to say, newest first. One key's argument history is the
/// part a flat table threw away.
#[derive(Deserialize)]
pub struct MemoryHistoryQuery {
    pub scope: String,
    #[serde(alias = "ref")]
    pub reference: Option<String>,
    pub key: String,
}

pub async fn memory_history(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryHistoryQuery>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let reference = q.reference.clone().unwrap_or_default();
    let scope = match q.scope.trim() {
        "global" => crate::memory::Scope::Global,
        "session" => crate::memory::Scope::Session(session_ref_to_id(&db, &reference)),
        "group" => crate::memory::Scope::Group(reference.trim().to_lowercase()),
        "agent" | "private" => crate::memory::Scope::Agent(reference.trim().to_string()),
        other => return Err(bad(format!("unknown scope '{other}'"))),
    };
    let rows = crate::memory::history(&db, &scope, &q.key).map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({
        "key": q.key,
        "history": rows.iter().map(|(rev, value, who, at)| json!({
            "revision": rev, "value": value, "who": who, "at_ms": at,
        })).collect::<Vec<Value>>(),
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

#[derive(Deserialize)]
pub struct ViewPost {
    /// Who is sharing. A client picks its own and keeps it, so a reconnect
    /// replaces its own frame rather than adding a second viewer.
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub w: u32,
    #[serde(default)]
    pub h: u32,
    /// The frame, as a data URI or bare base64. Whatever the browser's own
    /// encoder produced: it already has one, and the hub has no business
    /// re-encoding a picture it only passes on.
    pub frame: String,
}

/// Take the newest frame from an operator client.
///
/// A headset is a client nobody else can see into. This is how somebody at
/// a desk watches what the person in it is looking at: the client that is
/// drawing the scene flattens it to one picture a few times a second and
/// posts it here, and the hub keeps only the newest.
pub async fn publish_view(
    State(st): State<AppState>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
    Json(req): Json<ViewPost>,
) -> ApiResult {
    let id = req.id.trim();
    if id.is_empty() || id.len() > 64 {
        return Err(bad("a view needs an id"));
    }
    let raw = req.frame.rsplit_once("base64,").map(|(_, b)| b).unwrap_or(&req.frame);
    let bytes = nexmesh_proto::b64::decode(raw.trim())
        .map_err(|e| bad(format!("that frame is not base64: {e}")))?;
    if bytes.is_empty() {
        st.drop_view(id);
        return Ok(Json(json!({ "ok": true, "sharing": false })));
    }
    let media_type = match bytes.as_slice() {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        _ if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" => "image/webp",
        // A frame that is not a picture is not stored: this endpoint hands
        // whatever arrives straight back to another browser.
        _ => return Err(bad("a frame must be a jpeg, a png or a webp")),
    };
    let frame = crate::state::ViewFrame {
        label: if req.label.trim().is_empty() { op.label.clone() } else { req.label.trim().to_string() },
        kind: if req.kind.trim().is_empty() { "client".into() } else { req.kind.trim().to_string() },
        status: req.status.chars().take(160).collect(),
        w: req.w,
        h: req.h,
        at_ms: now_ms(),
        bytes,
        media_type: media_type.to_string(),
    };
    st.put_view(id, frame).map_err(bad)?;
    Ok(Json(json!({ "ok": true, "sharing": true })))
}

/// Who is sharing a view right now.
pub async fn list_views(State(st): State<AppState>) -> Json<Value> {
    let now = now_ms();
    let views: Vec<Value> = st
        .views()
        .into_iter()
        .map(|(id, f)| {
            json!({
                "id": id, "label": f.label, "kind": f.kind, "status": f.status,
                "w": f.w, "h": f.h, "age_ms": now - f.at_ms,
            })
        })
        .collect();
    Json(json!({ "views": views }))
}

/// One client's newest frame, as a picture a browser can show.
pub async fn get_view(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> std::result::Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let Some(f) = st.view(&id) else {
        return Err(bad("nobody is sharing that view"));
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, f.media_type),
            // Never cached: the whole point is that the next request gets a
            // different picture.
            (axum::http::header::CACHE_CONTROL, "no-store, must-revalidate".to_string()),
        ],
        f.bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct MouseTo {
    pub on: bool,
}

/// Give the wheel to the tmux view a seat sits in, or take it back.
///
/// Asked for by a viewer, for as long as somebody wants to scroll. Nothing
/// here is on by default: tmux's mouse mode scrolls a pane's history, which
/// is the one thing an attach cannot do for itself, and takes dragging to
/// select, which is the one thing a person does most.
pub async fn terminal_mouse(
    State(st): State<AppState>,
    Path(slot_ref): Path<String>,
    Json(req): Json<MouseTo>,
) -> ApiResult {
    let found: Option<(String, Option<String>)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT s.id, (SELECT a.node_id FROM agents a
                             WHERE a.slot_id = s.id AND a.exited_at_ms IS NULL
                             ORDER BY (a.io_kind IS NOT NULL) DESC, a.started_at_ms DESC
                             LIMIT 1)
               FROM slots s WHERE s.id = ?",
            [&slot_ref],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(bad)?
    };
    let Some((slot_id, node_id)) = found else {
        return Err(bad("no such seat"));
    };
    let Some(node_id) = node_id.filter(|n| !n.is_empty()) else {
        return Err(bad("that seat has no machine right now"));
    };
    if !st.node_is_connected(&node_id) {
        return Err(bad("that machine is not connected"));
    }
    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(
        &node_id,
        &nexmesh_proto::HubToNode::TerminalMouse { req_id, slot_id, on: req.on },
    )
    .await
    .map_err(bad)?;
    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .map_err(|_| bad("that machine did not answer in time"))?
        .map_err(|_| bad("that machine went away"))?;
    if answer["ok"].as_bool() != Some(true) {
        return Err(bad(format!(
            "tmux refused: {}",
            answer["detail"].as_str().unwrap_or("no reason given")
        )));
    }
    Ok(Json(json!({ "ok": true, "on": req.on })))
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
                    ORDER BY (exited_at_ms IS NULL) DESC, (io_kind IS NOT NULL) DESC, started_at_ms DESC LIMIT 1)
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
                -- An external participant has no node row, but it may have
                -- said where it runs. Naming the host beats the bare word
                -- external, which reads as nowhere.
                COALESCE(n.name, a.host_hint,
                         CASE WHEN a.provenance='external' THEN 'external' ELSE '-' END),
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
                    END, a.id),
                -- What this seat may do TO THE FLEET, which the dashboard
                -- shows and sets. Not `role` above: that is its job.
                COALESCE(s.authority, 'agent'), COALESCE(s.supervisor, 0),
                COALESCE(s.autorespawn, 1), COALESCE(s.auto_approve, 0),
                -- A guest: a seat whose home is another team and that also
                -- answers here. Flagged rather than hidden, because what it
                -- is told in this room it knows in the others.
                s.session_id <> ?1, COALESCE(hs.name, s.session_id),
                -- What this seat tells its agent, as sizes rather than text:
                -- a status poll should not carry 64 KiB of prompt. The text
                -- itself is at /v1/slots/<id>/prompt.
                LENGTH(COALESCE(s.system_prompt, '')),
                COALESCE(s.system_prompt_mode, 'append'),
                LENGTH(COALESCE(s.instructions, '')),
                -- HOW FULL this seat's context is now, and the window it
                -- has to fit in. Not the token columns above, which are
                -- what it has SPENT: a seat can spend twelve windows and be
                -- a third full, and it is this pair that says whether it is
                -- about to lose the middle of its own conversation.
                a.context_used_tokens, a.context_limit_tokens,
                -- What it is set to do about that, and about repeating
                -- itself, with NULL meaning it follows its team.
                s.compact_at_pct, s.loop_guard_repeats,
                a.loop_period, a.loop_repeats
         FROM slots s
         LEFT JOIN sessions hs ON hs.id = s.session_id
         LEFT JOIN agents a ON a.id = (
             SELECT id FROM agents WHERE slot_id = s.id
             ORDER BY (exited_at_ms IS NULL) DESC, (io_kind IS NOT NULL) DESC, started_at_ms DESC LIMIT 1)
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE {here} ORDER BY 2",
        word = crate::seat::agent_word("s", "a"),
        here = crate::guests::in_session("s", "?1"),
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
                // Authority, and the label to show for it. `role` is the
                // job; this is what the seat is allowed to do.
                "authority": r.get::<_, String>(23)?,
                "authority_label": nexmesh_proto::authority::Authority::parse(
                    &r.get::<_, String>(23)?).label(),
                "supervisor": r.get::<_, i64>(24)? == 1,
                // The two per-seat switches the operator sets from any
                // surface: whether it comes back by itself, and whether its
                // permission prompts are answered without asking.
                "autorespawn": r.get::<_, i64>(25)? == 1,
                "auto_approve": r.get::<_, i64>(26)? == 1,
                "visiting": r.get::<_, bool>(27)?,
                "home": r.get::<_, String>(28)?,
                // Set-or-not and how big, so a surface can show the badge and
                // only fetch the text when somebody opens the editor.
                "system_prompt_bytes": r.get::<_, i64>(29)?,
                "system_prompt_mode": r.get::<_, String>(30)?,
                "instruction_bytes": r.get::<_, i64>(31)?,
                // How full, and what is set about it. A surface shows the
                // percentage; the threshold is what it needs to offer to
                // change it.
                "context_used_tokens": r.get::<_, Option<i64>>(32)?,
                "context_limit_tokens": r.get::<_, Option<i64>>(33)?,
                "compact_at_pct": r.get::<_, Option<i64>>(34)?,
                "loop_guard_repeats": r.get::<_, Option<i64>>(35)?,
                "loop_period": r.get::<_, Option<i64>>(36)?,
                "loop_repeats": r.get::<_, Option<i64>>(37)?,
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
                {seen}, {audience},
                m.attachment, u.name, u.media_type, u.bytes, u.node_name
         FROM messages m
         LEFT JOIN slots sf ON sf.id = m.from_slot
         LEFT JOIN slots st2 ON st2.id = m.to_slot
         LEFT JOIN uploads u ON u.id = m.attachment
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
                // The file this message is about, when it is about one, so
                // the chat can show the picture instead of its path.
                "file": match r.get::<_, Option<String>>(10)? {
                    Some(id) => json!({
                        "id": id,
                        "name": r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                        "media_type": r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                        "bytes": r.get::<_, Option<i64>>(13)?.unwrap_or(0),
                        "node": r.get::<_, Option<String>>(14)?.unwrap_or_default(),
                        "url": format!("/v1/uploads/{}", r.get::<_, String>(10)?),
                    }),
                    None => Value::Null,
                },
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
    #[allow(clippy::type_complexity)]
    let found: Option<(String, String, String, Option<String>)> = db
        .query_row(
            "SELECT a.id, COALESCE(s.display_name, s.role, s.id), s.id, s.cli_id
             FROM slots s JOIN agents a ON a.slot_id = s.id
             WHERE s.session_id = ?1
               AND (s.id = ?2 OR a.id = ?2
                    OR LOWER(COALESCE(s.display_name,'')) = LOWER(?2)
                    OR LOWER(COALESCE(s.role,'')) = LOWER(?2))
             ORDER BY (a.exited_at_ms IS NULL) DESC, a.started_at_ms DESC LIMIT 1",
            params![session_id, who],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(bad)?;
    // "No agent matching" is true of a seat nobody is sitting in, and
    // unhelpful: the seat is in every listing, so the operator is left
    // doubting the name they just read. Say which of the two it is.
    let (agent_id, name, slot_id, cli_id) = match found {
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

    // THE TERMINAL THAT WENT AWAY.
    //
    // The link above counts only a terminal that is still running. When a
    // node restart kills the adoption -- the node owned that tmux client,
    // the CLI in the pane did not -- the CLI is still sitting there with its
    // conversation, and `attach` dead-ended: "its CLI was started by hand,
    // so only its own shim reaches it", advising `nexmesh seat`, which would
    // start a SECOND CLI on that conversation. Spawning was refused too,
    // rightly, because the seat was occupied. There was no way out but to
    // know a `term-*` seat existed and start it by hand.
    //
    // So the seat of that dead terminal is reported, and a viewer can adopt
    // the pane again, which is all that was missing.
    let dead_via: Option<(String, String)> = if io_kind.is_none() && via.is_none() {
        db.query_row(
            "SELECT vs.id, COALESCE(vs.display_name, vs.role, vs.id)
               FROM slots s
               JOIN agents v ON v.id = s.terminal_via
               JOIN slots vs ON vs.id = v.slot_id
              WHERE s.id = (SELECT slot_id FROM agents WHERE id = ?1)",
            [&agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
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
            // WHAT IS DRAWING THAT SCREEN. The CLI of the SEAT, not of the
            // terminal it sits in: a viewer asked to read the last reply
            // aloud has to know whose furniture to leave out, and the
            // furniture belongs to claude, not to tmux.
            "slot_id": slot_id, "cli_id": cli_id,
        }))),
        None => Ok(Json(json!({
            "agent_id": agent_id, "name": name,
            "status": status, "exited": exited.is_some(),
            // Which machine, and what kind of terminal -- so a viewer can say
            // whose screen you are looking at before it draws a byte of it.
            "host": host, "io_kind": io_kind,
            // The seat of a terminal this one WAS reachable through, whose
            // own process has gone. Adopting that pane again is what gives
            // this agent a keyboard back.
            "terminal_slot": dead_via.as_ref().map(|(s, _)| s.clone()),
            "terminal_name": dead_via.as_ref().map(|(_, n)| n.clone()),
            // See above: whose furniture is on this screen.
            "slot_id": slot_id, "cli_id": cli_id,
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
    // The ticket goes to the node over its own connection and the answer
    // comes back over this one, so the two race. Whoever is told "here is
    // your ticket" will spend it immediately, and a node that has not got it
    // yet refuses it as one it was never given -- which reads like a denial
    // rather than the timing accident it is. So wait for the node to say it
    // holds the ticket before handing it over.
    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(
        &node_id,
        &nexmesh_proto::HubToNode::AttachTicket {
            req_id,
            ticket: ticket.clone(),
            agent_id: req.agent.clone(),
            expires_at_ms,
            operator: who,
        },
    )
    .await
    .map_err(bad)?;
    // A node too old to answer, or too busy to answer yet, is not a reason to
    // refuse an attach that would have worked: the wait is a courtesy with a
    // bound, and `confirmed` says which of the two happened.
    let confirmed = tokio::time::timeout(std::time::Duration::from_secs(3), rx)
        .await
        .is_ok();
    if !confirmed {
        tracing::warn!(%node_id, "the node did not confirm a direct-attach ticket in time");
    }
    Ok(Json(json!({
        "ticket": ticket, "node": node_name, "agent": req.agent,
        "expires_at_ms": expires_at_ms, "confirmed": confirmed,
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
                                       sent_at_ms, claimed_at_ms, claimed_by, record)
                 VALUES (?, NULL, NULL, 'system', ?, ?, ?, 'console', 1)",
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
                         -- A seat the fleet put in a terminal of its own has
                         -- a keyboard too; see `delivery::interactive_tick`.
                         WHEN a.typed_delivery = 1 AND a.exited_at_ms IS NULL
                              THEN 'terminal'
                         ELSE 'next tool call' END,
                    {word}
               FROM slots s
               LEFT JOIN agents a ON a.id = (
                   SELECT id FROM agents WHERE slot_id = s.id
                   ORDER BY (exited_at_ms IS NULL) DESC, (io_kind IS NOT NULL) DESC, started_at_ms DESC LIMIT 1)
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
        Cmd::Voice { who, voice } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            {
                let db = st.db.lock().unwrap();
                db.execute(
                    "UPDATE slots SET voice = ? WHERE id = ?",
                    params![if voice.is_empty() { None } else { Some(voice.clone()) }, slot],
                )
                .map_err(bad)?;
            }
            Ok(if voice.is_empty() {
                format!("{name} is read in the default voice again")
            } else {
                format!("{name} is read in {voice}")
            })
        }
        Cmd::Supervisor { who, on } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            {
                let db = st.db.lock().unwrap();
                db.execute(
                    "UPDATE slots SET supervisor = ? WHERE id = ?",
                    params![if *on { 1 } else { 0 }, slot],
                )
                .map_err(bad)?;
            }
            Ok(if *on {
                format!(
                    "{name} supervises the fleet: what agents raise to the supervisors \
                     is delivered to it here, with a return address it can answer"
                )
            } else {
                format!("{name} no longer supervises the fleet")
            })
        }
        // The operator's own door, so the election rules in
        // `proto::authority` do not apply: the person running the fleet
        // outranks every level in them. A SEAT electing another seat goes
        // through the `elect` tool, which does apply them.
        Cmd::Authority { who, level } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let Json(v) = set_authority(
                State(st.clone()),
                Path(slot),
                Json(AuthorityRequest { authority: level.clone() }),
            )
            .await?;
            let now = v["authority"].as_str().unwrap_or("agent");
            let was = v["was"].as_str().unwrap_or("agent");
            if now == was {
                return Ok(format!(
                    "{name} is already {}",
                    nexmesh_proto::authority::Authority::parse(now).label()
                ));
            }
            Ok(format!(
                "{name} is now {} (was {}). In force immediately -- the hub checks a \
                 seat's authority on every call -- though its CLI may not list the \
                 fleet tools until it reconnects.",
                nexmesh_proto::authority::Authority::parse(now).label(),
                nexmesh_proto::authority::Authority::parse(was).label()
            ))
        }
        Cmd::Guest { who, join } => {
            // Named fleet-wide, not within this room: the seat being invited
            // is by definition somewhere else, so `seat_by_ref` -- which
            // looks in this session -- would never find it.
            let db = st.db.lock().unwrap();
            let slot: String = db
                .query_row(
                    "SELECT id FROM slots WHERE id = ?1 OR display_name = ?1 LIMIT 1",
                    [who.trim()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(bad)?
                .ok_or_else(|| bad(format!("no seat '{who}' anywhere in the fleet")))?;
            if *join {
                crate::guests::join(&db, &slot, session_id, "operator").map_err(bad)
            } else {
                crate::guests::leave(&db, &slot, session_id, "operator").map_err(bad)
            }
        }
        // Ask a seat to fold what it no longer needs.
        //
        // A message rather than a keystroke, so it works on every CLI
        // whatever its compaction command is -- and so the judgement stays
        // with the only thing that can make it: the agent knows which part
        // of its own context the step it is on has finished with, and the
        // hub does not.
        Cmd::Fold { who } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let found: Option<String> = {
                let db = st.db.lock().unwrap();
                db.query_row(
                    "SELECT a.id FROM agents a
                      WHERE a.slot_id = ? AND a.exited_at_ms IS NULL
                      ORDER BY a.started_at_ms DESC LIMIT 1",
                    [&slot],
                    |r| r.get(0),
                )
                .optional()
                .map_err(bad)?
            };
            let Some(agent_id) = found else {
                return Ok(format!("{name} has nothing running in it to ask."));
            };
            let folds = {
                let db = st.db.lock().unwrap();
                crate::folds::folded_since(&db, &slot, 0).unwrap_or(0)
            };
            crate::watchdog::nudge(
                st,
                crate::watchdog::Nudge {
                    agent_id,
                    slot_id: slot.clone(),
                    session_id: session_id.to_string(),
                    name: name.clone(),
                    // Not framed as an emergency: the operator asked, and
                    // nothing is waiting on the answer.
                    urgent: false,
                    folds,
                },
            )
            .await;
            Ok(format!(
                "Asked {name} to fold what it no longer needs. It decides what that is \u{2014} \
                 the summaries will appear in the memory tab, and the detail stays \
                 reachable with unfold_context."
            ))
        }

        Cmd::Compact { who } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            // The agent currently in that seat, and the node holding it. A
            // seat with nothing in it has no context to compact.
            let found: Option<(String, Option<String>)> = {
                let db = st.db.lock().unwrap();
                db.query_row(
                    "SELECT a.id, a.node_id FROM agents a
                      WHERE a.slot_id = ? AND a.exited_at_ms IS NULL
                      ORDER BY a.started_at_ms DESC LIMIT 1",
                    [&slot],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(bad)?
            };
            let Some((agent_id, node)) = found else {
                return Ok(format!("{name} has nothing running in it to compact."));
            };
            let Some(node) = node else {
                return Ok(format!(
                    "{name} is not on a node this hub can reach, so nothing can be typed at it."
                ));
            };
            st.send_to_node(
                &node,
                &nexmesh_proto::HubToNode::Compact { agent_id, by: "the operator".into() },
            )
            .await
            .map_err(bad)?;
            // What was typed, and whether it was refused, comes back as a
            // note in the chat from the node -- it is the only thing that
            // can see the screen.
            Ok(format!(
                "Asked {name} to compact its context. Its CLI's own compaction command is \
                 typed at it, and the result appears here \u{2014} including a refusal, if its \
                 terminal is waiting for an answer."
            ))
        }
        // How full a seat may get before the hub compacts it, and how many
        // times it may repeat itself before the hub stops it. Both are the
        // same shape -- per seat, per team, or inherited -- so they are
        // written the same way.
        Cmd::CompactAt { who, pct } => {
            let said = match pct {
                None => "follows its team's setting, then the fleet's".to_string(),
                Some(0) => "is never compacted automatically".to_string(),
                Some(n) => format!("is compacted once its context passes {n}% of its window"),
            };
            let scope = set_policy(st, session_id, who, "compact_at_pct", *pct)?;
            Ok(format!("{scope} {said}."))
        }
        Cmd::LoopGuard { who, repeats } => {
            let said = match repeats {
                None => "follows its team's setting, then the fleet's".to_string(),
                Some(0) => "is never stopped for repeating itself".to_string(),
                Some(n) => format!(
                    "is interrupted and paused after repeating the same turn {n} times in a row, \
                     with its conversation left intact and a note to you"
                ),
            };
            let scope = set_policy(st, session_id, who, "loop_guard_repeats", *repeats)?;
            Ok(format!("{scope} {said}."))
        }
        Cmd::Rules { text } => {
            // The same handler the operator's own endpoint uses, so what is
            // said in the chat and what is said over HTTP cannot drift --
            // including who is told about the change.
            match text {
                None => {
                    let db = st.db.lock().unwrap();
                    let have: Option<String> = db
                        .query_row(
                            "SELECT instructions FROM sessions WHERE id = ?",
                            [session_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(bad)?
                        .flatten();
                    Ok(match have.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                        None => "This team has no standing rules. `/rules <text>` sets \
                                 them, and everybody here is told."
                            .to_string(),
                        Some(t) => format!("This team's rules:\n\n{t}"),
                    })
                }
                Some(t) => {
                    let said = set_session_instructions(
                        State(st.clone()),
                        Path(session_id.to_string()),
                        Json(SessionRulesRequest { instructions: Some(t.clone()) }),
                    )
                    .await?;
                    Ok(said.0["detail"].as_str().unwrap_or("done").to_string())
                }
            }
        }
        Cmd::Flood { who, per_min, per_pair } => {
            let mut said: Vec<String> = Vec::new();
            let mut scope = String::new();
            if let Some(v) = per_min {
                scope = set_policy(st, session_id, who, "msg_rate_per_min", Some(*v))?;
                said.push(match v {
                    0 => "sends as much as it likes".to_string(),
                    n => format!("is refused messages past {n} a minute"),
                });
            }
            if let Some(v) = per_pair {
                scope = set_policy(st, session_id, who, "msg_pair_limit", Some(*v))?;
                said.push(match v {
                    0 => "may talk to one teammate as much as it likes".to_string(),
                    n => format!(
                        "is refused messages to any ONE teammate past {n} exchanged in \
                         ten minutes"
                    ),
                });
            }
            if said.is_empty() {
                return Ok("Nothing to set: say a number of messages a minute, a pair \
                           limit, or `off`."
                    .to_string());
            }
            Ok(format!(
                "{scope} {}. The refusal goes to the agent and says what to do instead; \
                 its work is not stopped, only its messages.",
                said.join(", and ")
            ))
        }
        Cmd::AutoRespawn { who, on } => {
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            {
                let db = st.db.lock().unwrap();
                db.execute(
                    "UPDATE slots SET autorespawn = ? WHERE id = ?",
                    params![i64::from(*on), slot],
                )
                .map_err(bad)?;
            }
            Ok(if *on {
                format!(
                    "{name} comes back by itself when its process exits. That prepares the \
                     seat so you can attach to it; it does not reattach your terminal."
                )
            } else {
                format!(
                    "{name} stays down when its process exits, until you start it again \
                     (/start {name})."
                )
            })
        }
        Cmd::AutoApprove { who, on } => {
            // `session` or `team` means the room this was typed in, which is
            // the only session a chat line belongs to.
            if matches!(who.to_lowercase().as_str(), "session" | "team" | "room" | "all") {
                let db = st.db.lock().unwrap();
                crate::approvals::set_auto(&db, "session", session_id, *on).map_err(bad)?;
                return Ok(if *on {
                    "Every seat in this room will answer its own permission prompts from now \
                     on, taking the narrowest affirmative on offer -- never an option that \
                     also stops it asking next time. /autoapprove session off to stop."
                        .to_string()
                } else {
                    "This room's permission prompts come back to you again.".to_string()
                });
            }
            let (slot, name) = seat_by_ref(st, session_id, who)?;
            let db = st.db.lock().unwrap();
            crate::approvals::set_auto(&db, "agent", &slot, *on).map_err(bad)?;
            Ok(if *on {
                format!(
                    "{name} will answer its own permission prompts from now on, taking the \
                     narrowest affirmative on offer -- never an option that also stops it \
                     asking next time. /autoapprove {name} off to stop."
                )
            } else {
                format!("{name}'s permission prompts come back to you again.")
            })
        }
        Cmd::Decide { who, allow } => {
            // The oldest first: if two agents are stopped, the one that has
            // been waiting longest is the one to answer.
            let want_slot = match who {
                Some(w) => Some(seat_by_ref(st, session_id, w)?.0),
                None => None,
            };
            let (id, node, agent) = {
                let db = st.db.lock().unwrap();
                let rows = crate::approvals::pending(&db, Some(session_id));
                let found = rows.into_iter().find(|r| match &want_slot {
                    Some(s) => r["slot_id"].as_str() == Some(s.as_str()),
                    None => true,
                });
                let Some(r) = found else {
                    return Ok(match who {
                        Some(w) => format!("{w} is not waiting on a permission prompt."),
                        None => "Nothing in this room is waiting on a permission prompt."
                            .to_string(),
                    });
                };
                let id = r["id"].as_str().unwrap_or_default().to_string();
                let agent = r["agent"].as_str().unwrap_or_default().to_string();
                let node: Option<String> = db
                    .query_row("SELECT node_id FROM approvals WHERE id = ?", [&id],
                               |x| x.get(0))
                    .optional()
                    .ok()
                    .flatten();
                (id, node, agent)
            };
            let (frame, said) = crate::approvals::decide(
                st, &id, if *allow { "allow" } else { "deny" }, None)
                .map_err(bad)?;
            let mut typed = false;
            if let (Some(frame), Some(node)) = (frame, node) {
                typed = st.send_to_node(&node, &frame).await.is_ok();
            }
            Ok(if typed {
                said
            } else {
                format!("{said} -- but {agent}'s node could not be reached, so nothing \
                         has been typed at it yet.")
            })
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
                // Not forced: `/start` on an occupied seat should say so
                // rather than put a second agent in it.
                Some(Json(SpawnRequest { cli: None, mode: mode.clone(), force: false })),
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
                    // A seat just created has nothing in it; if it somehow
                    // does, saying so beats doubling it up.
                    force: false,
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
/// Set one of the three-state seat policies, on a seat or on a whole team.
///
/// `column` must exist on both `slots` and `sessions`; NULL there means
/// "inherit from the next level up", which is what `value: None` writes.
/// Returns what to call the thing that was set, for the answer.
fn set_policy(
    st: &AppState,
    session_id: &str,
    who: &str,
    column: &str,
    value: Option<i64>,
) -> std::result::Result<String, (StatusCode, Json<Value>)> {
    // `session`/`team` means the room this was typed in, which is the only
    // session a chat line belongs to.
    if matches!(who.to_lowercase().as_str(), "session" | "team" | "room" | "all") {
        let db = st.db.lock().unwrap();
        db.execute(
            &format!("UPDATE sessions SET {column} = ? WHERE id = ?"),
            params![value, session_id],
        )
        .map_err(bad)?;
        return Ok("Every seat in this team that has not set its own".to_string());
    }
    let (slot, name) = seat_by_ref(st, session_id, who)?;
    let db = st.db.lock().unwrap();
    db.execute(
        &format!("UPDATE slots SET {column} = ? WHERE id = ?"),
        params![value, slot],
    )
    .map_err(bad)?;
    Ok(name)
}

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

/// What a participant token is, once authenticated.
pub(crate) struct Participant {
    /// The one session it may join, or None for any.
    pub session: Option<String>,
    /// What a seat taken with it may do. `Agent` unless the token says
    /// otherwise -- it is read with `Authority::parse`, which fails closed.
    pub authority: nexmesh_proto::authority::Authority,
}

/// Authenticate a participant token: which session it is scoped to, and what
/// a seat taken with it may do.
///
/// A participant token can call team tools for its own slot and nothing else
/// -- it can never cause a process to be spawned on anyone's machine.
pub(crate) fn auth_participant(
    st: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<Participant, (StatusCode, Json<Value>)> {
    let token = bearer(headers).ok_or_else(|| {
        err(
            StatusCode::UNAUTHORIZED,
            "missing participant bearer token; mint one with \
             `nexmesh-hub participant-token <label>` (an operator token will not work here)",
        )
    })?;
    let db = st.db.lock().unwrap();
    let row: Option<(Option<String>, Option<String>)> = db
        .query_row(
            "SELECT session_id, authority FROM participant_tokens
             WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
            [crate::state::sha256_hex(&token)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let (session, authority) = row
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "invalid or revoked participant token"))?;
    Ok(Participant {
        session,
        authority: authority
            .as_deref()
            .map(nexmesh_proto::authority::Authority::parse)
            .unwrap_or_default(),
    })
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
    /// WHERE THIS RUNS, as it calls itself -- usually its hostname.
    ///
    /// An external participant has no node row and no process the hub can
    /// see, so the fleet could say only that it was not a machine. On this
    /// fleet that put a service running on a known host under "participants
    /// with no machine", which is true of what the hub knew and wrong about
    /// the world: it has a machine, it is just not an agent CLI on a node.
    ///
    /// A claim, not a fact: nothing verifies it, exactly as `kind_hint` is
    /// whatever the participant says it is.
    #[serde(default)]
    pub host: Option<String>,
}

pub async fn participant_join(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ParticipantJoin>,
) -> ApiResult {
    let who = auth_participant(&st, &headers)?;
    let scoped = who.session.clone();
    let Some(asked) = req.sessions.take().filter(|v| !v.is_empty()) else {
        return Ok(Json(join_participant_as(&st, req, scoped, who.authority)?));
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
            // The same claim in every room it joins: one process, one host.
            host: req.host.clone(),
        };
        match join_participant_as(&st, one, scoped.clone(), who.authority) {
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
    join_participant_as(st, req, scoped, Default::default())
}

/// The same, with WHAT THE SEAT MAY DO carried by the token.
///
/// Separate so the ordinary call sites -- and every test -- keep their
/// shape, while the two doors that authenticate a token can pass on the
/// restriction it carries. An `observer` token can only ever seat an
/// observer: the client does not get a say, which is the whole point of
/// putting it on the token.
pub(crate) fn join_participant_as(
    st: &AppState,
    req: ParticipantJoin,
    scoped: Option<String>,
    authority: nexmesh_proto::authority::Authority,
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
            // A token that has since been restricted takes effect here: the
            // seat it returns to is brought down to what the token now
            // allows. Never UP -- an observer token cannot re-promote a seat
            // somebody demoted.
            if authority.is_read_only() {
                let _ = db.execute(
                    "UPDATE slots SET authority = ? WHERE id = ?",
                    params![authority.as_str(), id],
                );
            }
            tracing::info!(slot_id = %id, name = %req.name,
                "an external participant returned to its seat");
            id
        }
        None => {
            let id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            db.execute(
                "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                                    display_name, role, role_description, capabilities, status,
                                    authority, created_at_ms)
                 VALUES (?,?,?, NULL, 'external', 'headless', ?,?,?,?, 'connected', ?, ?)",
                params![
                    id, session_id, ws_id, req.name, req.role, req.role_description, caps,
                    authority.as_str(), now
                ],
            )
            .map_err(bad)?;
            id
        }
    };
    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO agents (id, slot_id, node_id, provenance, status, kind_hint, host_hint,
                             lease_expires_ms, started_at_ms, last_seen_ms)
         VALUES (?,?, NULL, 'external', 'running', ?, ?, ?, ?, ?)",
        params![agent_id, slot_id, req.kind_hint,
                req.host.as_deref().map(str::trim).filter(|h| !h.is_empty()),
                now + proto::NODE_LEASE_MS as i64, now, now],
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
        // Per seat, like every other tool list: an external participant can
        // hold authority too, and the hub knows its level right here.
        "tools": proto::tools::team_tools_for(crate::fleet::authority_of(&db, &slot_id)),
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
    let scoped = auth_participant(&st, &headers)?.session;
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
    let scoped = auth_participant(&st, &headers)?.session;
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
    let scoped = auth_participant(&st, &headers)?.session;
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
    let scoped = auth_participant(&st, &headers)?.session;
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

#[cfg(test)]
mod spawn_guard_tests {
    use super::*;
    use crate::state::AppState;

    fn seat_with(live: bool) -> AppState {
        let st = AppState::in_memory().unwrap();
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms)
                 VALUES ('s1','T',1,1)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                 VALUES ('sl-1','s1','Dev','claude',1)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO agents (id,slot_id,provenance,status,pid,started_at_ms,
                                     last_seen_ms,exited_at_ms)
                 VALUES ('ag-1','sl-1','managed','running',4242,1,1,?)",
                rusqlite::params![if live { None } else { Some(2i64) }],
            )
            .unwrap();
        }
        st
    }

    /// ONE AGENT PER SEAT.
    ///
    /// Spawning into an occupied seat gives two processes the same identity
    /// and, because a conversation is resumed by id, the same CONVERSATION.
    /// On the live fleet that put two claudes into one 123MB transcript for
    /// four minutes: recovery brought the seat back, and the operator -- who
    /// could not attach to it -- spawned it again.
    #[tokio::test]
    async fn spawning_into_an_occupied_seat_is_refused() {
        let st = seat_with(true);
        let out = spawn_slot(
            State(st.clone()),
            Path("sl-1".to_string()),
            Some(Json(SpawnRequest { cli: None, mode: None, force: false })),
        )
        .await;
        let err = format!("{:?}", out.err().expect("an occupied seat must refuse"));
        assert!(err.contains("already has"), "{err}");
        // The message has to say what to do instead, and name the process,
        // because "refused" alone sends somebody looking for a bug.
        assert!(err.contains("pid 4242"), "{err}");
        assert!(err.contains("attach to it instead"), "{err}");
    }

    /// And an empty seat is not blocked. The guard must not make a seat
    /// unstartable because something exited without being tidied up.
    #[tokio::test]
    async fn an_empty_seat_is_not_blocked_by_the_guard() {
        let st = seat_with(false);
        let out = spawn_slot(
            State(st.clone()),
            Path("sl-1".to_string()),
            Some(Json(SpawnRequest { cli: None, mode: None, force: false })),
        )
        .await;
        // It gets past the guard and fails later for want of a node, which
        // is a different error -- the point is that it is not the guard's.
        let err = format!("{:?}", out.err().expect("no node in this fixture"));
        assert!(!err.contains("already has"), "the guard blocked an empty seat: {err}");
    }
}

/// What a seat tells its agent: setting it, clearing it, and the limits.
#[cfg(test)]
mod seat_prompt_tests {
    use super::*;
    use crate::state::AppState;

    fn seat() -> AppState {
        let st = AppState::in_memory().unwrap();
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms)
                 VALUES ('s1','T',1,1)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                 VALUES ('sl-1','s1','Dev','claude',1)",
                [],
            )
            .unwrap();
        }
        st
    }

    async fn set(st: &AppState, body: Value) -> Value {
        set_slot_prompt(
            State(st.clone()),
            Path("sl-1".to_string()),
            Json(serde_json::from_value(body).unwrap()),
        )
        .await
        .map(|Json(v)| v)
        .unwrap_or_else(|(_, Json(v))| v)
    }

    /// A field left out is left alone. Setting the instruction must not wipe
    /// the system prompt, which is the whole reason one endpoint carries
    /// both: an operator who writes today's instruction has not changed who
    /// the agent is.
    #[tokio::test]
    async fn a_field_left_out_is_left_alone() {
        let st = seat();
        set(&st, json!({ "system_prompt": "you are the reviewer", "system_prompt_mode": "replace" }))
            .await;
        let v = set(&st, json!({ "instructions": "start with crates/node" })).await;
        assert_eq!(v["system_prompt"], "you are the reviewer");
        assert_eq!(v["system_prompt_mode"], "replace");
        assert_eq!(v["instructions"], "start with crates/node");
    }

    /// And a field present and empty clears it, which is how "stop doing
    /// that" is the same call rather than a delete verb.
    #[tokio::test]
    async fn present_and_empty_clears_it() {
        let st = seat();
        set(&st, json!({ "system_prompt": "x", "instructions": "y" })).await;
        let v = set(&st, json!({ "system_prompt": "" })).await;
        assert!(v["system_prompt"].is_null(), "{v}");
        assert_eq!(v["instructions"], "y", "clearing one must not clear the other");
    }

    /// The mode is only those two words. Anything else is a typo that would
    /// otherwise be stored and then quietly treated as `append` at spawn.
    #[tokio::test]
    async fn only_append_and_replace_are_modes() {
        let st = seat();
        let v = set(&st, json!({ "system_prompt_mode": "overwrite" })).await;
        assert!(
            v["error"].as_str().unwrap_or_default().contains("not a system prompt mode"),
            "{v}"
        );
    }

    /// A prompt goes to the CLI as ONE argv entry, and Linux caps one at
    /// 128 KiB. Refusing here is a sentence; not refusing is E2BIG inside a
    /// spawn nobody is watching.
    #[tokio::test]
    async fn an_argv_sized_prompt_is_refused_here_not_at_spawn() {
        let st = seat();
        let v = set(&st, json!({ "system_prompt": "x".repeat(PROMPT_MAX + 1) })).await;
        assert!(v["error"].as_str().unwrap_or_default().contains("single argument"), "{v}");
        // And nothing was stored.
        let Json(now) = slot_prompt(State(st.clone()), Path("sl-1".to_string())).await.unwrap();
        assert!(now["system_prompt"].is_null());
    }

    /// Changing it says WHEN it applies, separately for the two halves: the
    /// system prompt needs the next process, the instruction the next
    /// occupant, and the agent sitting there gets neither.
    #[tokio::test]
    async fn it_says_when_each_half_takes_effect() {
        let st = seat();
        let v = set(&st, json!({ "system_prompt": "a" })).await;
        let notes = v["takes_effect"].as_array().cloned().unwrap_or_default();
        assert_eq!(notes.len(), 1, "{v}");
        assert!(notes[0].as_str().unwrap().contains("next agent started"), "{v}");
        // Setting it to the same text again changes nothing, and must not
        // claim it did.
        let v = set(&st, json!({ "system_prompt": "a" })).await;
        assert!(v["takes_effect"].as_array().unwrap().is_empty(), "{v}");
    }

    #[tokio::test]
    async fn a_seat_that_does_not_exist_is_said_so() {
        let st = seat();
        let out = set_slot_prompt(
            State(st.clone()),
            Path("sl-nope".to_string()),
            Json(serde_json::from_value(json!({ "system_prompt": "x" })).unwrap()),
        )
        .await;
        assert!(format!("{:?}", out.err().unwrap()).contains("no seat"));
    }
}

#[derive(Deserialize)]
pub struct CancelRequest {
    /// One message, by the id the log and the dashboard show.
    pub id: Option<i64>,
    /// Or every message in this team that nobody has seen yet.
    #[serde(default)]
    pub all_unseen: bool,
}

/// Withdraw a message nobody has seen.
///
/// A message waits until it is delivered, and nothing expires it. Addressed
/// to a seat that has gone, or sent in error, it sits there for ever: the
/// sender believes it was heard, the tile counts it, and the next process to
/// take that seat is handed something stale the moment it starts.
///
/// Distinct from the two things that already existed, which an operator kept
/// reaching for and which do not do this:
///
///  * CLAIMING says a person has dealt with it. The message is still
///    delivered -- claiming is about the board, not the queue.
///  * ACKING says a model has seen it, and setting that to hide a message
///    would record that somebody read something they never saw. The log is
///    read later to work out what an agent knew.
///
/// So this is its own state, and a cancelled message is excluded at the one
/// gate every delivery route passes through.
pub async fn cancel_messages(
    State(st): State<AppState>,
    Path(session_ref): Path<String>,
    Extension(op): Extension<std::sync::Arc<crate::state::Operator>>,
    Json(req): Json<CancelRequest>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let session_id = session_ref_to_id(&db, &session_ref);
    let who = format!("operator:{}", op.label);
    let now = now_ms();

    // ONLY WHAT NOBODY HAS SEEN. A message that reached a model cannot be
    // unsent, and saying it was cancelled afterwards would make the log lie
    // about what that agent knew when it acted.
    let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match (req.id, req.all_unseen) {
        (Some(id), _) => (
            "UPDATE messages SET cancelled_at_ms = ?1, cancelled_by = ?2
              WHERE id = ?3 AND session_id = ?4
                AND acked_at_ms IS NULL AND cancelled_at_ms IS NULL",
            vec![Box::new(now), Box::new(who.clone()), Box::new(id), Box::new(session_id.clone())],
        ),
        (None, true) => (
            "UPDATE messages SET cancelled_at_ms = ?1, cancelled_by = ?2
              WHERE session_id = ?3
                AND acked_at_ms IS NULL AND cancelled_at_ms IS NULL",
            vec![Box::new(now), Box::new(who.clone()), Box::new(session_id.clone())],
        ),
        (None, false) => {
            return Err(bad(
                "say which message to cancel: an `id`, or `all_unseen: true` for every \
                 message in this team that nobody has seen yet",
            ))
        }
    };
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let n = db.execute(sql, refs.as_slice()).map_err(bad)?;
    if n > 0 {
        db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
            params![now, who, "messages_cancelled",
                    json!({ "session": session_id, "count": n, "id": req.id }).to_string()],
        )
        .ok();
        tracing::info!(session = %session_id, count = n, by = %who, "messages withdrawn");
    }
    // An id that matched nothing is worth saying out loud rather than
    // answering 0 and looking like success: it is almost always a message
    // somebody has already seen.
    let said = match (req.id, n) {
        (Some(id), 0) => format!(
            "nothing was cancelled: message {id} is not in this team, or an agent has \
             already seen it. A message that reached a model cannot be unsent."
        ),
        (Some(_), _) => "cancelled; it will not be delivered".to_string(),
        (None, 0) => "nothing was waiting to be seen".to_string(),
        (None, _) => format!("cancelled {n} message(s) nobody had seen"),
    };
    Ok(Json(json!({ "cancelled": n, "session_id": session_id, "said": said })))
}

/// Withdrawing a message nobody has seen.
#[cfg(test)]
mod cancel_tests {
    use super::*;
    use crate::state::AppState;

    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        {
            let db = st.db.lock().unwrap();
            db.execute_batch(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);
                 INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                   VALUES ('sl-1','s1','Dev','claude',1);
                 INSERT INTO messages (session_id,from_slot,to_slot,kind,body,sent_at_ms)
                   VALUES ('s1',NULL,'sl-1','system','never mind this',1);
                 INSERT INTO messages (session_id,from_slot,to_slot,kind,body,sent_at_ms,acked_at_ms)
                   VALUES ('s1',NULL,'sl-1','system','already read',1,2);",
            )
            .unwrap();
        }
        st
    }

    async fn cancel(st: &AppState, body: Value) -> Value {
        cancel_messages(
            State(st.clone()),
            Path("s1".to_string()),
            Extension(std::sync::Arc::new(crate::state::Operator::super_as("tester"))),
            Json(serde_json::from_value(body).unwrap()),
        )
        .await
        .map(|Json(v)| v)
        .unwrap_or_else(|(_, Json(v))| v)
    }

    /// THE POINT: a cancelled message is never handed to an agent. Checked
    /// through `pending_for_slot`, which is the one gate every delivery
    /// route goes through -- the push tick, the typed tick, the interactive
    /// tick, a shim asking and the participant endpoint all call it.
    #[tokio::test]
    async fn a_cancelled_message_is_never_delivered() {
        let st = fleet();
        let before = {
            let db = st.db.lock().unwrap();
            crate::tools::pending_for_slot(&db, "s1", "sl-1").unwrap().len()
        };
        assert_eq!(before, 1, "the unacked one is waiting");
        let v = cancel(&st, json!({ "id": 1 })).await;
        assert_eq!(v["cancelled"], 1, "{v}");
        let after = {
            let db = st.db.lock().unwrap();
            crate::tools::pending_for_slot(&db, "s1", "sl-1").unwrap().len()
        };
        assert_eq!(after, 0, "a withdrawn message must not be deliverable");
    }

    /// A message a model has SEEN cannot be unsent, and saying otherwise
    /// would make the log lie about what that agent knew when it acted.
    #[tokio::test]
    async fn a_message_already_seen_is_refused() {
        let st = fleet();
        let v = cancel(&st, json!({ "id": 2 })).await;
        assert_eq!(v["cancelled"], 0, "{v}");
        assert!(
            v["said"].as_str().unwrap_or_default().contains("cannot be unsent"),
            "it has to say why, or 0 reads as success: {v}"
        );
    }

    /// Cancelling everything unseen leaves the seen ones alone.
    #[tokio::test]
    async fn all_unseen_spares_what_was_seen() {
        let st = fleet();
        let v = cancel(&st, json!({ "all_unseen": true })).await;
        assert_eq!(v["cancelled"], 1, "only the unseen one: {v}");
        let db = st.db.lock().unwrap();
        let still: i64 = db
            .query_row("SELECT COUNT(*) FROM messages WHERE cancelled_at_ms IS NULL", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(still, 1, "the one that was read is untouched");
    }

    /// Asking for nothing in particular is a mistake worth naming, not a
    /// silent no-op that reads as success.
    #[tokio::test]
    async fn it_refuses_to_guess_what_to_cancel() {
        let st = fleet();
        let out = cancel_messages(
            State(st.clone()),
            Path("s1".to_string()),
            Extension(std::sync::Arc::new(crate::state::Operator::super_as("tester"))),
            Json(serde_json::from_value(json!({})).unwrap()),
        )
        .await;
        assert!(format!("{:?}", out.err().unwrap()).contains("say which message"));
    }

    /// Cancelling twice is not an error, and does not double-count.
    #[tokio::test]
    async fn cancelling_twice_changes_nothing() {
        let st = fleet();
        assert_eq!(cancel(&st, json!({ "id": 1 })).await["cancelled"], 1);
        assert_eq!(cancel(&st, json!({ "id": 1 })).await["cancelled"], 0);
    }
}

/// Where a participant that is not a CLI says it runs.
#[cfg(test)]
mod participant_host_tests {
    use super::*;
    use crate::state::AppState;

    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        st.db
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);",
            )
            .unwrap();
        st
    }

    fn join(st: &AppState, name: &str, host: Option<&str>) -> Value {
        join_participant(
            st,
            ParticipantJoin {
                session: Some("s1".into()),
                sessions: None,
                name: name.into(),
                role: Some("services".into()),
                role_description: None,
                kind_hint: Some("test bridge".into()),
                workspace: None,
                capabilities: vec!["ocr".into()],
                host: host.map(str::to_string),
            },
            None,
        )
        .expect("join")
    }

    /// THE POINT: a service on a known host is somewhere, and the fleet
    /// should say where. It was filed under "no machine", which is true of
    /// a node row and wrong about the world.
    #[tokio::test]
    async fn a_service_that_says_where_it_runs_is_recorded_there() {
        let st = fleet();
        join(&st, "CoderAI", Some("workstation"));
        let Json(v) = session_status(State(st.clone()), Path("s1".to_string())).await;
        let row = v["agents"].as_array().unwrap().iter().find(|a| a["name"] == "CoderAI").unwrap();
        assert_eq!(row["host"], "workstation", "{row}");
    }

    /// One that says nothing still reads as external rather than claiming a
    /// machine it never named.
    #[tokio::test]
    async fn one_that_says_nothing_is_not_given_a_machine() {
        let st = fleet();
        join(&st, "Quiet", None);
        let Json(v) = session_status(State(st.clone()), Path("s1".to_string())).await;
        let row = v["agents"].as_array().unwrap().iter().find(|a| a["name"] == "Quiet").unwrap();
        assert_eq!(row["host"], "external", "{row}");
    }

    /// Blank is the same as not saying, so a bridge that cannot work out its
    /// own hostname does not record an empty machine name.
    #[tokio::test]
    async fn a_blank_host_is_not_a_machine_name() {
        let st = fleet();
        join(&st, "Blank", Some("   "));
        let db = st.db.lock().unwrap();
        let h: Option<String> = db
            .query_row(
                "SELECT host_hint FROM agents a JOIN slots s ON s.id = a.slot_id
                  WHERE s.display_name = 'Blank'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(h.is_none(), "a blank claim is no claim: {h:?}");
    }
}
