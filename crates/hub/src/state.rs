//! Hub state: the database plus the set of live node connections.

use anyhow::{Context, Result};
use nexmesh_proto::{self as proto, now_ms, HubToNode};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_token(len: usize) -> String {
    use rand::Rng;
    const C: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len).map(|_| C[rng.gen_range(0..C.len())] as char).collect()
}

/// `CREATE TABLE IF NOT EXISTS` silently does nothing when a table already
/// exists, so a column added to the schema never reaches a database created by
/// an older build. Adding columns here keeps an existing hub.db usable instead
/// of failing at the first INSERT that mentions the new column.
fn migrate(conn: &Connection) -> Result<()> {
    let wanted: &[(&str, &str, &str)] = &[
        ("slots", "status", "TEXT NOT NULL DEFAULT 'disconnected'"),
        ("slots", "summary", "TEXT NOT NULL DEFAULT ''"),
        ("slots", "participant_kind", "TEXT NOT NULL DEFAULT 'cli'"),
        ("agents", "lease_expires_ms", "INTEGER"),
        ("agents", "push_capable", "INTEGER NOT NULL DEFAULT 0"),
        ("slots", "tmux_target", "TEXT"),
        ("sessions", "paused_at", "INTEGER"),
        ("slots", "paused_at", "INTEGER"),
        ("agents", "kind_hint", "TEXT"),
        ("slots", "model", "TEXT"),
        ("slots", "capabilities", "TEXT NOT NULL DEFAULT '[]'"),
        ("agents", "model", "TEXT"),
        ("sessions", "token_budget", "INTEGER"),
        ("sessions", "budget_hit_at_ms", "INTEGER"),
    ];
    // A table added after the fact: CREATE TABLE IF NOT EXISTS in the schema
    // covers this, but be explicit so an old database is definitely usable.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS operator_tokens (
            token_sha256  TEXT PRIMARY KEY,
            label         TEXT NOT NULL,
            issued_at_ms  INTEGER NOT NULL,
            revoked_at_ms INTEGER,
            last_used_ms  INTEGER
         )",
    )?;
    // Memory v2 and cross-session continuity. The schema's CREATE TABLE IF NOT
    // EXISTS covers a fresh database; this covers one that predates them.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            scope         TEXT NOT NULL,
            scope_ref     TEXT NOT NULL DEFAULT '',
            key           TEXT NOT NULL,
            category      TEXT NOT NULL DEFAULT 'context',
            value         TEXT NOT NULL,
            author_slot   TEXT,
            revision      INTEGER NOT NULL DEFAULT 1,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER,
            UNIQUE (scope, scope_ref, key)
         );
         CREATE INDEX IF NOT EXISTS memory_scope_idx ON memory(scope, scope_ref);
         CREATE TABLE IF NOT EXISTS memory_groups (
            group_name  TEXT NOT NULL,
            slot_id     TEXT NOT NULL,
            added_at_ms INTEGER NOT NULL,
            PRIMARY KEY (group_name, slot_id)
         );
         CREATE TABLE IF NOT EXISTS memory_history (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id      INTEGER NOT NULL,
            revision       INTEGER NOT NULL,
            value          TEXT NOT NULL,
            author_slot    TEXT,
            replaced_at_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS memory_history_idx
            ON memory_history(memory_id, revision);
         CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
            key, value, content='memory', content_rowid='id'
         );
         CREATE TRIGGER IF NOT EXISTS memory_fts_ins AFTER INSERT ON memory BEGIN
            INSERT INTO memory_fts(rowid, key, value) VALUES (new.id, new.key, new.value);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_fts_del AFTER DELETE ON memory BEGIN
            INSERT INTO memory_fts(memory_fts, rowid, key, value)
              VALUES ('delete', old.id, old.key, old.value);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_fts_upd AFTER UPDATE ON memory BEGIN
            INSERT INTO memory_fts(memory_fts, rowid, key, value)
              VALUES ('delete', old.id, old.key, old.value);
            INSERT INTO memory_fts(rowid, key, value) VALUES (new.id, new.key, new.value);
         END;
         CREATE TABLE IF NOT EXISTS agent_continuity (
            identity      TEXT PRIMARY KEY,
            cli_id        TEXT NOT NULL,
            workspace_key TEXT NOT NULL,
            name          TEXT NOT NULL,
            resume_handle TEXT,
            context       TEXT NOT NULL DEFAULT '',
            updated_at_ms INTEGER NOT NULL
         )",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dashboard_users (
            username TEXT PRIMARY KEY, password_hash TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL, last_login_ms INTEGER,
            failed_count INTEGER NOT NULL DEFAULT 0, locked_until_ms INTEGER);
         CREATE TABLE IF NOT EXISTS dashboard_sessions (
            id_sha256 TEXT PRIMARY KEY, username TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL, last_seen_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS dashboard_sessions_user_idx
            ON dashboard_sessions(username);
         CREATE TABLE IF NOT EXISTS dashboard_setup (
            id INTEGER PRIMARY KEY CHECK (id = 1), code_sha256 TEXT NOT NULL,
            issued_at_ms INTEGER NOT NULL, used_at_ms INTEGER)",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS capability_asks (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, capability TEXT NOT NULL,
            from_slot TEXT NOT NULL, to_slot TEXT, request TEXT NOT NULL, model TEXT,
            status TEXT NOT NULL DEFAULT 'pending', result TEXT,
            asked_at_ms INTEGER NOT NULL, answered_at_ms INTEGER);
         CREATE INDEX IF NOT EXISTS capability_asks_queue_idx
            ON capability_asks(to_slot, status)",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS code_graph_nodes (
            workspace_id TEXT NOT NULL, id TEXT NOT NULL, kind TEXT NOT NULL,
            name TEXT NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL,
            lang TEXT NOT NULL, community INTEGER,
            PRIMARY KEY (workspace_id, id));
         CREATE INDEX IF NOT EXISTS code_graph_name_idx
            ON code_graph_nodes(workspace_id, name);
         CREATE INDEX IF NOT EXISTS code_graph_comm_idx
            ON code_graph_nodes(workspace_id, community);
         CREATE TABLE IF NOT EXISTS code_graph_edges (
            workspace_id TEXT NOT NULL, from_id TEXT NOT NULL, to_id TEXT NOT NULL,
            kind TEXT NOT NULL, PRIMARY KEY (workspace_id, from_id, to_id, kind));
         CREATE INDEX IF NOT EXISTS code_graph_from_idx
            ON code_graph_edges(workspace_id, from_id);
         CREATE INDEX IF NOT EXISTS code_graph_to_idx
            ON code_graph_edges(workspace_id, to_id);
         CREATE TABLE IF NOT EXISTS code_graph_meta (
            workspace_id TEXT PRIMARY KEY, built_at_ms INTEGER NOT NULL,
            files INTEGER NOT NULL, skipped TEXT NOT NULL DEFAULT '[]',
            truncated INTEGER NOT NULL DEFAULT 0,
            communities INTEGER NOT NULL DEFAULT 0)",
    )?;

    // The CLI's own conversation id, for seats that follow a conversation
    // rather than a directory. Added after the fact: an existing hub must
    // keep its slots.
    let _ = conn.execute("ALTER TABLE slots ADD COLUMN cli_session TEXT", []);
    // Guarded: migrate() also runs against databases predating most of this
    // schema, where `slots` may not have session_id yet. An index naming a
    // column that is not there fails the whole migration.
    if has_column(conn, "slots", "session_id")? && has_column(conn, "slots", "cli_session")? {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS slots_cli_session_idx
                ON slots(session_id, cli_session)",
        )?;
    }


    // Carry any rows from the original flat `knowledge` table into session
    // scope. An upgraded hub must not look as though it forgot everything.
    // Runs once: the INSERT ignores keys memory already has.
    let has_knowledge: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='knowledge'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if has_knowledge {
        let moved = conn.execute(
            "INSERT OR IGNORE INTO memory (scope, scope_ref, key, category, value, author_slot,
                                           created_at_ms, updated_at_ms)
             SELECT 'session', session_id, key, category, value, author_slot,
                    updated_at_ms, updated_at_ms
               FROM knowledge",
            [],
        )?;
        if moved > 0 {
            tracing::info!(entries = moved, "carried knowledge into scoped memory");
        }
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_sessions (
            id            TEXT PRIMARY KEY,
            agent_id      TEXT NOT NULL REFERENCES agents(id),
            token_sha256  TEXT NOT NULL,
            protocol      TEXT NOT NULL,
            pending_ack   TEXT NOT NULL DEFAULT '[]',
            created_at_ms INTEGER NOT NULL,
            last_seen_ms  INTEGER NOT NULL,
            closed_at_ms  INTEGER
         )",
    )?;
    for (table, column, decl) in wanted {
        // A database old enough to lack the table entirely gets it from the
        // schema's CREATE TABLE IF NOT EXISTS; there is nothing to alter.
        if !table_exists(conn, table)? {
            continue;
        }
        if !has_column(conn, table, column)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
                .with_context(|| format!("adding {table}.{column}"))?;
            tracing::info!(table, column, "migrated: added column");
        }
    }
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        [table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|c| c.ok())
        .any(|c| c == column);
    Ok(found)
}

/// Cap per agent transcript before it rotates once to `.1`. Agent output is
/// unbounded in principle, so it must not be able to fill the hub's disk.
const TRANSCRIPT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// An agent id arrives over the wire and is used as a FILENAME, so it must be
/// validated or a node could write outside the transcript directory.
fn safe_id(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(id)
}

/// What the hub pushes to an attached operator. Output is raw bytes; control
/// messages are JSON. Keeping them distinct means no base64 on the hot path.
#[derive(Debug, Clone)]
pub enum AttachFrame {
    Data(Vec<u8>),
    Control(String),
}

struct AttachSink {
    tx: mpsc::Sender<AttachFrame>,
    node_id: String,
}

/// What is needed to rebuild the server's TLS configuration in place.
#[derive(Clone)]
pub struct TlsHandle {
    pub config: axum_server::tls_rustls::RustlsConfig,
    pub cert: String,
    pub key: String,
    pub client_ca: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    /// Live attach sessions, keyed by attach id, so frames coming back from a
    /// node can be routed to the right operator terminal.
    attaches: Arc<Mutex<HashMap<String, AttachSink>>>,
    /// Slots currently being respawned. One at a time per slot, or a slow node
    /// would get two agents for the same seat.
    respawning: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Outstanding workspace-sync requests, awaiting the node's answer.
    sync_waiters: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>>,
    sync_seq: Arc<std::sync::atomic::AtomicU64>,
    /// How to rebuild and swap the live TLS configuration, so revoking a
    /// certificate does not have to wait for a restart.
    tls: Arc<Mutex<Option<TlsHandle>>>,
    /// Per node name, a way to end its connection right now.
    kicks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// Where per-agent transcripts live, beside the database.
    pub transcripts: Arc<std::path::PathBuf>,
    /// Where the fleet CA lives, for signing pairing requests.
    pki: Arc<std::path::PathBuf>,
    /// The pairing listener's address, so the dashboard can show the exact
    /// command to run. None means pairing is switched off.
    pair_listen: Arc<Option<String>>,
    /// Live node sockets, so the hub can push a spawn or a kill. A node absent
    /// from this map is unreachable, whatever the database says.
    nodes: Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>,
    /// Whether a client certificate must agree with the credential presented
    /// alongside it. On by default; see `AppState::identity_binding`.
    bind_identity: Arc<bool>,
    /// Addresses whose `X-Client-Verify` / `X-Client-CN` headers are believed.
    /// Empty means none, which is the only safe default: those headers are
    /// exactly what an attacker would set.
    trusted_proxies: Arc<Vec<std::net::IpAddr>>,
}

impl AppState {
    pub fn open(path: &str) -> Result<Self> {
        let path = expand(path);
        if let Some(dir) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(&path).with_context(|| format!("opening {path}"))?;
        conn.execute_batch(proto::SCHEMA_SQL).context("applying schema")?;
        migrate(&conn).context("migrating schema")?;
        let transcripts = std::path::Path::new(&path)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("transcripts");
        std::fs::create_dir_all(&transcripts).ok();
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            transcripts: Arc::new(transcripts),
            pki: Arc::new(std::path::PathBuf::from(expand("~/.nexmesh/pki"))),
            pair_listen: Arc::new(None),
            nodes: Arc::new(Mutex::new(HashMap::new())),
            attaches: Arc::new(Mutex::new(HashMap::new())),
            respawning: Arc::new(Mutex::new(Default::default())),
            sync_waiters: Arc::new(Mutex::new(HashMap::new())),
            sync_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            tls: Arc::new(Mutex::new(None)),
            kicks: Arc::new(Mutex::new(HashMap::new())),
            bind_identity: Arc::new(true),
            trusted_proxies: Arc::new(Vec::new()),
        })
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(proto::SCHEMA_SQL)?;
        migrate(&conn)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            transcripts: Arc::new(std::env::temp_dir().join("nexmesh-test-transcripts")),
            pki: Arc::new(std::env::temp_dir().join("nexmesh-test-pki")),
            pair_listen: Arc::new(None),
            nodes: Arc::new(Mutex::new(HashMap::new())),
            attaches: Arc::new(Mutex::new(HashMap::new())),
            respawning: Arc::new(Mutex::new(Default::default())),
            sync_waiters: Arc::new(Mutex::new(HashMap::new())),
            sync_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            tls: Arc::new(Mutex::new(None)),
            kicks: Arc::new(Mutex::new(HashMap::new())),
            bind_identity: Arc::new(true),
            trusted_proxies: Arc::new(Vec::new()),
        })
    }

    /// Start a sync request and get the channel its answer will arrive on.
    pub fn new_sync_request(&self) -> (u64, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        let id = self
            .sync_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sync_waiters.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    pub fn complete_sync(&self, req_id: u64, value: serde_json::Value) {
        if let Some(tx) = self.sync_waiters.lock().unwrap().remove(&req_id) {
            let _ = tx.send(value);
        }
    }

    /// Is this hub serving TLS itself? Decides whether a session cookie is
    /// marked `Secure` -- marking it on a plain-HTTP hub would stop the
    /// browser sending it at all.
    pub fn tls_in_use(&self) -> bool {
        self.tls.lock().unwrap().is_some()
    }

    pub fn set_tls(&self, handle: TlsHandle) {
        *self.tls.lock().unwrap() = Some(handle);
    }

    /// Rebuild the TLS configuration, picking up the current revocation list,
    /// and swap it into the running server. False when TLS is not in use.
    pub async fn reload_tls(&self) -> bool {
        let handle = { self.tls.lock().unwrap().clone() };
        let Some(h) = handle else { return false };
        match crate::build_tls(&h.cert, &h.key, h.client_ca.as_deref(), &self.pki_dir()) {
            Ok(cfg) => {
                h.config.reload_from_config(cfg);
                true
            }
            Err(e) => {
                // Keep serving with the old configuration rather than fall
                // over; the caller reports that the reload did not happen.
                tracing::error!(error = %format!("{e:#}"), "could not reload TLS after revocation");
                false
            }
        }
    }

    /// Register a way to end a node's connection, and return it.
    pub fn kick_handle(&self, name: &str) -> Arc<tokio::sync::Notify> {
        let n = Arc::new(tokio::sync::Notify::new());
        self.kicks.lock().unwrap().insert(name.to_string(), n.clone());
        n
    }

    pub fn drop_kick_handle(&self, name: &str) {
        self.kicks.lock().unwrap().remove(name);
    }

    /// End a node's connection now. False if it was not connected.
    pub fn kick_node(&self, name: &str) -> bool {
        match self.kicks.lock().unwrap().get(name) {
            Some(n) => {
                n.notify_waiters();
                true
            }
            None => false,
        }
    }

    /// Claim the right to respawn a slot. False means somebody already has it.
    pub fn begin_respawn(&self, slot_id: &str) -> bool {
        self.respawning.lock().unwrap().insert(slot_id.to_string())
    }

    pub fn end_respawn(&self, slot_id: &str) {
        self.respawning.lock().unwrap().remove(slot_id);
    }

    pub fn pki_dir(&self) -> std::path::PathBuf {
        self.pki.as_ref().clone()
    }

    /// Must a client certificate name the same identity as the token sent with
    /// it? Normally yes: see the note in `peer.rs`. It can be turned off for a
    /// fleet whose certificates and tokens were issued separately, under
    /// different names, before this check existed.
    pub fn identity_binding(&self) -> bool {
        *self.bind_identity
    }

    pub fn with_identity_binding(mut self, on: bool) -> Self {
        self.bind_identity = Arc::new(on);
        self
    }

    /// Reverse proxies whose verified-client headers may be believed.
    pub fn with_trusted_proxies(mut self, proxies: Vec<std::net::IpAddr>) -> Self {
        self.trusted_proxies = Arc::new(proxies);
        self
    }

    pub fn trusted_proxies(&self) -> &[std::net::IpAddr] {
        &self.trusted_proxies
    }

    pub fn with_pki_dir(mut self, dir: &str) -> Self {
        self.pki = Arc::new(std::path::PathBuf::from(expand(dir)));
        self
    }

    pub fn with_pair_listen(mut self, addr: Option<String>) -> Self {
        self.pair_listen = Arc::new(addr);
        self
    }

    pub fn pair_listen(&self) -> Option<String> {
        self.pair_listen.as_ref().clone()
    }

    /// Is there a CA to sign pairing requests with?
    pub fn pki_ready(&self) -> bool {
        self.pki.join("ca-cert.pem").exists() && self.pki.join("ca-key.pem").exists()
    }

    // ------------------------------------------------------------ attach ----

    pub fn register_attach(&self, attach_id: &str, node_id: &str, tx: mpsc::Sender<AttachFrame>) {
        self.attaches.lock().unwrap().insert(
            attach_id.to_string(),
            AttachSink { tx, node_id: node_id.to_string() },
        );
    }

    pub fn drop_attach(&self, attach_id: &str) -> Option<String> {
        self.attaches.lock().unwrap().remove(attach_id).map(|s| s.node_id)
    }

    /// Route a frame to an attached operator. Returns false if nobody is there,
    /// which lets the caller tear the attach down rather than buffer forever.
    pub async fn send_attach(&self, attach_id: &str, frame: AttachFrame) -> bool {
        let tx = { self.attaches.lock().unwrap().get(attach_id).map(|s| s.tx.clone()) };
        match tx {
            Some(tx) => tx.send(frame).await.is_ok(),
            None => false,
        }
    }

    /// Every attach on a node that just went away, so their terminals can be
    /// told instead of silently freezing.
    pub fn attaches_on_node(&self, node_id: &str) -> Vec<String> {
        self.attaches
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.node_id == node_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    // -------------------------------------------------------- transcripts ----

    /// Append an agent's output so the operator can read what it actually did.
    /// The node streams this; dropping it meant an agent's reasoning and errors
    /// were unrecoverable the moment it exited.
    pub fn append_transcript(&self, agent_id: &str, data: &[u8]) {
        let Some(id) = safe_id(agent_id) else {
            tracing::warn!(agent_id, "refusing transcript for an unsafe agent id");
            return;
        };
        let path = self.transcripts.join(format!("{id}.log"));
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > TRANSCRIPT_MAX_BYTES {
                let _ = std::fs::rename(&path, self.transcripts.join(format!("{id}.log.1")));
            }
        }
        use std::io::Write;
        let _ = std::fs::create_dir_all(self.transcripts.as_path());
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                let _ = f.write_all(data);
            }
            Err(e) => tracing::warn!(agent_id, error = %e, "cannot write transcript"),
        }
    }

    /// The last `tail_bytes` of an agent's transcript.
    pub fn read_transcript(&self, agent_id: &str, tail_bytes: usize) -> Result<String> {
        let id = safe_id(agent_id).context("invalid agent id")?;
        let path = self.transcripts.join(format!("{id}.log"));
        let data = std::fs::read(&path).unwrap_or_default();
        let start = data.len().saturating_sub(tail_bytes);
        // Slice on a char boundary so a cut multi-byte sequence cannot panic.
        let mut s = start;
        while s < data.len() && (data[s] & 0b1100_0000) == 0b1000_0000 {
            s += 1;
        }
        Ok(String::from_utf8_lossy(&data[s..]).to_string())
    }

    // ------------------------------------------------------- connections ----

    pub fn attach_node(&self, node_id: &str, tx: mpsc::Sender<String>) {
        self.nodes.lock().unwrap().insert(node_id.to_string(), tx);
    }

    pub fn detach_node(&self, node_id: &str) {
        self.nodes.lock().unwrap().remove(node_id);
    }

    pub fn node_is_connected(&self, node_id: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(node_id)
    }

    /// Push a frame to a node. Returns an error if the node is not connected,
    /// so a caller can report "unreachable" rather than silently dropping it.
    pub async fn send_to_node(&self, node_id: &str, frame: &HubToNode) -> Result<()> {
        let tx = {
            let map = self.nodes.lock().unwrap();
            map.get(node_id).cloned()
        };
        let tx = tx.with_context(|| format!("node {node_id} is not connected"))?;
        tx.send(serde_json::to_string(frame)?)
            .await
            .with_context(|| format!("node {node_id} send queue closed"))?;
        Ok(())
    }

    // ------------------------------------------------------------- nodes ----

    /// Has operator authentication EVER been set up on this hub?
    ///
    /// A dashboard account counts: creating one is an explicit statement that
    /// this hub has an owner, so it must close the open case too. Otherwise
    /// setting up a password would leave the API wide open beside it.
    ///
    /// Deliberately counts revoked tokens too. If it only counted live ones,
    /// revoking your last token -- the thing you do the moment one leaks --
    /// would turn authentication off and leave the fleet wide open. Once auth
    /// exists, it stays required; revoking locks everyone out, which is the
    /// safe direction to fail.
    pub fn operator_auth_configured(&self) -> bool {
        let db = self.db.lock().unwrap();
        let tokens = db
            .query_row("SELECT 1 FROM operator_tokens LIMIT 1", [], |r| r.get::<_, i64>(0))
            .optional()
            .unwrap_or(None)
            .is_some();
        if tokens {
            return true;
        }
        db.query_row("SELECT 1 FROM dashboard_users LIMIT 1", [], |r| r.get::<_, i64>(0))
            .optional()
            .unwrap_or(None)
            .is_some()
    }

    /// Resolve an operator token to its label, recording the use.
    pub fn authenticate_operator(&self, token: &str) -> Option<String> {
        let hash = sha256_hex(token);
        let db = self.db.lock().unwrap();
        let label: Option<String> = db
            .query_row(
                "SELECT label FROM operator_tokens WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
                [&hash],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        if label.is_some() {
            let _ = db.execute(
                "UPDATE operator_tokens SET last_used_ms = ? WHERE token_sha256 = ?",
                params![now_ms(), hash],
            );
        }
        label
    }

    /// The label a live participant token was minted under, if it is one.
    pub fn participant_label(&self, token: &str) -> Option<String> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT label FROM participant_tokens
             WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
            [sha256_hex(token)],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn authenticate_node(&self, token: &str) -> Option<String> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT node_name FROM node_tokens WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
            [sha256_hex(token)],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// Upsert by NAME so a node keeps its id across restarts: slot history and
    /// audit entries stay attached to the same machine.
    pub fn register_node(&self, name: &str, hello: &proto::NodeToHub) -> Result<String> {
        let proto::NodeToHub::Hello { os, arch, node_version, labels, clis, .. } = hello else {
            anyhow::bail!("register_node called with a non-Hello frame");
        };
        let now = now_ms();
        let lease = now + proto::NODE_LEASE_MS as i64;
        let db = self.db.lock().unwrap();
        let id: String = db
            .query_row("SELECT id FROM nodes WHERE name = ?", [name], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        db.execute(
            "INSERT INTO nodes (id, name, os, arch, node_version, labels, status,
                                first_seen_ms, last_seen_ms, lease_expires_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'online', ?7, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET os = ?3, arch = ?4, node_version = ?5, labels = ?6,
                status = 'online', last_seen_ms = ?7, lease_expires_ms = ?8",
            params![id, name, os, arch, node_version, serde_json::to_string(labels)?, now, lease],
        )?;
        db.execute("DELETE FROM node_clis WHERE node_id = ?", [&id])?;
        for c in clis {
            db.execute(
                "INSERT INTO node_clis (node_id, cli_id, version, path) VALUES (?, ?, ?, ?)",
                params![id, c.cli_id, c.version, c.path],
            )?;
        }
        db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?, ?, ?, 'node_online', ?)",
            params![now, name, id, serde_json::to_string(clis)?],
        )?;
        Ok(id)
    }

    pub fn renew_node_lease(&self, node_id: &str) {
        let now = now_ms();
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE nodes SET last_seen_ms = ?, lease_expires_ms = ?, status = 'online' WHERE id = ?",
            params![now, now + proto::NODE_LEASE_MS as i64, node_id],
        );
    }

    /// A node whose lease expired is offline and its agents are `lost`. Their
    /// in-flight messages go back in the queue so a respawn elsewhere picks
    /// them up. The hub records all this; it never signals a process.
    pub fn expire_leases(&self) {
        let now = now_ms();
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE nodes SET status = 'offline' WHERE lease_expires_ms < ? AND status != 'offline'",
            [now],
        );
        let _ = db.execute(
            "UPDATE agents SET status = 'lost' WHERE exited_at_ms IS NULL AND node_id IN
                (SELECT id FROM nodes WHERE status = 'offline')",
            [],
        );
        // External agents keep their own lease.
        let _ = db.execute(
            "UPDATE agents SET status = 'lost' WHERE exited_at_ms IS NULL
               AND provenance = 'external' AND lease_expires_ms IS NOT NULL AND lease_expires_ms < ?",
            [now],
        );
        let _ = db.execute(
            "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
             WHERE acked_at_ms IS NULL AND inflight_agent IN
                (SELECT id FROM agents WHERE status IN ('lost', 'exited'))",
            [],
        );
    }

    pub fn mark_node_offline(&self, node_id: &str) {
        // Attach sessions on a node that has gone are dead; the terminals
        // holding them must be told rather than left hanging.
        for attach_id in self.attaches_on_node(node_id) {
            if let Some(sink) = self.attaches.lock().unwrap().remove(&attach_id) {
                let msg = serde_json::json!({ "t": "closed", "reason": "the node disconnected" });
                let _ = sink.tx.try_send(AttachFrame::Control(msg.to_string()));
            }
        }
        let db = self.db.lock().unwrap();
        let _ = db.execute("UPDATE nodes SET status = 'offline' WHERE id = ?", [node_id]);
        let _ = db.execute(
            "UPDATE agents SET status = 'lost' WHERE node_id = ? AND exited_at_ms IS NULL",
            [node_id],
        );
        // An ATTACHED agent reaches the hub through this node's unix socket,
        // so when the node goes the shim is gone with it -- there is no
        // process left to come back to that seat. Saying so is what lets the
        // next connection reclaim the seat instead of taking a new one: a
        // node restart used to leave a seat occupied by nobody, and three
        // restarts left three of them, all called the same thing.
        //
        // A MANAGED agent is left merely lost: its process may well still be
        // running, and the node can adopt it again when it returns.
        let _ = db.execute(
            "UPDATE agents SET exited_at_ms = ?
              WHERE node_id = ? AND exited_at_ms IS NULL AND provenance = 'attached'",
            params![now_ms(), node_id],
        );
        let _ = db.execute(
            "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
             WHERE acked_at_ms IS NULL AND inflight_agent IN
                (SELECT id FROM agents WHERE node_id = ? AND status = 'lost')",
            [node_id],
        );
    }

    pub fn nodes_json(&self) -> serde_json::Value {
        let db = self.db.lock().unwrap();
        let Ok(mut stmt) = db.prepare(
            "SELECT n.id, n.name, n.os, n.arch, n.status, n.last_seen_ms,
                    COALESCE(GROUP_CONCAT(c.cli_id), '')
             FROM nodes n LEFT JOIN node_clis c ON c.node_id = n.id
             GROUP BY n.id ORDER BY n.name",
        ) else {
            return serde_json::json!({ "nodes": [] });
        };
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], |r| {
                let clis: String = r.get(6)?;
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "os": r.get::<_, String>(2)?,
                    "arch": r.get::<_, String>(3)?,
                    "status": r.get::<_, String>(4)?,
                    "last_seen_ms": r.get::<_, i64>(5)?,
                    "clis": clis.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                }))
            })
            .and_then(|m| m.collect())
            .unwrap_or_default();
        drop(stmt);
        let connected: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|mut r| {
                let id = r["id"].as_str().unwrap_or("").to_string();
                r["connected"] = serde_json::Value::Bool(self.node_is_connected(&id));
                r
            })
            .collect();
        serde_json::json!({ "nodes": connected })
    }

    /// Choose a node for a slot: an explicit pin, then labels, then any online
    /// node that actually has the CLI installed.
    pub fn place(&self, cli_id: Option<&str>, placement: &proto::Placement) -> Result<String> {
        let db = self.db.lock().unwrap();
        if let Some(name) = &placement.node {
            let id: Option<String> = db
                .query_row("SELECT id FROM nodes WHERE name = ?", [name], |r| r.get(0))
                .optional()?;
            let id = id.with_context(|| format!("no node named '{name}'"))?;
            drop(db);
            anyhow::ensure!(self.node_is_connected(&id), "node '{name}' is not connected");
            if let Some(cli) = cli_id {
                let db = self.db.lock().unwrap();
                let has: Option<i64> = db
                    .query_row(
                        "SELECT 1 FROM node_clis WHERE node_id = ? AND cli_id = ?",
                        params![id, cli],
                        |r| r.get(0),
                    )
                    .optional()?;
                anyhow::ensure!(has.is_some(), "node '{name}' does not have '{cli}' installed");
            }
            return Ok(id);
        }

        let mut stmt = db.prepare(
            "SELECT n.id, n.name, n.labels FROM nodes n
             WHERE n.status = 'online'
               AND (?1 IS NULL OR EXISTS
                    (SELECT 1 FROM node_clis c WHERE c.node_id = n.id AND c.cli_id = ?1))
             ORDER BY n.name",
        )?;
        let candidates: Vec<(String, String, String)> = stmt
            .query_map([cli_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(db);

        for (id, _name, labels_json) in candidates {
            if !self.node_is_connected(&id) {
                continue;
            }
            if !placement.labels.is_empty() {
                let have: Vec<String> = serde_json::from_str(&labels_json).unwrap_or_default();
                if !placement.labels.iter().all(|w| have.contains(w)) {
                    continue;
                }
            }
            return Ok(id);
        }
        anyhow::bail!(
            "no connected node satisfies this placement{}",
            cli_id.map(|c| format!(" with '{c}' installed")).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod upgrade_tests {
    use super::*;

    /// A hub that already had the flat `knowledge` table must not look as
    /// though it forgot everything the moment memory gained scopes.
    #[test]
    fn knowledge_is_carried_into_scoped_memory() {
        let conn = Connection::open_in_memory().unwrap();
        // The original table, as an older hub created it.
        conn.execute_batch(
            "CREATE TABLE knowledge (
                session_id TEXT NOT NULL, key TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'context', value TEXT NOT NULL,
                author_slot TEXT, updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (session_id, key));
             INSERT INTO knowledge VALUES
                ('s1','db','decision','sqlite with WAL','sl-a',111),
                ('s1','style','convention','tabs',NULL,222);",
        )
        .unwrap();
        conn.execute_batch(proto::SCHEMA_SQL).unwrap();
        migrate(&conn).unwrap();

        let mut rows: Vec<(String, String, String, Option<String>)> = conn
            .prepare("SELECT key, value, scope, author_slot FROM memory
                       WHERE scope='session' AND scope_ref='s1' ORDER BY key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        rows.sort();
        assert_eq!(rows.len(), 2, "both entries must survive the upgrade");
        assert_eq!(rows[0].0, "db");
        assert_eq!(rows[0].1, "sqlite with WAL");
        assert_eq!(rows[0].2, "session", "and land in the session that learned them");
        assert_eq!(rows[0].3.as_deref(), Some("sl-a"), "keeping their author");

        // Idempotent: migrate runs on every open.
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "a second open must not duplicate them");
    }

    #[test]
    fn a_fresh_database_needs_no_carrying_over() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(proto::SCHEMA_SQL).unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_adds_a_column_missing_from_an_older_database() {
        // Simulate a hub.db created before `slots.status` existed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE slots (id TEXT PRIMARY KEY, session_id TEXT);
             CREATE TABLE agents (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        assert!(!has_column(&conn, "slots", "status").unwrap());
        migrate(&conn).unwrap();
        assert!(has_column(&conn, "slots", "status").unwrap());
        assert!(has_column(&conn, "slots", "participant_kind").unwrap());
        assert!(has_column(&conn, "agents", "lease_expires_ms").unwrap());
        // Idempotent: running it again must not fail.
        migrate(&conn).unwrap();
    }

    #[test]
    fn migration_skips_tables_a_database_does_not_have_yet() {
        // Altering a table that is not there is an error, and would stop the
        // hub starting against an older database.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE slots (id TEXT PRIMARY KEY);").unwrap();
        assert!(!table_exists(&conn, "sessions").unwrap());
        migrate(&conn).unwrap();
        assert!(has_column(&conn, "slots", "status").unwrap());
    }

    #[test]
    fn a_fresh_schema_needs_no_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(proto::SCHEMA_SQL).unwrap();
        assert!(has_column(&conn, "slots", "status").unwrap());
        migrate(&conn).unwrap();
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;

    fn fresh() -> AppState {
        let st = AppState::in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("nexmesh-tr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        AppState { transcripts: Arc::new(dir), ..st }
    }

    #[test]
    fn output_is_appended_and_read_back() {
        let st = fresh();
        st.append_transcript("ag-1", b"first line\n");
        st.append_transcript("ag-1", b"second line\n");
        let got = st.read_transcript("ag-1", 4096).unwrap();
        assert_eq!(got, "first line\nsecond line\n");
    }

    #[test]
    fn tailing_never_splits_a_utf8_character() {
        // Cutting mid-sequence on a byte boundary would produce replacement
        // characters or, with a naive slice, panic.
        let st = fresh();
        st.append_transcript("ag-2", "ααααα".as_bytes()); // 2 bytes each
        let got = st.read_transcript("ag-2", 5).unwrap();
        assert!(got.chars().all(|c| c == 'α'), "got {got:?}");
    }

    #[test]
    fn an_unsafe_agent_id_cannot_escape_the_transcript_directory() {
        // The id comes over the wire from a node and is used as a filename.
        let st = fresh();
        st.append_transcript("../../etc/nexmesh-pwned", b"nope");
        assert!(!std::path::Path::new("/etc/nexmesh-pwned").exists());
        assert!(st.read_transcript("../../etc/passwd", 10).is_err());
        assert!(st.read_transcript("ag-1/../../x", 10).is_err());
    }

    #[test]
    fn a_missing_transcript_reads_as_empty_rather_than_failing() {
        let st = fresh();
        assert_eq!(st.read_transcript("ag-never-ran", 100).unwrap(), "");
    }
}
