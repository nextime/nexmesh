-- nexmesh hub state. One writer (the hub); SQLite in WAL mode is sufficient.
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------- fleet ----

CREATE TABLE IF NOT EXISTS nodes (
  id               TEXT PRIMARY KEY,
  name             TEXT NOT NULL UNIQUE,     -- "zeiss", "sissy"
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

-- Credentials for operators: the people (and scripts) that drive the fleet.
-- An operator token is the most powerful credential here -- it can create
-- sessions, spawn processes on any node and attach to any agent's terminal --
-- so like the others only its hash is stored.
CREATE TABLE IF NOT EXISTS operator_tokens (
  token_sha256  TEXT PRIMARY KEY,
  label         TEXT NOT NULL,
  issued_at_ms  INTEGER NOT NULL,
  revoked_at_ms INTEGER,
  last_used_ms  INTEGER
);

-- Credentials for external (non-CLI, non-node) participants. Deliberately a
-- different table from node_tokens: a participant token may call team tools
-- for its own slot and nothing else -- in particular it can never spawn a
-- process on anybody's machine.
CREATE TABLE IF NOT EXISTS participant_tokens (
  token_sha256  TEXT PRIMARY KEY,
  label         TEXT NOT NULL,
  session_id    TEXT,
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
  last_active_ms INTEGER NOT NULL
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
  placement         TEXT NOT NULL DEFAULT '{}',      -- {"node":"zeiss"} | {"labels":["gpu"]} | {"any":true}
  display_name      TEXT,
  role              TEXT,
  role_description  TEXT,
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
  resume_handle     TEXT,                            -- CLI-specific resume id
  -- For an ADOPTED slot: the tmux target (session, window or pane) whose
  -- terminal this slot mirrors.
  tmux_target       TEXT,
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
  -- Whether this agent's driver can take a message mid-turn. Decides whether
  -- the hub pushes to it or waits for its next tool result.
  push_capable      INTEGER NOT NULL DEFAULT 0,
  status            TEXT NOT NULL,             -- starting|running|idle|exited|lost
  -- Liveness for agents with no process we can see: refreshed by the
  -- participant API, swept like a node lease.
  lease_expires_ms  INTEGER,
  kind_hint         TEXT,                      -- free-form: "hermes", "python", ...
  summary           TEXT NOT NULL DEFAULT '',
  cwd               TEXT,
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
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
  acked_by_agent  TEXT
);
CREATE INDEX IF NOT EXISTS messages_pending ON messages(session_id, to_slot) WHERE acked_at_ms IS NULL;

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
