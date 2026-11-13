// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Wire protocol and shared types for nexmesh.
//!
//! Transport shape: a node always *dials out* to the hub and holds one
//! long-lived WebSocket. The hub never opens a connection to a node. That one
//! decision is what lets a node live behind NAT, on a roaming machine, or on a
//! public host with no inbound port, without any later redesign.
//!
//! Everything below -- spawns, kills, message injection and interactive
//! attach -- is multiplexed over that single socket.

pub mod cli_descriptor;
pub mod codegraph;
pub mod identity;
#[cfg(feature = "pair-client")]
pub mod pair_client;
pub mod localsock;
pub mod plugin;
pub mod tools;

use serde::{Deserialize, Serialize};
pub use tools::ToolDef;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_HUB_PORT: u16 = 7870;
pub const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
/// A node is presumed lost after this long without a heartbeat. The hub then
/// marks its agents `lost` -- it does not, and cannot, signal them.
pub const NODE_LEASE_MS: u64 = 20_000;

pub type Ms = i64;

// ------------------------------------------------------------------ ids ----

/// A running process, identified the only way that is unique across machines.
///
/// A bare pid is meaningless fleet-wide: pid 4242 exists on every node. Worse,
/// pids are recycled, so `pid` alone can also be wrong on a *single* machine.
/// `start_ticks` (field 22 of `/proc/<pid>/stat`, the process start time in
/// clock ticks since boot) pins it down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcIdentity {
    pub node_id: String,
    pub pid: i32,
    pub start_ticks: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Batch protocol on stdio (e.g. `claude -p --output-format stream-json`).
    /// Best structured injection and token accounting; not a usable terminal.
    Headless,
    /// The CLI's normal TUI under a pty. Fully attachable and take-over-able;
    /// injection is coarser.
    Interactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The hub asked a node to spawn it.
    Managed,
    /// A human started the CLI by hand; it joined by way of its MCP shim.
    Attached,
    /// Not a process we host at all -- a hermes agent, a Python daemon, any
    /// program that speaks the participant API. It has no node and no pid, and
    /// its liveness is a lease. A full team member regardless.
    External,
}

impl Provenance {
    /// Can the fleet see this agent's process and therefore verify it via
    /// `/proc`? If not, liveness must come from a lease.
    pub fn has_local_process(&self) -> bool {
        matches!(self, Provenance::Managed | Provenance::Attached)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Idle,
    Working,
    DonePendingReview,
    AddressingFeedback,
    Approved,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Feedback,
    Approval,
    Nudge,
    Handoff,
    System,
}

/// What to do to a shared workspace on one machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOp {
    /// Make sure the checkout exists and is on the right branch.
    Ensure,
    /// Bring it up to date, keeping uncommitted work.
    Refresh,
    /// Commit what is here and share it.
    Publish,
    /// Report only; change nothing.
    Status,
}

/// Where a slot may run. Resolved by the hub against online nodes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Placement {
    /// Pin to one node by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Any node carrying all of these labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Any node that has the slot's CLI installed.
    #[serde(default)]
    pub any: bool,
}

/// A lock's arbitration domain. `Node` for work on one machine's own files,
/// `Workspace` for a tree that several machines hold a copy of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LockScope {
    Node { node_id: String },
    Workspace { workspace_id: String },
}

impl LockScope {
    pub fn as_key(&self) -> String {
        match self {
            LockScope::Node { node_id } => format!("node:{node_id}"),
            LockScope::Workspace { workspace_id } => format!("ws:{workspace_id}"),
        }
    }
}

// ------------------------------------------------------------ messages ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: i64,
    pub from_slot: Option<String>,
    pub from_name: Option<String>,
    pub to_slot: Option<String>,
    pub kind: MessageKind,
    pub body: String,
    pub sent_at_ms: Ms,
}

// ------------------------------------------------------- spawn request ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub slot_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub cli_id: String,
    pub mode: RunMode,
    /// Absolute path on *this* node, resolved by the hub from workspace_mounts.
    pub cwd: String,
    pub role: Option<String>,
    pub display_name: Option<String>,
    /// Full first prompt, protocol text and role practices included.
    pub prompt: String,
    /// Extra environment for the child.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Resume handle from a previous incarnation of this slot, if any.
    #[serde(default)]
    pub resume_handle: Option<String>,
    /// For an adopted slot: the tmux target whose terminal to mirror.
    #[serde(default)]
    pub tmux_target: Option<String>,
    /// The model this seat was asked to run. The node turns it into whatever
    /// this CLI wants -- a flag, or a field in its own protocol. None means
    /// the CLI's default.
    #[serde(default)]
    pub model: Option<String>,
}

// --------------------------------------------------------- node -> hub ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum NodeToHub {
    /// First frame on every connection.
    Hello {
        protocol: u32,
        /// Pre-shared enrolment token. The hub stores only its SHA-256.
        token: String,
        name: String,
        os: String,
        arch: String,
        node_version: String,
        boot_id: String,
        labels: Vec<String>,
        clis: Vec<DetectedCli>,
    },
    Heartbeat {
        seq: u64,
        agents: Vec<AgentHealth>,
    },
    /// A process came up -- whether the hub asked for it (`Managed`) or a human
    /// started it and its MCP shim found us (`Attached`).
    AgentUp {
        agent_id: String,
        slot_id: String,
        proc: ProcIdentity,
        provenance: Provenance,
        cwd: String,
        resume_handle: Option<String>,
        /// True when this agent has a driver that can take a message MID-TURN
        /// rather than at its next tool call. The hub uses it to decide
        /// whether to push messages or let them ride on a tool result.
        #[serde(default)]
        push_capable: bool,
    },
    /// A handle that lets a respawned agent pick up where this one left off
    /// (Claude's session id, Codex's thread id). Learned after startup, so it
    /// cannot ride along with `AgentUp`.
    ResumeHandle {
        agent_id: String,
        slot_id: String,
        handle: String,
    },
    /// A workspace's parsed code graph, answering BuildCodeGraph.
    CodeGraphResult {
        req_id: u64,
        workspace_id: String,
        graph: crate::codegraph::CodeGraph,
    },
    /// The model an agent reports it is actually running.
    ///
    /// Separate from the one requested, because a CLI may substitute or fall
    /// back. Without it the per-agent token counts are unreadable: 40k tokens
    /// means nothing until you know whether an expensive or a cheap model
    /// spent them.
    AgentModel {
        agent_id: String,
        model: String,
    },
    AgentExited {
        agent_id: String,
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// pty / transcript bytes, for live viewing and scrollback replay.
    Output {
        agent_id: String,
        seq: u64,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    TokenUsage {
        agent_id: String,
        input: i64,
        output: i64,
        cache_read: i64,
    },
    /// An agent called an MCP tool its shim could not answer locally.
    ToolCall {
        agent_id: String,
        call_id: String,
        tool: String,
        args: serde_json::Value,
    },
    /// An agent nobody spawned wants a seat: a human-launched CLI, or any
    /// program that found its local node. The hub creates or claims a slot.
    JoinRequest {
        req_id: u64,
        session_hint: Option<String>,
        cli_id: Option<String>,
        role: Option<String>,
        name: Option<String>,
        cwd: Option<String>,
        proc: Option<ProcIdentity>,
    },
    SyncResult {
        req_id: u64,
        workspace_id: String,
        ok: bool,
        /// True when the work could not be shared because it collides with
        /// what is already on the branch. Nothing was changed.
        conflict: bool,
        head: Option<String>,
        detail: String,
        changed: Vec<String>,
    },
    /// Messages handed to a driver that the agent died before confirming.
    /// They must go back in the queue: in flight forever means never
    /// redelivered and never seen.
    InjectionsLost {
        agent_id: String,
        message_ids: Vec<i64>,
    },
    /// Proof a message reached the model. Until this arrives the message stays
    /// in flight and is never handed to a second agent.
    MessageAcked {
        agent_id: String,
        message_ids: Vec<i64>,
    },
    /// Confirms an attach and says whether the viewer can actually type.
    /// A headless agent is a protocol on a pipe, not a terminal: you can watch
    /// it, but there is nothing to take over.
    AttachOpened {
        attach_id: String,
        agent_id: String,
        read_only: bool,
        cols: u16,
        rows: u16,
    },
    AttachRejected {
        attach_id: String,
        reason: String,
    },
    AttachData {
        attach_id: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    AttachClosed {
        attach_id: String,
        reason: String,
    },
    Pong { seq: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedCli {
    pub cli_id: String,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealth {
    pub agent_id: String,
    /// Verified by re-reading `/proc/<pid>/stat` and comparing start_ticks, so
    /// a recycled pid cannot masquerade as a live agent.
    pub alive: bool,
    pub idle_ms: u64,
}

// --------------------------------------------------------- hub -> node ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HubToNode {
    Welcome {
        protocol: u32,
        node_id: String,
        hub_version: String,
        heartbeat_ms: u64,
    },
    Rejected {
        reason: String,
    },
    Spawn(SpawnSpec),
    /// Let an agent finish and go.
    ///
    /// Distinct from `Kill`: the node closes the agent's input first, which is
    /// how a stdio-driven CLI learns there is no more work and exits of its own
    /// accord, and only terminates it if it is still there after the grace
    /// period. A released agent has done its job; killing it outright would
    /// lose whatever it was in the middle of writing.
    CloseAgent {
        agent_id: String,
        proc: ProcIdentity,
        grace_ms: u64,
    },
    /// Ask the node to stop an agent. Only the owning node ever signals a
    /// process, and only after verifying the identity triple.
    Kill {
        agent_id: String,
        proc: ProcIdentity,
        grace_ms: u64,
    },
    /// Fast-path injection for CLIs whose driver supports mid-turn input.
    Inject {
        agent_id: String,
        messages: Vec<Envelope>,
    },
    /// Universal-path delivery: the shim prepends these to the next tool result.
    Pending {
        agent_id: String,
        messages: Vec<Envelope>,
    },
    ToolResult {
        agent_id: String,
        call_id: String,
        result: serde_json::Value,
        /// Messages to surface to the model with this result. They are marked
        /// delivered only once the agent acks having received them.
        #[serde(default)]
        deliver: Vec<Envelope>,
    },
    /// Answer to a `JoinRequest`.
    JoinGranted {
        req_id: u64,
        slot_id: String,
        session_id: String,
        agent_id: String,
        display_name: Option<String>,
        role: Option<String>,
    },
    JoinDenied { req_id: u64, reason: String },
    AttachOpen {
        attach_id: String,
        agent_id: String,
        cols: u16,
        rows: u16,
        /// Replay this many bytes of scrollback before live data.
        replay_bytes: usize,
    },
    AttachData {
        attach_id: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// Act on a shared workspace's checkout on this node.
    /// Parse a workspace and send its code graph back. The node does this
    /// because the node is where the files are.
    BuildCodeGraph {
        req_id: u64,
        workspace_id: String,
        path: String,
    },
    SyncWorkspace {
        req_id: u64,
        workspace_id: String,
        path: String,
        repo_url: String,
        branch: String,
        op: SyncOp,
        /// Who to attribute a commit to.
        author: String,
        message: String,
    },
    AttachResize { attach_id: String, cols: u16, rows: u16 },
    AttachClose { attach_id: String },
    Ping { seq: u64 },
}

// --------------------------------------------------------------- utils ----

/// Base64 for pty bytes: terminal output is not valid UTF-8 in general.
mod b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        for c in input.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            out.push(T[(n >> 18 & 63) as usize] as char);
            out.push(T[(n >> 12 & 63) as usize] as char);
            out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
            out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
        }
        out
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, &'static str> {
        let mut rev = [255u8; 256];
        for (i, &c) in T.iter().enumerate() {
            rev[c as usize] = i as u8;
        }
        let raw: Vec<u8> = s.bytes().filter(|b| *b != b'=' && !b.is_ascii_whitespace()).collect();
        let mut out = Vec::with_capacity(raw.len() / 4 * 3);
        for c in raw.chunks(4) {
            let mut n = 0u32;
            for (i, &ch) in c.iter().enumerate() {
                let v = rev[ch as usize];
                if v == 255 {
                    return Err("invalid base64");
                }
                n |= (v as u32) << (18 - 6 * i);
            }
            let bytes = n.to_be_bytes();
            out.push(bytes[1]);
            if c.len() > 2 { out.push(bytes[2]); }
            if c.len() > 3 { out.push(bytes[3]); }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }
}

pub fn now_ms() -> Ms {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Ms)
        .unwrap_or(0)
}

pub const SCHEMA_SQL: &str = include_str!("../schema.sql");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_all_lengths() {
        for n in 0..32usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
            let frame = NodeToHub::Output { agent_id: "a".into(), seq: 1, data: data.clone() };
            let json = serde_json::to_string(&frame).unwrap();
            match serde_json::from_str(&json).unwrap() {
                NodeToHub::Output { data: got, .. } => assert_eq!(got, data, "len {n}"),
                other => panic!("wrong frame: {other:?}"),
            }
        }
    }

    #[test]
    fn base64_handles_high_bytes() {
        let data: Vec<u8> = (0..=255u8).collect();
        assert_eq!(b64::decode(&b64::encode(&data)).unwrap(), data);
    }

    #[test]
    fn lock_scopes_do_not_collide() {
        let n = LockScope::Node { node_id: "machine-a".into() };
        let w = LockScope::Workspace { workspace_id: "machine-a".into() };
        assert_ne!(n.as_key(), w.as_key());
    }

    #[test]
    fn schema_applies_cleanly() {
        assert!(SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS nodes"));
    }
}
