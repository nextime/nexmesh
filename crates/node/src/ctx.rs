//! Shared node state: live agents, pending hub round-trips, and the socket
//! back to the hub.

use std::collections::BTreeMap;
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

/// A request to a driver that is not a message: something only the CLI's own
/// protocol can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverControl {
    /// Compact the conversation now.
    Compact,
}

/// What a human attached to a seat is in the middle of typing.
///
/// A message injected while someone is composing at the input box lands in
/// the MIDDLE of their line -- their half-written sentence and a teammate's
/// message become one garbled submit. So the node tracks, from the keystrokes
/// it forwards, roughly how much unsent text is in the box and when the last
/// key arrived, and holds an injection back while that is non-empty.
#[derive(Default)]
struct Compose {
    /// Roughly how many characters sit in the human's unsent input line.
    /// Approximate on purpose: enough to tell "empty" from "mid-sentence".
    chars: usize,
    /// When the last human keystroke arrived (ms since epoch); 0 if none.
    last_ms: i64,
}

/// How long a human's half-typed input holds an injected message back before
/// it is delivered anyway. Long, because the cost of waiting is a late
/// message and the cost of not waiting is a message typed into the middle of
/// what somebody is writing -- but not unbounded, so a message is never stuck
/// behind input that was abandoned.
const COMPOSE_GRACE_MS: i64 = 45_000;

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
    /// What size each attached viewer says it has.
    ///
    /// Kept per viewer because the pty has ONE size and viewers do not
    /// agree: a terminal reserving a row for its key bar asks for one less
    /// than the dashboard's grid, and an XR panel being dragged asks for a
    /// new size every frame. Applying each as it arrives made the last
    /// speaker win, so the pty flipped between them for as long as both were
    /// open -- and every flip is a SIGWINCH, which is a full reflow of
    /// whatever the agent was drawing.
    sizes: Mutex<BTreeMap<String, (u16, u16)>>,
    /// The size actually pushed to the pty, so an unchanged one is not
    /// pushed again. A repeated identical resize still raises SIGWINCH, and
    /// a viewer that re-states its size on every poll turns that into a
    /// storm.
    applied: Mutex<Option<(u16, u16)>>,
    /// The tmux view this pty is a CLIENT of, when the seat is held in tmux.
    ///
    /// Resizing the pty is not enough for one of those. The kernel's
    /// TIOCSWINSZ raises SIGWINCH at the tmux client, and if that signal
    /// lands before the client has installed its handler -- which is a race
    /// the node loses whenever several seats come back at once -- tmux keeps
    /// the size it read at startup for ever. It then sizes the window one
    /// row taller than the viewer can show, the CLI draws a row nobody sees,
    /// and because a TUI repaints only what changed, the view never
    /// recovers: a refresh half-fixes it and the next keystroke spoils it
    /// again. Measured on the live fleet, every tmux client on one machine
    /// was exactly one row out.
    ///
    /// So the size is also STATED to tmux, which does not depend on a signal
    /// arriving.
    view: Mutex<Option<String>>,
    /// How many times the pty has actually been resized.
    ///
    /// Every one of these is a SIGWINCH the agent had to answer by redrawing,
    /// so it is worth being able to count them: a number that climbs while
    /// nobody is touching a window is the signature of viewers disagreeing.
    resizes: Mutex<u64>,
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
    /// What a human attached to this seat is in the middle of typing, so an
    /// injection does not land in the middle of their input line.
    compose: Mutex<Compose>,
    /// Whether the model is working right now, and since when. `None` until
    /// something that can see a turn has said: a seat whose state is unknown
    /// is shown as unknown, never as idle.
    activity: Mutex<Option<(proto::ActivityState, i64)>>,
    /// When the CLI last produced output (ms since epoch), 0 if never.
    last_output_ms: std::sync::atomic::AtomicI64,
    /// Where a protocol driver takes requests that are not messages. Set by
    /// the driver once it can act on them.
    control: Mutex<Option<mpsc::Sender<DriverControl>>>,
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
            sizes: Mutex::new(BTreeMap::new()),
            applied: Mutex::new(None),
            view: Mutex::new(None),
            resizes: Mutex::new(0),
            modes: Mutex::new(Default::default()),
            mode_tail: Mutex::new(Vec::new()),
            pending: Mutex::new(VecDeque::new()),
            compose: Mutex::new(Compose::default()),
            activity: Mutex::new(None),
            last_output_ms: std::sync::atomic::AtomicI64::new(0),
            control: Mutex::new(None),
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
            sizes: Mutex::new(self.sizes.lock().unwrap().clone()),
            applied: Mutex::new(*self.applied.lock().unwrap()),
            view: Mutex::new(self.view.lock().unwrap().clone()),
            resizes: Mutex::new(*self.resizes.lock().unwrap()),
            modes: Mutex::new(self.modes.lock().unwrap().clone()),
            mode_tail: Mutex::new(Vec::new()),
            pending: Mutex::new(Default::default()),
            compose: Mutex::new(Compose::default()),
            activity: Mutex::new(*self.activity.lock().unwrap()),
            last_output_ms: std::sync::atomic::AtomicI64::new(
                self.last_output_ms.load(Ordering::Relaxed),
            ),
            // A view without input cannot be asked to do anything either.
            control: Mutex::new(None),
        })
    }

    /// Record what the model is doing. True when that is a CHANGE, which is
    /// when the hub is told.
    pub fn set_activity(&self, state: proto::ActivityState) -> bool {
        let mut a = self.activity.lock().unwrap();
        if a.map(|(s, _)| s) == Some(state) {
            return false;
        }
        *a = Some((state, proto::now_ms()));
        true
    }

    /// What the model is doing, and since when; `None` if nothing has said.
    pub fn activity(&self) -> Option<(proto::ActivityState, i64)> {
        *self.activity.lock().unwrap()
    }

    /// A driver saying it takes control requests.
    pub fn set_control(&self, tx: mpsc::Sender<DriverControl>) {
        *self.control.lock().unwrap() = Some(tx);
    }

    /// Where to send a control request, if this agent's driver takes them.
    pub fn control(&self) -> Option<mpsc::Sender<DriverControl>> {
        self.control.lock().unwrap().clone()
    }

    /// The CLI wrote something.
    pub fn note_output(&self) {
        self.last_output_ms.store(proto::now_ms(), Ordering::Relaxed);
    }

    /// How long since the CLI last wrote anything; 0 if it never has.
    pub fn idle_ms(&self) -> u64 {
        match self.last_output_ms.load(Ordering::Relaxed) {
            0 => 0,
            at => proto::now_ms().saturating_sub(at).max(0) as u64,
        }
    }

    /// Has the CLI asked for bracketed paste?
    pub fn bracketed(&self) -> bool {
        self.modes.lock().unwrap().get(&2004).copied().unwrap_or(false)
    }

    /// Note keystrokes a HUMAN (an attached viewer) sent to this seat.
    ///
    /// Only ever the person's own keys: the node writes its injections
    /// straight to the pty, not through here, so what this sees is exactly
    /// what someone is typing at the box. The model is deliberately rough --
    /// a running count of characters in the unsent line, cleared on Enter and
    /// on the keys that clear a line -- because all `human_composing` needs
    /// from it is "empty" versus "mid-sentence".
    pub fn note_human_input(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut c = self.compose.lock().unwrap();
        let mut i = 0;
        while i < data.len() {
            match data[i] {
                // Enter submits the line; the box is empty again.
                b'\r' | b'\n' => c.chars = 0,
                // Ctrl-C and Ctrl-U clear the whole line at once.
                0x03 | 0x15 => c.chars = 0,
                // Backspace / delete takes a character back.
                0x08 | 0x7f => c.chars = c.chars.saturating_sub(1),
                // An escape sequence -- arrows, function keys, a
                // bracketed-paste marker -- is not typed text. Skip it
                // whole: CSI (`\x1b[`) and SS3 (`\x1b O`) run to a final
                // byte in 0x40..=0x7e. A lone ESC falls through and counts
                // for nothing.
                0x1b => {
                    i += 1;
                    if i < data.len() && (data[i] == b'[' || data[i] == b'O') {
                        i += 1;
                        while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                            i += 1;
                        }
                    }
                }
                // Any other control byte: not worth modelling.
                b if b < 0x20 => {}
                // A printable byte or the LEAD of a UTF-8 character counts as
                // one; continuation bytes (0x80..=0xbf) were counted by their
                // lead, so they add nothing.
                b if b < 0x80 || b >= 0xc0 => c.chars += 1,
                _ => {}
            }
            i += 1;
        }
        c.last_ms = proto::now_ms();
    }

    /// True while a human has an unsent input line and was typing recently
    /// enough that an injection now would land in the middle of it.
    ///
    /// After `COMPOSE_GRACE_MS` of quiet it goes back to false: a message
    /// must not be stuck for ever behind input somebody walked away from.
    pub fn human_composing(&self) -> bool {
        let c = self.compose.lock().unwrap();
        c.chars > 0 && proto::now_ms().saturating_sub(c.last_ms) < COMPOSE_GRACE_MS
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
        // ...and give back whatever room it was constraining: a small window
        // that has closed must not keep the agent's screen small.
        if self.sizes.lock().unwrap().remove(attach_id).is_some() {
            self.apply_agreed_size();
        }
    }

    pub fn attach_ids(&self) -> Vec<String> {
        self.attaches.lock().unwrap().iter().cloned().collect()
    }

    /// One viewer has said what size it is.
    ///
    /// Recorded rather than applied: the pty's size is the AGREEMENT of the
    /// viewers, computed below.
    /// Say which tmux view this pty drives, for a seat held in tmux.
    ///
    /// Set once the grouped client exists, because stating a size to a view
    /// that is not there yet does nothing.
    pub fn set_view(&self, view: &str) {
        *self.view.lock().unwrap() = Some(view.to_string());
        // The size agreed before the client existed never reached it.
        self.restate_size();
    }

    /// Tell tmux the size, rather than hoping its client noticed.
    fn restate_size(&self) {
        let Some(view) = self.view.lock().unwrap().clone() else { return };
        let Some((cols, rows)) = *self.applied.lock().unwrap() else { return };
        crate::tmux::resize_window(&view, cols, rows);
    }

    pub fn resize_from(&self, attach_id: &str, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return; // a viewer that does not know its own size yet
        }
        let changed = {
            let mut sizes = self.sizes.lock().unwrap();
            sizes.insert(attach_id.to_string(), (cols, rows)) != Some((cols, rows))
        };
        if changed {
            self.apply_agreed_size();
        }
    }

    /// The size every viewer can display: the SMALLEST each way.
    ///
    /// The alternative -- the newest viewer wins -- is what the pty did, and
    /// it meant a phone opening the dashboard shrank somebody's terminal
    /// mid-edit, while two viewers of different sizes made it oscillate for
    /// as long as both were open. Smallest-wins is what tmux does for
    /// several clients on one session, for the same reason: nobody is shown
    /// a screen with a piece missing.
    fn apply_agreed_size(&self) {
        let agreed = {
            let sizes = self.sizes.lock().unwrap();
            sizes.values().copied().reduce(|(ac, ar), (c, r)| (ac.min(c), ar.min(r)))
        };
        let Some((cols, rows)) = agreed else { return };
        // Nothing to do if that is already the size. A pty resize raises
        // SIGWINCH whether or not the numbers changed, and a TUI answers
        // SIGWINCH by redrawing everything.
        {
            let mut applied = self.applied.lock().unwrap();
            if *applied == Some((cols, rows)) {
                return;
            }
            *applied = Some((cols, rows));
        }
        *self.resizes.lock().unwrap() += 1;
        if let Some(m) = self.master.lock().unwrap().as_ref() {
            let _ = m.resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            tracing::debug!(cols, rows, "pty resized to the size every viewer can show");
        }
        // And for a seat held in tmux, say it rather than signal it.
        self.restate_size();
    }

    /// Set the size directly, for a caller that is not a viewer (the spawn
    /// path, which has no attachments yet).
    pub fn resize(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut applied = self.applied.lock().unwrap();
            if *applied == Some((cols, rows)) {
                return;
            }
            *applied = Some((cols, rows));
        }
        *self.resizes.lock().unwrap() += 1;
        if let Some(m) = self.master.lock().unwrap().as_ref() {
            let _ = m.resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// How many SIGWINCHes this agent has been given. Diagnostics, and what
    /// makes "it is not resized twice for the same size" testable.
    pub fn resize_count(&self) -> u64 {
        *self.resizes.lock().unwrap()
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
    /// What this seat may do to the fleet, as the hub told us at spawn.
    ///
    /// Held here only to build the tool list its shim advertises: the hub
    /// checks authority itself on every call, so this is a menu, not a key.
    /// It is recorded per AGENT and not per slot because that is the lifetime
    /// that matters -- a shim's tool list is fixed for as long as its CLI
    /// runs, so the level in force when the CLI started is the honest answer
    /// for as long as that CLI lives.
    pub authority: nexmesh_proto::authority::Authority,
    /// The directory this CLI was started in.
    ///
    /// Held because a CLI keeps its conversations per working directory --
    /// claude names the project directory after it -- so without it the
    /// node cannot find the file that says how full this agent's context
    /// is. The directory it was STARTED in, not whatever it may have
    /// changed to: that is the one the CLI filed its conversation under.
    pub cwd: String,
    /// Which conversation this agent is in, as last established.
    ///
    /// Only ever the CLI's own id, and only from a shim proved to be running
    /// inside this agent's process -- the same evidence `ResumeHandle`
    /// requires, for the same reason: a borrowed id would point the node at
    /// an unrelated conversation and every reading taken from it would be a
    /// measurement of somebody else's work.
    pub conversation: Option<String>,
}

pub struct ToolOutcome {
    pub result: serde_json::Value,
    pub deliver: Vec<Envelope>,
}

/// What the hub said about a CLI that joined by itself.
///
/// A named struct rather than the tuple it used to be: it grew a sixth field
/// and a six-tuple of mostly-`Option<String>` is a line nobody can read, let
/// alone add to safely.
pub struct Granted {
    pub slot_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    /// The seat's fleet authority, which decides the tool list this shim is
    /// given. Absent means `agent` -- authority fails CLOSED.
    pub authority: Option<String>,
}

pub struct JoinOutcome {
    pub granted: Result<Granted, String>,
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
    /// Notes for the chat about an agent that does not exist yet.
    ///
    /// Anything worth saying about HOW an agent was started is decided while
    /// it is being started -- before the hub has a row for it, because the
    /// node's own report of the start is what creates that row. A note sent
    /// then is dropped on the floor, which is how the one case worth saying
    /// something about (a system prompt the CLI cannot be given) said
    /// nothing at all. Queued here by agent id and flushed once the start is
    /// registered.
    pending_notes: Mutex<HashMap<String, Vec<String>>>,
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
            pending_notes: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn descriptor(&self, cli_id: &str) -> Option<&CliDescriptor> {
        self.descriptors.iter().find(|d| d.cli.id == cli_id)
    }

    /// Say something in the chat about an agent that is still starting.
    ///
    /// The hub has no row for it yet -- the node's own report of the start is
    /// what creates one -- so a `Noted` sent now is discarded. Held until
    /// `flush_notes`, which the start path calls once the agent is
    /// registered.
    pub fn note_when_started(&self, agent_id: &str, text: String) {
        self.pending_notes
            .lock()
            .unwrap()
            .entry(agent_id.to_string())
            .or_default()
            .push(text);
    }

    /// Send whatever was held for this agent. Called once it exists.
    pub async fn flush_notes(&self, agent_id: &str) {
        let held = self.pending_notes.lock().unwrap().remove(agent_id);
        for text in held.unwrap_or_default() {
            let _ = self
                .send_hub(&proto::NodeToHub::Noted {
                    agent_id: agent_id.to_string(),
                    text,
                })
                .await;
        }
    }

    /// Drop what was held for an agent that never started. Otherwise a seat
    /// that failed to start leaves a note that arrives, out of order and out
    /// of context, with the NEXT agent to use that id -- which cannot happen
    /// today, since ids are fresh per start, but a growing map that nothing
    /// empties is a leak either way.
    pub fn forget_notes(&self, agent_id: &str) {
        self.pending_notes.lock().unwrap().remove(agent_id);
    }

    /// Record what an agent's model is doing and, when that CHANGED, tell the
    /// hub at once -- the heartbeat repeats it, but a heartbeat is seconds and
    /// "it has started working" is worth knowing now.
    pub async fn report_activity(&self, agent_id: &str, io: &AgentIo, state: proto::ActivityState) {
        if io.set_activity(state) {
            let since_ms = io.activity().map(|(_, t)| t).unwrap_or_else(proto::now_ms);
            tracing::debug!(%agent_id, state = state.as_str(), "activity");
            let _ = self
                .send_hub(&proto::NodeToHub::Activity {
                    agent_id: agent_id.to_string(),
                    state,
                    since_ms,
                })
                .await;
        }
    }

    /// The same, for a caller that has only the agent's id.
    pub async fn report_activity_of(&self, agent_id: &str, state: proto::ActivityState) {
        if let Some(io) = self.agent_io(agent_id) {
            self.report_activity(agent_id, &io, state).await;
        }
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

    /// The agent this node believes is in that seat, if any.
    ///
    /// A seat holds at most one, so this is a lookup rather than a list. It
    /// exists because a seat held in tmux can outlive the hub's record of
    /// it: the node is still watching a live CLI while the hub has decided
    /// the seat is empty, and asking a second spawn to sort that out is how
    /// one seat ends up with two of everything.
    pub fn agent_in_slot(&self, slot_id: &str) -> Option<AgentRec> {
        self.agents.lock().unwrap().values().find(|a| a.slot_id == slot_id).cloned()
    }

    /// The agent whose PROCESS this shim is running inside.
    ///
    /// A CLI's MCP shim is told its agent id once, in the --mcp-config file
    /// written at the spawn that started it, and that file cannot be changed
    /// for the life of the process. So when a seat is REATTACHED -- the node
    /// restarts, finds the CLI still held in tmux and picks it up again --
    /// the hub mints a new agent id for a CLI whose shim is still saying the
    /// old one. The shim is then unknown, it is refused, and the agent can
    /// be typed at while every tool it has goes dead: it cannot reply to the
    /// operator, cannot store what it learns, cannot set its own summary.
    /// On this fleet that silently muted four seats, and the only visible
    /// symptom was one failed set_summary.
    ///
    /// Ancestry settles it without trusting the shim's claim about anything.
    /// A shim is a child of the CLI that started it, so the CLI is in its
    /// ancestor chain; and the start ticks say the pid is still the process
    /// the node recorded, not something wearing its number.
    /// Is this shim really running INSIDE that agent's CLI?
    ///
    /// A shim proves which seat it belongs to by its ancestry, never by the
    /// id it claims. The id is just text in an env var -- any process on this
    /// machine can set `NEXMESH_AGENT_ID` to a live agent and be believed,
    /// and what it is believed about includes WHICH CONVERSATION THAT SEAT IS
    /// IN. A diagnostic shim run by hand, carrying its own shell's
    /// `CLAUDE_CODE_SESSION_ID`, repointed a seat at an unrelated
    /// conversation that way; the next respawn of that seat read the handle
    /// back and tried to start its CLI on somebody else's thread, and only
    /// being held in tmux stopped it.
    ///
    /// So registration is still allowed on the claimed id -- refusing it
    /// would silence a live seat's tools over a check that has no business
    /// being fatal -- but anything the shim ASSERTS about the seat is gated
    /// on this.
    /// Record which conversation an agent is in.
    ///
    /// Called from the one place that learns it honestly: a shim proved to
    /// be running inside that agent's own process. It is kept on the node as
    /// well as sent to the hub because the node is what has to FIND the
    /// conversation on disk to measure it.
    pub fn note_conversation(&self, agent_id: &str, handle: &str) {
        if let Some(rec) = self.agents.lock().unwrap().get_mut(agent_id) {
            if rec.conversation.as_deref() != Some(handle) {
                tracing::debug!(agent = %agent_id, conversation = %handle,
                    "this agent's conversation, as its own shim reports it");
                rec.conversation = Some(handle.to_string());
            }
        }
    }

    pub fn shim_is_inside(&self, shim_pid: i32, rec: &AgentRec) -> bool {
        crate::proc::is_same_process(rec.proc.pid, rec.proc.start_ticks)
            && crate::proc::ancestors(shim_pid).contains(&rec.proc.pid)
    }

    pub fn agent_of_process(&self, shim_pid: i32) -> Option<AgentRec> {
        let line = crate::proc::ancestors(shim_pid);
        let map = self.agents.lock().unwrap();
        for p in line {
            if let Some(rec) = map
                .values()
                .find(|a| a.proc.pid == p && crate::proc::is_same_process(p, a.proc.start_ticks))
            {
                return Some(rec.clone());
            }
        }
        None
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

    /// What the pty would be sized to, given who is watching.
    ///
    /// `applied` is what was actually pushed, so these assertions are about
    /// the decision rather than about owning a real pty.
    fn applied_size(io: &Arc<AgentIo>) -> Option<(u16, u16)> {
        *io.applied.lock().unwrap()
    }

    #[test]
    fn the_pty_is_sized_to_what_every_viewer_can_show() {
        // One pty, several viewers, and they do not agree: a terminal that
        // reserves a row for its key bar asks for one less than the
        // dashboard's grid. Applying each as it arrived made the last
        // speaker win, so the size oscillated for as long as both were open
        // -- and every flip is a SIGWINCH, which is a full reflow of
        // whatever the agent was drawing.
        let io = io();
        io.resize_from("a", 120, 40);
        assert_eq!(applied_size(&io), Some((120, 40)));
        // A second, smaller viewer: the agent's screen has to fit in both.
        io.resize_from("b", 100, 30);
        assert_eq!(applied_size(&io), Some((100, 30)));
        // The bigger one restating its size changes nothing.
        io.resize_from("a", 120, 40);
        assert_eq!(applied_size(&io), Some((100, 30)));
    }

    #[test]
    fn an_unchanged_size_is_not_pushed_again() {
        // THE bug behind "the sizing keeps getting disrupted". A pty resize
        // raises SIGWINCH whether or not the numbers changed, and a viewer
        // that restates its size on every poll or every drag frame turned
        // that into a storm the agent answered by redrawing continuously.
        let io = io();
        io.resize_from("a", 100, 30);
        assert_eq!(io.resize_count(), 1);
        for _ in 0..20 {
            io.resize_from("a", 100, 30);
        }
        assert_eq!(io.resize_count(), 1,
                   "the pty was resized again for a size it already had, which is \
                    21 SIGWINCHes the agent had to redraw for");
    }

    #[test]
    fn a_viewer_that_leaves_gives_its_constraint_back() {
        // A phone that opened the dashboard must not keep somebody's
        // terminal small after it is closed.
        let io = io();
        io.add_attach("big");
        io.resize_from("big", 200, 50);
        io.add_attach("small");
        io.resize_from("small", 80, 24);
        assert_eq!(applied_size(&io), Some((80, 24)));
        io.remove_attach("small");
        assert_eq!(applied_size(&io), Some((200, 50)),
                   "the small viewer left and the screen stayed small");
    }

    #[test]
    fn a_viewer_with_no_size_yet_is_ignored() {
        // A zero from a client that has not measured itself would otherwise
        // become the agreement and collapse the screen to nothing.
        let io = io();
        io.resize_from("a", 100, 30);
        io.resize_from("b", 0, 0);
        assert_eq!(applied_size(&io), Some((100, 30)));
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

/// Finding the seat a shim belongs to when it names an agent id that is gone.
#[cfg(test)]
mod reattach_tests {
    use super::*;

    fn ctx() -> Arc<NodeCtx> {
        NodeCtx::new("machine-b".into(), vec![], "/bin/true".into(), "/tmp/x.sock".into())
    }


    /// A shim proves which seat it is in by ANCESTRY, never by the id it
    /// claims -- and what it would otherwise be believed about includes
    /// which conversation that seat is in.
    #[test]
    fn a_shim_outside_the_cli_is_not_inside_its_seat() {
        let c = ctx();
        // This test process stands in for the CLI, and its own pid for a
        // shim that is a descendant of it (every real shim is a child).
        let me = std::process::id() as i32;
        let parent = crate::proc::ppid(me).unwrap_or(1);
        let r = rec("ag-1", "sl-1", parent);
        c.agents.lock().unwrap().insert("ag-1".into(), r.clone());
        assert!(c.shim_is_inside(me, &r), "a shim in the CLI's own tree is inside it");
        // Pid 1 is not under this process, so a shim there is not ours
        // however correct the agent id it presents.
        assert!(!c.shim_is_inside(1, &r), "a shim outside the tree must not be believed");
    }

    #[test]
    fn a_dead_cli_cannot_have_a_shim_inside_it() {
        let c = ctx();
        // A recycled pid: the number is live, the start ticks are not this
        // process, so the identity does not hold and nothing is inside it.
        let me = std::process::id() as i32;
        let mut r = rec("ag-2", "sl-2", crate::proc::ppid(me).unwrap_or(1));
        r.proc.start_ticks = r.proc.start_ticks.wrapping_add(9_999);
        c.agents.lock().unwrap().insert("ag-2".into(), r.clone());
        assert!(!c.shim_is_inside(me, &r), "a recycled pid is not the CLI we recorded");
    }

    fn rec(agent: &str, slot: &str, pid: i32) -> AgentRec {
        AgentRec {
            agent_id: agent.into(),
            slot_id: slot.into(),
            session_id: "se-1".into(),
            cli_id: "claude".into(),
            authority: Default::default(),
            cwd: String::new(),
            conversation: None,
            // The real start ticks of that pid, so the identity check passes
            // for a live process and fails for a recycled number.
            proc: nexmesh_proto::ProcIdentity {
                node_id: "machine-b".into(),
                pid,
                start_ticks: crate::proc::start_ticks(pid).unwrap_or(0),
            },
            driver: nexmesh_proto::cli_descriptor::DriverSpec::McpOnly,
            tmux_view: None,
        }
    }

    /// THE BUG. A reattached seat has a new agent id; its CLI's shim still
    /// says the old one, because the --mcp-config naming it was written at
    /// the spawn that started the process and cannot be rewritten. The shim
    /// has to be matched by the process it is running inside.
    #[test]
    fn a_shim_is_found_by_the_process_it_runs_inside() {
        let ctx = ctx();
        // This test process stands in for the CLI, and its parent for the
        // node: a shim's own pid is a descendant of its CLI's.
        let me = std::process::id() as i32;
        let parent = crate::proc::ppid(me).unwrap_or(1);
        ctx.agents.lock().unwrap().insert("ag-new".into(), rec("ag-new", "sl-1", parent));

        // Asked about a pid inside that process's tree, it finds the seat.
        let found = ctx.agent_of_process(me).expect("the ancestor chain holds the CLI");
        assert_eq!(found.agent_id, "ag-new");
        assert_eq!(found.slot_id, "sl-1");
    }

    /// And it must not match something merely nearby. A shim whose tree holds
    /// no agent this node is watching is a shim the node should refuse,
    /// because registering it anyway is how one CLI takes another's seat.
    #[test]
    fn an_unrelated_process_is_not_matched() {
        let ctx = ctx();
        // A pid that is not in this process's ancestry. 1 is init, which is
        // in everybody's -- so use a number no tree of ours contains.
        ctx.agents.lock().unwrap().insert("ag-x".into(), rec("ag-x", "sl-9", i32::MAX - 7));
        assert!(ctx.agent_of_process(std::process::id() as i32).is_none());
    }

    /// A RECYCLED pid is not that process. The node records start ticks with
    /// every pid for exactly this, and a match on the number alone would
    /// hand a shim the seat of an agent that died.
    #[test]
    fn a_recycled_pid_does_not_count() {
        let ctx = ctx();
        let me = std::process::id() as i32;
        let parent = crate::proc::ppid(me).unwrap_or(1);
        let mut stale = rec("ag-old", "sl-1", parent);
        // Same pid, different process: what a reused pid looks like.
        stale.proc.start_ticks = stale.proc.start_ticks.wrapping_add(1);
        ctx.agents.lock().unwrap().insert("ag-old".into(), stale);
        assert!(
            ctx.agent_of_process(me).is_none(),
            "a pid whose start ticks do not match is not the process we recorded"
        );
    }

    /// An empty node matches nothing rather than guessing.
    #[test]
    fn a_node_watching_nothing_matches_nothing() {
        assert!(ctx().agent_of_process(std::process::id() as i32).is_none());
    }
}
