//! Signing in to the dashboard with a username and a password.
//!
//! The dashboard used to ask for an operator token pasted into the browser.
//! That works, but it means handling a fleet-wide credential by hand every
//! time, and a token good for the whole API is a poor thing to leave in
//! `localStorage`. An account authenticates a *person*; operator tokens stay
//! what a script or a CLI uses.
//!
//! ## The first account
//!
//! Whoever arrives first must not simply be able to claim the hub -- this one
//! may answer on a public address. So creating the first account needs a
//! **setup code** that the hub prints to its own log: bound in advance, single
//! use, and readable only by someone who already has access to the host. The
//! same reasoning as a pairing code, for the same reason.
//!
//! ## What is stored
//!
//! Argon2id for passwords, never a bare digest. For sessions, only the hash
//! of the cookie value, so the table is not a list of working credentials --
//! a dump of it cannot be replayed.
//!
//! ## Wrong passwords cost something
//!
//! A public login form invites guessing, so failures count and an account
//! locks for a while. Locking the account rather than the address, because an
//! address is cheap to change and this is a handful of human accounts, not a
//! consumer service.

use anyhow::{bail, Result};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

/// How long a browser session lasts without being used.
pub const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1000;
/// Failures before an account stops accepting attempts.
pub const MAX_FAILURES: i64 = 8;
/// How long it then refuses them.
pub const LOCKOUT_MS: i64 = 15 * 60 * 1000;
pub const COOKIE: &str = "nexmesh_session";
/// Short enough to type from a terminal, long enough not to be guessed.
const SETUP_CODE_BYTES: usize = 24;

fn sha256_hex(s: &str) -> String {
    crate::state::sha256_hex(s)
}

/// The same generator every other credential here uses.
fn random_token(len: usize) -> String {
    crate::state::random_token(len)
}

/// Has anyone set up an account yet?
pub fn configured(db: &Connection) -> bool {
    db.query_row("SELECT 1 FROM dashboard_users LIMIT 1", [], |_| Ok(true))
        .optional()
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Mint (or replace) the code that authorises creating the first account.
///
/// Returned once, to be printed. Only its hash is kept.
pub fn issue_setup_code(db: &Connection) -> Result<String> {
    if configured(db) {
        bail!(
            "this hub already has a dashboard account; a setup code would do nothing. Reset a \
             password with `nexmesh-hub dashboard-passwd <user>`."
        );
    }
    let code = random_token(SETUP_CODE_BYTES);
    db.execute(
        "INSERT INTO dashboard_setup (id, code_sha256, issued_at_ms, used_at_ms)
         VALUES (1, ?1, ?2, NULL)
         ON CONFLICT(id) DO UPDATE SET code_sha256 = ?1, issued_at_ms = ?2, used_at_ms = NULL",
        params![sha256_hex(&code), now_ms()],
    )?;
    Ok(code)
}

fn check_password_quality(password: &str) -> Result<()> {
    // Deliberately a length floor and nothing else. Composition rules push
    // people towards "Password1!" and this is a page on someone's own
    // infrastructure, not a bank.
    if password.chars().count() < 12 {
        bail!("a password needs at least 12 characters");
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    // The salt is chosen per password by the implementation, which is what
    // makes two accounts with one password store differently.
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing the password: {e}"))?
        .to_string())
}

fn valid_username(name: &str) -> Result<String> {
    let n = name.trim().to_lowercase();
    if n.len() < 2 || n.len() > 32 {
        bail!("a username must be 2 to 32 characters");
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("a username may use letters, digits, dot, dash and underscore");
    }
    Ok(n)
}

/// Create the first account, against the setup code.
pub fn setup_first(db: &Connection, code: &str, username: &str, password: &str) -> Result<String> {
    if configured(db) {
        bail!("this hub already has a dashboard account");
    }
    let row: Option<(String, Option<i64>)> = db
        .query_row(
            "SELECT code_sha256, used_at_ms FROM dashboard_setup WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((expected, used)) = row else {
        bail!(
            "this hub has not issued a setup code. Run `nexmesh-hub dashboard-setup-code` on the \
             host and use the code it prints."
        );
    };
    if used.is_some() {
        bail!("that setup code has already been used");
    }
    // Same message whatever was wrong, so the form cannot be used to learn
    // whether a code exists.
    if sha256_hex(code.trim()) != expected {
        bail!("that setup code is not right");
    }
    let name = valid_username(username)?;
    check_password_quality(password)?;
    let now = now_ms();
    db.execute(
        "INSERT INTO dashboard_users (username, password_hash, created_at_ms)
         VALUES (?,?,?)",
        params![name, hash_password(password)?, now],
    )?;
    db.execute("UPDATE dashboard_setup SET used_at_ms = ? WHERE id = 1", [now])?;
    db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![now, name, "dashboard_account_created", "{}"],
    )
    .ok();
    Ok(name)
}

/// Add another account. Only an existing signed-in user may.
pub fn add_user(db: &Connection, username: &str, password: &str) -> Result<String> {
    let name = valid_username(username)?;
    check_password_quality(password)?;
    if db
        .query_row("SELECT 1 FROM dashboard_users WHERE username = ?", [&name], |_| Ok(true))
        .optional()?
        .is_some()
    {
        bail!("there is already an account called '{name}'");
    }
    db.execute(
        "INSERT INTO dashboard_users (username, password_hash, created_at_ms) VALUES (?,?,?)",
        params![name, hash_password(password)?, now_ms()],
    )?;
    Ok(name)
}

/// Change a password, which also clears any lockout.
pub fn set_password(db: &Connection, username: &str, password: &str) -> Result<()> {
    let name = valid_username(username)?;
    check_password_quality(password)?;
    let n = db.execute(
        "UPDATE dashboard_users
            SET password_hash = ?, failed_count = 0, locked_until_ms = NULL
          WHERE username = ?",
        params![hash_password(password)?, name],
    )?;
    if n == 0 {
        bail!("no account called '{name}'");
    }
    // Every session for that account stops working: a password change that
    // leaves old sessions alive does not lock anyone out.
    db.execute("DELETE FROM dashboard_sessions WHERE username = ?", [&name])?;
    Ok(())
}

/// Check a password and open a session. Returns the cookie value.
pub fn login(db: &Connection, username: &str, password: &str) -> Result<(String, i64)> {
    let name = valid_username(username).map_err(|_| wrong())?;
    let row: Option<(String, i64, Option<i64>)> = db
        .query_row(
            "SELECT password_hash, failed_count, locked_until_ms FROM dashboard_users
              WHERE username = ?",
            [&name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let now = now_ms();
    let Some((hash, failures, locked)) = row else {
        // An unknown username still pays for a hash. Answering instantly
        // would tell the caller the account does not exist, which is the one
        // thing the identical error message is there to avoid.
        let _ = Argon2::default().hash_password(password.as_bytes());
        return Err(wrong());
    };
    if let Some(until) = locked {
        if until > now {
            let mins = ((until - now) / 60_000).max(1);
            bail!("too many failed attempts; this account is locked for another {mins} minute(s)");
        }
    }
    if Argon2::default().verify_password(password.as_bytes(), hash.as_str()).is_err() {
        let failures = failures + 1;
        let lock = if failures >= MAX_FAILURES { Some(now + LOCKOUT_MS) } else { None };
        db.execute(
            "UPDATE dashboard_users SET failed_count = ?, locked_until_ms = ? WHERE username = ?",
            params![failures, lock, name],
        )?;
        tracing::warn!(user = %name, failures, "dashboard login failed");
        return Err(wrong());
    }
    db.execute(
        "UPDATE dashboard_users
            SET failed_count = 0, locked_until_ms = NULL, last_login_ms = ?
          WHERE username = ?",
        params![now, name],
    )?;
    let token = random_token(32);
    db.execute(
        "INSERT INTO dashboard_sessions
            (id_sha256, username, created_at_ms, last_seen_ms, expires_at_ms)
         VALUES (?,?,?,?,?)",
        params![sha256_hex(&token), name, now, now, now + SESSION_TTL_MS],
    )?;
    tracing::info!(user = %name, "dashboard login");
    Ok((token, now + SESSION_TTL_MS))
}

fn wrong() -> anyhow::Error {
    // One message for a bad username and a bad password alike: the form must
    // not report which accounts exist.
    anyhow::anyhow!("that username and password do not match")
}

/// Who a session cookie belongs to, refreshing its expiry.
pub fn whoami(db: &Connection, cookie_value: &str) -> Option<String> {
    let now = now_ms();
    let row: Option<(String, i64)> = db
        .query_row(
            "SELECT username, expires_at_ms FROM dashboard_sessions WHERE id_sha256 = ?",
            [sha256_hex(cookie_value)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    let (user, expires) = row?;
    if expires <= now {
        let _ = db.execute(
            "DELETE FROM dashboard_sessions WHERE id_sha256 = ?",
            [sha256_hex(cookie_value)],
        );
        return None;
    }
    // Sliding expiry: being used keeps it alive, idleness ends it.
    let _ = db.execute(
        "UPDATE dashboard_sessions SET last_seen_ms = ?, expires_at_ms = ? WHERE id_sha256 = ?",
        params![now, now + SESSION_TTL_MS, sha256_hex(cookie_value)],
    );
    Some(user)
}

pub fn logout(db: &Connection, cookie_value: &str) {
    let _ = db.execute(
        "DELETE FROM dashboard_sessions WHERE id_sha256 = ?",
        [sha256_hex(cookie_value)],
    );
}

/// Remove expired sessions.
pub fn prune_sessions(db: &Connection) -> usize {
    db.execute("DELETE FROM dashboard_sessions WHERE expires_at_ms <= ?", [now_ms()])
        .unwrap_or(0)
}

/// Pull one cookie out of a Cookie header.
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    raw.split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        db
    }

    const GOOD: &str = "a-long-enough-password";

    #[test]
    fn a_fresh_hub_has_no_account_and_says_so() {
        let db = db();
        assert!(!configured(&db));
    }

    #[test]
    fn the_first_account_needs_the_setup_code() {
        // Otherwise whoever reaches a public hub first owns it.
        let db = db();
        let err = setup_first(&db, "guessed", "stefy", GOOD).unwrap_err().to_string();
        assert!(err.contains("has not issued a setup code"), "{err}");
        assert!(err.contains("dashboard-setup-code"), "it must say how: {err}");

        let code = issue_setup_code(&db).unwrap();
        assert!(setup_first(&db, "still-wrong", "stefy", GOOD).is_err());
        assert!(!configured(&db), "a wrong code must create nothing");
        let name = setup_first(&db, &code, "Stefy", GOOD).unwrap();
        assert_eq!(name, "stefy", "usernames are lower-cased");
        assert!(configured(&db));
    }

    #[test]
    fn a_setup_code_works_once() {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        setup_first(&db, &code, "stefy", GOOD).unwrap();
        let err = setup_first(&db, &code, "someone-else", GOOD).unwrap_err().to_string();
        assert!(err.contains("already has a dashboard account"), "{err}");
    }

    #[test]
    fn a_setup_code_is_not_offered_once_there_is_an_account() {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        setup_first(&db, &code, "stefy", GOOD).unwrap();
        let err = issue_setup_code(&db).unwrap_err().to_string();
        assert!(err.contains("already has a dashboard account"), "{err}");
        assert!(err.contains("dashboard-passwd"), "it must say what to do instead: {err}");
    }

    #[test]
    fn the_password_is_never_stored_in_a_readable_form() {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        setup_first(&db, &code, "stefy", GOOD).unwrap();
        let stored: String = db
            .query_row("SELECT password_hash FROM dashboard_users", [], |r| r.get(0))
            .unwrap();
        assert!(!stored.contains(GOOD), "the password itself must not be there");
        assert!(stored.starts_with("$argon2id$"), "argon2id, not a bare digest: {stored}");
    }

    #[test]
    fn two_accounts_with_one_password_get_different_hashes() {
        // Salted, so a hash tells you nothing about another account.
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        setup_first(&db, &code, "one", GOOD).unwrap();
        add_user(&db, "two", GOOD).unwrap();
        let hashes: Vec<String> = db
            .prepare("SELECT password_hash FROM dashboard_users ORDER BY username")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_ne!(hashes[0], hashes[1]);
    }

    fn with_user() -> Connection {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        setup_first(&db, &code, "stefy", GOOD).unwrap();
        db
    }

    #[test]
    fn signing_in_gives_a_session_that_identifies_the_user() {
        let db = with_user();
        let (token, expires) = login(&db, "stefy", GOOD).unwrap();
        assert!(expires > now_ms());
        assert_eq!(whoami(&db, &token).as_deref(), Some("stefy"));
        // And the stored row is not the cookie itself.
        let stored: String = db
            .query_row("SELECT id_sha256 FROM dashboard_sessions", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, token, "storing the cookie would make the table replayable");
    }

    #[test]
    fn a_wrong_password_and_a_wrong_username_say_the_same_thing() {
        // Otherwise the form reports which accounts exist.
        let db = with_user();
        let a = login(&db, "stefy", "not-the-password").unwrap_err().to_string();
        let b = login(&db, "nobody", GOOD).unwrap_err().to_string();
        assert_eq!(a, b, "{a} vs {b}");
        assert!(a.contains("do not match"), "{a}");
    }

    #[test]
    fn the_username_is_not_case_sensitive_for_signing_in() {
        let db = with_user();
        assert!(login(&db, "Stefy", GOOD).is_ok());
        assert!(login(&db, "  STEFY ", GOOD).is_ok());
    }

    #[test]
    fn enough_wrong_guesses_lock_the_account_for_a_while() {
        let db = with_user();
        for _ in 0..MAX_FAILURES {
            assert!(login(&db, "stefy", "wrong").is_err());
        }
        let err = login(&db, "stefy", GOOD).unwrap_err().to_string();
        assert!(err.contains("locked"), "even the right password waits: {err}");
        assert!(err.contains("minute"), "it must say for how long: {err}");
    }

    #[test]
    fn a_good_password_clears_the_failures_before_the_lock() {
        let db = with_user();
        for _ in 0..(MAX_FAILURES - 1) {
            assert!(login(&db, "stefy", "wrong").is_err());
        }
        assert!(login(&db, "stefy", GOOD).is_ok());
        let failures: i64 = db
            .query_row("SELECT failed_count FROM dashboard_users", [], |r| r.get(0))
            .unwrap();
        assert_eq!(failures, 0);
    }

    #[test]
    fn a_session_expires_and_stops_working() {
        let db = with_user();
        let (token, _) = login(&db, "stefy", GOOD).unwrap();
        db.execute("UPDATE dashboard_sessions SET expires_at_ms = 1", []).unwrap();
        assert!(whoami(&db, &token).is_none());
        // And is cleaned up rather than left lying there.
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM dashboard_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn using_a_session_keeps_it_alive() {
        let db = with_user();
        let (token, _) = login(&db, "stefy", GOOD).unwrap();
        db.execute("UPDATE dashboard_sessions SET expires_at_ms = ?", [now_ms() + 1000])
            .unwrap();
        assert!(whoami(&db, &token).is_some());
        let expires: i64 = db
            .query_row("SELECT expires_at_ms FROM dashboard_sessions", [], |r| r.get(0))
            .unwrap();
        assert!(expires > now_ms() + 1000, "idleness ends it, use extends it");
    }

    #[test]
    fn signing_out_ends_the_session_immediately() {
        let db = with_user();
        let (token, _) = login(&db, "stefy", GOOD).unwrap();
        logout(&db, &token);
        assert!(whoami(&db, &token).is_none());
    }

    #[test]
    fn changing_a_password_ends_every_session_for_that_account() {
        // A password change that leaves old sessions alive locks nobody out.
        let db = with_user();
        let (token, _) = login(&db, "stefy", GOOD).unwrap();
        set_password(&db, "stefy", "another-long-password").unwrap();
        assert!(whoami(&db, &token).is_none());
        assert!(login(&db, "stefy", GOOD).is_err());
        assert!(login(&db, "stefy", "another-long-password").is_ok());
    }

    #[test]
    fn a_short_password_is_refused_with_the_rule() {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        let err = setup_first(&db, &code, "stefy", "short").unwrap_err().to_string();
        assert!(err.contains("12 characters"), "{err}");
    }

    #[test]
    fn a_nonsense_username_is_refused() {
        let db = db();
        let code = issue_setup_code(&db).unwrap();
        for bad in ["a", "has space", "semi;colon", &"x".repeat(33)] {
            assert!(setup_first(&db, &code, bad, GOOD).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_duplicate_account_is_refused() {
        let db = with_user();
        let err = add_user(&db, "stefy", GOOD).unwrap_err().to_string();
        assert!(err.contains("already an account"), "{err}");
    }

    #[test]
    fn an_unknown_cookie_is_nobody() {
        let db = with_user();
        assert!(whoami(&db, "not-a-real-session").is_none());
    }

    #[test]
    fn expired_sessions_are_pruned() {
        let db = with_user();
        login(&db, "stefy", GOOD).unwrap();
        db.execute("UPDATE dashboard_sessions SET expires_at_ms = 1", []).unwrap();
        assert_eq!(prune_sessions(&db), 1);
        assert_eq!(prune_sessions(&db), 0);
    }

    #[test]
    fn a_cookie_is_found_among_others() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("cookie", "other=1; nexmesh_session=abc123; last=2".parse().unwrap());
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc123"));
        assert!(cookie_value(&h, "missing").is_none());
        assert!(cookie_value(&axum::http::HeaderMap::new(), COOKIE).is_none());
    }
}

// ------------------------------------------------------------ handlers ----

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> Response {
    (code, Json(json!({ "error": msg.to_string() }))).into_response()
}

/// Whether this hub has an account yet, and who is signed in.
///
/// Unauthenticated on purpose: the page needs it to decide between a sign-in
/// form and a first-run form before anyone can be signed in.
pub async fn state_of(State(st): State<AppState>, headers: HeaderMap) -> Response {
    // Asked BEFORE the lock is taken: `operator_auth_configured` locks the
    // database itself, and this mutex is not reentrant -- calling it while
    // holding the guard deadlocked the one endpoint the page cannot do
    // without, so the dashboard hung before it could even offer a form.
    let tokens_configured = st.operator_auth_configured();
    let db = st.db.lock().unwrap();
    let who = cookie_value(&headers, COOKIE).and_then(|c| whoami(&db, &c));
    Json(json!({
        "configured": configured(&db),
        "user": who,
        // So the page can still offer the old paste-a-token path on a hub
        // that has no account and is not meant to have one.
        "tokens_configured": tokens_configured,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SetupBody {
    pub setup_code: String,
    pub username: String,
    pub password: String,
}

pub async fn setup(State(st): State<AppState>, Json(b): Json<SetupBody>) -> Response {
    let db = st.db.lock().unwrap();
    match setup_first(&db, &b.setup_code, &b.username, &b.password) {
        Ok(name) => {
            tracing::info!(user = %name, "the first dashboard account was created");
            Json(json!({ "ok": true, "user": name })).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

/// Was this request served over TLS, directly or through a proxy?
///
/// Decides whether the cookie is marked `Secure`. Marking it on a plain-HTTP
/// hub would stop the browser sending it at all, so this has to be answered
/// honestly rather than optimistically.
fn secure_request(st: &AppState, headers: &HeaderMap) -> bool {
    if st.tls_in_use() {
        return true;
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

pub async fn sign_in(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<LoginBody>,
) -> Response {
    let outcome = {
        let db = st.db.lock().unwrap();
        login(&db, &b.username, &b.password)
    };
    match outcome {
        Ok((token, _expires)) => {
            let secure = if secure_request(&st, &headers) { "; Secure" } else { "" };
            // HttpOnly so a script cannot read it; SameSite=Strict because
            // nothing here is meant to be driven from another site.
            let cookie = format!(
                "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age={}",
                SESSION_TTL_MS / 1000
            );
            let mut res = Json(json!({ "ok": true })).into_response();
            if let Ok(v) = cookie.parse() {
                res.headers_mut().insert(axum::http::header::SET_COOKIE, v);
            }
            res
        }
        // 401 for a wrong password, 429 for a locked account: the second is
        // not a credential problem and should not look like one.
        Err(e) => {
            let msg = format!("{e:#}");
            let code = if msg.contains("locked") {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            };
            err(code, msg)
        }
    }
}

pub async fn sign_out(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(c) = cookie_value(&headers, COOKIE) {
        let db = st.db.lock().unwrap();
        logout(&db, &c);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0").parse() {
        res.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    res
}

#[derive(Deserialize)]
pub struct NewUserBody {
    pub username: String,
    pub password: String,
}

/// Add another account. Behind the operator middleware, so only someone
/// already signed in (or holding an operator token) can.
pub async fn create_user(
    State(st): State<AppState>,
    Json(b): Json<NewUserBody>,
) -> std::result::Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = st.db.lock().unwrap();
    match add_user(&db, &b.username, &b.password) {
        Ok(name) => Ok(Json(json!({ "ok": true, "user": name }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": format!("{e:#}") })))),
    }
}
