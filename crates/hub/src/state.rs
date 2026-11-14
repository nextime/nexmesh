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
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
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
        // Whether this agent has a terminal of its own (`pty`), runs on pipes
        // (`pipes`), or is a process the node never started and has no I/O
        // for at all (NULL). It decides whether a message can be TYPED at the
        // agent, and it is the difference between a seat you can take over
        // and one you can only read -- which used to be discoverable only by
        // attaching and finding out.
        ("agents", "io_kind", "TEXT"),
        // A seat whose CLI is sitting in a terminal some node has adopted.
        // `terminal_via` is the adopted agent whose pty the keystrokes go
        // through, and the pid/ticks are the CLI that must still be in that
        // pane for typing to be allowed. Nothing cleans these up and nothing
        // needs to: every use joins the via agent, so a link dies with the
        // adoption it depends on.
        ("slots", "terminal_via", "TEXT"),
        ("slots", "terminal_cli_pid", "INTEGER"),
        ("slots", "terminal_cli_ticks", "INTEGER"),
        ("slots", "terminal_at_ms", "INTEGER"),
        ("slots", "model", "TEXT"),
        ("slots", "capabilities", "TEXT NOT NULL DEFAULT '[]'"),
        ("agents", "model", "TEXT"),
        ("sessions", "token_budget", "INTEGER"),
        ("sessions", "budget_hit_at_ms", "INTEGER"),
        // How far an operator's reach goes. NULL is the whole fleet -- a
        // SUPER orchestrator -- and a session id confines the token to that
        // one session. Nullable on purpose: every token minted before this
        // existed was fleet-wide, and that is what NULL already means.
        ("operator_tokens", "session_id", "TEXT"),
        // Whether a note on the operator channel came from a program driving
        // a session or from an AGENT inside one. Both carry a from_session,
        // so without this an orchestrator cannot tell a peer's message from
        // one of its own team asking for help.
        ("operator_messages", "from_kind", "TEXT NOT NULL DEFAULT 'operator'"),
        // Delivered is not read. See schema.sql.
        ("messages", "claimed_at_ms", "INTEGER"),
        ("messages", "claimed_by", "TEXT"),
        // WHICH limit cut a scan short. "partial" on its own sends you
        // looking for the wrong fix -- a tree of data files that exhausts
        // the walk and one of source files that exhausts the clock want
        // opposite answers.
        ("code_graph_meta", "truncated_reason", "TEXT"),
        // The dashboard's view of a graph, computed when the graph is
        // stored. It was computed per poll, which for a large workspace is
        // half a minute of SQL against a page that asks every few seconds.
        ("code_graph_meta", "summary_json", "TEXT"),
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
    // The operator channel: how a session's own orchestrator reaches the
    // orchestrators who can see the whole fleet, and how they answer.
    //
    // Separate from `messages`, which belongs to a session and is read by its
    // agents. These are between the people (or programs) DRIVING sessions:
    // a scoped orchestrator cannot see another session exists, so when it
    // needs something from outside its own team the only honest thing it can
    // do is ask someone whose reach is wider.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS operator_messages (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            from_label    TEXT NOT NULL,
            from_kind     TEXT NOT NULL DEFAULT 'operator',  -- operator | agent
            from_session  TEXT,            -- NULL when a super sent it
            to_session    TEXT,            -- NULL = addressed to the supers
            body          TEXT NOT NULL,
            sent_at_ms    INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS operator_messages_to ON operator_messages(to_session, id);
         CREATE TABLE IF NOT EXISTS operator_read_cursor (
            label     TEXT PRIMARY KEY,
            last_id   INTEGER NOT NULL DEFAULT 0
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
            truncated_reason TEXT,
            summary_json TEXT,
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
    // Per-seat delivery. An existing hub has messages that were acked under
    // the old rule -- one ack for the whole team -- and re-offering those to
    // everyone on upgrade would replay old conversation at a fleet. What was
    // acked before this existed is acked for everyone, so it is written down
    // that way, once.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_recipients (
            message_id     INTEGER NOT NULL,
            slot_id        TEXT NOT NULL,
            inflight_agent TEXT,
            inflight_at_ms INTEGER,
            acked_at_ms    INTEGER,
            acked_by_agent TEXT,
            PRIMARY KEY (message_id, slot_id))",
    )?;
    if table_exists(conn, "messages")? && table_exists(conn, "slots")? {
        let seeded = conn.execute(
            "INSERT OR IGNORE INTO message_recipients (message_id, slot_id, acked_at_ms)
             SELECT m.id, s.id, m.acked_at_ms
               FROM messages m JOIN slots s ON s.session_id = m.session_id
              WHERE m.to_slot IS NULL AND m.acked_at_ms IS NOT NULL",
            [],
        )?;
        if seeded > 0 {
            tracing::info!(rows = seeded, "migrated: broadcasts already read stay read");
        }
    }

    // `lost` was the wrong word and it was on the operator's screen. What the
    // hub knows is that it has no connection to the process -- not that the
    // process is gone, and certainly not that the conversation is. Renamed in
    // place so an upgraded hub does not go on showing a word it no longer
    // writes, next to seats whose CLIs are still sitting there.
    // Guarded on the COLUMN, not just the table: migrate() also runs against
    // databases older than most of this schema, where `agents` may be little
    // more than an id, and a statement naming a column that is not there
    // fails the whole migration and the hub with it.
    if table_exists(conn, "agents")? && has_column(conn, "agents", "status")? {
        let renamed = conn.execute(
            "UPDATE agents SET status = 'disconnected' WHERE status = 'lost'",
            [],
        )?;
        if renamed > 0 {
            tracing::info!(agents = renamed, "migrated: `lost` is now `disconnected`");
        }
    }

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
/// An authenticated operator, and how far it reaches.
///
/// `session: None` is a SUPER orchestrator: the whole fleet, every session,
/// and the only one that may create a session or move a seat between two of
/// them. `session: Some(id)` is that session's own orchestrator -- it never
/// has to name its session and may not name another, so a scoped token
/// cannot read, drive or even discover the rest of the fleet.
#[derive(Clone, Debug)]
pub struct Operator {
    pub label: String,
    pub session: Option<String>,
}

impl Operator {
    pub fn is_super(&self) -> bool {
        self.session.is_none()
    }
    /// A signed-in dashboard account, or an unauthenticated hub on loopback.
    /// A person at the dashboard sees the whole fleet.
    pub fn super_as(label: &str) -> Self {
        Self { label: label.to_string(), session: None }
    }
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
    /// Code graphs arriving in pieces, keyed by the request they answer.
    /// Dropped if the node goes away mid-stream: half a graph is worse than
    /// none, because nothing about it says which half is missing.
    graph_parts: Arc<Mutex<HashMap<u64, (i64, nexmesh_proto::codegraph::CodeGraph)>>>,
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
            graph_parts: Arc::new(Mutex::new(HashMap::new())),
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
            graph_parts: Arc::new(Mutex::new(HashMap::new())),
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

    /// Take in one piece of a code graph, and return the whole thing once
    /// the last piece lands.
    ///
    /// Pieces from an abandoned stream would otherwise sit here for ever, so
    /// anything older than a quarter of an hour is dropped whenever a new
    /// stream starts: a node that died mid-graph is not coming back to
    /// finish it.
    pub fn take_graph_piece(
        &self,
        req_id: u64,
        piece: nexmesh_proto::codegraph::CodeGraph,
        last: bool,
    ) -> Option<nexmesh_proto::codegraph::CodeGraph> {
        let mut parts = self.graph_parts.lock().unwrap();
        assemble_piece(&mut parts, req_id, piece, last, nexmesh_proto::now_ms())
    }

    /// Hand a node's answer to whoever asked. False when nobody is there to
    /// take it any more -- the caller timed out, or went away -- which is
    /// worth knowing for an answer that cost minutes to produce.
    pub fn complete_sync(&self, req_id: u64, value: serde_json::Value) -> bool {
        match self.sync_waiters.lock().unwrap().remove(&req_id) {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
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
        self.operator_identity(token).map(|o| o.label)
    }

    /// Who this token is, and how far it reaches.
    pub fn operator_identity(&self, token: &str) -> Option<Operator> {
        let hash = sha256_hex(token);
        let db = self.db.lock().unwrap();
        let found: Option<(String, Option<String>)> = db
            .query_row(
                "SELECT label, session_id FROM operator_tokens
                  WHERE token_sha256 = ? AND revoked_at_ms IS NULL",
                [&hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap_or(None);
        if found.is_some() {
            let _ = db.execute(
                "UPDATE operator_tokens SET last_used_ms = ? WHERE token_sha256 = ?",
                params![now_ms(), hash],
            );
        }
        found.map(|(label, session_id)| Operator { label, session: session_id })
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

    /// A node whose lease expired is offline and its agents are
    /// `disconnected`. Their in-flight messages go back in the queue so a
    /// respawn elsewhere picks them up. The hub records all this; it never
    /// signals a process.
    pub fn expire_leases(&self) {
        let now = now_ms();
        let db = self.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE nodes SET status = 'offline' WHERE lease_expires_ms < ? AND status != 'offline'",
            [now],
        );
        let _ = db.execute(
            "UPDATE agents SET status = 'disconnected' WHERE exited_at_ms IS NULL AND node_id IN
                (SELECT id FROM nodes WHERE status = 'offline')",
            [],
        );
        // External agents keep their own lease.
        let _ = db.execute(
            "UPDATE agents SET status = 'disconnected' WHERE exited_at_ms IS NULL
               AND provenance = 'external' AND lease_expires_ms IS NOT NULL AND lease_expires_ms < ?",
            [now],
        );
        // Nothing may read `running` that nothing has vouched for. A node
        // confirms its agents against /proc every heartbeat, so a gap of a
        // whole lease means no node is claiming this process -- either it
        // died, or the node that knew about it was replaced by one that does
        // not. Without this a hub restart froze whatever status happened to
        // be in the table, and an agent stayed `running` for as long as the
        // hub stayed up: the one state that makes the fleet lie rather than
        // merely look stale.
        //
        // `disconnected` and not `lost`, which is what this said for a year:
        // the hub has lost its CONNECTION to the process, and nothing more
        // than that. The CLI is very often still sitting there, perfectly
        // alive, waiting to be typed at -- a word that declares it gone is
        // the hub overstating what it knows.
        let _ = db.execute(
            "UPDATE agents SET status = 'disconnected'
             WHERE exited_at_ms IS NULL AND status = 'running'
               AND provenance <> 'external' AND last_seen_ms < ?",
            [now - proto::NODE_LEASE_MS as i64],
        );
        let _ = db.execute(
            "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
             WHERE acked_at_ms IS NULL AND inflight_agent IN
                (SELECT id FROM agents WHERE status IN ('disconnected', 'exited'))",
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
            "UPDATE agents SET status = 'disconnected' WHERE node_id = ? AND exited_at_ms IS NULL",
            [node_id],
        );
        // An ATTACHED agent reaches the hub through this node's unix socket,
        // so when the node goes the shim is gone with it -- there is no
        // process left to come back to that seat. Saying so is what lets the
        // next connection reclaim the seat instead of taking a new one: a
        // node restart used to leave a seat occupied by nobody, and three
        // restarts left three of them, all called the same thing.
        //
        // A MANAGED agent is left merely disconnected: its process may well
        // still be running, and the node can adopt it again when it returns.
        let _ = db.execute(
            "UPDATE agents SET exited_at_ms = ?
              WHERE node_id = ? AND exited_at_ms IS NULL AND provenance = 'attached'",
            params![now_ms(), node_id],
        );
        let _ = db.execute(
            "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
             WHERE acked_at_ms IS NULL AND inflight_agent IN
                (SELECT id FROM agents WHERE node_id = ? AND status = 'disconnected')",
            [node_id],
        );
    }

    /// At startup, no node has connected yet, so no agent has been confirmed
    /// by anyone. Whatever the table says is a claim from before the restart.
    pub fn demote_running_agents(&self) -> usize {
        demote_running(&self.db.lock().unwrap())
    }

    /// Believe a reconnecting node about which of its agents are still here.
    ///
    /// The hub's record of a node's agents outlives the node; the node's own
    /// record does not. A restarted node therefore arrives knowing nothing of
    /// the processes its predecessor spawned, and nothing it ever sends will
    /// mention them again -- so before this they were kept for ever: `lost`
    /// if a lease sweep had caught them, `running` if the hub had itself
    /// restarted in between. Either way the seat stayed occupied by a process
    /// that no longer existed.
    ///
    /// An agent the node does not list has gone. Marking it exited is both
    /// the truth and the thing that frees its seat for the next connection.
    /// A managed process that somehow outlived its node is included in that:
    /// the hub can no longer reach it, and a seat it cannot drive is worth
    /// less than a seat the next agent can claim.
    pub fn reconcile_node_agents(&self, node_id: &str, live: &[String]) -> Vec<String> {
        reconcile_agents(&self.db.lock().unwrap(), node_id, live)
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

pub fn reconcile_agents(db: &Connection, node_id: &str, live: &[String]) -> Vec<String> {
    let mut sql = String::from(
        "SELECT id FROM agents WHERE node_id = ? AND exited_at_ms IS NULL",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(node_id.to_string())];
    if !live.is_empty() {
        sql.push_str(" AND id NOT IN (");
        sql.push_str(&vec!["?"; live.len()].join(","));
        sql.push(')');
        for id in live {
            args.push(Box::new(id.clone()));
        }
    }
    let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
    let Ok(mut stmt) = db.prepare(&sql) else { return Vec::new() };
    let Ok(rows) = stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    let gone: Vec<String> = rows.filter_map(Result::ok).collect();
    let now = now_ms();
    for id in &gone {
        let _ = db.execute(
            "UPDATE agents SET status = 'exited', exited_at_ms = ? WHERE id = ?",
            params![now, id],
        );
        // Work handed to it but never acked goes back in the queue, and
        // anyone blocked on a request it was serving is told, exactly as
        // if the node had reported the exit itself.
        let _ = db.execute(
            "UPDATE messages SET inflight_agent = NULL, inflight_at_ms = NULL
             WHERE inflight_agent = ? AND acked_at_ms IS NULL",
            [id],
        );
        if let Ok(Some(slot)) = db
            .query_row("SELECT slot_id FROM agents WHERE id = ?", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
        {
            let _ = crate::asks::fail_outstanding(&db, &slot);
        }
    }
    gone
}

/// At startup nothing is connected, so nothing has been confirmed by anyone.
pub fn demote_running(db: &Connection) -> usize {
    db.execute(
        "UPDATE agents SET status = 'disconnected'
         WHERE exited_at_ms IS NULL AND status = 'running' AND provenance <> 'external'",
        [],
    )
    .unwrap_or(0)
}

/// Add one piece to a graph being assembled, and return the whole thing once
/// the last piece lands.
///
/// Separate from the state it lives in so the assembly itself can be tested:
/// the failure it exists to prevent -- half a graph stored as though it were
/// all of one -- is silent, and a silent failure needs a test more than a
/// loud one does.
fn assemble_piece(
    parts: &mut HashMap<u64, (i64, nexmesh_proto::codegraph::CodeGraph)>,
    req_id: u64,
    piece: nexmesh_proto::codegraph::CodeGraph,
    last: bool,
    now: i64,
) -> Option<nexmesh_proto::codegraph::CodeGraph> {
    parts.retain(|_, (at, _)| now - *at < 15 * 60 * 1000);
    let entry = parts.entry(req_id).or_insert_with(|| (now, Default::default()));
    entry.0 = now;
    entry.1.nodes.extend(piece.nodes);
    entry.1.edges.extend(piece.edges);
    if !last {
        return None;
    }
    let (_, mut whole) = parts.remove(&req_id)?;
    // The counts describe the scan, not the piece, so they come from the one
    // that completes it.
    whole.files = piece.files;
    whole.skipped = piece.skipped;
    whole.truncated = piece.truncated;
    whole.truncated_reason = piece.truncated_reason;
    Some(whole)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod graph_pieces {
        use super::*;
        use nexmesh_proto::codegraph::{CodeGraph, GraphNode, NodeKind};

        fn piece(names: &[&str]) -> CodeGraph {
            CodeGraph {
                nodes: names
                    .iter()
                    .map(|n| GraphNode {
                        id: n.to_string(),
                        kind: NodeKind::Function,
                        name: n.to_string(),
                        path: "a.rs".into(),
                        line: 1,
                        lang: "rust".into(),
                    })
                    .collect(),
                ..Default::default()
            }
        }

        #[test]
        fn nothing_comes_out_until_the_last_piece_is_in() {
            let mut parts = HashMap::new();
            assert!(assemble_piece(&mut parts, 1, piece(&["a"]), false, 1000).is_none());
            assert!(assemble_piece(&mut parts, 1, piece(&["b"]), false, 1001).is_none());
            let whole = assemble_piece(&mut parts, 1, piece(&["c"]), true, 1002)
                .expect("the last piece completes it");
            let names: Vec<&str> = whole.nodes.iter().map(|n| n.name.as_str()).collect();
            assert_eq!(names, ["a", "b", "c"], "in the order they were sent");
            assert!(parts.is_empty(), "a finished graph is not left behind");
        }

        #[test]
        fn the_counts_come_from_the_piece_that_finishes_it() {
            // They describe the whole scan, and until it ends there is
            // nothing to describe.
            let mut parts = HashMap::new();
            assemble_piece(&mut parts, 7, piece(&["a"]), false, 1000);
            let mut tail = piece(&["b"]);
            tail.files = 12;
            tail.truncated = true;
            tail.truncated_reason = Some("the parse ran out of time".into());
            let whole = assemble_piece(&mut parts, 7, tail, true, 1001).unwrap();
            assert_eq!(whole.files, 12);
            assert!(whole.truncated);
            assert_eq!(whole.truncated_reason.as_deref(), Some("the parse ran out of time"));
            assert_eq!(whole.nodes.len(), 2);
        }

        #[test]
        fn two_graphs_arriving_at_once_do_not_mix() {
            let mut parts = HashMap::new();
            assemble_piece(&mut parts, 1, piece(&["mine"]), false, 1000);
            assemble_piece(&mut parts, 2, piece(&["yours"]), false, 1000);
            let first = assemble_piece(&mut parts, 1, piece(&["mine2"]), true, 1001).unwrap();
            let names: Vec<&str> = first.nodes.iter().map(|n| n.name.as_str()).collect();
            assert_eq!(names, ["mine", "mine2"]);
        }

        #[test]
        fn a_stream_that_was_abandoned_is_not_kept_for_ever() {
            // A node that died halfway is not coming back to finish, and
            // half a graph held in memory helps nobody.
            let mut parts = HashMap::new();
            assemble_piece(&mut parts, 1, piece(&["orphan"]), false, 1000);
            assemble_piece(&mut parts, 2, piece(&["later"]), false, 1000 + 16 * 60 * 1000);
            assert!(!parts.contains_key(&1), "the abandoned one is dropped");
            assert!(parts.contains_key(&2));
        }
    }

    /// A database with one node and two agents on it, both live.
    fn fleet() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
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
        for (slot, agent, prov) in [
            ("sl-a", "ag-a", "managed"),
            ("sl-b", "ag-b", "attached"),
        ] {
            db.execute(
                "INSERT INTO slots (id,session_id,display_name,created_at_ms) VALUES (?,'s1',?,1)",
                params![slot, slot],
            )
            .unwrap();
            db.execute(
                "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
                 VALUES (?,?,'nd-1',?,'running',1,?)",
                params![agent, slot, prov, now_ms()],
            )
            .unwrap();
        }
        db
    }

    fn status_of(db: &Connection, agent: &str) -> (String, Option<i64>) {
        db.query_row("SELECT status, exited_at_ms FROM agents WHERE id = ?", [agent], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    #[test]
    fn a_node_that_comes_back_with_nothing_frees_every_seat_it_held() {
        // The bug: a node keeps its agent table in memory, so a restarted
        // node never mentions the processes its predecessor spawned. Nothing
        // then resolved those agents, and their seats stayed occupied by
        // processes that no longer existed -- for as long as the hub ran.
        let db = fleet();
        let gone = reconcile_agents(&db, "nd-1", &[]);
        assert_eq!(gone.len(), 2, "both agents are unaccounted for");
        for a in ["ag-a", "ag-b"] {
            let (status, exited) = status_of(&db, a);
            assert_eq!(status, "exited");
            assert!(exited.is_some(), "a freed seat must have an exit time");
        }
    }

    #[test]
    fn an_agent_the_node_still_has_is_left_alone() {
        let db = fleet();
        let gone = reconcile_agents(&db, "nd-1", &["ag-b".to_string()]);
        assert_eq!(gone, vec!["ag-a".to_string()]);
        assert_eq!(status_of(&db, "ag-b").0, "running");
        assert!(status_of(&db, "ag-b").1.is_none());
    }

    #[test]
    fn reconciling_one_node_does_not_touch_another() {
        let db = fleet();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,last_seen_ms,
                                 lease_expires_ms)
             VALUES ('nd-2','machine-b','linux','x86_64','0.1.0','online',1,1,9999999999999)",
            [],
        )
        .unwrap();
        db.execute("UPDATE agents SET node_id = 'nd-2' WHERE id = 'ag-b'", []).unwrap();
        reconcile_agents(&db, "nd-1", &[]);
        assert_eq!(status_of(&db, "ag-b").0, "running", "the other machine is none of its business");
    }

    #[test]
    fn work_handed_to_a_freed_agent_goes_back_in_the_queue() {
        let db = fleet();
        db.execute(
            "INSERT INTO messages (id,session_id,from_slot,to_slot,body,sent_at_ms,
                                   inflight_agent,inflight_at_ms)
             VALUES (1,'s1','sl-b','sl-a','do the thing',1,'ag-a',1)",
            [],
        )
        .unwrap();
        reconcile_agents(&db, "nd-1", &[]);
        let inflight: Option<String> = db
            .query_row("SELECT inflight_agent FROM messages WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert!(inflight.is_none(), "an unacked message must not stay in flight to a dead agent");
    }

    #[test]
    fn an_already_exited_agent_is_not_reported_as_newly_gone() {
        let db = fleet();
        reconcile_agents(&db, "nd-1", &[]);
        assert!(reconcile_agents(&db, "nd-1", &[]).is_empty(), "idempotent");
    }

    #[test]
    fn a_restart_demotes_agents_nothing_has_confirmed() {
        // What made the worse half of the bug: on a hub restart the stored
        // status was taken as fact, so an agent read `running` hours after
        // its process died -- the one state that makes the fleet lie rather
        // than merely look stale.
        let db = fleet();
        db.execute(
            "INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms,
                                 lease_expires_ms)
             VALUES ('ag-x','sl-a','external','running',1,?,?)",
            params![now_ms(), now_ms() + 60_000],
        )
        .unwrap();
        assert_eq!(demote_running(&db), 2);
        assert_eq!(status_of(&db, "ag-a").0, "disconnected");
        assert_eq!(status_of(&db, "ag-b").0, "disconnected");
        assert_eq!(
            status_of(&db, "ag-x").0,
            "running",
            "an external agent keeps its own lease and is not a node's to demote"
        );
        // Demotion is not death: nothing is marked exited, so a node that
        // still has the process says so and it is running again.
        assert!(status_of(&db, "ag-a").1.is_none());
    }

    #[test]
    fn expiring_leases_cannot_leave_a_running_agent_nobody_confirmed() {
        let db = fleet();
        let stale = now_ms() - proto::NODE_LEASE_MS as i64 - 1;
        db.execute("UPDATE agents SET last_seen_ms = ? WHERE id = 'ag-a'", [stale]).unwrap();
        db.execute(
            "UPDATE agents SET status = 'disconnected'
             WHERE exited_at_ms IS NULL AND status = 'running'
               AND provenance <> 'external' AND last_seen_ms < ?",
            [now_ms() - proto::NODE_LEASE_MS as i64],
        )
        .unwrap();
        assert_eq!(status_of(&db, "ag-a").0, "disconnected");
        assert_eq!(status_of(&db, "ag-b").0, "running");
    }

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
