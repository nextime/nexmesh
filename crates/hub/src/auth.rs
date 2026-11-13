//! Operator authentication.
//!
//! The operator API can create sessions, spawn processes on any node and
//! attach to any agent's terminal. It is therefore the most powerful surface
//! here, and it is gated by a middleware rather than by per-handler checks:
//! a route added later is protected by default instead of by remembering.
//!
//! Node and participant credentials are deliberately separate. A node token
//! cannot drive the fleet, and a participant token cannot spawn anything.
//!
//! Where mutual TLS is in use the token is checked against the certificate
//! that carried it, so the two must name the same operator. See `peer.rs`.

use crate::state::AppState;
use rusqlite::OptionalExtension;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

/// Pull a bearer token from the Authorization header.
///
/// Header only, never a query parameter: URLs end up in proxy logs, shell
/// history and `ps` output, and this token is equivalent to shell access on
/// every node in the fleet.
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let (scheme, value) = raw.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| value.trim().to_string())
}

/// How far a scoped operator may reach, decided from the path it is asking for.
///
/// In the middleware rather than in each handler, for the reason this module
/// already gives: a route added later is covered without anyone having to
/// remember. A scoped orchestrator drives ONE session, so it may not name
/// another, may not touch a seat belonging to another, and may not do the
/// things that are about the fleet rather than about a team -- creating a
/// session, listing machines, moving a seat between two sessions.
///
/// Returns Err with the reason when the request is out of reach.
fn within_scope(st: &AppState, op: &crate::state::Operator, req: &Request) -> Result<(), String> {
    let Some(scope) = op.session.clone() else { return Ok(()) };
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    // Fleet-wide surfaces: a scoped orchestrator has no business here.
    let fleet_only = matches!(
        seg.as_slice(),
        ["v1", "nodes", ..] | ["v1", "certs", ..] | ["v1", "revoke", ..] | ["v1", "pair", ..]
    );
    if fleet_only {
        return Err(format!(
            "this token is scoped to one session, and {path} is about the whole fleet; \
             ask a super orchestrator"
        ));
    }
    if seg.as_slice() == ["v1", "sessions"] && method == axum::http::Method::POST {
        return Err("creating a session needs a super orchestrator's token".to_string());
    }

    let db = st.db.lock().unwrap();
    // /v1/sessions/<ref>/...
    if let ["v1", "sessions", given, ..] = seg.as_slice() {
        let id: Option<String> = db
            .query_row(
                "SELECT id FROM sessions WHERE id = ?1 OR name = ?1 ORDER BY (id = ?1) DESC LIMIT 1",
                [given],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        // An unknown name is the handler's to answer, not ours to leak a
        // judgement about: saying "out of scope" would tell a scoped token
        // that some other session has that name.
        if let Some(id) = id {
            if id != scope {
                return Err("that session is not the one this token drives".to_string());
            }
        }
        return Ok(());
    }
    // /v1/slots/<id>/... -- a seat belongs to a session, so the seat decides.
    if let ["v1", "slots", slot, rest @ ..] = seg.as_slice() {
        let owner: Option<String> = db
            .query_row("SELECT session_id FROM slots WHERE id = ?", [slot], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        if let Some(owner) = owner {
            if owner != scope {
                return Err("that seat is in a session this token does not drive".to_string());
            }
        }
        // Moving a seat BETWEEN sessions is how the fleet is arranged, and
        // the destination is by definition somewhere this token cannot see.
        if rest.first() == Some(&"move") {
            return Err(
                "moving a seat to another session needs a super orchestrator's token".to_string()
            );
        }
        return Ok(());
    }
    Ok(())
}

pub async fn require_operator(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // The certificate that carried this request, where there was one -- or
    // the one a trusted proxy verified on our behalf.
    let peer = crate::peer::resolve(&st, &req);
    // With no operator tokens minted, the API is open. That is only allowed on
    // loopback, which `main` enforces at startup, so this cannot silently
    // expose a fleet.
    if !st.operator_auth_configured() {
        let mut req = req;
        req.extensions_mut()
            .insert(std::sync::Arc::new(crate::state::Operator::super_as("anonymous")));
        return next.run(req).await;
    }
    // A signed-in dashboard session counts. The account authenticates a
    // person; a bearer token is what a script uses. Either is an operator,
    // and the identity it claims is the username or the token's label.
    if bearer(&headers).is_none() {
        if let Some(c) = crate::webauth::cookie_value(&headers, crate::webauth::COOKIE) {
            let who = {
                let db = st.db.lock().unwrap();
                crate::webauth::whoami(&db, &c)
            };
            if let Some(user) = who {
                if st.identity_binding() {
                    if let Err(why) = peer.agrees_with(&user) {
                        tracing::warn!(%user, path = %req.uri().path(),
                            "rejected: certificate does not match the signed-in account");
                        return (StatusCode::FORBIDDEN,
                                Json(serde_json::json!({ "error": why }))).into_response();
                    }
                }
                tracing::debug!(%user, path = %req.uri().path(), "authorised by session");
                // A person signed in to the dashboard sees the whole fleet.
                let mut req = req;
                req.extensions_mut()
                    .insert(std::sync::Arc::new(crate::state::Operator::super_as(&user)));
                return next.run(req).await;
            }
        }
    }
    let Some(token) = bearer(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                // Two audiences read this: a person in a browser and a
                // script holding a token. It names both ways in, and keeps
                // the wording a CLI user already knows.
                "error": "operator authentication required: sign in to the dashboard, or send \
                          'Authorization: Bearer <token>' (mint one with `nexmesh-hub \
                          operator-token <label>`)"
            })),
        )
            .into_response();
    };
    match st.operator_identity(&token) {
        Some(op) => {
            let label = op.label.clone();
            // A valid token is not enough under mutual TLS: it has to have
            // arrived on that operator's own connection.
            if st.identity_binding() {
                if let Err(why) = peer.agrees_with(&label) {
                    tracing::warn!(
                        operator = %label, certificate = ?peer.common_name,
                        path = %req.uri().path(), "rejected: certificate does not match the token"
                    );
                    return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": why })))
                        .into_response();
                }
            }
            // A scoped token reaches one session and no further.
            if let Err(why) = within_scope(&st, &op, &req) {
                tracing::warn!(operator = %label, scope = ?op.session,
                    path = %req.uri().path(), "rejected: out of scope");
                return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": why })))
                    .into_response();
            }
            tracing::debug!(operator = %label, scope = ?op.session,
                path = %req.uri().path(), "authorised");
            // Handlers that list things have to filter by reach, and a
            // handler cannot ask the token: it never sees the header.
            let mut req = req;
            req.extensions_mut().insert(std::sync::Arc::new(op));
            next.run(req).await
        }
        None => {
            tracing::warn!(path = %req.uri().path(), "rejected an invalid operator token");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid or revoked operator token" })),
            )
                .into_response()
        }
    }
}

/// Bind a participant's certificate to the token it sent, the same way the
/// operator middleware does.
///
/// A group middleware rather than a check inside each handler, for the reason
/// given above: a participant route added later is covered without anyone
/// having to remember. It settles identity only -- what the token is *allowed*
/// to do, and which session it is scoped to, stays with `auth_participant`.
pub async fn bind_participant(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    if !st.identity_binding() {
        return next.run(req).await;
    }
    let peer = crate::peer::resolve(&st, &req);
    // No certificate, or no token yet: nothing to bind. An absent or invalid
    // token is the handler's business, and it gives the better message.
    if !peer.presented {
        return next.run(req).await;
    }
    if let Some(token) = bearer(&headers) {
        if let Some(label) = st.participant_label(&token) {
            if let Err(why) = peer.agrees_with(&label) {
                tracing::warn!(
                    participant = %label, certificate = ?peer.common_name,
                    path = %req.uri().path(), "rejected: certificate does not match the token"
                );
                return (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": why })))
                    .into_response();
            }
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn parses_a_bearer_token_case_insensitively() {
        assert_eq!(bearer(&headers("Bearer abc123")).as_deref(), Some("abc123"));
        assert_eq!(bearer(&headers("bearer abc123")).as_deref(), Some("abc123"));
        assert_eq!(bearer(&headers("BEARER abc123")).as_deref(), Some("abc123"));
    }

    #[test]
    fn ignores_other_schemes_and_junk() {
        assert!(bearer(&headers("Basic abc123")).is_none());
        assert!(bearer(&headers("abc123")).is_none());
        assert!(bearer(&HeaderMap::new()).is_none());
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(bearer(&headers("Bearer   abc123  ")).as_deref(), Some("abc123"));
    }

    #[test]
    fn an_operator_token_is_verified_by_hash_and_can_be_revoked() {
        let st = AppState::in_memory().unwrap();
        assert!(!st.operator_auth_configured(), "fresh hub has no operator tokens");
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO operator_tokens (token_sha256, label, issued_at_ms) VALUES (?,?,?)",
                rusqlite::params![crate::state::sha256_hex("s3cret"), "machine-b", 1],
            )
            .unwrap();
        }
        assert!(st.operator_auth_configured());
        assert_eq!(st.authenticate_operator("s3cret").as_deref(), Some("machine-b"));
        assert!(st.authenticate_operator("wrong").is_none());
        {
            let db = st.db.lock().unwrap();
            db.execute("UPDATE operator_tokens SET revoked_at_ms = 2", []).unwrap();
        }
        assert!(st.authenticate_operator("s3cret").is_none(), "revoked must stop working");
        // The important part: revoking the LAST token must not disable
        // authentication. Otherwise revoking a leaked token -- exactly what you
        // do in an emergency -- would open the fleet to anyone.
        assert!(
            st.operator_auth_configured(),
            "revoking the last token must NOT turn authentication off"
        );
    }

    #[test]
    fn a_hub_that_never_had_a_token_is_open_but_one_that_did_is_not() {
        let st = AppState::in_memory().unwrap();
        assert!(!st.operator_auth_configured(), "a fresh hub is open (loopback only)");
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO operator_tokens (token_sha256, label, issued_at_ms, revoked_at_ms)
                 VALUES (?,?,?,?)",
                rusqlite::params![crate::state::sha256_hex("gone"), "old", 1, 2],
            )
            .unwrap();
        }
        assert!(
            st.operator_auth_configured(),
            "a hub whose only token is revoked still requires authentication"
        );
        assert!(st.authenticate_operator("gone").is_none());
    }
}
