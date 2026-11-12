//! Operator authentication.
//!
//! The operator API can create sessions, spawn processes on any node and
//! attach to any agent's terminal. It is therefore the most powerful surface
//! here, and it is gated by a middleware rather than by per-handler checks:
//! a route added later is protected by default instead of by remembering.
//!
//! Node and participant credentials are deliberately separate. A node token
//! cannot drive the fleet, and a participant token cannot spawn anything.

use crate::state::AppState;
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

pub async fn require_operator(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // With no operator tokens minted, the API is open. That is only allowed on
    // loopback, which `main` enforces at startup, so this cannot silently
    // expose a fleet.
    if !st.operator_auth_configured() {
        return next.run(req).await;
    }
    let Some(token) = bearer(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "operator authentication required; send 'Authorization: Bearer <token>' \
                          (mint one with `nexmesh-hub operator-token <label>`)"
            })),
        )
            .into_response();
    };
    match st.authenticate_operator(&token) {
        Some(label) => {
            tracing::debug!(operator = %label, path = %req.uri().path(), "authorised");
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
                rusqlite::params![crate::state::sha256_hex("s3cret"), "laptop", 1],
            )
            .unwrap();
        }
        assert!(st.operator_auth_configured());
        assert_eq!(st.authenticate_operator("s3cret").as_deref(), Some("laptop"));
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
