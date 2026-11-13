//! The MCP front door for external participants.
//!
//! An external agent could already join a team: `/v1/participant/*` gives it
//! a seat, the same tool dispatcher every CLI agent uses, and delivery with
//! acknowledgement. What it could not do was join over **MCP** -- so joining
//! meant writing a client for an API that is ours alone. Every agent runtime
//! worth integrating already speaks MCP; asking them to speak nexmesh HTTP
//! instead was the wrong way round.
//!
//! This is the same participant, reached by the protocol they already have.
//! One endpoint, a token, and a URL:
//!
//! ```text
//! claude mcp add --transport http team https://hub:7870/v1/mcp?name=Hermes \
//!     --header "Authorization: Bearer <participant token>"
//! ```
//!
//! ## Transport
//!
//! Streamable HTTP, the transport that replaced the old HTTP+SSE pair. POST
//! carries JSON-RPC in and the reply comes straight back as JSON; `initialize`
//! mints an `Mcp-Session-Id` the client echoes afterwards. There is no
//! server-initiated stream, so GET is answered 405 rather than left to hang:
//! teammate messages ride back on tool results, which is the universal
//! delivery floor and needs nothing from the client.
//!
//! ## Why the ack is deferred
//!
//! The stdio shim acks as soon as it has written a tool result, because its
//! stdout is a pipe to a process on the same machine. Here the result crosses
//! a network. Acking when the response is *generated* would mark messages
//! delivered that a dropped connection meant nobody read.
//!
//! So an MCP session carries the ids it last handed over, and they are acked
//! when that client comes back -- its return is the proof it got them. A
//! client that never returns has them reclaimed by the stale-inflight sweep
//! and redelivered. Later, or twice, but never silently lost.

use crate::{api, state::AppState};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use nexmesh_proto::{self as proto, now_ms, Envelope};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

/// MCP revisions this door is known to work with, newest first.
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// What we answer when a client asks for something we do not know.
const PREFERRED: &str = "2025-03-26";
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-client configuration, taken from the endpoint URL.
///
/// An MCP client has no "join" call and no way to pass a role, so the URL is
/// where that goes -- the same place a hosted MCP server takes its per-client
/// settings. Everything here is optional: with a session-scoped token and a
/// client that sends its own name, the bare endpoint is enough.
#[derive(Debug, Default, Deserialize)]
pub struct DoorParams {
    pub name: Option<String>,
    pub role: Option<String>,
    pub role_description: Option<String>,
    pub session: Option<String>,
    pub workspace: Option<String>,
    pub kind_hint: Option<String>,
    /// Capability labels, comma-separated: `?can=ocr,transcribe`. A client
    /// that bridges to something else says so here, and teammates can then
    /// address what it does rather than its name.
    pub can: Option<String>,
}

fn jerr(id: Value, code: i64, message: impl std::fmt::Display) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.to_string() } })
}

fn jok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Reject a cross-origin POST from a browser.
///
/// A browser on any page can POST to a loopback address, and this endpoint
/// acts on a bearer token. Non-browser clients send no `Origin` at all, so
/// requiring an absent or same-host origin costs them nothing.
fn origin_ok(headers: &HeaderMap, host: Option<&str>) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = host else { return false };
    origin
        .rsplit_once("//")
        .map(|(_, rest)| rest == host)
        .unwrap_or(false)
}

/// Messages rendered the way every other delivery path renders them, so an
/// agent sees one convention whichever door it came through.
fn with_pending(text: &str, deliver: &[Envelope]) -> String {
    if deliver.is_empty() {
        return text.to_string();
    }
    let mut body = String::from("[PENDING MESSAGES]\n");
    for m in deliver {
        body.push_str(&format!(
            "- from {}: {}\n",
            m.from_name.as_deref().unwrap_or("orchestrator"),
            m.body
        ));
    }
    body.push_str("[END PENDING MESSAGES]\nRespond to these before continuing.\n\n");
    body.push_str(text);
    body
}

struct Door {
    session: String,
    agent_id: String,
}

/// Look up an MCP session, check it belongs to this token, and ack whatever it
/// was last given.
fn resume(
    st: &AppState,
    sid: &str,
    token: &str,
) -> std::result::Result<Door, (StatusCode, Json<Value>)> {
    let db = st.db.lock().unwrap();
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT agent_id, pending_ack FROM mcp_sessions
             WHERE id = ? AND token_sha256 = ? AND closed_at_ms IS NULL",
            params![sid, crate::state::sha256_hex(token)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))))?;
    // Session id AND token, so a captured session id is not a credential on
    // its own.
    let (agent_id, pending) = row.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown or closed MCP session; send initialize again" })),
        )
    })?;
    // The client came back, which is proof it received the last result.
    let ids: Vec<i64> = serde_json::from_str(&pending).unwrap_or_default();
    if !ids.is_empty() {
        let _ = crate::tools::ack_messages(&db, &agent_id, &ids);
        let _ = db.execute(
            "UPDATE mcp_sessions SET pending_ack = '[]', last_seen_ms = ? WHERE id = ?",
            params![now_ms(), sid],
        );
        tracing::debug!(session = %sid, acked = ids.len(), "MCP client returned; messages acked");
    } else {
        let _ = db.execute(
            "UPDATE mcp_sessions SET last_seen_ms = ? WHERE id = ?",
            params![now_ms(), sid],
        );
    }
    Ok(Door { session: sid.to_string(), agent_id })
}

/// POST: one JSON-RPC message in, its reply out.
pub async fn post(
    State(st): State<AppState>,
    Query(params): Query<DoorParams>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let host = headers.get("host").and_then(|v| v.to_str().ok()).map(str::to_string);
    if !origin_ok(&headers, host.as_deref()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "cross-origin requests are not accepted here" })),
        )
            .into_response();
    }
    // The participant token is the credential, exactly as on the HTTP door.
    let scoped = match api::auth_participant(&st, &headers) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let token = match crate::auth::bearer(&headers) {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing token" }))).into_response(),
    };

    let req: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return Json(jerr(Value::Null, -32700, format!("parse error: {e}"))).into_response()
        }
    };
    let Some(method) = req["method"].as_str().map(str::to_string) else {
        // No method at all is not a JSON-RPC message. Accepting it as a
        // notification would hide a client bug behind a 202.
        return (
            StatusCode::BAD_REQUEST,
            Json(jerr(
                req.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                "not a JSON-RPC request: no 'method'",
            )),
        )
            .into_response();
    };
    let id = req.get("id").cloned();
    let sid = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);

    // A notification carries no id and must never be answered. 202 with no
    // body is what the transport expects.
    let Some(id) = id else {
        tracing::debug!(%method, "MCP notification");
        return StatusCode::ACCEPTED.into_response();
    };

    if method == "initialize" {
        return initialize(&st, &params, &req, id, &token, scoped).await;
    }

    // Everything else needs the session minted by initialize.
    let Some(sid) = sid else {
        return (
            StatusCode::BAD_REQUEST,
            Json(jerr(id, -32600, "missing Mcp-Session-Id header; call initialize first")),
        )
            .into_response();
    };
    let door = match resume(&st, &sid, &token) {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    api::touch_participant(&st, &door.agent_id);

    match method.as_str() {
        "ping" => Json(jok(id, json!({}))).into_response(),

        "tools/list" => Json(jok(
            id,
            json!({ "tools": proto::tools::team_tools().iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })).collect::<Vec<_>>() }),
        ))
        .into_response(),

        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("").to_string();
            let args = req["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, deliver, is_error) =
                match crate::nodes::run_tool_async(&st, &door.agent_id, &name, &args).await {
                    Ok((text, deliver)) => (text, deliver, false),
                    Err(e) => (format!("ERROR: {e}"), vec![], true),
                };
            // Remembered, not acked: see the note at the top of this file.
            if !deliver.is_empty() {
                let ids: Vec<i64> = deliver.iter().map(|m| m.id).collect();
                let db = st.db.lock().unwrap();
                let _ = db.execute(
                    "UPDATE mcp_sessions SET pending_ack = ? WHERE id = ?",
                    params![serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into()), door.session],
                );
            }
            let mut result = json!({
                "content": [ { "type": "text", "text": with_pending(&text, &deliver) } ]
            });
            if is_error {
                result["isError"] = Value::Bool(true);
            }
            Json(jok(id, result)).into_response()
        }

        other => Json(jerr(id, -32601, format!("method '{other}' is not supported"))).into_response(),
    }
}

/// A seat this token already holds under this name, if there is one.
///
/// `initialize` is not a one-off: a client reconnects, and tools like
/// `claude mcp list` send one just to check the server is alive. Seating a new
/// participant each time would fill a session with teammates that never answer
/// -- which is exactly what the first run against a real MCP client did.
///
/// The transport session is replaced (the client may have lost its id) while
/// the seat is kept. Anything the old session was holding undelivered goes
/// back in the queue rather than being quietly marked read.
fn reuse_seat(st: &AppState, token: &str, name: &str) -> Option<(String, String, String)> {
    let db = st.db.lock().unwrap();
    let found: Option<(String, String, String, String, String)> = db
        .query_row(
            "SELECT m.id, m.agent_id, m.pending_ack, s.id, s.session_id
               FROM mcp_sessions m
               JOIN agents a ON a.id = m.agent_id
               JOIN slots  s ON s.id = a.slot_id
              WHERE m.token_sha256 = ? AND m.closed_at_ms IS NULL
                AND a.exited_at_ms IS NULL AND s.display_name = ?
              ORDER BY m.last_seen_ms DESC LIMIT 1",
            params![crate::state::sha256_hex(token), name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (old_session, agent_id, pending, slot_id, session_id) = found?;
    let ids: Vec<i64> = serde_json::from_str(&pending).unwrap_or_default();
    if !ids.is_empty() {
        for id in &ids {
            let _ = db.execute(
                "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
                 WHERE id = ? AND acked_at_ms IS NULL",
                [id],
            );
        }
        tracing::info!(count = ids.len(),
            "an MCP client re-initialised holding undelivered messages; back in the queue");
    }
    let _ = db.execute(
        "UPDATE mcp_sessions SET closed_at_ms = ? WHERE id = ?",
        params![now_ms(), old_session],
    );
    Some((agent_id, slot_id, session_id))
}

async fn initialize(
    st: &AppState,
    params: &DoorParams,
    req: &Value,
    id: Value,
    token: &str,
    scoped: Option<String>,
) -> Response {
    let asked = req["params"]["protocolVersion"].as_str().unwrap_or(PREFERRED);
    let version = if SUPPORTED.contains(&asked) { asked } else { PREFERRED };

    // A client that sends no name in the URL still has one: its own.
    let name = params
        .name
        .clone()
        .or_else(|| req["params"]["clientInfo"]["name"].as_str().map(str::to_string))
        .unwrap_or_else(|| "mcp-participant".to_string());
    // Reconnecting, or merely being health-checked, must not cost a new seat.
    let (agent_id, slot_id, reused) = match reuse_seat(st, token, &name) {
        Some((agent_id, slot_id, session_id)) => {
            tracing::info!(%agent_id, %slot_id, %name,
                "an MCP client re-initialised; keeping its existing seat");
            let _ = session_id;
            (agent_id, slot_id, true)
        }
        None => {
            let join = api::ParticipantJoin {
                session: params.session.clone(),
                name: name.clone(),
                role: params.role.clone(),
                role_description: params.role_description.clone(),
                kind_hint: params.kind_hint.clone().or_else(|| {
                    req["params"]["clientInfo"]["name"].as_str().map(|c| format!("mcp:{c}"))
                }),
                workspace: params.workspace.clone(),
                capabilities: params
                    .can
                    .as_deref()
                    .map(|c| {
                        c.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
                    })
                    .unwrap_or_default(),
            };
            let seated = match api::join_participant(st, join, scoped) {
                Ok(v) => v,
                Err(e) => return e.into_response(),
            };
            (
                seated["agent_id"].as_str().unwrap_or_default().to_string(),
                seated["slot_id"].as_str().unwrap_or_default().to_string(),
                false,
            )
        }
    };
    let _ = reused;

    let sid = format!("mcp-{}", uuid::Uuid::new_v4().simple());
    {
        let db = st.db.lock().unwrap();
        if let Err(e) = db.execute(
            "INSERT INTO mcp_sessions (id, agent_id, token_sha256, protocol, created_at_ms, last_seen_ms)
             VALUES (?,?,?,?,?,?)",
            params![sid, agent_id, crate::state::sha256_hex(token), version, now_ms(), now_ms()],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }
    tracing::info!(session = %sid, %agent_id, %slot_id, protocol = %version,
        "an MCP client joined the team");

    let result = json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "nexmesh", "version": VERSION },
        "instructions": format!(
            "You are part of a nexmesh team as '{}' (slot {slot_id}). Teammates may run on \
             other machines, under other CLIs, or not be CLIs at all. Call query_knowledge \
             and check_team_status before planning. Teammate messages arrive prefixed \
             [PENDING MESSAGES] on any tool result.",
            name
        ),
    });
    let mut res = Json(jok(id, result)).into_response();
    res.headers_mut().insert(
        SESSION_HEADER,
        sid.parse().unwrap_or_else(|_| "invalid".parse().unwrap()),
    );
    res
}

/// GET: there is no server-initiated stream here.
///
/// Answered rather than left open, because a client that hangs waiting for a
/// stream looks like a hub that is not responding. Delivery rides on tool
/// results instead, which needs nothing the client does not already do.
pub async fn get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [("allow", "POST, DELETE")],
        Json(json!({
            "error": "this MCP endpoint has no server-initiated stream; teammate messages \
                      are returned on tool results"
        })),
    )
        .into_response()
}

/// DELETE: the client is done. Close the session and retire the seat.
pub async fn delete(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = crate::auth::bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "missing token" }))).into_response();
    };
    let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing Mcp-Session-Id header" })),
        )
            .into_response();
    };
    let door = match resume(&st, sid, &token) {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    {
        let db = st.db.lock().unwrap();
        let now = now_ms();
        let _ = db.execute(
            "UPDATE mcp_sessions SET closed_at_ms = ? WHERE id = ?",
            params![now, door.session],
        );
        // The participant is gone, so release the seat rather than leave a
        // teammate that looks present and never answers.
        let _ = db.execute(
            "UPDATE agents SET status = 'exited', exited_at_ms = ? WHERE id = ?",
            params![now, door.agent_id],
        );
        let _ = db.execute(
            "UPDATE slots SET status = 'done' WHERE id = (SELECT slot_id FROM agents WHERE id = ?)",
            [&door.agent_id],
        );
    }
    tracing::info!(session = %sid, agent = %door.agent_id, "an MCP client left the team");
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexmesh_proto::MessageKind;

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_client_that_sends_no_origin_is_fine() {
        // Which is every non-browser MCP client.
        assert!(origin_ok(&HeaderMap::new(), Some("hub:7870")));
    }

    #[test]
    fn a_same_origin_browser_request_is_allowed() {
        assert!(origin_ok(
            &hdr(&[("origin", "https://hub:7870")]),
            Some("hub:7870")
        ));
    }

    #[test]
    fn a_cross_origin_browser_request_is_refused() {
        // DNS rebinding: any page can POST to a loopback address, and this
        // endpoint acts on a bearer token.
        assert!(!origin_ok(
            &hdr(&[("origin", "https://evil.example")]),
            Some("127.0.0.1:7870")
        ));
        assert!(!origin_ok(&hdr(&[("origin", "https://hub:7870")]), None));
    }

    fn env(id: i64, from: &str, body: &str) -> Envelope {
        Envelope {
            id,
            from_slot: Some("sl-a".into()),
            from_name: Some(from.into()),
            to_slot: Some("sl-b".into()),
            kind: MessageKind::Text,
            body: body.into(),
            sent_at_ms: 0,
        }
    }

    #[test]
    fn a_result_with_no_messages_is_left_alone() {
        assert_eq!(with_pending("done", &[]), "done");
    }

    #[test]
    fn pending_messages_are_framed_like_every_other_door() {
        let out = with_pending("ok", &[env(1, "Iris", "please review")]);
        assert!(out.starts_with("[PENDING MESSAGES]"));
        assert!(out.contains("from Iris: please review"));
        assert!(out.contains("[END PENDING MESSAGES]"));
        assert!(out.ends_with("ok"), "{out}");
    }

    #[test]
    fn an_unnamed_sender_is_the_orchestrator() {
        let mut e = env(1, "x", "do this");
        e.from_name = None;
        assert!(with_pending("ok", &[e]).contains("from orchestrator: do this"));
    }

    #[test]
    fn we_answer_a_protocol_version_we_know_and_fall_back_otherwise() {
        for v in SUPPORTED {
            assert!(SUPPORTED.contains(v));
        }
        assert!(!SUPPORTED.contains(&"1999-01-01"));
        assert!(SUPPORTED.contains(&PREFERRED), "the fallback must be one we support");
    }
}
