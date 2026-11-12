//! HTTP API: the operator surface, plus the participant front door.
//!
//! The participant endpoints are what make a non-CLI agent -- a hermes agent,
//! a Python daemon, anything -- a first-class team member. It joins with a
//! token and a URL, calls the same tools through the same dispatcher, and
//! appears in `list_peers` like any other teammate. It needs no node, no pty
//! and no MCP implementation.

use crate::{nodes::run_tool, prompt, state::AppState, tools};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use nexmesh_proto::{self as proto, now_ms, HubToNode, Placement, RunMode, SpawnSpec};
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
}

pub async fn create_session(State(st): State<AppState>, Json(req): Json<NewSession>) -> ApiResult {
    if req.name.trim().is_empty() {
        return Err(bad("session name must not be empty"));
    }
    let db = st.db.lock().unwrap();
    let now = now_ms();
    let id = format!("se-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    db.execute(
        "INSERT INTO sessions (id, name, created_at_ms, last_active_ms) VALUES (?,?,?,?)",
        params![id, req.name, now, now],
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
                            placement, display_name, role, role_description, created_at_ms)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        params![
            slot_id, session_id, ws_id, req.cli_id, kind, mode,
            serde_json::to_string(&req.placement).unwrap_or_else(|_| "{}".into()),
            req.display_name, req.role, req.role_description, now_ms()
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
    #[allow(clippy::type_complexity)]
    let (session_id, session_name, cli_id, mode, placement_json, display_name, role,
         role_desc, ws_id, resume): (String, String, Option<String>, String, String,
         Option<String>, Option<String>, Option<String>, Option<String>, Option<String>) = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT s.session_id, se.name, s.cli_id, s.mode, s.placement, s.display_name,
                    s.role, s.role_description, s.workspace_id, s.resume_handle
             FROM slots s JOIN sessions se ON se.id = s.session_id WHERE s.id = ?",
            [&slot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?,
                    r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
        )
        .optional()
        .map_err(|e| bad(e))?
        .ok_or_else(|| bad(format!("no slot '{slot_id}'")))?
    };

    let placement: Placement = serde_json::from_str(&placement_json).unwrap_or_default();
    let node_id = st.place(cli_id.as_deref(), &placement).map_err(|e| bad(e))?;

    let (node_name, cwd, shared) = {
        let db = st.db.lock().unwrap();
        let node_name: String = db
            .query_row("SELECT name FROM nodes WHERE id = ?", [&node_id], |r| r.get(0))
            .map_err(|e| bad(e))?;
        let (cwd, shared) = match &ws_id {
            Some(w) => {
                let path: Option<String> = db
                    .query_row(
                        "SELECT path FROM workspace_mounts WHERE workspace_id = ? AND node_id = ?",
                        params![w, node_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| bad(e))?;
                let path = path.ok_or_else(|| {
                    bad(format!(
                        "workspace has no mount on node '{node_name}': add one so the agent knows \
                         where to work on that machine"
                    ))
                })?;
                let kind: String = db
                    .query_row("SELECT kind FROM workspaces WHERE id = ?", [w], |r| r.get(0))
                    .map_err(|e| bad(e))?;
                (path, kind == "repo")
            }
            None => (
                std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
                false,
            ),
        };
        (node_name, cwd, shared)
    };

    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let name = display_name.clone().unwrap_or_else(|| "agent".into());
    let role_s = role.clone().unwrap_or_else(|| "team member".into());
    let desc = role_desc.unwrap_or_default();
    let task = desc
        .split("TASK: ")
        .nth(1)
        .unwrap_or("Coordinate with your team.")
        .to_string();
    let brief = prompt::AgentBrief {
        display_name: &name,
        role: &role_s,
        role_description: desc.split("\n\nTASK: ").next().unwrap_or(""),
        session_name: &session_name,
        node_name: &node_name,
        workspace_path: &cwd,
        shared_workspace: shared,
        task: &task,
    };

    // Render the prompt before building the spec: the brief borrows `cwd`,
    // which the spec then takes ownership of.
    let prompt_text = prompt::first_prompt(&brief);
    drop(brief);

    let spec = SpawnSpec {
        slot_id: slot_id.clone(),
        agent_id: agent_id.clone(),
        session_id,
        cli_id: cli_id.unwrap_or_default(),
        mode: if mode == "interactive" { RunMode::Interactive } else { RunMode::Headless },
        cwd,
        role,
        display_name,
        prompt: prompt_text,
        env: vec![],
        resume_handle: resume,
    };

    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?, 'operator', ?, 'spawn', ?)",
            params![now_ms(), node_id, json!({ "slot": slot_id, "agent": agent_id }).to_string()],
        );
    }
    st.send_to_node(&node_id, &HubToNode::Spawn(spec))
        .await
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(json!({ "slot_id": slot_id, "agent_id": agent_id, "node": node_name })))
}

pub async fn session_status(State(st): State<AppState>, Path(session_id): Path<String>) -> Json<Value> {
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(
        "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id), COALESCE(s.role, '-'),
                s.participant_kind, COALESCE(s.cli_id, '-'), s.task_state,
                s.summary, COALESCE(a.provenance, '-'), COALESCE(a.status, 'disconnected'),
                COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external' ELSE '-' END),
                COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0)
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
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    Json(json!({ "session_id": session_id, "agents": rows }))
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
fn auth_participant(st: &AppState, headers: &HeaderMap) -> std::result::Result<Option<String>, (StatusCode, Json<Value>)> {
    let token = bearer(headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
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
    row.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "bad participant token"))
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

    Ok(Json(json!({
        "agent_id": agent_id,
        "slot_id": slot_id,
        "session_id": session_id,
        "heartbeat_ms": proto::HEARTBEAT_INTERVAL_MS,
        "tools": proto::tools::team_tools(),
    })))
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
    match run_tool(&st, &req.agent_id, &req.tool, &req.args) {
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
fn touch_participant(st: &AppState, agent_id: &str) {
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
fn verify_agent_scope(
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
