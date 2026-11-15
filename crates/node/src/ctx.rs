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
    /// The terminal modes this CLI has asked for, and the tail of its output,
    /// because a sequence can be split across two reads.
    ///
    /// Two things need them. Typing: a burst of bytes is a paste as far as a
    /// TUI is concerned, and one that asked for BRACKETED PASTE (2004) wants
    /// the markers, or the Enter after a long message is read as part of the
    /// paste. Attaching: a CLI sets these ONCE, at startup, and a viewer that
    /// arrives an hour later finds them long gone from the replay -- so the
    /// program believes the terminal reports the mouse and is in the
    /// alternate screen while the terminal in front of the operator is in
    /// neither. That is a TUI whose scrolling has stopped working for no
    /// visible reason.
    modes: Mutex<std::collections::BTreeMap<u16, bool>>,
    mode_tail: Mutex<Vec<u8>>,
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
            modes: Mutex::new(Default::default()),
            mode_tail: Mutex::new(Vec::new()),
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
            modes: Mutex::new(self.modes.lock().unwrap().clone()),
            mode_tail: Mutex::new(Vec::new()),
            pending: Mutex::new(Default::default()),
        })
    }

    /// Has the CLI asked for bracketed paste?
    pub fn bracketed(&self) -> bool {
        self.modes.lock().unwrap().get(&2004).copied().unwrap_or(false)
    }

    /// The DEC private modes worth remembering for a viewer.
    ///
    /// Not every mode: only the ones whose absence a person would notice.
    /// 1049 is the alternate screen, without which a full-screen program's
    /// drawing lands in the scrollback; 1 is application cursor keys, so the
    /// arrows send what the program is listening for; 1000/1002/1003 and the
    /// 1005/1006/1015 encodings are how it hears the mouse, which is how most
    /// TUIs scroll; 2004 is bracketed paste; 1004 is focus; 7 is autowrap;
    /// 25 is the cursor. 1007, alternate scroll, is deliberately NOT here:
    /// an attach refuses that one.
    const WORTH_KEEPING: &'static [u16] =
        &[1, 7, 25, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1049, 2004];

    /// Watch the CLI's own output for the modes it sets.
    ///
    /// A few bytes of the previous chunk are kept, because these sequences
    /// run to a dozen bytes and a read can end in the middle of one -- which
    /// would mean missing a mode exactly at startup, which is when every CLI
    /// sets all of them.
    pub fn note_modes(&self, data: &[u8]) {
        let mut tail = self.mode_tail.lock().unwrap();
        let mut buf = Vec::with_capacity(tail.len() + data.len());
        buf.extend_from_slice(&tail);
        buf.extend_from_slice(data);
        let mut modes = self.modes.lock().unwrap();
        // `CSI ? <params> h|l`, where params may be several modes at once:
        // `\x1b[?1000;1002;1006h` is one sequence setting three.
        let mut i = 0usize;
        while i + 3 < buf.len() {
            if !(buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?') {
                i += 1;
                continue;
            }
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j >= buf.len() {
                break;              // the sequence is still arriving
            }
            let enabling = match buf[j] {
                b'h' => true,
                b'l' => false,
                _ => {
                    i = j + 1;
                    continue;
                }
            };
            for part in buf[i + 3..j].split(|b| *b == b';') {
                if let Ok(n) = std::str::from_utf8(part).unwrap_or("").parse::<u16>() {
                    if Self::WORTH_KEEPING.contains(&n) {
                        modes.insert(n, enabling);
                    }
                }
            }
            i = j + 1;
        }
        let keep = buf.len().saturating_sub(24);
        *tail = buf[keep..].to_vec();
    }

    /// What to send a viewer before the replay, so the terminal it is looking
    /// at is in the state the program believes it is in.
    ///
    /// The alternate screen goes first: everything else is about a screen,
    /// and the order a real program sets them in is the order that works.
    pub fn modes_preamble(&self) -> Vec<u8> {
        let modes = self.modes.lock().unwrap();
        let mut out = Vec::new();
        let mut emit = |n: u16| {
            out.extend_from_slice(format!("\x1b[?{n}h").as_bytes());
        };
        if modes.get(&1049).copied().unwrap_or(false) {
            emit(1049);
        }
        for n in Self::WORTH_KEEPING.iter().filter(|n| **n != 1049) {
            if modes.get(n).copied().unwrap_or(false) {
                emit(*n);
            }
        }
        out
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
    /// For an ADOPTED terminal: the name of the node's own tmux session,
    /// grouped with the operator's. Keystrokes from this node land in
    /// whatever pane this view is looking at, so it is also what a link
    /// check must ask about -- never the operator's own view, which may be
    /// looking somewhere else entirely.
    pub tmux_view: Option<String>,
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
    /// Attaches whose other end is on THIS machine, keyed by attach id. A
    /// frame for one of these never reaches the hub: see `send_hub`.
    local_attaches: Mutex<HashMap<String, mpsc::Sender<proto::attachsock::Frame>>>,
    /// Tickets the hub has minted for direct attaches: ticket -> (agent,
    /// expiry). Nothing else lets an operator attach without the hub as the
    /// wire, and each is good once.
    tickets: Mutex<HashMap<String, (String, i64)>>,
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
            local_attaches: Mutex::new(HashMap::new()),
            tickets: Mutex::new(HashMap::new()),
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
        // An attach whose other end is on this machine is answered here, not
        // in the hub. Every part of the node that relays a terminal already
        // goes through this one function, so routing it here means the pty
        // code, the viewer and the spawn path need to know nothing about how
        // the operator got here.
        if let Some(attach_id) = attach_id_of(frame) {
            let local = { self.local_attaches.lock().unwrap().get(attach_id).cloned() };
            if let Some(tx) = local {
                let ends = matches!(
                    frame,
                    proto::NodeToHub::AttachClosed { .. } | proto::NodeToHub::AttachRejected { .. }
                );
                if let Some(f) = local_frame(frame) {
                    let _ = tx.send(f).await;
                }
                if ends {
                    self.local_attaches.lock().unwrap().remove(attach_id);
                }
                return Ok(());
            }
        }
        let tx = { self.hub_tx.lock().unwrap().clone() };
        let tx = tx.ok_or_else(|| anyhow::anyhow!("not connected to the hub"))?;
        tx.send(serde_json::to_string(frame)?).await?;
        Ok(())
    }

    // ------------------------------------------------- direct attaches ----

    /// Remember a ticket the hub minted. Expired ones are dropped on the way
    /// past, which is the only pruning this map needs: it is only ever as
    /// large as the attaches somebody is in the middle of starting.
    pub fn add_ticket(&self, ticket: String, agent_id: String, expires_at_ms: i64) {
        let now = proto::now_ms();
        let mut t = self.tickets.lock().unwrap();
        t.retain(|_, (_, exp)| *exp > now);
        t.insert(ticket, (agent_id, expires_at_ms));
    }

    /// Spend a ticket: the agent it was for, or why not. Good once -- a second
    /// use of the same string is somebody replaying it.
    pub fn spend_ticket(&self, ticket: &str) -> std::result::Result<String, &'static str> {
        let mut t = self.tickets.lock().unwrap();
        match t.remove(ticket) {
            None => Err("that ticket is not one the hub gave this machine, or it has been used"),
            Some((_, exp)) if exp <= proto::now_ms() => Err("that ticket has expired"),
            Some((agent, _)) => Ok(agent),
        }
    }

    pub fn add_local_attach(
        &self,
        attach_id: String,
        tx: mpsc::Sender<proto::attachsock::Frame>,
    ) {
        self.local_attaches.lock().unwrap().insert(attach_id, tx);
    }

    pub fn drop_local_attach(&self, attach_id: &str) {
        self.local_attaches.lock().unwrap().remove(attach_id);
    }
}

/// The attach a node-to-hub frame belongs to, when it belongs to one.
fn attach_id_of(frame: &proto::NodeToHub) -> Option<&str> {
    match frame {
        proto::NodeToHub::AttachOpened { attach_id, .. }
        | proto::NodeToHub::AttachRejected { attach_id, .. }
        | proto::NodeToHub::AttachData { attach_id, .. }
        | proto::NodeToHub::AttachClosed { attach_id, .. } => Some(attach_id),
        _ => None,
    }
}

/// The same thing, said on a direct connection.
fn local_frame(frame: &proto::NodeToHub) -> Option<proto::attachsock::Frame> {
    use proto::attachsock::{Control, Frame};
    Some(match frame {
        proto::NodeToHub::AttachOpened { read_only, cols, rows, .. } => {
            Frame::Control(Control::Opened { read_only: *read_only, cols: *cols, rows: *rows })
        }
        proto::NodeToHub::AttachRejected { reason, .. } => {
            Frame::Control(Control::Error { message: reason.clone() })
        }
        proto::NodeToHub::AttachData { data, .. } => Frame::Data(data.clone()),
        proto::NodeToHub::AttachClosed { reason, .. } => {
            Frame::Control(Control::Closed { reason: reason.clone() })
        }
        _ => return None,
    })
}

impl NodeCtx {

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

    /// Does the terminal `via` is holding still have exactly this CLI in it?
    ///
    /// The whole safety of typing at an adopted session rests on this being
    /// asked again at the moment of writing, by the machine that shares a
    /// kernel with the process. Three things must hold, and the error says
    /// which one did not:
    ///
    ///   - `via` is still an adopted terminal of ours, with a view to ask
    ///     about;
    ///   - the CLI is still the same process, by pid AND start ticks -- a
    ///     recycled pid is a different program;
    ///   - it is still inside the pane that view is looking at, because a
    ///     keystroke lands in that pane and nowhere else. An operator who
    ///     switches window has moved the keyboard, and we wait.
    pub fn terminal_still_holds(
        &self,
        via: &str,
        cli: &proto::ProcIdentity,
    ) -> std::result::Result<(), String> {
        let view = self
            .agents
            .lock()
            .unwrap()
            .get(via)
            .and_then(|r| r.tmux_view.clone())
            .ok_or_else(|| format!("{via} is not an adopted terminal on this node"))?;
        if !crate::proc::is_same_process(cli.pid, cli.start_ticks) {
            return Err(format!("the CLI at pid {} has gone", cli.pid));
        }
        let pane = crate::tmux::pane_pid(&view)
            .ok_or_else(|| format!("tmux no longer answers for {view}"))?;
        if !crate::proc::is_descendant_of(cli.pid, pane) {
            return Err(format!(
                "the CLI at pid {} is not in the pane this view is looking at ({pane})",
                cli.pid
            ));
        }
        Ok(())
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

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn io() -> Arc<AgentIo> {
        AgentIo::new(IoKind::Pty, None, None)
    }

    #[test]
    fn several_modes_in_one_sequence_are_all_remembered() {
        // What a TUI actually sends at startup, in one breath.
        let io = io();
        io.note_modes(b"\x1b[?1049h\x1b[?1000;1002;1003;1006h\x1b[?2004h\x1b[?25l");
        let pre = String::from_utf8(io.modes_preamble()).unwrap();
        for want in ["\x1b[?1049h", "\x1b[?1000h", "\x1b[?1002h", "\x1b[?1003h",
                     "\x1b[?1006h", "\x1b[?2004h"] {
            assert!(pre.contains(want), "preamble is missing {want:?}: {pre:?}");
        }
        assert!(!pre.contains("\x1b[?25h"), "a hidden cursor is not re-shown");
        assert!(pre.starts_with("\x1b[?1049h"), "the alternate screen comes first: {pre:?}");
    }

    #[test]
    fn a_mode_the_program_gave_back_is_not_re_asserted() {
        let io = io();
        io.note_modes(b"\x1b[?1000h\x1b[?1049h");
        io.note_modes(b"\x1b[?1000l");
        let pre = String::from_utf8(io.modes_preamble()).unwrap();
        assert!(pre.contains("\x1b[?1049h"));
        assert!(!pre.contains("\x1b[?1000h"), "it asked for the mouse and then gave it back");
    }

    #[test]
    fn modes_nobody_needs_are_not_carried() {
        // Alternate scroll is refused by an attach, so remembering it would
        // only re-enable something the viewer then strips.
        let io = io();
        io.note_modes(b"\x1b[?1007h\x1b[?47h");
        assert_eq!(io.modes_preamble(), Vec::<u8>::new());
    }

    #[test]
    fn bracketed_paste_is_noticed_and_can_be_turned_off_again() {
        let io = io();
        assert!(!io.bracketed(), "nothing has asked for it yet");
        io.note_modes(b"\x1b[?1049h\x1b[?2004h\x1b[2J");
        assert!(io.bracketed());
        io.note_modes(b"bye\x1b[?2004l");
        assert!(!io.bracketed(), "a CLI that gives it back is taken at its word");
    }

    #[test]
    fn a_mode_switch_split_across_two_reads_is_still_seen() {
        // Seven bytes, and a pty read can end anywhere. Missing it would
        // mean missing it exactly at startup, which is when every CLI sets
        // it -- and then a long message would be pasted unbracketed for the
        // rest of that agent's life.
        let io = io();
        io.note_modes(b"welcome\x1b[?20");
        assert!(!io.bracketed(), "half a sequence is not a sequence");
        io.note_modes(b"04h> ");
        assert!(io.bracketed(), "the other half completes it");
    }

    #[test]
    fn the_last_switch_in_a_chunk_wins() {
        let io = io();
        io.note_modes(b"\x1b[?2004h\x1b[?2004l\x1b[?2004h");
        assert!(io.bracketed());
        io.note_modes(b"\x1b[?2004h\x1b[?2004l");
        assert!(!io.bracketed());
    }
}
