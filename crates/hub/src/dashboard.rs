//! The web dashboard: one snapshot endpoint and one static page.
//!
//! Served by the hub rather than a second daemon on a second port, so there is
//! one process to run, one address to reach and one credential to hold. The
//! snapshot sits behind the same operator middleware as everything else; the
//! page itself carries no data, so it is served without a token and asks for
//! one in the browser.

use crate::state::AppState;
use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Response},
    Json,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};

const PAGE: &str = include_str!("../static/dashboard.html");

pub async fn page() -> Response {
    // no-store, deliberately. The page is one self-contained file that
    // changes whenever the hub is upgraded, and it carried no cache headers
    // at all -- so a browser cached it heuristically and kept serving an old
    // dashboard against a new hub. That is invisible and maddening: the forms
    // on screen are from a version that no longer exists.
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-store, must-revalidate"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Html(PAGE),
    )
        .into_response()
}

/// Tiny inline favicon so the browser stops asking for one.
pub async fn favicon() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#111827"/><circle cx="16" cy="9" r="3.2" fill="#6ee7b7"/><circle cx="8" cy="23" r="3.2" fill="#93c5fd"/><circle cx="24" cy="23" r="3.2" fill="#fcd34d"/><path d="M16 12v4m0 0-6 5m6-5 6 5" stroke="#4b5563" stroke-width="1.6" fill="none"/></svg>"##,
    )
        .into_response()
}

/// The manifest that makes this installable on a phone.
///
/// A fleet is watched from wherever you happen to be, which is usually not
/// at a desk. Installed, it opens as its own app rather than a tab in a
/// browser you then have to find again.
pub async fn manifest() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        r##"{
  "name": "nexmesh",
  "short_name": "nexmesh",
  "description": "Agents across every machine you own.",
  "start_url": "./",
  "scope": "./",
  "display": "standalone",
  "orientation": "any",
  "background_color": "#0b0f14",
  "theme_color": "#0b0f14",
  "icons": [
    { "src": "icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any" },
    { "src": "icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any" },
    { "src": "icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable" },
    { "src": "icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any" }
  ]
}"##,
    )
        .into_response()
}

/// The installed app's icon. One SVG at any size, so there is nothing to
/// regenerate and nothing to keep in step.
pub async fn app_icon() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" fill="#0b0f14"/><circle cx="256" cy="150" r="46" fill="#6ee7b7"/><circle cx="140" cy="370" r="46" fill="#93c5fd"/><circle cx="372" cy="370" r="46" fill="#fcd34d"/><path d="M256 196v70m0 0-92 70m92-70 92 70" stroke="#4b5563" stroke-width="22" fill="none" stroke-linecap="round"/></svg>"##,
    )
        .into_response()
}

/// PNG icons, because a manifest with only an SVG does not meet Chrome's bar
/// for installability -- it wants 192 and 512 px bitmaps, and without them
/// Android never offers to install at all.
pub async fn app_icon_png(axum::extract::Path(size): axum::extract::Path<String>) -> Response {
    let bytes: &[u8] = match size.as_str() {
        "icon-192.png" => include_bytes!("../static/icon-192.png"),
        "icon-512.png" => include_bytes!("../static/icon-512.png"),
        _ => return (axum::http::StatusCode::NOT_FOUND, "no such icon").into_response(),
    };
    ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
}

/// A service worker, which is what makes the browser offer to install it.
///
/// Deliberately almost nothing: it claims the scope and passes every request
/// straight through. A dashboard is a live view of a fleet, and a cached one
/// is worse than none -- it would show yesterday's agents as though they
/// were running.
pub async fn service_worker() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        r##"self.addEventListener("install", (e) => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));
// A fetch handler is part of what makes a browser treat this as an app, but
// it caches NOTHING and never will: a stale view of a fleet would show
// yesterday's agents as though they were running.
self.addEventListener("fetch", (e) => e.respondWith(fetch(e.request)));
"##,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct StateQuery {
    /// Which session to detail. Omitted: just the fleet and the session list.
    pub session: Option<String>,
}

/// Everything the page draws, in one request.
///
/// Deliberately one round trip rather than a dozen: the page polls this, so a
/// single query keeps the view internally consistent — agents and the messages
/// between them always come from the same instant.
pub async fn snapshot(State(st): State<AppState>, Query(q): Query<StateQuery>) -> Json<Value> {
    let authed = st.operator_auth_configured();
    let nodes = st.nodes_json();
    let pair_listen = st.pair_listen();
    let pki_ready = st.pki_ready();
    let db = st.db.lock().unwrap();

    // Outstanding pairing codes, never the codes themselves.
    let now = nexmesh_proto::now_ms();
    let pairing: Vec<Value> = db
        .prepare(
            "SELECT id, kind, name, expires_at_ms, used_at_ms, revoked_at_ms
             FROM pairing_codes ORDER BY issued_at_ms DESC LIMIT 25",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| {
                let used: Option<i64> = r.get(4)?;
                let revoked: Option<i64> = r.get(5)?;
                let expires: i64 = r.get(3)?;
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                    "name": r.get::<_, String>(2)?, "expires_at_ms": expires,
                    "state": if revoked.is_some() { "revoked" }
                             else if used.is_some() { "used" }
                             else if expires < now { "expired" }
                             else { "open" },
                }))
            })
            .and_then(|m| m.collect())
        })
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

    let mut sessions: Vec<Value> = db
        .prepare(
            "SELECT s.id, s.name, s.status, s.created_at_ms, s.last_active_ms,
                    (SELECT COUNT(*) FROM slots WHERE session_id = s.id)
             FROM sessions s ORDER BY s.last_active_ms DESC",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                    "status": r.get::<_, String>(2)?, "created_at_ms": r.get::<_, i64>(3)?,
                    "last_active_ms": r.get::<_, i64>(4)?, "slots": r.get::<_, i64>(5)?,
                }))
            })
            .and_then(|m| m.collect())
        })
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

    // Teammates that are not machines. An external participant joins over
    // HTTP with no node of its own, so it appears on no fleet row at all --
    // and the fleet tab is the first place somebody looks to ask who is
    // here. Carried with the session list because that is what the fleet
    // tab already has in hand.
    for sx in sessions.iter_mut() {
        let Some(sid) = sx["id"].as_str().map(String::from) else { continue };
        let outside: Vec<Value> = db
            .prepare(
                "SELECT s.id, s.display_name, s.role, s.status, s.participant_kind
                   FROM slots s
                  WHERE s.session_id = ? AND s.participant_kind <> 'cli'
                  ORDER BY s.display_name",
            )
            .and_then(|mut q| {
                q.query_map([&sid], |r| {
                    Ok(json!({
                        "slot_id": r.get::<_, String>(0)?,
                        "name": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        "role": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        "status": r.get::<_, String>(3)?,
                        "kind": r.get::<_, String>(4)?,
                    }))
                })
                .and_then(|m| m.collect())
            })
            .unwrap_or_default();
        if !outside.is_empty() {
            sx["external"] = json!(outside);
        }
    }

    // Default to the most recently active session so the page is useful with
    // no query string at all.
    let sid = q
        .session
        .filter(|s| !s.is_empty())
        .or_else(|| sessions.first().and_then(|s| s["id"].as_str().map(String::from)));

    // `*` means every session at once. A fleet is usually several teams, and
    // the thing a person most wants at a glance -- is anything stuck? -- was
    // the one thing the dashboard could not answer without clicking through
    // each session in turn.
    const ALL: &str = "*";
    let detail = match &sid {
        None => Value::Null,
        Some(sid) => {
            let agents: Vec<Value> = db
                .prepare(
                    "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id),
                            COALESCE(s.role, '-'), s.participant_kind, COALESCE(s.cli_id, '-'),
                            s.task_state, s.summary, s.status, s.restarts,
                            COALESCE(sx.name, ''),
                            COALESCE(a.provenance, '-'), COALESCE(a.status, 'disconnected'),
                            COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external'
                                                  ELSE '-' END),
                            COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0),
                            COALESCE(a.cache_read_tokens,0), COALESCE(a.push_capable,0),
                            a.id, s.mode, s.paused, a.pid, a.io_kind
                     FROM slots s
                     LEFT JOIN sessions sx ON sx.id = s.session_id
                     LEFT JOIN agents a ON a.id = (
                        SELECT id FROM agents WHERE slot_id = s.id
                        ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
                     LEFT JOIN nodes n ON n.id = a.node_id
                     WHERE (?1 = '*' OR s.session_id = ?1) ORDER BY 2",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "slot_id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                            "role": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                            "cli": r.get::<_, String>(4)?, "task_state": r.get::<_, String>(5)?,
                            "summary": r.get::<_, String>(6)?, "slot_status": r.get::<_, String>(7)?,
                            "restarts": r.get::<_, i64>(8)?,
                            "session": r.get::<_, String>(9)?,
                            "provenance": r.get::<_, String>(10)?,
                            "status": r.get::<_, String>(11)?, "host": r.get::<_, String>(12)?,
                            "input_tokens": r.get::<_, i64>(13)?, "output_tokens": r.get::<_, i64>(14)?,
                            "cache_read_tokens": r.get::<_, i64>(15)?,
                            "push_capable": r.get::<_, i64>(16)? == 1,
                            "agent_id": r.get::<_, Option<String>>(17)?,
                            "mode": r.get::<_, String>(18)?,
                            "paused": r.get::<_, i64>(19)? == 1,
                            // Which process, and whether it has a terminal.
                            // `pty` can be attached to and typed at, `pipes`
                            // can only be watched, and nothing at all means
                            // the node never started it.
                            "pid": r.get::<_, Option<i64>>(20)?,
                            "io_kind": r.get::<_, Option<String>>(21)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let messages: Vec<Value> = db
                .prepare(
                    "SELECT m.id, m.sent_at_ms, m.kind, m.body,
                            COALESCE(sf.display_name, sf.role, 'operator'),
                            CASE WHEN m.to_slot IS NULL THEN '(team)'
                                 ELSE COALESCE(st2.display_name, st2.role, m.to_slot) END,
                            m.acked_at_ms IS NOT NULL, m.inflight_at_ms IS NOT NULL,
                            COALESCE(mx.name, ''),
                            m.claimed_at_ms IS NOT NULL, COALESCE(m.claimed_by, ''),
                            COALESCE(m.from_slot, ''), COALESCE(m.to_slot, '')
                     FROM messages m
                     LEFT JOIN slots sf ON sf.id = m.from_slot
                     LEFT JOIN slots st2 ON st2.id = m.to_slot
                     LEFT JOIN sessions mx ON mx.id = m.session_id
                     WHERE (?1 = '*' OR m.session_id = ?1) ORDER BY m.id DESC LIMIT 120",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "id": r.get::<_, i64>(0)?, "at_ms": r.get::<_, i64>(1)?,
                            "kind": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                            "from": r.get::<_, String>(4)?, "to": r.get::<_, String>(5)?,
                            "delivered": r.get::<_, bool>(6)?, "in_flight": r.get::<_, bool>(7)?,
                            "session": r.get::<_, String>(8)?,
                            "claimed": r.get::<_, bool>(9)?,
                            "claimed_by": r.get::<_, String>(10)?,
                            "from_slot": r.get::<_, String>(11)?,
                            "to_slot": r.get::<_, String>(12)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let plan: Vec<Value> = db
                .prepare(
                    "SELECT p.ord, p.label, p.status,
                            COALESCE(s.display_name, s.role, '')
                     FROM plan_items p LEFT JOIN slots s ON s.id = p.assignee_slot
                     WHERE (?1 = '*' OR p.session_id = ?1) ORDER BY p.ord",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "ord": r.get::<_, i64>(0)?, "label": r.get::<_, String>(1)?,
                            "status": r.get::<_, String>(2)?, "who": r.get::<_, String>(3)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            // Session memory, plus the global memory this session can see.
            // Scope is carried through so the page can distinguish a fleet-wide
            // convention from something this team decided; reading the old flat
            // `knowledge` table here is what made the dashboard go blank when
            // memory gained scopes.
            let knowledge: Vec<Value> = db
                .prepare(
                    "SELECT m.key, m.category, m.value,
                            COALESCE(s.display_name, s.role, '?'), m.updated_at_ms,
                            m.scope, m.revision
                     FROM memory m LEFT JOIN slots s ON s.id = m.author_slot
                     WHERE (m.scope = 'session' AND (?1 = '*' OR m.scope_ref = ?1))
                        OR m.scope = 'global'
                       AND (m.expires_at_ms IS NULL OR m.expires_at_ms > ?2)
                     ORDER BY m.scope, m.category, m.key",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![sid, nexmesh_proto::now_ms()], |r| {
                        Ok(json!({
                            "key": r.get::<_, String>(0)?, "category": r.get::<_, String>(1)?,
                            "value": r.get::<_, String>(2)?, "who": r.get::<_, String>(3)?,
                            "at_ms": r.get::<_, i64>(4)?, "scope": r.get::<_, String>(5)?,
                            "revision": r.get::<_, i64>(6)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let locks: Vec<Value> = db
                .prepare(
                    "SELECT l.scope, l.path, COALESCE(s.display_name, s.role, l.held_by_slot),
                            COALESCE(l.purpose, ''), l.expires_at_ms
                     FROM locks l LEFT JOIN slots s ON s.id = l.held_by_slot
                     WHERE (?1 = '*' OR l.session_id = ?1) ORDER BY l.scope, l.path",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "scope": r.get::<_, String>(0)?, "path": r.get::<_, String>(1)?,
                            "who": r.get::<_, String>(2)?, "purpose": r.get::<_, String>(3)?,
                            "expires_at_ms": r.get::<_, i64>(4)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let zones: Vec<Value> = db
                .prepare(
                    "SELECT w.name, o.pattern, COALESCE(s.display_name, s.role, o.slot_id)
                     FROM ownership o
                     JOIN workspaces w ON w.id = o.workspace_id
                     LEFT JOIN slots s ON s.id = o.slot_id
                     WHERE (?1 = '*' OR w.session_id = ?1) ORDER BY w.name, o.pattern",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "workspace": r.get::<_, String>(0)?, "pattern": r.get::<_, String>(1)?,
                            "who": r.get::<_, String>(2)?,
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let workspaces: Vec<Value> = db
                .prepare(
                    "SELECT w.name, w.kind, COALESCE(w.repo_url, ''), COALESCE(n.name, '-'),
                            (SELECT GROUP_CONCAT(nn.name || ':' || m.path, '  ')
                             FROM workspace_mounts m JOIN nodes nn ON nn.id = m.node_id
                             WHERE m.workspace_id = w.id)
                     FROM workspaces w LEFT JOIN nodes n ON n.id = w.node_id
                     WHERE (?1 = '*' OR w.session_id = ?1) ORDER BY w.name",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "name": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                            "repo": r.get::<_, String>(2)?, "node": r.get::<_, String>(3)?,
                            "mounts": r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_else(|e| {
            tracing::error!(error = %e, "a dashboard query failed; showing nothing for it");
            vec![]
        });

            let session = db
                .query_row(
                    "SELECT id, name, status, created_at_ms FROM sessions
                      WHERE id = ?1 AND ?1 <> '*'",
                    [sid],
                    |r| {
                        Ok(json!({
                            "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                            "status": r.get::<_, String>(2)?, "created_at_ms": r.get::<_, i64>(3)?,
                        }))
                    },
                )
                .optional()
                .unwrap_or(None)
                .unwrap_or_else(|| {
                    if sid == ALL {
                        json!({ "id": ALL, "name": "all sessions", "status": "active",
                                "created_at_ms": 0 })
                    } else {
                        Value::Null
                    }
                });

            // The numbers worth a glance: who is working, and what is stuck.
            let working = agents.iter().filter(|a| a["task_state"] == "working").count();
            let approved = agents.iter().filter(|a| a["task_state"] == "approved").count();
            let awaiting = agents
                .iter()
                .filter(|a| a["task_state"] == "done_pending_review")
                .count();
            let live = agents.iter().filter(|a| a["status"] == "running").count();
            let undelivered = messages
                .iter()
                .filter(|m| m["delivered"] == false && m["in_flight"] == false)
                .count();
            // Delivered is not dealt with. This is the number a person
            // actually wants: what is waiting on somebody.
            let unclaimed = messages.iter().filter(|m| m["claimed"] == false).count();
            let tin: i64 = agents.iter().filter_map(|a| a["input_tokens"].as_i64()).sum();
            let tout: i64 = agents.iter().filter_map(|a| a["output_tokens"].as_i64()).sum();
            let flapping = agents.iter().filter(|a| a["slot_status"] == "flapping").count();

            // The code graph, per workspace: its subsystems and what is in
            // them. Until now this existed only behind a tool, so an agent
            // could see the shape of a codebase and a person could not --
            // and the subsystem map is the part a person most wants.
            // Queried from the workspaces table rather than derived from the
            // array above, which deliberately does not expose internal ids.
            let session_workspaces: Vec<(String, String)> = db
                .prepare(
                    "SELECT id, name FROM workspaces WHERE (?1 = '*' OR session_id = ?1)
                      ORDER BY name",
                )
                .and_then(|mut st| {
                    st.query_map([sid], |r| Ok((r.get(0)?, r.get(1)?)))
                        .and_then(|m| m.collect())
                })
                .unwrap_or_default();
            let graph: Vec<Value> = session_workspaces
                .iter()
                .filter_map(|(ws_id, ws_name)| {
                    let ws_id = ws_id.as_str();
                    // Read, not recomputed. The view is built when the graph
                    // is stored; a graph stored by an older build has none
                    // yet, so the first read after an upgrade builds it once
                    // and keeps it.
                    let cached: Option<String> = db
                        .query_row(
                            "SELECT summary_json FROM code_graph_meta WHERE workspace_id = ?",
                            [ws_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    let mut view: Value = match cached.as_deref().map(serde_json::from_str) {
                        Some(Ok(v)) => v,
                        _ => {
                            let built = crate::graph::summarise(&db, ws_id, ws_name)?;
                            db.execute(
                                "UPDATE code_graph_meta SET summary_json = ? WHERE workspace_id = ?",
                                rusqlite::params![built.to_string(), ws_id],
                            )
                            .ok();
                            built
                        }
                    };
                    // The name lives with the workspace, not with the graph,
                    // so a rename does not need the graph parsed again.
                    view["workspace"] = json!(ws_name);
                    view["workspace_id"] = json!(ws_id);
                    Some(view)
                })
                .collect();

            json!({
                "session": session,
                "agents": agents,
                "messages": messages,
                "plan": plan,
                "knowledge": knowledge,
                "graph": graph,
                "locks": locks,
                "zones": zones,
                "workspaces": workspaces,
                "stats": {
                    "slots": agents.len(), "live": live, "working": working,
                    "awaiting_review": awaiting, "approved": approved,
                    "undelivered": undelivered, "unclaimed": unclaimed, "flapping": flapping,
                    "input_tokens": tin, "output_tokens": tout,
                },
            })
        }
    };

    Json(json!({
        "hub": { "version": env!("CARGO_PKG_VERSION"), "authenticated": authed,
                 "now_ms": now, "pair_listen": pair_listen, "pki_ready": pki_ready },
        "pairing": pairing,
        "nodes": nodes["nodes"],
        "sessions": sessions,
        "detail": detail,
    }))
}
