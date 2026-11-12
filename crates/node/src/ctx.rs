//! Shared node state: live agents, pending hub round-trips, and the socket
//! back to the hub.

use nexmesh_proto::{self as proto, cli_descriptor::CliDescriptor, Envelope, ProcIdentity};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot};

/// Scrollback kept per agent so an attach can show what already happened
/// instead of starting from a blank screen. Capped: agent output is unbounded.
const RING_BYTES: usize = 256 * 1024;

/// The live I/O of one running agent, and whoever is watching it.
pub struct AgentIo {
    /// Keystrokes in. `None` for a pipe-run (headless) agent: there is no
    /// terminal to type into, so such an attach is view-only.
    pub input: Option<mpsc::Sender<Vec<u8>>>,
    /// The pty master, for window resizes. `None` for a pipe-run agent.
    pub master: Option<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    ring: Mutex<VecDeque<u8>>,
    attaches: Mutex<HashSet<String>>,
}

impl AgentIo {
    pub fn new(
        input: Option<mpsc::Sender<Vec<u8>>>,
        master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            input,
            master: master.map(Mutex::new),
            ring: Mutex::new(VecDeque::new()),
            attaches: Mutex::new(HashSet::new()),
        })
    }

    pub fn read_only(&self) -> bool {
        self.input.is_none()
    }

    pub fn push_output(&self, data: &[u8]) {
        let mut ring = self.ring.lock().unwrap();
        ring.extend(data.iter().copied());
        // Drop from the front rather than clearing, so recent context survives.
        let overflow = ring.len().saturating_sub(RING_BYTES);
        if overflow > 0 {
            ring.drain(..overflow);
        }
    }

    /// The last `n` bytes of scrollback, for replay on attach.
    pub fn scrollback(&self, n: usize) -> Vec<u8> {
        let ring = self.ring.lock().unwrap();
        let skip = ring.len().saturating_sub(n);
        ring.iter().skip(skip).copied().collect()
    }

    pub fn add_attach(&self, attach_id: &str) {
        self.attaches.lock().unwrap().insert(attach_id.to_string());
    }

    pub fn remove_attach(&self, attach_id: &str) {
        self.attaches.lock().unwrap().remove(attach_id);
    }

    pub fn attach_ids(&self) -> Vec<String> {
        self.attaches.lock().unwrap().iter().cloned().collect()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(m) = &self.master {
            let _ = m.lock().unwrap().resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRec {
    pub agent_id: String,
    pub slot_id: String,
    pub session_id: String,
    pub cli_id: String,
    pub proc: ProcIdentity,
}

pub struct ToolOutcome {
    pub result: serde_json::Value,
    pub deliver: Vec<Envelope>,
}

pub struct JoinOutcome {
    pub granted: Result<(String, String, String, Option<String>, Option<String>), String>,
}

pub struct NodeCtx {
    pub node_name: String,
    pub node_id: Mutex<String>,
    pub descriptors: Vec<CliDescriptor>,
    pub mcpd_path: String,
    pub sock_path: String,
    /// None while disconnected from the hub.
    pub hub_tx: Mutex<Option<mpsc::Sender<String>>>,
    pub agents: Mutex<HashMap<String, AgentRec>>,
    /// Live I/O per agent, keyed by agent id. Separate from `agents` so the
    /// metadata can be cloned cheaply without the pty handles.
    pub io: Mutex<HashMap<String, Arc<AgentIo>>>,
    tool_waiters: Mutex<HashMap<String, oneshot::Sender<ToolOutcome>>>,
    join_waiters: Mutex<HashMap<u64, oneshot::Sender<JoinOutcome>>>,
    seq: AtomicU64,
}

impl NodeCtx {
    pub fn new(
        node_name: String,
        descriptors: Vec<CliDescriptor>,
        mcpd_path: String,
        sock_path: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            node_name,
            node_id: Mutex::new(String::new()),
            descriptors,
            mcpd_path,
            sock_path,
            hub_tx: Mutex::new(None),
            agents: Mutex::new(HashMap::new()),
            io: Mutex::new(HashMap::new()),
            tool_waiters: Mutex::new(HashMap::new()),
            join_waiters: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn descriptor(&self, cli_id: &str) -> Option<&CliDescriptor> {
        self.descriptors.iter().find(|d| d.cli.id == cli_id)
    }

    pub async fn send_hub(&self, frame: &proto::NodeToHub) -> anyhow::Result<()> {
        let tx = { self.hub_tx.lock().unwrap().clone() };
        let tx = tx.ok_or_else(|| anyhow::anyhow!("not connected to the hub"))?;
        tx.send(serde_json::to_string(frame)?).await?;
        Ok(())
    }

    // -------------------------------------------------- hub round-trips ----

    pub fn register_tool_waiter(&self, call_id: String) -> oneshot::Receiver<ToolOutcome> {
        let (tx, rx) = oneshot::channel();
        self.tool_waiters.lock().unwrap().insert(call_id, tx);
        rx
    }

    pub fn complete_tool(&self, call_id: &str, outcome: ToolOutcome) {
        if let Some(tx) = self.tool_waiters.lock().unwrap().remove(call_id) {
            let _ = tx.send(outcome);
        }
    }

    pub fn drop_tool_waiter(&self, call_id: &str) {
        self.tool_waiters.lock().unwrap().remove(call_id);
    }

    pub fn register_join_waiter(&self, req_id: u64) -> oneshot::Receiver<JoinOutcome> {
        let (tx, rx) = oneshot::channel();
        self.join_waiters.lock().unwrap().insert(req_id, tx);
        rx
    }

    pub fn complete_join(&self, req_id: u64, outcome: JoinOutcome) {
        if let Some(tx) = self.join_waiters.lock().unwrap().remove(&req_id) {
            let _ = tx.send(outcome);
        }
    }

    /// When the hub connection drops, every outstanding round-trip must fail
    /// fast rather than leave an agent's tool call hanging forever.
    pub fn fail_all_waiters(&self) {
        self.tool_waiters.lock().unwrap().clear();
        self.join_waiters.lock().unwrap().clear();
    }

    pub fn agent(&self, agent_id: &str) -> Option<AgentRec> {
        self.agents.lock().unwrap().get(agent_id).cloned()
    }

    pub fn agent_io(&self, agent_id: &str) -> Option<Arc<AgentIo>> {
        self.io.lock().unwrap().get(agent_id).cloned()
    }

    /// Find which agent an attach belongs to, so input and closes can be routed
    /// without the caller having to remember.
    pub fn io_for_attach(&self, attach_id: &str) -> Option<Arc<AgentIo>> {
        self.io
            .lock()
            .unwrap()
            .values()
            .find(|io| io.attach_ids().iter().any(|a| a == attach_id))
            .cloned()
    }
}
