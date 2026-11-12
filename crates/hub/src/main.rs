// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! nexmesh hub: the single source of truth.
//!
//! It holds all state, decides what runs where, and routes messages. It
//! deliberately has NO ability to signal a process: killing is a request to the
//! owning node, which verifies the identity triple first. That asymmetry is
//! what makes a pid stored in this database harmless.

mod api;
mod attach;
mod auth;
mod dashboard;
mod delivery;
mod launch;
mod nodes;
mod pki;
mod prompt;
mod state;
mod tools;

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::{Parser, Subcommand};
use nexmesh_proto::now_ms;
use rusqlite::params;
use state::{random_token, sha256_hex, AppState};
use std::net::SocketAddr;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "nexmesh-hub", version, about = "nexmesh coordination hub")]
struct Args {
    #[arg(long, env = "NEXMESH_DB", default_value = "~/.nexmesh/hub.db")]
    db: String,
    /// Listen address. Bind to loopback or a VPN address; never 0.0.0.0
    /// without TLS in front, since this API can start processes.
    #[arg(long, env = "NEXMESH_LISTEN", default_value = "127.0.0.1:7870")]
    listen: String,
    /// Serve HTTPS/WSS with this certificate chain (PEM). Needs --tls-key.
    #[arg(long, env = "NEXMESH_TLS_CERT")]
    tls_cert: Option<String>,
    /// Private key for --tls-cert (PEM).
    #[arg(long, env = "NEXMESH_TLS_KEY")]
    tls_key: Option<String>,
    /// Require every client to present a certificate signed by this CA
    /// (mutual TLS). With it, a stolen token is not enough to reach the hub:
    /// the caller also needs a private key that never leaves its machine.
    #[arg(long, env = "NEXMESH_CLIENT_CA")]
    client_ca: Option<String>,
    /// Serve the operator API with NO authentication on a non-loopback
    /// address. Only for a hub already behind its own authenticating proxy.
    #[arg(long, env = "NEXMESH_ALLOW_ANONYMOUS")]
    allow_anonymous: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the hub (default).
    Serve,
    /// Mint an enrolment token for a node. Printed once; only its hash is kept.
    Enrol { node_name: String },
    /// Revoke a node's tokens.
    Revoke { node_name: String },
    /// Mint an operator token: full control of the fleet. Printed once.
    OperatorToken { label: String },
    /// Revoke operator tokens by label.
    RevokeOperator { label: String },
    /// List operator tokens (labels and last use, never the tokens).
    Operators,
    /// Fleet certificate authority: mutual TLS.
    #[command(subcommand)]
    Pki(PkiCmd),
    /// Generate a self-signed TLS certificate and key for this hub.
    GenCert {
        /// Hostnames and IPs the certificate should cover, e.g. --host zeiss
        /// --host 192.168.42.3. Nodes must reach the hub by one of these.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// Where to write cert.pem and key.pem.
        #[arg(long, default_value = "~/.nexmesh")]
        out: String,
    },
    /// Mint a token for an external participant (a hermes agent, a script, ...).
    /// It can call team tools but can never spawn a process.
    ParticipantToken {
        label: String,
        /// Restrict the token to one session.
        #[arg(long)]
        session: Option<String>,
    },
    /// List known nodes.
    Nodes,
}

#[derive(Subcommand)]
enum PkiCmd {
    /// Create the fleet CA and the hub's own server certificate.
    Init {
        /// Names and addresses nodes will use to reach this hub.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        #[arg(long, default_value = "~/.nexmesh/pki")]
        dir: String,
    },
    /// Issue a client certificate for a node or an operator.
    Issue {
        /// Name it asserts, e.g. the node name.
        name: String,
        #[arg(long, default_value = "~/.nexmesh/pki")]
        dir: String,
        /// Where to write the certificate and key.
        #[arg(long)]
        out: Option<String>,
    },
}

/// Build the TLS configuration, requiring client certificates when a client CA
/// is given.
fn tls_config(cert: &str, key: &str, client_ca: Option<&str>)
    -> Result<axum_server::tls_rustls::RustlsConfig>
{
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let _ = rustls::crypto::ring::default_provider().install_default();

    let read_certs = |p: &str| -> Result<Vec<CertificateDer<'static>>> {
        let data = std::fs::read(p).with_context(|| format!("reading {p}"))?;
        let certs: Vec<_> = rustls_pemfile::certs(&mut &data[..])
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("parsing certificates in {p}"))?;
        anyhow::ensure!(!certs.is_empty(), "{p} contains no certificates");
        Ok(certs)
    };

    let certs = read_certs(cert)?;
    let data = std::fs::read(key).with_context(|| format!("reading {key}"))?;
    let private: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &data[..])
        .with_context(|| format!("parsing the key in {key}"))?
        .with_context(|| format!("{key} contains no private key"))?;

    let builder = match client_ca {
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            for c in read_certs(ca)? {
                roots.add(c).with_context(|| format!("adding a CA from {ca}"))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(std::sync::Arc::new(roots))
                .build()
                .context("building the client certificate verifier")?;
            rustls::ServerConfig::builder().with_client_cert_verifier(verifier)
        }
        None => rustls::ServerConfig::builder().with_no_client_auth(),
    };
    let cfg = builder
        .with_single_cert(certs, private)
        .context("the certificate and key do not match")?;
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(cfg)))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let st = AppState::open(&args.db)?;

    match args.cmd {
        Some(Cmd::Enrol { node_name }) => {
            let token = random_token(48);
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO node_tokens (token_sha256, node_name, issued_at_ms) VALUES (?,?,?)",
                params![sha256_hex(&token), node_name, now_ms()],
            )?;
            println!("node:  {node_name}");
            println!("token: {token}");
            println!("\nShown once; the hub stores only its SHA-256.");
            return Ok(());
        }
        Some(Cmd::Revoke { node_name }) => {
            let db = st.db.lock().unwrap();
            let n = db.execute(
                "UPDATE node_tokens SET revoked_at_ms = ? WHERE node_name = ? AND revoked_at_ms IS NULL",
                params![now_ms(), node_name],
            )?;
            println!("revoked {n} token(s) for {node_name}");
            return Ok(());
        }
        Some(Cmd::ParticipantToken { label, session }) => {
            let token = random_token(48);
            let db = st.db.lock().unwrap();
            let session_id: Option<String> = match &session {
                Some(s) => Some(
                    db.query_row(
                        "SELECT id FROM sessions WHERE id = ?1 OR name = ?1",
                        [s],
                        |r| r.get(0),
                    )
                    .with_context(|| format!("no session '{s}'"))?,
                ),
                None => None,
            };
            db.execute(
                "INSERT INTO participant_tokens (token_sha256, label, session_id, issued_at_ms)
                 VALUES (?,?,?,?)",
                params![sha256_hex(&token), label, session_id, now_ms()],
            )?;
            println!("participant: {label}");
            if let Some(s) = &session_id {
                println!("session:     {s}");
            }
            println!("token:       {token}");
            println!("\nShown once. This token may call team tools; it cannot spawn processes.");
            return Ok(());
        }
        Some(Cmd::OperatorToken { label }) => {
            let token = random_token(48);
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO operator_tokens (token_sha256, label, issued_at_ms) VALUES (?,?,?)",
                params![sha256_hex(&token), label, now_ms()],
            )?;
            println!("operator: {label}");
            println!("token:    {token}");
            println!("\nShown once; the hub stores only its SHA-256.");
            println!("Use it with:  export NEXMESH_OPERATOR_TOKEN={token}");
            println!("\nThis token can spawn processes on every node and attach to any");
            println!("agent's terminal. Treat it like an ssh key.");
            return Ok(());
        }
        Some(Cmd::RevokeOperator { label }) => {
            let db = st.db.lock().unwrap();
            let n = db.execute(
                "UPDATE operator_tokens SET revoked_at_ms = ? WHERE label = ? AND revoked_at_ms IS NULL",
                params![now_ms(), label],
            )?;
            println!("revoked {n} operator token(s) for {label}");
            return Ok(());
        }
        Some(Cmd::Operators) => {
            let db = st.db.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT label, issued_at_ms, last_used_ms, revoked_at_ms FROM operator_tokens
                 ORDER BY issued_at_ms",
            )?;
            let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<_, _>>()?;
            if rows.is_empty() {
                println!("no operator tokens: the operator API is UNAUTHENTICATED");
                println!("mint one with: nexmesh-hub operator-token <label>");
            } else {
                println!("{:<20} {:<10} {}", "LABEL", "STATE", "LAST USED");
                for (label, _issued, used, revoked) in rows {
                    println!(
                        "{:<20} {:<10} {}",
                        label,
                        if revoked.is_some() { "revoked" } else { "active" },
                        used.map(|_| "yes").unwrap_or("never")
                    );
                }
            }
            return Ok(());
        }
        Some(Cmd::Pki(PkiCmd::Init { hosts, dir })) => {
            let dir = std::path::PathBuf::from(state::expand(&dir));
            let files = pki::init(&dir, &hosts)?;
            println!("fleet CA created in {}", dir.display());
            for (what, p) in &files {
                println!("  {what:<22} {}", p.display());
            }
            println!("\nStart the hub with mutual TLS:");
            println!("  nexmesh-hub --tls-cert {}/server-cert.pem \\", dir.display());
            println!("              --tls-key  {}/server-key.pem \\", dir.display());
            println!("              --client-ca {}/ca-cert.pem --listen 0.0.0.0:7870", dir.display());
            println!("\nThen issue a certificate per node:");
            println!("  nexmesh-hub pki issue zeiss --out /tmp/zeiss");
            println!("\nThe CA key never needs to leave this machine. Copy only ca-cert.pem");
            println!("and that node's own cert and key.");
            return Ok(());
        }
        Some(Cmd::Pki(PkiCmd::Issue { name, dir, out })) => {
            let dir = std::path::PathBuf::from(state::expand(&dir));
            let out = std::path::PathBuf::from(state::expand(
                &out.unwrap_or_else(|| dir.to_string_lossy().to_string()),
            ));
            let (cert, key) = pki::issue_client(&dir, &name, &out)?;
            println!("certificate: {}", cert.display());
            println!("private key: {}  (mode 0600)", key.display());
            println!("\nCopy those two files and {}/ca-cert.pem to {name}, then:", dir.display());
            println!("  nexmesh-node --hub wss://<hub>:7870 --ca-cert ca-cert.pem \\");
            println!("               --client-cert {name}-cert.pem --client-key {name}-key.pem");
            return Ok(());
        }
        Some(Cmd::GenCert { hosts, out }) => {
            let dir = state::expand(&out);
            std::fs::create_dir_all(&dir)?;
            let cert = rcgen::generate_simple_self_signed(hosts.clone())
                .context("generating a self-signed certificate")?;
            let cert_path = format!("{dir}/cert.pem");
            let key_path = format!("{dir}/key.pem");
            std::fs::write(&cert_path, cert.cert.pem())?;
            std::fs::write(&key_path, cert.key_pair.serialize_pem())?;
            // The key must not be world-readable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("certificate: {cert_path}");
            println!("private key: {key_path}  (mode 0600)");
            println!("covers:      {}", hosts.join(", "));
            println!("\nStart the hub with:");
            println!("  nexmesh-hub --tls-cert {cert_path} --tls-key {key_path} --listen 0.0.0.0:7870");
            println!("\nThis certificate is self-signed, so each node needs it as a trust anchor:");
            println!("  copy {cert_path} to the node and pass --ca-cert <path>");
            return Ok(());
        }
        Some(Cmd::Nodes) => {
            println!("{:#}", st.nodes_json());
            return Ok(());
        }
        Some(Cmd::Serve) | None => {}
    }

    // Delivery and recovery. Driven agents get messages pushed to them within
    // a tick instead of waiting for their next tool call; crashed agents come
    // back with a handoff.
    {
        let st = st.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                tick.tick().await;
                delivery::push_tick(&st).await;
                delivery::recovery_tick(&st).await;
            }
        });
    }

    // Lease sweeper: the only thing that ages a node or an external agent out,
    // and the only thing that returns their in-flight messages to the queue.
    {
        let st = st.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                st.expire_leases();
                let db = st.db.lock().unwrap();
                // A message handed over but unacked for two minutes is assumed
                // never to have reached a model.
                if let Ok(n) = tools::reclaim_stale_inflight(&db, 120_000) {
                    if n > 0 {
                        tracing::info!(reclaimed = n, "returned stale in-flight messages to the queue");
                    }
                }
            }
        });
    }

    // Operator routes sit behind the auth middleware as a GROUP, so a route
    // added later is protected by default rather than by remembering to check.
    let operator = Router::new()
        .route("/v1/nodes", get(api::list_nodes))
        .route("/v1/sessions", get(api::list_sessions).post(api::create_session))
        .route("/v1/sessions/:id/slots", post(api::create_slot))
        .route("/v1/sessions/:id/adopt", post(api::adopt_slot))
        .route("/v1/sessions/:id/status", get(api::session_status))
        .route("/v1/sessions/:id/messages", post(api::operator_message))
        .route("/v1/sessions/:id/log", get(api::session_log))
        .route("/v1/sessions/:sid/resolve/:who", get(api::resolve_agent))
        .route("/v1/agents/:id/transcript", get(api::agent_transcript))
        .route("/v1/slots/:id/spawn", post(api::spawn_slot))
        .route("/v1/attach/ws", get(attach::attach_ws))
        .route("/v1/dashboard/state", get(dashboard::snapshot))
        .layer(axum::middleware::from_fn_with_state(
            st.clone(),
            auth::require_operator,
        ));

    // These carry their own credentials: a node authenticates in its first
    // frame, a participant with a participant token.
    // The page itself carries no data, so it is served without a token and
    // asks for one in the browser; the snapshot it fetches is authenticated.
    let other = Router::new()
        .route("/", get(dashboard::page))
        .route("/dashboard", get(dashboard::page))
        .route("/favicon.ico", get(dashboard::favicon))
        .route("/healthz", get(api::health))
        .route("/v1/participant/join", post(api::participant_join))
        .route("/v1/participant/tool", post(api::participant_tool))
        .route("/v1/participant/ack", post(api::participant_ack))
        .route("/v1/participant/messages", get(api::participant_messages))
        .route("/v1/participant/heartbeat", post(api::participant_heartbeat))
        .route("/v1/node/ws", get(nodes::node_ws));

    let app = operator.merge(other).with_state(st.clone());

    let addr: SocketAddr = args.listen.parse().context("parsing --listen")?;
    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(c), Some(k)) => Some((state::expand(c), state::expand(k))),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must be given together"),
    };

    // Refuse the one combination that cannot be made safe by accident: a fleet
    // API that can spawn processes, reachable off-box, with nobody checking.
    let authed = st.operator_auth_configured();
    if !addr.ip().is_loopback() && !authed && !args.allow_anonymous {
        anyhow::bail!(
            "refusing to serve an UNAUTHENTICATED operator API on {addr}.\n\
             That API can spawn processes on every node and attach to any agent's terminal.\n\
             Either mint a token:   nexmesh-hub operator-token <label>\n\
             or bind loopback:      --listen 127.0.0.1:7870\n\
             or, if something in front already authenticates, pass --allow-anonymous."
        );
    }
    if !authed {
        tracing::warn!(
            "no operator tokens exist: the operator API is UNAUTHENTICATED. \
             Mint one with `nexmesh-hub operator-token <label>`."
        );
    }
    if args.client_ca.is_some() && tls.is_none() {
        anyhow::bail!("--client-ca requires --tls-cert and --tls-key: there is no mutual TLS without TLS");
    }
    if tls.is_none() && !addr.ip().is_loopback() {
        tracing::warn!(
            %addr,
            "serving PLAIN HTTP off-box: tokens and agent terminals cross the network in \
             cleartext. Pass --tls-cert/--tls-key, or keep this inside a VPN."
        );
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    match tls {
        Some((cert, key)) => {
            let client_ca = args.client_ca.as_deref().map(state::expand);
            let config = tls_config(&cert, &key, client_ca.as_deref())
                .with_context(|| format!("loading TLS material from {cert} and {key}"))?;
            drop(listener);
            tracing::info!(%addr, version = VERSION, tls = true, authenticated = authed,
                mutual_tls = client_ca.is_some(),
                "hub listening (https/wss); dashboard at https://{addr}/");
            if client_ca.is_none() {
                tracing::warn!(
                    "TLS without --client-ca: a stolen token is enough to reach this hub. \
                     Run `nexmesh-hub pki init` to require client certificates."
                );
            }
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            tracing::info!(%addr, version = VERSION, tls = false, authenticated = authed,
                "hub listening (http/ws); dashboard at http://{addr}/");
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}
