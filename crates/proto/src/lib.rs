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

pub mod authority;
pub mod cli_descriptor;
pub mod codegraph;
pub mod identity;
#[cfg(feature = "pair-client")]
pub mod pair_client;
pub mod attachsock;
pub mod localsock;
pub mod plugin;
pub mod roles;
pub mod screenread;
pub mod skill;
pub mod tools;

use serde::{Deserialize, Serialize};
pub use tools::ToolDef;

pub const PROTOCOL_VERSION: u32 = 1;

/// serde default for a flag that is true unless a sender says otherwise.
/// A message from a build that predates the flag means the old behaviour,
/// which here is "this is the whole thing".
fn yes() -> bool {
    true
}
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

/// Queued messages, as an agent reads them on a tool result.
///
/// One convention for every delivery path -- the shim, the hub's own MCP
/// door, the driver plugins -- because an agent that sees two conventions
/// learns neither.
///
/// The operator gets a line of their own. A message with no sending slot is
/// from the person running the fleet, who is reading the session's chat, and
/// an agent told only to "respond" has no way to know that a reply to a
/// TEAMMATE does not reach them. One did exactly that: it was asked in the
/// chat whether it was connected, answered a sibling seat, and the question
/// read as ignored.
/// What an automatic note from the hub is said to be from.
pub const AUTOMATIC: &str = "nexmesh (automatic, not a person -- no answer expected)";

pub fn render_pending(deliver: &[Envelope]) -> String {
    if deliver.is_empty() {
        return String::new();
    }
    let mut body = String::from("[PENDING MESSAGES]\n");
    let mut from_operator = false;
    for m in deliver {
        let who = match (&m.from_slot, &m.from_name) {
            // The hub's own automatic notes -- a reminder, a fold nudge, a
            // seat's memory pointer. Not a person, and labelled so: called
            // "the OPERATOR", each one came with the line telling the agent
            // to answer the person, and every reminded seat did.
            (None, _) if m.kind == MessageKind::Nudge => AUTOMATIC.to_string(),
            (None, _) => {
                from_operator = true;
                "the OPERATOR".to_string()
            }
            (Some(_), Some(n)) => n.clone(),
            (Some(_), None) => "a teammate".to_string(),
        };
        // The body keeps its newlines -- this path always could, and the
        // formatting is often the point -- but it is indented under its own
        // bullet so that where one message ends and the next begins stays
        // legible when several arrive together. A uniform shift preserves
        // the relative indent of a list or a code block.
        let shaped = m.body.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\n  ");
        body.push_str(&format!("- #{} from {}: {}\n", m.id, who, shaped));
    }
    body.push_str("[END PENDING MESSAGES]\n");
    if from_operator {
        body.push_str(
            "The operator is a person, reading this session's chat. Answer them with \
             `reply` and the message's own number -- reply(to: <#>, text: ...) -- which \
             puts your answer in the chat they are reading WITHOUT sending it to your \
             teammates. `broadcast` is for something the whole team needs; using it to \
             answer one person is how six agents come to read a note meant for one.\n",
        );
    }
    body.push_str("Respond to these before continuing.\n\n");
    body
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
    ///
    /// Empty means the slot has no workspace and the node should decide. The
    /// hub must not guess: it once filled this from its own `HOME`, which is a
    /// path on the hub's machine, belonging to the hub's user -- so every
    /// workspace-less spawn asked a node to work in a directory that, on that
    /// node, it had no right to create.
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
    /// Every conversation this seat might be resuming, best first.
    ///
    /// There is more than one place a conversation id can come from -- the
    /// per-machine binding, the seat's last handle, the id the CLI itself
    /// reported, an earlier session's continuity record -- and the hub cannot
    /// tell which of them still EXISTS, because the files are on the node.
    ///
    /// Sending one and hoping was the bug: a ghost handle in front of a real
    /// conversation meant the real one was never tried, and the seat started
    /// blank on top of work that was still there. So the hub offers all of
    /// them in order and the node, which can see the disk, takes the first
    /// that is really there.
    ///
    /// `resume_handle` stays as the first of these, so a node from before
    /// this behaves exactly as it did.
    #[serde(default)]
    pub resume_candidates: Vec<String>,
    /// A system prompt this seat carries, and whether it adds to the CLI's
    /// own or replaces it ("append" / "replace").
    /// What this seat is allowed to do TO THE FLEET, as the hub holds it.
    ///
    /// The node needs it for ONE reason: the tool list a shim advertises.
    /// Authority is checked hub-side on every call, so this is not the gate
    /// -- but a tool that is never advertised is a tool the model cannot
    /// call at all, however much authority the seat holds. A
    /// superorchestrator whose shim advertised the plain-agent list was
    /// super on paper and powerless in practice.
    ///
    /// Absent means `agent`: a node or hub from before this, or a level this
    /// build does not know, must fail CLOSED.
    #[serde(default)]
    pub authority: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub system_prompt_mode: Option<String>,
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
        /// Every agent this node still holds a live process for, verified
        /// against /proc as the connection is made.
        ///
        /// `None` means "I am not telling you", and the hub leaves its own
        /// record alone. `Some` is authoritative: an agent the hub believes
        /// is on this node and that is not in this list has gone, and saying
        /// so is the only way a seat is ever freed after a node restart --
        /// the node keeps its agent table in memory, so a restarted node
        /// knows nothing of the processes its predecessor spawned and will
        /// never heartbeat them again.
        #[serde(default)]
        live_agents: Option<Vec<String>>,
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
        /// `pty` for an agent with a terminal of its own, `pipes` for one run
        /// on pipes, absent for one the node never started and so has no I/O
        /// for at all. Worth recording because it is the difference between
        /// a seat you can attach to and type at and one you can only read --
        /// which until now could be discovered only by trying.
        #[serde(default)]
        io_kind: Option<String>,
        /// True when this agent is at its own prompt in a terminal the node
        /// spawned, so a line typed at it is a message to the agent. The
        /// hub's only other route to such a seat is its next tool call.
        #[serde(default)]
        typed_delivery: bool,
    },
    /// This seat is sitting in a terminal the node has adopted, so it can be
    /// typed at even though nothing drives it.
    ///
    /// Reported once, when the CLI's shim registers -- that registration is
    /// what proves an agent CLI is in there, and its ancestry is what proves
    /// which pane. `cli` is the CLI's own identity triple, not the shim's:
    /// the shim is the CLI's child, so the link outlives a shim that dies or
    /// loses its connection, which is the case this exists for.
    TerminalLink {
        agent_id: String,
        slot_id: String,
        /// The adopted agent whose pty the keystrokes go through.
        via_agent_id: String,
        cli: ProcIdentity,
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
    ///
    /// Sent in pieces. A real workspace is tens of thousands of symbols and
    /// as many relationships, which is megabytes of JSON, and one websocket
    /// message that size is refused by the far end: the connection is reset,
    /// the node reconnects, and the answer is simply gone -- while the
    /// caller waits out its whole timeout for a result that was destroyed
    /// seconds after it was made. Each piece carries some of the nodes and
    /// edges; the last one carries the counts and the truncation notice.
    ///
    /// `seq` and `last` default to a single final piece, so a node built
    /// before this still speaks to a hub built after it.
    CodeGraphResult {
        req_id: u64,
        workspace_id: String,
        graph: crate::codegraph::CodeGraph,
        #[serde(default)]
        seq: u32,
        #[serde(default = "crate::yes")]
        last: bool,
    },
    /// What the tree looks like now, without parsing it: see
    /// [`HubToNode::TreeFingerprint`]. `files` is how many source files the
    /// walk would have parsed, and `fingerprint` changes whenever any of
    /// them is added, removed, resized or rewritten.
    TreeFingerprintResult {
        req_id: u64,
        workspace_id: String,
        fingerprint: String,
        files: u64,
        /// Set when the walk could not look at the whole tree, so a
        /// fingerprint that did not change is not proof the tree did not.
        #[serde(default)]
        partial: bool,
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
    /// How full this agent's context is NOW, and the window it has to fit
    /// in. Not spending: see `agents.context_used_tokens` in the schema for
    /// why the two are different numbers and why summing one to get the
    /// other is wrong.
    ///
    /// The node sends it because only the node can see the conversation --
    /// and the node sends the LIMIT too, because the limit comes from the
    /// CLI's descriptor, which only the node holds. The hub compares the two
    /// against the threshold the operator set and decides what to do; it is
    /// not asked to know the context window of every model on the fleet.
    ContextUsage {
        agent_id: String,
        used: i64,
        limit: i64,
        /// Where the reading came from, verbatim, for an operator asking why
        /// a seat was compacted: a transcript path, or a driver's name.
        source: String,
    },
    /// This agent has been doing the same thing over and over.
    ///
    /// `period` is how many turns the repeating unit is (1 is the same step
    /// again and again, 2 an agent flipping between two) and `repeats` is
    /// how many times it has repeated back to back, up to and including the
    /// most recent turn.
    ///
    /// The node reports the cycle it can SEE and does not decide whether it
    /// matters: how many repeats are too many is the operator's setting, and
    /// it lives with the rest of them in the hub. Sent when the cycle
    /// changes, not on every turn.
    /// The agent's model started or stopped working.
    ///
    /// ACTIVE means a turn is running: the model is generating and somebody
    /// is waiting on its answer. IDLE means it is waiting for input and doing
    /// nothing. BLOCKED means it stopped on a question only a person can
    /// answer. Sent when it CHANGES, by whichever part of the node can see the
    /// turn -- a driver's own turn events, or the screen of a terminal seat --
    /// and repeated on every heartbeat, so a lost frame is repaired rather
    /// than shown for ever.
    Activity {
        agent_id: String,
        state: ActivityState,
        /// When it changed, by the node's clock.
        since_ms: i64,
    },
    /// The agent can no longer reach its model -- or, with `block: None`, it
    /// can again.
    ///
    /// Read from what the CLI itself reported, never guessed from silence: a
    /// claude transcript's `quotaLimits` and API error class, codex's
    /// `usageLimitExceeded`, a provider's 429. The node reports; the hub
    /// decides what to hold and when to let it go.
    UsageLimited {
        agent_id: String,
        block: Option<UsageBlock>,
    },
    /// Files this agent has read since the last report, counted from its
    /// own conversation (a Read, Grep or Glob, or a shell `cat`/`rg`/...).
    /// What the hub compares against its code-graph queries: a seat reading
    /// the tree file by file is a seat the graph could have answered.
    FileReads {
        agent_id: String,
        n: u32,
    },
    LoopSuspected {
        agent_id: String,
        period: i64,
        repeats: i64,
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
        /// The CLI's own conversation id, when it has one. See
        /// `McpdToNode::Register::cli_session`.
        #[serde(default)]
        cli_session: Option<String>,
        /// True when `cli_session` is the CLI's own conversation id rather
        /// than an operator's label; see `localsock`.
        #[serde(default)]
        cli_session_is_own: bool,
    },
    /// What became of a `PutFile`.
    /// Whether the wheel now goes to that tmux view.
    TerminalMouseSet {
        req_id: u64,
        ok: bool,
        detail: String,
    },
    /// The bytes of a file this machine was asked for, base64.
    FileBytes {
        req_id: u64,
        ok: bool,
        data_b64: String,
        media_type: String,
        detail: String,
    },
    /// This machine now holds a direct-attach ticket and will honour it.
    ///
    /// The hub pushes a ticket over a connection and answers the operator
    /// over another; without this the answer can arrive first, and a ticket
    /// presented a millisecond later is refused as one this machine was
    /// never given. See `attachsock`.
    TicketStored { req_id: u64 },
    PutFileResult {
        req_id: u64,
        ok: bool,
        /// Absolute path it was written to, when it was.
        path: String,
        bytes: u64,
        detail: String,
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
    /// Something the NODE did on an agent's behalf that a person should be
    /// able to see afterwards: answering a permission prompt while nobody was
    /// attached, or finding one it did not dare answer. It lands in the
    /// session's chat as a system line, which is where the record of what the
    /// fleet did to itself belongs -- a decision taken for somebody must be
    /// visible to them even when they were not watching.
    Noted {
        agent_id: String,
        text: String,
    },
    /// This agent is stopped at a permission prompt and nobody is attached.
    ///
    /// The node does not answer it. It reports the question and the choices
    /// the CLI offered, and waits for `ApprovalDecided` -- which the hub may
    /// send back immediately if a standing auto-approval covers this seat.
    ApprovalAsked {
        agent_id: String,
        /// `pty_prompt` for a dialog on a terminal.
        kind: String,
        /// The question in the CLI's own words.
        question: String,
        options: Vec<PromptOption>,
        /// The shape of the screen that asked, so one dialog is raised once
        /// however many idle ticks notice it.
        fingerprint: String,
    },
    /// The prompt went away on its own -- the agent gave up waiting, or
    /// somebody attached and answered it. The approval is retired rather
    /// than left looking live in the operator's chat.
    ApprovalWithdrawn {
        agent_id: String,
        fingerprint: String,
    },
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
    /// The account this CLI signs in as, when its descriptor names one. Seats
    /// on one account share one allowance, so when it runs out it runs out
    /// for all of them. Absent: every seat of this CLI on this machine is
    /// taken to share one login, which is what one config directory means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Whether an agent's model is working right now. See [`NodeToHub::Activity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    /// A turn is running: the model is generating, and we wait on it.
    Active,
    /// Waiting for input. Nothing is running.
    Idle,
    /// Stopped on a question only a person can answer.
    Blocked,
}

impl ActivityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
        }
    }
}

/// Why an agent cannot reach its model. See [`NodeToHub::UsageLimited`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBlock {
    /// `quota`: the account's allowance is used up, and comes back on its
    /// own -- at `reset_at_ms` when the CLI said. `auth`: the login is gone.
    /// `billing`: the subscription cannot be used. Only `quota` recovers
    /// without a person.
    pub kind: String,
    /// When the allowance comes back, if the CLI said.
    #[serde(default)]
    pub reset_at_ms: Option<i64>,
    /// Which window ran out, as the provider names it: `five_hour`,
    /// `seven_day`, ...
    #[serde(default)]
    pub window: Option<String>,
    /// What the CLI said, verbatim, for the operator.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealth {
    pub agent_id: String,
    /// Verified by re-reading `/proc/<pid>/stat` and comparing start_ticks, so
    /// a recycled pid cannot masquerade as a live agent.
    pub alive: bool,
    /// How long since the agent last produced output. 0 when unknown.
    pub idle_ms: u64,
    /// What it is doing now, repeated so a lost `Activity` edge is repaired
    /// within a heartbeat. Absent from a node that cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityState>,
    /// Whether the seat's nexmesh tools are connected: a shim is registered
    /// for it on this node. `Some(false)` is a CLI that takes an MCP server
    /// and has had none for over a minute and a half -- typable, but every
    /// fleet tool it has is dead. Absent for a CLI with no MCP at all (a tmux
    /// screen) and from a node too old to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_link: Option<bool>,
}

// --------------------------------------------------------- hub -> node ----

/// One choice a CLI offered at a permission prompt.
///
/// Carried verbatim from the terminal to the operator's buttons. The LABEL is
/// the CLI's own wording and the KEYS are exactly what has to be typed to
/// pick it, because the two are not derivable from each other: a menu whose
/// affirmative reads "Yes, and don't ask again" must not be shown as "Yes",
/// and the keystroke for it is a digit nobody could guess from the text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptOption {
    /// Stable within one approval: the digit, the letter, or "enter".
    pub key: String,
    /// As the CLI wrote it.
    pub label: String,
    /// The bytes to send to choose it.
    pub keys: String,
    /// Does picking this let the action proceed? Used to decide what a
    /// standing auto-approval picks, and never to relabel the button.
    #[serde(default)]
    pub affirmative: bool,
    /// Does it also widen permission beyond this one action ("don't ask
    /// again", "allow all edits")? An auto-approval must never pick one of
    /// these: the operator's standing decision is "answer this prompt for
    /// me", not "let the agent turn its own guardrails off".
    #[serde(default)]
    pub widens: bool,
}

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
    /// How a permission prompt was answered, so the node can type it.
    ///
    /// The hub sends the KEYS rather than the choice, because the node is
    /// what read the menu and the hub is only echoing back one of the
    /// options the node gave it. Nothing is interpreted in between.
    ApprovalDecided {
        approval_id: String,
        agent_id: String,
        /// Exactly what to type. Empty means "do not answer" -- the prompt
        /// was withdrawn, or somebody answered it at the terminal.
        keys: String,
        /// For the note that goes in the agent's own scrollback.
        label: String,
        /// Who decided: `operator`, `auto:agent`, `auto:session`.
        by: String,
    },
    /// Stop what this agent is doing, without ending it.
    ///
    /// The node resolves the keystroke from the CLI's descriptor, as with
    /// `Compact`: the hub does not know how to interrupt every CLI and
    /// should not have to. Sent when a seat is being stopped for looping --
    /// pausing a seat withholds future work, and an agent already round the
    /// bend will go on spending until the turn it is on is cut short.
    Interrupt {
        agent_id: String,
        by: String,
    },
    /// Ask this agent's CLI to compact its own context now.
    ///
    /// The hub does not say WHAT to type: compaction is a slash command and
    /// which one differs per CLI, so the node looks it up in the descriptor
    /// it already holds. The hub only says which agent, and who asked.
    ///
    /// Typed at the terminal like any other keystroke, so it is refused when
    /// the screen is waiting for an answer -- a compaction request must not
    /// be the Enter that approves a tool call nobody read.
    Compact {
        agent_id: String,
        /// For the note that goes in the agent's own scrollback and the chat.
        by: String,
    },
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
        /// Set when this is a TYPED delivery into an adopted terminal, where
        /// `agent_id` is the terminal and this is the CLI that must be
        /// sitting in it. The node re-checks both -- that the process is
        /// still alive by its triple, and that it is still inside the pane
        /// its own view is looking at -- and types nothing otherwise.
        ///
        /// Deliberately carried rather than trusted: the hub's record was
        /// true when it was made, and a pane can hold a shell by the time
        /// the message arrives. Only the node shares a kernel with the
        /// process, so only the node can answer the question.
        #[serde(default)]
        expect_cli: Option<ProcIdentity>,
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
        /// The seat's fleet authority, for the shim's tool list. See
        /// `SpawnSpec::authority`; absent means `agent`.
        #[serde(default)]
        authority: Option<String>,
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
    /// HAS THE TREE MOVED?
    ///
    /// Parsing a workspace is minutes of somebody else's CPU, so the hub
    /// cannot rebuild a graph on a timer and call it fresh. This asks the
    /// node for something far cheaper: a fingerprint of the source files it
    /// would have parsed -- how many, how large, and the newest timestamp
    /// among them. The walk is the same walk, without the parsing.
    ///
    /// A node too old to know this frame logs it and carries on, which is
    /// why the hub also has an age rule for graphs it cannot get a
    /// fingerprint for.
    TreeFingerprint {
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
    /// Write a file into a workspace on this machine.
    ///
    /// The operator has something an agent needs to look at -- a screenshot,
    /// a PDF, a recording -- and an agent reads files, not chat attachments.
    /// So it lands in the workspace, on a machine that holds it, and the team
    /// is told the path. The node is the only thing that may write there: the
    /// hub has no file system of anyone else's.
    PutFile {
        req_id: u64,
        workspace_id: String,
        /// Absolute directory this workspace is mounted at on this node.
        root: String,
        /// Path relative to the root, already made safe by the hub.
        rel: String,
        /// The bytes, base64.
        data_b64: String,
    },
    /// Read a file back out of a workspace on this machine.
    ///
    /// The mirror of `PutFile`. The hub keeps no files of anyone's, so the
    /// only way to show an uploaded picture in the chat again is to ask the
    /// machine that holds it. Bounded: a node refuses anything larger than
    /// `MAX_FETCH_BYTES`, because this answer goes through the hub's
    /// WebSocket and a 2GB video would stall every other agent on it.
    GetFile {
        req_id: u64,
        root: String,
        rel: String,
    },
    /// Give the wheel to the tmux view this seat sits in, or take it back.
    ///
    /// Scrolling a pane's history is tmux's job and nobody else's: the
    /// terminal the operator sits at has no scrollback for a full-screen
    /// program, and the viewer in between has none either. But mouse mode
    /// also takes dragging to select, so this is never on by default and
    /// never set for longer than somebody asked for.
    TerminalMouse {
        req_id: u64,
        slot_id: String,
        on: bool,
    },
    AttachResize { attach_id: String, cols: u16, rows: u16 },
    AttachClose { attach_id: String },
    /// An operator may attach to this agent directly, bypassing the hub as a
    /// wire -- see `attachsock`. The hub has already decided they may; this
    /// says so to the node that holds the terminal, and is the only reason a
    /// node accepts a direct attach at all.
    AttachTicket {
        /// Correlates the node's `TicketStored` with the operator still
        /// holding the HTTP request: a ticket is worth nothing until the
        /// machine that must honour it actually has it.
        #[serde(default)]
        req_id: u64,
        ticket: String,
        agent_id: String,
        /// Wall-clock milliseconds after which it is worth nothing.
        expires_at_ms: i64,
        /// Who it was minted for, for the node's log.
        operator: String,
    },
    Ping { seq: u64 },
}

/// The most a node will read back out of a workspace in one answer.
///
/// A picture in a chat is the use; a whole recording is not. The limit is the
/// hub's own socket, which every node and every attach shares.
pub const MAX_FETCH_BYTES: u64 = 24 * 1024 * 1024;

// --------------------------------------------------------------- utils ----

/// Base64 for pty bytes: terminal output is not valid UTF-8 in general.
pub mod b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
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
    #[test]
    fn render_pending_tells_a_hub_note_from_the_operator() {
        let note = |kind| Envelope {
            id: 7, from_slot: None, from_name: None, to_slot: None, kind,
            body: "keep it".into(), sent_at_ms: 0,
        };
        let auto = render_pending(&[note(MessageKind::Nudge)]);
        assert!(auto.contains(AUTOMATIC) && !auto.contains("OPERATOR") && !auto.contains("`reply`"),
                "{auto}");
        let person = render_pending(&[note(MessageKind::System)]);
        assert!(person.contains("the OPERATOR") && person.contains("`reply`"), "{person}");
    }

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

    /// A node built before tickets were acknowledged sends no `req_id`, and
    /// an old hub sends none either. Neither is a reason to drop the frame
    /// that authorises an attach.
    #[test]
    fn an_attach_ticket_without_a_req_id_still_parses() {
        let json = r#"{"t":"attach_ticket","ticket":"t","agent_id":"ag-1",
                        "expires_at_ms":1,"operator":"me"}"#;
        match serde_json::from_str::<HubToNode>(json).unwrap() {
            HubToNode::AttachTicket { req_id, ticket, .. } => {
                assert_eq!(req_id, 0);
                assert_eq!(ticket, "t");
            }
            other => panic!("wrong frame: {other:?}"),
        }
    }

    #[test]
    fn schema_applies_cleanly() {
        assert!(SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS nodes"));
    }
}
