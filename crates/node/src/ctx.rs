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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoKind {
    /// A real terminal: attachable and take-over-able.
    Pty,
    /// Pipes. Still writable -- a driver injects through stdin -- but there is
    /// no terminal, so an attach to it is view-only.
    Pipes,
}

/// A message handed to a driver, waiting for the CLI to confirm it.
#[derive(Debug, Clone)]
pub struct Injected {
    pub text: String,
    pub message_ids: Vec<i64>,
}

/// The live I/O of one running agent, and whoever is watching it.
pub struct AgentIo {
    /// Keystrokes in. `None` for a pipe-run (headless) agent: there is no
    /// terminal to type into, so such an attach is view-only.
    pub input: Option<mpsc::Sender<Vec<u8>>>,
    /// The pty master, for window resizes. `None` for a pipe-run agent.
    pub master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    pub kind: IoKind,
    /// Drivers that are not byte streams -- Codex's JSON-RPC, opencode's HTTP
    /// server -- take whole messages rather than bytes on stdin.
    pub push: Option<mpsc::Sender<Vec<Envelope>>>,
    /// For an agent that runs its own server: where it is, and how to open a
    /// terminal onto it. Learned after startup, hence the lock.
    server: Mutex<Option<(String, Option<(String, Vec<String>)>)>>,
    ring: Mutex<VecDeque<u8>>,
    attaches: Mutex<HashSet<String>>,
    /// Injections written to the CLI but not yet echoed back, oldest first.
    pending: Mutex<VecDeque<Injected>>,
}

impl AgentIo {
    pub fn new(
        kind: IoKind,
        input: Option<mpsc::Sender<Vec<u8>>>,
        master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    ) -> Arc<Self> {
        Self::build(kind, input, master, None)
    }

    /// A driver whose injector takes messages, not bytes.
    pub fn new_push(push: mpsc::Sender<Vec<Envelope>>) -> Arc<Self> {
        Self::build(IoKind::Pipes, None, None, Some(push))
    }

    fn build(
        kind: IoKind,
        input: Option<mpsc::Sender<Vec<u8>>>,
        master: Option<Box<dyn portable_pty::MasterPty + Send>>,
        push: Option<mpsc::Sender<Vec<Envelope>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            input,
            master: Mutex::new(master),
            kind,
            push,
            server: Mutex::new(None),
            ring: Mutex::new(VecDeque::new()),
            attaches: Mutex::new(HashSet::new()),
            pending: Mutex::new(VecDeque::new()),
        })
    }

    /// Whether an ATTACHED VIEWER may type. Pipes are writable by a driver but
    /// are not a terminal, so a human attaching to them can only watch.
    /// The same agent, minus anything that can write to it. Scrollback and
    /// attach state are kept so a viewer watching it finish is not cut off.
    pub fn without_input(&self) -> Arc<Self> {
        Arc::new(Self {
            input: None,
            master: Mutex::new(None),
            kind: self.kind,
            push: None,
            server: Mutex::new(self.server.lock().unwrap().clone()),
            ring: Mutex::new(self.ring.lock().unwrap().clone()),
            attaches: Mutex::new(self.attaches.lock().unwrap().clone()),
            pending: Mutex::new(Default::default()),
        })
    }

    /// Does this agent have a pty of its own?
    pub fn has_pty(&self) -> bool {
        self.master.lock().unwrap().is_some()
    }

    /// Can a message be TYPED at this agent?
    ///
    /// A pty with an input channel is a keyboard, and a CLI sitting at its
    /// own prompt reads what is typed there exactly as it reads what a human
    /// types. That is a delivery route for an interactive agent, which
    /// otherwise has none until it calls a tool of its own accord.
    pub fn typable(&self) -> bool {
        self.kind == IoKind::Pty && self.input.is_some()
    }

    /// What the hub records for this agent's I/O. `None` for an agent the
    /// node never started: there is no terminal and no pipe to speak of.
    pub fn io_kind_name(&self) -> &'static str {
        match self.kind {
            IoKind::Pty => "pty",
            IoKind::Pipes => "pipes",
        }
    }

    pub fn read_only(&self) -> bool {
        // An agent that runs its own server can have a terminal opened onto it
        // on demand, so it is not read-only even though it has no pty of its
        // own.
        self.kind != IoKind::Pty && self.attach_command().is_none()
    }

    pub fn set_server(&self, url: String, attach_cmd: Option<(String, Vec<String>)>) {
        *self.server.lock().unwrap() = Some((url, attach_cmd));
    }

    /// The command that opens a terminal onto this agent's server, with the
    /// URL filled in. `None` when there is no server, or no way to view it.
    pub fn attach_command(&self) -> Option<(String, Vec<String>)> {
        let g = self.server.lock().unwrap();
        let (url, cmd) = g.as_ref()?;
        let (bin, args) = cmd.as_ref()?;
        Some((
            bin.clone(),
            args.iter().map(|a| a.replace("{url}", url)).collect(),
        ))
    }

    pub fn push_pending(&self, injected: Injected) {
        self.pending.lock().unwrap().push_back(injected);
    }

    /// Match a replayed message back to what we injected.
    ///
    /// The CLI strips any custom field we add, so the echoed text is the only
    /// correlator (verified against claude 2.1.322). Replays arrive in order,
    /// so an exact text match is taken oldest-first; if the text does not
    /// match anything we still retire the oldest entry rather than leak it,
    /// because a message stuck "in flight" forever is never redelivered.
    pub fn ack_replay(&self, text: &str) -> Vec<i64> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(idx) = pending.iter().position(|p| p.text == text) {
            return pending.remove(idx).map(|p| p.message_ids).unwrap_or_default();
        }
        match pending.pop_front() {
            Some(p) => {
                tracing::warn!(
                    "a replayed message did not match what was injected; \
                     retiring the oldest injection anyway so it is not stuck"
                );
                p.message_ids
            }
            None => vec![],
        }
    }

    /// Retire pending injections by the ids a driver says it delivered.
    ///
    /// A plugin names the message ids it got through, so unlike a stream-json
    /// replay there is nothing to correlate -- but only ids we are actually
    /// holding are returned, so a plugin cannot ack a message twice, ack one
    /// belonging to another agent, or invent one. An entry is dropped once
    /// every id in it has been claimed.
    pub fn ack_ids(&self, ids: &[i64]) -> Vec<i64> {
        let mut pending = self.pending.lock().unwrap();
        let mut acked = Vec::new();
        for entry in pending.iter_mut() {
            entry.message_ids.retain(|id| {
                if ids.contains(id) {
                    acked.push(*id);
                    false
                } else {
                    true
                }
            });
        }
        pending.retain(|e| !e.message_ids.is_empty());
        acked
    }

    /// Injections still outstanding, for release when the agent dies.
    pub fn drain_pending(&self) -> Vec<i64> {
        self.pending
            .lock()
            .unwrap()
            .drain(..)
            .flat_map(|p| p.message_ids)
            .collect()
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
        if let Some(m) = self.master.lock().unwrap().as_ref() {
            let _ = m.resize(portable_pty::PtySize {
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
    /// How this agent is driven, which decides whether the hub pushes messages
    /// to it or lets them ride on its next tool result.
    pub driver: nexmesh_proto::cli_descriptor::DriverSpec,
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
    /// Terminal clients opened on demand for agents that have no pty of their
    /// own, keyed by attach id.
    viewers: Mutex<HashMap<String, Arc<crate::viewer::Viewer>>>,
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
            viewers: Mutex::new(HashMap::new()),
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

    pub fn add_viewer(&self, attach_id: &str, v: crate::viewer::Viewer) {
        self.viewers.lock().unwrap().insert(attach_id.to_string(), Arc::new(v));
    }

    pub fn viewer(&self, attach_id: &str) -> Option<Arc<crate::viewer::Viewer>> {
        self.viewers.lock().unwrap().get(attach_id).cloned()
    }

    pub fn remove_viewer(&self, attach_id: &str) -> Option<Arc<crate::viewer::Viewer>> {
        self.viewers.lock().unwrap().remove(attach_id)
    }

    /// Drop an agent's input channels, so a CLI reading stdin sees EOF.
    ///
    /// The `AgentIo` is replaced rather than mutated: the writer task holding
    /// the other end exits when the last sender goes, and that is what closes
    /// the pipe.
    pub fn close_agent_input(&self, agent_id: &str) {
        let mut map = self.io.lock().unwrap();
        if let Some(old) = map.remove(agent_id) {
            map.insert(agent_id.to_string(), old.without_input());
        }
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
