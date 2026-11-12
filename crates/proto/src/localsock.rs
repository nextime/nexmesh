//! The node↔mcpd local protocol, newline-delimited JSON over a unix socket.
//!
//! `mcpd` is deliberately dumb: it speaks MCP to its CLI and forwards every
//! tool call to the local node, which relays to the hub. All tool logic lives
//! in one place (the hub), and mcpd holds no credentials -- a compromised
//! agent gains no fleet access, and a hub outage cannot wedge an agent's
//! stdio.

use crate::{Envelope, ToolDef};
use serde::{Deserialize, Serialize};

/// Where mcpd expects to find its node, overridable with `NEXMESH_SOCK`.
pub const DEFAULT_SOCKET: &str = "/run/nexmesh/node.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum McpdToNode {
    /// First frame. A managed agent passes the ids the node gave it in its
    /// environment; an attached agent (a human ran the CLI by hand) passes
    /// none and the hub creates a slot for it.
    Register {
        cli_id: String,
        pid: i32,
        cwd: String,
        agent_hint: Option<String>,
        slot_hint: Option<String>,
        session_hint: Option<String>,
        role: Option<String>,
        name: Option<String>,
    },
    Tool {
        req_id: u64,
        tool: String,
        args: serde_json::Value,
    },
    /// Sent once the tool result -- messages included -- has been written back
    /// to the CLI. Only then is a message considered delivered.
    Ack { message_ids: Vec<i64> },
    Bye,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum NodeToMcpd {
    Registered {
        agent_id: String,
        slot_id: String,
        session_id: String,
        display_name: Option<String>,
        role: Option<String>,
        tools: Vec<ToolDef>,
    },
    ToolReply {
        req_id: u64,
        result: serde_json::Value,
        /// Messages to surface to the model alongside this result. mcpd
        /// prepends them and then acks.
        #[serde(default)]
        deliver: Vec<Envelope>,
    },
    Error {
        req_id: Option<u64>,
        message: String,
    },
}
