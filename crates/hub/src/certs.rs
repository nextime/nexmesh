//! Revoking a certificate without re-issuing the CA.
//!
//! Revocation is only a real control if it takes effect at the handshake and
//! takes effect *now*. So three things happen together:
//!
//! 1. The serial goes into a certificate revocation list the CA signs, which
//!    rustls checks while completing the TLS handshake. A revoked machine
//!    cannot connect, as opposed to being refused after it already has.
//! 2. The running hub reloads its TLS configuration, so this does not wait for
//!    a restart. A revocation you have to schedule downtime for is not one.
//! 3. The machine's bearer token is revoked too, and if it is connected right
//!    now it is kicked off. Otherwise a compromised machine keeps the session
//!    it already holds for as long as it cares to.

use crate::state::AppState;
use anyhow::{Context, Result};
use nexmesh_proto::now_ms;
use rusqlite::params;
use serde_json::{json, Value};

/// Remember a certificate we issued, so it can be revoked later.
pub fn record_issued(st: &AppState, serial: u64, name: &str, kind: &str) {
    let db = st.db.lock().unwrap();
    let _ = db.execute(
        "INSERT OR REPLACE INTO issued_certs (serial, name, kind, issued_at_ms)
         VALUES (?,?,?,?)",
        params![serial.to_string(), name, kind, now_ms()],
    );
}

fn revoked_serials(st: &AppState) -> Vec<(u64, i64)> {
    let db = st.db.lock().unwrap();
    db.prepare("SELECT serial, revoked_at_ms FROM issued_certs WHERE revoked_at_ms IS NOT NULL")
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(s, at)| s.parse::<u64>().ok().map(|s| (s, at)))
        .collect()
}

/// Write the current revocation list to disk. Returns how many are on it.
pub fn write_crl(st: &AppState) -> Result<usize> {
    let dir = st.pki_dir();
    let revoked = revoked_serials(st);
    // The CRL number must increase, or a verifier may keep the old list.
    let number = {
        let db = st.db.lock().unwrap();
        let n: i64 = db
            .query_row("SELECT COUNT(*) FROM issued_certs WHERE revoked_at_ms IS NOT NULL", [], |r| r.get(0))
            .unwrap_or(0);
        (n as u64) + 1
    };
    let pem = crate::pki::build_crl(&dir, &revoked, number)?;
    std::fs::write(dir.join("crl.pem"), pem)
        .with_context(|| format!("writing {}", dir.join("crl.pem").display()))?;
    Ok(revoked.len())
}

pub struct Revocation {
    pub certs: usize,
    pub tokens: usize,
    pub kicked: bool,
    pub on_crl: usize,
    pub reloaded: bool,
}

/// Revoke everything belonging to one name.
pub async fn revoke(st: &AppState, name: &str, reason: Option<&str>) -> Result<Revocation> {
    let (certs, kind) = {
        let db = st.db.lock().unwrap();
        let kind: Option<String> = db
            .query_row(
                "SELECT kind FROM issued_certs WHERE name = ? ORDER BY issued_at_ms DESC LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .ok();
        let n = db.execute(
            "UPDATE issued_certs SET revoked_at_ms = ?, reason = ?
             WHERE name = ? AND revoked_at_ms IS NULL",
            params![now_ms(), reason, name],
        )?;
        (n, kind)
    };
    anyhow::ensure!(
        certs > 0,
        "no live certificate issued to '{name}'. `nexmesh-hub pki certs` lists them."
    );

    let on_crl = write_crl(st)?;
    let reloaded = st.reload_tls().await;

    // The certificate is only half the credential. Leaving the token alive
    // would let the machine back in the moment anyone hands it a new one.
    let tokens = {
        let db = st.db.lock().unwrap();
        let a = db
            .execute(
                "UPDATE node_tokens SET revoked_at_ms = ?
                 WHERE node_name = ? AND revoked_at_ms IS NULL",
                params![now_ms(), name],
            )
            .unwrap_or(0);
        let b = db
            .execute(
                "UPDATE operator_tokens SET revoked_at_ms = ?
                 WHERE label = ? AND revoked_at_ms IS NULL",
                params![now_ms(), name],
            )
            .unwrap_or(0);
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, action, detail) VALUES (?, 'operator', 'revoke', ?)",
            params![
                now_ms(),
                json!({ "name": name, "certs": certs, "reason": reason }).to_string()
            ],
        );
        a + b
    };

    // And if it is connected this second, end that connection.
    let kicked = st.kick_node(name);

    tracing::warn!(
        %name, kind = ?kind, certs, tokens, kicked, on_crl, reloaded,
        "revoked"
    );
    Ok(Revocation { certs, tokens, kicked, on_crl, reloaded })
}

pub fn list(st: &AppState) -> Value {
    let db = st.db.lock().unwrap();
    let rows: Vec<Value> = db
        .prepare(
            "SELECT serial, name, kind, issued_at_ms, revoked_at_ms, reason
             FROM issued_certs ORDER BY issued_at_ms DESC LIMIT 100",
        )
        .and_then(|mut s| {
            s.query_map([], |r| {
                let revoked: Option<i64> = r.get(4)?;
                Ok(json!({
                    "serial": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?,
                    "issued_at_ms": r.get::<_, i64>(3)?,
                    "revoked_at_ms": revoked,
                    "reason": r.get::<_, Option<String>>(5)?,
                    "state": if revoked.is_some() { "revoked" } else { "live" },
                }))
            })
            .and_then(|m| m.collect())
        })
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "listing certificates failed");
            vec![]
        });
    json!({ "certificates": rows })
}
