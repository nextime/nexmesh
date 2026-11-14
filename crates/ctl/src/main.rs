// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! `nexmesh` -- the operator CLI. Runs on any machine and talks to the hub over
//! HTTP, so it works from any machine, whether or not that machine hosts anything.

mod install;
mod seatcmd;
mod tlstrust;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nexmesh", version, about = "nexmesh fleet control")]
struct Args {
    #[arg(long, global = true, env = "NEXMESH_HUB_HTTP", default_value = "http://127.0.0.1:7870")]
    hub: String,
    /// Operator token. Mint one with `nexmesh-hub operator-token <label>`.
    #[arg(long, global = true, env = "NEXMESH_OPERATOR_TOKEN")]
    token: Option<String>,
    /// PEM file with the hub's certificate (or its CA), for a self-signed hub.
    #[arg(long, global = true, env = "NEXMESH_CA_CERT")]
    ca_cert: Option<String>,
    /// Your client certificate, when the hub requires mutual TLS.
    #[arg(long, global = true, env = "NEXMESH_CLIENT_CERT", requires = "client_key")]
    client_cert: Option<String>,
    /// Private key for --client-cert.
    #[arg(long, global = true, env = "NEXMESH_CLIENT_KEY", requires = "client_cert")]
    client_key: Option<String>,
    /// Directory holding a paired identity; anything not passed explicitly is
    /// taken from here.
    #[arg(long, global = true, env = "NEXMESH_IDENTITY", default_value = nexmesh_proto::identity::DEFAULT_DIR)]
    identity_dir: String,
    /// Left out, and run from a terminal, this opens the operator console.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hub health and version.
    Health,
    /// Nodes, their CLIs and whether they are connected.
    Nodes,
    /// List sessions.
    Sessions,
    /// Move a seat -- and whoever is sitting in it -- to another session.
    /// No restart: the agent's next tool call lands in the new session. Its
    /// workspace, file locks and ownership zones are released, since all
    /// three describe the session being left.
    #[command(after_help = "\
EXAMPLE
  nexmesh status payments            # find the seat in the SEAT column
  nexmesh move-agent sl-1a2b3c4d docs-site

The agent keeps working; its next tool call lands in the new session. It
returns to that session on every later restart, whatever directory it
starts in.")]
    MoveAgent {
        /// Seat id: the SEAT column of `nexmesh status`, like sl-1a2b3c4d.
        #[arg(value_name = "SEAT")]
        slot: String,
        /// Session to move it to: its name or id.
        #[arg(value_name = "SESSION")]
        session: String,
    },
    /// Remove a seat from a session, with whatever history belongs to it.
    /// Refuses while someone is sitting in it unless --force.
    #[command(after_help = "\
EXAMPLE
  nexmesh status payments            # find the seat in the SEAT column
  nexmesh remove-agent sl-1a2b3c4d   # refuses if somebody is sitting in it
  nexmesh remove-agent sl-1a2b3c4d --force")]
    RemoveAgent {
        /// Seat id: the SEAT column of `nexmesh status`, like sl-1a2b3c4d.
        #[arg(value_name = "SEAT")]
        slot: String,
        /// Remove it even though an agent is live in it.
        #[arg(long)]
        force: bool,
    },
    /// Rename an agent. The name is how every command, message and log line
    /// addresses it
    #[command(after_help = "\
EXAMPLES
  nexmesh rename-agent sl-1a2b3c4d Reviewer
  nexmesh rename-agent Claude-zeiss-3 Reviewer --session payments

A seat id works on its own. A NAME only means something inside a session,
so it needs --session. A name already used in that session is refused.")]
    RenameAgent {
        /// Seat id (sl-1a2b3c4d), or the agent's current name with --session.
        #[arg(value_name = "SEAT|NAME")]
        who: String,
        /// The new name, 64 characters or fewer.
        #[arg(value_name = "NEW-NAME")]
        name: String,
        /// Session to look the name up in. Needed when WHO is a name.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },
    /// Change what a seat is for. The role decides who gets asked for what,
    /// and who may approve whose work
    #[command(after_help = "\
EXAMPLES
  nexmesh set-role payments Reviewer reviewer
  nexmesh set-role sl-1a2b3c4d architect --description \"you decide the shape\"

A role is not decoration: list_peers and the capability router use it to
decide who to ask, and the review state machine uses it to decide who may
approve.")]
    SetRole {
        /// Session the seat is in — or a bare seat id (sl-1a2b3c4d).
        #[arg(value_name = "SESSION|SEAT")]
        session: String,
        /// Agent name from the NAME column, or a seat id.
        #[arg(value_name = "NAME|SEAT")]
        who: Option<String>,
        /// The new role: engineer, reviewer, architect — your words.
        #[arg(value_name = "ROLE")]
        role: Option<String>,
        /// What the role means for this seat, in its own words.
        #[arg(long)]
        description: Option<String>,
    },
    /// Rename a session. The name is what you read on the dashboard and in
    /// every message, so it should be allowed to change.
    RenameSession {
        session: String,
        name: String,
    },
    /// Create a session, optionally with a workspace on one node.
    NewSession {
        name: String,
        /// Workspace name.
        #[arg(long)]
        workspace: Option<String>,
        /// "node_local" or "repo".
        #[arg(long, default_value = "node_local")]
        kind: String,
        /// Node the workspace lives on (required for node_local).
        #[arg(long)]
        node: Option<String>,
        /// Absolute path on that node.
        #[arg(long)]
        path: Option<String>,
        /// Git remote, for a repo workspace.
        #[arg(long)]
        repo: Option<String>,
        /// Pause this session once it has spent this many tokens in total.
        #[arg(long)]
        token_budget: Option<i64>,
    },
    /// Set, raise or clear a session's token budget. Changing it re-arms the
    /// guard, so a session paused on budget can be given more and resumed.
    Budget {
        session: String,
        /// New ceiling in total tokens. Omit to remove the ceiling.
        #[arg(long)]
        tokens: Option<i64>,
    },
    /// Add a slot to a session.
    AddAgent {
        session: String,
        /// CLI id from a node's inventory, e.g. claude, opencode, qwen.
        #[arg(long)]
        cli: String,
        /// Model this seat should run, if its CLI can be told.
        #[arg(long)]
        model: Option<String>,
        /// Capability label teammates can address, e.g. --can strong --can vision.
        #[arg(long = "can")]
        can: Vec<String>,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        /// Pin to a node by name.
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value = "headless")]
        mode: String,
        /// Glob this agent owns, repeatable.
        #[arg(long = "owns")]
        owns: Vec<String>,
    },
    /// Start an agent in a seat, on the machine that seat belongs to.
    #[command(after_help = "\
EXAMPLES
  nexmesh spawn payments Reviewer           # by name, the usual way
  nexmesh spawn payments Reviewer opencode  # start a different CLI in it
  nexmesh spawn sl-1a2b3c4d                 # by seat id, no session needed

Then watch it from any machine:
  nexmesh attach payments Reviewer

Close the CLI in its own terminal first, or the seat ends up with two
processes in it. Naming a CLI is kept: the seat really is running that one
from then on.")]
    Spawn {
        /// Session the seat is in — or a bare seat id (sl-1a2b3c4d), in
        /// which case nothing else is needed.
        #[arg(value_name = "SESSION|SEAT")]
        session: String,
        /// Agent name from the NAME column, or a seat id.
        #[arg(value_name = "NAME|SEAT")]
        who: Option<String>,
        /// Start this CLI instead of the one the seat has been running:
        /// claude, codex, opencode, qwen — whatever `nexmesh nodes` lists.
        #[arg(value_name = "CLI")]
        cli: Option<String>,
        /// headless (driven, best delivery and token counts) or interactive
        /// (a real terminal you can attach to and type at).
        ///
        /// Left out, a seat spawned FROM A TERMINAL becomes interactive --
        /// someone typing this wants something to talk to -- while a spawn
        /// from a script or the orchestrator leaves the seat's own mode
        /// alone. Pass it explicitly and it is kept, like the CLI is.
        #[arg(long, value_name = "MODE")]
        mode: Option<String>,
    },
    /// Adopt an already-running tmux session so you can watch and drive it
    /// from anywhere. Your own terminal stays attached.
    #[command(after_help = "\
EXAMPLE
  tmux new -s mywork                 # start your CLI inside it
  nexmesh adopt payments --node machine-b --tmux mywork --name Reviewer
  nexmesh attach payments Reviewer

Your own terminal stays attached; others can watch and type too.")]
    Adopt {
        /// Session to join: its name or id.
        #[arg(value_name = "SESSION")]
        session: String,
        /// Node whose tmux server holds it.
        #[arg(long)]
        node: String,
        /// tmux target: session, window or pane (e.g. work, work:1.0, %3).
        #[arg(long = "tmux")]
        tmux_target: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Give one of your own conversations a keyboard: a tmux pane, adopted,
    /// with that conversation resumed inside it.
    ///
    /// A CLI you started by hand can only be reached when it next calls a
    /// tool. In an adopted pane the hub can type at it, so a message arrives
    /// in seconds. Run this on the machine the seat belongs to.
    Seat {
        /// Session the seat is in: its name or id.
        #[arg(value_name = "SESSION")]
        session: String,
        /// The seat, by the name you address it by.
        #[arg(value_name = "NAME")]
        who: String,
        /// tmux session to use or create. Default: nexmesh-<name>.
        #[arg(long = "tmux")]
        tmux: Option<String>,
        /// What to run in the pane. Default: the seat's own CLI, resuming the
        /// conversation that seat holds. Pass '' for a plain shell.
        #[arg(long)]
        cmd: Option<String>,
        #[arg(long, env = "NEXMESH_CLIS", default_value = "/etc/nexmesh/clis")]
        clis: String,
        /// Do not attach your own terminal afterwards.
        #[arg(long)]
        no_attach: bool,
        /// Print the three steps instead of taking them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a session and everything that belonged to it. Refuses while
    /// anybody is still sitting in it.
    RemoveSession {
        #[arg(value_name = "SESSION")]
        session: String,
        /// Remove it even though agents are still live in it.
        #[arg(long)]
        force: bool,
    },
    /// Live status of every agent in a session.
    Status { session: String },
    /// List the certificates the fleet CA has issued.
    Certs,
    /// Revoke a machine or operator: its certificate stops working at the TLS
    /// handshake, its token is revoked, and any connection it holds is closed.
    Revoke {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Stop sending work to an agent, or to the whole team. Reversible; a
    /// turn already running finishes.
    Pause {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Start sending work again.
    Resume {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
    },
    /// Accept an agent's work and let it stop. Omit the name to release the
    /// whole team.
    Release {
        session: String,
        who: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// How long to let it finish before terminating it.
        #[arg(long, default_value_t = 15000)]
        grace_ms: u64,
    },
    /// Publish, pull or report on a shared workspace, on every machine at once.
    Sync {
        session: String,
        /// publish | refresh | status
        #[arg(long, default_value = "status")]
        op: String,
        /// Workspace name; omitted means all shared ones in the session.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Read the conversation between the agents.
    Log {
        session: String,
        /// Keep printing new messages as they are sent.
        #[arg(short, long)]
        follow: bool,
        /// How many past messages to show first.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Read what one agent's process actually printed.
    Transcript {
        session: String,
        /// Agent name, role, slot id or agent id.
        who: String,
        /// Bytes from the end.
        #[arg(long, default_value_t = 65536)]
        tail: usize,
    },
    /// Attach to an agent's terminal, wherever it is running.
    ///
    /// An interactive agent can be watched AND typed into; a headless one is
    /// view-only. Detach with Ctrl-] -- the agent keeps working either way.
    #[command(after_help = "\
EXAMPLE
  nexmesh status payments            # the NAME column is what you pass
  nexmesh attach payments Reviewer

Reaches an agent the hub holds a terminal for: one it spawned in
interactive mode, or one adopted from tmux. A CLI you started by hand in an
ordinary terminal has none, so `spawn` or `adopt` it first.")]
    Attach {
        /// Session: its name or id.
        #[arg(value_name = "SESSION")]
        session: String,
        /// Agent name from the NAME column, or a seat id.
        #[arg(value_name = "NAME|SEAT")]
        who: String,
        /// Bytes of scrollback to replay first.
        #[arg(long, default_value_t = 16384)]
        replay: usize,
    },
    /// Join a fleet as an operator, using a one-time code from the dashboard.
    /// Your key is generated here and never leaves this machine.
    Pair {
        /// The hub's pairing address, e.g. https://machine-a:7871
        #[arg(long = "pair-hub")]
        pair_hub: String,
        /// The code shown in the dashboard.
        #[arg(long)]
        code: String,
        /// The address to use afterwards, e.g. https://machine-a:7870
        #[arg(long)]
        connect: Option<String>,
        /// Accept the hub's certificate without verifying it, for a first
        /// contact you have decided to trust.
        #[arg(long)]
        insecure: bool,
    },
    /// Register the orchestrator as an MCP server in one of your agent CLIs,
    /// so you can spin up teams from whatever session you are in.
    InstallOrchestrator {
        /// CLI to install into: claude, opencode, qwen, codex, gemini.
        #[arg(long = "cli")]
        cli_id: String,
        /// Directory of CLI descriptors.
        #[arg(long, env = "NEXMESH_CLIS", default_value = "/etc/nexmesh/clis")]
        clis: String,
        /// Name the server appears under inside that CLI.
        #[arg(long, default_value = "nexmesh")]
        name: String,
        /// Path to nexmesh-orch; defaults to a sibling of this binary.
        #[arg(long)]
        orch: Option<String>,
        /// Print what would be done without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Mount a workspace into a session that already exists. The directory is
    /// how an attached CLI finds the session it belongs to.
    AddWorkspace {
        /// Session id or name.
        session: String,
        /// Absolute path on that node.
        path: String,
        /// Node the directory is on.
        #[arg(long)]
        node: String,
        /// Workspace name; defaults to the directory's basename.
        #[arg(long)]
        name: Option<String>,
        /// "node_local" or "repo".
        #[arg(long, default_value = "node_local")]
        kind: String,
        /// Git remote, for a repo workspace.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Make one of your own agent CLIs a member of a team: every conversation
    /// you start in it takes a seat, wherever you are running it.
    InstallSeat {
        /// CLI to install into, as its descriptor names it.
        #[arg(long = "cli")]
        cli_id: String,
        /// Directory of CLI descriptors.
        #[arg(long, env = "NEXMESH_CLIS", default_value = "/etc/nexmesh/clis")]
        clis: String,
        /// Name the server appears under inside that CLI.
        #[arg(long, default_value = "nexmesh")]
        name: String,
        /// Role these seats take, unless the session says otherwise.
        #[arg(long)]
        role: Option<String>,
        /// Seat name. Leave unset: the hub names each conversation as it
        /// arrives, and a fixed name makes them all ask for the same seat.
        #[arg(long = "seat-name")]
        seat_name: Option<String>,
        /// The local node's socket.
        #[arg(long, env = "NEXMESH_SOCK", default_value = nexmesh_proto::localsock::DEFAULT_SOCKET)]
        sock: String,
        /// Path to nexmesh-mcpd; defaults to a sibling of this binary.
        #[arg(long)]
        mcpd: Option<String>,
        /// Print what would be done without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Send a message to the team or one agent, as the operator.
    Send {
        session: String,
        text: String,
        #[arg(long)]
        to: Option<String>,
    },
}

/// The operator token, read once from args/env and attached to every request.
/// Kept in a process-local rather than threaded through every call site, so a
/// new subcommand cannot forget to authenticate.
static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn token() -> Option<&'static String> {
    TOKEN.get().and_then(|t| t.as_ref())
}

fn client(ca_cert: Option<&str>, identity: Option<(&str, &str)>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
    if let Some(path) = ca_cert {
        let pem = std::fs::read(path).with_context(|| format!("reading CA file {path}"))?;
        b = b.add_root_certificate(
            reqwest::Certificate::from_pem(&pem)
                .with_context(|| format!("parsing {path} as a PEM certificate"))?,
        );
    }
    if let Some((cert, key)) = identity {
        // reqwest wants one PEM containing both.
        let mut pem = std::fs::read(cert)
            .with_context(|| format!("reading client certificate {cert}"))?;
        pem.push(b'\n');
        pem.extend(std::fs::read(key).with_context(|| format!("reading client key {key}"))?);
        b = b.identity(
            reqwest::Identity::from_pem(&pem)
                .context("the client certificate and key could not be loaded together")?,
        );
    }
    Ok(b.build()?)
}

fn authed(r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token() {
        Some(t) => r.bearer_auth(t),
        None => r,
    }
}

/// Turn an HTTP error into the hub's own message, so an auth failure says what
/// to do instead of surfacing as a JSON parse error.
async fn body_or_error(r: reqwest::Response, what: &str) -> Result<Value> {
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(v);
    }
    let msg = v
        .get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| text.chars().take(200).collect());
    anyhow::bail!("{what} failed ({status}): {msg}")
}

/// A seat id, from a name and the session it is in.
///
/// Every command that acts on one agent needs the same thing, and a person
/// reading the dashboard has the NAME, not the id. Making them look it up
/// first is a step a computer can take.
async fn resolve_seat(
    c: &reqwest::Client,
    base: &str,
    session: &str,
    who: &str,
) -> Result<String> {
    let v = get(c, format!("{base}/v1/sessions/{session}/status")).await?;
    let empty = vec![];
    let agents = v["agents"].as_array().unwrap_or(&empty);
    if let Some(slot) = agents
        .iter()
        .find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(who)))
        .and_then(|a| a["slot_id"].as_str())
    {
        return Ok(slot.to_string());
    }
    let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
    anyhow::bail!(
        "no agent called '{who}' in '{session}'. There is: {}",
        if names.is_empty() { "nobody".to_string() } else { names.join(", ") }
    )
}

async fn get(c: &reqwest::Client, url: String) -> Result<Value> {
    let r = authed(c.get(&url)).send().await.with_context(|| format!("GET {url}"))?;
    body_or_error(r, "request").await
}

async fn post(c: &reqwest::Client, url: String, body: Value) -> Result<Value> {
    let r = authed(c.post(&url).json(&body))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    body_or_error(r, "request").await
}

fn bail_on_error(v: &Value) -> Result<()> {
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    Ok(())
}

/// Token counts get large; 12400 reads better as 12.4k in a table.
fn human(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn hhmmss(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::offset::LocalResult::Single(t) => t.format("%H:%M:%S").to_string(),
        _ => "--:--:--".to_string(),
    }
}

/// One line per message. The delivery marker matters: a message that was sent
/// but not acked is one the recipient's model has NOT seen yet, which is
/// usually the answer to "why is this team stuck?".
///
/// For a team message the TO column carries the count, because "delivered"
/// on a broadcast used to mean one teammate had taken it -- and a capability
/// bridge, which is handed team chat it cannot answer, acks everything. An
/// operator's question read as delivered while the only seats that could
/// answer had never been handed it: `(team 1/3)` is the line that says so.
fn print_message(m: &Value) {
    let mark = if m["delivered"].as_bool() == Some(true) {
        " "
    } else if m["in_flight"].as_bool() == Some(true) {
        "~" // handed over, not yet confirmed
    } else {
        "!" // queued, recipient has not seen it
    };
    let kind = m["kind"].as_str().unwrap_or("text");
    let kind = if kind == "text" { String::new() } else { format!("[{kind}] ") };
    let to = match (m["to"].as_str(), m["audience"].as_i64()) {
        (Some("(team)"), Some(n)) => {
            format!("(team {}/{n})", m["seen"].as_i64().unwrap_or(0))
        }
        (Some(t), _) => t.to_string(),
        _ => "?".to_string(),
    };
    println!(
        "{} {} {:>12} -> {:<12} {}{}",
        hhmmss(m["at_ms"].as_i64().unwrap_or(0)),
        mark,
        m["from"].as_str().unwrap_or("?"),
        to,
        kind,
        m["body"].as_str().unwrap_or("").replace('\n', " / "),
    );
}

/// Puts the terminal back the way we found it, even on an early return or a
/// panic. Leaving a shell in raw mode is a genuinely hostile thing to do.
struct RawGuard;

impl RawGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("entering raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        println!("\r");
    }
}

/// Ctrl-] detaches, the way telnet does. Ctrl-C cannot: in raw mode it belongs
/// to the agent, which is the whole point of a takeover.
const DETACH_BYTE: u8 = 0x1d;

/// A keyboard this viewer can WALK AWAY FROM.
///
/// `tokio::io::stdin()` reads on a blocking thread that nothing can cancel.
/// For a viewer the hub can close -- a refusal, an agent that exits, a node
/// that goes -- that is not a detail: the read is still outstanding when the
/// viewer is done, so the process hangs at exit until somebody presses a key,
/// and inside the console it was worse. The console came back, drew itself,
/// and the first keys went to the dead attach's reader instead of to it. The
/// console looked broken, which is exactly what it was told it was.
///
/// So the viewer opens its OWN handle on the terminal, non-blocking, and
/// polls it. Dropping it abandons nothing and swallows nothing.
enum Keyboard {
    Tty(tokio::io::unix::AsyncFd<std::fs::File>),
    /// Not a terminal (a pipe, a test, a script). Nothing can be typed at it
    /// anyway, so the old path is fine.
    Piped(tokio::io::Stdin),
}

impl Keyboard {
    fn open() -> Self {
        use std::os::fd::AsRawFd;
        if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
            // O_NONBLOCK on OUR fd, never on fd 0: that flag is shared with
            // whoever gave us stdin, and setting it there has broken the
            // shell that started us ever since shells existed.
            use nix::fcntl::{fcntl, FcntlArg, OFlag};
            let fd = f.as_raw_fd();
            if let Ok(bits) = fcntl(fd, FcntlArg::F_GETFL) {
                let mut fl = OFlag::from_bits_truncate(bits);
                fl.insert(OFlag::O_NONBLOCK);
                if fcntl(fd, FcntlArg::F_SETFL(fl)).is_ok() {
                    // Safe because the AsyncFd takes the File and owns it
                    // for as long as it lives: the fd cannot be closed under
                    // the registration, which is the thing being promised.
                    let registered = unsafe { tokio::io::unix::AsyncFd::register(f) };
                    if let Ok(afd) = registered {
                        return Keyboard::Tty(afd);
                    }
                }
            }
        }
        Keyboard::Piped(tokio::io::stdin())
    }

    /// Cancel-safe: on a tty this waits for readability and reads without
    /// blocking, so dropping the future leaves no thread holding the terminal.
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Keyboard::Tty(afd) => loop {
                let mut guard = afd.readable().await?;
                match guard.try_io(|inner| {
                    use std::io::Read;
                    inner.get_ref().read(buf)
                }) {
                    Ok(res) => return res,
                    Err(_would_block) => continue,
                }
            },
            Keyboard::Piped(stdin) => {
                use tokio::io::AsyncReadExt;
                stdin.read(buf).await
            }
        }
    }
}

/// One row at the bottom of the screen that is ours, not the agent's.
///
/// An attach is a full-screen takeover of somebody else's terminal, and the
/// question it always raises is "how do I get out of this?". The remote is
/// told the screen is one row shorter than it is, and that row says whose
/// terminal this is and which key brings you back.
struct StatusBar {
    text: String,
    cols: u16,
    rows: u16,
}

impl StatusBar {
    /// The bar is skipped on a screen too short to spare a row for it.
    fn new(text: String, cols: u16, rows: u16) -> Option<Self> {
        (rows >= 4).then_some(StatusBar { text, cols, rows })
    }

    /// Reserve the row: the agent's screen is everything above it.
    fn agent_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }

    /// Save the cursor, paint the bar, put the cursor back. Reverse video
    /// rather than colour, because this has to read the same on every
    /// terminal anyone attaches from.
    fn paint(&self) -> Vec<u8> {
        let mut line = self.text.clone();
        let w = self.cols as usize;
        if line.chars().count() > w {
            line = line.chars().take(w).collect();
        } else {
            let pad = w - line.chars().count();
            line.push_str(&" ".repeat(pad));
        }
        format!("\x1b7\x1b[{};1H\x1b[7m{}\x1b[0m\x1b8", self.rows, line).into_bytes()
    }

    /// Keep the agent's output from scrolling over the bar. A full-screen app
    /// positions absolutely and stays inside the size it was given; a stream
    /// of plain output scrolls, and without this it would scroll our row away.
    fn scroll_region(&self) -> Vec<u8> {
        format!("\x1b[1;{}r", self.agent_rows()).into_bytes()
    }

    fn release(&self) -> Vec<u8> {
        format!("\x1b[r\x1b[{};1H\x1b[2K", self.rows).into_bytes()
    }
}

async fn attach(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    replay: usize,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
) -> Result<String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let r = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
    bail_on_error(&r)?;
    let agent = r["agent_id"].as_str().context("no agent id returned")?.to_string();
    let name = r["name"].as_str().unwrap_or(who).to_string();
    // There is a terminal to attach to only while there is a process. Saying
    // so here beats failing inside the websocket upgrade, where the reason
    // reads like a transport problem.
    if r["exited"].as_bool() == Some(true) || r["status"].as_str() == Some("disconnected") {
        anyhow::bail!(
            "{name} has no process to attach to right now (its last agent is {}).\n\
             Read what it said with:  nexmesh transcript {session} {who}\n\
             Put something in the seat with:  nexmesh spawn {session} {who}",
            r["status"].as_str().unwrap_or("gone")
        );
    }

    // A terminal with no size at all is not a terminal: run under `script`
    // into a file, or any pty nobody sized, and `size()` answers 0x0 -- which
    // we then asked the hub to make the agent's screen.
    let (cols, rows) = match crossterm::terminal::size() {
        Ok((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (120, 40),
    };
    let host = r["host"].as_str().unwrap_or("");
    let mut bar = StatusBar::new(
        format!(
            " nexmesh \u{203a} {name}{}{}  \u{00b7}  Ctrl-] detach",
            if host.is_empty() || host == "-" { String::new() } else { format!(" on {host}") },
            format!("  \u{00b7}  {session}")
        ),
        cols,
        rows,
    );
    // The agent's screen is what is left after the bar takes its row, so it
    // is the size the hub is told. Without this the bar is painted over by
    // anything that fills the screen.
    let agent_rows = bar.as_ref().map(|b| b.agent_rows()).unwrap_or(rows);
    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!(
        "{ws_base}/v1/attach/ws?agent={agent}&cols={cols}&rows={agent_rows}&replay={replay}"
    );
    // The attach upgrade needs the operator token too. It goes in a header,
    // never the query string: URLs land in proxy logs and shell history.
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.as_str().into_client_request().context("building the attach request")?;
    if let Some(t) = token() {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {t}").parse().context("building the auth header")?,
        );
    }
    let (ws, _) = if url.starts_with("wss://") {
        let tls = tlstrust::client_config(ca_cert, identity)?;
        tokio_tungstenite::connect_async_tls_with_config(
            req,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        )
        .await
        .with_context(|| format!("attaching to {name}"))?
    } else {
        tokio_tungstenite::connect_async(req)
            .await
            .with_context(|| format!("attaching to {name}"))?
    };
    let (mut sink, mut stream) = ws.split();

    println!("attached to {name} ({agent}) -- Ctrl-] to detach");

    // Raw mode only after the connection is up, so a failure prints normally.
    let _guard = RawGuard::enter()?;

    // The bar, and the room made for it, before a byte of the agent's output
    // arrives.
    if let Some(b) = &bar {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(&b.scroll_region()).await;
        let _ = out.write_all(&b.paint()).await;
        let _ = out.flush().await;
    }

    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::channel::<String>(4);

    // Hub -> our terminal.
    let reader = {
        let quit_tx = quit_tx.clone();
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            while let Some(Ok(msg)) = stream.next().await {
                match msg {
                    WsMessage::Binary(data) => {
                        if out.write_all(&data).await.is_err() || out.flush().await.is_err() {
                            break;
                        }
                    }
                    WsMessage::Text(t) => {
                        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        match v["t"].as_str() {
                            Some("opened") => {
                                if v["read_only"].as_bool() == Some(true) {
                                    let _ = out.write_all(
                                        b"\r\n[nexmesh] headless agent: view-only.\r\n").await;
                                    let _ = out.flush().await;
                                }
                            }
                            Some("closed") | Some("error") => {
                                let why = v["reason"].as_str()
                                    .or(v["message"].as_str())
                                    .unwrap_or("session ended");
                                let _ = quit_tx.send(why.to_string()).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
            let _ = quit_tx.send("connection closed".into()).await;
        })
    };

    // Window changes must reach the pty or a TUI redraws at the wrong size.
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
    tokio::spawn(async move {
        let Ok(mut sigwinch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while sigwinch.recv().await.is_some() {
            if let Ok((c, r)) = crossterm::terminal::size() {
                if resize_tx.send((c, r)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Our keyboard -> hub. Our own fd, non-blocking: see `Keyboard`.
    let mut keys = Keyboard::open();
    let mut buf = vec![0u8; 4096];
    // A full-screen agent redraws over anything, including our row, so the
    // bar is repainted rather than merely written once. Four times a second
    // is below noticing and far below costing anything.
    let mut repaint = tokio::time::interval(Duration::from_millis(250));
    repaint.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        tokio::select! {
            _ = repaint.tick(), if bar.is_some() => {
                if let Some(b) = &bar {
                    let mut out = tokio::io::stdout();
                    let _ = out.write_all(&b.paint()).await;
                    let _ = out.flush().await;
                }
            }
            n = keys.read(&mut buf) => {
                let n = match n { Ok(0) | Err(_) => break "input ended".to_string(), Ok(n) => n };
                if let Some(pos) = buf[..n].iter().position(|b| *b == DETACH_BYTE) {
                    // Send anything typed before the detach key, then leave.
                    if pos > 0 {
                        let _ = sink.send(WsMessage::Binary(buf[..pos].to_vec())).await;
                    }
                    break "detached".to_string();
                }
                if sink.send(WsMessage::Binary(buf[..n].to_vec())).await.is_err() {
                    break "connection lost".to_string();
                }
            }
            Some((c, r)) = resize_rx.recv() => {
                // The agent still gets one row less than the window, and the
                // bar moves to the new bottom.
                let mut agent_r = r;
                if let Some(b) = &mut bar {
                    b.cols = c;
                    b.rows = r;
                    agent_r = b.agent_rows();
                    let mut out = tokio::io::stdout();
                    let _ = out.write_all(&b.scroll_region()).await;
                    let _ = out.write_all(&b.paint()).await;
                    let _ = out.flush().await;
                }
                let msg = json!({ "t": "resize", "cols": c, "rows": agent_r }).to_string();
                let _ = sink.send(WsMessage::Text(msg)).await;
            }
            Some(why) = quit_rx.recv() => break why,
        }
    };

    let _ = sink.send(WsMessage::Text(json!({ "t": "close" }).to_string())).await;
    let _ = sink.close().await;
    reader.abort();
    if let Some(b) = &bar {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(&b.release()).await;
        let _ = out.flush().await;
    }
    drop(_guard);
    // "the agent exited. The agent is still running." was printed as one
    // line, and read as nonsense. The reassurance belongs only to a detach
    // the viewer chose: if the agent has gone, saying it is running is the
    // opposite of true.
    Ok(end_line(&reason))
}

/// What to say about an attach that has ended. Returned rather than printed,
/// because the console shows it as a notice on its own screen -- printing it
/// there left the reason under whatever the console drew next, so an attach
/// that was REFUSED looked like an attach that did nothing.
fn end_line(reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    let gone = reason.contains("exited")
        || reason.contains("closed the connection")
        || reason.contains("node disconnected");
    // A seat with no terminal is not a failure to explain away: it is the one
    // case with a cure, so the cure goes on the SECOND line. The console
    // shows the first line only, which is why the first line has to stand on
    // its own.
    let cure = if reason.contains("no terminal here") {
        "\nGive it one:  nexmesh seat <session> <name>           (a tmux pane, adopted, with that conversation in it)"
    } else {
        ""
    };
    if gone {
        format!("{reason}.{cure}")
    } else {
        format!("{reason}. The agent is still running.{cure}")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    // A paired identity supplies anything not given explicitly.
    let identity = nexmesh_proto::identity::load(&args.identity_dir);
    if let Some(id) = &identity {
        if args.token.is_none() {
            args.token = Some(id.identity.token.clone());
        }
        if args.ca_cert.is_none() {
            args.ca_cert = Some(id.ca_cert_path());
        }
        if args.client_cert.is_none() && args.client_key.is_none() {
            args.client_cert = Some(id.cert_path());
            args.client_key = Some(id.key_path());
        }
        if let (Some(h), true) = (&id.identity.hub, args.hub == "http://127.0.0.1:7870") {
            args.hub = h.clone();
        }
    }

    let base = args.hub.trim_end_matches('/').to_string();
    let _ = TOKEN.set(args.token.clone());
    let identity = match (&args.client_cert, &args.client_key) {
        (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
        _ => None,
    };
    let c = client(args.ca_cert.as_deref(), identity)?;

    // No subcommand. A person at a terminal gets the console; a script gets
    // help, as it always did. Judging that by the terminal rather than by a
    // flag is the whole difference between a tool that feels alive when you
    // sit down at it and one that clears your screen inside a pipeline.
    let Some(cmd) = args.cmd else {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            let owned = match (&args.client_cert, &args.client_key) {
                (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
                _ => None,
            };
            return tui::run(base, c, args.ca_cert.clone(), owned).await;
        }
        // `--help` writes to stdout and exits 0; a missing subcommand is a
        // usage error, so this goes to stderr and exits non-zero.
        use clap::CommandFactory;
        Args::command().print_help()?;
        eprintln!();
        anyhow::bail!("no subcommand given, and this is not a terminal");
    };

    match cmd {
        Cmd::Health => println!("{:#}", get(&c, format!("{base}/healthz")).await?),

        Cmd::Nodes => {
            let v = get(&c, format!("{base}/v1/nodes")).await?;
            let empty = vec![];
            let nodes = v["nodes"].as_array().unwrap_or(&empty);
            if nodes.is_empty() {
                println!("no nodes enrolled");
                return Ok(());
            }
            println!("{:<12} {:<11} {:<14} CLIS", "NAME", "STATE", "ARCH");
            for n in nodes {
                let state = if n["connected"].as_bool() == Some(true) {
                    "connected"
                } else {
                    n["status"].as_str().unwrap_or("?")
                };
                println!(
                    "{:<12} {:<11} {:<14} {}",
                    n["name"].as_str().unwrap_or("?"),
                    state,
                    format!("{}/{}", n["os"].as_str().unwrap_or("?"), n["arch"].as_str().unwrap_or("?")),
                    n["clis"].as_array().map(|a| a.iter().filter_map(|x| x.as_str())
                        .collect::<Vec<_>>().join(",")).unwrap_or_default(),
                );
            }
        }

        Cmd::Sessions => {
            let v = get(&c, format!("{base}/v1/sessions")).await?;
            let empty = vec![];
            let rows = v["sessions"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no sessions");
                return Ok(());
            }
            println!("{:<14} {:<22} {:<10} SLOTS", "ID", "NAME", "STATUS");
            for s in rows {
                println!(
                    "{:<14} {:<22} {:<10} {}",
                    s["id"].as_str().unwrap_or("?"),
                    s["name"].as_str().unwrap_or("?"),
                    s["status"].as_str().unwrap_or("?"),
                    s["slots"].as_i64().unwrap_or(0)
                );
            }
        }

        Cmd::MoveAgent { slot, session } => {
            let v = post(&c, format!("{base}/v1/slots/{slot}/move"),
                         json!({ "session": session })).await?;
            println!("{} moved to {}", slot, v["session_name"].as_str().unwrap_or(&session));
            let left = v["messages_left_unread"].as_i64().unwrap_or(0);
            if left > 0 {
                println!("  {left} message(s) it had not read stay in the old session");
            }
            let l = v["locks_released"].as_i64().unwrap_or(0);
            let z = v["zones_released"].as_i64().unwrap_or(0);
            if l > 0 || z > 0 {
                println!("  released {l} lock(s) and {z} ownership zone(s)");
            }
        }

        Cmd::RemoveAgent { slot, force } => {
            let url = format!("{base}/v1/slots/{slot}{}", if force { "?force=1" } else { "" });
            // authed(), like every other call here: a bare client sends no
            // operator token and the hub rightly refuses it.
            let r = authed(c.delete(&url)).send().await.with_context(|| format!("DELETE {url}"))?;
            let v = body_or_error(r, "removing the slot").await?;
            println!("removed {} ({} agent rows)", slot, v["agent_rows"].as_i64().unwrap_or(0));
        }

        Cmd::RenameAgent { who, name, session } => {
            // A seat id works anywhere; a name needs to be looked up, and
            // only a session can say which agent a name means.
            let slot = if who.starts_with("sl-") {
                who.clone()
            } else {
                let s = session.clone().ok_or_else(|| anyhow::anyhow!(
                    "'{who}' is a name, not a seat id, so say which session: --session <name>"
                ))?;
                let v = get(&c, format!("{base}/v1/sessions/{s}/status")).await?;
                let empty = vec![];
                let agents = v["agents"].as_array().unwrap_or(&empty);
                agents
                    .iter()
                    .find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&who)))
                    .and_then(|a| a["slot_id"].as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        let names: Vec<&str> =
                            agents.iter().filter_map(|a| a["name"].as_str()).collect();
                        anyhow::anyhow!(
                            "no agent called '{who}' in '{s}'. There is: {}",
                            if names.is_empty() { "nobody".to_string() } else { names.join(", ") }
                        )
                    })?
            };
            let v = post(&c, format!("{base}/v1/slots/{slot}/name"),
                         json!({ "name": name })).await?;
            bail_on_error(&v)?;
            match v["was"].as_str() {
                Some(was) => println!("{slot}: \"{was}\" is now \"{}\"",
                                      v["name"].as_str().unwrap_or(&name)),
                None => println!("{slot} is now \"{}\"", v["name"].as_str().unwrap_or(&name)),
            }
        }

        Cmd::SetRole { session, who, role, description } => {
            // Same two shapes as spawn: a seat id on its own, or a session
            // and a name.
            let (slot, role) = match (who.as_deref(), role.as_deref()) {
                (Some(w), Some(r)) if w.starts_with("sl-") => (w.to_string(), r.to_string()),
                (Some(w), Some(r)) => (resolve_seat(&c, &base, &session, w).await?, r.to_string()),
                (Some(r), None) if session.starts_with("sl-") => (session.clone(), r.to_string()),
                _ => anyhow::bail!("say which seat and which role: \
                            `nexmesh set-role <session> <name> <role>`, or \
                            `nexmesh set-role <seat> <role>`"),
            };
            let mut body = json!({ "role": role });
            if let Some(d) = description { body["description"] = json!(d); }
            let v = post(&c, format!("{base}/v1/slots/{slot}/role"), body).await?;
            bail_on_error(&v)?;
            match v["was"].as_str() {
                Some(was) => println!("{slot}: \"{was}\" is now \"{}\"",
                                      v["role"].as_str().unwrap_or(&role)),
                None => println!("{slot} is now \"{}\"", v["role"].as_str().unwrap_or(&role)),
            }
        }

        Cmd::RenameSession { session, name } => {
            let v = post(&c, format!("{base}/v1/sessions/{session}/name"),
                         json!({ "name": name })).await?;
            println!("{} is now \"{}\"", session, v["name"].as_str().unwrap_or(&name));
        }

        Cmd::NewSession { name, workspace, kind, node, path, repo, token_budget } => {
            let mut body = json!({ "name": name, "workspaces": [] });
            if let Some(b) = token_budget {
                body["token_budget"] = json!(b);
            }
            if let Some(ws) = workspace {
                if kind == "node_local" && (node.is_none() || path.is_none()) {
                    anyhow::bail!("a node_local workspace needs --node and --path");
                }
                let mut w = json!({ "name": ws, "kind": kind, "mounts": {} });
                if let Some(n) = &node {
                    w["node"] = json!(n);
                    if let Some(p) = &path {
                        w["mounts"] = json!({ n.clone(): p });
                    }
                }
                if let Some(r) = &repo {
                    w["repo_url"] = json!(r);
                }
                body["workspaces"] = json!([w]);
            }
            let v = post(&c, format!("{base}/v1/sessions"), body).await?;
            bail_on_error(&v)?;
            println!("session {} created", v["session_id"].as_str().unwrap_or("?"));
        }

        Cmd::Budget { session, tokens } => {
            let mut body = json!({});
            if let Some(t) = tokens {
                body["token_budget"] = json!(t);
            }
            let v = post(&c, format!("{base}/v1/sessions/{session}/budget"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("budget updated"));
        }

        Cmd::AddAgent {
            session, cli, model, can, name, role, task, workspace, node, mode, owns,
        } => {
            let mut body = json!({
                "cli_id": cli, "display_name": name, "mode": mode,
                "file_ownership": owns, "capabilities": can,
            });
            if let Some(m) = model { body["model"] = json!(m); }
            if let Some(r) = role { body["role"] = json!(r); }
            if let Some(t) = task { body["task"] = json!(t); }
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            if let Some(n) = node { body["placement"] = json!({ "node": n }); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/slots"), body).await?;
            bail_on_error(&v)?;
            println!("slot {} created", v["slot_id"].as_str().unwrap_or("?"));
        }

        Cmd::Spawn { session, who, cli, mode } => {
            // Two shapes, because both are natural: name the seat directly
            // when you have its id, or say which team and who in it.
            let slot = match who.as_deref() {
                None => session.clone(),
                Some(w) if w.starts_with("sl-") => w.to_string(),
                Some(w) => resolve_seat(&c, &base, &session, w).await?,
            };
            let mut body = json!({});
            if let Some(c) = cli.as_deref().filter(|x| !x.trim().is_empty()) {
                body["cli"] = json!(c);
            }
            // A person at a terminal who spawns a seat wants something they
            // can talk to; headless is the right default for a program and
            // the wrong one for a keyboard. A script keeps today's behaviour
            // exactly -- nothing is sent, and the seat's own mode stands --
            // because a team declared headless must not quietly become
            // interactive just because something restarted it.
            let chosen = match mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                Some(m) => Some(m.to_string()),
                None => std::io::IsTerminal::is_terminal(&std::io::stdout())
                    .then(|| "interactive".to_string()),
            };
            if let Some(m) = &chosen {
                body["mode"] = json!(m);
            }
            let v = post(&c, format!("{base}/v1/slots/{slot}/spawn"), body).await?;
            bail_on_error(&v)?;
            println!(
                "spawning {} on {} ({})",
                v["agent_id"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?"),
                v["mode"].as_str().unwrap_or("?")
            );
            // Said once, where the decision was made. A default nobody can
            // see is indistinguishable from a bug the first time it is not
            // what you wanted.
            if mode.is_none() && v["mode"].as_str() == Some("interactive") {
                println!("  interactive because you are at a terminal; --mode headless for a driven agent");
                println!("  watch it with:  nexmesh attach {session} {}",
                         who.as_deref().unwrap_or(&slot));
            }
        }

        Cmd::Adopt { session, node, tmux_target, name, role, workspace } => {
            let mut body = json!({ "node": node, "tmux_target": tmux_target, "name": name });
            if let Some(r) = role { body["role"] = json!(r); }
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/adopt"), body).await?;
            bail_on_error(&v)?;
            println!(
                "adopted {} on {} as slot {}",
                v["tmux_target"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?"),
                v["slot_id"].as_str().unwrap_or("?")
            );
            println!("attach with:  nexmesh attach {session} {name}");
            if let Some(n) = v["note"].as_str() {
                println!("note: {n}");
            }
        }

        Cmd::Seat { session, who, tmux, cmd, clis, no_attach, dry_run } => {
            seatcmd::run(&base, &c, seatcmd::Opts {
                session: &session,
                who: &who,
                tmux: tmux.as_deref(),
                clis_dir: &clis,
                cmd: cmd.as_deref(),
                attach: !no_attach,
                dry_run,
            })
            .await?;
        }

        Cmd::RemoveSession { session, force } => {
            let url = format!(
                "{base}/v1/sessions/{session}{}",
                if force { "?force=1" } else { "" }
            );
            let r = authed(c.delete(&url)).send().await.with_context(|| format!("DELETE {url}"))?;
            let v = body_or_error(r, "removing the session").await?;
            bail_on_error(&v)?;
            println!(
                "removed session '{}' and its {} seat(s)",
                v["name"].as_str().unwrap_or(&session),
                v["seats"].as_i64().unwrap_or(0)
            );
        }

        Cmd::Status { session } => {
            let v = get(&c, format!("{base}/v1/sessions/{session}/status")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let rows = v["agents"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no agents in this session");
                return Ok(());
            }
            // Both identifiers, always. An agent has a NAME, which is what
            // every command addresses it by and what a person reads, and a
            // SEAT id, which is what the hub and half the API speak. Showing
            // one here and the other there is how a rename gets applied to
            // the wrong agent: it happened on this fleet.
            // AGENT and STATE are two different questions and reading one
            // for the other wastes an afternoon: STATE is the WORK (idle,
            // working, released), which outlives the process doing it, while
            // AGENT is whether there is a process there at all. A seat
            // reading `idle` with nothing behind it cannot be attached to,
            // messaged, or spawned into without taking the seat back first.
            //
            // PID and IO say which process and whether it has a terminal:
            // `pty` can be attached to and typed at, `pipes` can be watched,
            // and a blank means the node never started it -- it connected to
            // the node itself, so only its own shim can reach it.
            println!(
                "{:<16} {:<14} {:<16} {:<11} {:<9} {:<9} {:<14} {:<13} {:>7} {:<5} {:>8} SUMMARY",
                "NAME", "SEAT", "CLI", "ROLE", "HOST", "KIND", "STATE", "AGENT", "PID", "IO",
                "TOKENS"
            );
            for a in rows {
                let tin = a["input_tokens"].as_i64().unwrap_or(0);
                let tout = a["output_tokens"].as_i64().unwrap_or(0);
                let tokens = if tin + tout == 0 {
                    "-".to_string()
                } else {
                    format!("{}/{}", human(tin), human(tout))
                };
                println!(
                    "{:<16} {:<14} {:<16} {:<11} {:<9} {:<9} {:<14} {:<13} {:>7} {:<5} {:>8} {}",
                    a["name"].as_str().unwrap_or("?"),
                    a["slot_id"].as_str().unwrap_or("-"),
                    // Which program is in the seat. Worth a column of its
                    // own now that a seat can be spawned with a different
                    // one: you cannot choose what to change without seeing
                    // what it is.
                    a["cli"].as_str().unwrap_or("-"),
                    a["role"].as_str().unwrap_or("-"),
                    a["host"].as_str().unwrap_or("-"),
                    a["provenance"].as_str().unwrap_or("-"),
                    if a["paused"].as_bool() == Some(true) {
                        format!("{} (paused)", a["task_state"].as_str().unwrap_or("-"))
                    } else {
                        a["task_state"].as_str().unwrap_or("-").to_string()
                    },
                    a["status"].as_str().unwrap_or("-"),
                    // The pid of a process that has ENDED is not a pid, it
                    // is a number somebody will go looking for on the wrong
                    // machine. A seat that merely lost touch keeps its pid,
                    // because that process is very often still sitting there.
                    if a["exited"].as_bool() == Some(true) {
                        "-".to_string()
                    } else {
                        a["pid"].as_i64().map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                    },
                    a["io_kind"].as_str().unwrap_or("-"),
                    tokens,
                    a["summary"].as_str().unwrap_or(""),
                );
            }
            println!(
                "\n  (TOKENS = input/output; STATE is the work, AGENT the process behind it;\n\
                 \x20  `waiting` = a CLI that takes this seat once per turn and is due back)"
            );
        }

        Cmd::Certs => {
            let v = get(&c, format!("{base}/v1/certs")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let rows = v["certificates"].as_array().unwrap_or(&empty);
            if rows.is_empty() {
                println!("no certificates issued yet");
                return Ok(());
            }
            println!("{:<22} {:<10} {:<9} SERIAL", "NAME", "KIND", "STATE");
            for x in rows {
                println!("{:<22} {:<10} {:<9} {}",
                    x["name"].as_str().unwrap_or("?"), x["kind"].as_str().unwrap_or("?"),
                    x["state"].as_str().unwrap_or("?"), x["serial"].as_str().unwrap_or("?"));
            }
        }

        Cmd::Revoke { name, reason } => {
            let mut body = json!({ "name": name });
            if let Some(r) = reason { body["reason"] = json!(r); }
            let v = post(&c, format!("{base}/v1/certs/revoke"), body).await?;
            bail_on_error(&v)?;
            println!("revoked {} certificate(s) and {} token(s) for {}",
                     v["certs"].as_i64().unwrap_or(0), v["tokens"].as_i64().unwrap_or(0),
                     v["name"].as_str().unwrap_or("?"));
            println!("{} on the revocation list; connection closed: {}; TLS reloaded: {}",
                     v["on_crl"].as_i64().unwrap_or(0),
                     v["kicked"].as_bool().unwrap_or(false),
                     v["tls_reloaded"].as_bool().unwrap_or(false));
        }

        Cmd::Pause { session, who, reason } => {
            let mut body = json!({ "paused": true });
            if let Some(w) = who { body["who"] = json!(w); }
            if let Some(r) = reason { body["reason"] = json!(r); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/pause"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("paused"));
        }

        Cmd::Resume { session, who } => {
            let mut body = json!({ "paused": false });
            if let Some(w) = who { body["who"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/pause"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("resumed"));
        }

        Cmd::Release { session, who, note, grace_ms } => {
            let mut body = json!({ "grace_ms": grace_ms });
            if let Some(w) = who { body["who"] = json!(w); }
            if let Some(n) = note { body["note"] = json!(n); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/release"), body).await?;
            bail_on_error(&v)?;
            println!("{}", v["detail"].as_str().unwrap_or("released"));
        }

        Cmd::Sync { session, op, workspace } => {
            let mut body = json!({ "op": op });
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/sync"), body).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            for ws in v["workspaces"].as_array().unwrap_or(&empty) {
                if let Some(e) = ws["error"].as_str() {
                    println!("{}: {e}", ws["workspace"].as_str().unwrap_or("?"));
                    continue;
                }
                println!("{} ({})", ws["workspace"].as_str().unwrap_or("?"),
                         ws["branch"].as_str().unwrap_or("?"));
                for r in ws["results"].as_array().unwrap_or(&empty) {
                    println!("  {:<12} {}{}", r["node"].as_str().unwrap_or("?"),
                             r["detail"].as_str().unwrap_or(""),
                             if r["conflict"].as_bool() == Some(true) { "   [CONFLICT]" } else { "" });
                }
            }
        }

        Cmd::Log { session, follow, limit } => {
            // Seed with the tail of the history, then stream forward by id so
            // nothing is missed or repeated between polls.
            let v = get(&c, format!("{base}/v1/sessions/{session}/log?limit={limit}")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let msgs = v["messages"].as_array().unwrap_or(&empty).clone();
            if msgs.is_empty() && !follow {
                println!("no messages yet in {session}");
                return Ok(());
            }
            println!("{:<8} {:<1} {:>12}    {:<12} MESSAGE", "TIME", "", "FROM", "TO");
            let mut last = 0i64;
            for m in &msgs {
                print_message(m);
                last = last.max(m["id"].as_i64().unwrap_or(0));
            }
            if !follow {
                println!(
                    "\n  ( ! = somebody it was for has not seen it yet, \
                     ~ = delivery in flight,\n    (team n/m) = how many of the \
                     seats it was for have seen it )"
                );
                return Ok(());
            }
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let v = get(&c, format!("{base}/v1/sessions/{session}/log?after={last}")).await?;
                for m in v["messages"].as_array().unwrap_or(&empty) {
                    print_message(m);
                    last = last.max(m["id"].as_i64().unwrap_or(0));
                }
            }
        }

        Cmd::Transcript { session, who, tail } => {
            let r = get(&c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
            bail_on_error(&r)?;
            let agent = r["agent_id"].as_str().context("no agent id returned")?;
            let name = r["name"].as_str().unwrap_or(&who);
            let v = get(&c, format!("{base}/v1/agents/{agent}/transcript?tail={tail}")).await?;
            bail_on_error(&v)?;
            let text = v["transcript"].as_str().unwrap_or("");
            if text.is_empty() {
                println!("no output recorded for {name} ({agent})");
            } else {
                println!("--- {name} ({agent}) ---");
                print!("{text}");
            }
        }

        Cmd::Attach { session, who, replay } => {
            let reason =
                attach(&base, &c, &session, &who, replay, args.ca_cert.as_deref(), identity)
                    .await?;
            println!("{reason}");
        }

        Cmd::Pair { pair_hub, code, connect, insecure } => {
            let p = nexmesh_proto::pair_client::pair(
                &pair_hub, &code, &args.identity_dir,
                connect.as_deref(), args.ca_cert.as_deref(), insecure,
            )
            .await?;
            println!("Paired as {} '{}'.", p.kind, p.name);
            println!("Identity written to {}", p.identity_file.display());
            println!("\nTry it:  nexmesh nodes");
            println!("Then give your CLIs the orchestrator:");
            println!("  nexmesh install-orchestrator --cli claude");
            return Ok(());
        }

        Cmd::InstallOrchestrator { cli_id, clis, name, orch, dry_run } => {
            let msg = install::install_orchestrator(install::OrchOpts {
                cli_id: &cli_id,
                clis_dir: &clis,
                server_name: &name,
                orch: orch.as_deref(),
                hub: &base,
                token: args.token.as_deref(),
                ca_cert: args.ca_cert.as_deref(),
                dry_run,
            })?;
            println!("{msg}");
            if !dry_run {
                println!("Restart {cli_id} to pick it up, then ask it to list your nodes.");
                if args.token.is_none() {
                    println!(
                        "NOTE: no operator token was passed, so the orchestrator will only work \
                         against a hub that has none. Re-run with --token once you mint one."
                    );
                }
            }
        }

        Cmd::AddWorkspace { session, path, node, name, kind, repo } => {
            let name = name.unwrap_or_else(|| {
                path.trim_end_matches('/').rsplit('/').next().unwrap_or("work").to_string()
            });
            let mut body = json!({ "name": name, "kind": kind, "node": node, "path": path });
            if let Some(r) = repo { body["repo_url"] = json!(r); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/workspaces"), body).await?;
            bail_on_error(&v)?;
            println!(
                "mounted {} on {} as workspace \"{}\" of {}",
                path, node, name, v["session_id"].as_str().unwrap_or(&session)
            );
        }

        Cmd::InstallSeat { cli_id, clis, name, role, seat_name, sock, mcpd, dry_run } => {
            let msg = install::install_seat(install::SeatOpts {
                cli_id: &cli_id,
                clis_dir: &clis,
                server_name: &name,
                mcpd: mcpd.as_deref(),
                sock: &sock,
                role: role.as_deref(),
                name: seat_name.as_deref(),
                dry_run,
            })?;
            println!("{msg}");
            if !dry_run {
                println!(
                    "Every NEW conversation in {cli_id} on this machine now takes a seat; \
                     ones already open pick it up when they restart."
                );
                // Deliberately NOT a --session flag. This config is shared by
                // every conversation on the machine, and naming a session here
                // is an explicit answer that beats the seat -- so it would drag
                // a moved conversation back to one team on every restart.
                // Placing a conversation is per-conversation work: move it once.
                println!(
                    "Place each one with `nexmesh move-agent <seat> <session>`; a conversation \
                     returns to the session it was moved to on every restart."
                );
            }
        }

        Cmd::Send { session, text, to } => {
            let mut body = json!({ "text": text });
            if let Some(t) = to { body["to"] = json!(t); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/messages"), body).await?;
            bail_on_error(&v)?;
            // Sent is not heard. Who it is for, and by what route it reaches
            // them -- because a CLI started by hand in a plain terminal
            // cannot be interrupted, and will see this when it next calls a
            // tool. A question asked into a room of those reads as ignored,
            // and there was no way to tell from here.
            let empty = vec![];
            let seats = v["seats"].as_array().unwrap_or(&empty);
            if seats.is_empty() {
                println!("sent -- but no seat in this session can see it yet");
            } else {
                println!("sent, queued for {} seat(s):", seats.len());
                for s in seats {
                    let how = match s["route"].as_str().unwrap_or("") {
                        "driven" => "in seconds -- the hub drives it",
                        "terminal" => "in seconds -- the hub types at its terminal",
                        "polls" => "in seconds -- it polls the hub",
                        _ => "when it next calls a tool -- nothing can interrupt it",
                    };
                    println!(
                        "  {:<16} {:<13} {}",
                        s["name"].as_str().unwrap_or("?"),
                        s["status"].as_str().unwrap_or("-"),
                        how
                    );
                }
            }
        }
    }
    Ok(())
}
