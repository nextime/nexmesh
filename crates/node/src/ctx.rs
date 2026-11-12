//! Shared node state: live agents, pending hub round-trips, and the socket
//! back to the hub.

use nexmesh_proto::{self as proto, cli_descriptor::CliDescriptor, Envelope, ProcIdentity};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot};

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
}
