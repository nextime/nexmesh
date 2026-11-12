//! Pairing: turning a stranger into a member, without copying key material by
//! hand.
//!
//! An operator mints a short-lived, single-use code bound in advance to a name.
//! The far machine runs one command: it generates a key pair LOCALLY, sends
//! only a certificate request, and gets back a signed certificate, the CA
//! certificate and its token. The private key never travels.
//!
//! The code is the whole security boundary for that moment, so it is bound to
//! one identity, usable once, expires in minutes, and only its hash is stored.

use crate::state::{sha256_hex, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use nexmesh_proto::now_ms;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

type ApiResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.to_string() })))
}

/// Crockford-style base32 without look-alike characters, so a code can be read
/// aloud or retyped without 0/O or 1/I confusion.
fn code_text() -> String {
    use rand::Rng;
    const C: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut rng = rand::thread_rng();
    let raw: String = (0..16).map(|_| C[rng.gen_range(0..C.len())] as char).collect();
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join("-")
}

/// Compare ignoring case and dashes: the code is meant to be retyped.
fn normalise(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[derive(Deserialize)]
pub struct MintRequest {
    /// "node" or "operator".
    pub kind: String,
    /// The identity this code may claim, e.g. the node's name.
    pub name: String,
    #[serde(default)]
    pub ttl_minutes: Option<i64>,
}

pub async fn mint(State(st): State<AppState>, Json(req): Json<MintRequest>) -> ApiResult {
    if !["node", "operator"].contains(&req.kind.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "kind must be 'node' or 'operator'"));
    }
    let name = req.name.trim().to_string();
    // The name becomes a certificate subject and, for a node, a database
    // identity, so keep it to something unambiguous.
    if name.is_empty()
        || name.len() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1-64 characters of letters, digits, dot, dash or underscore",
        ));
    }
    let ttl = req.ttl_minutes.unwrap_or(15).clamp(1, 1440);
    let code = code_text();
    let id = format!("pc-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let now = now_ms();
    let expires = now + ttl * 60_000;
    {
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO pairing_codes (id, code_sha256, kind, name, issued_at_ms, expires_at_ms)
             VALUES (?,?,?,?,?,?)",
            params![id, sha256_hex(&normalise(&code)), req.kind, name, now, expires],
        )
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, action, detail) VALUES (?, 'operator', 'pair_code', ?)",
            params![now, json!({ "kind": req.kind, "name": name, "id": id }).to_string()],
        );
    }
    tracing::info!(kind = %req.kind, %name, ttl_minutes = ttl, "minted a pairing code");
    Ok(Json(json!({
        "id": id, "code": code, "kind": req.kind, "name": name,
        "expires_at_ms": expires, "ttl_minutes": ttl,
    })))
}

pub async fn list(State(st): State<AppState>) -> Json<Value> {
    let now = now_ms();
    let db = st.db.lock().unwrap();
    let Ok(mut stmt) = db.prepare(
        "SELECT id, kind, name, issued_at_ms, expires_at_ms, used_at_ms, used_from, revoked_at_ms
         FROM pairing_codes ORDER BY issued_at_ms DESC LIMIT 50",
    ) else {
        return Json(json!({ "codes": [] }));
    };
    // The code itself is never returned: it was shown once, at mint time.
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            let used: Option<i64> = r.get(5)?;
            let revoked: Option<i64> = r.get(7)?;
            let expires: i64 = r.get(4)?;
            Ok(json!({
                "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?, "issued_at_ms": r.get::<_, i64>(3)?,
                "expires_at_ms": expires, "used_at_ms": used,
                "used_from": r.get::<_, Option<String>>(6)?,
                "state": if revoked.is_some() { "revoked" }
                         else if used.is_some() { "used" }
                         else if expires < now { "expired" }
                         else { "open" },
            }))
        })
        .and_then(|m| m.collect())
        .unwrap_or_default();
    Json(json!({ "codes": rows }))
}

pub async fn revoke(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let db = st.db.lock().unwrap();
    let n = db
        .execute(
            "UPDATE pairing_codes SET revoked_at_ms = ?
             WHERE id = ? AND used_at_ms IS NULL AND revoked_at_ms IS NULL",
            params![now_ms(), id],
        )
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if n == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no open code with that id"));
    }
    Ok(Json(json!({ "revoked": id })))
}

#[derive(Deserialize)]
pub struct ClaimRequest {
    pub code: String,
    /// PEM certificate request, generated on the machine being paired.
    pub csr_pem: String,
}

/// Redeem a code. Unauthenticated by necessity -- the caller has no credential
/// yet; that is the entire point of pairing -- so this lives on its own
/// listener and the code must carry all the weight.
pub async fn claim(
    State(st): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> ApiResult {
    let pki_dir = st.pki_dir();
    let now = now_ms();
    let hash = sha256_hex(&normalise(&req.code));

    // Claim the code and mark it used in one statement, so two machines racing
    // on the same code cannot both succeed.
    let (id, kind, name) = {
        let db = st.db.lock().unwrap();
        let found: Option<(String, String, String)> = db
            .query_row(
                "SELECT id, kind, name FROM pairing_codes
                 WHERE code_sha256 = ? AND used_at_ms IS NULL AND revoked_at_ms IS NULL
                   AND expires_at_ms > ?",
                params![hash, now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        // Deliberately one message for every failure: a caller must not learn
        // whether a code exists, is expired or was already used.
        let (id, kind, name) =
            found.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "that pairing code is not usable"))?;
        let claimed = db
            .execute(
                "UPDATE pairing_codes SET used_at_ms = ? WHERE id = ? AND used_at_ms IS NULL",
                params![now, id],
            )
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        if claimed != 1 {
            return Err(err(StatusCode::UNAUTHORIZED, "that pairing code is not usable"));
        }
        (id, kind, name)
    };

    let cert = crate::pki::sign_csr(&pki_dir, &req.csr_pem, &name).map_err(|e| {
        // Give the code back: the failure was ours or the request's shape, not
        // an attempt to use it twice.
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE pairing_codes SET used_at_ms = NULL WHERE id = ?",
            [&id],
        );
        err(StatusCode::BAD_REQUEST, format!("{e:#}"))
    })?;
    let ca = crate::pki::ca_cert_pem(&pki_dir)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;

    // Mint the matching bearer credential, so one command fully provisions.
    let token = crate::state::random_token(48);
    {
        let db = st.db.lock().unwrap();
        let r = if kind == "node" {
            db.execute(
                "INSERT INTO node_tokens (token_sha256, node_name, issued_at_ms) VALUES (?,?,?)",
                params![sha256_hex(&token), name, now],
            )
        } else {
            db.execute(
                "INSERT INTO operator_tokens (token_sha256, label, issued_at_ms) VALUES (?,?,?)",
                params![sha256_hex(&token), name, now],
            )
        };
        r.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, action, detail) VALUES (?, ?, 'paired', ?)",
            params![now, name, json!({ "kind": kind, "code": id }).to_string()],
        );
    }
    tracing::info!(%kind, %name, "paired");

    Ok(Json(json!({
        "kind": kind, "name": name,
        "cert_pem": cert, "ca_cert_pem": ca, "token": token,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_avoid_look_alike_characters() {
        for _ in 0..50 {
            let c = code_text();
            assert_eq!(c.len(), 19, "{c}");
            assert_eq!(c.matches('-').count(), 3, "{c}");
            for ch in c.chars().filter(|c| *c != '-') {
                assert!(!"01IO".contains(ch), "{c} contains a look-alike character");
            }
        }
    }

    #[test]
    fn codes_are_not_predictable_between_calls() {
        let a: std::collections::HashSet<String> = (0..200).map(|_| code_text()).collect();
        assert_eq!(a.len(), 200, "generated a duplicate in 200 draws");
    }

    #[test]
    fn retyping_is_forgiving_about_case_and_dashes() {
        let c = code_text();
        assert_eq!(normalise(&c), normalise(&c.to_lowercase()));
        assert_eq!(normalise(&c), normalise(&c.replace('-', "")));
        assert_eq!(normalise(&c), normalise(&format!(" {c} ")));
        assert_ne!(normalise(&c), normalise(&code_text()));
    }
}
