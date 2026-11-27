-- nexmesh hub state. One writer (the hub); SQLite in WAL mode is sufficient.
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------- fleet ----

CREATE TABLE IF NOT EXISTS nodes (
  id               TEXT PRIMARY KEY,
  name             TEXT NOT NULL UNIQUE,     -- "machine-c", "machine-b"
  os               TEXT NOT NULL,
  arch             TEXT NOT NULL,
  node_version     TEXT NOT NULL,
  labels           TEXT NOT NULL DEFAULT '[]',
  status           TEXT NOT NULL DEFAULT 'offline',  -- online | draining | offline
  first_seen_ms    INTEGER NOT NULL,
  last_seen_ms     INTEGER NOT NULL,
  lease_expires_ms INTEGER NOT NULL           -- liveness is a LEASE, never a pid probe
);

-- Which agent CLIs a node can actually run, refreshed on every Hello.
CREATE TABLE IF NOT EXISTS node_clis (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  cli_id  TEXT NOT NULL,
  version TEXT,
  path    TEXT,
  -- The account this CLI signs in as, when its descriptor names one.
  account TEXT,
  PRIMARY KEY (node_id, cli_id)
);

-- Pre-shared node enrolment tokens. A node authenticates before it may spawn.
CREATE TABLE IF NOT EXISTS node_tokens (
  token_sha256 TEXT PRIMARY KEY,
  node_name    TEXT NOT NULL,
  issued_at_ms INTEGER NOT NULL,
  revoked_at_ms INTEGER
);

-- One-time, short-lived codes that let a machine or a person enrol itself.
--
-- Pairing is the moment a stranger becomes a member, so the code is the whole
-- security boundary: it is single-use, expires quickly, is bound in advance to
-- the identity it may claim, and only its hash is stored. The private key is
-- generated on the far end and never travels.
CREATE TABLE IF NOT EXISTS pairing_codes (
  id            TEXT PRIMARY KEY,
  code_sha256   TEXT NOT NULL UNIQUE,
  kind          TEXT NOT NULL,            -- node | operator
  name          TEXT NOT NULL,            -- the identity this code may claim
  issued_at_ms  INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  used_at_ms    INTEGER,
  used_from     TEXT,
  revoked_at_ms INTEGER
);

-- Every client certificate this CA has issued, so one can be revoked without
-- re-issuing the CA and starting the fleet again.
CREATE TABLE IF NOT EXISTS issued_certs (
  serial        TEXT PRIMARY KEY,         -- decimal, as it appears in the cert
  name          TEXT NOT NULL,            -- the identity it asserts
  kind          TEXT NOT NULL,            -- node | operator
  issued_at_ms  INTEGER NOT NULL,
  revoked_at_ms INTEGER,
  reason        TEXT
);

-- -------------------------------------------------- dashboard accounts ----
--
-- The dashboard used to want an operator token pasted into the browser. That
-- works, but it means handling a fleet-wide credential by hand every time,
-- and a token good for the whole API is a poor thing to keep in a browser.
--
-- So the dashboard has accounts of its own: a username and a password, set up
-- on first use. The account authenticates a PERSON; operator tokens remain
-- what a script or a CLI uses.
--
-- The first account cannot simply be claimed by whoever arrives first -- this
-- hub may be on a public address -- so creating it needs a setup code the hub
-- prints to its own log. Same reasoning as a pairing code: bound in advance,
-- single use, and shown where only someone with access to the host can read
-- it.
CREATE TABLE IF NOT EXISTS dashboard_users (
  username      TEXT PRIMARY KEY,
  -- Argon2id PHC string. Never a reversible encoding, never a bare digest.
  password_hash TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  last_login_ms INTEGER,
  -- Consecutive failures, and when the account stops accepting attempts.
  -- A public hub needs a wrong password to cost something.
  failed_count  INTEGER NOT NULL DEFAULT 0,
  locked_until_ms INTEGER
);

-- Browser sessions. The cookie holds a random id; only its hash is stored, so
-- the table is not a list of working credentials.
CREATE TABLE IF NOT EXISTS dashboard_sessions (
  id_sha256     TEXT PRIMARY KEY,
  username      TEXT NOT NULL REFERENCES dashboard_users(username) ON DELETE CASCADE,
  created_at_ms INTEGER NOT NULL,
  last_seen_ms  INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS dashboard_sessions_user_idx ON dashboard_sessions(username);

-- The one-time code that authorises creating the first account.
CREATE TABLE IF NOT EXISTS dashboard_setup (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  code_sha256   TEXT NOT NULL,
  issued_at_ms  INTEGER NOT NULL,
  used_at_ms    INTEGER
);

-- ------------------------------------------------------ capability asks ----
--
-- A synchronous request from one teammate to another.
--
-- Messages are the right shape for "here is something you should know" and
-- the wrong shape for "OCR this and give me the text": the asker needs the
-- answer before it can continue. So a capability ask is a row with a reply
-- slot, which the asker waits on and the holder fills in.
--
-- Rows rather than an in-memory channel, so an ask survives the holder being
-- restarted mid-flight: it is still pending, and visible.
CREATE TABLE IF NOT EXISTS capability_asks (
  id            TEXT PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  capability    TEXT NOT NULL,
  from_slot     TEXT NOT NULL,
  to_slot       TEXT,                     -- NULL until one is chosen
  request       TEXT NOT NULL,            -- JSON the holder understands
  -- Which of the holder's models to use, when it offers more than one.
  model         TEXT,
  status        TEXT NOT NULL DEFAULT 'pending',  -- pending|done|failed
  result        TEXT,
  asked_at_ms   INTEGER NOT NULL,
  answered_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS capability_asks_queue_idx
  ON capability_asks(to_slot, status);

-- ------------------------------------------- a seat's conversations ----
--
-- WHICH CONVERSATION THIS SEAT IS ON, PER MACHINE.
--
-- A CLI conversation is a local file. It belongs to one machine and -- for
-- claude, which keys them by project directory -- to one directory on it. A
-- handle is therefore not portable: move a seat to another machine and the
-- conversation does not follow. The CLI then either starts blank wearing the
-- right name, or (claude) refuses with "No conversation found with session
-- ID" and exits at once, which is a seat that cannot be opened at all.
--
-- So the binding is recorded rather than assumed, and it is recorded PER
-- PLACE. A seat that moves workstation -> laptop -> workstation finds its workstation
-- conversation again instead of having lost it, and a seat on a machine it
-- has never run on starts fresh deliberately rather than by failing.
--
-- It follows the SEAT, so a rename changes nothing (the name is not part of
-- the key) and moving between teams changes nothing (the session is not
-- either). The things that DO change the conversation are the three that
-- really do mean a different conversation: a different machine, a different
-- working directory, a different CLI.
CREATE TABLE IF NOT EXISTS slot_conversations (
  slot_id       TEXT NOT NULL REFERENCES slots(id) ON DELETE CASCADE,
  node_id       TEXT NOT NULL,
  cwd           TEXT NOT NULL,
  cli_id        TEXT NOT NULL,
  handle        TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (slot_id, node_id, cwd, cli_id)
);
CREATE INDEX IF NOT EXISTS slot_conversations_by_slot ON slot_conversations(slot_id);

-- ------------------------------------------------------------- guests ----
--
-- A seat that serves MORE THAN ONE team.
--
-- A session is the isolation boundary: agents never see across one, and
-- almost nothing should. But some seats are services rather than teammates
-- -- a capability bridge offering OCR and transcription, a reviewer every
-- team sends work to -- and giving each team its own copy means N processes,
-- N models and N sets of warm context for one job.
--
-- So this punches a deliberate, recorded hole in that boundary. `slots.
-- session_id` stays the seat's HOME: where it lives, whose workspace it
-- holds, who owns it. Each row here is one extra team it also answers in.
--
-- Deliberately not a flag on the slot. Who may open the hole matters as much
-- as that it is open: it takes reach over BOTH teams, so only the operator
-- or a superorchestrator can -- and `added_by` records which.
CREATE TABLE IF NOT EXISTS slot_sessions (
  slot_id     TEXT NOT NULL REFERENCES slots(id) ON DELETE CASCADE,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  added_at_ms INTEGER NOT NULL,
  -- `operator`, or `seat <name> (<slot>)`. A seat reaching into another team
  -- is exactly the thing somebody will need to account for afterwards.
  added_by    TEXT NOT NULL DEFAULT 'operator',
  PRIMARY KEY (slot_id, session_id)
);
CREATE INDEX IF NOT EXISTS slot_sessions_by_session ON slot_sessions(session_id);

-- ---------------------------------------------------------- approvals ----
--
-- A decision only the OPERATOR can make, waiting for them.
--
-- An agent CLI stops and asks before it runs a command, writes a file or
-- calls a tool. When somebody is attached to its terminal that question is
-- theirs to answer and this table is not involved. When nobody is, the node
-- used to answer the narrowest affirmative itself -- which keeps the fleet
-- moving and quietly means nobody authorised anything.
--
-- So the question comes HERE instead: into the operator's chat, with the
-- choices the CLI offered as buttons, and a notification, and it waits. A
-- pending approval is a stopped agent, which is the honest state: it is
-- waiting for a person. Nothing times out into "allowed" -- a timeout that
-- grants permission is the behaviour this replaces.
--
-- `fingerprint` is the shape of the screen that asked, so one dialog is
-- raised once however many idle ticks see it. A terminal shows ONE question,
-- so there is at most one pending row per slot: a report whose choices match
-- a waiting one refreshes it in place, a genuinely different question retires
-- it, and a request whose agent has gone is retired too -- its buttons could
-- only fail.
CREATE TABLE IF NOT EXISTS approvals (
  id             TEXT PRIMARY KEY,
  session_id     TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  slot_id        TEXT NOT NULL,
  agent_id       TEXT,
  node_id        TEXT,
  -- How the question reached us, because answering it differs: `pty_prompt`
  -- is a dialog on a terminal, answered by typing the keys the CLI listed.
  -- Structured kinds (a CLI that asks over its own protocol) answer through
  -- that protocol instead.
  kind           TEXT NOT NULL DEFAULT 'pty_prompt',
  -- What the agent is being asked, in the CLI's own words, and the choices
  -- it offered: [{"key":"1","label":"Yes","keys":"1"}, ...]. The buttons the
  -- operator is shown are these, not a guess at them -- a menu whose
  -- affirmative is "Yes, and don't ask again" must not be presented as "Yes".
  question       TEXT NOT NULL,
  options        TEXT NOT NULL DEFAULT '[]',
  fingerprint    TEXT,
  status         TEXT NOT NULL DEFAULT 'pending',  -- pending|decided|withdrawn
  -- Which option was chosen, who chose it, and how: `operator` when a person
  -- clicked, `auto:agent` or `auto:session` when a standing auto-approval
  -- answered, `attached` when somebody answered at the terminal instead.
  decision       TEXT,
  -- `operator`, `auto:agent`, `auto:session`, `attached` (answered at its own
  -- terminal, or the prompt went away), `superseded` (the seat is on a
  -- different question now) or `gone` (the agent that asked is not there any
  -- more).
  decided_by     TEXT,
  asked_at_ms    INTEGER NOT NULL,
  decided_at_ms  INTEGER,
  -- The chat line that carries this, so the buttons can be retired in place
  -- once it is answered rather than leaving a live-looking prompt behind.
  message_id     INTEGER
);
CREATE INDEX IF NOT EXISTS approvals_pending_idx ON approvals(status, asked_at_ms);
CREATE INDEX IF NOT EXISTS approvals_slot_idx ON approvals(slot_id, status);

-- --------------------------------------------------------------- folds ----
--
-- Context that outlives the window it was learned in.
--
-- A long-lived agent spends its context twice: once working, and then again
-- carrying what it did. A CLI's own compaction answers the second cost by
-- summarising everything into its own context, where the next compaction
-- summarises the summary; the fourth generation of that remembers the shape
-- of the work and none of its facts, and none of it survives a restart on a
-- CLI that cannot resume.
--
-- A fold is the alternative: the agent writes down what a stretch of work
-- established, the detail is kept HERE rather than in its context, and it is
-- handed back in full whenever anything asks. Folds fold into folds, so a
-- month of work is a handful of summaries over a tree of detail, read
-- downwards only where a question needs it. An LSM tree over prose, which is
-- billion-context's design, reimplemented rather than imported -- see
-- `folds.rs` for what nexmesh does differently and why it needs no proxy.
--
-- Scoped like `memory`, and read the same way: nearest first.
CREATE TABLE IF NOT EXISTS folds (
  id             TEXT PRIMARY KEY,
  scope          TEXT NOT NULL,                    -- agent|group|session|global
  scope_ref      TEXT NOT NULL DEFAULT '',         -- slot, group name, session, ''
  -- 1 folds the agent's own work; 2 folds tier-1 folds; and so on up.
  tier           INTEGER NOT NULL DEFAULT 1,
  topic          TEXT NOT NULL,
  -- What survives in context. Short on purpose.
  summary        TEXT NOT NULL,
  -- What was taken OUT of context, verbatim. Kept rather than pointed at: a
  -- transcript rotates and a conversation file is rewritten by its own CLI,
  -- so a pointer would be a promise this table could not keep.
  detail         TEXT NOT NULL DEFAULT '',
  -- The folds this one replaced: ["fd-...", ...]. Their lineage, so a
  -- question can walk down from a summary to the detail underneath it.
  covers         TEXT NOT NULL DEFAULT '[]',
  author_slot    TEXT,
  agent_id       TEXT,
  -- What it stood for, and what it costs to carry. Estimated at four
  -- characters to a token unless the caller counted properly.
  tokens_before  INTEGER NOT NULL DEFAULT 0,
  tokens_after   INTEGER NOT NULL DEFAULT 0,
  -- 0 once something folded this one: reachable through its parent, and kept
  -- out of the map so a stretch of work is never counted twice.
  active         INTEGER NOT NULL DEFAULT 1,
  unfolded_count INTEGER NOT NULL DEFAULT 0,
  unfolded_at_ms INTEGER,
  created_at_ms  INTEGER NOT NULL,
  retired_at_ms  INTEGER,
  retired_why    TEXT
);
CREATE INDEX IF NOT EXISTS folds_scope_idx ON folds(scope, scope_ref, active);
CREATE INDEX IF NOT EXISTS folds_author_idx ON folds(author_slot, created_at_ms);

-- --------------------------------------------------------- code graph ----
--
-- One workspace's parsed structure, kept so a codebase is read once rather
-- than once per agent. DERIVED, never authored: it is regenerated from the
-- files and holds no opinions, which is exactly why it can be thrown away.
-- Anything concluded about the code belongs in `memory`, which is attributed
-- and scoped.
CREATE TABLE IF NOT EXISTS code_graph_nodes (
  workspace_id TEXT NOT NULL,
  id           TEXT NOT NULL,        -- '<rel path>#<symbol>', or 'module:<name>'
  kind         TEXT NOT NULL,
  name         TEXT NOT NULL,
  path         TEXT NOT NULL,
  line         INTEGER NOT NULL,
  lang         TEXT NOT NULL,
  -- Leiden community: the de facto subsystem this symbol belongs to. NULL
  -- until communities have been computed.
  community    INTEGER,
  PRIMARY KEY (workspace_id, id)
);
CREATE INDEX IF NOT EXISTS code_graph_name_idx ON code_graph_nodes(workspace_id, name);
CREATE INDEX IF NOT EXISTS code_graph_comm_idx ON code_graph_nodes(workspace_id, community);

CREATE TABLE IF NOT EXISTS code_graph_edges (
  workspace_id TEXT NOT NULL,
  from_id      TEXT NOT NULL,
  to_id        TEXT NOT NULL,
  -- defines | imports are read off the syntax tree; calls | references are
  -- resolved by name. The distinction is carried to the agent rather than
  -- flattened, because a guess must not read as a fact.
  kind         TEXT NOT NULL,
  PRIMARY KEY (workspace_id, from_id, to_id, kind)
);
CREATE INDEX IF NOT EXISTS code_graph_from_idx ON code_graph_edges(workspace_id, from_id);
CREATE INDEX IF NOT EXISTS code_graph_to_idx ON code_graph_edges(workspace_id, to_id);

CREATE TABLE IF NOT EXISTS code_graph_meta (
  workspace_id TEXT PRIMARY KEY,
  built_at_ms  INTEGER NOT NULL,
  files        INTEGER NOT NULL,
  skipped      TEXT NOT NULL DEFAULT '[]',
  truncated    INTEGER NOT NULL DEFAULT 0,
  communities  INTEGER NOT NULL DEFAULT 0,
  -- WHEN THE TREE MOVED UNDER IT.
  --
  -- The graph was built on demand and never again, so every team that asked
  -- it a structural question after the first day got an answer about a
  -- codebase that no longer existed -- and an agent cannot tell a stale
  -- answer from a wrong one. Set when work is published into the workspace,
  -- cleared by the rebuild. NULL means the graph matches what was last
  -- shared.
  stale_at_ms  INTEGER,
  -- Why the last automatic rebuild did not happen, if it did not: no node
  -- holding the workspace, the node refused, parsing timed out. Shown with
  -- the graph, because silently serving a stale graph is the failure.
  stale_reason TEXT,
  -- WHAT THE TREE LOOKED LIKE WHEN WE LAST ASKED.
  --
  -- Publishing work is not the only way a tree moves, and on a `node_local`
  -- workspace it never happens at all -- which left every graph on a real
  -- fleet built once and never again. The hub now asks the node that holds
  -- the files for a fingerprint (file count, total size, newest mtime): a
  -- walk with the parsing taken out. When it differs from the one here the
  -- graph is marked stale and the ordinary rebuild picks it up.
  --
  -- `fp_asked_at_ms` is set when the question goes out and `fp_at_ms` when
  -- an answer arrives, so a node too old to know the frame is visible as a
  -- question that was never answered -- and those workspaces fall back to
  -- rebuilding on age instead.
  tree_fingerprint TEXT,
  fp_at_ms     INTEGER,
  fp_asked_at_ms INTEGER,
  fp_files     INTEGER
);

-- ------------------------------------------------------------- memory ----
--
-- Shared memory with a scope, replacing the flat per-session `knowledge`
-- table (which is kept as a view-like path through the same tools).
--
-- Four scopes, resolved nearest-first on a read -- agent, then any group the
-- reader belongs to, then the session, then global. So a cheap seat can be
-- given a private scratchpad while still reading the team's conventions, and
-- a convention written once outlives the session that discovered it.
--
-- `scope_ref` is '' for global, else the session id, the group name or the
-- slot id. It is in the uniqueness constraint rather than a separate table
-- because "one value per key per scope" is the whole contract.
CREATE TABLE IF NOT EXISTS memory (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  scope         TEXT NOT NULL,              -- global | session | group | agent
  scope_ref     TEXT NOT NULL DEFAULT '',
  key           TEXT NOT NULL,
  category      TEXT NOT NULL DEFAULT 'context',
  value         TEXT NOT NULL,
  author_slot   TEXT,
  -- Bumped on every overwrite, with the old value kept in memory_history.
  -- The flat table silently lost the previous value, so two agents writing
  -- one key meant last-writer-wins with no way to see it had happened.
  revision      INTEGER NOT NULL DEFAULT 1,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  -- Long-term hygiene: a fact that was only true for a while says so.
  expires_at_ms INTEGER,
  UNIQUE (scope, scope_ref, key)
);
CREATE INDEX IF NOT EXISTS memory_scope_idx ON memory(scope, scope_ref);
CREATE INDEX IF NOT EXISTS memory_expiry_idx ON memory(expires_at_ms)
  WHERE expires_at_ms IS NOT NULL;

-- Skill enablement, scoped like memory. A seat's carried skills begin with
-- its role template's own list; a row here turns one skill ON or OFF at a
-- given breadth, and a nearer scope wins over a wider one (seat over session
-- over global), the same shadowing memory uses. This is NOT a column on
-- slots because a single skill's state is set at three different breadths by
-- different people at different times -- a team-wide default the operator
-- sets once, a per-seat exception another sets later -- and flattening that
-- onto the seat row would lose which breadth each decision was made at, so a
-- team default could never be changed without re-deciding every seat.
-- scope: global | session | seat.  scope_ref: '' | session_id | slot_id.
CREATE TABLE IF NOT EXISTS skill_prefs (
  scope      TEXT NOT NULL,
  scope_ref  TEXT NOT NULL DEFAULT '',
  skill      TEXT NOT NULL,
  enabled    INTEGER NOT NULL,
  at_ms      INTEGER NOT NULL,
  PRIMARY KEY (scope, scope_ref, skill)
);

-- Who may read a group scope. Membership is explicit: a group is a named
-- subset of a team, not something inferred from a role, so moving an agent
-- between groups never silently changes what it can see.
CREATE TABLE IF NOT EXISTS memory_groups (
  group_name  TEXT NOT NULL,
  slot_id     TEXT NOT NULL,
  added_at_ms INTEGER NOT NULL,
  PRIMARY KEY (group_name, slot_id)
);

-- Previous revisions. Capped by a prune, not unbounded: this is an audit
-- trail for "who changed that and to what", not a version control system.
CREATE TABLE IF NOT EXISTS memory_history (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  memory_id      INTEGER NOT NULL,
  revision       INTEGER NOT NULL,
  value          TEXT NOT NULL,
  author_slot    TEXT,
  replaced_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS memory_history_idx ON memory_history(memory_id, revision);

-- Full-text search over keys and values.
--
-- The flat table could only be read by EXACT key or by listing a category,
-- so an agent had to already know the key. External-content FTS5 keeps the
-- index beside the rows rather than duplicating them.
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
  key, value, content='memory', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memory_fts_ins AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts(rowid, key, value) VALUES (new.id, new.key, new.value);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_del AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, key, value)
    VALUES ('delete', old.id, old.key, old.value);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_upd AFTER UPDATE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, key, value)
    VALUES ('delete', old.id, old.key, old.value);
  INSERT INTO memory_fts(rowid, key, value) VALUES (new.id, new.key, new.value);
END;

-- Continuity for one agent identity ACROSS sessions.
--
-- slots.resume_handle dies with its session, so a CLI conversation could
-- never be picked up again the next day. Keyed by what actually identifies a
-- returning agent -- its CLI, where it works, and what it is called -- rather
-- than by a slot id, which is new every time.
--
-- `context` is a rolling summary the agent maintains itself, so a seat can be
-- rehydrated even on a CLI with no resume facility at all.
CREATE TABLE IF NOT EXISTS agent_continuity (
  identity      TEXT PRIMARY KEY,           -- cli_id|workspace_key|name
  cli_id        TEXT NOT NULL,
  workspace_key TEXT NOT NULL,
  name          TEXT NOT NULL,
  resume_handle TEXT,
  context       TEXT NOT NULL DEFAULT '',
  updated_at_ms INTEGER NOT NULL
);

-- Live MCP sessions on the participant front door.
--
-- An MCP client has no "join" call, so `initialize` seats it and mints the
-- session id it echoes afterwards. The token hash is kept beside it because a
-- captured session id must not be a credential on its own, and `pending_ack`
-- holds what was last handed to that client: it is acked when the client
-- returns, which is the only proof over a network that it arrived.
-- An optional ceiling on what a session may spend, in total tokens across
-- every agent in it. Reaching it PAUSES the session rather than killing
-- anything: pausing is reversible, already understood by every agent, and
-- leaves the operator to decide. NULL means no ceiling.
--
-- Deliberately not a per-agent cap. With a mixed team the interesting number
-- is what the work cost, not what one seat cost.

CREATE TABLE IF NOT EXISTS mcp_sessions (
  id            TEXT PRIMARY KEY,         -- the Mcp-Session-Id we issued
  agent_id      TEXT NOT NULL REFERENCES agents(id),
  token_sha256  TEXT NOT NULL,            -- which participant token opened it
  protocol      TEXT NOT NULL,            -- the MCP revision agreed
  pending_ack   TEXT NOT NULL DEFAULT '[]',
  created_at_ms INTEGER NOT NULL,
  last_seen_ms  INTEGER NOT NULL,
  closed_at_ms  INTEGER
);

-- Credentials for operators: the people (and scripts) that drive the fleet.
-- An operator token is the most powerful credential here -- it can create
-- sessions, spawn processes on any node and attach to any agent's terminal --
-- so like the others only its hash is stored.
CREATE TABLE IF NOT EXISTS operator_tokens (
  token_sha256  TEXT PRIMARY KEY,
  label         TEXT NOT NULL,
  -- How far this operator reaches. NULL is the whole fleet -- a SUPER
  -- orchestrator -- and a session id confines the token to that one session:
  -- it drives that team and cannot see, touch or discover the rest.
  session_id    TEXT REFERENCES sessions(id) ON DELETE CASCADE,
  issued_at_ms  INTEGER NOT NULL,
  revoked_at_ms INTEGER,
  last_used_ms  INTEGER
);

-- Notes between the programs DRIVING sessions, and from agents to them.
--
-- Separate from `messages`, which belongs to a session and is read by its
-- agents. A scoped orchestrator cannot see that another session exists, so
-- when it needs something from outside its own team the only honest thing it
-- can do is ask someone whose reach is wider. An agent uses the same channel
-- through notify_operator: its own session's orchestrator first, and the
-- supervisors when there is none or it has gone quiet.
CREATE TABLE IF NOT EXISTS operator_messages (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  from_label    TEXT NOT NULL,
  -- operator | agent. Both carry a from_session, so without this an
  -- orchestrator cannot tell a peer's note from one of its own team asking
  -- for help.
  from_kind     TEXT NOT NULL DEFAULT 'operator',
  from_session  TEXT,                      -- NULL when a super sent it
  to_session    TEXT,                      -- NULL = addressed to the supers
  body          TEXT NOT NULL,
  sent_at_ms    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS operator_messages_to ON operator_messages(to_session, id);

-- Where each operator has read up to on that channel. Per label, so two
-- supervisors each see a request rather than the first reader consuming it.
CREATE TABLE IF NOT EXISTS operator_read_cursor (
  label     TEXT PRIMARY KEY,
  last_id   INTEGER NOT NULL DEFAULT 0
);

-- Credentials for external (non-CLI, non-node) participants. Deliberately a
-- different table from node_tokens: a participant token may call team tools
-- for its own slot and nothing else -- in particular it can never spawn a
-- process on anybody's machine.
CREATE TABLE IF NOT EXISTS participant_tokens (
  token_sha256  TEXT PRIMARY KEY,
  label         TEXT NOT NULL,
  session_id    TEXT,
  -- What a seat taken with this token is allowed to do: NULL for an ordinary
  -- agent, or `observer` for one that can only read.
  --
  -- ON THE TOKEN, not asked for by the client. An observer seat exists so
  -- that somebody outside the team -- a browser session, a dashboard of
  -- their own -- can watch without being able to steer, and a restriction
  -- the watcher chooses for itself is not a restriction. Whoever holds this
  -- token cannot be anything else.
  authority     TEXT,
  issued_at_ms  INTEGER NOT NULL,
  revoked_at_ms INTEGER
);

-- ------------------------------------------------------------- sessions ----

CREATE TABLE IF NOT EXISTS sessions (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  status         TEXT NOT NULL DEFAULT 'active',  -- active|paused|ended|archived
  pause_reason   TEXT,
  paused_at      INTEGER,
  config         TEXT NOT NULL DEFAULT '{}',
  created_at_ms  INTEGER NOT NULL,
  last_active_ms INTEGER NOT NULL,
  -- Total tokens this session may spend before it is paused. NULL = no cap.
  token_budget     INTEGER,
  budget_hit_at_ms INTEGER,
  -- Answer every seat's permission prompts in this team without asking. The
  -- whole-team switch, for a session whose work is understood and whose
  -- blast radius is known; a single seat has its own flag in `slots`.
  auto_approve     INTEGER NOT NULL DEFAULT 0,
  -- How full a seat's context may get in this team before the hub types the
  -- CLI's own compaction command at it. Percent of that CLI's window.
  --
  -- NULL means "whatever the fleet is set to" -- the hub's own default --
  -- and 0 means off for this team however the fleet is set. Three states
  -- rather than two, because "inherit" and "deliberately off" are different
  -- answers and a single integer with 0 for both cannot express a team that
  -- opts out of a fleet-wide setting.
  compact_at_pct   INTEGER,
  -- How many times a seat in this team may repeat the same turn before the
  -- hub stops it. Same three states: NULL inherits, 0 is off.
  loop_guard_repeats INTEGER,
  -- Whether the hub reminds this team's seats to keep what they learn
  -- (remind_memory) and to look things up in the code graph before reading
  -- the tree (remind_graph). NULL follows the fleet, which is on; 0 is off.
  remind_memory    INTEGER,
  remind_graph     INTEGER,
  -- THE TEAM'S STANDING RULES: what every seat in this session is told, over
  -- and above its own brief.
  --
  -- On the SESSION, because this is the part that is true of the work rather
  -- than of one seat -- the conventions, the things not to touch, the way
  -- this team does releases. Written once instead of into fourteen seats'
  -- prompts, where it drifts the moment one is edited.
  --
  -- Who may write it: the operator, and a seat holding orchestrator or
  -- superorchestrator authority. Deliberately not an ordinary agent: rules
  -- an agent can rewrite are not rules, and the first thing a confused
  -- agent would do is edit them.
  --
  -- Every seat gets it in its first prompt, and a change is said in the
  -- chat -- otherwise the agents already running would keep working to the
  -- old rules with nothing to tell them apart.
  instructions     TEXT,
  -- FLOOD PROTECTION. How many messages one seat here may send a minute,
  -- and how many may go back and forth between the SAME TWO seats in ten.
  -- NULL inherits the fleet's, 0 is off, and off is what both ship as.
  --
  -- Two numbers because there are two failures. A seat sending thirty
  -- messages a minute is one agent spending everybody's turns; two seats
  -- answering each other forty times is a pair that has stopped working and
  -- is now just talking, which no per-seat rate would ever catch -- each of
  -- them is sending perfectly reasonably.
  msg_rate_per_min INTEGER,
  msg_pair_limit   INTEGER
);

-- A workspace is WHERE work happens. This single table is what lets per-host
-- ops work and shared-codebase work coexist in one session.
--   kind=node_local -> files on exactly one node; locks are node-scoped.
--   kind=repo       -> the same logical tree on many nodes; locks are repo-scoped.
CREATE TABLE IF NOT EXISTS workspaces (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL,                  -- node_local | repo
  node_id     TEXT REFERENCES nodes(id),      -- kind=node_local
  repo_url    TEXT,                           -- kind=repo
  repo_branch TEXT,
  sync        TEXT NOT NULL DEFAULT 'none',   -- none | git | shared_fs
  UNIQUE (session_id, name)
);

-- Where a workspace is materialised on each node (paths differ per machine).
CREATE TABLE IF NOT EXISTS workspace_mounts (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  node_id      TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  path         TEXT NOT NULL,
  PRIMARY KEY (workspace_id, node_id)
);

-- A slot is a seat in the team: stable identity across crashes and respawns.
CREATE TABLE IF NOT EXISTS slots (
  id               TEXT PRIMARY KEY,
  session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  workspace_id      TEXT REFERENCES workspaces(id),
  cli_id            TEXT,                            -- claude | opencode | qwen | ...
                                                     -- NULL for an external participant
  participant_kind  TEXT NOT NULL DEFAULT 'cli',     -- cli | external
  mode              TEXT NOT NULL DEFAULT 'headless',-- headless | interactive
  placement         TEXT NOT NULL DEFAULT '{}',      -- {"node":"machine-c"} | {"labels":["gpu"]} | {"any":true}
  display_name      TEXT,
  role              TEXT,
  role_description  TEXT,
  -- The model this seat was ASKED for, so a respawn gets the same one. NULL
  -- means whatever the CLI defaults to.
  model             TEXT,
  -- Free-form capability labels: "strong", "cheap", "vision", "ocr", ...
  -- What a teammate can address instead of guessing who is behind a role.
  -- A list rather than one tier, because capability is not a single axis.
  capabilities      TEXT NOT NULL DEFAULT '[]',
  -- Last reported status line. On the SLOT, not the agent: it describes the
  -- WORK, so it must survive the process exiting or being respawned on
  -- another machine -- exactly like task_state and queued messages.
  summary           TEXT NOT NULL DEFAULT '',
  task_state        TEXT NOT NULL DEFAULT 'idle',
  -- Whether anything is currently filling this seat. Distinct from
  -- task_state, which is about the WORK and survives a reconnect.
  status            TEXT NOT NULL DEFAULT 'disconnected',  -- connected|disconnected
  paused            INTEGER NOT NULL DEFAULT 0,
  paused_at         INTEGER,
  restarts          INTEGER NOT NULL DEFAULT 0,
  last_crash_ms     INTEGER,
  -- The most recent conversation id, for display. What a SPAWN uses comes
  -- from `slot_conversations`, which remembers one per machine -- see there.
  resume_handle     TEXT,
  -- For an ADOPTED slot: the tmux target (session, window or pane) whose
  -- terminal this slot mirrors.
  tmux_target       TEXT,
  -- A seat that SUPERVISES the fleet. What agents raise to the supervisors is
  -- delivered into this seat's session as an ordinary message, with a return
  -- address it can answer: the operator channel has no read tool for seats,
  -- and a channel you can only shout into is not a channel.
  supervisor        INTEGER NOT NULL DEFAULT 0,
  -- WHAT THIS SEAT IS ALLOWED TO DO to the fleet: agent | orchestrator | super.
  --
  -- Distinct from `role` above, which is the seat's JOB ("Dev", "Reviewer") and
  -- carries no authority at all. This is the authority, and it lives here --
  -- on the seat, in the hub -- for one reason: it used to live in whichever
  -- MCP config happened to hold an operator token. A user-scoped install of
  -- the orchestrator server therefore made EVERY CLI that user started a
  -- fleet operator, indistinguishable from the person running it, because
  -- possession of the token WAS the authority. Authority has to be a property
  -- of an identity the hub knows, which is the seat.
  --
  -- Reach follows the level: `orchestrator` reaches its own session, `super`
  -- reaches the fleet -- the same two scopes `operator_tokens.session_id`
  -- already described. An orchestrator may elect other orchestrators inside
  -- its own session; it can never mint a `super`, because a privilege that
  -- can widen itself is not a boundary.
  authority         TEXT NOT NULL DEFAULT 'agent',
  -- Answer this seat's permission prompts without asking the operator.
  -- Deliberately per-seat: a seat doing something routine in a sandbox is a
  -- different risk from one with a shell on a live box, and "approve
  -- everything" as a global switch is how the operator stops reading them.
  auto_approve      INTEGER NOT NULL DEFAULT 0,
  -- Bring this seat back by itself when whatever is filling it exits.
  --
  -- On by default, which is what the hub has always done for a managed
  -- seat -- but it is now a DECISION rather than a rule, because not every
  -- seat should come back: one kept for a job that is over, or one being
  -- deliberately stopped to look at something, used to be respawned out
  -- from under the operator every few seconds.
  --
  -- A respawn PREPARES the seat: the process is there and can be attached
  -- to. It never reattaches anybody's terminal, because whose terminal that
  -- would be is not the hub's to decide.
  autorespawn       INTEGER NOT NULL DEFAULT 1,
  -- This seat's own compaction threshold, overriding its team's and the
  -- fleet's. Percent of its CLI's context window; NULL inherits, 0 is off.
  --
  -- Per seat because the right answer is not uniform: a seat doing long
  -- mechanical work wants compacting early and often, and a seat holding a
  -- design discussion loses the whole point of itself every time it is
  -- compacted. The operator who made the seat is the one who knows which.
  compact_at_pct    INTEGER,
  -- How many times this seat may repeat the same turn before the hub stops
  -- it. NULL inherits its team's, then the fleet's; 0 is off.
  --
  -- Per seat for the same reason: a seat polling something until it changes
  -- is SUPPOSED to repeat itself, and a seat writing code is not.
  loop_guard_repeats INTEGER,
  -- This seat's own reminder switches, over its team's. See `sessions`.
  remind_memory     INTEGER,
  remind_graph      INTEGER,
  -- When this seat was last reminded. The seat's, not the agent's: a
  -- respawn or reattach must not restart the cooldown.
  memory_reminded_at_ms INTEGER,
  graph_reminded_at_ms  INTEGER,
  -- This seat's own flood limits, overriding its team's. See `sessions`.
  -- A coordinator legitimately sends more than anybody else; a seat that
  -- only ever answers should never be sending thirty a minute.
  msg_rate_per_min  INTEGER,
  msg_pair_limit    INTEGER,
  -- WHICH ROLE TEMPLATE this seat's system prompt was rendered from, and
  -- which of that template's skills it carries (a JSON array of names).
  --
  -- The prompt itself is stored RENDERED, in `system_prompt`, and that does
  -- not change: what a seat tells its agent should be readable and editable
  -- in one place, and a running team must not change under itself because
  -- somebody improved a template. But storing only the rendered text threw
  -- away the CHOICE, so there was nothing to select or deselect against --
  -- an operator wanting one skill off had to restate the template and the
  -- whole list from memory.
  --
  -- NULL means the prompt was written by hand. Writing `system_prompt`
  -- directly clears both, deliberately: once somebody has edited the text,
  -- re-rendering it from a template would throw their edit away.
  prompt_template   TEXT,
  prompt_skills     TEXT,
  -- HELD because its account cannot reach the model: quota | auth |
  -- billing, NULL when not held. A seat paused with no hold reason was paused
  -- by a person, and nothing automatic ever releases it. `hold_until_ms` is
  -- when a quota hold lifts itself; NULL for one that needs a person.
  hold_reason       TEXT,
  hold_until_ms     INTEGER,
  hold_text         TEXT,
  hold_since_ms     INTEGER,
  hold_account      TEXT,
  hold_tries        INTEGER,
  -- 0 keeps a held seat paused when its allowance comes back. NULL = resume.
  quota_auto_resume INTEGER,
  created_at_ms     INTEGER NOT NULL
);

-- Whatever is currently filling a slot. Usually an OS process on a node, in
-- which case identity is (node_id, pid, start_ticks) -- NEVER a bare pid,
-- since pids are only unique within one kernel.
--
-- But an agent need not be a process we host at all. An `external` agent --
-- a hermes agent, a Python daemon, anything that speaks the participant API --
-- has no node, no pid and no start_ticks, and its liveness is a lease like a
-- node's. Hence all four columns are nullable: "agent" must not mean
-- "CLI process we spawned".
CREATE TABLE IF NOT EXISTS agents (
  id                TEXT PRIMARY KEY,
  slot_id           TEXT NOT NULL REFERENCES slots(id) ON DELETE CASCADE,
  node_id           TEXT REFERENCES nodes(id),  -- NULL for external agents
  pid               INTEGER,
  proc_start_ticks  INTEGER,                   -- /proc/<pid>/stat field 22
  provenance        TEXT NOT NULL,             -- managed | attached | external
  -- The model this agent REPORTED it is running. Distinct from slots.model,
  -- which is what was asked for: a CLI may substitute, fall back, or ignore
  -- the request entirely, and the token counts beside this column only mean
  -- something if you know which model spent them.
  model             TEXT,
  -- Whether this agent's driver can take a message mid-turn. Decides whether
  -- the hub pushes to it or waits for its next tool result. True for a
  -- structured driver on pipes, and for an interactive seat with a terminal
  -- of its own, where the message is typed at the CLI's prompt.
  push_capable      INTEGER NOT NULL DEFAULT 0,
  -- pty | pipes, or NULL for a process the node never started and so has no
  -- I/O for. A pty is a keyboard: it is what makes a seat attachable, and
  -- typeable.
  io_kind           TEXT,
  -- starting|running|idle|exited|disconnected. `disconnected` is deliberate
  -- and replaced `lost`: the hub has lost its connection to the process, not
  -- the process. The CLI is frequently still there.
  status            TEXT NOT NULL,
  -- Liveness for agents with no process we can see: refreshed by the
  -- participant API, swept like a node lease.
  lease_expires_ms  INTEGER,
  kind_hint         TEXT,                      -- free-form: "hermes", "python", ...
  -- Where an EXTERNAL participant runs, as it reports itself. A node has a
  -- row in `nodes` and `node_id` points at it; a bridge, a bot or a service
  -- has neither, so without this the fleet knows only that it is "not a
  -- machine" -- which reads as nowhere, when in fact it is on a particular
  -- host doing particular work. Per agent rather than per seat, because a
  -- bridge can be moved and the next run should say so.
  host_hint         TEXT,
  summary           TEXT NOT NULL DEFAULT '',
  cwd               TEXT,
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  -- HOW FULL THIS CONVERSATION IS RIGHT NOW, as the node last measured it.
  --
  -- Emphatically not the three columns above. Those are what this agent has
  -- SPENT -- cumulative, monotonic, and the right number for a budget. This
  -- is what it is CARRYING: the size of the prompt its next turn will send,
  -- which goes up as the conversation grows and drops to almost nothing when
  -- it is compacted. Summing spend to guess occupancy says a seat is twelve
  -- windows deep when it is in fact a third full.
  --
  -- `context_limit_tokens` is the window that number is a fraction of, as
  -- the CLI's descriptor declares it, because the hub cannot know the window
  -- of every model every CLI might be running. Both nullable: a seat whose
  -- CLI says nothing about either is simply not measured, and is never
  -- compacted on a guess.
  context_used_tokens  INTEGER,
  context_limit_tokens INTEGER,
  -- Where the number came from, for an operator wondering why a seat was
  -- compacted -- the transcript file that was read, or the driver stream.
  context_source       TEXT,
  context_at_ms        INTEGER,
  -- What the model is doing now: active (a turn is running), idle, blocked
  -- on a question. NULL until the node has said: unknown, never assumed idle.
  activity             TEXT,
  activity_at_ms       INTEGER,
  -- Turns finished since this agent last kept anything (store_memory,
  -- store_knowledge, fold_context), and when it was last reminded to.
  -- Whether its nexmesh tools are connected (a shim is registered for it on
  -- its node): 1, 0 once it has had none for a while, NULL when the node
  -- does not say (a CLI with no MCP, a seat still coming up).
  mcp_link             INTEGER,
  turns_unkept         INTEGER NOT NULL DEFAULT 0,
  memory_reminded_at_ms INTEGER,
  -- Files it has read since it last asked the code graph, and when it was
  -- last reminded the graph is there.
  reads_unguided       INTEGER NOT NULL DEFAULT 0,
  graph_reminded_at_ms INTEGER,
  -- When the hub last told this agent to compact itself because its context
  -- was over the threshold. Per AGENT, not per seat: a respawn starts a new
  -- process on a conversation whose occupancy is measured afresh, and the
  -- previous process's compaction says nothing about this one.
  compact_asked_at_ms  INTEGER,
  -- WHEN THIS SEAT WAS LAST ASKED TO FOLD, and how full it was when it was
  -- asked. The pair makes the nudge growth-gated rather than a reminder
  -- every second: a seat is asked again only once it has taken on another
  -- quarter of a window, and a compaction waits briefly for the fold it
  -- asked for before going ahead anyway -- a full context is worse than a
  -- fold nobody wrote. See `folds` and `watchdog.rs`.
  fold_asked_at_ms     INTEGER,
  fold_baseline_tokens INTEGER,
  -- THE SAME TURN, OVER AND OVER. `loop_period` is how many turns the
  -- repeating unit is and `loop_repeats` how many times it has repeated
  -- back to back; both as the node last measured them, and both cleared
  -- when the agent does something else.
  --
  -- An agent in a loop is not a stuck process -- it is a busy one. It
  -- answers heartbeats, it burns tokens, and from the outside it looks
  -- exactly like an agent working hard. These two numbers are the
  -- difference.
  loop_period          INTEGER,
  loop_repeats         INTEGER,
  -- When the hub stopped this agent for looping. Set once per agent: the
  -- seat is paused, so there is nothing to do again, and an operator who
  -- releases it has made a decision that must not be overridden on the next
  -- reading.
  loop_stopped_at_ms   INTEGER,
  -- Was this start given a conversation to resume by the hub?
  --
  -- A start that dies seconds after being handed a resume died on OUR
  -- argument, not on its own account, and that is a defect to correct
  -- rather than a state to respect: it is retried fresh even when the seat
  -- is set not to respawn, because otherwise a bad handle leaves the
  -- operator with a seat that cannot be opened at all. The retry carries no
  -- resume, so this cannot loop.
  resumed           INTEGER NOT NULL DEFAULT 0,
  started_at_ms     INTEGER NOT NULL,
  last_seen_ms      INTEGER NOT NULL,
  exited_at_ms      INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS agents_identity
  ON agents(node_id, pid, proc_start_ticks) WHERE pid IS NOT NULL AND node_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS agents_by_slot ON agents(slot_id) WHERE exited_at_ms IS NULL;

-- ------------------------------------------------------------- messaging ----

-- Addressed slot-to-slot, not process-to-process: a message queued for a slot
-- is still there for whatever agent next occupies it, on whatever machine.
-- A file handed to the team, so the chat can show it again later.
--
-- The bytes are NOT here. They live on the machine the file was written to,
-- which is the whole point of an upload: an agent reads files, and a file an
-- agent can read is a file on its own disk. This row is how the dashboard
-- asks that machine for them again -- see `GET /v1/uploads/:id`.
CREATE TABLE IF NOT EXISTS uploads (
  id            TEXT PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  workspace_id  TEXT,
  node_id       TEXT NOT NULL,
  node_name     TEXT NOT NULL,
  root          TEXT NOT NULL,          -- the workspace mount on that machine
  rel           TEXT NOT NULL,          -- path inside it
  path          TEXT NOT NULL,          -- absolute, as the node reported it
  name          TEXT NOT NULL,
  media_type    TEXT NOT NULL,
  bytes         INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS uploads_session ON uploads(session_id, created_at_ms);

CREATE TABLE IF NOT EXISTS messages (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  from_slot       TEXT,                        -- NULL = orchestrator or human
  to_slot         TEXT,                        -- NULL = broadcast
  kind            TEXT NOT NULL DEFAULT 'text',-- text|feedback|approval|nudge|handoff|system
  body            TEXT NOT NULL,
  sent_at_ms      INTEGER NOT NULL,
  inflight_agent  TEXT,                        -- handed to a driver, awaiting ack
  inflight_at_ms  INTEGER,
  acked_at_ms     INTEGER,                     -- proven to have reached the model
  acked_by_agent  TEXT,
  -- DELIVERED is not READ. `acked_at_ms` says a message reached the model,
  -- which is what stops it being handed over again; it says nothing about
  -- anyone having dealt with it. Checking your messages used to ack them,
  -- so a glance was indistinguishable from an answer and the message was
  -- gone. A message stays UNCLAIMED until an agent explicitly claims it, or
  -- an operator clears it by hand.
  claimed_at_ms   INTEGER,
  claimed_by      TEXT,
  -- WHO TO ANSWER, and where they are. A message arriving with no return
  -- address leaves an agent one safe move -- broadcast -- which is how a note
  -- meant for one teammate comes to be read by six. For an ordinary message
  -- this is the sender; for one forwarded from the operator channel it is the
  -- seat that raised it, in its own session, and answering it is the only
  -- sanctioned way a reply crosses a session.
  reply_to_slot    TEXT,
  reply_to_session TEXT,
  -- A RECORD, not a message: the console's housekeeping lines, the node's
  -- notes about what it did while nobody was attached, an answer addressed to
  -- the operator. It belongs in the chat a person reads and must never be
  -- handed to an agent as work.
  --
  -- This is a flag of its own because the obvious shortcut is wrong: these
  -- rows are also pre-claimed, and filtering delivery on the CLAIM took a
  -- team message away from every other seat the moment one of them claimed
  -- it -- which is the one thing the delivery query's own comment warns
  -- about, since a team message's state is per RECIPIENT.
  record           INTEGER NOT NULL DEFAULT 0,
  -- When the sender was told this message is not getting through. A message
  -- to a seat whose process has died, or whose machine has gone, waits for
  -- ever and silently: the sender believes it was heard. Reported once, which
  -- is what this records.
  stalled_at_ms    INTEGER,
  -- Withdrawn by the operator before anyone saw it. A message to a seat that
  -- has gone, or one sent in error, otherwise waits for ever: nothing
  -- expires it and nothing but delivery clears it. Cancelling is not
  -- claiming -- claiming says a person has dealt with it, this says it must
  -- never arrive -- and it is not acking, which would record that a model
  -- read something it never saw.
  cancelled_at_ms  INTEGER,
  cancelled_by     TEXT
);
-- Who has had a given message, one row per SEAT.
--
-- `messages` carries one `acked_at_ms`, which is the right shape for a
-- message addressed to somebody and the wrong one for a broadcast: the first
-- teammate to ack consumed it for everyone, so "I told the team" reached
-- exactly one of them, at random, and nobody could see that it had. This is
-- what decides whether a seat still has a broadcast waiting; the columns on
-- `messages` stay as they were, recording the FIRST delivery, because that
-- is what the log and the dashboard mean by delivered.
--
-- Written lazily, when a message is handed to a seat: a seat that joins
-- after a broadcast was sent is still a recipient, and materialising rows at
-- send time would have decided otherwise.
CREATE TABLE IF NOT EXISTS message_recipients (
  message_id     INTEGER NOT NULL,
  slot_id        TEXT NOT NULL,
  inflight_agent TEXT,
  inflight_at_ms INTEGER,
  acked_at_ms    INTEGER,
  acked_by_agent TEXT,
  PRIMARY KEY (message_id, slot_id)
);

CREATE INDEX IF NOT EXISTS messages_pending ON messages(session_id, to_slot) WHERE acked_at_ms IS NULL;
-- For flood protection, which counts what one seat has sent recently on
-- every single send. Without this that count is a full scan of the table,
-- and this table is the biggest one here -- so the protection would cost
-- more than the flood.
CREATE INDEX IF NOT EXISTS messages_from_recent ON messages(from_slot, sent_at_ms);

-- --------------------------------------------------- file coordination ----

-- scope is "ws:<workspace_id>" for a shared tree, "node:<node_id>" for local
-- work. Same table serves both models; the UNIQUE index does the arbitration.
CREATE TABLE IF NOT EXISTS locks (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id     TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  scope          TEXT NOT NULL,
  path           TEXT NOT NULL,
  held_by_slot   TEXT NOT NULL REFERENCES slots(id) ON DELETE CASCADE,
  purpose        TEXT,
  acquired_at_ms INTEGER NOT NULL,
  expires_at_ms  INTEGER NOT NULL,
  UNIQUE (scope, path)
);

CREATE TABLE IF NOT EXISTS ownership (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  slot_id      TEXT NOT NULL REFERENCES slots(id) ON DELETE CASCADE,
  pattern      TEXT NOT NULL,
  PRIMARY KEY (workspace_id, slot_id, pattern)
);

CREATE TABLE IF NOT EXISTS plan_items (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ord           INTEGER NOT NULL,
  label         TEXT NOT NULL,
  assignee_slot TEXT,
  status        TEXT NOT NULL DEFAULT 'pending',  -- pending|in_progress|done|blocked
  updated_at_ms INTEGER NOT NULL
);

-- What happened the last time each machine's checkout was synced, so the
-- state of a shared workspace is visible without asking every node.
CREATE TABLE IF NOT EXISTS workspace_sync (
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  node_id      TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  at_ms        INTEGER NOT NULL,
  ok           INTEGER NOT NULL DEFAULT 0,
  conflict     INTEGER NOT NULL DEFAULT 0,
  head         TEXT,
  detail       TEXT,
  PRIMARY KEY (workspace_id, node_id)
);

-- ------------------------------------------------------------ knowledge ----

CREATE TABLE IF NOT EXISTS knowledge (
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  key           TEXT NOT NULL,
  category      TEXT NOT NULL DEFAULT 'context', -- decision|convention|discovery|blocker|context
  value         TEXT NOT NULL,
  author_slot   TEXT,
  updated_at_ms INTEGER NOT NULL,
  PRIMARY KEY (session_id, key)
);

CREATE TABLE IF NOT EXISTS guardrails (
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  id            TEXT NOT NULL,
  current_value REAL NOT NULL,
  PRIMARY KEY (session_id, id)
);

-- Append-only audit of everything the hub told a node to do. Spawning
-- processes on other people's machines deserves a paper trail.
CREATE TABLE IF NOT EXISTS audit (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  at_ms     INTEGER NOT NULL,
  actor     TEXT NOT NULL,
  node_id   TEXT,
  action    TEXT NOT NULL,
  detail    TEXT NOT NULL DEFAULT '{}'
);

-- How often each seat calls each nexmesh tool. What shows whether a team is
-- actually using its code graph and its memory rather than reading the tree
-- and forgetting: a prompt can ask for that, and only this says it happened.
CREATE TABLE IF NOT EXISTS tool_counts (
  slot_id TEXT NOT NULL,
  tool    TEXT NOT NULL,
  n       INTEGER NOT NULL DEFAULT 0,
  last_ms INTEGER NOT NULL,
  PRIMARY KEY (slot_id, tool)
);
