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
mod approvals;
mod folds;
mod asks;
mod attach;
mod auth;
mod certs;
mod chatcmd;
mod control;
mod dashboard;
mod delivery;
mod fleet;
mod graph;
mod guests;
mod launch;
mod leiden;
mod nodes;
mod budget;
mod mcp;
mod memory;
mod pairing;
mod peer;
mod pki;
mod prompt;
mod quota;
mod reminders;
mod seat;
mod skills;
mod state;
mod sync;
mod tools;
mod watchdog;
mod webauth;

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::{Parser, Subcommand};

/// Where a hub keeps its database when nobody says otherwise.
const DEFAULT_DB: &str = "~/.nexmesh/hub.db";
use nexmesh_proto::now_ms;
use rusqlite::{params, OptionalExtension};
use state::{random_token, sha256_hex, AppState};
use std::net::SocketAddr;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Read a password from an argument or from stdin.
///
/// stdin is the default because a password on a command line ends up in shell
/// history and in `ps` output for anyone on the machine to read.
fn read_password(given: Option<String>) -> anyhow::Result<String> {
    if let Some(p) = given {
        return Ok(p);
    }
    eprint!("new password (not echoed by your terminal if it supports it): ");
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let p = line.trim_end_matches(['\n', '\r']).to_string();
    if p.is_empty() {
        anyhow::bail!("no password given");
    }
    Ok(p)
}

/// Largest request body the hub accepts. See the note where it is applied.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "nexmesh-hub", version, about = "nexmesh coordination hub")]
struct Args {
    #[arg(long, env = "NEXMESH_DB", default_value = DEFAULT_DB)]
    db: String,
    /// Listen address, repeatable. Bind to loopback or a LAN/VPN address;
    /// never 0.0.0.0 without TLS in front, since this API can start
    /// processes -- and on a host with a public address of its own, 0.0.0.0
    /// means the internet.
    ///
    /// Repeatable because that case is real: loopback for a reverse proxy
    /// and a LAN address for the machines, without ever offering the public
    /// interface.
    #[arg(long, env = "NEXMESH_LISTEN", default_value = "127.0.0.1:7870")]
    listen: Vec<String>,
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
    /// Believe the X-Client-Verify / X-Client-CN headers from this address,
    /// so mutual TLS keeps working when nginx terminates it. Repeatable.
    ///
    /// Only safe when the hub is unreachable except through that proxy: a
    /// client able to connect directly could set those headers itself and
    /// claim any identity. Bind to loopback when you use this.
    #[arg(long = "trusted-proxy")]
    trusted_proxies: Vec<std::net::IpAddr>,
    /// Stop requiring a client certificate to name the same identity as the
    /// token sent with it. Only for a fleet whose certificates and tokens were
    /// issued under different names before that check existed: with this on, a
    /// stolen token can be used from any machine holding any valid certificate.
    #[arg(long)]
    no_bind_identity: bool,
    /// Where the fleet CA lives, for pairing.
    #[arg(long, env = "NEXMESH_PKI", default_value = "~/.nexmesh/pki")]
    pki_dir: String,
    /// Address for the pairing listener. A machine being paired has no
    /// certificate yet, so it cannot use the mutual-TLS port; pairing gets its
    /// own, serving ONLY the claim endpoint. Omit it and pairing is off.
    #[arg(long, env = "NEXMESH_PAIR_LISTEN")]
    pair_listen: Option<String>,
    /// Serve the operator API with NO authentication on a non-loopback
    /// address. Only for a hub already behind its own authenticating proxy.
    #[arg(long, env = "NEXMESH_ALLOW_ANONYMOUS")]
    allow_anonymous: bool,
    /// How full a seat's context may get, as a percentage of its window,
    /// before the hub types that CLI's own compaction command at it.
    ///
    /// The FLEET-WIDE default, and the bottom of three levels: a seat's own
    /// setting wins, then its team's, then this. 0 is off, and off is the
    /// default -- compacting an agent throws away context it may still need,
    /// which is not a thing to start doing to every seat on a fleet because
    /// a hub was upgraded. Set it per seat with `nexmesh compact-at`.
    #[arg(long, env = "NEXMESH_COMPACT_AT_PCT", default_value_t = 0)]
    compact_at_pct: i64,
    /// How many times a seat may repeat the same turn before the hub
    /// interrupts it and pauses its seat. Fleet-wide default; 0 is off, and
    /// off is the default. Set it per seat with `nexmesh loop-guard`.
    ///
    /// Below 2 is refused: one "repeat" is a single turn.
    #[arg(long, env = "NEXMESH_LOOP_REPEATS", default_value_t = 0)]
    loop_repeats: i64,
    /// Stop reminding seats to keep what they learn (store_memory,
    /// store_knowledge, fold_context) after a stretch of work in which they
    /// kept nothing. Fleet-wide default; a team or seat can set its own.
    /// Reminders are ON unless this is given.
    #[arg(long, env = "NEXMESH_NO_REMIND_MEMORY")]
    no_remind_memory: bool,
    /// Stop reminding seats that read many files without asking the code
    /// graph that it is there. Fleet-wide default, ON unless this is given.
    #[arg(long, env = "NEXMESH_NO_REMIND_GRAPH")]
    no_remind_graph: bool,
    /// How many messages one seat may send a minute before the hub starts
    /// refusing them. Fleet-wide default; 0 is off, and off is the default.
    ///
    /// A seat's own setting wins, then its team's, then this. The refusal
    /// goes to the AGENT and says what to do instead, so it is a brake
    /// rather than a trap -- and its work is not stopped, only its messages.
    /// 30 is a number no team doing real work reaches; a runaway reaches it
    /// in seconds.
    #[arg(long, env = "NEXMESH_MSG_RATE_PER_MIN", default_value_t = 0)]
    msg_rate_per_min: i64,
    /// How many messages two seats may exchange with EACH OTHER in ten
    /// minutes. Fleet-wide default; 0 is off, and off is the default.
    ///
    /// Separate from the rate because a ping-pong is not fast, it is
    /// relentless: two agents answering each other every twenty seconds
    /// never trip a per-minute rate, and between them burn two turns a
    /// minute for as long as nobody looks. 20 is generous for a pair that is
    /// actually working.
    #[arg(long, env = "NEXMESH_MSG_PAIR_LIMIT", default_value_t = 0)]
    msg_pair_limit: i64,
    /// This fleet's OWN role templates and skill fragments:
    /// `<dir>/roles/<role>.toml` and `<dir>/skills/<name>.md`.
    ///
    /// A file here replaces the built-in of the same name and a new name adds
    /// a role, so local wording survives an upgrade instead of being
    /// overwritten by it. Omitted, the built-in templates are used -- there
    /// is always a full set, because they are compiled into the binary.
    #[arg(long, env = "NEXMESH_PROMPTS_DIR")]
    prompts_dir: Option<String>,
    /// A Digital Asset Links file to serve at `/.well-known/assetlinks.json`.
    ///
    /// Only needed for a HEADSET package. Horizon OS has no install button in
    /// its browser: a Quest app is a signed package built from this hub's
    /// manifest, and an immersive one refuses to launch unless the origin it
    /// claims serves this file naming its package and signing fingerprint.
    /// Generate it with Bubblewrap and point this at it.
    #[arg(long, env = "NEXMESH_ASSETLINKS")]
    assetlinks: Option<String>,
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
    OperatorToken {
        label: String,
        /// Confine this token to one session, by id or name: it drives that
        /// team and cannot see, touch or discover the rest of the fleet.
        /// Omitted, the token is a SUPER orchestrator's -- the whole fleet.
        #[arg(long)]
        session: Option<String>,
    },
    /// Revoke operator tokens by label.
    RevokeOperator { label: String },
    /// List operator tokens (labels and last use, never the tokens).
    Operators,
    /// Fleet certificate authority: mutual TLS.
    #[command(subcommand)]
    Pki(PkiCmd),
    /// Generate a self-signed TLS certificate and key for this hub.
    GenCert {
        /// Hostnames and IPs the certificate should cover, e.g. --host machine-a
        /// --host 10.0.0.2. Nodes must reach the hub by one of these.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// Where to write cert.pem and key.pem.
        #[arg(long, default_value = "~/.nexmesh")]
        out: String,
    },
    /// Print a one-time code authorising the first dashboard account.
    ///
    /// Needed because a hub may answer on a public address, and whoever
    /// arrives first must not simply be able to claim it.
    DashboardSetupCode,
    /// Set or reset a dashboard account's password. Ends its sessions.
    DashboardPasswd {
        username: String,
        /// The new password. Omitted means read it from stdin, which keeps it
        /// out of shell history and `ps`.
        #[arg(long)]
        password: Option<String>,
    },
    /// Add a dashboard account.
    DashboardUser {
        username: String,
        #[arg(long)]
        password: Option<String>,
    },
    /// Mint a token for an external participant (a hermes agent, a script, ...).
    /// It can call team tools but can never spawn a process.
    ParticipantToken {
        label: String,
        /// Restrict the token to one session.
        #[arg(long)]
        session: Option<String>,
        /// Seat whoever uses this token as an OBSERVER: it can read the
        /// team's state and memory, answer whoever asks it, and ask a seat
        /// for its progress -- and nothing else. No messages of its own, no
        /// writes, no touching a seat.
        ///
        /// For letting somebody outside the team watch it: a browser-based
        /// Claude session, a dashboard of their own, a person on a phone.
        /// The restriction is on the TOKEN, so whoever holds it cannot opt
        /// out of it, which is what makes it safe to hand out.
        #[arg(long)]
        observer: bool,
    },
    /// List known nodes.
    Nodes,
    /// Fork the built-in skills and role templates into a directory as files,
    /// to edit them in bulk on disk. Idempotent: a file already there is LEFT
    /// ALONE. NOTE a forked file then SHADOWS its built-in and stops tracking
    /// upgrades to it -- which is why an install does NOT run this; it only
    /// makes the directory, and the built-ins serve from the binary until an
    /// operator deliberately overrides one (through the dashboard, or here).
    SeedPrompts {
        /// The store directory; `skills/` and `roles/` are created under it.
        dir: String,
    },
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
    /// Revoke everything belonging to a name: its certificate, its token, and
    /// the connection it holds right now.
    Revoke {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List the certificates this CA has issued.
    Certs,
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
/// Build the rustls server configuration, including the current revocation
/// list if there is one. Public so the running server can be reloaded after a
/// certificate is revoked.
pub fn build_tls(
    cert: &str,
    key: &str,
    client_ca: Option<&str>,
    pki_dir: &std::path::Path,
) -> Result<std::sync::Arc<rustls::ServerConfig>> {
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
            let mut v = rustls::server::WebPkiClientVerifier::builder(std::sync::Arc::new(roots));
            // The revocation list is checked during the handshake, so a
            // revoked machine cannot connect at all.
            let crl_path = pki_dir.join("crl.pem");
            if crl_path.exists() {
                let data = std::fs::read(&crl_path)
                    .with_context(|| format!("reading {}", crl_path.display()))?;
                let crls: Vec<_> = rustls_pemfile::crls(&mut &data[..])
                    .collect::<std::result::Result<_, _>>()
                    .with_context(|| format!("parsing {}", crl_path.display()))?;
                if !crls.is_empty() {
                    tracing::info!(path = %crl_path.display(), lists = crls.len(),
                        "enforcing certificate revocation");
                    // Only leaf certificates: the CA itself is not on its own
                    // list, and demanding status for it would reject everyone.
                    v = v.with_crls(crls).only_check_end_entity_revocation();
                }
            }
            let verifier = v.build().context("building the client certificate verifier")?;
            rustls::ServerConfig::builder().with_client_cert_verifier(verifier)
        }
        None => rustls::ServerConfig::builder().with_no_client_auth(),
    };
    let cfg = builder
        .with_single_cert(certs, private)
        .context("the certificate and key do not match")?;
    Ok(std::sync::Arc::new(cfg))
}

/// Write the built-in skills and templates into a store directory as editable
/// files, skipping any that already exist. Returns (written, kept).
///
/// Idempotent by design: the installer runs it on every install and upgrade,
/// and an operator's edit to a seeded file -- or a file they added -- must
/// survive that. So an existing file is never touched; only genuinely new
/// built-ins (a skill added in a later release) are written.
fn seed_prompts(dir: &str) -> anyhow::Result<(usize, usize)> {
    let root = std::path::Path::new(dir);
    std::fs::create_dir_all(root.join("skills"))?;
    std::fs::create_dir_all(root.join("roles"))?;
    let mut wrote = 0;
    let mut kept = 0;
    let mut seed = |sub: &str, name: &str, ext: &str, body: &str| -> anyhow::Result<()> {
        let p = root.join(sub).join(format!("{name}.{ext}"));
        if p.exists() {
            kept += 1;
        } else {
            std::fs::write(&p, body)?;
            wrote += 1;
        }
        Ok(())
    };
    for (name, body) in nexmesh_proto::roles::builtin_skills() {
        seed("skills", name, "md", body)?;
    }
    for (id, body) in nexmesh_proto::roles::builtin_roles() {
        seed("roles", id, "toml", body)?;
    }
    Ok((wrote, kept))
}

/// Refuse an administrative subcommand whose database is not there.
///
/// Only applied when the path is the built-in default: an explicit `--db` or
/// `NEXMESH_DB` is someone saying where they want it, and creating it then is
/// their call. The default, though, is where a system install is *not* -- its
/// database lives wherever the service unit says -- so this is exactly the
/// case where a silently created empty database costs an hour.
fn guard_db_exists(args: &Args) -> anyhow::Result<()> {
    let administrative = !matches!(
        args.cmd,
        None | Some(Cmd::Serve)
            | Some(Cmd::Pki(_))
            | Some(Cmd::GenCert { .. })
            // Seeding the prompts store touches no database -- it is run at
            // install time, when there may be none yet.
            | Some(Cmd::SeedPrompts { .. })
    );
    if !administrative || args.db != DEFAULT_DB || std::env::var_os("NEXMESH_DB").is_some() {
        return Ok(());
    }
    let path = state::expand(&args.db);
    if std::path::Path::new(&path).exists() {
        return Ok(());
    }
    anyhow::bail!(
        "no hub database at {path}, and this command will not create one.\n\
         A hub installed as a service keeps its database elsewhere -- read the path from its \n\
         configuration and pass it:\n\
         \n    NEXMESH_DB=/var/lib/nexmesh/hub.db nexmesh-hub <command>\n\
         \nOr, on a hub that has never run, start it once with `serve` first."
    )
}

/// A log sink that never reports an I/O error -- the node's fix, for the hub.
///
/// The hub is built `panic = "abort"` and its service sends stdout and stderr
/// to one logfile. When that filesystem fills, the fmt layer's failed write
/// falls back to `eprintln!`, which fails on the same disk and panics; abort
/// then takes the hub down with no word in its log, and every seat on the
/// fleet loses its team. That happened on 27 Nov 2026 when another service's
/// database filled the hub's root. Dropping the line is the right loss.
struct NeverErr(std::io::Stdout);

impl std::io::Write for NeverErr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut self.0, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut self.0);
        Ok(())
    }
}

struct SafeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SafeWriter {
    type Writer = NeverErr;
    fn make_writer(&'a self) -> Self::Writer {
        NeverErr(std::io::stdout())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(SafeWriter)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    // An administrative subcommand pointed at a database that does not exist
    // is a mistake every time. SQLite would happily create an empty one,
    // print a setup code or a token into it, and leave you holding a
    // credential the running hub has never heard of -- with no error to say
    // so. Only `serve` may bring a database into being.
    guard_db_exists(&args)?;
    // Seed the editable store and exit, before any database is opened: this
    // is what the package installer runs to lay down the built-ins as files.
    if let Some(Cmd::SeedPrompts { dir }) = &args.cmd {
        let (wrote, kept) = seed_prompts(&state::expand(dir))?;
        println!("seeded {wrote} new file(s) into {dir}; left {kept} existing file(s) alone");
        return Ok(());
    }
    // Where this fleet's own skills and templates live, if anywhere. The flag
    // (or NEXMESH_PROMPTS_DIR) wins; absent it, fall back to the packaged
    // store at /var/lib/nexmesh/prompts BUT ONLY IF it exists -- so an
    // installed fleet picks up its editable store with no configuration,
    // while a dev run with neither flag nor directory is unchanged and keeps
    // using the built-ins compiled into the binary.
    let prompts_dir = args.prompts_dir.clone().or_else(|| {
        let d = "/var/lib/nexmesh/prompts";
        std::path::Path::new(d).is_dir().then(|| d.to_string())
    });
    let st = AppState::open(&args.db)?
        .with_pki_dir(&args.pki_dir)
        .with_pair_listen(args.pair_listen.clone())
        .with_identity_binding(!args.no_bind_identity)
        .with_watchdog(args.compact_at_pct, args.loop_repeats)
        .with_reminders(!args.no_remind_memory, !args.no_remind_graph)
        .with_prompts_dir(prompts_dir)
        .with_assetlinks(args.assetlinks.clone())
        .with_flood(args.msg_rate_per_min, args.msg_pair_limit)
        .with_trusted_proxies(args.trusted_proxies.clone());

    // A LOCAL TEMPLATE THAT WILL NOT PARSE IS SKIPPED, AND SAID OUT LOUD.
    //
    // Skipped because one bad file must not take away the other thirteen
    // roles, and a hub that refuses to start over a typo in a prompt is
    // worse than one that starts without it. Said out loud because an
    // operator who wrote a template and then sees the built-in behaviour
    // has nothing at all to go on.
    for complaint in nexmesh_proto::roles::complaints(st.prompts_dir()) {
        tracing::warn!("role template ignored: {complaint}");
    }
    if let Some(dir) = st.prompts_dir() {
        tracing::info!(dir = %dir.display(), roles = nexmesh_proto::roles::all(Some(dir)).len(),
            "role templates (this fleet's own, over the built-in ones)");
    }

    match args.cmd {
        // Handled before the database was opened; it never reaches here.
        Some(Cmd::SeedPrompts { .. }) => unreachable!(),
        Some(Cmd::DashboardSetupCode) => {
            let db = st.db.lock().unwrap();
            match webauth::issue_setup_code(&db) {
                Ok(code) => {
                    println!("setup code: {code}");
                    println!();
                    println!("Open the dashboard and create the first account with it. It works");
                    println!("once, and this is the only time it is shown.");
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }

        Some(Cmd::DashboardPasswd { username, password }) => {
            let password = read_password(password)?;
            let db = st.db.lock().unwrap();
            webauth::set_password(&db, &username, &password)?;
            println!("password set for {username}; any open sessions were ended");
            return Ok(());
        }

        Some(Cmd::DashboardUser { username, password }) => {
            let password = read_password(password)?;
            let db = st.db.lock().unwrap();
            let name = webauth::add_user(&db, &username, &password)?;
            println!("account {name} created");
            return Ok(());
        }

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
        Some(Cmd::ParticipantToken { label, session, observer }) => {
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
            let authority = observer.then(|| {
                nexmesh_proto::authority::Authority::Observer.as_str().to_string()
            });
            if observer && session_id.is_none() {
                // An observer token with no session would seat its holder in
                // whichever team it names -- and a watcher that can pick its
                // own team is not what anybody means by an observer.
                anyhow::bail!(
                    "an observer token has to name the team it may watch: \
                     --session <team>"
                );
            }
            db.execute(
                "INSERT INTO participant_tokens (token_sha256, label, session_id, authority,
                                                 issued_at_ms)
                 VALUES (?,?,?,?,?)",
                params![sha256_hex(&token), label, session_id, authority, now_ms()],
            )?;
            println!("participant: {label}");
            if let Some(s) = &session_id {
                println!("session:     {s}");
            }
            if observer {
                println!("authority:   observer (reads and reports; cannot steer anything)");
            }
            println!("token:       {token}");
            // What to DO with it. A token with no instructions is a token
            // that sits in a terminal scrollback until it is lost.
            let addr = args.listen.first().cloned().unwrap_or_else(|| "127.0.0.1:7870".into());
            let scheme = if args.tls_cert.is_some() { "https" } else { "http" };
            println!("\nMCP endpoint: {scheme}://{addr}/v1/mcp");
            println!("  header:     Authorization: Bearer <the token above>");
            if observer {
                println!(
                    "\nPoint an outside MCP client at that endpoint with this token and it \
                     takes a\nseat in the team that can watch and nothing else: it reads \
                     `check_team_status`,\n`get_plan`, `query_knowledge`, `recall` and the \
                     team's history, answers with `reply`,\nand can ask one seat for \
                     progress, test results or an ETA with `ask_progress`.\n\
                     It cannot send a message of its own writing, change anything, or \
                     touch a seat --\nenforced in the hub on every call, not asked for in \
                     a prompt."
                );
            }
            println!("\nShown once. This token may call team tools; it cannot spawn processes.");
            return Ok(());
        }
        Some(Cmd::OperatorToken { label, session }) => {
            let token = random_token(48);
            let db = st.db.lock().unwrap();
            // A scope has to name a session that exists. Minting a token for
            // a typo would produce a credential that authenticates perfectly
            // and can reach nothing, which is a bad half-hour for whoever
            // holds it.
            let scope: Option<String> = match session.as_deref() {
                Some(given) => Some(
                    db.query_row(
                        "SELECT id FROM sessions WHERE id = ?1 OR name = ?1
                          ORDER BY (id = ?1) DESC LIMIT 1",
                        [given],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .with_context(|| format!(
                        "no session '{given}'; `nexmesh-hub --db <db> sessions` or the \
                         dashboard lists them"
                    ))?,
                ),
                None => None,
            };
            db.execute(
                "INSERT INTO operator_tokens (token_sha256, label, session_id, issued_at_ms)
                 VALUES (?,?,?,?)",
                params![sha256_hex(&token), label, scope, now_ms()],
            )?;
            println!("operator: {label}");
            println!("token:    {token}");
            match &scope {
                Some(id) => println!("scope:    {id} (this session only)"),
                None => println!("scope:    the whole fleet (super orchestrator)"),
            }
            println!("\nShown once; the hub stores only its SHA-256.");
            println!("Use it with:  export NEXMESH_OPERATOR_TOKEN={token}");
            if scope.is_some() {
                println!("\nThis token drives one session. It cannot create sessions, list");
                println!("machines, move a seat to another session, or learn that any other");
                println!("session exists. To reach past it, message the super orchestrators.");
            } else {
                println!("\nThis token can spawn processes on every node and attach to any");
                println!("agent's terminal. Treat it like an ssh key.");
            }
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
                println!("{:<20} {:<10} LAST USED", "LABEL", "STATE");
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
            println!("  nexmesh-hub pki issue machine-a --out /tmp/machine-a");
            println!("\nThe CA key never needs to leave this machine. Copy only ca-cert.pem");
            println!("and that node's own cert and key.");
            return Ok(());
        }
        Some(Cmd::Pki(PkiCmd::Revoke { name, reason })) => {
            // A hub process started for this one command cannot reload the TLS
            // of the one that is serving, so say so rather than imply it.
            let r = certs::revoke(&st, &name, reason.as_deref()).await?;
            println!("revoked {} certificate(s) for {name}", r.certs);
            println!("revoked {} token(s)", r.tokens);
            println!("{} certificate(s) now on the revocation list", r.on_crl);
            if !r.reloaded {
                println!(
                    "\nThe revocation list has been written, but a RUNNING hub keeps the TLS \
                     configuration it started with.\nRestart it, or revoke through the API, for \
                     this to take effect on live connections."
                );
            }
            return Ok(());
        }
        Some(Cmd::Pki(PkiCmd::Certs)) => {
            let v = certs::list(&st);
            let empty = vec![];
            let rows = v["certificates"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no certificates issued yet");
                return Ok(());
            }
            println!("{:<22} {:<10} {:<9} SERIAL", "NAME", "KIND", "STATE");
            for c in rows {
                println!(
                    "{:<22} {:<10} {:<9} {}",
                    c["name"].as_str().unwrap_or("?"),
                    c["kind"].as_str().unwrap_or("?"),
                    c["state"].as_str().unwrap_or("?"),
                    c["serial"].as_str().unwrap_or("?")
                );
            }
            return Ok(());
        }
        Some(Cmd::Pki(PkiCmd::Issue { name, dir, out })) => {
            let dir = std::path::PathBuf::from(state::expand(&dir));
            let out = std::path::PathBuf::from(state::expand(
                &out.unwrap_or_else(|| dir.to_string_lossy().to_string()),
            ));
            let serial = pki::new_serial();
            let (cert, key) = pki::issue_client(&dir, &name, &out, serial)?;
            certs::record_issued(&st, serial, &name, "node");
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

    // Nothing is connected yet, so nothing in the table can be trusted to be
    // running. Each node says what it still has as it arrives, and a live
    // agent is back to `running` within a heartbeat; what this stops is a
    // stale `running` row from before the restart surviving as a fact.
    st.demote_running_agents();

    // A permission prompt is a stopped agent waiting for a person, so it
    // means nothing once that agent is gone -- and nothing will ever mention
    // it again, so no node can mend it. Swept here because this is the only
    // place that sees the whole table at once: a hub has been found holding
    // 77 cards with live buttons for three seats that had died days earlier.
    {
        let db = st.db.lock().unwrap();
        let n = approvals::retire_gone(&db);
        if n > 0 {
            tracing::info!(retired = n,
                           "permission requests for agents that are gone were retired");
        }
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
                delivery::typed_tick(&st).await;
                delivery::interactive_tick(&st).await;
                delivery::recovery_tick(&st).await;
                delivery::stalled_tick(&st).await;
                // A code graph whose tree has moved. One per tick: parsing
                // is minutes of a node's CPU.
                graph::stale_tick(&st).await;
                // And whether any tree HAS moved: a fingerprint is a walk
                // with the parsing taken out, so this is the cheap half of
                // keeping a graph honest. One workspace per tick, and each
                // is asked at most once every ten minutes.
                graph::fresh_tick(&st).await;
            }
        });
    }

    // Transcripts land on disk here, a fifth of a second behind the pane that
    // produced them, so ten chatty terminals cost ten writes a second instead
    // of a write per chunk. A read flushes first, so nothing is ever read
    // stale; this is what writes the bytes nobody has asked for yet.
    //
    // The WAL is kept short in the same place. SQLite checkpoints on its own
    // when it can, and cannot while a reader holds an old snapshot -- which on
    // a hub being polled once a second is most of the time, which is how a
    // 64MB WAL grew beside a 158MB database and made every read page through
    // it.
    {
        let st = st.clone();
        tokio::spawn(async move {
            let mut flush = tokio::time::interval(std::time::Duration::from_millis(200));
            flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut since_checkpoint = 0u32;
            loop {
                flush.tick().await;
                let st2 = st.clone();
                // Disk work, off the async workers: a slow write must not
                // stall an attach relay sharing the thread.
                let _ = tokio::task::spawn_blocking(move || st2.flush_transcripts()).await;
                since_checkpoint += 1;
                if since_checkpoint >= 150 {
                    since_checkpoint = 0;
                    let st2 = st.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let db = st2.db.lock().unwrap();
                        let _ = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
                    })
                    .await;
                }
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
                // A seat held for its allowance comes back at its reset.
                quota::tick(&db);
                // A message nobody has consumed in a day is aged out: kept in
                // the log, but never typed at an agent so late it is noise.
                if let Ok(n) = tools::expire_undelivered(&db, tools::UNDELIVERED_TTL_MS) {
                    if n > 0 {
                        tracing::info!(expired = n, "aged out undelivered messages nobody consumed");
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
        .route("/v1/sessions/:id/name", post(api::rename_session))
        .route(
            "/v1/sessions/:id",
            get(api::session_detail).delete(api::remove_session),
        )
        .route("/v1/sessions/:id/instructions", post(api::set_session_instructions))
        .route("/v1/sessions/:id/slots", post(api::create_slot))
        .route("/v1/sessions/:id/adopt", post(api::adopt_slot))
        .route("/v1/sessions/:id/sync", post(api::sync_workspaces))
        .route("/v1/sessions/:id/workspaces", post(api::add_workspace))
        .route("/v1/slots/:id/name", post(api::rename_slot))
        .route("/v1/slots/:id/role", post(api::set_role))
        .route("/v1/slots/:id/authority", post(api::set_authority))
        .route("/v1/slots/:id/auto-approve", post(api::slot_auto_approve))
        .route("/v1/slots/:id/quota-resume", post(api::slot_quota_resume))
        .route("/v1/slots/:id/autorespawn", post(api::slot_autorespawn))
        .route("/v1/slots/:id/switch", post(api::slot_switch))
        .route("/v1/sessions/:id/switch", post(api::session_switch))
        .route("/v1/slots/:id/resume-handle", post(api::set_resume_handle))
        // What a seat tells its agent: GET reads the text, POST sets or
        // clears it. Deliberately not part of the status poll.
        .route(
            "/v1/roles",
            axum::routing::get(api::roles),
        )
        .route(
            "/v1/roles/:id",
            axum::routing::get(api::role).put(api::write_role).delete(api::delete_role),
        )
        // The editable skill library: GET the whole catalogue with each
        // skill's text and source, PUT to add or edit one, DELETE to remove
        // a local one (a built-in reverts rather than vanishes). And the
        // three-scope enable/disable dials.
        .route("/v1/skills", axum::routing::get(api::list_skills))
        .route(
            "/v1/skills/prefs",
            axum::routing::get(api::get_skill_prefs).post(api::set_skill_pref),
        )
        .route(
            "/v1/skills/:name",
            axum::routing::get(api::read_skill)
                .put(api::write_skill)
                .delete(api::delete_skill),
        )
        .route(
            "/v1/slots/:id/prompt",
            get(api::slot_prompt).post(api::set_slot_prompt),
        )
        .route("/v1/sessions/:id/auto-approve", post(api::session_auto_approve))
        .route("/v1/sessions/:id/guests",
               get(api::session_guests).post(api::session_guest))
        .route("/v1/approvals", get(api::list_approvals))
        .route("/v1/approvals/:id/decide", post(api::decide_approval))
        .route("/v1/slots/:id/owns", post(api::set_owns))
        .route("/v1/sessions/:id/plan", post(api::add_plan_item))
        .route("/v1/sessions/:id/upload", post(api::upload))
        .route("/v1/sessions/:id/messages/clear", post(api::clear_unclaimed))
        // Withdraw a message nobody has seen, so it is never delivered.
        .route("/v1/sessions/:id/messages/cancel", post(api::cancel_messages))
        .route("/v1/sessions/:id/seats/tidy", post(api::tidy_seats))
        // A person at the dashboard using the fleet's own services --
        // transcription, speech -- rather than only agents being able to.
        .route("/v1/uploads/:id", get(api::upload_bytes))
        .route("/v1/slots/:id/terminal-mouse", post(api::terminal_mouse))
        .route("/v1/operator/view", post(api::publish_view))
        .route("/v1/operator/views", get(api::list_views))
        .route("/v1/operator/view/:id", get(api::get_view))
        .route("/v1/capabilities/ask", post(api::operator_ask))
        .route("/v1/capabilities/ask/:id", get(api::operator_ask_result))
        .route("/v1/workspaces/:id/graph", post(api::build_graph))
        .route("/v1/workspaces/:id/subsystem/:community", get(api::subsystem_detail))

        // The operator channel. Not session-scoped on purpose: it is how a
        // scoped orchestrator reaches past its own session, which is the one
        // thing it is otherwise unable to do.
        .route("/v1/operator/messages", post(api::operator_note).get(api::operator_inbox))
        .route("/v1/sessions/:id/pause", post(api::pause))
        .route("/v1/sessions/:id/budget", post(api::set_budget))
        .route("/v1/sessions/:id/groups", post(api::set_group))
        // WHAT THE FLEET REMEMBERS, across every scope, and the operator's
        // own way to write it. Not under a session on purpose: global
        // memory belongs to no session, and a seat's own notes belong to
        // the seat.
        .route("/v1/memory", get(api::list_memory).post(api::write_memory))
        .route("/v1/memory/history", get(api::memory_history))
        .route("/v1/folds", get(api::list_folds))
        .route("/v1/folds/:id", get(api::fold_detail))
        // The last thing an agent said, out of the screen it drew. The rule
        // for what is furniture lives in the proto crate and is shared with
        // the console; the screen itself lives in the browser.
        .route("/v1/speech/reply", post(api::screen_reply))
        .route("/v1/auth/users", post(webauth::create_user))
        .route("/v1/sessions/:id/release", post(api::release))
        .route("/v1/sessions/:id/status", get(api::session_status))
        .route("/v1/sessions/:id/messages", post(api::operator_message))
        .route("/v1/sessions/:id/log", get(api::session_log))
        .route("/v1/sessions/:sid/resolve/:who", get(api::resolve_agent))
        .route("/v1/session-for", get(api::session_for_path))
        .route("/v1/attach/ticket", post(api::attach_ticket))
        .route("/v1/agents/:id/transcript", get(api::agent_transcript))
        .route("/v1/slots/:id/spawn", post(api::spawn_slot))
        .route("/v1/slots/:id/move", post(api::move_slot))
        .route("/v1/slots/:id", axum::routing::delete(api::remove_slot))
        .route("/v1/attach/ws", get(attach::attach_ws))
        .route("/v1/dashboard/state", get(dashboard::snapshot))
        .route("/v1/certs", get(api::list_certs))
        .route("/v1/certs/revoke", post(api::revoke_cert))
        .route("/v1/pair/codes", get(pairing::list).post(pairing::mint))
        .route("/v1/pair/codes/:id/revoke", post(pairing::revoke))
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
        // Installable on a phone: a fleet is watched from wherever you are.
        // Public, like the page: a browser fetches a manifest, an icon and
        // a service worker without credentials, and none of the three holds
        // anything about the fleet.
        .route("/manifest.webmanifest", get(dashboard::manifest))
        .route("/icon.svg", get(dashboard::app_icon))
        .route("/:icon_png", get(dashboard::app_icon_png))
        .route("/sw.js", get(dashboard::service_worker))
        .route("/term.js", get(dashboard::term_js))
        .route("/xr", get(dashboard::xr_page))
        // The room, installable in its own right: an app installed from the
        // dashboard's manifest starts at the dashboard, and an installed app
        // has no address bar to type `/xr` into.
        .route("/manifest-xr.webmanifest", get(dashboard::manifest_xr))
        // What a headset package is verified against. 404 with an
        // explanation until `--assetlinks` says otherwise.
        .route("/.well-known/assetlinks.json", get(dashboard::assetlinks))
        .route("/xr.js", get(dashboard::xr_js))
        .route("/healthz", get(api::health))
        // Signing in cannot itself require being signed in. These three are
        // the only unauthenticated write paths on the hub, and each is
        // guarded by its own evidence: a setup code, or a password.
        .route("/v1/auth/state", get(webauth::state_of))
        .route("/v1/auth/setup", post(webauth::setup))
        .route("/v1/auth/login", post(webauth::sign_in))
        .route("/v1/auth/logout", post(webauth::sign_out))
        .route("/v1/node/ws", get(nodes::node_ws));

    // Participants are a group too, so the certificate check covers a route
    // added here later. The token itself is still checked per handler, which
    // is where the session scoping lives.
    let participant = Router::new()
        // The MCP front door: one endpoint, and any MCP client is a teammate.
        .route(
            "/v1/mcp",
            post(mcp::post).get(mcp::get).delete(mcp::delete),
        )
        .route("/v1/participant/join", post(api::participant_join))
        .route("/v1/participant/tool", post(api::participant_tool))
        .route("/v1/participant/ack", post(api::participant_ack))
        .route("/v1/participant/messages", get(api::participant_messages))
        .route("/v1/participant/heartbeat", post(api::participant_heartbeat))
        .layer(axum::middleware::from_fn_with_state(
            st.clone(),
            auth::bind_participant,
        ));

    let app = operator
        .merge(participant)
        .merge(other)
        // Chosen, not inherited. The default is 2MB, which silently made
        // base64 payloads impossible: an agent handing a scanned page to a
        // capability bridge got a 413 from the hub with no explanation, long
        // before the bridge's own inline limit could say anything useful.
        // 16MB leaves room for roughly 8MB of base64 plus its JSON envelope;
        // anything larger belongs in the target service's own blob store,
        // passed by reference.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(st.clone());

    // The pairing listener, if asked for: its own address, no client
    // certificate required, and only the claim endpoint on it.
    if let Some(pair) = &args.pair_listen {
        let paddr: SocketAddr = pair.parse().context("parsing --pair-listen")?;
        let papp = Router::new()
            .route("/healthz", get(api::health))
            .route("/v1/pair/claim", post(pairing::claim))
            .with_state(st.clone());
        let ptls = match (&args.tls_cert, &args.tls_key) {
            (Some(c), Some(k)) => Some((state::expand(c), state::expand(k))),
            _ => None,
        };
        tokio::spawn(async move {
            match ptls {
                Some((cert, key)) => {
                    // Server certificate only: whoever is pairing has none yet.
                    match build_tls(&cert, &key, None, std::path::Path::new("/nonexistent"))
                        .map(axum_server::tls_rustls::RustlsConfig::from_config)
                    {
                        Ok(cfg) => {
                            tracing::info!(%paddr, "pairing listener (https, no client certificate required)");
                            if let Err(e) = axum_server::bind_rustls(paddr, cfg)
                                .serve(papp.into_make_service())
                                .await
                            {
                                tracing::error!(error = %e, "pairing listener stopped");
                            }
                        }
                        Err(e) => tracing::error!(error = %e, "pairing listener could not start"),
                    }
                }
                None => {
                    tracing::warn!(%paddr,
                        "pairing listener on PLAIN HTTP: a pairing code and a freshly issued \
                         token would cross the network in cleartext. Configure TLS.");
                    match tokio::net::TcpListener::bind(paddr).await {
                        Ok(l) => {
                            if let Err(e) = axum::serve(l, papp).await {
                                tracing::error!(error = %e, "pairing listener stopped");
                            }
                        }
                        Err(e) => tracing::error!(error = %e, "pairing listener could not bind"),
                    }
                }
            }
        });
    }

    let mut addrs: Vec<SocketAddr> = Vec::new();
    for a in &args.listen {
        addrs.push(a.parse().with_context(|| format!("parsing --listen {a}"))?);
    }
    addrs.sort();
    addrs.dedup();
    let addr = *addrs.first().context("no --listen address")?;
    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(c), Some(k)) => Some((state::expand(c), state::expand(k))),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must be given together"),
    };

    // Refuse the one combination that cannot be made safe by accident: a fleet
    // API that can spawn processes, reachable off-box, with nobody checking.
    let authed = st.operator_auth_configured();
    let off_box_addrs: Vec<SocketAddr> =
        addrs.iter().copied().filter(|a| !a.ip().is_loopback()).collect();
    if !off_box_addrs.is_empty() && !authed && !args.allow_anonymous {
        anyhow::bail!(
            // Name the addresses that are actually the problem. Naming the
            // first one sorted said "127.0.0.1" and then advised binding
            // loopback, which was already the case -- true of every detail
            // and useless as a whole.
            "refusing to serve an UNAUTHENTICATED operator API on {}.\n\
             That API can spawn processes on every node and attach to any agent's terminal.\n\
             Either set up access:  nexmesh-hub operator-token <label>\n\
                               or:  nexmesh-hub dashboard-setup-code\n\
             or drop {} and keep only a loopback --listen,\n\
             or, if something in front already authenticates, pass --allow-anonymous.",
            off_box_addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if off_box_addrs.len() == 1 { "that address" } else { "those addresses" }
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
            let pki = std::path::PathBuf::from(state::expand(&args.pki_dir));
            let server_cfg = build_tls(&cert, &key, client_ca.as_deref(), &pki)
                .with_context(|| format!("loading TLS material from {cert} and {key}"))?;
            let config = axum_server::tls_rustls::RustlsConfig::from_config(server_cfg);
            // Remember how to rebuild it, so a revocation takes effect at once.
            st.set_tls(state::TlsHandle {
                config: config.clone(),
                cert: cert.clone(),
                key: key.clone(),
                client_ca: client_ca.clone(),
            });
            drop(listener);
            for a in &addrs {
                tracing::info!(addr = %a, version = VERSION, tls = true, authenticated = authed,
                    mutual_tls = client_ca.is_some(),
                    "hub listening (https/wss); dashboard at https://{a}/");
            }
            if client_ca.is_none() {
                tracing::warn!(
                    "TLS without --client-ca: a stolen token is enough to reach this hub. \
                     Run `nexmesh-hub pki init` to require client certificates."
                );
            }
            if args.no_bind_identity && client_ca.is_some() {
                tracing::warn!(
                    "--no-bind-identity: a certificate is no longer checked against the token \
                     sent with it, so any valid certificate will carry any valid token"
                );
            }
            // Not `bind_rustls`: the plain acceptor discards the handshake, and
            // with it the peer's certificate. This one keeps the certificate's
            // name on every request of the connection.
            // One server per address, all sharing the same router and state.
            let mut servers = Vec::new();
            for a in &addrs {
                let acceptor = peer::IdentityAcceptor::new(
                    axum_server::tls_rustls::RustlsAcceptor::new(config.clone()),
                );
                let svc = app.clone().into_make_service_with_connect_info::<SocketAddr>();
                let a = *a;
                servers.push(tokio::spawn(async move {
                    axum_server::bind(a).acceptor(acceptor).serve(svc).await
                }));
            }
            // If any one of them stops, stop: a hub half-listening is worse
            // than a hub that is plainly down, because half the fleet would
            // still think it was fine.
            let (first, _, _) = futures_util::future::select_all(servers).await;
            first??;
        }
        None => {
            drop(listener);
            let mut servers = Vec::new();
            for a in &addrs {
                tracing::info!(addr = %a, version = VERSION, tls = false,
                    authenticated = authed, "hub listening (http/ws); dashboard at http://{a}/");
                let svc = app.clone().into_make_service_with_connect_info::<SocketAddr>();
                let l = tokio::net::TcpListener::bind(a)
                    .await
                    .with_context(|| format!("binding {a}"))?;
                servers.push(tokio::spawn(async move { axum::serve(l, svc).await }));
            }
            let (first, _, _) = futures_util::future::select_all(servers).await;
            first??;
        }
    }
    Ok(())
}
