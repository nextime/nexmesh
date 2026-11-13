# nexmesh

Multi-agent orchestration for agent CLIs **across machines**.

## Credits

**nexmesh is a reimplementation of
[multiagents](https://github.com/zetbrush/multiagents) by Arman Andreasyan
([@zetbrush](https://github.com/zetbrush)).** That project worked out what a
team of agent CLIs actually needs in order to function as a team, and nexmesh
would not exist without it. If you want this on a single machine, use the
original — it is the more mature tool, and it is where these ideas come from.

The design nexmesh inherits from it, essentially unchanged:

- **The slot/peer split** — a *slot* is a seat on the team with a stable
  identity; an *agent* is whatever process is currently filling it. This is the
  idea that makes crash recovery and respawning coherent, and everything else
  here is built on it.
- **The task state machine** — `idle → working → done_pending_review →
  addressing_feedback → approved → released`, with reviewer roles
  self-approving, and agents unable to disconnect until the loop completes.
- **Delivery with acknowledgement** — a message counts as delivered only once
  it has actually been surfaced to the model, so a dropped hand-off is retried
  instead of lost.
- **Piggybacking delivery on MCP tool results** — prepending unread messages to
  an agent's next tool result, under a `[PENDING MESSAGES]` header. This is the
  trick that makes an arbitrary MCP-speaking CLI usable with no bespoke code,
  and it is the floor the whole plugin model here stands on.
- **The shared knowledge store** — a persistent key-value store of decisions,
  conventions and discoveries, on the diagnosis that context drift between
  agents is the main way these systems fail.
- **File locks plus ownership zones** — static zones for cheap separation,
  dynamic locks for genuinely shared files.
- **Driving a headless CLI through its stdin** — writing teammate messages into
  `claude -p --input-format stream-json` so they are read mid-turn rather than
  at the end of it, and using Codex's `turn/steer` for the same purpose.

Rediscovering any one of those would have taken weeks. The credit for the
system design belongs upstream; what follows is only about running it on more
than one machine.

## What nexmesh changes

multiagents is single-host by construction — a reasonable choice for its
purpose, and not a flaw. But a fleet breaks four of its assumptions, and none
of the four is a port away:

| Original | Problem across machines | Here |
|---|---|---|
| `process.kill(pid, 0)` on pids from the DB (`broker.ts:326,372,593,1658`) | Every node has a pid 4242. A hub probing or signalling a remote agent's pid hits an unrelated **local** process. | Identity is `(node_id, pid, start_ticks)`. The hub cannot signal anything; only the owning node can, after verifying the triple. |
| No auth anywhere on the broker HTTP API | The API spawns processes, so exposing the socket is RCE. | Per-node enrolment tokens, SHA-256 at rest, verified before any frame is honoured. The token decides identity, so a node can't claim another's history. |
| `Bun.spawn` + stdio pipes in the orchestrator | A pipe does not cross a machine boundary, so the driver must live where the CLI does. | A `node` daemon per machine owns spawning and driving. This component has no equivalent in the original. |
| Locks keyed `(session_id, file_path)` | `src/**` on machine-a is not `src/**` on machine-c. | Locks are keyed by **scope**: `node:<id>` for local work, `ws:<id>` for a tree several machines hold. |

Plus: its three bespoke CLI drivers become declarative descriptors, so adding
a CLI is a TOML file rather than a Rust change — and an agent no longer has to
be a CLI at all.

## Shape

```
          machine B (roaming)              machine A (always on)            any other machine
   ┌────────────────────┐      ┌─────────────────────┐      ┌──────────────────┐
   │ nexmesh (ctl)      │      │   nexmesh-hub       │      │  nexmesh-node    │
   │ nexmesh-node       │──────▶  SQLite: sessions,  ◀──────│  spawns + drives │
   │  spawns + drives   │  WS  │  slots, agents,     │  WS  │  local CLIs      │
   │  local CLIs        │ (out)│  messages, locks    │ (out)│                  │
   └────────────────────┘      └─────────────────────┘      └──────────────────┘
            │                            │
       claude / opencode / qwen      web dashboard
       each with nexmesh-mcpd
```

A node **always dials out** to the hub and holds one WebSocket; the hub never
connects to a node. Spawns, kills, message injection and interactive attach are
all multiplexed over that socket. That single choice is why a node works
unchanged on a roaming machine, behind NAT, or on a public host with no inbound
port — so starting with two machines does not paint us into a corner.

## Binaries

| Binary | Runs on | Owns |
|---|---|---|
| `nexmesh-hub` | one host | All state. Decides what runs where. **Cannot signal a process.** |
| `nexmesh-node` | every machine | Spawns local CLIs, drives them, reports health. The only component that may signal. |
| `nexmesh-mcpd` | per agent, stdio | The MCP shim the CLI loads. Talks to its local node over a unix socket; holds no credentials. |
| `nexmesh` | anywhere | Operator CLI. |
| `nexmesh-orch` | wherever you work | The orchestrator, as an MCP server, so **any** MCP-capable CLI can run the fleet. |

## Supporting any agent CLI

A CLI is a descriptor in `clis/`, not Rust. Delivery has three tiers:

- **`mcp_only`** — the universal floor. Any CLI that speaks MCP works with zero
  bespoke code: messages ride in on the next tool result. This is what makes
  "and any other we can support" true rather than aspirational.
- **built-in fast paths** — `stream_json` (Claude Code), `app_server` (Codex),
  `http_server` (opencode): mid-turn injection and exact token accounting.
- **`plugin`** — an external helper binary, for a CLI with its own quirks. Same
  guarantees as a built-in driver, in any language, with no nexmesh release in
  between.

Shipped and verified against the real binaries: `claude`, `opencode`, `qwen`,
`codex`, `gemini`. A malformed descriptor is skipped loudly rather than taking
the node down, and `cargo test` validates every shipped descriptor.

> **opencode is a special case worth exploiting.** It already has a client/server
> split of its own — `opencode serve --port N --hostname H` plus `opencode attach
> <url>`. For opencode, remote access needs no pty multiplexing from us at all,
> which is what driver kind `http_server` does.

## Writing a driver plugin

The built-in fast paths exist because particular CLIs speak particular
protocols. Every further CLI with a shape of its own would otherwise mean
another module in `nexmesh-node` — and a release of nexmesh before anyone
could use it. A plugin moves that work outside:

```toml
[driver]
kind = "plugin"
command = "my-cli-plugin"      # resolved on PATH, or in cli.extra_paths
args = []
```

It is an ordinary executable in any language, exchanging one JSON object per
line on stdin and stdout. The node sends it four kinds of frame and accepts
five:

| node → plugin | |
|---|---|
| `hello` | protocol version, agent, slot, cwd, the **prompt**, and a `resume` handle when restarting |
| `stdout` | a chunk of the CLI's output — not necessarily a whole line |
| `deliver` | teammate messages, both rendered and as envelopes |
| `exit` | the CLI has gone |

| plugin → node | |
|---|---|
| `stdin` | bytes to write to the CLI |
| `ack` | the message ids that reached the model |
| `tokens` | cumulative `input` / `output` / `cache_read` |
| `resume` | the CLI's own handle for continuing this conversation |
| `log` | a line for the node's log (stderr works too) |

That is the whole contract, and it buys a new CLI everything a built-in driver
has: mid-turn delivery, acknowledgement, token accounting and crash recovery.
`tests/fake-cli/quirkplugin` is a complete worked example in under a hundred
lines of Python, driving a CLI (`tests/fake-cli/fakequirk`) whose protocol
nexmesh knows nothing about.

**A plugin is a parser, not a privilege.** The node spawns the CLI, owns its
pid and remains the only component that may signal it; a plugin only ever sees
output and asks for input. It cannot kill an agent, spawn anything, or reach
another machine. Nor can it lie about delivery: an `ack` only retires message
ids that were actually handed to that agent, so inventing one marks nothing
delivered.

**A plugin that dies is not an outage.** Injection then fails, the hub puts
those messages back in its queue, and the agent collects them through MCP at
its next tool call — the universal floor. Slower, never silent. The agent
itself keeps running: its stdin is tied to the lifetime of the CLI, not of the
plugin.

## An agent does not have to be a CLI

The mistake would be letting "agent" mean "CLI process we spawned". A
participant is anything that can call team tools and receive messages, so the
tool dispatcher lives in the hub, transport-agnostic, behind several front
doors:

| Front door | For | Needs |
|---|---|---|
| `mcpd` stdio -> node -> hub | agent CLIs | a node on that machine |
| **MCP over HTTP** | **anything that already speaks MCP** | **a URL and a token** |
| participant HTTP API | a hermes agent, a Python daemon, a shell script | a URL and a token |
| node-supervised command | a program that is not an "agent CLI" | a descriptor |

An external participant joins with `POST /v1/participant/join`, calls the same
26 tools through the same dispatcher, and shows up in `list_peers` like anyone
else. It needs no node, no pty and no MCP implementation. Consequently
`agents.node_id`, `pid` and `proc_start_ticks` are all nullable, and such an
agent's liveness is a lease it renews -- the same mechanism as a node, for the
same reason: there is no process we can inspect.

Participant tokens are a **separate table** from node tokens on purpose: a
participant may call team tools for its own slot and nothing else. It can never
cause a process to be spawned on anybody's machine.

```bash
nexmesh-hub participant-token hermes --session se-1234abcd
# then, from the agent:
curl -H "authorization: Bearer $TOKEN" -d '{"name":"Hermes","role":"archivist","kind_hint":"hermes"}' \
     http://hub:7870/v1/participant/join
curl -H "authorization: Bearer $TOKEN" \
     -d '{"agent_id":"ag-...","tool":"send_message","args":{"to":"Max","text":"corpus indexed"}}' \
     http://hub:7870/v1/participant/tool
```

### The MCP front door

The HTTP API above asks a newcomer to implement a protocol that is ours alone.
But every agent runtime worth integrating already speaks MCP — so that was the
wrong way round. The hub therefore serves the *same* participant seat over MCP
itself, at one endpoint:

```bash
nexmesh-hub participant-token hermes --session se-1234abcd

claude mcp add --transport http team \
    "https://hub:7870/v1/mcp?name=Hermes&role=archivist" \
    --header "Authorization: Bearer $TOKEN"
```

That is the whole integration. No nexmesh client, no SDK, no node on the
machine: a token and a URL make anything that speaks MCP a teammate, with the
same 26 tools and the same delivery. Verified against both `claude` and
`opencode`, which each report the endpoint connected and negotiate MCP
`2025-03-26`.

The transport is **Streamable HTTP** — POST carries JSON-RPC and the reply
comes straight back, with an `Mcp-Session-Id` minted by `initialize`. Three
details are worth knowing, because each one is a decision rather than a
default:

- **An MCP client has no "join" call**, so `initialize` takes the seat, and
  per-client settings (`name`, `role`, `session`, `workspace`) ride on the URL
  — the same place a hosted MCP server takes them. A client that sends no name
  still has one: its own.
- **`initialize` is not a one-off.** Clients reconnect, and `claude mcp list`
  sends one just to check the server is alive. So it reuses the seat this
  token already holds rather than taking another; the first run against a real
  client left a new silent teammate behind every time, which is how that was
  found. `DELETE` releases the seat properly.
- **The ack waits for proof.** The stdio shim acks as soon as it has written a
  tool result, because its stdout is a pipe on the same machine. Here the
  result crosses a network, so the session remembers what it handed over and
  acks it when that client comes back — its return being the only evidence it
  arrived. A client that never returns has those messages reclaimed and
  redelivered. Later, or twice, but never silently lost.

There is no server-initiated stream, so `GET` is answered `405` with an
explanation rather than left to hang: teammate messages ride back on tool
results, which needs nothing the client does not already do. A session id is
useless without the token that opened it, and a cross-origin `POST` is
refused, because a browser on any page can reach a loopback address.

> Not to be confused with `nexmesh-orch`, which is the mirror image: that one
> makes *your* CLI the operator of the fleet, where this one makes an external
> agent a *member* of a team.

## Memory with a scope

The first version of shared memory was one flat table per session: exact-key
lookup only, last-writer-wins with no trace, and everything destroyed with the
session that learned it. Useful, and too small to lean on. Three things fixed
that.

**Who may see it.** Four scopes, resolved nearest-first on a read — `agent`,
then any `group` the reader belongs to, then `session`, then `global`:

```
recall {"search": "which hashing did we pick?"}

2 entr(ies):
- [decision] auth-choice (session): argon2id with a 64MB cost  -- by Iris
- [convention] style (global): tabs  -- by Scribe
```

A nearer scope **shadows** a wider one for the same key, so a group can
override a global default without editing it, and a cheap seat can be given a
private scratchpad while still reading the team's conventions. Agent scope is
private and group scope is members-only — and that is enforced by *building*
the query from the reader's scopes rather than filtering afterwards, so a
scope the reader cannot see is unreachable by any argument it passes.

**Who may write it.** `agent`, `group` and `session` are open to any teammate.
`global` outlives every session, so it needs the `memory-writer` capability: a
confused or experimental seat must not be able to leave something behind for
every future team. Group membership is operator-driven
(`POST /v1/sessions/:id/groups`), because membership *is* the access control —
an agent cannot put itself in a group.

**How it is found.** FTS5 over keys and values, because an agent that must
already know the key does not have memory, it has a filing cabinet. Queries
are quoted as phrases before they reach FTS, since agents type questions and a
stray `AND` or quote should be a search rather than a syntax error.

Overwrites keep the previous revision, with its author. Two agents writing one
key is normal in a parallel team, and the old table made that invisible:

```
memory_history {"key": "db"}
- rev 1 by Scribe: sqlite with WAL
```

Entries may carry a TTL, and `prune` reclaims them — a fact that was only true
for a while says so.

### Continuity across sessions

`slots.resume_handle` is cascade-deleted with its session, so yesterday's
conversation could never be picked up. Handles are now also kept against an
**identity** — the CLI, the workspace and the name, which is what makes
tomorrow's agent the same agent — in `agent_continuity`.

A seat also keeps its own rolling summary via `save_context`, and both are
used on launch:

```
[CONTINUING EARLIER WORK]
When you last worked here you wrote: rewrote the tokeniser; next is the parser
[END]
```

The summary matters independently of the handle: some CLIs cannot resume at
all, and a handle is opaque, so it cannot be read by a different CLI picking
the work up. One rehydrates the conversation, the other rehydrates the intent.

## A team of unequal agents

Agents differ. One seat is an expensive model that reasons well, another is
cheap and fast, a third does OCR and nothing else. nexmesh does not try to be
a scheduler — it never decides who is more capable — but it does carry the
facts that let a team work that way.

**Which model is in a seat.** `slots.model` is what the seat was *asked* for
and survives a respawn; `agents.model` is what the running agent *reported*,
which is the truth, because a CLI may substitute or fall back. Without the
second, the token counts next to it are unreadable: 40k tokens means nothing
until you know whether an expensive or a cheap model spent them. Each driver
reports it from its own protocol, verified rather than assumed:

| CLI | asked via | reported from |
|---|---|---|
| claude, qwen, gemini | `model_args` in the descriptor (`--model {model}`) | `system`/`init` carries `model` |
| codex | `model` on `thread/start` / `thread/resume` | `model`, a required field on both responses |
| opencode | `ModelRef {providerID, id}` on `POST /api/session` | `Session.model` |
| a plugin | the `model` field in its `hello` | the plugin's own `model` frame |

**What a seat can do.** `capabilities` is a list of free-form labels —
`strong`, `cheap`, `vision`, `ocr` — lower-cased and de-duplicated so that
addressing them is not a spelling lottery. A list rather than one tier,
because capability is not a single axis. `list_peers` shows them and filters
on them, and `send_message` resolves a label the way it resolves a name:

```
- Iris [code reviewer] on machine-b (cli, running) state=working model=anthropic/claude-opus-4 can=strong,vision
- Hermes [archivist] on external (external, running) state=idle can=ocr
```

Name and role win over a capability, and a label several teammates hold is
**refused rather than guessed** — it tells you to use `list_peers` and choose.
Sending review feedback to the wrong agent is worse than a retry.

**Handing work up.** `escalate {to, reason, task?}` is the mirror of
`submit_feedback`: that one hands work back down for rework, this one hands it
up to someone better equipped. With a mixed team the cheap seat reaching its
limit is the normal case, not an error, and it needs a move that is neither
"done" nor "failed". Both ends move — the escalator to `escalated`, the
receiver to `working` — because leaving either alone is how a task is silently
dropped between two agents that each think the other has it. The reason and
the task travel as a `handoff` message.

**A ceiling on the bill.** A session may carry `token_budget`, and reaching it
**pauses** the session:

```bash
nexmesh new-session audit --token-budget 2000000
# ... later, in the hub log:
#   WARN token budget reached; pausing the session  spent=2004311 budget=2000000
```

Pausing rather than killing, for three reasons: it is reversible by an
operator who can look at what was bought, every agent already understands
pause (it withholds work rather than yanking a process), and terminating
agents mid-edit destroys work to save money, which is the wrong trade at
almost any budget. The ceiling is per session, not per agent — with a mixed
team the number worth capping is what the task cost, not what one seat cost.
Cache reads are excluded, because charging for them would pause a session for
being efficient.

The crossing is stamped, so a session an operator deliberately resumes is not
re-paused by the next token report. Changing the budget
(`POST /v1/sessions/:id/budget`) re-arms the guard — otherwise raising a
budget would have no effect.

**What this deliberately is not.** There is no automatic routing by ability,
no model tiering the hub enforces, and no agent the hub decides is better. An
agent asks for a capability and the hub routes it, exactly as it routes every
other message here. Composing the team stays yours.

## Reaching any CLI from any machine

Two independent axes, deliberately:

- **Provenance.** `managed` = the hub asked a node to spawn it. `attached` = a
  human ran `claude` or `opencode` by hand, anywhere, and its `mcpd` registered
  it into a slot. An attached agent is a full team member — it can message
  peers, take locks and be reviewed.
- **Mode.** `headless` gives the best injection and token accounting but is a
  protocol, not a terminal. `interactive` runs the CLI's TUI under a pty on the
  node, so it can be viewed *and taken over* from any machine. Every shipped
  descriptor has at least one attachable mode; a test enforces it.

Messages are addressed **slot to slot**, not process to process, so a message
queued for a slot is still there for whatever agent next occupies it, on
whatever machine. The original keys the inbox on the peer and has to graft it
onto a respawned agent.

## Running the fleet from whatever CLI you use

Driving a fleet should not require one particular CLI. `nexmesh-orch` is a
plain MCP stdio server, so Claude Code, Codex, opencode, qwen, Gemini — or a
program of your own — can load it and spin up teams from the session you are
already in.

```bash
nexmesh install-orchestrator --cli claude   --token <operator-token>
nexmesh install-orchestrator --cli opencode --token <operator-token>
# then, in that CLI:
#   "list my nodes"
#   "start a team on machine-a: a claude engineer to build the login flow and an
#    opencode reviewer; the repo is at /srv/app"
```

Its tools are the operator's verbs, not an agent's: `list_nodes`,
`create_team`, `add_agent`, `team_status`, `read_log`, `message_team`,
`adopt_terminal`, `read_transcript`, `list_sessions`. `create_team` is one
call — session, workspace, plan, and every agent placed and started.

Unlike `nexmesh-mcpd`, which deliberately holds no credentials, this one holds
an **operator token**: its tools start processes on every machine in the fleet.
Install it only into a CLI you trust with that.

`install-orchestrator` uses each CLI's own `mcp add` where one exists, rather
than editing a config format that may change underneath us; for opencode, whose
`mcp add` is interactive and so unscriptable, it writes the documented config
instead. Either way the token never appears in the output, including `--dry-run`.

## The web dashboard

```bash
nexmesh-hub --listen 127.0.0.1:7870 serve   # then open http://127.0.0.1:7870/
```

Served by the hub itself rather than a second daemon on a second port: one
process to run, one address to reach, one credential to hold. Tabs for agents,
the conversation, the plan, shared knowledge, files and the fleet, with stat
tiles for what is working, what awaits review, what nobody has seen, and what
is flapping.

It refreshes once a second and pauses when the tab is hidden. The page itself
carries no data, so it is served without a token and asks for one in the
browser, keeping it in `localStorage`; the snapshot it fetches sits behind the
same operator middleware as everything else.

There are **no external fonts, scripts or stylesheets**. A fleet hub may sit on
a network with no route to the internet, and a dashboard that needs a CDN to
render is a dashboard that fails exactly when you need it.

## Watching the team

Two different things, deliberately separate:

```bash
nexmesh log se-1234abcd --follow        # the conversation BETWEEN agents
nexmesh transcript se-1234abcd Max      # what one agent's process printed
```

```
TIME               FROM    TO           MESSAGE
21:05:40         Hermes -> Max          Prior decision: JWT with refresh rotation.
21:05:40            Max -> Iris         Login flow is in. Please review.
21:05:40            Max -> (team)       [handoff] Max signalled done and is awaiting review.
21:05:41           Iris -> Max          [feedback] (ACTION REQUIRED): Refresh tokens aren't rotated on reuse.
21:05:41            Max -> Iris         Fixed: rotating on reuse now.
21:05:41 !         Iris -> Max          [approval] Iris approved your work.
21:05:41 !     operator -> (team)       [system] nice work both, ship it

  ( ! = recipient has not seen it yet, ~ = delivery in flight )
```

That marker column is the useful part. Because delivery is acked separately
from sending, the log distinguishes *sent* from *actually seen by the
recipient's model* — so "why is this team stuck?" usually has a visible answer:
somebody is sitting on an unread message. `--follow` streams forward by message
id, so it never repeats or skips one.

`transcript` is the agent's own stdout and stderr, which the node streams and
the hub persists per agent. It survives the process exiting, which is the point:
an agent's errors are most interesting precisely when it has died.

## Attaching to an agent's terminal

```bash
nexmesh attach se-1234abcd Max      # Ctrl-] to detach
```

An agent launched in `interactive` mode runs under a **pty** on its node, so it
can be watched *and typed into* from any machine. A `headless` agent is a
protocol on a pipe, not a terminal — you can watch it, but there is nothing to
take over, and the client says so rather than swallowing your keystrokes.

The hub is only a relay: it never touches the pty, because only the agent's own
node can. An attach is three hops — your terminal → hub → the node's existing
*outbound* socket — which is why it works for a node behind NAT with no inbound
port. Raw terminal bytes travel as binary frames and control messages as JSON,
so there is no encoding on the hot path.

- Scrollback is replayed on attach, so you do not start at a blank screen.
- Your window size is sent on connect and on every `SIGWINCH`.
- Several people can attach to the same agent and all see the same session.
- Detaching leaves the agent working. Ctrl-C belongs to the agent — that is the
  point of a takeover — so Ctrl-] is the detach key, as in telnet.
- The terminal is restored on any exit path, including a panic.

## Getting a message to a working agent

Delivery has two tiers, and the difference is latency, not reliability.

**The floor** works for every agent: unread messages ride back on its next MCP
tool result. Always correct, but an agent deep in a long edit may not call a
tool for minutes.

**The driver fast path** writes into the CLI's own input stream, so a message
is read at the next tool-call boundary instead — seconds, not minutes. For
Claude Code that means `stream-json` on stdin, and the acknowledgement is not a
guess: `--replay-user-messages` makes the CLI echo the message back, and only
that echo marks it delivered. If the agent dies before echoing, the message
returns to the queue for whoever fills the slot next.

Everything in that path was verified against claude 2.1.322 rather than
assumed, and two things were not what you would guess:

- `--output-format=stream-json` **requires `--verbose`**, or the CLI exits
  immediately. The descriptor shipped here would have failed on first use.
- Custom fields added to an input message are **stripped** from the replay, so
  acknowledgements correlate on the message text, not an id you attach.

The same stream carries token usage (`assistant` events meter each API call,
the turn's `result` gives totals) and the session id from `system`/`init`,
which becomes the `--resume` handle.

**opencode is the best of the three to drive**, because it already has a
client/server split. `opencode serve` exposes a documented API (OpenAPI at
`/doc`) whose prompt endpoint takes `delivery: "steer" | "queue"` — mid-turn
steering is a first-class operation there, not something bolted on. So that
driver holds no pty and parses no stdio protocol: it starts the server, reads
the URL it prints, and makes HTTP calls. Verified against opencode 1.19.5 by
reading `/doc` and calling a running server.

Two details that matter:

- The server is given a **per-agent password** (`OPENCODE_SERVER_PASSWORD`,
  HTTP Basic, username `opencode`). Without one it says it is unsecured, and
  anyone with a shell on that machine could drive the agent through its
  loopback port.
- The first line it prints is that warning, not the URL. A parser that grabs
  the first line, or the first thing resembling a URL, starts the agent against
  nothing.

It also gives something neither stdio-driven CLI can: because `opencode attach
<url>` exists, a **headless** opencode agent can still be opened as a real TUI.
Attach to one and the node starts a terminal client against its server just for
that viewer; detach and the client goes away while the agent carries on. Best
delivery and exact token accounting, *and* take-over on demand.

**Codex's `app-server`** is the other fast path, and it works differently
enough to be worth describing. It is JSON-RPC over stdio with threads and
turns, so there is no "write a line of text": a message is either steered into
the turn that is already running (`turn/steer`) or starts a new one
(`turn/start`). Every shape came from `codex app-server generate-json-schema
--out` plus a live handshake against codex-cli 0.167.2, and three details would
have been easy to get wrong:

- **Notifications omit the `jsonrpc` field** and add `emittedAtMs`. A parser
  that insists on `jsonrpc: "2.0"` silently drops every one of them, including
  all token usage and turn state.
- The server **asks the client questions and blocks until answered**. An
  unattended agent has nobody to ask, so command and file-change approvals are
  accepted and anything needing a human's words is declined — leaving them
  unanswered wedges the turn.
- The approval vocabularies **differ per method**: `execCommandApproval` wants
  `approved`, while `item/commandExecution/requestApproval` wants `accept` /
  `acceptForSession`. The wrong word for the method leaves the agent stuck.

The resume handle is `thread.id`, replayed through `thread/resume`. Token usage
comes from `thread/tokenUsage/updated` (`inputTokens`, `outputTokens`,
`cachedInputTokens`).

One honesty note on acknowledgement: Claude's replay echo proves a message
reached the model, whereas a successful `turn/steer` only proves the Codex
runtime accepted it. That is the strongest signal the protocol offers, and the
code says so where it acks.

## Crash recovery

A managed agent that dies with work outstanding is respawned on a placed node —
possibly a different one, since recovery goes through the same placement path
as a first launch — resuming its previous session and carrying a handoff note
telling it to read the plan and knowledge before redoing anything.

A slot that has *signalled done* is deliberately left alone unless feedback is
waiting for it; restarting an agent with nothing to do only burns tokens. And
after three crashes in five minutes a slot is marked `flapping` and left dead,
with a message to the team saying so, rather than hammering a broken agent
forever.

## Adopting a session that is already running

```bash
nexmesh adopt se-1234abcd --node machine-a --tmux work --name Legacy
nexmesh attach se-1234abcd Legacy
```

For a terminal you started yourself and want to reach from elsewhere. The
obvious implementation is `pipe-pane` plus `send-keys`; this does something
better. tmux is built for several clients at once, so the node runs
`tmux new-session -t <target>` under a pty it owns. That is a *grouped*
session: it shares the windows but keeps its own size, so a viewer on a small
terminal does not shrink the session under your hands the way plain
`attach-session` would. Your own terminal stays attached throughout, and the
whole existing attach path — scrollback, resize, several viewers, takeover —
works unchanged.

Being honest about the limit: adoption gives you the **terminal**, not a team
member. That session has no MCP shim, so it cannot call team tools or receive
messages. Configure `nexmesh-mcpd` in the CLI running inside the pane and it
joins as an `attached` agent too; the two are orthogonal and combine.

There is deliberately **no send-keys message delivery**. Typing a teammate's
message into a pane that happens to be sitting at a shell prompt would execute
it. Delivery belongs to the MCP path, which knows it is talking to an agent.

## Pausing and releasing

Two different things, often confused.

**Pause** is reversible and leaves the agent alive. Work stops reaching it and
it is told to stand by; messages queue and arrive when it resumes. A turn
already in flight finishes — nothing here reaches into a model mid-thought — so
pause means *take on nothing further*, not *stop this instant*. A paused agent
is also not respawned if it dies: bringing back something you deliberately
stopped would be the opposite of what you asked for.

**Release** is the end of the review loop. Agents deliberately cannot
disconnect on their own, which is what stops one wandering off before its work
is reviewed; release lifts that hold. Its input is closed — which is how a
stdio-driven CLI learns there is no more work and exits of its own accord — and
it is terminated only if still there after a grace period. Killing it outright
would lose whatever it was in the middle of writing.

```bash
nexmesh pause   se-1234abcd Max --reason "checking something"
nexmesh resume  se-1234abcd Max
nexmesh pause   se-1234abcd            # the whole team
nexmesh release se-1234abcd Max --note "nice work"
nexmesh release se-1234abcd            # everyone; the session's work is done
```

From a CLI, the orchestrator offers `pause_agents`, `resume_agents` and
`release_agents`.

## Shared workspaces

A `node_local` workspace is files on one machine. A `repo` workspace is the
same tree checked out on several, and that is where work has to actually
travel between machines.

```bash
nexmesh sync se-1234abcd --op status     # what is unshared, and what is unpulled
nexmesh sync se-1234abcd --op publish    # push everyone's work
nexmesh sync se-1234abcd --op refresh    # pull it everywhere
```

Agents get `publish_work`, `sync_workspace` and `workspace_status`, and two
moments happen without being asked: a machine is given a checkout **before its
agent starts**, and `signal_done` **publishes automatically** — finishing is
exactly when teammates need to see the work, and an agent should not have to
remember a second call. A publish also messages the team, so nobody has to
guess that there is something to pull.

Three decisions worth stating:

- **Git is the transport; the declared remote is the authority.** The hub
  coordinates but is not a git server. Making it one would mean implementing
  smart HTTP and holding everybody's code, when every machine already has git
  and a remote it can reach.
- **Ownership zones are the conflict-avoidance strategy.** Agents are given
  disjoint globs, so in normal running two of them never touch the same file.
- **A conflict is reported, never resolved.** Guessing at a merge would
  silently destroy somebody's work. The publish is rolled back, the agent is
  told which files collided, and its own files are left exactly as they were —
  there is a test asserting precisely that.

`sync = "shared_fs"` means the filesystem already does this and nexmesh stays
out of the way.

## What attach can and cannot reach

Attach relays a pty **the node created**. That distinction decides what works:

| The agent | Attach gives you |
|---|---|
| launched by nexmesh, `mode = "interactive"` | the real TUI: watch **and type**. Several viewers at once. |
| launched by nexmesh, `mode = "headless"` | view-only — it is a protocol on a pipe, not a terminal |
| joined by itself via `mcpd` (an *attached* agent) | nothing. It is a full team member for messages, tools and review, but its terminal belongs to whoever started it — unless you also `adopt` its tmux session |
| adopted with `nexmesh adopt` | the real terminal: watch **and type**, alongside your own client |
| headless on a CLI that has its own client (opencode) | a real TUI, started on demand for each viewer and closed when they leave |
| an external participant (a hermes agent) | nothing — there is no terminal at all |

So a `claude` agent that nexmesh launched on machine-a in interactive mode is fully
drivable from machine B. A `claude` you started yourself in a bare ssh session
on machine-a is **not**: that pty belongs to your shell. Start it inside tmux and
`nexmesh adopt` reaches it — see above.

## Security

**Authenticated: yes, three separate credentials, none of which substitutes for
another.** Only SHA-256 hashes are stored.

| Credential | Mint with | Can |
|---|---|---|
| node | `nexmesh-hub enrol <node>` | run agents on that one machine. The *token* decides which node it is, so a node cannot claim another machine's identity or slot history |
| operator | `nexmesh-hub operator-token <label>` | everything: create sessions, spawn on any node, attach to any terminal. Treat it like an ssh key |
| participant | `nexmesh-hub participant-token <label>` | call team tools for its own slot. **Never** spawn a process |

Operator routes sit behind a middleware as a group, so a route added later is
protected by default rather than by remembering. Revoking the last operator
token does **not** turn authentication off — that would open the fleet at the
exact moment you are reacting to a leak.

**Pairing: no certificate wrangling.** Copying key material around by hand is
how this goes wrong, so a machine or a person joins with a one-time code from
the dashboard's **fleet** tab (or `POST /v1/pair/codes`):

```bash
# shown in the dashboard, to run on the new machine:
nexmesh-node pair --hub https://machine-a:7871 --code JVJB-FQXS-4N8C-DCR7 \
    --connect wss://machine-a:7870
nexmesh-node                      # no flags: it reads the identity it was given

# and for a person:
nexmesh pair --pair-hub https://machine-a:7871 --code ... --connect https://machine-a:7870
nexmesh nodes                     # likewise
```

The key pair is generated **on the joining machine** and the private key never
travels: only a certificate request goes to the hub, which signs it and sends
back a certificate, the CA certificate and a token. The hub never sees the key,
and nor does anyone watching the wire.

The code is the whole boundary for that moment, so: bound in advance to one
identity, single-use, expiring in minutes, stored only as a hash, shown exactly
once, and revocable from the dashboard. A requester does not get to choose the
name it is issued — the hub replaces the subject with the one the code was
bound to. Every failure gives the same message, so nobody can probe which codes
exist.

A machine being paired has no certificate yet, so it cannot use the mutual-TLS
port. Pairing therefore gets its own listener (`--pair-listen`), serving only
the claim endpoint, and it is off unless you ask for it.

**Mutual TLS: yes.** A token is a bearer secret — copy it and you are that
node. A client certificate adds something that cannot be lifted out of a log,
an environment listing or a config file, so reaching the hub needs both.

```bash
nexmesh-hub pki init --host machine-a --host 10.0.0.2   # CA + the hub's own cert
nexmesh-hub pki issue machine-a --out /tmp/machine-a    # by hand, if you prefer
nexmesh-hub --tls-cert ~/.nexmesh/pki/server-cert.pem \
            --tls-key  ~/.nexmesh/pki/server-key.pem \
            --client-ca ~/.nexmesh/pki/ca-cert.pem --listen 0.0.0.0:7870 serve

nexmesh-node --hub wss://machine-a:7870 --ca-cert ca-cert.pem \
             --client-cert machine-a-cert.pem --client-key machine-a-key.pem
```

The CA key never leaves the hub; a node gets only `ca-cert.pem` plus its own
certificate and key, all written 0600. With `--client-ca` set, a correct token
with no certificate **cannot complete a TLS handshake at all** — it is refused
before anything reads the token. `--client-ca` without `--tls-cert` is a
startup error rather than a silently ignored flag.

The two credentials answer different questions, and both are checked:

- the **certificate** says *you belong to this fleet* — the CA is the boundary
- the **token** says *which node or operator you are*

**And the two are bound together.** Checked separately they leave a gap: every
machine in the fleet holds a certificate the CA issued, so a token lifted from
one machine could be presented from another, and an operator's certificate
could carry a node's token. So the hub also requires that they agree — the
common name on the certificate must be the identity the token was issued to:

```
$ nexmesh --hub https://machine-a:7870 nodes     # the operator's token, somebody else's certificate
Error: request failed (403 Forbidden): this connection's certificate was issued to
'intruder', but the credential claims to be 'operator'. A certificate and a token
must belong to the same identity.
```

All three credential types are bound, each where it is checked: an operator on
the route group, a node in its first frame — before it is registered, so one
machine cannot take another's place in the fleet even holding its token — and
an external participant on its own route group. An absent or invalid token
still gets its own clearer error; binding decides *who*, not *whether*.

Pairing issues both under one name, so a machine paired with `nexmesh pair`
satisfies this without you doing anything. A fleet whose certificates and
tokens were minted separately, under names that do not match, can start the hub
with `--no-bind-identity` while they are brought into line; the hub says in its
log that the check is off. With no `--client-ca` there is no certificate to
compare against, and the token stands alone.

A certificate presented without a common name is refused rather than waved
through, and the comparison is exact — not a prefix, not case-insensitive.

**Encrypted: yes, natively.**

```bash
nexmesh-hub gen-cert --host machine-a --host 10.0.0.2     # self-signed, key mode 0600
nexmesh-hub --tls-cert ~/.nexmesh/cert.pem --tls-key ~/.nexmesh/key.pem \
            --listen 0.0.0.0:7870 serve
# each node and operator trusts that certificate explicitly
nexmesh-node --hub wss://machine-a:7870 --ca-cert /etc/nexmesh/hub-cert.pem
export NEXMESH_OPERATOR_TOKEN=...  NEXMESH_CA_CERT=/etc/nexmesh/hub-cert.pem
```

Public CAs still work, so a hub behind a real certificate needs no `--ca-cert`.
There is deliberately **no "skip verification" switch**: an attacker who can
redirect the connection is precisely who the token must be protected from.

Two refusals rather than warnings, because both failure modes are silent:

- A node **will not send its enrolment token in cleartext to a remote host**.
  That token is worth a shell on the machine. Loopback is fine, `wss://` is
  fine; anything else needs an explicit `--insecure` for a path you know is
  already encrypted, such as your WireGuard or OpenVPN tunnel.
- The hub **will not serve an unauthenticated operator API off-box**. Binding a
  non-loopback address with no operator token minted is a startup error telling
  you to mint one, bind loopback, or pass `--allow-anonymous` if something in
  front already authenticates.

**Revocable: yes.** A compromised certificate does not mean re-issuing the CA.
`nexmesh revoke <name>` does three things at once, because any one of them
alone leaves a door open: the serial goes onto a CRL the hub signs and checks
*during the handshake*, the running hub reloads its TLS configuration so this
takes effect without a restart, and that name's token is revoked and its live
connection dropped. `nexmesh certs` lists what has been issued and what has
been revoked.

## Status

**Milestone 1 -- done.** Wire protocol, schema, CLI descriptors and detection,
hub with token enrolment and lease-based liveness, node with dial-out,
reconnect-with-backoff and identity-checked signalling.

**Milestone 2 -- done.** The chain runs end to end:

- `mcpd`, the MCP stdio shim, and the node's unix socket.
- Managed spawn: the hub places a slot, the node spawns the CLI, writes its MCP
  config, and reports the identity triple and the exit.
- Attached join: a human runs a CLI by hand, anywhere; the hub gives it a seat
  and infers its workspace from its working directory.
- External participants over HTTP (the hermes case).
- All 17 team tools, the review state machine, scoped file locks, shared
  knowledge, the plan.
- Delivery with ack: messages ride back on any tool result under
  `[PENDING MESSAGES]`, and are marked delivered only once the shim has written
  them to its CLI. An agent that dies first releases them for the next agent in
  that slot rather than losing them.
- Driver fast path, token accounting, crash recovery, tmux adoption (above).
- Observability: the inter-agent message log with delivery state
  (`nexmesh log --follow`), and per-agent transcripts of process output that
  outlive the process (`nexmesh transcript`).
- An operator CLI: `nodes`, `sessions`, `new-session`, `add-agent`, `spawn`,
  `status`, `send`, `log`, `transcript`, `attach`, `adopt`, `pause`, `resume`,
  `release`, `budget`, `sync`, `pair`, `certs`, `revoke`, `install-orchestrator`.
- A web dashboard and an MCP orchestrator server (above).

132 unit tests, plus fifteen end-to-end suites that drive real processes
(`tests/README.md`). None of them spends API credits.

```bash
cargo test
cargo build
tests/run-all.sh      # a suite printing no verdict counts as a failure

# a two-machine team, by hand
nexmesh-hub enrol machine-a                     # on the hub, once per node
nexmesh-hub --listen 127.0.0.1:7870 serve
NEXMESH_TOKEN=... nexmesh-node --hub ws://hub:7870 --clis ./clis
nexmesh nodes
nexmesh new-session auth --workspace src --kind node_local --node machine-a --path /srv/app
nexmesh add-agent se-xxxx --cli claude --name Max --role engineer \
        --workspace src --node machine-a --task "Build the login flow." --owns 'src/**'
nexmesh spawn sl-xxxx
nexmesh status se-xxxx
```

**Milestone 3 -- done.** Interactive agents run under a pty with per-agent
scrollback; attach is multiplexed over the node's existing outbound socket, with
resize, multiple simultaneous viewers, read-only for headless agents, and
detach that leaves the agent running. Plus the transport guard above.

**Security hardening -- done.** Native TLS on the hub with self-signed cert
generation, explicit trust anchors on nodes and the operator CLI, three
separate credential types, operator auth as a route-group middleware, and the
two refusals described above.

**Milestone 4 -- done.** Both driver fast paths (`stream_json` for Claude Code,
`app_server` for Codex) with mid-turn injection and acknowledgement, token
accounting, resume handles, crash recovery with handoff and flap detection, and
tmux adoption.

**Milestone 5 -- done.** Workspace sync for shared-repo sessions, the web
dashboard, and the orchestrator as an MCP server so any CLI can run the fleet.

**Done since.** Pausing and releasing agents; pairing with one-time codes;
mutual TLS; certificate revocation that takes effect during the handshake;
binding a certificate to the credential sent with it; the `plugin` driver, so a
CLI nexmesh has never heard of gets the fast path without a nexmesh release;
and the MCP front door, so anything that speaks MCP is a teammate with nothing
but a URL and a token.

**Known gaps**, deliberate and not silent: `sync = "shared_fs"` is a no-op by
design, because a shared filesystem already does that job.

Throughout this document, machines are called *machine A*, *machine B* and so
on; substitute your own.

## Deployment notes

- Devuan/sysvinit hosts need `/etc/init.d` scripts, not systemd units.
- Build `--target x86_64-unknown-linux-musl` for a dependency-free drop-in.
- **Do not** build with `-C target-cpu=native` on a host with AVX and ship it to
  one without (machine-c, Celeron J1900).
- Bind the hub to loopback or a VPN address. Never `0.0.0.0` without TLS.

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).

On the relationship to the upstream project: nexmesh is an independent
implementation in Rust. It reuses the *design* credited above and **none of the
source code** of [multiagents](https://github.com/zetbrush/multiagents), which
is TypeScript/Bun and declares MIT in its `package.json` and README (that
repository carries no `LICENSE` file). No upstream file was copied, adapted or
translated; the schema, wire protocol and every line here were written from
scratch. The GPLv3 therefore covers this work only, and takes nothing away from
the original.
