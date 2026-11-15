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

/// What build of the page this hub serves.
///
/// An installed PWA is the problem this solves: the page is `no-store` and
/// the service worker caches nothing, but a phone keeps the app open for
/// days and never reloads it, so an operator looks at a dashboard from two
/// deploys ago and reasonably concludes the fix was not deployed. The page
/// compares this with what the snapshot reports and offers to reload.
///
/// A hash of what is actually served, not the version number: the version
/// does not change between deploys of the same release, and the thing that
/// matters is whether THIS page differs from the one in front of you.
pub fn build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(PAGE.as_bytes());
        h.update(include_str!("../static/term.js").as_bytes());
        h.update(include_str!("../static/xr.html").as_bytes());
        h.update(include_str!("../static/xr.js").as_bytes());
        hex::encode(h.finalize())[..10].to_string()
    })
}

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
        // The page is told its own build so it can notice when the hub has
        // a newer one. One small allocation per load; the page is served
        // rarely and read for hours.
        Html(PAGE.replacen("__BUILD__", build_id(), 1)),
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
/// The terminal emulator the dashboard uses to attach to an agent.
///
/// Its own file rather than inline in the page, for the same reason the
/// service worker is: a VT parser is a parser and a page is a page. Served
/// from the hub like everything else, so a fleet with no route to the
/// internet still has a working terminal.
/// The room: the same fleet, as something you can stand in.
///
/// Served from the hub like everything else, with no CDN and no library, so
/// a headset on a network with no route to the internet still gets a scene.
pub async fn xr_page() -> Response {
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-store, must-revalidate"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Html(include_str!("../static/xr.html")),
    )
        .into_response()
}

pub async fn xr_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (axum::http::header::CACHE_CONTROL, "no-store, must-revalidate"),
        ],
        include_str!("../static/xr.js"),
    )
        .into_response()
}

pub async fn term_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            // Same reasoning as the page: it changes with the hub, and a
            // browser caching it heuristically would run an old parser
            // against a new one.
            (axum::http::header::CACHE_CONTROL, "no-store, must-revalidate"),
        ],
        include_str!("../static/term.js"),
    )
        .into_response()
}

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

/// Who could read a team message, and who has -- for the whole fleet, once.
///
/// The dashboard asks this of every message it shows, for every session it
/// shows, twice a second. Asked in SQL it is a correlated subquery over slots
/// and agents, re-run per message and three times over per row once
/// `delivered` is included: 0.42s of CPU per poll on the live hub, which with
/// two pollers was most of a core. Asked once, it is two small queries and a
/// pair of hash lookups.
struct Delivery {
    /// Seats that can read a team message, per session.
    eligible: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Which seats have acked each message. Counted at the point of asking
    /// over the seats that still count AND are not the sender -- otherwise a
    /// message read by a seat since removed reads `4/3`, and a sender that
    /// acked its own broadcast reads `2/1`.
    acked: std::collections::HashMap<i64, Vec<String>>,
}

impl Delivery {
    fn read(db: &rusqlite::Connection) -> Self {
        let mut eligible: std::collections::HashMap<String, std::collections::HashSet<String>> =
            Default::default();
        if let Ok(mut q) = db.prepare(&crate::seat::eligible_seats()) {
            if let Ok(rows) =
                q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            {
                for (session, slot) in rows.flatten() {
                    eligible.entry(session).or_default().insert(slot);
                }
            }
        }
        // Every ack, with the seat that gave it, so the count can be taken
        // over the seats that still count. There is one row per recipient per
        // message, which is the smallest table involved.
        let mut acked: std::collections::HashMap<i64, Vec<String>> = Default::default();
        if let Ok(mut q) = db.prepare(
            "SELECT r.message_id, r.slot_id, m.session_id
               FROM message_recipients r JOIN messages m ON m.id = r.message_id
              WHERE r.acked_at_ms IS NOT NULL",
        ) {
            if let Ok(rows) = q.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            }) {
                for (id, slot, session) in rows.flatten() {
                    if eligible.get(&session).is_some_and(|set| set.contains(&slot)) {
                        acked.entry(id).or_default().push(slot);
                    }
                }
            }
        }
        Self { eligible, acked }
    }

    /// How many seats a team message is for: everyone eligible except whoever
    /// sent it. The operator is not a seat, so nothing is subtracted for it.
    fn audience(&self, session: &str, from_slot: &str) -> i64 {
        let Some(set) = self.eligible.get(session) else { return 0 };
        let mut n = set.len() as i64;
        if !from_slot.is_empty() && set.contains(from_slot) {
            n -= 1;
        }
        n
    }

    /// How many of the seats it was for have seen it. Over the same seats the
    /// audience is counted over, or the two numbers disagree: the sender is
    /// not in the room it spoke to.
    fn seen(&self, id: i64, from_slot: &str) -> i64 {
        self.acked
            .get(&id)
            .map(|slots| slots.iter().filter(|s| s.as_str() != from_slot).count() as i64)
            .unwrap_or(0)
    }

    /// A message to one seat is its own business: delivered when acked. A team
    /// message is not delivered until the team has it, and a team nobody is in
    /// has not received anything.
    fn delivered(&self, id: i64, session: &str, to_slot: &str, from_slot: &str, acked: bool)
        -> bool
    {
        if !to_slot.is_empty() {
            return acked;
        }
        let a = self.audience(session, from_slot);
        a > 0 && self.seen(id, from_slot) >= a
    }
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
            // Seats, not windows: an adopted terminal has a slot so it can
            // be attached to, and counting it told the operator a team of
            // five was a team of ten.
            &format!(
                // `last_active_ms` is bumped when an agent calls a tool,
                // which misses a team whose last event was an operator
                // message or an agent being started -- so activity is
                // COMPUTED from what actually happened, the same way the API
                // listing computes it. Two orderings that disagree is how
                // the session you were just in ends up third on the phone
                // and first in the terminal.
                "SELECT s.id, s.name, s.status, s.created_at_ms,
                        MAX(s.last_active_ms, s.created_at_ms,
                            COALESCE((SELECT MAX(m.sent_at_ms) FROM messages m
                                       WHERE m.session_id = s.id), 0),
                            COALESCE((SELECT MAX(a.started_at_ms) FROM agents a
                                       JOIN slots y ON y.id = a.slot_id
                                      WHERE y.session_id = s.id), 0)) AS last_at_ms,
                        (SELECT COUNT(*) FROM slots x
                          WHERE x.session_id = s.id AND NOT {term}),
                        (SELECT COUNT(*) FROM slots x
                          WHERE x.session_id = s.id AND {term})
                 FROM sessions s ORDER BY last_at_ms DESC",
                term = crate::seat::is_terminal("x")
            ),
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                    "status": r.get::<_, String>(2)?, "created_at_ms": r.get::<_, i64>(3)?,
                    "last_active_ms": r.get::<_, i64>(4)?, "slots": r.get::<_, i64>(5)?,
                    "terminals": r.get::<_, i64>(6)?,
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
    // Who can read what, for the whole fleet, in two small queries. See
    // `Delivery`: this used to be a correlated subquery evaluated four times
    // per message shown.
    let delivery = Delivery::read(&db);
    let detail = match &sid {
        None => Value::Null,
        Some(sid) => {
            let agents: Vec<Value> = db
                .prepare(&format!(
                    "SELECT s.id, COALESCE(s.display_name, s.role, 'slot-' || s.id),
                            COALESCE(s.role, '-'), s.participant_kind, COALESCE(s.cli_id, '-'),
                            s.task_state, s.summary, s.status, s.restarts,
                            COALESCE(sx.name, ''),
                            COALESCE(a.provenance, '-'), {word},
                            COALESCE(n.name, CASE WHEN a.provenance='external' THEN 'external'
                                                  ELSE '-' END),
                            COALESCE(a.input_tokens,0), COALESCE(a.output_tokens,0),
                            COALESCE(a.cache_read_tokens,0), COALESCE(a.push_capable,0),
                            a.id, s.mode, s.paused, a.pid,
                            COALESCE(a.io_kind, CASE WHEN s.terminal_via IS NOT NULL
                                                     THEN 'tmux' END),
                            COALESCE(s.capabilities, '[]'),
                            -- The agent an attach should open for this seat:
                            -- its own process if that has a terminal, else
                            -- the adopted terminal it sits in.
                            COALESCE(
                                CASE WHEN a.io_kind IS NULL THEN
                                    (SELECT v.id FROM agents v
                                      WHERE v.id = s.terminal_via
                                        AND v.exited_at_ms IS NULL)
                                END, a.id),
                            s.voice
                     FROM slots s
                     LEFT JOIN sessions sx ON sx.id = s.session_id
                     LEFT JOIN agents a ON a.id = (
                        SELECT id FROM agents WHERE slot_id = s.id
                        ORDER BY (exited_at_ms IS NULL) DESC, (io_kind IS NOT NULL) DESC, started_at_ms DESC LIMIT 1)
                     LEFT JOIN nodes n ON n.id = a.node_id
                     WHERE (?1 = '*' OR s.session_id = ?1)
                       -- Adopted terminals are not teammates: they are the
                       -- screens other seats are sitting in, and every seat
                       -- they serve carries `attach_agent_id` so the terminal
                       -- button still opens them. Listing them doubled the
                       -- agent count on the dashboard with rows nobody can
                       -- talk to.
                       AND COALESCE(s.cli_id,'') <> 'tmux' AND s.tmux_target IS NULL
                     ORDER BY 2",
                    word = crate::seat::agent_word("s", "a"),
                ))
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
                            // What this seat says it can be asked for, as a
                            // list. The dashboard used to look for the label
                            // inside the seat's summary PROSE to decide
                            // whether to offer the microphone, which made a
                            // phone's voice input depend on a sentence.
                            "capabilities": serde_json::from_str::<Value>(
                                &r.get::<_, String>(22)?).unwrap_or(json!([])),
                            "attach_agent_id": r.get::<_, Option<String>>(23)?,
                            // Which voice reads this one out, when the fleet
                            // is being listened to rather than read.
                            "voice": r.get::<_, Option<String>>(24)?,
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
                            COALESCE(m.from_slot, ''), COALESCE(m.to_slot, ''),
                            m.session_id,
                            m.attachment, u.name, u.media_type, u.bytes, u.node_name
                     FROM messages m
                     LEFT JOIN slots sf ON sf.id = m.from_slot
                     LEFT JOIN slots st2 ON st2.id = m.to_slot
                     LEFT JOIN sessions mx ON mx.id = m.session_id
                     LEFT JOIN uploads u ON u.id = m.attachment
                     WHERE (?1 = '*' OR m.session_id = ?1) ORDER BY m.id DESC LIMIT 120",
                )
                .and_then(|mut stmt| {
                    stmt.query_map([sid], |r| {
                        let id: i64 = r.get(0)?;
                        let acked: bool = r.get(6)?;
                        let from_slot: String = r.get(11)?;
                        let to_slot: String = r.get(12)?;
                        let msg_session: String = r.get(13)?;
                        Ok(json!({
                            "id": id, "at_ms": r.get::<_, i64>(1)?,
                            "kind": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                            "from": r.get::<_, String>(4)?, "to": r.get::<_, String>(5)?,
                            "delivered": delivery.delivered(
                                id, &msg_session, &to_slot, &from_slot, acked),
                            "in_flight": r.get::<_, bool>(7)?,
                            "session": r.get::<_, String>(8)?,
                            "claimed": r.get::<_, bool>(9)?,
                            "claimed_by": r.get::<_, String>(10)?,
                            "from_slot": from_slot,
                            "to_slot": to_slot,
                            "seen": delivery.seen(id, &from_slot),
                            "audience": delivery.audience(&msg_session, &from_slot),
                            // The file this message is about, so the chat can
                            // show it rather than name a path on a machine the
                            // reader may not even be able to reach.
                            "file": match r.get::<_, Option<String>>(14)? {
                                Some(up) => json!({
                                    "id": up,
                                    "name": r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                                    "media_type":
                                        r.get::<_, Option<String>>(16)?.unwrap_or_default(),
                                    "bytes": r.get::<_, Option<i64>>(17)?.unwrap_or(0),
                                    "node": r.get::<_, Option<String>>(18)?.unwrap_or_default(),
                                    "url": format!("/v1/uploads/{}",
                                                   r.get::<_, String>(14)?),
                                }),
                                None => Value::Null,
                            },
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
                 "now_ms": now, "pair_listen": pair_listen, "pki_ready": pki_ready,
                 // Which dashboard this hub would serve right now. A page
                 // left open on a phone compares it with its own.
                 "build": build_id() },
        "pairing": pairing,
        "nodes": nodes["nodes"],
        "sessions": sessions,
        "detail": detail,
    }))
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use rusqlite::{params, Connection};

    /// Two seats that can read, one corpse that cannot, and one adopted
    /// terminal that is a window rather than a teammate.
    fn room() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        crate::state::migrate(&db).unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,
                                lease_expires_ms)
             VALUES ('nd-1','machine-a','linux','x86_64','0.1.0','online',1,1,9999999999999)",
            [],
        )
        .unwrap();
        let now = nexmesh_proto::now_ms();
        for (slot, name) in [("sl-a", "Ann"), ("sl-b", "Bob")] {
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
                 VALUES (?,'s1',?,'claude',1)",
                params![slot, name],
            )
            .unwrap();
            db.execute(
                "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,
                                     last_seen_ms)
                 VALUES (?,?,'nd-1','attached','running',?,?)",
                params![format!("ag-{slot}"), slot, now, now],
            )
            .unwrap();
        }
        // A window onto Ann's pane: a seat, with a process, and not a mind.
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,cli_id,tmux_target,created_at_ms)
             VALUES ('sl-term','s1','term-Ann','tmux','nexmesh-ann',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag-term','sl-term','nd-1','managed','running',?,?)",
            params![now, now],
        )
        .unwrap();
        db
    }

    fn say(db: &Connection, id: i64, from: Option<&str>, to: Option<&str>) {
        db.execute(
            "INSERT INTO messages (id,session_id,from_slot,to_slot,kind,body,sent_at_ms)
             VALUES (?,'s1',?,?,'text','hello',?)",
            params![id, from, to, nexmesh_proto::now_ms()],
        )
        .unwrap();
    }

    fn ack(db: &Connection, id: i64, slot: &str) {
        db.execute(
            "INSERT INTO message_recipients (message_id,slot_id,acked_at_ms) VALUES (?,?,?)",
            params![id, slot, nexmesh_proto::now_ms()],
        )
        .unwrap();
    }

    #[test]
    fn a_broadcast_is_for_everyone_but_the_seat_that_sent_it() {
        let db = room();
        say(&db, 1, Some("sl-a"), None);
        let d = Delivery::read(&db);
        assert_eq!(d.audience("s1", "sl-a"), 1, "Bob, and not the terminal");
        assert_eq!(d.audience("s1", ""), 2, "from the operator: both of them");
    }

    #[test]
    fn the_sender_acking_its_own_broadcast_changes_nothing() {
        // `2/1` on the live dashboard: the sender's own ack was counted
        // against an audience it was not in.
        let db = room();
        say(&db, 1, Some("sl-a"), None);
        ack(&db, 1, "sl-a");
        let d = Delivery::read(&db);
        assert_eq!(d.seen(1, "sl-a"), 0, "nobody it was for has seen it yet");
        assert!(!d.delivered(1, "s1", "", "sl-a", false));
        ack(&db, 1, "sl-b");
        let d = Delivery::read(&db);
        assert_eq!((d.seen(1, "sl-a"), d.audience("s1", "sl-a")), (1, 1));
        assert!(d.delivered(1, "s1", "", "sl-a", false), "now the team has it");
    }

    #[test]
    fn a_message_to_one_seat_is_delivered_when_that_seat_acks() {
        let db = room();
        say(&db, 1, Some("sl-a"), Some("sl-b"));
        let d = Delivery::read(&db);
        assert!(!d.delivered(1, "s1", "sl-b", "sl-a", false));
        assert!(d.delivered(1, "s1", "sl-b", "sl-a", true));
    }

    #[test]
    fn an_ack_from_a_seat_that_has_gone_cannot_push_the_count_past_the_room() {
        let db = room();
        say(&db, 1, None, None);
        ack(&db, 1, "sl-a");
        ack(&db, 1, "sl-b");
        ack(&db, 1, "sl-gone");
        let d = Delivery::read(&db);
        assert_eq!((d.seen(1, ""), d.audience("s1", "")), (2, 2));
    }

    #[test]
    fn a_team_nobody_is_in_has_not_received_anything() {
        let db = room();
        db.execute("UPDATE slots SET task_state = 'released'", []).unwrap();
        say(&db, 1, None, None);
        let d = Delivery::read(&db);
        assert_eq!(d.audience("s1", ""), 0);
        assert!(!d.delivered(1, "s1", "", "", false));
    }
}
