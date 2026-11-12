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
mod nodes;
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
        Some(Cmd::Nodes) => {
            println!("{:#}", st.nodes_json());
            return Ok(());
        }
        Some(Cmd::Serve) | None => {}
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

    let app = Router::new()
        .route("/healthz", get(api::health))
        .route("/v1/nodes", get(api::list_nodes))
        .route("/v1/sessions", get(api::list_sessions).post(api::create_session))
        .route("/v1/sessions/:id/slots", post(api::create_slot))
        .route("/v1/sessions/:id/status", get(api::session_status))
        .route("/v1/sessions/:id/messages", post(api::operator_message))
        .route("/v1/slots/:id/spawn", post(api::spawn_slot))
        .route("/v1/participant/join", post(api::participant_join))
        .route("/v1/participant/tool", post(api::participant_tool))
        .route("/v1/participant/ack", post(api::participant_ack))
        .route("/v1/participant/messages", get(api::participant_messages))
        .route("/v1/participant/heartbeat", post(api::participant_heartbeat))
        .route("/v1/node/ws", get(nodes::node_ws))
        .with_state(st);

    let addr: SocketAddr = args.listen.parse().context("parsing --listen")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, version = VERSION, "hub listening");
    axum::serve(listener, app).await?;
    Ok(())
}
