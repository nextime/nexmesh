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

    let sessions: Vec<Value> = db
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

    // Default to the most recently active session so the page is useful with
    // no query string at all.
    let sid = q
        .session
        .filter(|s| !s.is_empty())
        .or_else(|| sessions.first().and_then(|s| s["id"].as_str().map(String::from)));

    let detail = match &sid {
        None => Value::Null,
        Some(sid) => {
            let agents: Vec<Value> = db
                .prepare(
                    "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id),
                            COALESCE(s.role, '-'), s.participant_kind, COALESCE(s.cli_id, '-'),
                            s.task_state, s.summary, s.status, s.restarts,
                            COALESCE(a.provenance, '-'), COALESCE(a.status, 'disconnected'),
                            COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external'
                                                  ELSE '-' END),
                            COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0),
                            COALESCE(a.cache_read_tokens,0), COALESCE(a.push_capable,0),
                            a.id, s.mode, s.paused
                     FROM slots s
                     LEFT JOIN agents a ON a.id = (
                        SELECT id FROM agents WHERE slot_id = s.id
                        ORDER BY (exited_at_ms IS NULL) DESC, started_at_ms DESC LIMIT 1)
                     LEFT JOIN nodes n ON n.id = a.node_id
                     WHERE s.session_id = ? ORDER BY 2",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "slot_id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                            "role": r.get::<_, String>(2)?, "kind": r.get::<_, String>(3)?,
                            "cli": r.get::<_, String>(4)?, "task_state": r.get::<_, String>(5)?,
                            "summary": r.get::<_, String>(6)?, "slot_status": r.get::<_, String>(7)?,
                            "restarts": r.get::<_, i64>(8)?, "provenance": r.get::<_, String>(9)?,
                            "status": r.get::<_, String>(10)?, "host": r.get::<_, String>(11)?,
                            "input_tokens": r.get::<_, i64>(12)?, "output_tokens": r.get::<_, i64>(13)?,
                            "cache_read_tokens": r.get::<_, i64>(14)?,
                            "push_capable": r.get::<_, i64>(15)? == 1,
                            "agent_id": r.get::<_, Option<String>>(16)?,
                            "mode": r.get::<_, String>(17)?,
                            "paused": r.get::<_, i64>(18)? == 1,
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
                            m.acked_at_ms IS NOT NULL, m.inflight_at_ms IS NOT NULL
                     FROM messages m
                     LEFT JOIN slots sf ON sf.id = m.from_slot
                     LEFT JOIN slots st2 ON st2.id = m.to_slot
                     WHERE m.session_id = ? ORDER BY m.id DESC LIMIT 120",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        Ok(json!({
                            "id": r.get::<_, i64>(0)?, "at_ms": r.get::<_, i64>(1)?,
                            "kind": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                            "from": r.get::<_, String>(4)?, "to": r.get::<_, String>(5)?,
                            "delivered": r.get::<_, bool>(6)?, "in_flight": r.get::<_, bool>(7)?,
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
                     WHERE p.session_id = ? ORDER BY p.ord",
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
                     WHERE (m.scope = 'session' AND m.scope_ref = ?1)
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
                     WHERE l.session_id = ? ORDER BY l.scope, l.path",
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
                     WHERE w.session_id = ? ORDER BY w.name, o.pattern",
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
                     WHERE w.session_id = ? ORDER BY w.name",
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
                    "SELECT id, name, status, created_at_ms FROM sessions WHERE id = ?",
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
                .unwrap_or(Value::Null);

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
                .prepare("SELECT id, name FROM workspaces WHERE session_id = ? ORDER BY name")
                .and_then(|mut st| {
                    st.query_map([sid], |r| Ok((r.get(0)?, r.get(1)?)))
                        .and_then(|m| m.collect())
                })
                .unwrap_or_default();
            let graph: Vec<Value> = session_workspaces
                .iter()
                .filter_map(|(ws_id, ws_name)| {
                    let ws_id = ws_id.as_str();
                    let (built, files, truncated, communities): (i64, i64, i64, i64) = db
                        .query_row(
                            "SELECT built_at_ms, files, truncated, communities
                               FROM code_graph_meta WHERE workspace_id = ?",
                            [ws_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        )
                        .optional()
                        .ok()
                        .flatten()?;
                    let counts: Vec<Value> = db
                        .prepare(
                            "SELECT kind, COUNT(*) FROM code_graph_nodes
                              WHERE workspace_id = ? GROUP BY kind ORDER BY 2 DESC",
                        )
                        .and_then(|mut st| {
                            st.query_map([ws_id], |r| {
                                Ok(json!({ "kind": r.get::<_, String>(0)?,
                                           "n": r.get::<_, i64>(1)? }))
                            })
                            .and_then(|m| m.collect())
                        })
                        .unwrap_or_default();
                    // Largest first, with a few recognisable members each:
                    // a list of numbered communities tells a reader nothing.
                    let subsystems: Vec<Value> = db
                        .prepare(
                            "SELECT community, COUNT(*) FROM code_graph_nodes
                              WHERE workspace_id = ? AND community IS NOT NULL
                              GROUP BY community ORDER BY 2 DESC LIMIT 24",
                        )
                        .and_then(|mut st| {
                            st.query_map([ws_id], |r| {
                                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                            })
                            .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(c, n)| {
                            let members: Vec<Value> = db
                                .prepare(
                                    "SELECT name, path, kind FROM code_graph_nodes
                                      WHERE workspace_id = ? AND community = ?
                                      ORDER BY CASE kind WHEN 'type' THEN 0
                                                         WHEN 'function' THEN 1 ELSE 2 END,
                                               name LIMIT 8",
                                )
                                .and_then(|mut st| {
                                    st.query_map(rusqlite::params![ws_id, c], |r| {
                                        Ok(json!({ "name": r.get::<_, String>(0)?,
                                                   "path": r.get::<_, String>(1)?,
                                                   "kind": r.get::<_, String>(2)? }))
                                    })
                                    .and_then(|m| m.collect())
                                })
                                .unwrap_or_default();
                            json!({ "id": c, "n": n, "members": members })
                        })
                        .collect();
                    Some(json!({
                        "workspace": ws_name,
                        "workspace_id": ws_id,
                        "built_at_ms": built,
                        "files": files,
                        "truncated": truncated != 0,
                        "communities": communities,
                        "counts": counts,
                        "subsystems": subsystems,
                    }))
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
                    "undelivered": undelivered, "flapping": flapping,
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
