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

mod camera;
mod install;
mod replies;
mod runcmd;
mod seatcmd;
mod tlstrust;
mod tui;
mod voice;
mod wire;
mod windowtitle;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "nexmesh",
    version,
    about = "nexmesh fleet control \u{2014} agent CLIs as one team, across machines",
    // clap sorts subcommands alphabetically, which puts `budget` above
    // `health` and tells a newcomer nothing. The grouped list below is the
    // same commands in the order somebody actually meets them.
    after_help = "\
COMMANDS, BY WHAT YOU ARE DOING

  looking        health  nodes  sessions  status  log  transcript  certs
  teams          new-session  rename-session  remove-session  budget
                 add-workspace  sync
  seats          add-agent  spawn  run  move-agent  rename-agent  set-role
                 remove-agent  release  pause  resume
  terminals      attach  seat  adopt
  talking        send
  setup          pair  install-seat  install-orchestrator  revoke

EXAMPLES

  nexmesh                                  the operator console: the whole fleet,
                                           one screen, j/k and Enter
  nexmesh run claude                       start a CLI here, on a terminal the
                                           fleet owns, attachable at once
  nexmesh install-seat --cli claude --role engineer
                                           make the CLI you already use a member
                                           of the team
  nexmesh sessions                         which teams exist
  nexmesh status payments                  who is in one, and what they are doing
  nexmesh attach payments Dev              that agent's real terminal, from here
  nexmesh send payments 'staging is back'  say something to the team
  nexmesh log payments                     what they said to each other

Every command takes --hub, and --token where the hub wants one; `nexmesh pair`
writes both into ~/.nexmesh so you do not have to pass them again.

  nexmesh help <command>                   the long version, with examples
  man nexmesh                              every argument, at the terminal"
)]
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
    #[command(after_help = "\
EXAMPLE
  nexmesh health                     # is the hub up, and which version
  nexmesh --hub https://hub.example.net health")]
    /// Hub health and version.
    Health,
    #[command(after_help = "\
EXAMPLE
  nexmesh nodes                      # which machines are connected
  nexmesh nodes | grep claude        # and which of them can run claude

A machine with no CLIs listed has a node running but nothing installed for
it to launch.")]
    /// Nodes, their CLIs and whether they are connected.
    Nodes,
    #[command(after_help = "\
EXAMPLE
  nexmesh sessions                   # every team, with its seat count

SEATS counts teammates; TERMINALS counts adopted panes, which are screens
rather than team members.")]
    /// List sessions.
    Sessions,
    /// Move a seat, and whoever is in it, to another session. 
    ///
    /// No restart: the agent's next tool call lands in the new session. Its
    /// workspace, file locks and ownership zones are released, since all three
    /// describe the session being left.
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
    ///
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
    /// Rename an agent. 
    ///
    /// The name is how every command, message and log line addresses it
    #[command(after_help = "\
EXAMPLES
  nexmesh rename-agent sl-1a2b3c4d Reviewer
  nexmesh rename-agent Claude-machine-c-3 Reviewer --session payments

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
    /// Put a file in a session's workspace, where its agents can read it.
    #[command(after_help = "\
EXAMPLES
  nexmesh upload payments ./screenshot.png
  nexmesh upload payments ./spec.pdf --say 'the API we have to match'
  nexmesh upload payments ./clip.mp4 --node machine-a --dir media

The file lands in the workspace on a machine that holds it, and the path is
said in the team's chat -- because an agent reads files, and a file nobody
has been told about is a file nobody opens.

Up to 64MB in one go. Anything larger belongs in the workspace by other
means, with `nexmesh send` to say where it is.")]
    Upload {
        /// Session, by name or id.
        #[arg(value_name = "SESSION")]
        session: String,
        /// The file to put there.
        #[arg(value_name = "FILE")]
        file: String,
        /// A line for the team's chat, instead of the plain one.
        #[arg(long, value_name = "TEXT")]
        say: Option<String>,
        /// Which machine's copy to write to. Any that holds it, by default.
        #[arg(long, value_name = "NODE")]
        node: Option<String>,
        /// The agent it is for. The file lands on THAT agent's machine, in
        /// the workspace it is working in, and the message is addressed to
        /// it -- which is what you want when you are attached to something
        /// on another machine.
        #[arg(long, value_name = "AGENT")]
        to: Option<String>,
        /// Which workspace, when the session has more than one.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Directory inside the workspace. `uploads` by default.
        #[arg(long, value_name = "DIR")]
        dir: Option<String>,
    },
    /// Take a photo or a clip with this machine's camera, for an agent to look at.
    #[command(after_help = "\
EXAMPLES
  nexmesh camera payments                         # a photo, into the workspace
  nexmesh camera payments --to Max --say 'this screen'
  nexmesh camera payments --seconds 8             # a clip instead

\"Look at this\" is most of what a camera is for on a fleet: a screen, a
whiteboard, a machine with a light on it. The picture is taken HERE, and
lands on the machine the agent is running on, in its workspace.

Needs ffmpeg and a /dev/video* on this machine. The dashboard and the
installed app have the same thing as a button.")]
    Camera {
        /// Session, by name or id.
        #[arg(value_name = "SESSION")]
        session: String,
        /// Record this many seconds instead of taking a still.
        #[arg(long, value_name = "N")]
        seconds: Option<u32>,
        /// The agent it is for: it lands on that agent's machine.
        #[arg(long, value_name = "AGENT")]
        to: Option<String>,
        /// A line for the team's chat.
        #[arg(long, value_name = "TEXT")]
        say: Option<String>,
    },
    /// Say which parts of the workspace a seat is responsible for.
    #[command(after_help = "\
EXAMPLES
  nexmesh set-owns sl-1a2b3c4d 'verti/**' ombia      # these, and nothing else
  nexmesh set-owns Max --session payments --add digesta
  nexmesh set-owns Max --session payments --none     # owns nothing again

A zone is a pattern over the workspace: a plain name like `ombia` covers
everything under it, `verti/**` is a glob, and `src/auth/*` stops at one
level.

Teammates READ these: list_peers shows what each one owns, who_owns answers
for a single path, and acquire_file says whose area a file is in. That is
what makes four agents in one checkout hand work to each other instead of
editing over each other.")]
    SetOwns {
        /// Seat id, or a name with --session.
        #[arg(value_name = "SEAT|NAME")]
        who: String,
        /// Patterns. Omit them with --none to clear the list.
        #[arg(value_name = "PATTERN")]
        patterns: Vec<String>,
        /// Session to look the name up in. Needed when WHO is a name.
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
        /// Add to what it already owns instead of replacing the list.
        #[arg(long)]
        add: bool,
        /// Clear every zone this seat has.
        #[arg(long, conflicts_with = "add")]
        none: bool,
    },
    /// Change what a seat is for.
    ///
    /// The role decides who gets asked for what, and who may approve whose
    /// work.
    #[command(after_help = "\
EXAMPLES
  nexmesh set-role payments Reviewer reviewer
  nexmesh set-role sl-1a2b3c4d architect --description \"you decide the shape\"

A role is a free-form string, and there is no fixed list. It is an ADDRESS --
--to reviewer reaches whoever holds that role -- it goes into the agent's
first prompt, and one family changes behaviour: a role containing review, qa
or test approves its own work, because nobody reviews the reviewer. Everyone
else waits for a reviewer.

Common ones, by convention only: architect, engineer, code reviewer, qa,
analyst, archivist, doc writer. What a seat CAN DO (ocr, tts, strong, vision)
is a capability, which is a separate list and what ask_capability routes on.")]
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
    #[command(after_help = "\
EXAMPLE
  nexmesh rename-session se-1a2b3c4d payments
  nexmesh rename-session old-name payments

Nothing else changes: the same seats, the same agents, the same history.")]
    /// Rename a session. 
    ///
    /// The name is what you read on the dashboard and in every message, so it
    /// should be allowed to change.
    RenameSession {
        session: String,
        name: String,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh new-session payments
  nexmesh new-session payments --node machine-a --path /srv/app --workspace app

The workspace is how an attached CLI finds the team it belongs to: a
conversation started in that directory joins this session by default.")]
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
    #[command(after_help = "\
EXAMPLES
  nexmesh budget payments 2000000        # two million tokens, then it pauses
  nexmesh budget payments --clear        # no limit

Changing it re-arms the guard, so a team paused on budget can be given more
and carry on.")]
    /// Set, raise or clear a session's token budget. 
    ///
    /// Changing it re-arms the guard, so a session paused on budget can be
    /// given more and resumed.
    Budget {
        session: String,
        /// New ceiling in total tokens. Omit to remove the ceiling.
        #[arg(long)]
        tokens: Option<i64>,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh add-agent payments --name Reviewer --role reviewer --cli opencode
  nexmesh add-agent payments --name Dev --cli claude --node machine-a \
      --task 'build the login flow' --owns 'src/auth/**'

This makes the SEAT. `nexmesh spawn` starts a process in it -- or pass
--spawn to do both.")]
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
    /// Adopt a running tmux session, so it can be driven from anywhere. 
    ///
    /// Your own terminal stays attached.
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
    #[command(after_help = "\
EXAMPLES
  nexmesh seat payments Dev              # a tmux pane, with Dev's conversation in it
  nexmesh seat payments Dev --no-attach  # make it, leave it running, do not attach

For a seat whose CLI you started by hand: it gives that conversation a
keyboard the fleet owns, so it can be attached to from anywhere.")]
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
    #[command(after_help = "\
EXAMPLE
  nexmesh remove-session payments

Refuses while anybody is still sitting in it. Release or remove the seats
first:  nexmesh status payments")]
    /// Remove a session and everything that belonged to it. 
    ///
    /// Refuses while anybody is still sitting in it.
    RemoveSession {
        #[arg(value_name = "SESSION")]
        session: String,
        /// Remove it even though agents are still live in it.
        #[arg(long)]
        force: bool,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh status payments                # every seat, what it is doing
  nexmesh status payments --watch        # the same, refreshed

STATE is the work; AGENT is the process behind the seat. `waiting` means a
CLI that takes its seat once per turn and is due back -- not a problem.")]
    /// Live status of every agent in a session.
    Status {
        session: String,
        /// Also list the adopted terminals other seats are sitting in. They
        /// are plumbing, not teammates, so they are hidden by default.
        #[arg(long)]
        terminals: bool,
    },
    #[command(after_help = "\
EXAMPLE
  nexmesh certs                      # what the fleet CA has issued, and to whom

A certificate listed as revoked stops working at the TLS handshake, not at
the API.")]
    /// List the certificates the fleet CA has issued.
    Certs,
    #[command(after_help = "\
EXAMPLE
  nexmesh revoke machine-d

Takes effect at the handshake: its certificate stops working, its token is
revoked, and any connection it is holding is closed.")]
    /// Revoke a machine or operator: its certificate stops working at the TLS
    /// handshake, its token is revoked, and any connection it holds is closed.
    Revoke {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh pause payments                     # the whole team
  nexmesh pause payments --who Dev --reason 'waiting on the API key'

A turn already running finishes. This means \"take on nothing further\", not
\"stop this instant\".")]
    /// Stop sending work to an agent, or to the whole team. 
    ///
    /// Reversible; a turn already running finishes.
    Pause {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh resume payments
  nexmesh resume payments --who Dev

Anything queued while they were paused is delivered.")]
    /// Start sending work again.
    Resume {
        session: String,
        /// Agent name or role; omitted means everyone.
        who: Option<String>,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh release payments Dev       # accept Dev's work; it may stop
  nexmesh release payments           # the whole team

A released agent does not need to wait for review any more.")]
    /// Accept an agent's work and let it stop. 
    ///
    /// Omit the name to release the whole team.
    Release {
        session: String,
        who: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// How long to let it finish before terminating it.
        #[arg(long, default_value_t = 15000)]
        grace_ms: u64,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh sync payments app --status     # who holds what, and whether it matches
  nexmesh sync payments app --publish machine-a
  nexmesh sync payments app --pull machine-b

For a workspace several machines hold a copy of.")]
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
    #[command(after_help = "\
EXAMPLES
  nexmesh log payments               # what the agents said to each other
  nexmesh log payments --limit 200

A line marked ! has not reached its recipient yet; ~ is a delivery in
flight. For one agent's own output, use `nexmesh transcript`.")]
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
    #[command(after_help = "\
EXAMPLES
  nexmesh transcript payments Dev        # what that process actually printed
  nexmesh transcript payments Dev --tail 200

The conversation between agents is `nexmesh log`; this is one agent's own
output, as its terminal saw it.")]
    /// Read what one agent's process actually printed.
    Transcript {
        session: String,
        /// Agent name, role, slot id or agent id.
        who: String,
        /// Bytes from the end.
        #[arg(long, default_value_t = 65536)]
        tail: usize,
    },
    /// Start an agent CLI here, on a terminal the fleet owns -- so it is
    /// attachable from anywhere, from the first keystroke.
    ///
    /// Type `nexmesh run claude` where you would have typed `claude`. It joins
    /// the team that owns this directory, takes the seat it had last time, and
    /// puts the CLI on your screen. Closing the window does not end it.
    #[command(after_help = "\
EXAMPLES
  nexmesh run claude                 # here, in this directory
  nexmesh run codex --name Reviewer  # a seat by that name
  nexmesh run claude --dry-run       # say what it would do

Behind a shell function the difference disappears:

  claude() { nexmesh run claude; }

A CLI you start by hand is a full teammate -- it registers, takes a seat,
sends and receives -- but it cannot be attached to: the terminal behind it
belongs to the window you typed in, and nothing outside that process can
read or write it. This makes the terminal on the other side of the fence.")]
    Run {
        /// Which CLI: claude, codex, opencode, qwen -- whatever `nexmesh nodes`
        /// lists for this machine.
        #[arg(value_name = "CLI")]
        cli: String,
        /// Join this session instead of the one that owns this directory.
        #[arg(long)]
        session: Option<String>,
        /// Name the seat. Default: the CLI and this machine, like Claude-machine-b.
        #[arg(long)]
        name: Option<String>,
        /// The machine to start it on. Default: this one.
        #[arg(long)]
        node: Option<String>,
        /// What the seat is for, if the session's default is not it.
        #[arg(long)]
        role: Option<String>,
        /// Work here instead of the current directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Start it and say where it is, without taking over this terminal.
        #[arg(long)]
        no_attach: bool,
        /// Print what it would do and stop.
        #[arg(long)]
        dry_run: bool,
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
        ///
        /// The node keeps 256 KiB per agent, which is what this asks for:
        /// coming back to a terminal and finding one screenful of context,
        /// with everything above it gone, is the thing people complain
        /// about. Lower it on a slow link.
        #[arg(long, default_value_t = 262144)]
        replay: usize,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh pair 7Q2K-9FXD                 # the code from the dashboard
  nexmesh pair 7Q2K-9FXD --hub https://hub.example.net

Your key is generated here and never leaves this machine. What arrives is a
certificate and the fleet's CA, written to ~/.nexmesh.")]
    /// Join a fleet as an operator, using a one-time code from the dashboard. 
    ///
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
    #[command(after_help = "\
EXAMPLES
  nexmesh install-orchestrator --cli claude --token <operator-token>
  nexmesh install-orchestrator --cli opencode --dry-run

Gives that CLI the tools to drive the fleet: create teams, add agents, move
them about. Scope the token with --session to limit it to one team.")]
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
    #[command(after_help = "\
EXAMPLE
  nexmesh add-workspace payments app --node machine-a --path /srv/app

The directory is how an attached CLI finds the session it belongs to, so a
conversation started in /srv/app joins `payments` by default.")]
    /// Mount a workspace into a session that already exists. 
    ///
    /// The directory is how an attached CLI finds the session it belongs to.
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
    #[command(after_help = "\
EXAMPLES
  nexmesh install-seat --cli claude --role engineer
  nexmesh install-seat --cli codex --seat-name Dev --dry-run

Every conversation you start in that CLI then takes its own seat, keyed on
the CLI's own conversation id -- so it survives a restart or a /resume.")]
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
    #[command(after_help = "\
EXAMPLES
  nexmesh send payments 'the staging database is back'
  nexmesh send payments --to Dev 'rebase onto main first'

You appear as the operator. Agents answer by addressing 'operator', and the
same chat is what the dashboard and the console show.")]
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

/// base64, for the one thing here that carries bytes over HTTP.
fn b64(data: &[u8]) -> String {
    const B: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `~/shot.png` is a path people type, and only a shell expands it.
fn shellexpand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Put a file from this machine into the session's workspace, on the machine
/// the agent is running on, and say so in the chat.
///
/// The same endpoint the dashboard's paperclip uses. Which machine it lands
/// on is decided by WHO it is for: a file sent to something you are attached
/// to belongs where that something can open it.
async fn send_file(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    path: &std::path::Path,
    to: Option<&str>,
    say: Option<&str>,
) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > 64 * 1024 * 1024 {
        anyhow::bail!(
            "{} is {:.1}MB, over the 64MB limit for one upload",
            path.display(),
            bytes.len() as f64 / 1_048_576.0
        );
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "upload".into());
    let mut body = json!({ "name": name, "data_base64": b64(&bytes) });
    if let Some(t) = to {
        body["to"] = json!(t);
    }
    if let Some(s) = say {
        body["say"] = json!(s);
    }
    let v = post(c, format!("{base}/v1/sessions/{session}/upload"), body).await?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    Ok(format!(
        "{name} is at {} on {}",
        v["path"].as_str().unwrap_or("?"),
        v["node"].as_str().unwrap_or("?")
    ))
}

/// "2m", "3h", "4d" -- how long ago something was, for a listing.
fn ago(now: i64, then: i64) -> String {
    let d = ((now - then) / 1000).max(0);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86_400 {
        format!("{}h", d / 3600)
    } else {
        format!("{}d", d / 86_400)
    }
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
/// The voice keys, from `~/.nexmesh/console.toml`.
///
/// Defaults: Ctrl-^ dictates, Ctrl-_ reads a reply back, Ctrl-\ reads every
/// new one as it arrives. All three are control characters because an attach
/// forwards every printable byte to the agent, and all three are
/// configurable because Ctrl-^ is Ctrl+Shift+6 on a US layout and awkward or
/// impossible on others.
fn voice_keys() -> voice::Keys {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&home).join(".nexmesh").join("console.toml");
    voice::keys_from(&std::fs::read_to_string(path).unwrap_or_default())
}

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
    /// Who this is: the seat, and where it is running.
    who: String,
    host: String,
    session: String,
    /// The adopted pane a seat's CLI is sitting in, when it is reached
    /// through one.
    via: String,
    /// Whether the bytes are going straight to the node.
    direct: bool,
    /// The keys, in the order they are given up when the width runs out.
    keys: Vec<(String, String)>,
    /// Passing state: `listening`, `aloud`, `reading 2 back`.
    marks: Vec<String>,
    /// Command mode: the keys below are letters, and nothing reaches the
    /// agent until it is left again.
    command: bool,
    /// What those letters are, for the row to say.
    menu: Vec<(String, String)>,
    /// A question being typed on the row itself: (what is being asked, what
    /// has been typed). An attach has no other line to ask on, and the
    /// agent must not be shown the question.
    ask: Option<(String, String)>,
    cols: u16,
    rows: u16,
}

impl StatusBar {
    /// The bar is skipped on a screen too short to spare a row for it.
    fn new(who: String, host: String, session: String, via: String, cols: u16, rows: u16)
        -> Option<Self>
    {
        (rows >= 4).then_some(StatusBar {
            who,
            host,
            session,
            via,
            direct: false,
            keys: Vec::new(),
            marks: Vec::new(),
            command: false,
            menu: Vec::new(),
            ask: None,
            cols,
            rows,
        })
    }

    fn set_mark(&mut self, name: &str, on: bool) {
        self.marks.retain(|m| !m.starts_with(name));
        if on {
            self.marks.push(name.to_string());
        }
    }

    /// A mark with something after it -- "reading 2 back" -- replacing any
    /// earlier one with the same first word.
    fn say_mark(&mut self, name: &str, text: String) {
        self.marks.retain(|m| !m.starts_with(name));
        self.marks.push(text);
    }

    /// The line, fitted to the width it has.
    ///
    /// The way out comes FIRST, because an attach is a takeover of somebody
    /// else's terminal and "how do I get out of this" is the question this
    /// row exists to answer. Then who you are talking to, in the same
    /// `session:agent` form the window title uses -- one way of naming a
    /// seat, everywhere. Everything after that gives way as the terminal
    /// narrows: the machine and the pane first, then the key labels, then
    /// the other keys, least useful first.
    fn line(&self) -> String {
        let w = self.cols as usize;
        let sep = "  \u{00b7}  ";
        // A question owns the row while it is up.
        if let Some((what, typed)) = &self.ask {
            let line = format!(" {what}: {typed}\u{2588}{sep}Enter sends \u{b7} Esc cancels");
            if line.chars().count() <= w {
                return line;
            }
            let bare = format!(" {what}: {typed}\u{2588}");
            return bare.chars().skip(bare.chars().count().saturating_sub(w)).collect();
        }
        // In command mode the row IS the menu: what the letters do, and how
        // to get out. Nothing else on it matters while it is up.
        if self.command {
            let id = if self.session.is_empty() {
                self.who.clone()
            } else {
                format!("{}:{}", self.session, self.who)
            };
            let full: Vec<String> =
                self.menu.iter().map(|(k, what)| format!("{k} {what}")).collect();
            let short: Vec<String> = self.menu.iter().map(|(k, _)| k.clone()).collect();
            let marks: String = if self.marks.is_empty() {
                String::new()
            } else {
                format!("{sep}{}", self.marks.join(sep))
            };
            for body in [full.join(sep), short.join(" ")] {
                let line = format!(" COMMANDS ON{sep}{body}{marks}{sep}{id}");
                if line.chars().count() <= w {
                    return line;
                }
                let line = format!(" COMMANDS{sep}{body}");
                if line.chars().count() <= w {
                    return line;
                }
            }
            return format!(" COMMANDS{sep}{}", short.join(" ")).chars().take(w).collect();
        }
        let id = if self.session.is_empty() {
            self.who.clone()
        } else {
            format!("{}:{}", self.session, self.who)
        };
        // The last key is the way out; the rest are optional.
        let (out, rest) = match self.keys.split_last() {
            Some(((k, what), rest)) => ((k.clone(), what.clone()), rest.to_vec()),
            None => (("Ctrl-]".to_string(), "detach".to_string()), Vec::new()),
        };
        let marks: String = if self.marks.is_empty() {
            String::new()
        } else {
            format!("{sep}{}", self.marks.join(sep))
        };
        // `keep` counts from the END of the optional keys, because the ones
        // nearest the way out are the ones worth keeping.
        let voice = |short: bool, keep: usize| -> String {
            let picked: Vec<String> = rest
                .iter()
                .skip(rest.len().saturating_sub(keep))
                .map(|(k, what)| if short { k.clone() } else { format!("{k} {what}") })
                .collect();
            if picked.is_empty() {
                String::new()
            } else {
                format!("{sep}{}", picked.join(sep))
            }
        };

        let n = rest.len();
        let mut tries: Vec<(bool, usize)> = vec![(false, n), (true, n)];
        for keep in (0..n).rev() {
            tries.push((true, keep));
        }
        let mut shortest = String::new();
        for (short_keys, keep) in tries {
            for drop_level in 0..3 {
                let mut line = format!(" {} {}{sep}{id}{marks}{}", out.0, out.1, voice(short_keys, keep));
                if drop_level < 2 && !self.host.is_empty() && self.host != "-" {
                    line.push_str(&format!("{sep}on {}", self.host));
                }
                if drop_level < 1 {
                    if self.direct {
                        line.push_str(&format!("{sep}direct"));
                    }
                    if !self.via.is_empty() {
                        line.push_str(&format!("{sep}via {}", self.via));
                    }
                }
                if line.chars().count() <= w {
                    return line;
                }
                shortest = line;
            }
        }
        // Narrower than the two things that cannot go: cut, rather than
        // wrap onto the agent's screen.
        let bare = format!(" {}{sep}{id}", out.0);
        if bare.chars().count() <= w {
            return bare;
        }
        if shortest.chars().count() > w {
            return shortest.chars().take(w).collect();
        }
        shortest
    }

    /// Reserve the row: the agent's screen is everything above it.
    fn agent_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }

    /// Save the cursor, paint the bar, put the cursor back.
    ///
    /// Black background, plain foreground: it is one row at the bottom of
    /// somebody else's screen and should read as a frame around it, not as a
    /// second thing competing for attention. (Reverse video did the latter.)
    fn paint(&self) -> Vec<u8> {
        let mut line = self.line();
        let w = self.cols as usize;
        if line.chars().count() > w {
            line = line.chars().take(w).collect();
        } else {
            let pad = w - line.chars().count();
            line.push_str(&" ".repeat(pad));
        }
        // Black and grey while the agent has the keyboard; bright and
        // coloured while this row does. The difference has to be visible
        // from the corner of an eye, because the whole point of a mode is
        // knowing which one you are in without trying a key.
        let colour = if self.ask.is_some() {
            // A question is a third state and looks like one.
            "\x1b[44m\x1b[97m\x1b[1m"
        } else if self.command {
            "\x1b[45m\x1b[97m\x1b[1m"
        } else {
            "\x1b[40m\x1b[37m"
        };
        format!("\x1b7\x1b[{};1H{}{}\x1b[0m\x1b8", self.rows, colour, line).into_bytes()
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

/// The session's NAME, whichever way it was addressed.
///
/// Every command takes a session id or a session name, and a title that read
/// `se-2d00687a:Nexmesh-dev` would be naming the plumbing at the one moment
/// the operator is trying to tell two windows apart.
async fn session_name(c: &reqwest::Client, base: &str, session: &str) -> String {
    if !session.starts_with("se-") {
        return session.to_string();
    }
    if let Ok(v) = get(c, format!("{base}/v1/sessions")).await {
        if let Some(rows) = v["sessions"].as_array() {
            for s in rows {
                if s["id"].as_str() == Some(session) {
                    if let Some(n) = s["name"].as_str() {
                        return n.to_string();
                    }
                }
            }
        }
    }
    session.to_string()
}

/// A one-time ticket for attaching straight to the node holding a terminal.
///
/// The hub decides; this is it saying yes, and telling the node so over the
/// connection it already holds. See `proto::attachsock`.
async fn get_ticket(c: &reqwest::Client, base: &str, agent: &str) -> Result<String> {
    let v = post(c, format!("{base}/v1/attach/ticket"), json!({ "agent": agent })).await?;
    bail_on_error(&v)?;
    v["ticket"]
        .as_str()
        .map(String::from)
        .context("the hub did not return a ticket")
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
    use tokio::io::AsyncWriteExt;

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
    // `session:agent`, the way the title and every other command say it.
    let session_label = session_name(c, base, session).await;
    // A seat whose CLI sits in an adopted terminal is reached THROUGH that
    // terminal: the hub says so rather than refusing, and the bar says so
    // rather than pretending the pty is the agent's own.
    let via = r["via"].as_str().unwrap_or_default().to_string();
    let vkeys = voice_keys();
    let mut bar = StatusBar::new(
        name.to_string(),
        host.to_string(),
        session_label.clone(),
        via,
        cols,
        rows,
    );
    // The agent's screen is what is left after the bar takes its row, so it
    // is the size the hub is told. Without this the bar is painted over by
    // anything that fills the screen.
    let agent_rows = bar.as_ref().map(|b| b.agent_rows()).unwrap_or(rows);
    // The data path: straight to the node when the agent is on this machine
    // and that node is listening for a direct attach, and through the hub
    // when it is not. The hub authorises either way -- a direct attach needs
    // a ticket it minted, which it pushes to the node itself.
    let mut wire = None;
    if let Some(socket) = wire::local_socket_for(host, &runcmd::this_node(None)) {
        match get_ticket(c, base, &agent).await {
            Ok(ticket) => {
                match wire::open_local(&socket, &ticket, &agent, cols, agent_rows, replay).await {
                    Ok(w) => wire = Some(w),
                    // Not fatal: the hub can carry it, and saying why once is
                    // worth more than failing an attach that has a route.
                    Err(e) => eprintln!("(direct attach unavailable: {e:#}; going through the hub)"),
                }
            }
            Err(e) => eprintln!("(no direct ticket: {e:#}; going through the hub)"),
        }
    }
    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!(
        "{ws_base}/v1/attach/ws?agent={agent}&cols={cols}&rows={agent_rows}&replay={replay}"
    );
    let mut wire = match wire {
        Some(w) => w,
        None => wire::open_hub(&url, token().cloned(), ca_cert, identity, &name).await?,
    };
    // Said out loud, because "why is this slow" should be answerable by
    // looking at the screen.
    // Two keys, and what is behind one of them. Everything else on the
    // keyboard belongs to the agent: a viewer that quietly eats Ctrl-R or
    // Ctrl-W breaks somebody's editor, and finding out which three it took
    // is not a thing anybody should have to do.
    if let Some(b) = &mut bar {
        let a = voice::detect();
        b.keys.push((voice::key_name(vkeys.command), "commands".into()));
        b.keys.push((voice::key_name(vkeys.detach), "detach".into()));
        if a.can_hear() {
            b.menu.push((voice::key_name(vkeys.dictate), "say it".into()));
        }
        if a.can_speak() {
            b.menu.push((voice::key_name(vkeys.read), "read".into()));
            b.menu.push((voice::key_name(vkeys.aloud), "aloud".into()));
        }
        b.menu.push((voice::key_name(vkeys.send), "send a file".into()));
        if camera::detect().ready() {
            b.menu.push((voice::key_name(vkeys.photo), "photo".into()));
        }
        b.menu.push((voice::key_name(vkeys.command), "off".into()));
        b.direct = wire.direct;
    }


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

    // And the window says which seat it is showing, for as long as we have
    // it. What was there before goes on the terminal's title stack.
    {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(windowtitle::PUSH).await;
        let _ = out.write_all(&windowtitle::set(&windowtitle::of(&session_label, &name))).await;
        let _ = out.flush().await;
    }

    let (quit_tx, mut quit_rx) = tokio::sync::mpsc::channel::<String>(4);

    // What the agent has said, split into replies -- everything printed
    // since the operator last pressed Enter is its answer to that. Shared
    // with the key loop, which is where one of them is chosen to be read.
    let heard = std::sync::Arc::new(std::sync::Mutex::new(replies::Replies::default()));
    // When the last byte arrived, so "it has stopped talking" can be noticed
    // without polling the process.
    let quiet = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));

    // The agent's terminal -> ours, whichever way the bytes came.
    let reader = {
        let quit_tx = quit_tx.clone();
        let heard = heard.clone();
        let quiet = quiet.clone();
        let mut rx = std::mem::replace(&mut wire.rx, tokio::sync::mpsc::channel(1).1);
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            // The agent sets the title to something about itself; this window
            // is saying which seat it shows, so those writes come out here
            // rather than being fought over.
            let mut strip = windowtitle::Strip::default();
            while let Some(msg) = rx.recv().await {
                match msg {
                    wire::In::Data(data) => {
                        let data = strip.pass(&data);
                        // A copy of what went past, as words, so a reply can
                        // be read back or read out as it arrives.
                        if let Ok(mut seen) = heard.lock() {
                            seen.saw(&voice::plain(&data));
                        }
                        if let Ok(mut q) = quiet.lock() {
                            *q = std::time::Instant::now();
                        }
                        if out.write_all(&data).await.is_err() || out.flush().await.is_err() {
                            break;
                        }
                    }
                    wire::In::Opened { read_only } => {
                        if read_only {
                            let _ = out
                                .write_all(b"\r\n[nexmesh] headless agent: view-only.\r\n")
                                .await;
                            let _ = out.flush().await;
                        }
                    }
                    wire::In::Ended(why) => {
                        let _ = quit_tx.send(why).await;
                        break;
                    }
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
    // Only consulted while reading aloud; cheap enough to leave running.
    let mut settle = tokio::time::interval(Duration::from_millis(400));
    settle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Speaking and listening, if this machine has anything to do it with and
    // the fleet has a teammate that can. An attached terminal is a place you
    // type, so it is a place you should be able to talk to -- on a phone or a
    // tablet it is the only comfortable way to put a sentence in a prompt.
    let audio = voice::detect();
    let mut rec: Option<(std::process::Child, std::path::PathBuf)> = None;
    // The player, while it plays: the same key again stops it.
    let playing: voice::Playing = Default::default();
    // Reading every new reply out as it settles, rather than on request.
    let mut aloud = false;
    // Command mode: one key in, one key out, and three keys that are ours
    // while it is on.
    let mut command = false;
    // A question being typed on the bar -- the path of a file to send. While
    // it is up, keystrokes go into it rather than to the agent.
    let mut asking: Option<String> = None;
    // Whether the last thing pressed was the read key, so pressing it again
    // means "the one before that" rather than "that one again".
    let mut spoke_once = false;
    let (said_tx, mut said_rx) = tokio::sync::mpsc::channel::<Result<String>>(4);
    let scratch = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("nexmesh-voice");

    let reason = loop {
        tokio::select! {
            _ = repaint.tick(), if bar.is_some() => {
                if let Some(b) = &bar {
                    let mut out = tokio::io::stdout();
                    let _ = out.write_all(&b.paint()).await;
                    let _ = out.flush().await;
                }
            }
            // Reading every reply as it arrives means noticing when one has
            // ENDED. An agent prints in bursts, so the boundary is silence:
            // a second and a bit with nothing new, which is long enough that
            // a paragraph is read as one and short enough to feel immediate.
            _ = settle.tick(), if aloud => {
                let idle = quiet.lock().map(|q| q.elapsed()).unwrap_or_default();
                let talking = playing.lock().map(|p| p.is_some()).unwrap_or(false);
                if idle >= std::time::Duration::from_millis(1200) && !talking {
                    let text = heard.lock().ok().and_then(|mut h| h.take_settled());
                    if let Some(text) = text {
                        let (bs, cl, tx, a, dir, pl) = (
                            base.to_string(), c.clone(), said_tx.clone(),
                            audio.clone(), scratch.clone(), playing.clone(),
                        );
                        tokio::spawn(async move {
                            if let Err(e) = voice::speak(&bs, &cl, &a, &text, &dir, &pl).await {
                                let _ = tx.send(Err(e)).await;
                            }
                        });
                    }
                }
            }
            // Words that came back from the fleet, typed into the agent's
            // prompt. Never submitted: Enter stays the operator's.
            Some(words) = said_rx.recv() => {
                match words {
                    // A sent file reports itself; it is not dictation, and
                    // must not be typed into the agent's prompt.
                    Ok(text) if text.starts_with('\u{0001}') => {
                        if let Some(b) = &mut bar {
                            b.set_mark("camera", false);
                        }
                        let mut out = tokio::io::stdout();
                        let _ = out
                            .write_all(format!("\r\n[nexmesh] {}\r\n", &text[1..]).as_bytes())
                            .await;
                        let _ = out.flush().await;
                    }
                    Ok(text) if !text.trim().is_empty() => {
                        if wire.tx.send(wire::Out::Data(text.into_bytes())).await.is_err() {
                            break "connection lost".to_string();
                        }
                        if let Some(b) = &mut bar {
                            b.set_mark("listening", false);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // One line, on the agent's screen, since there is
                        // nowhere else to put it mid-attach.
                        let mut out = tokio::io::stdout();
                        let _ = out
                            .write_all(format!("\r\n[nexmesh] {}\r\n", format!("{e:#}").lines().next().unwrap_or("that did not work").to_string()).as_bytes())
                            .await;
                        let _ = out.flush().await;
                    }
                }
            }
            n = keys.read(&mut buf) => {
                let n = match n { Ok(0) | Err(_) => break "input ended".to_string(), Ok(n) => n };
                // Two keys are ours. Everything else on this keyboard
                // belongs to the agent -- which is why the rest of what this
                // viewer can do lives behind ONE of them rather than taking
                // a control character each.
                let mut forward: Vec<u8> = Vec::with_capacity(n);
                let mut leaving: Option<String> = None;
                // A question on the bar owns the keyboard until it is
                // answered or abandoned. Nothing typed into it reaches the
                // agent -- a path typed at somebody's prompt would run.
                if let Some(typed) = asking.as_mut() {
                    let mut done: Option<Option<String>> = None;
                    for &byte in &buf[..n] {
                        match byte {
                            b'\r' | b'\n' => {
                                done = Some(Some(typed.clone()));
                                break;
                            }
                            0x1b | 0x03 => {
                                done = Some(None);
                                break;
                            }
                            0x7f | 0x08 => {
                                typed.pop();
                            }
                            b if b >= 0x20 => typed.push(b as char),
                            _ => {}
                        }
                    }
                    if let Some(b) = &mut bar {
                        b.ask = asking.clone().map(|t| ("file to send".to_string(), t));
                    }
                    if let Some(answer) = done {
                        asking = None;
                        if let Some(b) = &mut bar {
                            b.ask = None;
                        }
                        if let Some(path) = answer.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
                            let path = std::path::PathBuf::from(shellexpand(&path));
                            let (bs, cl, tx, sess, who) = (
                                base.to_string(), c.clone(), said_tx.clone(),
                                session.to_string(), name.to_string(),
                            );
                            tokio::spawn(async move {
                                let r = send_file(&bs, &cl, &sess, &path, Some(&who), None).await;
                                let _ = tx.send(r.map(|said| format!("\u{0001}{said}"))).await;
                            });
                        }
                    }
                    continue;
                }
                for &byte in &buf[..n] {
                    if byte == vkeys.detach {
                        leaving = Some("detached".to_string());
                        break;
                    }
                    if byte == vkeys.command {
                        command = !command;
                        if let Some(b) = &mut bar {
                            b.command = command;
                        }
                        continue;
                    }
                    // In command mode THREE keys are nexmesh's and everything
                    // else -- text, Enter, arrows, every other control
                    // character -- still goes to the agent. So the mode can
                    // be left on while you carry on typing at the prompt.
                    let Some(act) = (if command { vkeys.act(byte) } else { None }) else {
                        forward.push(byte);
                        continue;
                    };
                    match act {
                        voice::Act::Dictate => {
                            rec = match rec.take() {
                                Some((child, path)) => {
                                    match voice::stop(child, &path) {
                                        Ok(bytes) => {
                                            let (b, cl, tx) =
                                                (base.to_string(), c.clone(), said_tx.clone());
                                            let lang = std::env::var("LANG").ok().and_then(|l| {
                                                l.split(['.', '_']).next()
                                                    .filter(|s| s.len() == 2)
                                                    .map(|s| s.to_string())
                                            });
                                            tokio::spawn(async move {
                                                let _ = tx
                                                    .send(voice::transcribe(
                                                        &b, &cl, &bytes, lang.as_deref(),
                                                    )
                                                    .await)
                                                    .await;
                                            });
                                        }
                                        Err(e) => {
                                            let _ = said_tx.send(Err(e)).await;
                                        }
                                    }
                                    if let Some(b) = &mut bar {
                                        b.set_mark("listening", false);
                                    }
                                    None
                                }
                                None => match voice::start(&audio, &scratch) {
                                    Ok(r) => {
                                        if let Some(b) = &mut bar {
                                            b.set_mark("listening", true);
                                        }
                                        Some(r)
                                    }
                                    Err(e) => {
                                        let _ = said_tx.send(Err(e)).await;
                                        None
                                    }
                                },
                            };
                        }
                        voice::Act::Read => {
                            // Talking already? The same key is a stop.
                            if playing.lock().map(|p| p.is_some()).unwrap_or(false) {
                                voice::hush(&playing);
                                continue;
                            }
                            let (text, back) = match heard.lock() {
                                Ok(mut h) => {
                                    if spoke_once {
                                        h.older();
                                    }
                                    (h.selected().map(|s| s.to_string()), h.back())
                                }
                                Err(_) => (None, 0),
                            };
                            spoke_once = true;
                            match text {
                                Some(text) if !text.trim().is_empty() => {
                                    if let Some(b) = &mut bar {
                                        if back == 0 {
                                            b.set_mark("reading", false);
                                        } else {
                                            b.say_mark("reading", format!("reading {back} back"));
                                        }
                                    }
                                    let (bs, cl, tx, a, dir, pl) = (
                                        base.to_string(), c.clone(), said_tx.clone(),
                                        audio.clone(), scratch.clone(), playing.clone(),
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) =
                                            voice::speak(&bs, &cl, &a, &text, &dir, &pl).await
                                        {
                                            let _ = tx.send(Err(e)).await;
                                        }
                                    });
                                }
                                _ => {
                                    let mut out = tokio::io::stdout();
                                    let _ = out
                                        .write_all(b"\r\n[nexmesh] nothing further back to read\r\n")
                                        .await;
                                    let _ = out.flush().await;
                                }
                            }
                        }
                        // A file from THIS machine to the one the agent is
                        // on, in the workspace it is working in. The path is
                        // typed on the bar: an attach has no other line to
                        // ask on, and the agent must not see the question.
                        voice::Act::Send => {
                            if asking.is_none() {
                                asking = Some(String::new());
                                if let Some(b) = &mut bar {
                                    b.ask = Some(("file to send".into(), String::new()));
                                }
                            }
                        }
                        // The camera on this machine, straight into the
                        // agent's workspace.
                        voice::Act::Photo => {
                            let cam = camera::detect();
                            if !cam.ready() {
                                let mut out = tokio::io::stdout();
                                let _ = out
                                    .write_all(format!("\r\n[nexmesh] {}\r\n", cam.why_not()).as_bytes())
                                    .await;
                                let _ = out.flush().await;
                            } else {
                                if let Some(b) = &mut bar {
                                    b.set_mark("camera", true);
                                }
                                let (bs, cl, tx, sess, who) = (
                                    base.to_string(), c.clone(), said_tx.clone(),
                                    session.to_string(), name.to_string(),
                                );
                                let dir = scratch.clone();
                                tokio::spawn(async move {
                                    let shot = camera::photo(&cam, &dir).await;
                                    let r = match shot {
                                        Ok(path) => {
                                            let out = send_file(&bs, &cl, &sess, &path, Some(&who),
                                                Some("A photo for you to look at, taken just now."))
                                                .await;
                                            let _ = std::fs::remove_file(&path);
                                            out
                                        }
                                        Err(e) => Err(e),
                                    };
                                    let _ = tx.send(r.map(|said| format!("\u{0001}{said}"))).await;
                                });
                            }
                        }
                        voice::Act::Aloud => {
                            aloud = !aloud;
                            if !aloud {
                                voice::hush(&playing);
                            }
                            if let Some(b) = &mut bar {
                                b.set_mark("aloud", aloud);
                            }
                        }
                    }
                }
                if let Some(why) = leaving {
                    if !forward.is_empty() {
                        let _ = wire.tx.send(wire::Out::Data(forward)).await;
                    }
                    break why;
                }
                if !forward.is_empty() {
                    // Anything the operator submits closes off the agent's
                    // last answer: what comes next answers THIS.
                    if forward.iter().any(|b| *b == b'\r' || *b == b'\n') {
                        if let Ok(mut h) = heard.lock() {
                            h.submitted();
                        }
                        spoke_once = false;
                    }
                    if wire.tx.send(wire::Out::Data(forward)).await.is_err() {
                        break "connection lost".to_string();
                    }
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
                let _ = wire.tx.send(wire::Out::Resize { cols: c, rows: agent_r }).await;
            }
            Some(why) = quit_rx.recv() => break why,
        }
    };

    let _ = wire.tx.send(wire::Out::Close).await;
    reader.abort();
    if let Some(b) = &bar {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(&b.release()).await;
        let _ = out.flush().await;
    }
    {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(windowtitle::POP).await;
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
            // SEATS is who is on the team; TERMINALS is how many of them have
            // a window adopted onto them. One number covering both read as a
            // team twice its size.
            // Busiest first, because that is the order the hub answers in:
            // the team something happened in a minute ago is the one you
            // came to look at.
            println!("{:<14} {:<22} {:<10} {:>5}  {:<10} {}", "ID", "NAME", "STATUS", "SEATS",
                     "TERMINALS", "LAST");
            let now = nexmesh_proto::now_ms();
            for s in rows {
                println!(
                    "{:<14} {:<22} {:<10} {:>5}  {:<10} {}",
                    s["id"].as_str().unwrap_or("?"),
                    s["name"].as_str().unwrap_or("?"),
                    s["status"].as_str().unwrap_or("?"),
                    s["slots"].as_i64().unwrap_or(0),
                    match s["terminals"].as_i64().unwrap_or(0) {
                        0 => "-".to_string(),
                        n => n.to_string(),
                    },
                    match s["last_at_ms"].as_i64() {
                        Some(t) if t > 0 => ago(now, t),
                        _ => "-".to_string(),
                    }
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

        Cmd::Camera { session, seconds, to, say } => {
            let cam = camera::detect();
            if !cam.ready() {
                anyhow::bail!("{}", cam.why_not());
            }
            let dir = std::env::temp_dir().join("nexmesh-camera");
            let (path, what) = match seconds {
                Some(n) => (camera::clip(&cam, &dir, n).await?, format!("{n}s clip")),
                None => (camera::photo(&cam, &dir).await?, "photo".to_string()),
            };
            let bytes = std::fs::read(&path)?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "capture".into());
            println!("{what}: {} ({} bytes)", path.display(), bytes.len());
            let mut body = json!({ "name": name, "data_base64": b64(&bytes) });
            body["say"] = json!(say.unwrap_or_else(|| {
                "A picture for the team to look at, taken just now.".to_string()
            }));
            if let Some(t) = to {
                body["to"] = json!(t);
            }
            let v = post(&c, format!("{base}/v1/sessions/{session}/upload"), body).await?;
            bail_on_error(&v)?;
            let _ = std::fs::remove_file(&path);
            println!(
                "  it is at {} on {}",
                v["path"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?")
            );
        }

        Cmd::Upload { session, file, say, node, workspace, dir, to } => {
            let bytes = std::fs::read(&file)
                .with_context(|| format!("reading {file}"))?;
            // 64MB, the same limit the node enforces: said here, before the
            // bytes travel, rather than after.
            if bytes.len() > 64 * 1024 * 1024 {
                anyhow::bail!(
                    "{file} is {:.1}MB, over the 64MB limit for one upload. Put it in the \
                     workspace directly and say where it is with `nexmesh send`.",
                    bytes.len() as f64 / 1_048_576.0
                );
            }
            let name = std::path::Path::new(&file)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "upload".into());
            let mut body = json!({ "name": name, "data_base64": b64(&bytes) });
            if let Some(t) = say { body["say"] = json!(t); }
            if let Some(n) = node { body["node"] = json!(n); }
            if let Some(w) = workspace { body["workspace"] = json!(w); }
            if let Some(d) = dir { body["dir"] = json!(d); }
            if let Some(t) = to { body["to"] = json!(t); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/upload"), body).await?;
            bail_on_error(&v)?;
            println!(
                "{} is at {} on {} ({} bytes)",
                name,
                v["path"].as_str().unwrap_or("?"),
                v["node"].as_str().unwrap_or("?"),
                v["bytes"].as_i64().unwrap_or(0)
            );
            println!("  the team has been told where it is");
        }

        Cmd::SetOwns { who, patterns, session, add, none } => {
            let slot = if who.starts_with("sl-") {
                who.clone()
            } else {
                let s = session.clone().ok_or_else(|| anyhow::anyhow!(
                    "'{who}' is a name, not a seat id, so say which session: --session <name>"
                ))?;
                resolve_seat(&c, &base, &s, &who).await?
            };
            if patterns.is_empty() && !none {
                anyhow::bail!(
                    "say what it owns, or --none to clear it: nexmesh set-owns {who} 'verti/**'"
                );
            }
            let v = post(
                &c,
                format!("{base}/v1/slots/{slot}/owns"),
                json!({
                    "patterns": if none { Vec::new() } else { patterns.clone() },
                    "add": add,
                }),
            )
            .await?;
            bail_on_error(&v)?;
            let owns: Vec<&str> =
                v["owns"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
            if owns.is_empty() {
                println!("{who} owns nothing in {}", v["workspace"].as_str().unwrap_or("?"));
            } else {
                println!("{who} owns: {}", owns.join(", "));
                println!("  teammates see this in list_peers; who_owns answers for one path");
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

        Cmd::Status { session, terminals } => {
            let v = get(&c, format!("{base}/v1/sessions/{session}/status")).await?;
            bail_on_error(&v)?;
            let empty = vec![];
            let all = v["agents"].as_array().unwrap_or(&empty);
            // An adopted terminal is the screen another seat is sitting in,
            // not a teammate. Ten of them on a fleet of ten doubles this
            // table with rows nobody can talk to, so they are shown only
            // when asked for.
            let rows: Vec<&Value> = all
                .iter()
                .filter(|a| terminals || a["cli"].as_str() != Some("tmux"))
                .collect();
            let hidden = all.len() - rows.len();
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
            for a in &rows {
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
            if hidden > 0 {
                println!(
                    "  ({hidden} adopted terminal{} not shown; they are the screens these \
                     seats sit in -- `--terminals` lists them)",
                    if hidden == 1 { "" } else { "s" }
                );
            }
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

        Cmd::Run { cli, session, name, node, role, cwd, no_attach, dry_run } => {
            runcmd::run(
                &base,
                &c,
                runcmd::Opts {
                    cli: &cli,
                    session: session.as_deref(),
                    name: name.as_deref(),
                    node: node.as_deref(),
                    role: role.as_deref(),
                    cwd: cwd.as_deref(),
                    dry_run,
                    attach: !no_attach,
                },
                args.ca_cert.as_deref(),
                identity,
            )
            .await?;
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

#[cfg(test)]
mod bar_tests {
    use super::StatusBar;

    fn bar(cols: u16) -> StatusBar {
        let mut b = StatusBar::new(
            "Nexmesh-dev".into(),
            "machine-b".into(),
            "nexmesh".into(),
            "term-Nexmesh-dev".into(),
            cols,
            40,
        )
        .unwrap();
        b.keys = vec![
            ("Ctrl-\\".into(), "commands".into()),
            ("Ctrl-]".into(), "detach".into()),
        ];
        b.menu = vec![
            ("Ctrl-G".into(), "say it".into()),
            ("Ctrl-O".into(), "read".into()),
            ("Ctrl-T".into(), "aloud".into()),
            ("Ctrl-\\".into(), "off".into()),
        ];
        b
    }

    #[test]
    fn the_way_out_is_first_and_the_seat_is_named_the_way_the_title_names_it() {
        let line = bar(160).line();
        assert!(line.trim_start().starts_with("Ctrl-] detach"), "{line}");
        // The same form as the window title: one way of naming a seat.
        assert!(line.contains("nexmesh:Nexmesh-dev"), "{line}");
        assert!(line.contains("on machine-b"), "{line}");
        assert!(line.contains("via term-Nexmesh-dev"), "{line}");
        assert!(line.contains("Ctrl-\\ commands"), "{line}");
        assert!(line.chars().count() <= 160);
    }

    #[test]
    fn a_narrow_one_gives_up_everything_but_the_way_out_and_the_name() {
        // The pane goes first, then the machine, then the key labels, then
        // the other keys. Detach and the seat survive to the last.
        for cols in [120u16, 100, 86, 72, 60, 48, 36, 24] {
            let line = bar(cols).line();
            assert!(line.chars().count() <= cols as usize, "{cols}: {line}");
            assert!(line.contains("Ctrl-]"), "{cols}: no way out on the screen: {line}");
            if cols >= 36 {
                assert!(line.contains("Nexmesh-dev"), "{cols}: {line}");
            }
        }
        assert!(!bar(80).line().contains("via "), "the adopted pane goes first");
        assert!(!bar(60).line().contains("on machine-b"), "then the machine: {}", bar(60).line());
        assert!(bar(80).line().contains("Ctrl-] detach"), "{}", bar(80).line());
        assert!(bar(80).line().contains("nexmesh:Nexmesh-dev"), "{}", bar(80).line());
    }

    #[test]
    fn a_passing_state_replaces_itself_rather_than_repeating() {
        let mut b = bar(160);
        b.set_mark("listening", true);
        assert_eq!(b.line().matches("listening").count(), 1);
        b.set_mark("listening", true);
        assert_eq!(b.line().matches("listening").count(), 1, "pressed twice, said once");
        b.say_mark("reading", "reading 2 back".into());
        b.say_mark("reading", "reading 3 back".into());
        let line = b.line();
        assert!(line.contains("reading 3 back") && !line.contains("reading 2 back"), "{line}");
        b.set_mark("listening", false);
        assert!(!b.line().contains("listening"));
    }
}

#[cfg(test)]
mod bar_shapes {
    use super::StatusBar;

    /// What the row actually says at a few widths. Not an assertion about
    /// taste: it is here so a change to the fitting shows up as a diff
    /// somebody can read.
    #[test]
    fn the_shapes_it_takes() {
        for cols in [160u16, 110, 90, 70, 50, 30] {
            let mut b = StatusBar::new(
                "Nexmesh-dev".into(),
                "machine-b".into(),
                "nexmesh".into(),
                "term-Nexmesh-dev".into(),
                cols,
                40,
            )
            .unwrap();
            b.keys = vec![
                ("Ctrl-\\".into(), "commands".into()),
                ("Ctrl-]".into(), "detach".into()),
            ];
            b.menu = vec![
                ("Ctrl-G".into(), "say it".into()),
                ("Ctrl-O".into(), "read".into()),
                ("Ctrl-T".into(), "aloud".into()),
                ("Ctrl-\\".into(), "off".into()),
            ];
            println!("{cols:>4} |{}|", b.line());
            b.command = true;
            println!("{cols:>4} *|{}|", b.line());
            assert!(b.line().chars().count() <= cols as usize);
            b.command = false;
            assert!(b.line().chars().count() <= cols as usize);
        }
    }
}
