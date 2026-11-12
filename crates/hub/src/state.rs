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
        ("agents", "kind_hint", "TEXT"),
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
    for (table, column, decl) in wanted {
        if !has_column(conn, table, column)? {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
                .with_context(|| format!("adding {table}.{column}"))?;
            tracing::info!(table, column, "migrated: added column");
        }
    }
    Ok(())
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
    /// Deliberately counts revoked tokens too. If it only counted live ones,
    /// revoking your last token -- the thing you do the moment one leaks --
    /// would turn authentication off and leave the fleet wide open. Once auth
    /// exists, it stays required; revoking locks everyone out, which is the
    /// safe direction to fail.
    pub fn operator_auth_configured(&self) -> bool {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT 1 FROM operator_tokens LIMIT 1", [], |r| r.get::<_, i64>(0))
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
