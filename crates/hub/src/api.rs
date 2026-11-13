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
    .map_err(|e| bad(e))?;

    for w in &req.workspaces {
        if !["node_local", "repo"].contains(&w.kind.as_str()) {
            return Err(bad(format!("workspace kind must be node_local or repo, got '{}'", w.kind)));
        }
        let node_id: Option<String> = match &w.node {
            Some(n) => Some(
                db.query_row("SELECT id FROM nodes WHERE name = ?", [n], |r| r.get(0))
                    .optional()
                    .map_err(|e| bad(e))?
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
            params![ws_id, id, w.name, w.kind, node_id, w.repo_url, w.repo_branch,
                    if w.kind == "repo" { "git" } else { "none" }],
        )
        .map_err(|e| bad(e))?;
        for (node_name, path) in &w.mounts {
            let nid: Option<String> = db
                .query_row("SELECT id FROM nodes WHERE name = ?", [node_name], |r| r.get(0))
                .optional()
                .map_err(|e| bad(e))?;
            let nid = nid.ok_or_else(|| bad(format!("mount names unknown node '{node_name}'")))?;
            db.execute(
                "INSERT INTO workspace_mounts (workspace_id, node_id, path) VALUES (?,?,?)",
                params![ws_id, nid, path],
            )
            .map_err(|e| bad(e))?;
        }
    }

    for (i, label) in req.plan.iter().enumerate() {
        db.execute(
            "INSERT INTO plan_items (session_id, ord, label, updated_at_ms) VALUES (?,?,?,?)",
            params![id, i as i64 + 1, label, now],
        )
        .map_err(|e| bad(e))?;
    }
    Ok(Json(json!({ "session_id": id, "name": req.name })))
}

pub async fn list_sessions(State(st): State<AppState>) -> Json<Value> {
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(
        "SELECT s.id, s.name, s.status, s.created_at_ms,
                (SELECT COUNT(*) FROM slots WHERE session_id = s.id)
         FROM sessions s ORDER BY s.created_at_ms DESC",
    ) else {
        return Json(json!({ "sessions": [] }));
    };
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
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
    Path(session_id): Path<String>,
    Json(req): Json<NewSlot>,
) -> ApiResult {
    let db = st.db.lock().unwrap();
    let exists: Option<i64> = db
        .query_row("SELECT 1 FROM sessions WHERE id = ?", [&session_id], |r| r.get(0))
        .optional()
        .map_err(|e| bad(e))?;
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
            .map_err(|e| bad(e))?
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
    .map_err(|e| bad(e))?;
    if let (Some(ws), false) = (&ws_id, req.file_ownership.is_empty()) {
        for pat in &req.file_ownership {
            db.execute(
                "INSERT OR IGNORE INTO ownership (workspace_id, slot_id, pattern) VALUES (?,?,?)",
                params![ws, slot_id, pat],
            )
            .map_err(|e| bad(e))?;
        }
    }
    if let Some(t) = &req.task {
        db.execute(
            "UPDATE slots SET role_description = COALESCE(role_description, '') || ?
             WHERE id = ?",
            params![format!("\n\nTASK: {t}"), slot_id],
        )
        .map_err(|e| bad(e))?;
    }
    Ok(Json(json!({ "slot_id": slot_id })))
}

/// Place a slot on a node and ask that node to start it.
pub async fn spawn_slot(State(st): State<AppState>, Path(slot_id): Path<String>) -> ApiResult {
    match crate::launch::launch_slot(&st, &slot_id, None).await {
        Ok((agent_id, node)) => Ok(Json(json!({
            "slot_id": slot_id, "agent_id": agent_id, "node": node
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
            .map_err(|e| bad(e))?;
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
                .map_err(|e| bad(e))?,
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
                format!("You are an adopted terminal session.\n\nTASK: continue what you were doing."),
                req.tmux_target,
                now_ms()
            ],
        )
        .map_err(|e| bad(e))?;
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
    Path(session_id): Path<String>,
    Json(req): Json<SyncRequest>,
) -> ApiResult {
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
            .map_err(|e| bad(e))?;
        stmt.query_map(params![session_id, req.workspace], |r| r.get(0))
            .and_then(|m| m.collect())
            .map_err(|e| bad(e))?
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
    Path(session_id): Path<String>,
    Json(req): Json<PauseRequest>,
) -> ApiResult {
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
    Path(session_id): Path<String>,
    Json(req): Json<BudgetRequest>,
) -> ApiResult {
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
        .map_err(|e| bad(e))?;
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
    Path(session_id): Path<String>,
    Json(req): Json<ReleaseRequest>,
) -> ApiResult {
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

pub async fn session_status(State(st): State<AppState>, Path(session_id): Path<String>) -> Json<Value> {
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
    Path(session_id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Json<Value> {
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
    let text = st.read_transcript(&agent_id, tail).map_err(|e| bad(e))?;
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
        .map_err(|e| bad(e))?;
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
    Path(session_id): Path<String>,
    Json(req): Json<OperatorMessage>,
) -> ApiResult {
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
            .map_err(|e| bad(e))?
            .ok_or_else(|| bad(format!("no slot matching '{t}'")))?,
        ),
        None => None,
    };
    db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, ?, 'system', ?, ?)",
        params![session_id, to_slot, req.text, now_ms()],
    )
    .map_err(|e| bad(e))?;
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
                    .map_err(|e| bad(e))?;
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
                .map_err(|e| bad(e))?
                .ok_or_else(|| bad(format!("no session '{s}'")))?
            }
        }
        None => {
            let ids: Vec<String> = db
                .prepare("SELECT id FROM sessions WHERE status = 'active'")
                .and_then(|mut s| s.query_map([], |r| r.get(0)).and_then(|m| m.collect()))
                .map_err(|e| bad(e))?;
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
            .map_err(|e| bad(e))?,
        None => None,
    };

    let now = now_ms();
    let slot_id = format!("sl-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO slots (id, session_id, workspace_id, cli_id, participant_kind, mode,
                            display_name, role, role_description, status, created_at_ms)
         VALUES (?,?,?, NULL, 'external', 'headless', ?,?,?, 'connected', ?)",
        params![slot_id, session_id, ws_id, req.name, req.role, req.role_description, now],
    )
    .map_err(|e| bad(e))?;
    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO agents (id, slot_id, node_id, provenance, status, kind_hint,
                             lease_expires_ms, started_at_ms, last_seen_ms)
         VALUES (?,?, NULL, 'external', 'running', ?, ?, ?, ?)",
        params![agent_id, slot_id, req.kind_hint, now + proto::NODE_LEASE_MS as i64, now, now],
    )
    .map_err(|e| bad(e))?;
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
    tools::ack_messages(&db, &req.agent_id, &req.message_ids).map_err(|e| bad(e))?;
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
        .map_err(|e| bad(e))?
        .ok_or_else(|| bad("unknown agent"))?;
    let pending = tools::pending_for_slot(&db, &session_id, &slot_id).map_err(|e| bad(e))?;
    let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
    tools::mark_inflight(&db, &q.agent_id, &ids).map_err(|e| bad(e))?;
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
