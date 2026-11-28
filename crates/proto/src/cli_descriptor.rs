//! Declarative agent-CLI support.
//!
//! Adding a CLI must not mean writing Rust. A descriptor file says how to
//! detect the binary, how to register our MCP shim with it, and how to launch
//! it in each mode. Delivery then has three tiers:
//!
//!   `mcp_only`     -- works for ANY CLI that speaks MCP, zero bespoke code.
//!                     Messages ride in on the next tool result.
//!   built-in       -- `stream_json` (Claude), `app_server` (Codex): mid-turn
//!                     injection, exact token accounting.
//!   `plugin`       -- an external helper binary speaking our driver protocol
//!                     on stdio, for a CLI with its own quirks.
//!
//! The floor is what matters: a CLI nobody has ever heard of still works at
//! `mcp_only`, and the fast paths are an optimisation, not a requirement.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliDescriptor {
    pub cli: CliMeta,
    #[serde(default)]
    pub mcp: McpRegistration,
    #[serde(default)]
    pub modes: BTreeMap<String, ModeSpec>,
    #[serde(default)]
    pub driver: DriverSpec,
    /// How to add a server to this CLI's own configuration, for the
    /// orchestrator. Absent means nexmesh cannot install itself there.
    #[serde(default)]
    pub user_install: Option<UserInstall>,
    /// Give each SEAT its own copy of this CLI's data directory.
    ///
    /// Why: a CLI that keeps its conversations in one shared store, and
    /// tells nobody which conversation a given process is on, cannot be
    /// resumed per seat. Verified against the real thing rather than
    /// assumed -- codex 0.169 and opencode 1.19 both:
    ///   * export no conversation id to their MCP children,
    ///   * take no conversation NAME at launch (unlike claude's
    ///     `--session-id`), and
    ///   * record every seat's conversations in one store with no column
    ///     that identifies the process.
    /// Two opencode seats spawned together were 610ms apart with the same
    /// cwd, so neither time nor directory can tell them apart, and resuming
    /// the WRONG conversation is worse than starting a fresh one.
    ///
    /// With a store per seat, "continue the last conversation" becomes
    /// unambiguous -- it is that seat's -- and no id has to be learned,
    /// recorded or guessed at. That is what `resume_latest_args` uses.
    #[serde(default)]
    pub session_store: Option<SessionStore>,
    /// Keep this CLI alive when the NODE restarts.
    ///
    /// An interactive agent normally runs on a pty the node owns, as the
    /// node's own child. That makes the node's lifetime the agent's: stop
    /// the node and the master closes, the slave sends SIGHUP, and the CLI
    /// dies with whatever it was doing. Deploying a new node binary
    /// therefore ends every conversation on that machine, which is a poor
    /// thing to have to think about before every deploy.
    ///
    /// `kind = "tmux"` puts the CLI inside a tmux session instead. The tmux
    /// server owns the real pty and outlives the node; the node attaches a
    /// SECOND, grouped client under a pty of its own -- the same mechanism
    /// adoption already uses -- so everything downstream of the pty is
    /// unchanged. Restart the node and it finds the session still there and
    /// attaches to it again, with the CLI none the wiser.
    ///
    /// Opt-in per CLI, because it moves the agent's process out from under
    /// the node and that is not a change to make silently to every seat on
    /// a fleet at once.
    #[serde(default)]
    pub supervise: Option<Supervise>,
    /// How to tell this CLI that a directory is trusted, when it refuses to
    /// start in one it has not been told about.
    ///
    /// claude asks "do you trust the files in this folder?" as a modal
    /// question, and no automation can answer it: the seat is simply stuck,
    /// and from the outside it looks like a CLI that will not start. The
    /// operator chose that directory when they made the seat, so running an
    /// agent there is the decision -- this just records it where the CLI
    /// looks.
    ///
    /// It is said out loud the first time, because marking a directory
    /// trusted on somebody's behalf is not a thing to do silently.
    #[serde(default)]
    pub trust: Option<TrustPath>,
    /// Where this CLI reads SKILLS from, if it has such a thing.
    ///
    /// Registering the MCP server gives an agent the tools; it does not tell
    /// it how a team works. Which tool answers the operator rather than the
    /// room, that knowledge is worth writing down for the next seat, that a
    /// capability it lacks can be asked for -- none of that is derivable
    /// from a tool list, and an agent that has to infer it gets it wrong in
    /// the same few ways every time. So the installer writes a skill beside
    /// the registration.
    ///
    /// Absent means this CLI has no skill mechanism, and nothing is written.
    #[serde(default)]
    pub skill: Option<SkillInstall>,
    /// How full this CLI's context is, and how big it is.
    ///
    /// Absent means this CLI is not measured, and a seat running it is never
    /// compacted automatically -- which is the right default for a CLI
    /// nobody has checked, because the alternative is typing a slash command
    /// at an agent on the strength of a number that might mean anything.
    #[serde(default)]
    pub context: Option<ContextSpec>,
}

/// How to tell how close a conversation is to its limit.
///
/// Two separate facts, and they come from different places. How FULL the
/// context is can only be learnt from the running CLI -- so the node reads
/// it. How full it is ALLOWED to get is a property of the model, which no
/// CLI reports and which changes under us as models are released, so it is
/// declared here where an operator can correct it without a new binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSpec {
    /// The context window a conversation with this CLI has to fit in, in
    /// tokens. The fallback: `models` wins when it has an answer.
    pub limit_tokens: i64,
    /// Where the node reads current occupancy from.
    #[serde(default)]
    pub source: ContextSource,
    /// Per-model windows, because the window belongs to the MODEL and not to
    /// the CLI: one `claude` binary runs several, and they do not agree.
    ///
    /// Matched against the model the agent REPORTS it is running -- exactly
    /// first, then by the longest key that is a prefix of it, so
    /// `claude-opus-4` covers `claude-opus-4-8` without an entry per point
    /// release. No match falls back to `limit_tokens`.
    #[serde(default)]
    pub models: BTreeMap<String, i64>,
}

/// Where a node reads how full a conversation is.
///
/// One real source so far. A CLI whose driver reports per-call usage on a
/// pipe could be read without touching the disk at all, and that is a
/// variant and a match arm when there is a CLI to try it against -- the two
/// that would qualify, codex and opencode, declare no compaction command, so
/// measuring them would produce a number and no way to act on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// Not measurable. Declared explicitly so a descriptor can say "this CLI
    /// has a window, and no, we cannot see into it" -- which is different
    /// from saying nothing and is worth being able to write down.
    #[default]
    None,
    /// The last usage record in the conversation file `session_path` names.
    ///
    /// claude writes one JSON object per line and stamps every assistant
    /// message with the usage of the API call that produced it. The prompt
    /// of that call IS the conversation as it stood, so the newest such
    /// record is the occupancy -- no estimating, no counting tokens
    /// ourselves, and it is the same number the CLI shows its user.
    ClaudeTranscript,
    /// The newest `token_count` event in the seat's own codex rollout file
    /// (`<store>/sessions/YYYY/MM/DD/rollout-*.jsonl`). codex writes the
    /// last call's input and output and the model's context window into the
    /// same record, so the window is read rather than declared; `limit_tokens`
    /// is only the fallback for a record that leaves it out.
    CodexRollout,
    /// The newest assistant message with token counts in the seat's own
    /// opencode database (`<store>/opencode/opencode.db`). opencode records
    /// no window, so it comes from `models` by the message's `modelID`.
    OpencodeDb,
}

impl ContextSpec {
    /// The window for the model this agent is actually running.
    ///
    /// `model` is what the agent REPORTED, not what the seat asked for: a
    /// CLI may substitute or fall back, and a percentage against the window
    /// of a model that is not running is a number with nothing behind it.
    pub fn limit_for(&self, model: Option<&str>) -> i64 {
        let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
            return self.limit_tokens;
        };
        if let Some(exact) = self.models.get(model) {
            return *exact;
        }
        self.models
            .iter()
            .filter(|(k, _)| model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
            .unwrap_or(self.limit_tokens)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Supervise {
    /// Only `tmux` today. Anything else is ignored rather than fatal, so a
    /// descriptor written for a newer node still starts on an older one --
    /// it just starts the old way.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustPath {
    /// The CLI's own config file, e.g. `~/.claude.json`.
    pub file: String,
    /// A JSON pointer to the flag, with `{cwd}` standing for the working
    /// directory. Slashes inside it are escaped as JSON pointers require.
    pub pointer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStore {
    /// The environment variable through which this CLI is told where to keep
    /// its data, e.g. `CODEX_HOME`, `XDG_DATA_HOME`.
    pub env: String,
    /// Where the CLI actually keeps its data, relative to that variable.
    /// Empty for a variable that IS the store (`CODEX_HOME`); `opencode` for
    /// one that is a parent of it (`XDG_DATA_HOME`).
    #[serde(default)]
    pub store_rel: String,
    /// The shared store this seat's one is branching from. Entries named in
    /// `share` are linked out of here.
    pub root: String,
    /// What to SHARE with the default store rather than isolate: credentials,
    /// configuration, installed packages, skills. Anything not listed is per
    /// seat, which is the point.
    ///
    /// An entry that does not exist is skipped, so one list can serve several
    /// versions of a CLI.
    #[serde(default)]
    pub share: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstall {
    /// The directory of skill folders, e.g. `~/.claude/skills`. The
    /// installer writes `<dir>/<name>/SKILL.md`.
    pub dir: String,
    /// The folder name to write. Defaults to `nexmesh`.
    #[serde(default = "default_skill_name")]
    pub name: String,
}

fn default_skill_name() -> String {
    "nexmesh".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliMeta {
    pub id: String,
    pub display_name: String,
    /// Binary name, looked up on PATH; may be an absolute path.
    pub command: String,
    #[serde(default)]
    pub version_args: Vec<String>,
    /// Extra places to look when the binary is not on a login-shell PATH
    /// (npm-global, ~/.local/bin, ~/.opencode/bin -- all real cases here).
    #[serde(default)]
    pub extra_paths: Vec<String>,
    /// How to ask this CLI for a particular model, with `{model}` substituted.
    /// Empty means it cannot be told on the command line -- which is not the
    /// same as cannot be told at all: Codex takes the model in its own
    /// protocol instead, and a plugin may do the same.
    #[serde(default)]
    pub model_args: Vec<String>,
    /// Environment to SET for this CLI.
    ///
    /// What makes a named configuration possible: two descriptors can run the
    /// same binary as the same user against different credentials, by pointing
    /// each at its own config directory. `CLAUDE_CONFIG_DIR` for Claude Code,
    /// `CODEX_HOME` for Codex, or `HOME` as the blunt instrument that works
    /// for anything.
    ///
    /// A leading `~/` is expanded against the spawning user's home, because a
    /// descriptor is shared across machines and an absolute path is not.
    ///
    /// It cannot override the NEXMESH_* variables the shim needs to find its
    /// node: those are applied afterwards and win.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The account this CLI signs in as, for grouping seats that share one
    /// allowance: `anthropic:work`, `openai:team`. Free text; seats whose
    /// descriptors name the same account are held together when it runs out,
    /// on whatever machine they run. Absent, every seat of this descriptor on
    /// one machine is taken to share one login -- which is what one config
    /// directory means.
    #[serde(default)]
    pub account: Option<String>,
    /// Environment variables to CLEAR for this CLI. A node started from inside
    /// tmux inherits `$TMUX`, and a tmux client that sees it refuses to run
    /// ("sessions should be nested with care"), so adoption would fail
    /// depending on how the node happened to be launched.
    #[serde(default)]
    pub env_unset: Vec<String>,
}

/// How to make this CLI load our MCP shim.
///
/// Each CLI wants a different JSON/TOML shape in a different place, and those
/// shapes change. Keeping the shape here, as DATA, means supporting a new CLI
/// or tracking a schema change is a descriptor edit -- never a Rust change and
/// never a guess baked into code. (opencode, for instance, takes
/// `mcp.servers.<name>` with `command` as an ARRAY and env under
/// `environment`, while Claude takes `mcpServers.<name>` with `command` as a
/// STRING plus `args`, and env under `env`.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum McpRegistration {
    /// Pass a generated config file on the command line. Always preferred: it
    /// cannot corrupt the user's own CLI configuration.
    InlineFlag {
        flag: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Merge into a JSON config. A relative `path` is a project file inside the
    /// workspace; a `~/` path is the user's global config.
    JsonFile {
        path: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Merge into a TOML config under a dotted key (Codex).
    TomlFile {
        path: String,
        key: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    #[default]
    None,
}

/// Convert a descriptor's TOML entry template into JSON, substituting
/// `{mcpd}` with the shim's path and injecting environment variables under
/// whichever key this CLI expects.
pub fn render_entry(
    entry: &toml::Value,
    env_key: Option<&str>,
    mcpd_path: &str,
    env: &[(String, String)],
) -> serde_json::Value {
    let mut json = toml_to_json(entry, mcpd_path);
    if let (Some(key), Some(obj)) = (env_key, json.as_object_mut()) {
        let map: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        obj.insert(key.to_string(), serde_json::Value::Object(map));
    }
    json
}

fn toml_to_json(v: &toml::Value, mcpd_path: &str) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        toml::Value::String(s) => J::String(s.replace("{mcpd}", mcpd_path)),
        toml::Value::Integer(i) => J::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(a) => J::Array(a.iter().map(|x| toml_to_json(x, mcpd_path)).collect()),
        toml::Value::Table(t) => J::Object(
            t.iter().map(|(k, x)| (k.clone(), toml_to_json(x, mcpd_path))).collect(),
        ),
    }
}

/// Set a value at an RFC 6901 JSON Pointer, creating intermediate objects.
/// Used to merge our entry into a config we do not own, leaving the rest of
/// the user's settings untouched.
pub fn set_pointer(
    root: &mut serde_json::Value,
    pointer: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let parts: Vec<String> = pointer
        .trim_start_matches('/')
        .split('/')
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    if parts.is_empty() || parts[0].is_empty() {
        return Err("empty JSON pointer".into());
    }
    if !root.is_object() {
        *root = serde_json::json!({});
    }
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()
            .ok_or("pointer traverses a non-object")?
            .entry(part.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    cur.as_object_mut()
        .ok_or("pointer traverses a non-object")?
        .insert(parts[parts.len() - 1].clone(), value);
    Ok(())
}

/// How to register a server in the USER's own CLI configuration, so it is
/// there every time they start that CLI by hand.
///
/// Distinct from `McpRegistration`, which is about the throwaway config a node
/// writes for an agent it is about to spawn. This one edits something the user
/// owns, so where a CLI offers its own command we use it rather than guess at
/// a file format that may change under us.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum UserInstall {
    /// Let the CLI do it: `<cli> mcp add --scope user <name> -- <command>`.
    Command {
        argv: Vec<String>,
        /// Flag used to pass one environment variable, repeated per variable
        /// (`-e KEY=value`). Absent means the CLI takes no env.
        #[serde(default)]
        env_flag: Option<String>,
    },
    /// Merge into a JSON config ourselves, for a CLI whose `mcp add` is
    /// interactive and so unusable from a script.
    JsonFile {
        path: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Same, for TOML.
    TomlFile {
        path: String,
        key: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
}

// `Default` so a test -- or a caller describing one aspect of a mode -- can
// name the field it cares about. Every field is already `#[serde(default)]`
// or required by a descriptor, so a defaulted ModeSpec is the same "nothing
// declared" a minimal descriptor produces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModeSpec {
    /// Argv after the command. `{prompt}`, `{cwd}`, `{session}`, `{resume}`
    /// and `{mcp_config}` are substituted.
    #[serde(default)]
    pub args: Vec<String>,
    /// Write the prompt to stdin instead of passing it as an argument.
    #[serde(default)]
    pub prompt_on_stdin: bool,
    /// Argv fragment added when resuming a previous session.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Argv fragment that CHOOSES the conversation id at a first spawn, with
    /// `{session_uuid}` standing for a fresh one.
    ///
    /// Without this, an interactive seat loses its conversation whenever its
    /// process is replaced: the id is only learned from a CLI's structured
    /// output, and a TUI emits none, so there was never anything to resume
    /// with. A node restart therefore started every seat's thinking over --
    /// files and fleet state survived, the conversation did not. Naming the
    /// id ourselves at the first spawn makes `resume_args` usable at the
    /// second.
    #[serde(default)]
    pub session_id_args: Vec<String>,
    /// How to continue THIS SEAT's last conversation, for a CLI that offers
    /// "the last one" but will not name conversations: codex
    /// `resume --last`, opencode `--continue`.
    ///
    /// Only used when the seat has a store of its own (`session_store`) and
    /// that store already holds something -- otherwise "the last one" is
    /// whatever another seat happened to do most recently, which is the
    /// mistake this avoids rather than makes.
    #[serde(default)]
    pub resume_latest_args: Vec<String>,
    /// How to give this CLI a system prompt INSTEAD of its own.
    ///
    /// `{prompt}` is the text. Absent means this CLI cannot be told to
    /// replace its system prompt, and asking for it is refused rather than
    /// quietly ignored -- a seat that believes it has been given a prompt and
    /// has not is worse than one that was told no.
    #[serde(default)]
    pub system_prompt_args: Vec<String>,
    /// How to ADD to this CLI's own system prompt. `{prompt}` is the text.
    ///
    /// Almost always the one to use: the CLI's own prompt carries how to use
    /// its tools, and replacing it to add a sentence about what a seat is for
    /// throws that away.
    #[serde(default)]
    pub append_system_prompt_args: Vec<String>,
    /// How to ADD to the system prompt through the ENVIRONMENT, for a CLI
    /// that reads instructions from a config it takes inline (opencode's
    /// `OPENCODE_CONFIG_CONTENT`). Values take the same placeholders as the
    /// args. Used with or instead of `append_system_prompt_args`.
    ///
    /// Placeholders, in args and here: `{prompt}` the text as it is,
    /// `{prompt_toml}` and `{prompt_json}` the text as a quoted TOML or JSON
    /// string (codex's `-c key=value` parses its value as TOML), and
    /// `{prompt_file}` the path of a file the node writes the text into, in
    /// the seat's own directory, for a CLI that takes instructions by path
    /// (it is named `AGENTS.md`), `{prompt_dir}` that file's directory, and
    /// `{prompt_plugin_dir}` the same directory laid out as a Cursor plugin.
    #[serde(default)]
    pub append_system_prompt_env: BTreeMap<String, String>,
    /// Where this CLI keeps a named conversation on disk, so a handle can be
    /// CHECKED before it is used.
    ///
    /// `{id}` is the conversation id. `{cwd_slug}` is the working directory
    /// with every `/` replaced by `-`, which is how claude names the project
    /// directory a conversation belongs to (`/home/dev` ->
    /// `-home-dev`).
    ///
    /// Why this is needed: `session_id_args` NAMES a conversation at spawn
    /// and the handle is recorded immediately -- before the CLI has created
    /// anything. If that start then fails (a bad flag, an auth prompt, a
    /// terminal it would not accept), the handle points at a conversation
    /// that never existed, and the next spawn runs `--resume <it>`, which
    /// claude answers with "No conversation found with session ID" and
    /// exits at once. That is a seat that cannot start, with an error
    /// nobody would connect to nexmesh having named it.
    ///
    /// The same check catches a conversation created in another directory or
    /// cleaned up since: `--resume` only finds one in the project directory
    /// it is launched in.
    #[serde(default)]
    pub session_path: Option<String>,
    /// What to type at this CLI to make it compact its own context now.
    ///
    /// Compaction is a slash command inside the TUI in every CLI checked --
    /// none of them takes a flag for it -- so this is literally what a
    /// person would type. Unset means this CLI does not declare one, and
    /// `/compact <agent>` says so rather than typing a guess: a wrong slash
    /// command is mostly harmless, but the neighbouring ones (`/clear`,
    /// `/new`) are not, and guessing in that neighbourhood is not worth it.
    #[serde(default)]
    pub compact_command: Option<String>,
    /// What to type at this CLI to STOP what it is doing, without ending it.
    ///
    /// Raw keys, not a command: interrupting is a keystroke in every TUI
    /// checked, and it has to work when the CLI is mid-turn and not reading
    /// a line at all -- which is exactly when it needs to. Escape for
    /// claude.
    ///
    /// Unset means this CLI cannot be interrupted as far as nexmesh knows,
    /// and a seat of it is paused without one rather than having something
    /// plausible typed at it. The keys next to the right one end the
    /// process or clear the conversation.
    #[serde(default)]
    pub interrupt_keys: Option<String>,
    /// Does this mode give a human-usable terminal when attached?
    #[serde(default)]
    pub attachable: bool,
    /// Is this mode the agent's OWN prompt, so that a line typed at it is a
    /// message to the agent rather than a command to a shell?
    ///
    /// Only meaningful for a terminal the node spawned itself. It is what
    /// lets a CLI with no driver be reached at all: without it such a seat
    /// can be watched and attached to, and nothing the team says ever
    /// arrives, because its only route is a tool call it may never make.
    ///
    /// Default false, and deliberately so. `tmux` is a registered CLI whose
    /// interactive mode is a SHELL, where a teammate's message and a newline
    /// would be executed; so is anything else whose prompt is not an agent's.
    /// A CLI claims this in its descriptor or does not get typed at.
    #[serde(default)]
    pub typed_delivery: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverSpec {
    /// Universal floor: delivery piggybacks on MCP tool results.
    #[default]
    McpOnly,
    /// Newline-delimited JSON on stdio, Claude Code's `stream-json` shape.
    StreamJson {
        #[serde(default)]
        replay_user_messages: bool,
    },
    /// JSON-RPC app-server with threads and turns, Codex's shape.
    AppServer,
    /// The CLI runs its own HTTP server and we drive it over that.
    ///
    /// opencode works this way: `opencode serve` exposes a documented API with
    /// native mid-turn steering, so there is no stdio protocol to parse and no
    /// pty to hold. It also means the same agent can be attached to as a real
    /// TUI on demand, which a stdio-driven agent cannot offer.
    HttpServer {
        /// Username for HTTP Basic auth on that server.
        #[serde(default)]
        basic_auth_user: Option<String>,
        /// Environment variable through which to hand it a password. A
        /// per-agent secret stops anyone else with a shell on the machine
        /// driving the agent through its own loopback port.
        #[serde(default)]
        password_env: Option<String>,
        /// Command that opens a terminal onto a running server, for attach.
        #[serde(default)]
        attach_args: Vec<String>,
    },
    /// External helper binary speaking the driver protocol on stdio.
    Plugin { command: String, #[serde(default)] args: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum DescriptorError {
    #[error("parsing cli descriptor: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("descriptor {0} declares no launch modes")]
    NoModes(String),
    #[error("descriptor {0}: a plugin driver needs a command to run")]
    NoPluginCommand(String),
    #[error("descriptor {0}: headless mode '{1}' has no way to deliver the prompt \
(needs a {{prompt}} placeholder, prompt_on_stdin, or a driver with its own prompt channel)")]
    NoPromptPath(String, String),
}

impl CliDescriptor {
    pub fn parse(text: &str) -> Result<Self, DescriptorError> {
        let d: CliDescriptor = toml::from_str(text)?;
        if d.modes.is_empty() {
            return Err(DescriptorError::NoModes(d.cli.id.clone()));
        }
        // Caught here rather than at spawn time, where the symptom would be an
        // agent that starts and then quietly has no driver.
        if let DriverSpec::Plugin { command, .. } = &d.driver {
            if command.trim().is_empty() {
                return Err(DescriptorError::NoPluginCommand(d.cli.id.clone()));
            }
        }
        // An interactive mode may legitimately take no arguments at all --
        // bare `codex` or `gemini` opens the TUI. Only a HEADLESS mode has to
        // prove it can get the first prompt in: via a {prompt} placeholder,
        // via stdin, or through a driver protocol that carries prompts itself.
        for (name, m) in &d.modes {
            if name != "headless" {
                continue;
            }
            let driver_carries_prompt = !matches!(d.driver, DriverSpec::McpOnly);
            let via_argv = m.args.iter().any(|a| a.contains("{prompt}"));
            if !driver_carries_prompt && !via_argv && !m.prompt_on_stdin {
                return Err(DescriptorError::NoPromptPath(d.cli.id.clone(), name.clone()));
            }
        }
        Ok(d)
    }

    /// The MCP registration for this CLI, if any.
    pub fn mcp(&self) -> &McpRegistration {
        &self.mcp
    }

    pub fn user_install(&self) -> Option<&UserInstall> {
        self.user_install.as_ref()
    }

    /// Can a human take over a session in this mode, or only watch it?
    pub fn attachable(&self, mode: &str) -> bool {
        self.modes.get(mode).map(|m| m.attachable).unwrap_or(false)
    }

    /// Expand an argv template. Unknown placeholders are left alone so a CLI's
    /// own `{}` syntax is not mangled.
    pub fn render_args(&self, mode: &str, vars: &BTreeMap<&str, String>) -> Vec<String> {
        let Some(spec) = self.modes.get(mode) else { return vec![] };
        spec.args
            .iter()
            .map(|a| {
                let mut s = a.clone();
                for (k, v) in vars {
                    s = s.replace(&format!("{{{k}}}"), v);
                }
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"
[cli]
id = "claude"
display_name = "Claude Code"
command = "claude"
version_args = ["--version"]
extra_paths = ["~/.local/bin"]

[mcp]
style = "inline_flag"
flag = "--mcp-config"
pointer = "/mcpServers/nexmesh"
env_key = "env"
entry = { command = "{mcpd}", args = [] }

[modes.headless]
args = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--mcp-config", "{mcp_config}"]
prompt_on_stdin = true
resume_args = ["--resume", "{resume}"]
attachable = false

[modes.interactive]
args = ["--mcp-config", "{mcp_config}"]
attachable = true

[driver]
kind = "stream_json"
replay_user_messages = true
"#;

    #[test]
    fn a_descriptor_can_carry_its_own_environment() {
        // Two named configurations, one binary, one user, different logins.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "claude-work"
display_name = "Claude Code (work account)"
command = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
[modes.interactive]
args = []
attachable = true
"#,
        )
        .unwrap();
        assert_eq!(d.cli.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
                   Some("~/.claude-work"));
        assert_eq!(d.cli.command, "claude", "the same binary");
    }

    #[test]
    fn a_descriptor_with_no_environment_is_still_valid() {
        let d = CliDescriptor::parse(CLAUDE).unwrap();
        assert!(d.cli.env.is_empty());
    }

    #[test]
    fn parses_a_real_descriptor() {
        let d = CliDescriptor::parse(CLAUDE).unwrap();
        assert_eq!(d.cli.id, "claude");
        assert_eq!(d.driver, DriverSpec::StreamJson { replay_user_messages: true });
        assert!(d.attachable("interactive"));
        assert!(!d.attachable("headless"));
    }

    #[test]
    fn unknown_cli_defaults_to_the_universal_floor() {
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "somenewcli"
display_name = "Some New CLI"
command = "somenewcli"
[modes.interactive]
args = ["chat"]
attachable = true
"#,
        )
        .unwrap();
        assert_eq!(d.driver, DriverSpec::McpOnly);
        assert!(matches!(d.mcp, McpRegistration::None));
    }

    #[test]
    fn substitutes_only_known_placeholders() {
        let d = CliDescriptor::parse(CLAUDE).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("mcp_config", "/tmp/x.json".to_string());
        let args = d.render_args("headless", &vars);
        assert!(args.contains(&"/tmp/x.json".to_string()));
        assert!(!args.iter().any(|a| a.contains('{')));
    }

    #[test]
    fn rejects_a_headless_mode_that_cannot_receive_a_prompt() {
        // mcp_only driver, no {prompt}, no stdin -- the agent would start with
        // no task at all. Better to fail here than at spawn time.
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "nopath"
display_name = "No Path"
command = "nopath"
[modes.headless]
args = ["run"]
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoPromptPath(_, _)), "got {err}");
    }

    #[test]
    fn allows_an_argless_interactive_mode() {
        // `codex` and `gemini` with no args open their TUI; that is valid.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "tuionly"
display_name = "TUI Only"
command = "tuionly"
[modes.interactive]
args = []
attachable = true
"#,
        )
        .unwrap();
        assert!(d.attachable("interactive"));
    }

    #[test]
    fn allows_a_headless_mode_whose_driver_carries_the_prompt() {
        // Codex takes no prompt on argv: it arrives over the app-server RPC.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "rpcdriven"
display_name = "RPC Driven"
command = "rpcdriven"
[modes.headless]
args = ["app-server"]
[driver]
kind = "app_server"
"#,
        )
        .unwrap();
        assert_eq!(d.driver, DriverSpec::AppServer);
    }

    #[test]
    fn a_plugin_driver_must_name_its_helper() {
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "hollow"
display_name = "Hollow"
command = "hollow"
[modes.interactive]
args = []
attachable = true
[driver]
kind = "plugin"
command = "  "
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoPluginCommand(_)), "got {err}");
    }

    #[test]
    fn a_plugin_driver_parses_with_its_command_and_args() {
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "quirky"
display_name = "Quirky"
command = "quirky"
[modes.headless]
args = ["--serve"]
[modes.interactive]
args = []
attachable = true
[driver]
kind = "plugin"
command = "quirky-plugin"
args = ["--verbose"]
"#,
        )
        .unwrap();
        assert_eq!(
            d.driver,
            DriverSpec::Plugin {
                command: "quirky-plugin".into(),
                args: vec!["--verbose".into()]
            }
        );
    }

    #[test]
    fn a_plugin_headless_mode_needs_no_prompt_placeholder() {
        // The prompt reaches the CLI through the plugin's hello frame, so
        // requiring {prompt} on argv would reject every plugin descriptor.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "viaplugin"
display_name = "Via Plugin"
command = "viaplugin"
[modes.headless]
args = []
[driver]
kind = "plugin"
command = "viaplugin-driver"
"#,
        )
        .unwrap();
        assert!(matches!(d.driver, DriverSpec::Plugin { .. }));
    }

    #[test]
    fn rejects_a_descriptor_with_no_modes() {
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "broken"
display_name = "Broken"
command = "broken"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoModes(_)));
    }
}

/// Validates every descriptor shipped in the repo's `clis/` directory, so a
/// typo in a TOML file fails the build rather than a spawn at 3am.
#[cfg(test)]
mod shipped_descriptors {
    use super::*;

    #[test]
    fn every_shipped_descriptor_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("clis/ directory must exist") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let d = CliDescriptor::parse(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                Some(d.cli.id.as_str()),
                path.file_stem().and_then(|s| s.to_str()),
                "{}: cli.id must match the filename",
                path.display()
            );
            assert!(
                d.modes.contains_key("headless") || d.modes.contains_key("interactive"),
                "{}: needs a headless or interactive mode",
                path.display()
            );
            seen += 1;
        }
        assert!(seen >= 5, "expected the shipped descriptors, found {seen}");
    }

    /// An interactive seat must not be launched into a CLI that will stop
    /// and ask a human for permission, because there is no human at it: the
    /// message a teammate typed sits in the composer behind the dialog and
    /// the Enter answers the dialog instead of submitting. Whatever each CLI
    /// calls that switch, a descriptor that offers typed delivery has to
    /// carry it.
    #[test]
    fn a_typed_interactive_mode_does_not_stop_to_ask_permission() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis");
        // What each CLI's own unattended switch looks like on its command
        // line. A CLI with no such flag is listed here deliberately, so
        // adding one is a decision rather than an oversight.
        let unattended: &[(&str, &[&str])] = &[
            ("codex", &["--ask-for-approval", "never"]),
            ("qwen", &["--approval-mode", "auto-edit"]),
            // `--yolo` rather than `--approval-mode`, whose mode NAMES the
            // two forks spell differently (`auto-edit` above, `auto_edit`
            // in gemini's own table).
            ("gemini", &["--yolo"]),
            ("cursor", &["--force", "--approve-mcps"]),
            // The only thing freebuff stops to ask is whether this
            // directory is trusted, and until it is answered it loads no
            // mcp.json -- so the seat would come up with no team tools and
            // no visible reason why.
            ("freebuff", &["--trust-agents"]),
            // Copilot asks before each tool, path and URL; `--allow-all` is
            // the documented sum of the three (docs, not run here).
            ("copilot", &["--allow-all"]),
            // These take their permissions from their own configuration,
            // not from a flag nexmesh passes.
            ("claude", &[]),
            ("opencode", &[]),
            ("hermes", &[]),
        ];
        for entry in std::fs::read_dir(dir).expect("clis/ directory must exist") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let d = CliDescriptor::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let Some(m) = d.modes.get("interactive") else { continue };
            if !m.typed_delivery {
                continue;
            }
            let Some((_, want)) = unattended.iter().find(|(id, _)| *id == d.cli.id) else {
                panic!(
                    "{}: offers typed delivery but is not in the table above. \
                     Say what its unattended switch is, or say it has none by \
                     listing it with an empty one -- a seat nobody can answer \
                     for, launched into a CLI that stops to ask, is the one \
                     failure that looks like an agent ignoring you.",
                    path.display()
                );
            };
            for flag in *want {
                assert!(
                    m.args.iter().any(|a| a == flag),
                    "{}: an interactive mode that is typed at must pass {flag}",
                    path.display()
                );
            }
        }
    }

    /// The named-configuration examples are not loaded by a node -- `clis/`
    /// is not scanned recursively -- so nothing else would catch a typo in
    /// them until somebody copied one and it was skipped at startup.
    #[test]
    fn every_example_profile_parses_and_carries_an_environment() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis/profiles");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("clis/profiles must exist") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let d = CliDescriptor::parse(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                Some(d.cli.id.as_str()),
                path.file_stem().and_then(|s| s.to_str()),
                "{}: cli.id must match the filename, since the id is what a slot names",
                path.display()
            );
            assert!(
                !d.cli.env.is_empty(),
                "{}: a named configuration with no environment is the same login twice",
                path.display()
            );
            // The whole point is a different config directory for the same
            // binary, so it must not have been renamed too.
            assert!(
                !d.cli.command.contains("-work"),
                "{}: it should run the real binary, not a renamed one",
                path.display()
            );
            seen += 1;
        }
        assert!(seen >= 4, "expected an example per CLI, found {seen}");
    }

    #[test]
    fn at_least_one_mode_per_cli_is_attachable() {
        // The user must be able to reach any agent's CLI from any machine, so
        // every supported CLI needs a mode a human can actually drive.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let d = CliDescriptor::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                d.modes.values().any(|m| m.attachable),
                "{}: no attachable mode",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod registration {
    use super::*;

    fn desc(name: &str) -> CliDescriptor {
        let path = format!("{}/../../clis/{name}.toml", env!("CARGO_MANIFEST_DIR"));
        CliDescriptor::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn claude_entry_uses_a_string_command_and_env() {
        let d = desc("claude");
        let McpRegistration::InlineFlag { pointer, entry, env_key, flag } = d.mcp() else {
            panic!("claude should use an inline flag so it never touches ~/.claude.json");
        };
        assert_eq!(flag, "--mcp-config");
        assert_eq!(pointer, "/mcpServers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd",
            &[("NEXMESH_AGENT_ID".into(), "ag-1".into())]);
        assert_eq!(j["command"], "/usr/bin/nexmesh-mcpd");
        assert!(j["args"].is_array());
        assert_eq!(j["env"]["NEXMESH_AGENT_ID"], "ag-1");
    }

    #[test]
    fn opencode_entry_uses_an_array_command_and_environment() {
        // The two CLIs disagree on both the key names and the type of
        // `command`. Getting this wrong means the shim silently never loads.
        let d = desc("opencode");
        let McpRegistration::JsonFile { path, pointer, entry, env_key } = d.mcp() else {
            panic!("opencode should merge into a project json file");
        };
        assert_eq!(path, "opencode.json", "relative => project file, not the user's global config");
        assert_eq!(pointer, "/mcp/servers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd", &[("K".into(), "v".into())]);
        assert_eq!(j["type"], "local");
        assert_eq!(j["command"], serde_json::json!(["/usr/bin/nexmesh-mcpd"]));
        assert_eq!(j["enabled"], true);
        assert_eq!(j["environment"]["K"], "v", "opencode calls it 'environment', not 'env'");
    }

    /// AN ISOLATED SEAT MUST STILL BE LOGGED IN.
    ///
    /// opencode's per-seat store exists so that "continue the last session"
    /// means this seat's last one. It relocates XDG_DATA_HOME, and the login
    /// `opencode auth` writes lives in that directory -- so with nothing
    /// shared, an isolated seat came up logged out on every host that signs
    /// in that way rather than by an API key in the environment.
    ///
    /// Measured against opencode 1.19: the `credential` table inside
    /// opencode.db, which is what the empty list was protecting, holds no
    /// rows; the login is `auth.json` beside the database, an ordinary file
    /// that shares like codex's does.
    #[test]
    fn an_isolated_opencode_seat_keeps_the_login_it_did_not_write() {
        let d = desc("opencode");
        let st = d.session_store.as_ref().expect("opencode seats need a store of their own");
        assert_eq!(st.env, "XDG_DATA_HOME");
        assert_eq!(st.root, "~/.local/share/opencode");
        assert!(st.share.iter().any(|e| e == "auth.json"),
            "the login must be shared into the seat's store, or an isolated \
             seat is a logged-out one: {:?}", st.share);
        // And NOT the conversations: sharing the database back would undo
        // the whole point of giving the seat a store.
        assert!(!st.share.iter().any(|e| e.contains("opencode.db")),
            "the conversation store must stay per seat: {:?}", st.share);
    }

    #[test]
    fn pointer_merge_preserves_unrelated_settings() {
        // We write into config files we do not own. Clobbering a user's other
        // MCP servers or settings would be unforgivable.
        let mut root = serde_json::json!({
            "theme": "dark",
            "mcpServers": { "someone-elses": { "command": "keepme" } }
        });
        set_pointer(&mut root, "/mcpServers/nexmesh", serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["someone-elses"]["command"], "keepme");
        assert_eq!(root["mcpServers"]["nexmesh"]["command"], "x");
    }

    #[test]
    fn pointer_merge_creates_missing_intermediate_objects() {
        let mut root = serde_json::json!({});
        set_pointer(&mut root, "/mcp/servers/nexmesh", serde_json::json!(1)).unwrap();
        assert_eq!(root["mcp"]["servers"]["nexmesh"], 1);
    }

    #[test]
    fn pointer_merge_replaces_a_non_object_in_the_path() {
        // A config where `mcp` is a string must not make us panic or corrupt.
        let mut root = serde_json::json!({ "mcp": "nonsense" });
        set_pointer(&mut root, "/mcp/servers/nexmesh", serde_json::json!(1)).unwrap();
        assert_eq!(root["mcp"]["servers"]["nexmesh"], 1);
    }

    #[test]
    fn an_empty_pointer_is_an_error_not_a_wipe() {
        let mut root = serde_json::json!({ "keep": 1 });
        assert!(set_pointer(&mut root, "", serde_json::json!(2)).is_err());
        assert_eq!(root["keep"], 1);
    }

    #[test]
    fn escaped_pointer_segments_are_decoded() {
        let mut root = serde_json::json!({});
        set_pointer(&mut root, "/a~1b/c", serde_json::json!(1)).unwrap();
        assert_eq!(root["a/b"]["c"], 1);
    }

    #[test]
    fn every_shipped_descriptor_declares_a_registration() {
        // A CLI with no way to load the shim could never join a team.
        for name in ["claude", "opencode", "qwen", "gemini", "codex", "cursor", "freebuff"] {
            assert!(
                !matches!(desc(name).mcp(), McpRegistration::None),
                "{name} has no MCP registration"
            );
        }
    }

    #[test]
    fn freebuff_writes_a_project_file_in_the_directory_it_scans() {
        // It reads `mcp.json` out of the `.agents` directories around the
        // working directory. A relative path makes that the seat's
        // workspace, so the user's own ~/.agents/mcp.json is untouched.
        let d = desc("freebuff");
        let McpRegistration::JsonFile { path, pointer, entry, env_key } = d.mcp() else {
            panic!("freebuff should merge into a project json file");
        };
        assert_eq!(path, ".agents/mcp.json");
        assert_eq!(pointer, "/mcpServers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd",
                             &[("NEXMESH_AGENT_ID".into(), "ag-1".into())]);
        assert_eq!(j["command"], "/usr/bin/nexmesh-mcpd", "a string, not an array");
        assert_eq!(j["env"]["NEXMESH_AGENT_ID"], "ag-1");
    }

    #[test]
    fn a_freebuff_seat_keeps_its_conversations_to_itself_and_its_login() {
        // Its chat directories are named by start time inside one store, so
        // without a store per seat `--continue` is a coin toss between two
        // seats in the same directory. And the login lives in that same
        // store, so an isolated seat that does not share it is logged out.
        let d = desc("freebuff");
        let s = d.session_store.as_ref().expect("freebuff needs a store per seat");
        assert_eq!(s.env, "FREEBUFF_CONFIG_DIR");
        assert!(s.store_rel.is_empty(), "the variable IS the store, as with CODEX_HOME");
        for must in ["credentials.json", "device-key.json"] {
            assert!(s.share.iter().any(|e| e == must),
                    "a seat that does not share {must} is a seat that is logged out");
        }
        assert!(!s.share.iter().any(|e| e == "projects"),
                "the conversations are the one thing that must NOT be shared");
        let m = d.modes.get("interactive").expect("interactive mode");
        assert_eq!(m.resume_latest_args, ["--continue"]);
        assert!(m.resume_args.is_empty(),
                "freebuff publishes no conversation id, so there is no handle to resume by");
    }

    #[test]
    fn cursor_declares_the_stdio_type_its_schema_requires() {
        // Alone among the CLIs here, Cursor's documented schema requires a
        // `type` on every server entry. Leaving it out means the shim
        // silently never loads.
        let d = desc("cursor");
        let McpRegistration::JsonFile { path, pointer, entry, env_key } = d.mcp() else {
            panic!("cursor should merge into a project json file");
        };
        assert_eq!(path, ".cursor/mcp.json");
        assert_eq!(pointer, "/mcpServers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd", &[]);
        assert_eq!(j["type"], "stdio");
        assert_eq!(j["command"], "/usr/bin/nexmesh-mcpd");
        let Some(UserInstall::JsonFile { entry, .. }) = d.user_install() else {
            panic!("cursor has no `mcp add`, so nexmesh writes the file itself");
        };
        assert_eq!(render_entry(entry, None, "x", &[])["type"], "stdio",
                   "the user's own config needs it too");
    }

    #[test]
    fn cursor_takes_its_prompt_last_and_never_resumes_a_stranger() {
        let d = desc("cursor");
        let h = d.modes.get("headless").expect("print mode");
        assert_eq!(h.args.last().map(String::as_str), Some("{prompt}"),
                   "the prompt is positional, so it has to come after the flags");
        assert!(h.args.iter().any(|a| a == "--force"),
                "without it print mode rejects anything that would need approval");
        let i = d.modes.get("interactive").expect("interactive mode");
        assert!(i.resume_latest_args.is_empty(),
                "`--continue` is the USER's previous chat, not this seat's: there is no \
                 way to give a cursor seat a store of its own");
    }

    /// Each CLI compacts with its own command, and the commands next door
    /// (`/clear`, `/new`) destroy the conversation instead of summarising
    /// it. So this is a table of what was actually read in each CLI's own
    /// command list -- and of which CLIs declare nothing, which is a
    /// decision rather than an omission.
    #[test]
    fn compaction_is_each_clis_own_command_or_nothing() {
        let expect: &[(&str, Option<&str>)] = &[
            ("claude", Some("/compact")),
            ("freebuff", Some("/compact")),
            ("gemini", Some("/compress")),
            ("cursor", Some("/summarize")),
            // Not verified against the real binary, so nothing is typed at
            // them; `/compact <agent>` says so instead of guessing.
            ("codex", None),
            ("opencode", None),
        ];
        for (name, want) in expect {
            let d = desc(name);
            let got = d
                .modes
                .get("interactive")
                .and_then(|m| m.compact_command.as_deref());
            assert_eq!(&got, want, "{name}: compaction command");
        }
    }

    /// Interrupting is a keystroke, and the keys beside the right one end
    /// the process or clear the conversation -- so a CLI whose stop key was
    /// not established declares none and is paused without one.
    #[test]
    fn only_a_verified_stop_key_is_declared() {
        let expect: &[(&str, Option<&str>)] = &[
            ("claude", Some("\u{1b}")),
            // Its own help screen: Esc = "Close help / stop".
            ("freebuff", Some("\u{1b}")),
            // Ctrl+C both cancels a request and quits when the composer is
            // empty, and nothing outside can tell which it will be.
            ("gemini", None),
            // Undocumented.
            ("cursor", None),
        ];
        for (name, want) in expect {
            let d = desc(name);
            let got = d
                .modes
                .get("interactive")
                .and_then(|m| m.interrupt_keys.as_deref());
            assert_eq!(&got, want, "{name}: interrupt keys");
        }
    }
}
