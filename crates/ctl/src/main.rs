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
mod editor;
mod camui;
mod install;
mod replies;
mod termmode;
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
    // clap's own command list is dropped from the template below, because
    // this one replaces it: 38 commands sorted alphabetically, each with as
    // much prose as it takes to wrap, told a newcomer nothing and buried the
    // grouped list under the options. One listing, grouped by what you are
    // doing, with a gloss short enough to stay on its line.
    //
    // `nexmesh help <command>` is where the long version lives.
    help_template = "\
{about}

USAGE
  nexmesh [OPTIONS] <COMMAND>
  nexmesh                     with no command: the operator console{after-help}
OPTIONS
{options}
",
    after_help = "LOOKING
  health              hub health and version
  nodes               machines, their CLIs, and whether they are connected
  sessions            which teams exist
  status              who is in a team, and what they are doing
  log                 what the agents said to each other
  transcript          what one agent's process actually printed
  certs               certificates the fleet CA has issued

STARTING AGENTS
  run                 start a CLI here, attachable from the first keystroke
  spawn               start an agent in a seat that already exists
  attach              that agent's real terminal, from anywhere
  seat                give a conversation you already have a keyboard
  adopt               take over a tmux session that is already running

TEAMS
  new-session         make a team
  rename-session      rename one
  remove-session      remove one, and everything that belonged to it
  add-workspace       mount a directory into a team
  upload              put a file where a team's agents can read it
  sync                publish, pull or report on a shared workspace
  budget              set, raise or clear a team's token budget
  team-rules          read or set the rules everybody in a team is told

SEATS
  add-agent           add a seat to a team
  rename-agent        rename one
  move-agent          move a seat, and whoever is in it, to another team
  remove-agent        remove a seat and its history
  set-role            change what a seat is for
  set-owns            say which parts of the workspace a seat answers for
  invite              have a seat from another team also answer in this one
  seat-respawn        whether the hub brings a seat back when it exits
  seat-approve        answer a seat's permission prompts without asking you
  seat-prompt         what a seat tells its agent: system prompt, instruction
  roles               the ready-made role briefs, to use as a seat's prompt
  compact-at          compact a seat by itself when its context fills up
  loop-guard          stop a seat that repeats the same turn over and over
  flood               refuse messages from a seat that is flooding the team

WORK
  send                say something to the team, or to one agent
  pause               stop sending work to an agent, or to everyone
  resume              start sending work again
  release             accept an agent's work and let it stop
  cancel              withdraw a message nobody has seen yet
  camera              take a photo or clip for an agent to look at

SETUP
  pair                join a fleet as an operator, with a code from the dashboard
  install-seat        make a CLI you already use a member of a team
  install-orchestrator
                      let a CLI spin up teams from whatever session it is in
  revoke              cut off a machine or operator, at the TLS handshake

EXAMPLES
  nexmesh
      the operator console: the whole fleet on one screen, j/k and Enter
  nexmesh run
      start claude here, on a terminal the fleet owns, attachable at once
  nexmesh run codex --role reviewer
      another CLI, in a seat of its own, with a role
  nexmesh status payments
      who is in that team, and what they are doing
  nexmesh attach payments Dev
      that agent's real terminal, from here
  nexmesh send payments 'staging is back'
      say something to the whole team
  nexmesh install-seat --cli claude --role engineer
      make the CLI you already use a member of the team

Every command takes --hub, and --token where the hub wants one. `nexmesh pair`
writes both into ~/.nexmesh so you need not pass them again.

  nexmesh help <command>    the long version of any command, with examples
  man nexmesh               every argument, at the terminal\n"
)]
struct Args {
    /// The hub to talk to.
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
  nexmesh set-owns Max --session payments --add team
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
    #[command(after_help = "\
A template is a ready-made brief for a role: what that seat is for, what it
must not do, and how to do it well. Apply one with:

  nexmesh seat-prompt <team> <seat> --template reviewer

It is rendered and STORED on the seat, so it can be read and edited
afterwards and does not change under a running team because somebody
improved a template.

A fleet can keep its own in the hub's --prompts-dir: a file there replaces
the built-in of the same name, and a new name adds a role.")]
    #[command(long_about = "\
A team's STANDING RULES: what everybody working on this session is told, over
and above their own brief.

For what is true of the WORK rather than of one seat -- the conventions, the
things not to touch, how this team does releases. Written once here instead of
into every seat's prompt, where it drifts the moment one is edited.

Every seat started from now on gets them in its first prompt, and everybody
already running is told in the chat, so a change does not wait for a respawn.

The operator sets them here. A seat holding orchestrator authority can set
them with its own `set_team_rules` tool; an ordinary agent cannot, because
rules an agent can rewrite are not rules.")]
    #[command(after_help = "\
  nexmesh team-rules payments                      # read them
  nexmesh team-rules payments --from rules.md      # set them from a file
  cat rules.md | nexmesh team-rules payments --from -
  nexmesh team-rules payments --clear

Keep them short: rules nobody can hold in their head are rules nobody
follows. The detail belongs in the team's knowledge, not here.")]
    /// Read or set a team's standing rules.
    TeamRules {
        /// The team, by name or id.
        session: String,
        /// Read the rules from a markdown file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        from: Option<String>,
        /// Remove them.
        #[arg(long, conflicts_with = "from")]
        clear: bool,
    },
    /// The ready-made role briefs this fleet has.
    Roles {
        /// Show one in full, as it would be given to a seat.
        role: Option<String>,
        /// With a role: preview it carrying these skills instead.
        #[arg(long, value_name = "A,B")]
        skills: Option<String>,
    },
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
        ///
        /// Left out, claude: `nexmesh run` is meant to be what you type
        /// instead of the CLI's own name, and having to name it every time
        /// made the common case longer than the thing it replaces.
        #[arg(value_name = "CLI", default_value = "claude")]
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
        /// Take the seat of this name back, with the conversation it was
        /// last on, instead of starting a new agent beside it.
        ///
        /// Without this, `run` starts a NEW agent every time: two people
        /// running it in one directory get two agents with two
        /// conversations, rather than the second quietly inheriting the
        /// first one's history.
        #[arg(long)]
        reuse: bool,
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
        /// CLI to install into: any whose descriptor says how, which is
        /// claude, codex, opencode, qwen, gemini, cursor or freebuff today.
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
    #[command(after_help = "\
A seat usually belongs to one team, because a session is the boundary agents
never see across. Some seats are SERVICES rather than teammates, though -- a
capability bridge offering OCR and transcription, a reviewer every team sends
work to -- and one copy per team means one process, one model and one set of
warm context per team for a single job.

  nexmesh invite app CoderAI        # CoderAI also answers in 'app'
  nexmesh invite app CoderAI --out  # and stops again

It does not MOVE: its own team, workspace and owner are unchanged. It becomes
addressable in the invited team by name, hears that team's messages, and keeps
ONE conversation across all of them -- which is the point, and also means
anything it is told in one room it knows in the others.")]
    #[command(after_help = "\
A seat that comes back by itself is PREPARED for you: the process is there and
`nexmesh attach <agent>` reaches it. It never reattaches your terminal -- whose
terminal that would be is not the hub's to decide.

  nexmesh seat-respawn team alice        # comes back by itself
  nexmesh seat-respawn team alice --off  # stays down until you start it

Turn it off for a seat whose work is over, or one you are deliberately stopping
to look at: those used to be respawned out from under you every few seconds.")]
    /// Whether the hub brings a seat back by itself when its process exits.
    SeatRespawn {
        /// The team the seat is in, by name or id.
        session: String,
        /// The seat, by name or slot id.
        agent: String,
        /// Leave it down instead.
        #[arg(long)]
        off: bool,
    },
    #[command(after_help = "\
EXAMPLES
  nexmesh compact payments Dev

Done the way the seat's CLI does it: typed at a terminal, sent as a command to
a headless claude, or through codex's own protocol. A terminal waiting on a
question refuses, and says so in the team's chat. `nexmesh status <session>`
shows how full each seat is (CTX).")]
    /// Compact a seat's context now.
    Compact {
        /// The team the seat is in, by name or id.
        session: String,
        /// The seat, by name or slot id.
        agent: String,
    },
    #[command(after_help = "\
Compacting an agent throws away context it may still need, so this is OFF for
every seat until somebody sets it. Three levels: a seat's own setting wins,
then its team's, then the fleet's (`nexmesh-hub --compact-at-pct`).

  nexmesh compact-at payments Dev 85        # this seat, at 85% of its window
  nexmesh compact-at payments session 80         # everyone in the team who has not set their own
  nexmesh compact-at payments Dev off       # never, whatever the team says
  nexmesh compact-at payments Dev default   # follow the team, then the fleet

The window is the model\'s, not a guess: see `[context]` in the CLI\'s
descriptor. `nexmesh status <session>` shows how full each seat is now.")]
    /// Compact a seat by itself once its context passes a percentage of its window.
    CompactAt {
        /// The team the seat is in, by name or id.
        session: String,
        /// The seat, by name or slot id -- or `session` for the whole team.
        agent: String,
        /// A percentage (1-100), `off`, or `default` to inherit.
        pct: String,
    },
    #[command(after_help = "\
An agent in a loop is not a stuck process -- it is a busy one: it answers
heartbeats, it burns tokens, and from outside it looks like an agent working
hard. What gives it away is doing the SAME thing over and over, which is what
this counts: the same tool with the same arguments, or the same answer, back to
back. Different arguments are different work and are never counted.

Over the threshold the seat is interrupted, PAUSED, and reported to you. Its
conversation is left intact, because whatever drove it into a loop is in there.

  nexmesh loop-guard payments Dev 5       # five identical turns is a loop
  nexmesh loop-guard payments session 8
  nexmesh loop-guard payments poller off       # a seat that polls is meant to repeat itself

OFF for every seat until somebody sets it. Minimum 2.")]
    /// Stop a seat that repeats the same turn over and over.
    LoopGuard {
        /// The team the seat is in, by name or id.
        session: String,
        /// The seat, by name or slot id -- or `session` for the whole team.
        agent: String,
        /// How many identical turns in a row, `off`, or `default` to inherit.
        repeats: String,
    },
    #[command(after_help = "\
Two different failures, two numbers:

  --per-min   one seat sending too much, to anybody. A runaway reaches 30 in
              seconds; a team doing real work never does.
  --per-pair  two seats talking only to EACH OTHER. A ping-pong is not fast,
              it is relentless: two agents answering each other every twenty
              seconds never trip a per-minute rate, and between them burn two
              turns a minute for as long as nobody looks. 20 is generous.

  nexmesh flood payments session --per-min 30 --per-pair 20
  nexmesh flood payments coordinator --per-min 90     # it legitimately sends more
  nexmesh flood payments alice off

Over the limit the message is REFUSED and the agent is told what to do
instead -- its work is not stopped, only its messages -- and you are told
once. OFF for every seat until somebody sets it.")]
    /// Refuse messages from a seat that is flooding the team.
    Flood {
        /// The team, by name or id.
        session: String,
        /// The seat, by name or slot id -- or `session` for the whole team.
        agent: String,
        /// Messages a minute. `off` or `default` also accepted here.
        #[arg(long, value_name = "N")]
        per_min: Option<String>,
        /// Messages exchanged with any ONE other seat in ten minutes.
        #[arg(long, value_name = "N")]
        per_pair: Option<String>,
        /// Turn both off for this seat.
        #[arg(long, conflicts_with_all = ["per_min", "per_pair"])]
        off: bool,
    },
    /// Answer a seat's own permission prompts without asking the operator.
    SeatApprove {
        session: String,
        agent: String,
        /// Bring the prompts back to the operator instead.
        #[arg(long)]
        off: bool,
    },
    #[command(long_about = "\
What a seat tells its agent, on top of whatever the CLI says by itself.

Two separate things:

  * A SYSTEM PROMPT -- who the agent in this seat is. It goes to the CLI as a
    system prompt, added to the CLI's own or used instead of it, and it lasts:
    every agent that ever fills this seat gets it, including a respawn at 4am
    that nobody watched.

  * An INSTRUCTION -- what it should do on arrival. It is said to each NEW
    occupant as the first thing it hears. The agent already sitting in the
    seat does not get it again.

Both are markdown, read from a file or piped in, because a prompt worth having
is longer than a shell line.")]
    #[command(after_help = "\
  nexmesh seat-prompt team alice                         # show what is set
  nexmesh seat-prompt team alice --system role.md        # from a file
  cat role.md | nexmesh seat-prompt team alice --system -
  nexmesh seat-prompt team alice --system role.md --replace
  nexmesh seat-prompt team alice --instructions today.md
  nexmesh seat-prompt team alice --clear-instructions
  nexmesh seat-prompt payments Iris --template reviewer
  nexmesh seat-prompt payments Iris --template sysadmin --skills live-systems
  nexmesh seat-prompt payments Iris --skill secrets-hygiene    # one more, for this seat
  nexmesh seat-prompt payments Iris --no-skill handover        # one fewer
  nexmesh roles                                                # what there is

A seat remembers the template it was built from and which of its skills it
carries, so --skill and --no-skill change one thing and leave the rest; the
brief is re-rendered from the template each time. Writing --system yourself
drops the link, because re-rendering would then throw your words away.

--replace throws away the CLI's own system prompt, which is right for a seat
that is not meant to behave like that CLI at all and a trap otherwise. The
default, --append, keeps it.

Neither reaches the agent now sitting there: the system prompt applies to the
next process started in the seat, and the instruction to the next occupant.")]
    /// What this seat tells its agent: a standing system prompt, and an
    /// instruction for each new occupant.
    SeatPrompt {
        /// The team the seat is in, by name or id.
        session: String,
        /// The seat, by name or slot id.
        agent: String,
        /// Read the system prompt from a markdown file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        system: Option<String>,
        /// Use the ready-made brief for this role instead of writing one:
        /// `coder`, `reviewer`, `sysadmin`, ... (`nexmesh roles` lists them).
        #[arg(long, value_name = "ROLE", conflicts_with = "system")]
        template: Option<String>,
        /// Which skill sections the brief carries, comma-separated,
        /// replacing whatever it carries now. `--skills ''` for none.
        #[arg(long, value_name = "A,B")]
        skills: Option<String>,
        /// Select one more skill for THIS seat, leaving the rest alone.
        /// Repeatable: `--skill secrets-hygiene --skill handover`.
        #[arg(long = "skill", value_name = "NAME")]
        skills_on: Vec<String>,
        /// Deselect one, leaving the rest alone. Repeatable.
        #[arg(long = "no-skill", value_name = "NAME")]
        skills_off: Vec<String>,
        /// Read the arrival instruction from a markdown file, or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        instructions: Option<String>,
        /// Add the seat's system prompt to the CLI's own (the default).
        #[arg(long, conflicts_with = "replace")]
        append: bool,
        /// Use the seat's system prompt INSTEAD of the CLI's own.
        #[arg(long)]
        replace: bool,
        /// Forget the system prompt.
        #[arg(long, conflicts_with = "system")]
        clear_system: bool,
        /// Forget the arrival instruction.
        #[arg(long, conflicts_with = "instructions")]
        clear_instructions: bool,
    },
    #[command(long_about = "\
Withdraw a message nobody has seen, so it is never delivered.

A message waits until it is delivered, and nothing expires it. Addressed to a
seat that has gone, or sent in error, it sits there: the sender believes it
was heard, and the next process to take that seat is handed something stale
the moment it starts.

This is not `clear`, which marks what a person has dealt with and still
delivers the message. A message an agent has ALREADY seen cannot be unsent,
and is refused -- the log is read later to work out what an agent knew.")]
    #[command(after_help = "\
  nexmesh cancel team --id 4821    # one message, by the id the log shows
  nexmesh cancel team --unseen     # everything nobody has seen yet

`nexmesh log <team>` prints the ids.")]
    /// Withdraw a message nobody has seen yet.
    Cancel {
        /// The team it was sent in, by name or id.
        session: String,
        /// One message, by the id the log and the dashboard show.
        #[arg(long)]
        id: Option<i64>,
        /// Or every message in this team that nobody has seen yet.
        #[arg(long, conflicts_with = "id")]
        unseen: bool,
    },
    /// Have a seat from another team also answer in this one.
    Invite {
        /// The team it should also answer in, by name or id.
        session: String,
        /// The seat, by name or slot id.
        agent: String,
        /// Take it out of that team again instead.
        #[arg(long)]
        out: bool,
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
/// The seat's own row from the session listing: slot id, provenance, status.
///
/// `resolve` answers about the AGENT and says nothing about whether the seat
/// is one the hub may start, which is the question attach has to ask before
/// starting anything.
async fn seat_row(
    c: &reqwest::Client,
    base: &str,
    session: &str,
    who: &str,
) -> Option<Value> {
    let v = get(c, format!("{base}/v1/sessions/{session}/status")).await.ok()?;
    let agents = v["agents"].as_array()?.clone();
    agents.into_iter().find(|a| {
        a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(who))
            || a["slot_id"].as_str() == Some(who)
    })
}

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
        // Hand the terminal back the way it was handed over. See
        // `TERMINAL_RESTORE`.
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(TERMINAL_RESTORE);
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
        println!("\r");
    }
}

/// Put somebody's terminal back the way it was found.
///
/// An attach hands the screen to a program that changes how the terminal
/// behaves: a full-screen CLI turns on the alternate screen, mouse
/// reporting, and ALTERNATE SCROLL -- the mode where the wheel is turned
/// into cursor keys. Leave those on after a detach and the wheel scrolls
/// the shell's command history instead of the scrollback, which is a
/// terminal that looks broken and is fixed by `reset`. Nobody should have
/// to know that.
///
/// So detaching says, in order: leave the alternate screen, turn off every
/// mouse mode and alternate scroll, reset the scroll region, show the
/// cursor, put wrap and insert mode back, and clear any leftover colour.
/// Take the terminal as it is found, before the agent's first byte.
///
/// `?1007` is ALTERNATE SCROLL: with it on, the wheel is translated into
/// cursor keys, so scrolling walks a CLI's input history instead of its
/// scrollback -- Claude Code says "scroll wheel is sending arrow keys, use
/// PgUp/Down" when it notices. xterm turns it on by default in the
/// alternate screen, which is where every full-screen agent lives, so an
/// attach has to turn it off rather than inherit it.
///
/// Bracketed paste goes too: the agent's own CLI turns it on if it wants
/// it, and ours pasting `\x1b[200~` into somebody's prompt is not a thing
/// anybody asked for.
const TERMINAL_PREPARE: &[u8] = b"\x1b[?1007l\x1b[?2004l";

/// What the wheel does inside an attach.
///
/// With alternate scroll refused, the wheel has nothing to do: the screen
/// belongs to a full-screen program whose history is its own, and the
/// terminal's scrollback is empty. So the viewer takes the wheel and sends
/// PAGE keys -- which is what every full-screen program scrolls with, and
/// what Claude Code asks for by name when it catches a terminal turning the
/// wheel into arrow keys.
///
/// Only while the agent is not using the mouse itself. The moment it asks
/// for mouse events the viewer gets out of the way, because a program that
/// asked for clicks should get clicks.
const PAGE_UP: &[u8] = b"\x1b[5~";
const PAGE_DOWN: &[u8] = b"\x1b[6~";

const TERMINAL_RESTORE: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1007l\x1b[?2004l\x1b[r\x1b[?25h\x1b[?7h\x1b[4l\x1b[0m\x1b(B";

/// Ctrl-] detaches, the way telnet does. Ctrl-C cannot: in raw mode it belongs
/// to the agent, which is the whole point of a takeover.
/// The voice keys, from `~/.nexmesh/console.toml`.
///
/// Defaults: Ctrl-^ dictates, Ctrl-_ reads a reply back, Ctrl-\ reads every
/// new one as it arrives. All three are control characters because an attach
/// forwards every printable byte to the agent, and all three are
/// configurable because Ctrl-^ is Ctrl+Shift+6 on a US layout and awkward or
/// impossible on others.
fn read_console_toml() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&home).join(".nexmesh").join("console.toml");
    std::fs::read_to_string(path).unwrap_or_default()
}

fn voice_keys() -> voice::Keys {
    voice::keys_from(&read_console_toml())
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

/// Whitespace to single spaces, and no line breaks at all.
///
/// What a transcription service answers with is a transcript, not a line: it
/// breaks on utterances, and those breaks are Enter when they are typed at a
/// CLI prompt. See the dictation path for what that did.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Fit something into a row of a status bar without lying about its length.
fn clipped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// One row at the bottom of the screen that is ours, not the agent's.
///
/// An attach is a full-screen takeover of somebody else's terminal, and the
/// question it always raises is "how do I get out of this?". The remote is
/// told the screen is one row shorter than it is, and that row says whose
/// terminal this is and which key brings you back.
#[derive(Clone)]
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
    /// Passing state: `aloud`, `reading 2 back`.
    marks: Vec<String>,
    /// Which entry the command row starts at. `?` moves it on and wraps,
    /// so every key is reachable from the row itself.
    page: usize,
    /// Recording, and since when. The bar goes RED and says so: a mode you
    /// cannot see is a microphone you forget is on. `hands_free` is the
    /// difference between one sentence and a conversation.
    recording: Option<std::time::Instant>,
    hands_free: bool,
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
            page: 0,
            recording: None,
            hands_free: false,
            command: false,
            menu: Vec::new(),
            ask: None,
            cols,
            rows,
        })
    }

    /// How many menu entries fit on a row of `w`, starting at `self.page`.
    ///
    /// Measured against the row this will actually draw, labels and all:
    /// a count worked out from an average width is a count that is wrong
    /// at both ends of the menu.
    fn page_len(&self, w: usize) -> usize {
        let sep = "  \u{00b7}  ";
        let labelled: Vec<String> =
            self.menu.iter().map(|(k, what)| format!("{k} {what}")).collect();
        if labelled.is_empty() {
            return 0;
        }
        let from = self.page.min(labelled.len() - 1);
        let room = labelled.len() - from;
        for n in (1..=room).rev() {
            // The widest shape this could end up as, so a row that fits
            // here still fits once the hint and the marks are on it.
            let body = labelled[from..from + n].join(sep);
            let hint = if n < labelled.len() { format!("{sep}? 9/9") } else { String::new() };
            if format!(" COMMANDS{sep}{body}{hint}").chars().count() <= w {
                return n;
            }
        }
        1
    }

    /// How many pages the menu takes at this width, and which one is up.
    /// Counted by walking from the start, because the pages are not all
    /// the same size: the labels differ in length.
    fn pages(&self, w: usize) -> usize {
        let mut at = 0usize;
        let mut pages = 0usize;
        let mut probe = StatusBar { page: 0, ..self.clone() };
        while at < self.menu.len() && pages < 32 {
            probe.page = at;
            at += probe.page_len(w).max(1);
            pages += 1;
        }
        pages.max(1)
    }

    fn page_no(&self, w: usize) -> usize {
        let mut at = 0usize;
        let mut n = 0usize;
        let mut probe = StatusBar { page: 0, ..self.clone() };
        while at < self.menu.len() && n < 32 {
            if at >= self.page {
                return n;
            }
            probe.page = at;
            at += probe.page_len(w).max(1);
            n += 1;
        }
        n.saturating_sub(1)
    }

    /// `?`: on to the next few, and round to the first.
    fn next_page(&mut self) {
        let w = self.cols as usize;
        let n = self.page_len(w).max(1);
        self.page += n;
        if self.page >= self.menu.len() {
            self.page = 0;
        }
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
        // Recording owns the row above everything except a question, and
        // says how long it has been going: a counter that moves is the proof
        // the microphone is actually open, which a static word is not.
        if let (Some(since), None) = (self.recording, self.ask.as_ref()) {
            let secs = since.elapsed().as_secs();
            let what = if self.hands_free { "LISTENING" } else { "RECORDING" };
            let how = if self.hands_free {
                "stops when you stop talking"
            } else {
                "press again to send it"
            };
            let line = format!(
                " \u{25cf} {what} {}:{:02}{sep}{how}",
                secs / 60,
                secs % 60
            );
            if line.chars().count() <= w {
                return line;
            }
            return format!(" \u{25cf} {what} {}:{:02}", secs / 60, secs % 60)
                .chars()
                .take(w)
                .collect();
        }
        // A question owns the row while it is up.
        if let Some((what, typed)) = &self.ask {
            let line = format!(" {what}: {typed}\u{2588}{sep}Enter sends \u{b7} Esc cancels");
            if line.chars().count() <= w {
                return line;
            }
            let bare = format!(" {what}: {typed}\u{2588}");
            return bare.chars().skip(bare.chars().count().saturating_sub(w)).collect();
        }
        // In command mode the row IS the menu: what the keys do, and how
        // to get out. Nothing else on it matters while it is up.
        //
        // More keys than fit on one row, so the row PAGES. `?` turns to the
        // next few and wraps round to the first -- which keeps everything
        // in the one place somebody is already looking, instead of printing
        // a list into the middle of the agent's screen.
        if self.command {
            let id = if self.session.is_empty() {
                self.who.clone()
            } else {
                format!("{}:{}", self.session, self.who)
            };
            let marks: String = if self.marks.is_empty() {
                String::new()
            } else {
                format!("{sep}{}", self.marks.join(sep))
            };
            let labelled: Vec<String> =
                self.menu.iter().map(|(k, what)| format!("{k} {what}")).collect();
            if labelled.is_empty() {
                return format!(" COMMANDS{sep}{id}").chars().take(w).collect();
            }
            let from = self.page.min(labelled.len() - 1);
            let n = self.page_len(w);
            let more = n < labelled.len();
            let shown = &labelled[from..(from + n).min(labelled.len())];
            // `? 2/3` rather than `? more`: where you are, and how much
            // there is, which is what somebody pressing it again wants.
            let hint = if more {
                format!("{sep}? {}/{}", self.page_no(w) + 1, self.pages(w))
            } else {
                String::new()
            };
            let body = shown.join(sep);
            // The seat's name is the first thing to give up when the row is
            // tight: the row above has been saying it all along.
            for line in [
                format!(" COMMANDS{sep}{body}{hint}{marks}{sep}{id}"),
                format!(" COMMANDS{sep}{body}{hint}{marks}"),
                format!(" COMMANDS{sep}{body}{hint}"),
            ] {
                if line.chars().count() <= w {
                    return line;
                }
            }
            return format!(" COMMANDS{sep}{body}{hint}").chars().take(w).collect();
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
        let mut out = b"\x1b7".to_vec();
        out.extend_from_slice(&self.paint_body());
        out.extend_from_slice(b"\x1b8");
        out
    }

    /// The bar itself, with NO save or restore around it.
    ///
    /// Split out because DECSC IS NOT A STACK: a terminal has one saved
    /// cursor, so a save inside a save overwrites the outer one. `refresh`
    /// wrapped `paint`, which saved again -- and because the scroll region
    /// in between had just homed the cursor, the inner save stored 1,1 and
    /// both restores put the cursor there. Four times a second the agent's
    /// real cursor position was destroyed: it sat in the top-left corner and
    /// typing appeared there. One pair, around everything, or none.
    fn paint_body(&self) -> Vec<u8> {
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
        let colour = if self.recording.is_some() && self.ask.is_none() {
            // Red, and the only red this row ever is.
            "\x1b[41m\x1b[97m\x1b[1m"
        } else if self.ask.is_some() {
            // A question is a third state and looks like one.
            "\x1b[44m\x1b[97m\x1b[1m"
        } else if self.command {
            "\x1b[45m\x1b[97m\x1b[1m"
        } else {
            // The pale pink of the transgender flag (#f7a8b8), with near
            // black on it so the text stays readable on a light ground.
            //
            // Truecolor, because the 256-colour pinks are either too
            // magenta (218) or too salmon (217) to read as that flag; a
            // terminal without it falls back to its own nearest pink.
            "\x1b[48;2;247;168;184m\x1b[38;2;26;22;24m"
        };
        format!("\x1b[{};1H{}{}\x1b[0m", self.rows, colour, line).into_bytes()
    }

    /// The scroll region AND the bar, in one go.
    ///
    /// THE BAR USED TO SCROLL INTO THE CONTENT. The modes were re-asserted
    /// four times a second but the scroll region was not -- it was set once
    /// at the start and again only on a resize. Anything in the agent's
    /// output that resets it (`\x1b[r`, which a full-screen program emits on
    /// its way out, and tmux in several situations) therefore left the
    /// region reset until the next resize: plain output then scrolled over
    /// this row, carrying the bar up into the text, where the next repaint
    /// drew another one. Which is exactly what it looked like -- a bar
    /// appearing "under" the terminal in various places while scrolling.
    ///
    /// The region cannot simply be re-sent on the timer on its own, because
    /// DECSTBM homes the cursor on some terminals and that would move the
    /// agent's. Inside the save/restore pair it is harmless.
    fn refresh(&self) -> Vec<u8> {
        // ONE save/restore pair around all of it: the region, which homes the
        // cursor, and the bar. `paint_body` deliberately adds none of its own.
        let mut out = b"\x1b7".to_vec();
        out.extend_from_slice(&self.scroll_region());
        out.extend_from_slice(&self.paint_body());
        out.extend_from_slice(b"\x1b8");
        out
    }

    /// Keep the agent's output from scrolling over the bar. A full-screen app
    /// positions absolutely and stays inside the size it was given; a stream
    /// of plain output scrolls, and without this it would scroll our row away.
    fn scroll_region(&self) -> Vec<u8> {
        format!("\x1b[1;{}r", self.agent_rows()).into_bytes()
    }

    fn release(&self) -> Vec<u8> {
        let mut out = format!("\x1b[r\x1b[{};1H\x1b[2K", self.rows).into_bytes();
        out.extend_from_slice(TERMINAL_RESTORE);
        out
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

/// Adopt this agent's pane again, when the adoption died but the CLI did not.
///
/// The CLI is alive in a tmux pane; what died was the ADOPTION -- the node
/// owned that tmux client, the CLI in the pane did not, so a node restart
/// takes it. Before this, attach dead-ended with "its CLI was started by
/// hand, so only its own shim reaches it" and advised `nexmesh seat`, which
/// would have started a SECOND CLI on the same conversation. Spawning was
/// refused too, rightly, because the seat was occupied. The only way out was
/// to know that a `term-*` seat existed and to start it by hand.
///
/// This is not the same as filling an empty seat: nothing new is started, an
/// existing pane is given its keyboard back.
async fn readopt_terminal(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    name: &str,
    term: &str,
    term_name: &str,
) -> Result<Value> {
    eprintln!("({name} is in a terminal whose adoption has gone; adopting it again)");
    let v = post(c, format!("{base}/v1/slots/{term}/spawn"), json!({})).await?;
    bail_on_error(&v)?;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let again = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
        // Wait for the LINK, not merely for the terminal to exist: the node
        // re-proves through /proc that this seat's CLI is the process inside
        // that pane, and until it has, attaching would reach nothing.
        if !again["error"].is_string()
            && again["io_kind"].as_str().is_some_and(|k| !k.is_empty())
        {
            return Ok(again);
        }
    }
    anyhow::bail!(
        "adopted {term_name} again, but {name} is still not reachable through it. \
         Check the pane is still there:  nexmesh status {session} --terminals"
    )
}

/// Put something in the seat, because somebody is trying to attach to it.
///
/// Being told "put something in the seat with: nexmesh spawn ..." is an
/// instruction to type the obvious next command, and it came up every time a
/// node restarted. It also led somewhere bad: a seat that WAS coming back by
/// itself got spawned a second time, and because a conversation is resumed by
/// id, two CLIs ended up appending to one transcript.
///
/// Only for a seat the hub is entitled to fill: one it has been running, or
/// one nothing has ever filled. A seat whose last process was ATTACHED
/// belongs to whoever started it -- the hub does not know how they launched
/// it and has no business inventing a replacement -- and an EXTERNAL
/// participant is not ours to run at all.
async fn start_for_attach(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    was: &Value,
) -> Result<Value> {
    let seat = seat_row(c, base, session, who).await;
    let provenance =
        seat.as_ref().and_then(|s| s["provenance"].as_str()).unwrap_or("-").to_string();
    let slot = seat.as_ref().and_then(|s| s["slot_id"].as_str()).map(str::to_string);
    let name = seat
        .as_ref()
        .and_then(|s| s["name"].as_str())
        .or_else(|| was["name"].as_str())
        .unwrap_or(who)
        .to_string();

    // `-` is a seat nothing has ever filled, which IS ours to fill.
    let ours = matches!(provenance.as_str(), "managed" | "-" | "");
    let Some(slot) = slot.filter(|_| ours) else {
        anyhow::bail!(
            "{name} has no process to attach to, and is not a seat this fleet starts ({}).\n\
             Read what it said with:  nexmesh transcript {session} {who}",
            match provenance.as_str() {
                "attached" => "it was started by hand, so start it the same way again",
                "external" => "it is an external participant, which this fleet does not run",
                _ => "there is no such seat here",
            }
        );
    };

    eprintln!("({name} has no process; starting one)");
    // No mode override: the seat's own mode stands. Attaching is not a
    // reason to turn a team that was declared headless into an interactive
    // one for ever.
    let v = post(c, format!("{base}/v1/slots/{slot}/spawn"), json!({})).await?;
    bail_on_error(&v)?;
    // Wait for something worth attaching to, rather than racing the
    // websocket upgrade against the spawn.
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let again = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await?;
        if !again["error"].is_string()
            && again["exited"].as_bool() != Some(true)
            && again["status"].as_str() != Some("disconnected")
        {
            return Ok(again);
        }
    }
    anyhow::bail!(
        "{name} was started but did not come up within 30s. What it printed is in:\n\
         nexmesh transcript {session} {who}"
    )
}

async fn attach(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    replay: usize,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
) -> Result<AttachEnd> {
    use tokio::io::AsyncWriteExt;

    // Resolve can fail outright for a seat nothing has ever filled, and
    // succeed-but-report-exited for one whose process is gone. Both mean the
    // same thing to somebody attaching: there is nothing to attach to yet.
    // A seat nothing has ever filled makes resolve answer 400, not a JSON
    // body with an error in it -- so the first attempt must be allowed to
    // fail without taking the command down with it. Both outcomes, the 400
    // and a body reporting `exited`, mean the same thing to somebody trying
    // to attach: nothing is there yet.
    let first = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await;
    let mut r = match &first {
        Ok(v)
            if !v["error"].is_string()
                && v["exited"].as_bool() != Some(true)
                && v["status"].as_str() != Some("disconnected") =>
        {
            v.clone()
        }
        // Either way: try to put something in the seat. If that is not ours
        // to do, `start_for_attach` says why -- and if resolving failed for
        // some OTHER reason, that error is what the operator should see, so
        // it is carried through.
        _ => {
            let was = first.as_ref().cloned().unwrap_or_else(|_| json!({}));
            match start_for_attach(base, c, session, who, &was).await {
                Ok(v) => v,
                Err(e) => match first {
                    // A real failure to reach the hub beats our guess at
                    // what the operator should do about an empty seat.
                    Err(orig) if !format!("{orig}").contains("no process behind it") => {
                        return Err(orig)
                    }
                    _ => return Err(e),
                },
            }
        }
    };
    bail_on_error(&r)?;
    // Alive, but with no terminal of its own and a dead adoption on record.
    // The node would reject this attach ("only its own shim reaches it"), so
    // give the pane its keyboard back first.
    if r["io_kind"].as_str().unwrap_or("").is_empty() {
        if let Some(term) = r["terminal_slot"].as_str().filter(|s| !s.is_empty()) {
            let term_name = r["terminal_name"].as_str().unwrap_or(term).to_string();
            let nm = r["name"].as_str().unwrap_or(who).to_string();
            r = readopt_terminal(base, c, session, who, &nm, term, &term_name).await?;
        }
    }
    let agent = r["agent_id"].as_str().context("no agent id returned")?.to_string();
    let name = r["name"].as_str().unwrap_or(who).to_string();

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
    // Whose scrollback is it? A seat in an adopted tmux pane has its
    // history inside tmux, and only tmux can scroll it; a seat on a plain
    // pty has none at all, and the wheel is translated into the page keys
    // a full-screen program uses. `Ctrl-F` does whichever applies.
    let in_tmux = !via.is_empty() || r["io_kind"].as_str() == Some("tmux");
    let seat_slot = r["slot_id"].as_str().unwrap_or_default().to_string();
    let vkeys = voice_keys();
    // Who has the mouse. The terminal, unless this machine's config says
    // otherwise: see `voice::mouse_to_viewer`.
    // In a tmux pane the node has already given tmux the wheel, so the
    // viewer starts in step with it: pressing Ctrl-F once gives the mouse
    // back rather than appearing to do nothing.
    let mut mouse_ours = voice::mouse_to_viewer(&read_console_toml());
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
            b.menu.push((voice::key_name(vkeys.read), "read it back".into()));
            b.menu.push((voice::key_name(vkeys.aloud), "read everything".into()));
        }
        if a.can_hear() && voice::listen_cmd(&a).is_some() {
            b.menu.push((voice::key_name(vkeys.listen), "hands free".into()));
        }
        b.menu.push((voice::key_name(vkeys.mouse), "plain mouse".into()));
        b.menu.push((voice::key_name(vkeys.link), "open a link".into()));
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
    // arrives -- and the terminal taken as it is found.
    {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(TERMINAL_PREPARE).await;
        if mouse_ours {
            let _ = out.write_all(termmode::MOUSE_ON).await;
        }
        let _ = out.flush().await;
    }
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
    // Set when the NODE went away rather than the agent: the caller then
    // waits for it and attaches again instead of ending the session.
    let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Whether the agent has taken the mouse for itself. While it has, the
    // viewer stops translating the wheel.
    let agent_mouse = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // What the agent has said, split into replies -- everything printed
    // since the operator last pressed Enter is its answer to that. Shared
    // with the key loop, which is where one of them is chosen to be read.
    // Whose screen this is: the CLI's own furniture -- its box, its spinner,
    // its status row -- is not part of anything it said, and which glyphs
    // those are depends on the CLI.
    let heard = std::sync::Arc::new(std::sync::Mutex::new(replies::Replies::watching(
        r["cli_id"].as_str().unwrap_or_default(),
    )));
    // When the last byte arrived, so "it has stopped talking" can be noticed
    // without polling the process.
    let quiet = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));

    // The agent's terminal -> ours, whichever way the bytes came.
    let reader = {
        let quit_tx = quit_tx.clone();
        let paused_flag = paused.clone();
        let heard = heard.clone();
        let quiet = quiet.clone();
        let agent_mouse = agent_mouse.clone();
        let mut rx = std::mem::replace(&mut wire.rx, tokio::sync::mpsc::channel(1).1);
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            // The agent sets the title to something about itself; this window
            // is saying which seat it shows, so those writes come out here
            // rather than being fought over.
            let mut strip = windowtitle::Strip::default();
            // Alternate scroll is taken out of the stream rather than merely
            // turned off at the start: tmux turns it back on in the middle,
            // which is why the wheel started sending arrow keys "sometimes".
            let mut modes = termmode::NoAltScroll::default();
            while let Some(msg) = rx.recv().await {
                match msg {
                    wire::In::Data(data) => {
                        let data = strip.pass(&data);
                        let data = modes.pass(&data);
                        agent_mouse
                            .store(modes.agent_wants_mouse, std::sync::atomic::Ordering::Relaxed);
                        // A copy of what went past, as words, so a reply can
                        // be read back or read out as it arrives.
                        if let Ok(mut seen) = heard.lock() {
                            // A line at a time, with the escape sequences
                            // out: what a person SAW, which is the only
                            // thing a reply can be told from furniture by.
                            seen.saw_lines(&voice::plain_lines(&data));
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
                    wire::In::Paused(why) => {
                        // Marked so the caller waits for the node and
                        // attaches again, rather than ending the session.
                        paused_flag.store(true, std::sync::atomic::Ordering::SeqCst);
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
    // Only consulted while hands-free is on.
    let mut listen_tick = tokio::time::interval(Duration::from_millis(300));
    listen_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Speaking and listening, if this machine has anything to do it with and
    // the fleet has a teammate that can. An attached terminal is a place you
    // type, so it is a place you should be able to talk to -- on a phone or a
    // tablet it is the only comfortable way to put a sentence in a prompt.
    let audio = voice::detect();
    let mut rec: Option<(std::process::Child, std::path::PathBuf)> = None;
    // Hands-free: a recorder that ends its own sentence. Separate from `rec`
    // because the two are started and stopped by different things -- one by a
    // key at each end, the other by silence.
    let mut listening: Option<(std::process::Child, std::path::PathBuf)> = None;
    // The player, while it plays: the same key again stops it.
    let playing: voice::Playing = Default::default();
    // Mouse reports arrive in the keyboard stream and are not keystrokes.
    let mut mouse = termmode::MouseReports::default();
    // And the right button is tmux's way out of the agent: see `RightClicks`.
    let mut rclicks = termmode::RightClicks::default();
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

    // Whatever was borrowed is given back on the way out, however this
    // ends: a detach that leaves tmux holding the mouse is the bug this
    // whole thing is about, one layer down.
    let give_back = (in_tmux && !seat_slot.is_empty()).then(|| {
        (c.clone(), base.to_string(), seat_slot.clone())
    });

    let reason = loop {
        tokio::select! {
            _ = repaint.tick(), if bar.is_some() => {
                if let Some(b) = &bar {
                    let mut out = tokio::io::stdout();
                    // The mode is filtered out of the agent's stream, and
                    // said again here anyway: anything that reaches the
                    // terminal by another route is undone within a quarter
                    // of a second rather than lasting until `reset`.
                    let _ = out.write_all(TERMINAL_PREPARE).await;
                    // The region as well as the modes: see `refresh`.
                    let _ = out.write_all(&b.refresh()).await;
                    let _ = out.flush().await;
                }
            }
            // A hands-free sentence ends when the recorder exits, which is
            // sox deciding the talking stopped. Checked on the same cheap
            // tick as everything else rather than with a thread of its own.
            _ = listen_tick.tick(), if bar.as_ref().map(|b| b.hands_free).unwrap_or(false) => {
                // Half duplex, on purpose. A microphone that is open while the
                // fleet is reading a reply aloud hears the reply, transcribes
                // it, and types the agent's own words back at it -- a loop
                // that costs money and looks like the machine talking to
                // itself. So speaking wins: the recorder is stopped, whatever
                // it had is thrown away rather than sent, and listening
                // resumes when the room is quiet again.
                let talking = playing.lock().map(|p| p.is_some()).unwrap_or(false);
                if talking {
                    if let Some((mut child, _)) = listening.take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    if let Some(b) = &mut bar {
                        b.recording = None;
                    }
                    continue;
                }
                if listening.is_none() {
                    match voice::listen(&audio, &scratch) {
                        Ok(r) => {
                            listening = Some(r);
                            if let Some(b) = &mut bar {
                                b.recording = Some(std::time::Instant::now());
                            }
                        }
                        Err(e) => {
                            let _ = said_tx.send(Err(e)).await;
                            if let Some(b) = &mut bar {
                                b.recording = None;
                                b.hands_free = false;
                            }
                        }
                    }
                    continue;
                }
                let done = listening.as_mut().and_then(|(child, path)| {
                    voice::listened(child, path).map(|bytes| (bytes, path.clone()))
                });
                if let Some((bytes, _)) = done {
                    listening = None;
                    if !bytes.is_empty() {
                        let (b, cl, tx) = (base.to_string(), c.clone(), said_tx.clone());
                        let lang = std::env::var("LANG").ok().and_then(|l| {
                            l.split(['.', '_']).next()
                                .filter(|s| s.len() == 2)
                                .map(|s| s.to_string())
                        });
                        tokio::spawn(async move {
                            let _ = tx
                                .send(voice::transcribe(&b, &cl, &bytes, lang.as_deref()).await)
                                .await;
                        });
                    }
                    // The next tick starts the next sentence, once nothing
                    // is being read aloud. Doing it here instead would open
                    // the microphone into the fleet's own reply.
                    if let Some(b) = &mut bar {
                        b.recording = None;
                    }
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
                            if let Err(e) = voice::speak(&bs, &cl, &a, &text, None, &dir, &pl).await {
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
                        // ONE LINE, ALWAYS.
                        //
                        // A transcription of more than one sentence comes
                        // back with NEWLINES in it -- the fleet's own
                        // transcriber answers a line per utterance -- and a
                        // newline typed at a CLI prompt is Enter. So the
                        // first sentence was submitted on its own, the rest
                        // arrived as separate turns, and what went in was
                        // never what was said. Enter stays the operator's,
                        // which is what this has always claimed to do.
                        let said = one_line(&text);
                        if wire.tx.send(wire::Out::Data(said.clone().into_bytes()))
                            .await
                            .is_err()
                        {
                            break "connection lost".to_string();
                        }
                        // AND SAID BACK. Dictation whose only evidence is
                        // whatever the CLI happens to echo is dictation you
                        // cannot tell from a microphone that is not working
                        // -- and a CLI drawing its prompt off the bottom of
                        // a phone screen echoes it where nobody can see.
                        if let Some(b) = &mut bar {
                            b.say_mark("heard", format!("heard: {}", clipped(&said, 48)));
                            // Hands-free is still on: the next sentence is
                            // already being recorded, so the row stays red.
                            if !b.hands_free {
                                b.recording = None;
                            }
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
                // The wheel first. While the agent is not using the mouse,
                // a turn is a page key -- the thing a full-screen program
                // actually scrolls with. When it IS using the mouse, every
                // report is handed over untouched.
                let bytes: Vec<u8> =
                    if !mouse_ours
                        || agent_mouse.load(std::sync::atomic::Ordering::Relaxed) {
                        // tmux has the mouse. Everything goes through
                        // EXCEPT the right button, whose menu offers to
                        // split the pane -- and a new pane in a held
                        // session is a shell on that machine. An agent that
                        // has asked for the mouse gets its own clicks: tmux
                        // shows no menu while that is true, so there is
                        // nothing to take away.
                        if in_tmux && !agent_mouse.load(std::sync::atomic::Ordering::Relaxed) {
                            rclicks.pass(&buf[..n])
                        } else {
                            buf[..n].to_vec()
                        }
                    } else {
                        let (keys, wheels) = mouse.read(&buf[..n]);
                        for w in wheels {
                            let key = match w {
                                termmode::Wheel::Up => PAGE_UP,
                                termmode::Wheel::Down => PAGE_DOWN,
                                // A click nobody asked for goes nowhere.
                                termmode::Wheel::Other => continue,
                            };
                            if wire.tx.send(wire::Out::Data(key.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        keys
                    };
                let n = bytes.len();
                buf[..n].copy_from_slice(&bytes);
                if n == 0 {
                    continue;
                }
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
                            // Back to the first page: the mode is entered
                            // to do one thing, and the keys for it should
                            // be where they were last time.
                            b.page = 0;
                        }
                        continue;
                    }
                    // In command mode THREE keys are nexmesh's and everything
                    // else -- text, Enter, arrows, every other control
                    // character -- still goes to the agent. So the mode can
                    // be left on while you carry on typing at the prompt.
                    // `?` while the mode is on prints the whole list. The
                    // row can only hold a few, so this is where the rest
                    // live -- and it was promised in the documentation long
                    // before it existed.
                    // `?` turns the row to the next few keys and wraps
                    // round. In the row, because that is where somebody
                    // asking "what are these" is already looking -- and
                    // printing a list would scroll the agent's own screen
                    // to say it.
                    if command && byte == b'?' {
                        if let Some(b) = &mut bar {
                            b.next_page();
                            let mut out = tokio::io::stdout();
                            let _ = out.write_all(&b.paint()).await;
                            let _ = out.flush().await;
                        }
                        continue;
                    }
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
                                        b.recording = None;
                                        b.hands_free = false;
                                    }
                                    None
                                }
                                None => match voice::start(&audio, &scratch) {
                                    Ok(r) => {
                                        if let Some(b) = &mut bar {
                                            b.recording = Some(std::time::Instant::now());
                                            b.hands_free = false;
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
                        voice::Act::Listen => {
                            // Hands-free: the recorder stops itself when you
                            // stop talking, so a conversation does not need a
                            // keypress at each end of every sentence.
                            if listening.is_some() {
                                if let Some((mut child, _)) = listening.take() {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                }
                                if let Some(b) = &mut bar {
                                    b.recording = None;
                                    b.hands_free = false;
                                }
                            } else {
                                match voice::listen(&audio, &scratch) {
                                    Ok(r) => {
                                        listening = Some(r);
                                        if let Some(b) = &mut bar {
                                            b.recording = Some(std::time::Instant::now());
                                            b.hands_free = true;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = said_tx.send(Err(e)).await;
                                    }
                                }
                            }
                        }
                        voice::Act::Mouse => {
                            // Hand the wheel to whatever actually holds the
                            // scrollback, and say which that is. Giving it
                            // back is the same key, because a mode you
                            // cannot leave by the way you entered it is a
                            // trap.
                            mouse_ours = !mouse_ours;
                            let mut out = tokio::io::stdout();
                            let said = if in_tmux && !seat_slot.is_empty() {
                                // tmux owns the history: ask the node to let
                                // tmux have the mouse for its own view.
                                let r = post(
                                    &c,
                                    format!(
                                        "{base}/v1/slots/{}/terminal-mouse",
                                        tui::urlish(&seat_slot)
                                    ),
                                    json!({ "on": mouse_ours }),
                                )
                                .await;
                                match r.map(|v| v.get("error").and_then(|e| e.as_str())
                                                 .map(|e| e.to_string())) {
                                    Ok(None) if mouse_ours =>
                                        "the wheel scrolls this pane now \u{2014} hold SHIFT to \
                                         select or click a link, Ctrl-F to give it back".into(),
                                    Ok(None) =>
                                        "the mouse is yours again: select, copy and click as \
                                         usual".to_string(),
                                    Ok(Some(e)) => { mouse_ours = !mouse_ours; e }
                                    Err(e) => { mouse_ours = !mouse_ours; format!("{e:#}") }
                                }
                            } else {
                                // A plain pty has no history to scroll, so
                                // the wheel becomes the keys a full-screen
                                // program scrolls with.
                                let _ = out
                                    .write_all(if mouse_ours {
                                        termmode::MOUSE_ON
                                    } else {
                                        termmode::MOUSE_OFF
                                    })
                                    .await;
                                if mouse_ours {
                                    "the wheel pages the agent now \u{2014} hold SHIFT to select \
                                     or click a link, Ctrl-F to give it back".to_string()
                                } else {
                                    "the mouse is yours again: select, copy and click as usual"
                                        .to_string()
                                }
                            };
                            let _ = out
                                .write_all(format!("\r\n[nexmesh] {said}\r\n").as_bytes())
                                .await;
                            let _ = out.flush().await;
                            if let Some(b) = &mut bar {
                                b.set_mark("wheel", mouse_ours);
                            }
                        }
                        voice::Act::Link => {
                            // A link on an agent's screen, opened HERE.
                            //
                            // An attach is a terminal inside tmux inside a
                            // viewer, and a click in one of those reaches no
                            // browser at all -- so the viewer finds the link
                            // and hands it to this machine's desktop, which
                            // is where the person is.
                            let url = heard.lock().ok().and_then(|h| h.newest_link());
                            let mut out = tokio::io::stdout();
                            match url {
                                Some(u) => {
                                    let said = match tui::open_in_browser(&u) {
                                        Ok(()) => format!("opening {u}"),
                                        Err(e) => format!("could not open {u}: {e}"),
                                    };
                                    let _ = out
                                        .write_all(format!("\r\n[nexmesh] {said}\r\n").as_bytes())
                                        .await;
                                }
                                None => {
                                    let _ = out
                                        .write_all(b"\r\n[nexmesh] no link on screen\r\n")
                                        .await;
                                }
                            }
                            let _ = out.flush().await;
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
                                            voice::speak(&bs, &cl, &a, &text, None, &dir, &pl).await
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
                        // The camera on this machine. It takes over the
                        // screen: a still taken blind is not using a camera,
                        // since you cannot tell whether the thing you meant
                        // to photograph is in shot.
                        voice::Act::Photo => {
                            let cam = camera::detect();
                            if !cam.ready() {
                                let mut o = tokio::io::stdout();
                                let _ = o
                                    .write_all(format!("\r\n[nexmesh] {}\r\n", cam.why_not()).as_bytes())
                                    .await;
                                let _ = o.flush().await;
                            } else {
                                // The bar's row is the overlay's too, and the
                                // scroll region would keep it out of it.
                                let mut o = tokio::io::stdout();
                                let _ = o.write_all(b"\x1b[r").await;
                                let _ = o.flush().await;
                                let took =
                                    camui::run(&mut keys, &cam, &scratch, cols, rows).await;
                                // The agent's screen is not ours to redraw,
                                // so the remote is resized by a row and back:
                                // that is what makes a full-screen program
                                // repaint itself.
                                if let Some(b) = &bar {
                                    let mut o = tokio::io::stdout();
                                    let _ = o.write_all(TERMINAL_PREPARE).await;
                                    let _ = o.write_all(&b.scroll_region()).await;
                                    let _ = o.write_all(b"\x1b[2J").await;
                                    let _ = o.write_all(&b.paint()).await;
                                    let _ = o.flush().await;
                                }
                                let agent_rows =
                                    bar.as_ref().map(|b| b.agent_rows()).unwrap_or(rows);
                                let _ = wire
                                    .tx
                                    .send(wire::Out::Resize {
                                        cols,
                                        rows: agent_rows.saturating_sub(1).max(2),
                                    })
                                    .await;
                                let _ = wire
                                    .tx
                                    .send(wire::Out::Resize { cols, rows: agent_rows })
                                    .await;
                                match took {
                                    Ok(camui::Took::File(path, what)) => {
                                        let (bs, cl, tx, sess, who) = (
                                            base.to_string(), c.clone(), said_tx.clone(),
                                            session.to_string(), name.to_string(),
                                        );
                                        let say = format!(
                                            "A {what} for you to look at, taken just now."
                                        );
                                        tokio::spawn(async move {
                                            let r = send_file(
                                                &bs, &cl, &sess, &path, Some(&who), Some(&say),
                                            )
                                            .await;
                                            let _ = std::fs::remove_file(&path);
                                            let _ = tx
                                                .send(r.map(|said| format!("\u{0001}{said}")))
                                                .await;
                                        });
                                    }
                                    Ok(camui::Took::Failed(why)) => {
                                        let _ = said_tx.send(Err(anyhow::anyhow!(why))).await;
                                    }
                                    Ok(camui::Took::Nothing) => {}
                                    Err(e) => {
                                        let _ = said_tx.send(Err(e)).await;
                                    }
                                }
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
    // Give the wheel back. A detach that leaves tmux holding the mouse is
    // the same bug one layer down: somebody's pane, where selection has
    // quietly stopped working and nothing said why.
    if mouse_ours {
        if let Some((cl, b, slot)) = give_back {
            let _ = post(&cl, format!("{b}/v1/slots/{}/terminal-mouse", tui::urlish(&slot)),
                         json!({ "on": false })).await;
        }
    }
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
    if paused.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(AttachEnd::Paused(reason));
    }
    Ok(AttachEnd::Ended(end_line(&reason)))
}

/// How an attach finished.
pub enum AttachEnd {
    /// Over: the agent exited, or the viewer detached.
    Ended(String),
    /// The NODE went away. The agent is very likely still running, so this
    /// is a pause and the viewer should come back when the node does.
    Paused(String),
}

/// Attach, and keep attaching across node restarts.
///
/// The operator asked for exactly this: "when we restart the hub, can we not
/// disconnect the agents attached to a terminal? (we can pause it showing in
/// terminal they are paused until the node comes back)".
///
/// Being dropped instead is not just an inconvenience -- it is what made a
/// seat get spawned a second time after a node restart, which put two CLIs
/// in one conversation.
pub(crate) async fn attach_until_ended(
    base: &str,
    c: &reqwest::Client,
    session: &str,
    who: &str,
    replay: usize,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
) -> Result<String> {
    loop {
        match attach(base, c, session, who, replay, ca_cert, identity).await? {
            AttachEnd::Ended(line) => return Ok(line),
            AttachEnd::Paused(why) => {
                eprintln!("\r\n[nexmesh] paused: {why}.");
                eprintln!("[nexmesh] the agent keeps running; this will reattach by itself. \
                           Ctrl-C to stop waiting.");
                // Wait for the node to come back AND for the seat to have a
                // process again -- attaching the instant the node registers
                // would race the respawn that follows it.
                let mut said = false;
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let v = get(c, format!("{base}/v1/sessions/{session}/resolve/{who}")).await;
                    let live = v
                        .as_ref()
                        .map(|v| {
                            !v["error"].is_string()
                                && v["exited"].as_bool() != Some(true)
                                && v["status"].as_str() != Some("disconnected")
                        })
                        .unwrap_or(false);
                    if live {
                        eprintln!("[nexmesh] the node is back; reattaching.");
                        break;
                    }
                    if !said {
                        said = true;
                        eprintln!("[nexmesh] still waiting for the node...");
                    }
                }
            }
        }
    }
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

        Cmd::TeamRules { session, from, clear } => {
            let text = if clear {
                Some(String::new())
            } else if let Some(path) = from.as_deref() {
                if path == "-" {
                    use std::io::{IsTerminal, Read};
                    if std::io::stdin().is_terminal() {
                        anyhow::bail!(
                            "`-` means read them from stdin, and stdin is your terminal. \
                             Pipe a file in, or pass its path instead."
                        );
                    }
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
                    Some(buf)
                } else {
                    Some(
                        std::fs::read_to_string(path)
                            .with_context(|| format!("reading {path}"))?,
                    )
                }
            } else {
                None
            };
            match text {
                // Nothing to set is a question, not a mistake.
                None => {
                    let v = get(&c, format!("{base}/v1/sessions/{session}")).await?;
                    bail_on_error(&v)?;
                    match v["instructions"].as_str() {
                        Some(t) if !t.trim().is_empty() => {
                            println!("{session} rules, told to everybody working here:");
                            for line in t.lines() {
                                println!("  {line}");
                            }
                        }
                        _ => println!(
                            "{session} has no standing rules. Set them with --from <file>."
                        ),
                    }
                }
                Some(t) => {
                    let v = post(
                        &c,
                        format!("{base}/v1/sessions/{session}/instructions"),
                        json!({ "instructions": t }),
                    )
                    .await?;
                    bail_on_error(&v)?;
                    println!("{}", v["detail"].as_str().unwrap_or("done"));
                }
            }
        }

        Cmd::Roles { role, skills } => {
            match role {
                None => {
                    let v = get(&c, format!("{base}/v1/roles")).await?;
                    bail_on_error(&v)?;
                    let empty = vec![];
                    // The skills go on their own line rather than in a
                    // column. Four of them is 54 characters, which either
                    // truncates or pushes every summary out of line, and a
                    // list of named things is no use abbreviated.
                    println!("{:<14} WHAT IT IS FOR", "ROLE");
                    for r in v["roles"].as_array().unwrap_or(&empty) {
                        let carries: Vec<&str> = r["skills"]
                            .as_array()
                            .unwrap_or(&empty)
                            .iter()
                            .filter_map(|s| s.as_str())
                            .collect();
                        println!(
                            "{:<14} {}",
                            r["id"].as_str().unwrap_or("?"),
                            r["summary"].as_str().unwrap_or("")
                        );
                        if !carries.is_empty() {
                            // Plain text: nothing else this tool prints uses
                            // escapes, and one place that does is one place
                            // that ignores NO_COLOR and lands in somebody's
                            // log as `[2m`.
                            println!("{:<14} carries: {}", "", carries.join(", "));
                        }
                    }
                    if let Some(dir) = v["local"].as_str() {
                        println!("\n  (this fleet's own templates: {dir})");
                    } else {
                        println!(
                            "\n  (the built-in templates; a fleet can keep its own in the \
                             hub's --prompts-dir)"
                        );
                    }
                    println!(
                        "  (apply one: nexmesh seat-prompt <team> <seat> --template <role>)"
                    );
                }
                Some(role) => {
                    // Skill names are lower-case words and dashes, so the
                    // only character needing care is the comma. Anything
                    // else is refused here rather than url-encoded: a skill
                    // name with a space in it is a typo, not a request.
                    let q = match &skills {
                        None => String::new(),
                        Some(s) => {
                            if s.chars().any(|c| {
                                !(c.is_ascii_alphanumeric() || c == '-' || c == ',' || c == '_')
                            }) {
                                anyhow::bail!(
                                    "skill names are words, dashes and commas: {s:?}"
                                );
                            }
                            format!("?skills={}", s.replace(',', "%2C"))
                        }
                    };
                    let v = get(&c, format!("{base}/v1/roles/{role}{q}")).await?;
                    bail_on_error(&v)?;
                    println!("{}", v["prompt"].as_str().unwrap_or(""));
                }
            }
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
                "{:<16} {:<14} {:<16} {:<11} {:<9} {:<9} {:<14} {:<11} {:<13} {:>7} {:<5} {:>8} {:>5} SUMMARY",
                "NAME", "SEAT", "CLI", "ROLE", "HOST", "KIND", "STATE", "NOW", "AGENT", "PID", "IO",
                "TOKENS", "CTX"
            );
            for a in &rows {
                let tin = a["input_tokens"].as_i64().unwrap_or(0);
                let tout = a["output_tokens"].as_i64().unwrap_or(0);
                let tokens = if tin + tout == 0 {
                    "-".to_string()
                } else {
                    format!("{}/{}", human(tin), human(tout))
                };
                // HOW FULL, not how much spent -- the column beside it is
                // the spend. A seat at 90% is a seat about to lose the
                // middle of its own conversation, which TOKENS cannot tell
                // you however large it gets.
                let ctx = match (
                    a["context_used_tokens"].as_i64(),
                    a["context_limit_tokens"].as_i64(),
                ) {
                    (Some(used), Some(limit)) if limit > 0 => {
                        format!("{}%", used.saturating_mul(100) / limit)
                    }
                    _ => "-".to_string(),
                };
                // WHAT ITS MODEL IS DOING NOW, beside the work it is on: a
                // turn running, idle, stopped on a question -- or held,
                // because its account cannot reach the model.
                let now = match (a["hold_reason"].as_str(), a["activity"].as_str()) {
                    (Some("quota"), _) => "HELD:quota".to_string(),
                    (Some(r), _) => format!("HELD:{r}"),
                    (None, Some("active")) => "thinking".to_string(),
                    (None, Some("blocked")) => "needs-you".to_string(),
                    (None, Some("idle")) => "idle".to_string(),
                    _ => "-".to_string(),
                };
                println!(
                    "{:<16} {:<14} {:<16} {:<11} {:<9} {:<9} {:<14} {:<11} {:<13} {:>7} {:<5} {:>8} {:>5} {}",
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
                    now,
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
                    ctx,
                    a["summary"].as_str().unwrap_or(""),
                );
            }
            println!(
                "\n  (TOKENS = input/output spent, CTX = how full its context is NOW;\n\
                 \x20  STATE is the work, NOW what its model is doing this moment\n\
                 \x20  (thinking = a turn is running; HELD = its account cannot reach\n\
                 \x20  the model), AGENT the process behind it;\n\
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

        Cmd::Run { cli, session, name, node, role, cwd, reuse, no_attach, dry_run } => {
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
                    reuse,
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
                attach_until_ended(&base, &c, &session, &who, replay,
                                   args.ca_cert.as_deref(), identity)
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

        Cmd::SeatRespawn { session, agent, off } => {
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/autorespawn {agent}{}", if off { " off" } else { "" }) }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }
        Cmd::Compact { session, agent } => {
            // Through the chat command, like `compact-at`: one implementation.
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/compact {agent}") }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }

        Cmd::CompactAt { session, agent, pct } => {
            // Through the chat command, like `seat-respawn`: one
            // implementation of the setting, one set of words explaining it,
            // and whatever is typed in the chat and whatever is run here
            // cannot drift apart.
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/compact-at {agent} {pct}") }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }

        Cmd::LoopGuard { session, agent, repeats } => {
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/loop-guard {agent} {repeats}") }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }

        Cmd::Flood { session, agent, per_min, per_pair, off } => {
            if !off && per_min.is_none() && per_pair.is_none() {
                anyhow::bail!(
                    "say --per-min, --per-pair, or --off. `nexmesh flood --help` \
                     explains why there are two numbers."
                );
            }
            // `-` is the chat command's way of saying "leave that one as it
            // is", which is what an omitted flag means here.
            let text = if off {
                "off".to_string()
            } else {
                format!(
                    "{} {}",
                    per_min.as_deref().unwrap_or("-"),
                    per_pair.as_deref().unwrap_or("-")
                )
            };
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/flood {agent} {}", text.trim()) }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }

        Cmd::SeatPrompt {
            session,
            agent,
            system,
            template,
            skills,
            skills_on,
            skills_off,
            instructions,
            append,
            replace,
            clear_system,
            clear_instructions,
        } => {
            // A seat id is an answer by itself; a name needs the team to say
            // which seat it means.
            let slot = if agent.starts_with("sl-") {
                agent.clone()
            } else {
                resolve_seat(&c, &base, &session, &agent).await?
            };
            // Only one of them can be stdin: a pipe is read once, and
            // silently handing the same text to both would be worse than
            // refusing.
            if system.as_deref() == Some("-") && instructions.as_deref() == Some("-") {
                anyhow::bail!(
                    "both --system and --instructions read stdin, and a pipe arrives once. \
                     Pipe one and pass the other as a file."
                );
            }
            let read_md = |arg: Option<&str>| -> Result<Option<String>> {
                let Some(path) = arg else { return Ok(None) };
                if path == "-" {
                    use std::io::{IsTerminal, Read};
                    if std::io::stdin().is_terminal() {
                        anyhow::bail!(
                            "`-` means read it from stdin, and stdin is your terminal. \
                             Pipe a file in, or pass its path instead."
                        );
                    }
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf).context("reading stdin")?;
                    return Ok(Some(buf));
                }
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {path}"))?;
                Ok(Some(text))
            };
            let mut body = serde_json::Map::new();
            let csv = |v: &Option<String>| -> Option<Vec<String>> {
                v.as_deref().map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
            };
            if clear_system {
                body.insert("system_prompt".into(), json!(""));
            } else if template.is_some() || skills.is_some() || !skills_on.is_empty()
                || !skills_off.is_empty()
            {
                // The hub renders it, not this: it holds the fleet's own
                // templates, and a brief rendered here would be whatever
                // THIS machine's copy says. With no --template, it
                // re-renders from the one the seat already has.
                if let Some(role) = template.as_deref() {
                    body.insert("template".into(), json!(role));
                }
                if let Some(list) = csv(&skills) {
                    body.insert("skills".into(), json!(list));
                }
                if !skills_on.is_empty() {
                    body.insert("skills_on".into(), json!(skills_on));
                }
                if !skills_off.is_empty() {
                    body.insert("skills_off".into(), json!(skills_off));
                }
            } else if let Some(text) = read_md(system.as_deref())? {
                body.insert("system_prompt".into(), json!(text));
            }
            if clear_instructions {
                body.insert("instructions".into(), json!(""));
            } else if let Some(text) = read_md(instructions.as_deref())? {
                body.insert("instructions".into(), json!(text));
            }
            if replace || append {
                body.insert(
                    "system_prompt_mode".into(),
                    json!(if replace { "replace" } else { "append" }),
                );
            }
            // Nothing to set is a question, not a mistake: show what is there.
            let v = if body.is_empty() {
                get(&c, format!("{base}/v1/slots/{slot}/prompt")).await?
            } else {
                post(&c, format!("{base}/v1/slots/{slot}/prompt"), Value::Object(body)).await?
            };
            bail_on_error(&v)?;
            // WHAT IT IS BUILT FROM, before the text itself: an operator
            // turning a skill on or off needs to see the selection, and a
            // dump of 92 lines of prose does not show it.
            if let Some(tpl) = v["template"].as_str() {
                let empty = vec![];
                let names = |k: &str| -> Vec<String> {
                    v[k].as_array()
                        .unwrap_or(&empty)
                        .iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                };
                let on = names("skills");
                let suggested = names("template_skills");
                println!("{agent} is built from the {tpl} template.");
                println!(
                    "  carries: {}",
                    if on.is_empty() { "nothing".to_string() } else { on.join(", ") }
                );
                let off: Vec<String> = names("skills_available")
                    .into_iter()
                    .filter(|k| !on.contains(k))
                    .map(|k| {
                        if suggested.contains(&k) {
                            format!("{k} (this role suggests it)")
                        } else {
                            k
                        }
                    })
                    .collect();
                if !off.is_empty() {
                    println!("  not carrying: {}", off.join(", "));
                }
                println!("  --skill <name> adds one, --no-skill <name> takes one away.\n");
            }
            let mode = v["system_prompt_mode"].as_str().unwrap_or("append");
            match v["system_prompt"].as_str() {
                Some(t) => {
                    let verb = if mode == "replace" { "instead of" } else { "on top of" };
                    println!(
                        "{agent} system prompt ({} lines, {verb} the CLI's own):",
                        t.lines().count()
                    );
                    for line in t.lines() {
                        println!("  {line}");
                    }
                }
                None => println!("{agent} has no system prompt of its own."),
            }
            match v["instructions"].as_str() {
                Some(t) => {
                    println!("\nTold to each new agent that takes the seat:");
                    for line in t.lines() {
                        println!("  {line}");
                    }
                }
                None => println!("\nNothing is said to a new agent on arrival."),
            }
            for note in v["takes_effect"].as_array().unwrap_or(&vec![]) {
                if let Some(s) = note.as_str() {
                    println!("\n{s}.");
                }
            }
        }
        Cmd::Cancel { session, id, unseen } => {
            if id.is_none() && !unseen {
                anyhow::bail!(
                    "say what to cancel: --id <n> for one message, or --unseen for every \
                     message in this team that nobody has seen yet"
                );
            }
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages/cancel"),
                json!({ "id": id, "all_unseen": unseen }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }
        Cmd::SeatApprove { session, agent, off } => {
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/messages"),
                json!({ "text": format!("/autoapprove {agent}{}", if off { " off" } else { "" }) }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }
        Cmd::Invite { session, agent, out } => {
            let v = post(
                &c,
                format!("{base}/v1/sessions/{session}/guests"),
                json!({ "agent": agent, "leave": out }),
            )
            .await?;
            bail_on_error(&v)?;
            println!("{}", v["said"].as_str().unwrap_or("done"));
        }
        Cmd::Send { session, text, to } => {
            let mut body = json!({ "text": text });
            if let Some(t) = to { body["to"] = json!(t); }
            let v = post(&c, format!("{base}/v1/sessions/{session}/messages"), body).await?;
            bail_on_error(&v)?;
            // A line starting with `/` is a COMMAND, not a message: the hub
            // ran it and said what happened. Printing "sent, queued for 0
            // seats" over the top of that -- which is what this did -- reads
            // as though the command failed when it worked.
            if v["command"].as_bool() == Some(true) {
                println!("{}", v["said"].as_str().unwrap_or("done"));
                return Ok(());
            }
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

    fn menu_bar(cols: u16) -> StatusBar {
        let mut b = bar(cols);
        b.command = true;
        b.menu.clear();
        for (k, what) in [("Ctrl-G", "say it"), ("Ctrl-O", "read it back"),
                          ("Ctrl-T", "read everything"), ("Ctrl-N", "hands free"),
                          ("Ctrl-F", "plain mouse"), ("Ctrl-B", "open a link"),
                          ("Ctrl-Y", "send a file"), ("Ctrl-P", "camera")] {
            b.menu.push((k.into(), what.into()));
        }
        b
    }

    /// A key with no label is useless. The row shows as many whole entries
    /// as fit, never a bare chord, and says which page of them this is.
    #[test]
    fn a_narrow_row_keeps_the_words_and_says_there_are_more() {
        let b = menu_bar(58);
        let line = b.line();
        assert!(line.contains("Ctrl-G say it"), "{line}");
        assert!(line.contains("? 1/"), "and where the rest are: {line}");
        assert!(line.chars().count() <= 58, "{} chars", line.chars().count());
        assert!(!line.contains("Ctrl-B  "), "a bare key with no label: {line}");
    }

    /// `?` walks the whole menu and comes back to the start, so nothing is
    /// unreachable and nobody is left on a page they cannot leave.
    #[test]
    fn the_question_mark_cycles_every_key_and_wraps() {
        let mut b = menu_bar(58);
        let mut seen: Vec<String> = Vec::new();
        let mut firsts: Vec<usize> = Vec::new();
        for _ in 0..12 {
            let line = b.line();
            for (k, what) in &b.menu {
                if line.contains(&format!("{k} {what}")) && !seen.contains(k) {
                    seen.push(k.clone());
                }
            }
            if firsts.contains(&b.page) {
                break;              // wrapped
            }
            firsts.push(b.page);
            b.next_page();
        }
        assert_eq!(seen.len(), b.menu.len(), "every key is reachable: saw {seen:?}");
        assert_eq!(b.page, 0, "and it comes back to the first page");
    }

    /// A row wide enough for all of them shows all of them, and says
    /// nothing about pages that do not exist.
    #[test]
    fn a_wide_row_needs_no_pages() {
        let b = menu_bar(200);
        let line = b.line();
        assert!(line.contains("Ctrl-P camera"), "{line}");
        assert!(!line.contains("?"), "nothing was dropped: {line}");
    }

    #[test]
    fn a_passing_state_replaces_itself_rather_than_repeating() {
        let mut b = bar(160);
        b.set_mark("aloud", true);
        assert_eq!(b.line().matches("aloud").count(), 1);
        b.set_mark("aloud", true);
        assert_eq!(b.line().matches("aloud").count(), 1, "pressed twice, said once");
        b.say_mark("reading", "reading 2 back".into());
        b.say_mark("reading", "reading 3 back".into());
        let line = b.line();
        assert!(line.contains("reading 3 back") && !line.contains("reading 2 back"), "{line}");
        b.set_mark("aloud", false);
        assert!(!b.line().contains("aloud"));
    }

    /// An open microphone has to be impossible to miss. A word among other
    /// words on a grey row is missable, and was missed: the row itself says
    /// it now, in the only red it is ever painted, with a counter that moves
    /// so you can see it is live rather than left over.
    #[test]
    fn recording_takes_the_whole_row_and_says_how_long() {
        let mut b = bar(120);
        b.keys.push(("Ctrl-]".into(), "detach".into()));
        assert!(!b.line().contains("RECORDING"));
        b.recording = Some(std::time::Instant::now());
        let line = b.line();
        assert!(line.contains("RECORDING"), "{line}");
        assert!(line.contains("0:00"), "a counter, from zero: {line}");
        assert!(line.contains("press again"), "and how to end it: {line}");
        assert!(String::from_utf8(b.paint()).unwrap().contains("\x1b[41m"), "red");

        // Hands-free says a different thing, because it ends differently.
        b.hands_free = true;
        let line = b.line();
        assert!(line.contains("LISTENING"), "{line}");
        assert!(line.contains("stops when you stop talking"), "{line}");

        b.recording = None;
        assert!(!b.line().contains("LISTENING"));
        assert!(!String::from_utf8(b.paint()).unwrap().contains("\x1b[41m"));
    }

    /// A question is the one thing that outranks it: it is on the row being
    /// typed into, and taking it away mid-answer loses what was typed.
    #[test]
    fn a_question_still_owns_the_row_while_recording() {
        let mut b = bar(120);
        b.recording = Some(std::time::Instant::now());
        b.ask = Some(("file".into(), "/tmp/x".into()));
        let line = b.line();
        assert!(line.contains("file: /tmp/x"), "{line}");
        assert!(!line.contains("RECORDING"), "{line}");
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

#[cfg(test)]
mod restore_tests {
    use super::TERMINAL_RESTORE;

    #[test]
    fn attaching_turns_off_the_mode_that_makes_the_wheel_send_arrows() {
        let s = String::from_utf8(super::TERMINAL_PREPARE.to_vec()).unwrap();
        // The symptom is a CLI saying "scroll wheel is sending arrow keys,
        // use PgUp/Down". The cause is alternate scroll, which xterm turns
        // on by default in the alternate screen -- where every full-screen
        // agent lives.
        assert!(s.contains("\x1b[?1007l"), "{s:?}");
        assert!(s.contains("\x1b[?2004l"), "bracketed paste is the agent's to ask for");
    }

    #[test]
    fn detaching_turns_off_everything_a_full_screen_cli_turns_on() {
        let s = String::from_utf8(TERMINAL_RESTORE.to_vec()).unwrap();
        // The one that actually bites: alternate scroll turns the wheel into
        // cursor keys, so a shell's history scrolls instead of the
        // scrollback, and only `reset` puts it right.
        assert!(s.contains("\x1b[?1007l"), "alternate scroll must be turned off");
        // The alternate screen, so the shell's own screen comes back.
        assert!(s.contains("\x1b[?1049l"));
        // Every mouse reporting mode a TUI might have enabled.
        for mode in ["1000", "1002", "1003", "1006", "1015"] {
            assert!(s.contains(&format!("\x1b[?{mode}l")), "mouse mode {mode} left on");
        }
        // Bracketed paste, the scroll region, the cursor, wrap, insert mode
        // and any leftover colour.
        assert!(s.contains("\x1b[?2004l"));
        assert!(s.contains("\x1b[r"));
        assert!(s.contains("\x1b[?25h"));
        assert!(s.contains("\x1b[?7h"));
        assert!(s.contains("\x1b[4l"));
        assert!(s.ends_with("\x1b[0m\x1b(B"), "attributes and charset come last: {s:?}");
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use clap::CommandFactory;

    /// EVERY COMMAND MUST BE IN THE GROUPED LISTING.
    ///
    /// The listing is written by hand, because the useful order is not
    /// alphabetical -- and a hand-written listing rots. It had: `camera`,
    /// `invite`, `seat-approve`, `seat-respawn`, `set-owns` and `upload`
    /// were all missing, so six commands existed and were documented
    /// nowhere a person would look.
    ///
    /// Now that clap's own list is gone from the template, a command missing
    /// from the listing is a command nobody can find. So this fails until it
    /// is added.
    #[test]
    fn the_grouped_listing_names_every_command() {
        let cmd = Args::command();
        let listing = cmd.get_after_help().map(|s| s.to_string()).unwrap_or_default();
        let missing: Vec<&str> = cmd
            .get_subcommands()
            .map(|s| s.get_name())
            // `help` is clap's own and is mentioned in the footer by name.
            .filter(|n| *n != "help")
            .filter(|n| {
                // A whole word, so `run` does not match `running` and
                // `resume` does not satisfy `remove-session`.
                !listing
                    .split(|c: char| !(c.is_alphanumeric() || c == '-'))
                    .any(|w| w == *n)
            })
            .collect();
        assert!(missing.is_empty(), "commands missing from the help listing: {missing:?}");
    }

    /// And the listing must not name commands that do not exist, which is
    /// the same rot in the other direction: a reader typing one gets an
    /// error from a tool that advertised it.
    #[test]
    fn the_grouped_listing_invents_no_commands() {
        let cmd = Args::command();
        let real: Vec<String> =
            cmd.get_subcommands().map(|s| s.get_name().to_string()).collect();
        let listing = cmd.get_after_help().map(|s| s.to_string()).unwrap_or_default();
        // Only the command columns, which all sit above EXAMPLES -- below it
        // the same shape is a shell line, and `man nexmesh` is a different
        // program rather than a command this tool invented.
        let columns = listing.split("EXAMPLES").next().unwrap_or_default();
        let claimed: Vec<String> = columns
            .lines()
            .filter_map(|l| {
                let t = l.strip_prefix("  ")?;
                if t.starts_with(' ') {
                    return None;
                }
                let name = t.split_whitespace().next()?;
                (name.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    && name.len() > 2
                    && !name.starts_with("nexmesh"))
                    .then(|| name.to_string())
            })
            .collect();
        let invented: Vec<&String> = claimed.iter().filter(|c| !real.contains(c)).collect();
        assert!(invented.is_empty(), "help lists commands that do not exist: {invented:?}");
    }
}

#[cfg(test)]
mod bar_refresh_tests {
    use super::*;

    fn b(cols: u16, rows: u16) -> StatusBar {
        StatusBar::new(
            "Nexmesh-dev".into(),
            "laptop".into(),
            "nexmesh".into(),
            String::new(),
            cols,
            rows,
        )
        .unwrap()
    }

    /// THE BAR USED TO SCROLL INTO THE CONTENT.
    ///
    /// The modes were re-asserted four times a second; the scroll region
    /// was set once at the start and again only on a resize. Anything in the
    /// agent's output that reset it -- `\x1b[r`, which a full-screen program
    /// emits on its way out, and tmux in several situations -- left it reset
    /// until the next resize, so plain output scrolled over this row and
    /// carried the bar up into the text.
    #[test]
    fn a_refresh_reasserts_the_scroll_region() {
        let out = String::from_utf8(b(80, 24).refresh()).unwrap();
        assert!(out.contains("\x1b[1;23r"), "the region must be re-sent: {out:?}");
    }

    /// And it must not move the agent's cursor doing it: DECSTBM homes the
    /// cursor on some terminals, which is why the region cannot be sent on
    /// the timer by itself.
    #[test]
    fn the_refresh_saves_and_restores_the_cursor_around_it() {
        let out = String::from_utf8(b(80, 24).refresh()).unwrap();
        // EXACTLY ONE PAIR. DECSC is not a stack: a terminal keeps one saved
        // cursor, so a second save inside the first overwrites it. refresh
        // used to wrap paint, which saved again -- and since the scroll
        // region between them homes the cursor, the inner save stored 1,1 and
        // both restores went there. The agent's cursor sat in the top-left
        // corner and typing appeared there, four times a second.
        assert_eq!(out.matches('\x07').count(), 0, "no bells: {out:?}");
        assert_eq!(out.matches("\x1b7").count(), 1, "exactly one cursor save: {out:?}");
        assert_eq!(out.matches("\x1b8").count(), 1, "exactly one cursor restore: {out:?}");
        assert!(out.starts_with("\x1b7"), "must open with a cursor save: {out:?}");
        assert!(out.ends_with("\x1b8"), "must close with a cursor restore: {out:?}");
        // The region change has to be INSIDE that pair, or restoring is no
        // use: the home would already have happened outside it.
        let save = out.find("\x1b7").unwrap();
        let region = out.find("\x1b[1;23r").unwrap();
        let restore = out.rfind("\x1b8").unwrap();
        assert!(save < region && region < restore, "region outside the pair: {out:?}");
    }

    /// The idle bar is the pale pink of the transgender flag. Stated as a
    /// test because a colour is the one thing nobody notices has regressed
    /// until they look at it.
    #[test]
    fn the_idle_bar_is_trans_flag_pink() {
        let out = String::from_utf8(b(80, 24).paint()).unwrap();
        assert!(out.contains("\x1b[48;2;247;168;184m"), "#f7a8b8 ground: {out:?}");
        assert!(out.contains("\x1b[38;2;26;22;24m"), "near-black text on it: {out:?}");
    }
}
