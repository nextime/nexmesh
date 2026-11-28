//! Starting an agent CLI on this machine.
//!
//! The node is the only component that spawns anything, because the thing that
//! drives an agent has to share a kernel with it.

use crate::{
    ctx::{AgentRec, NodeCtx},
    proc,
};
use anyhow::{Context, Result};
use nexmesh_proto::{
    self as proto,
    cli_descriptor::{render_entry, set_pointer, CliDescriptor, DriverSpec, McpRegistration},
    ProcIdentity, Provenance, RunMode, SpawnSpec,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// Random bytes without pulling in another dependency.
fn getrandom_bytes(out: &mut [u8]) {
    use rand_core_shim::fill;
    fill(out);
}

mod rand_core_shim {
    /// /dev/urandom is the right source here and always present on the
    /// platforms a node runs on.
    pub fn fill(out: &mut [u8]) {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(out).is_ok() {
                return;
            }
        }
        // Never silently hand back zeros: a predictable password is worse
        // than a loud failure.
        panic!("cannot read /dev/urandom to generate a server password");
    }
}

fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// The environment an mcpd inherits so it can identify itself to the node.
/// Record that a directory is trusted, where this CLI looks for that.
///
/// claude asks "do you trust the files in this folder?" as a modal question
/// and will not start until it is answered. No automation can answer it, so
/// the seat is stuck and looks, from the outside, like a CLI that refuses to
/// run -- which is exactly how it was reported.
///
/// The operator chose the directory when they made the seat; running an
/// agent there IS the decision. This records it where the CLI looks for it.
///
/// Returns `Some(path)` when it had to change something, so the caller can
/// say so: marking a directory trusted on somebody's behalf is not a thing
/// to do silently.
fn ensure_trusted(
    trust: &nexmesh_proto::cli_descriptor::TrustPath,
    cwd: &str,
) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let file = if let Some(rest) = trust.file.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        trust.file.clone()
    };
    // JSON pointer escaping: `~` then `/`, in that order, per RFC 6901.
    let escaped = cwd.replace('~', "~0").replace('/', "~1");
    let pointer = trust.pointer.replace("{cwd}", &escaped);

    let text = std::fs::read_to_string(&file).ok()?;
    let mut root: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Already true? Then nothing to do and nothing to announce.
    if root.pointer(&pointer).and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    // Walk the pointer, creating objects as needed.
    let mut cur = &mut root;
    let parts: Vec<String> = pointer
        .trim_start_matches('/')
        .split('/')
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    let (last, lead) = parts.split_last()?;
    for p in lead {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()?
            .entry(p.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    cur.as_object_mut()?.insert(last.clone(), serde_json::Value::Bool(true));

    // Written through a temporary file and renamed: this is somebody's live
    // CLI config, and a half-written one would lock them out of their own
    // tool.
    let tmp = format!("{file}.nexmesh-tmp");
    let pretty = serde_json::to_string_pretty(&root).ok()?;
    std::fs::write(&tmp, pretty).ok()?;
    std::fs::rename(&tmp, &file).ok()?;
    Some(file)
}

/// What this spawn should say about conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Continue this conversation, by id.
    Resume(String),
    /// Start a NEW conversation and NAME it, so there is something to resume
    /// next time.
    NameFresh,
    /// Continue whatever this seat was last on, out of its own store.
    ContinueLatest,
    /// Say nothing: this CLI manages its own conversations.
    Nothing,
}

/// Decide it in one place, so the decision can be tested rather than believed.
///
/// The rule that matters most: a resume that cannot be honoured becomes a
/// NEW NAMED conversation, never nothing. A seat whose conversation has gone
/// -- the start that named it died before the CLI wrote it, the file belongs
/// to another directory, the seat has moved machines -- must still come up,
/// and come up with a conversation of its own so the next restart has
/// something to continue. Leaving it with no session argument is how a seat
/// ends up unopenable with nothing the operator can do about it.
pub(crate) fn resume_plan(
    mode: &nexmesh_proto::cli_descriptor::ModeSpec,
    // Every conversation this seat might be resuming, best first. The node
    // is the only component that can see the files, so choosing between
    // them is its job, not the hub's.
    candidates: &[String],
    // Asks whether a conversation is really there. `Some(false)`: checked,
    // and it is not. `None`: this CLI does not say where conversations
    // live, so nothing is checkable and nothing is refused.
    exists: impl Fn(&str) -> Option<bool>,
    // Does this seat's own store already hold a conversation?
    store_has_prior: bool,
) -> ResumePlan {
    // A handle is only worth anything if the CLI takes one at all. Holding
    // one for a CLI with no `resume_args` is not a reason to skip naming a
    // fresh conversation -- which is what the old chain did, leaving such a
    // seat with no session argument whatsoever.
    if !mode.resume_args.is_empty() {
        // THE FIRST ONE THAT IS REALLY THERE.
        //
        // Taking only the first candidate and declining it if absent was
        // the bug: a ghost in front of a real conversation meant the real
        // one was never tried, and the seat started blank on top of work
        // that was still on disk.
        for h in candidates {
            match exists(h) {
                // Checked and absent: a ghost. Try the next one.
                Some(false) => continue,
                // Either checked and there, or not checkable at all.
                _ => return ResumePlan::Resume(h.clone()),
            }
        }
    }
    if !mode.session_id_args.is_empty() {
        return ResumePlan::NameFresh;
    }
    if !mode.resume_latest_args.is_empty() && store_has_prior {
        return ResumePlan::ContinueLatest;
    }
    ResumePlan::Nothing
}

/// Does the conversation this handle names actually exist?
///
/// `None` when the CLI does not say where conversations live -- nothing is
/// checkable, so nothing is refused.
///
/// This exists because a named conversation is recorded BEFORE the CLI has
/// created it. A start that fails leaves a handle pointing at nothing, and
/// `claude --resume <nothing>` answers "No conversation found with session
/// ID" and exits immediately -- so the seat cannot start at all, with an
/// error nobody would trace back to nexmesh having named it. Seen on the
/// live fleet: the handle 7a8a2910-… existed in the hub and in no project
/// directory anywhere.
fn conversation_exists(
    mode: &nexmesh_proto::cli_descriptor::ModeSpec,
    handle: &str,
    cwd: &str,
) -> Option<bool> {
    Some(conversation_exists_at(mode.session_path.as_ref()?, handle, cwd))
}

/// The same question, from the path template alone.
fn conversation_exists_at(tpl: &str, handle: &str, cwd: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    // How claude names the project directory for a working directory: every
    // slash becomes a dash, leading one included.
    let slug = cwd.replace('/', "-");
    let path = tpl
        .replace("{id}", handle)
        .replace("{cwd_slug}", &slug);
    let path = if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path
    };
    std::path::Path::new(&path).exists()
}

/// Fill a system-prompt template: `{prompt}` as it is, `{prompt_toml}` and
/// `{prompt_json}` as quoted strings in those languages, `{prompt_file}` the
/// file the text was written to and `{prompt_dir}` its directory
/// (`{prompt_plugin_dir}` the same directory, laid out as a Cursor plugin).
pub(crate) fn fill_prompt(template: &str, text: &str, file: &str, dir: &str) -> String {
    template
        .replace("{prompt_toml}", &toml_string(text))
        .replace("{prompt_json}", &serde_json::Value::String(text.to_string()).to_string())
        .replace("{prompt_file}", file)
        .replace("{prompt_plugin_dir}", dir)
        .replace("{prompt_dir}", dir)
        .replace("{prompt}", text)
}

/// A TOML basic string. codex parses the value of `-c key=value` as TOML,
/// so a prompt with a quote or a newline in it must arrive escaped.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write a seat's system prompt where a CLI that takes instructions by PATH
/// can read it, in the seat's own directory, replaced on every spawn so it is
/// never an older prompt than the one this start was given.
///
/// `<seat>/nexmesh-prompt/AGENTS.md`: named so because Copilot's
/// `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` takes DIRECTORIES and reads the
/// `AGENTS.md` in each; opencode's `instructions` take the file itself. With
/// `plugin`, the same directory is also laid out as a Cursor plugin
/// (`.cursor-plugin/plugin.json` and an always-applied rule), for
/// `cursor-agent --plugin-dir`. Returns (file, directory).
fn write_prompt_file(cli_id: &str, slot_id: &str, text: &str, plugin: bool) -> Option<(String, String)> {
    let dir = format!("{}/nexmesh-prompt", seat_store_base(cli_id, slot_id)?);
    std::fs::create_dir_all(&dir).ok()?;
    let put = |rel: &str, body: &str| -> Option<()> {
        let path = format!("{dir}/{rel}");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, body).ok()?;
        std::fs::rename(&tmp, &path).ok()
    };
    put("AGENTS.md", text)?;
    if plugin {
        put(".cursor-plugin/plugin.json",
            "{\"name\": \"nexmesh-seat\", \"description\": \"This seat's nexmesh instructions\"}\n")?;
        put("rules/nexmesh.mdc",
            &format!("---\ndescription: This seat's nexmesh instructions\nalwaysApply: true\n---\n\n{text}\n"))?;
    }
    Some((format!("{dir}/AGENTS.md"), dir))
}

/// Where a seat's own data directory for one CLI is: the base, before the
/// descriptor's `store_rel`. The same rule `prepare_session_store` makes it
/// by, for whatever needs to find it again (the context watcher).
pub(crate) fn seat_store_base(cli_id: &str, slot_id: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let root_dir = std::env::var("NEXMESH_CLI_STATE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{home}/.nexmesh/cli-state"));
    Some(format!("{root_dir}/{cli_id}/{slot_id}"))
}

/// Give one seat its own copy of a CLI's data directory.
///
/// Returns the environment variable to set, its value, and whether that
/// store ALREADY holds a conversation -- which is what makes "continue the
/// last one" mean this seat's last one rather than whichever seat happened
/// to run most recently.
///
/// Shared entries are symlinked out of the real store, so credentials,
/// configuration and installed packages are not duplicated and do not drift.
/// Everything else is per seat: that is the whole point.
///
/// Best-effort. A store that cannot be prepared must not stop a seat
/// starting -- it only costs that seat its resume -- so this reports and
/// returns None rather than failing the spawn.
fn prepare_session_store(
    store: &nexmesh_proto::cli_descriptor::SessionStore,
    slot_id: &str,
    cli_id: &str,
) -> Option<(String, String, bool)> {
    let home = std::env::var("HOME").ok()?;
    let expand = |p: &str| -> String {
        if let Some(rest) = p.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            p.to_string()
        }
    };
    // Per seat AND per CLI: one seat respawned under a different CLI is a
    // different conversation, and mixing the two stores would offer it one
    // that the new CLI cannot read.
    //
    // `NEXMESH_CLI_STATE` moves the whole tree. These stores hold real
    // conversation history and grow, so the machine where that cannot live
    // under $HOME -- a small root filesystem, a tmpfs home -- needs to be
    // able to say where it goes. It is also what lets a test use one.
    let root_dir = std::env::var("NEXMESH_CLI_STATE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("{home}/.nexmesh/cli-state"));
    let base = format!("{root_dir}/{cli_id}/{slot_id}");
    let dir = if store.store_rel.is_empty() {
        base.clone()
    } else {
        format!("{base}/{}", store.store_rel)
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%slot_id, %cli_id, dir = %dir, error = %e,
                       "could not make this seat its own data directory; it will not resume");
        return None;
    }
    let root = expand(&store.root);
    let mut shared = 0usize;
    for entry in &store.share {
        let from = format!("{root}/{entry}");
        let to = format!("{dir}/{entry}");
        if !std::path::Path::new(&from).exists() {
            continue; // one list serves several versions of a CLI
        }
        if std::path::Path::new(&to).exists() {
            shared += 1;
            continue;
        }
        match std::os::unix::fs::symlink(&from, &to) {
            Ok(()) => shared += 1,
            Err(e) => tracing::warn!(%slot_id, entry = %entry, error = %e,
                                     "could not share this entry into the seat's store"),
        }
    }
    // Is there a conversation in here already? Anything that is not one of
    // the entries we linked ourselves. Deliberately generic: the node does
    // not need to understand a CLI's storage format to tell whether it has
    // written any.
    let prior = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    !store.share.iter().any(|s| *s == name)
                })
        })
        .unwrap_or(false);
    tracing::info!(%slot_id, %cli_id, dir = %dir, shared, prior,
                   "this seat has a data directory of its own");
    Some((store.env.clone(), base, prior))
}

fn shim_env(spec: &SpawnSpec, sock_path: &str) -> Vec<(String, String)> {
    vec![
        ("NEXMESH_AGENT_ID".into(), spec.agent_id.clone()),
        ("NEXMESH_SLOT".into(), spec.slot_id.clone()),
        ("NEXMESH_SESSION".into(), spec.session_id.clone()),
        ("NEXMESH_SOCK".into(), sock_path.to_string()),
        ("NEXMESH_CLI".into(), spec.cli_id.clone()),
    ]
}

/// A descriptor's own environment, with `~/` expanded.
///
/// This is what lets two named configurations run the same binary as the same
/// user against different logins: each points at its own config directory.
/// The NEXMESH_* variables are applied after these and win, because they are
/// how the shim finds its node -- a descriptor that could overwrite them
/// would produce an agent that cannot join the team it was spawned for.
fn descriptor_env(d: &nexmesh_proto::cli_descriptor::CliDescriptor) -> Vec<(String, String)> {
    d.cli
        .env
        .iter()
        .map(|(k, v)| {
            let v = if let Some(rest) = v.strip_prefix("~/") {
                match std::env::var("HOME") {
                    Ok(home) => format!("{}/{}", home.trim_end_matches('/'), rest),
                    Err(_) => v.clone(),
                }
            } else {
                v.clone()
            };
            (k.clone(), v)
        })
        .collect()
}

/// The fleet-wide orchestrator MCP server, for a seat whose role is
/// `orchestrator`.
///
/// Returns None when this machine has no `nexmesh-orch` beside the mcpd, or no
/// paired identity for it to authenticate with. Both are the ordinary case on a
/// node that runs only ordinary seats, so neither is an error.
///
/// No token is written into the config: `nexmesh-orch` takes the hub address
/// and its client certificate from the identity directory, so the secret stays
/// in one file with one owner instead of being copied into a file per seat.
fn fleet_entry(mcpd_path: &str, env_key: Option<&str>) -> Option<serde_json::Value> {
    let orch = PathBuf::from(mcpd_path).parent()?.join("nexmesh-orch");
    if !orch.is_file() {
        tracing::warn!(expected = %orch.display(),
            "orchestrator seat, but no nexmesh-orch on this node: no fleet tools");
        return None;
    }
    let identity = PathBuf::from(expand(
        &std::env::var("NEXMESH_IDENTITY").unwrap_or_else(|_| "~/.nexmesh".into()),
    ));
    if !identity.join("identity.json").is_file() {
        tracing::warn!(expected = %identity.display(),
            "orchestrator seat, but no paired identity: no fleet tools");
        return None;
    }
    let mut entry = serde_json::json!({
        "command": orch.to_string_lossy(),
        "args": [],
    });
    entry[env_key.unwrap_or("env")] =
        serde_json::json!({ "NEXMESH_IDENTITY": identity.to_string_lossy() });
    Some(entry)
}

/// Merge our MCP server entry into whichever config this CLI reads, and return
/// a path to pass on the command line if it takes one.
///
/// Config files we do not own are read, merged and rewritten -- never
/// truncated -- so the user's own servers and settings survive.
pub fn write_mcp_config(
    d: &CliDescriptor,
    spec: &SpawnSpec,
    mcpd_path: &str,
    sock_path: &str,
    state_dir: &PathBuf,
) -> Result<Option<String>> {
    let env = shim_env(spec, sock_path);
    match d.mcp() {
        McpRegistration::None => Ok(None),

        McpRegistration::InlineFlag { pointer, entry, env_key, .. } => {
            let mut root = serde_json::json!({});
            let value = render_entry(entry, env_key.as_deref(), mcpd_path, &env);
            set_pointer(&mut root, pointer, value).map_err(|e| anyhow::anyhow!(e))?;

            // An orchestrator seat is given fleet reach as well as its own
            // session. Without it the one agent whose job is to talk to other
            // sessions is the only one that cannot, and has to borrow the
            // operator's identity over ssh to do it.
            //
            // Only in this arm. A private per-seat file is ours to extend; the
            // JsonFile and TomlFile arms merge into a config shared with every
            // CLI the user starts by hand on this machine, and operator reach
            // written there would be handed to all of them.
            if spec.role.as_deref() == Some("orchestrator") {
                if let Some(fleet) = fleet_entry(mcpd_path, env_key.as_deref()) {
                    let parent = pointer.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    set_pointer(&mut root, &format!("{parent}/nexmesh-fleet"), fleet)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
            }
            std::fs::create_dir_all(state_dir)?;
            let path = state_dir.join(format!("mcp-{}.json", spec.agent_id));
            std::fs::write(&path, serde_json::to_string_pretty(&root)?)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(Some(path.to_string_lossy().to_string()))
        }

        McpRegistration::JsonFile { path, pointer, entry, env_key } => {
            // Relative => a project file in the workspace; ~/ => the user's own
            // global config, which we must merge into rather than overwrite.
            let full = if path.starts_with('/') || path.starts_with('~') {
                PathBuf::from(expand(path))
            } else {
                PathBuf::from(&spec.cwd).join(path)
            };
            let mut root: serde_json::Value = match std::fs::read_to_string(&full) {
                Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
                    .with_context(|| format!("{} is not valid JSON; refusing to overwrite it", full.display()))?,
                _ => serde_json::json!({}),
            };
            let value = render_entry(entry, env_key.as_deref(), mcpd_path, &env);
            set_pointer(&mut root, pointer, value).map_err(|e| anyhow::anyhow!(e))?;
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(&full, serde_json::to_string_pretty(&root)?)
                .with_context(|| format!("writing {}", full.display()))?;
            tracing::info!(path = %full.display(), "merged nexmesh into MCP config");
            Ok(None)
        }

        McpRegistration::TomlFile { path, key, entry, env_key } => {
            let full = PathBuf::from(expand(path));
            let mut root: toml::Value = match std::fs::read_to_string(&full) {
                Ok(text) if !text.trim().is_empty() => text
                    .parse()
                    .with_context(|| format!("{} is not valid TOML; refusing to overwrite it", full.display()))?,
                _ => toml::Value::Table(Default::default()),
            };
            // Convert our rendered JSON entry back into TOML.
            let json = render_entry(entry, env_key.as_deref(), mcpd_path, &env);
            let as_toml: toml::Value = toml::Value::try_from(json)?;
            let parts: Vec<&str> = key.split('.').collect();
            let mut cur = &mut root;
            for p in &parts[..parts.len() - 1] {
                let table = cur
                    .as_table_mut()
                    .ok_or_else(|| anyhow::anyhow!("{} traverses a non-table", key))?;
                cur = table
                    .entry(p.to_string())
                    .or_insert_with(|| toml::Value::Table(Default::default()));
            }
            cur.as_table_mut()
                .ok_or_else(|| anyhow::anyhow!("{} traverses a non-table", key))?
                .insert(parts[parts.len() - 1].to_string(), as_toml);
            if let Some(dir) = full.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(&full, toml::to_string_pretty(&root)?)
                .with_context(|| format!("writing {}", full.display()))?;
            tracing::info!(path = %full.display(), "merged nexmesh into MCP config");
            Ok(None)
        }
    }
}

/// Where a slot with no workspace should work.
///
/// Empty means the hub had nothing to resolve, so the choice belongs here:
/// `HOME` on this machine is the home of the user this daemon runs as. The
/// hub reading its own `HOME` names a directory on the hub's machine, owned
/// by the hub's user, which is how every workspace-less spawn came to fail
/// with a bare "Permission denied" on the node.
fn resolve_cwd(cwd: &str) -> String {
    if !cwd.trim().is_empty() {
        return cwd.to_string();
    }
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

pub async fn spawn_agent(ctx: Arc<NodeCtx>, spec: SpawnSpec) -> Result<()> {
    let d = ctx
        .descriptor(&spec.cli_id)
        .ok_or_else(|| anyhow::anyhow!("no descriptor for CLI '{}'", spec.cli_id))?
        .clone();
    let mut bin = crate::cli_detect::resolve(&d.cli.command, &d.cli.extra_paths)
        .ok_or_else(|| anyhow::anyhow!("'{}' is not installed on {}", d.cli.command, ctx.node_name))?;

    // A plugin driver's helper is resolved here, before the CLI is started:
    // finding out afterwards would leave an agent running with no driver and
    // no obvious reason why its messages are slow.
    let mut d = d;
    if let DriverSpec::Plugin { command, .. } = &d.driver {
        let found = crate::cli_detect::resolve(command, &d.cli.extra_paths).ok_or_else(|| {
            anyhow::anyhow!(
                "the driver plugin '{command}' for '{}' is not on PATH or in any of its \
                 extra_paths on {}; install it or correct the descriptor",
                spec.cli_id,
                ctx.node_name
            )
        })?;
        if let DriverSpec::Plugin { command, .. } = &mut d.driver {
            *command = found;
        }
    }
    let d = d;

    let mode_name = match spec.mode {
        RunMode::Headless => "headless",
        RunMode::Interactive => "interactive",
    };
    let mode = d
        .modes
        .get(mode_name)
        .ok_or_else(|| anyhow::anyhow!("'{}' has no {mode_name} mode", spec.cli_id))?
        .clone();

    let mut spec = spec;
    spec.cwd = resolve_cwd(&spec.cwd);

    let state_dir = PathBuf::from(&spec.cwd).join(".nexmesh");
    // Named, because "Permission denied (os error 13)" on its own sends you
    // looking at the CLI rather than at the directory.
    std::fs::create_dir_all(&state_dir).with_context(|| {
        format!(
            "creating {} as {}: the agent's working directory must be writable by \
             the user this node runs as",
            state_dir.display(),
            std::env::var("USER").unwrap_or_else(|_| "this node's user".into())
        )
    })?;
    let inline = write_mcp_config(&d, &spec, &ctx.mcpd_path, &ctx.sock_path, &state_dir)?;

    let mut vars: BTreeMap<&str, String> = BTreeMap::new();
    vars.insert("prompt", spec.prompt.clone());
    vars.insert("cwd", spec.cwd.clone());
    vars.insert("session", spec.session_id.clone());
    if let Some(p) = &inline {
        vars.insert("mcp_config", p.clone());
    }
    if let Some(r) = &spec.resume_handle {
        vars.insert("resume", r.clone());
    }
    if let Some(t) = &spec.tmux_target {
        vars.insert("tmux_target", t.clone());
    }
    vars.insert("slot", spec.slot_id.clone());
    let mut args = d.render_args(mode_name, &vars);
    // An adopted terminal talks to whichever tmux server this node is pointed
    // at: normally the user's own, and under test one of the suite's own, so a
    // suite can create and kill sessions without touching the ones somebody is
    // working in. `-L` must come before the subcommand, which is why it is
    // spliced in here rather than written into the descriptor.
    if spec.tmux_target.is_some() {
        if let Some(sock) = crate::tmux::socket() {
            args.splice(0..0, ["-L".to_string(), sock]);
        }
        // And clear our own leftover view, or `new-session -s` collides with
        // it and the adoption cannot come back after a node restart.
        crate::tmux::clear_stale_view(&tmux_view(&spec.slot_id));
    }
    // A data directory of this seat's own, for a CLI that keeps its
    // conversations in one shared store and tells nobody which is which.
    // Prepared BEFORE the resume decision, because whether that store
    // already holds a conversation is what decides it.
    let store = d
        .session_store
        .as_ref()
        .and_then(|s| prepare_session_store(s, &spec.slot_id, &spec.cli_id));

    // Resuming a conversation we already know the id of...
    let mut fresh_session: Option<String> = None;
    // A directory this CLI will not start in unless it has been told it is
    // trusted. Done before the spawn, because afterwards the seat is
    // already sitting on a modal question nothing can answer.
    if let Some(t) = &d.trust {
        if let Some(file) = ensure_trusted(t, &spec.cwd) {
            tracing::info!(slot = %spec.slot_id, cwd = %spec.cwd, %file,
                           "recorded this directory as trusted for the CLI");
            let _ = ctx
                .send_hub(&proto::NodeToHub::Noted {
                    agent_id: spec.agent_id.clone(),
                    text: format!(
                        "[nexmesh] marked {} as trusted for this CLI in {file}, because this \
                         seat is configured to run there and the CLI will not start \
                         otherwise \u{2014} it asks a modal question nothing can answer. Undo \
                         it there if that is not what you want.",
                        spec.cwd
                    ),
                })
                .await;
        }
    }

    // What this spawn says about conversations, decided in one place so it
    // can be tested rather than believed. See `resume_plan`.
    // Every conversation the hub thinks this seat might be on, best first.
    // A hub from before `resume_candidates` sends only `resume_handle`, so
    // that stands in for the list and the behaviour is unchanged.
    let candidates: Vec<String> = if spec.resume_candidates.is_empty() {
        spec.resume_handle.iter().cloned().collect()
    } else {
        spec.resume_candidates.clone()
    };
    let plan = resume_plan(
        &mode,
        &candidates,
        |h| conversation_exists(&mode, h, &spec.cwd),
        store.as_ref().map(|(_, _, prior)| *prior).unwrap_or(false),
    );
    // Say what was skipped and why. Without this the operator sees a seat
    // quietly start over -- or, before the check existed, a bare "no
    // conversation found" from the CLI and no reason to connect it to
    // nexmesh having named one.
    let ghosts: Vec<&String> = candidates
        .iter()
        .take_while(|h| !matches!(&plan, ResumePlan::Resume(r) if r == *h))
        .filter(|h| conversation_exists(&mode, h, &spec.cwd) == Some(false))
        .collect();
    if !ghosts.is_empty() {
        tracing::warn!(
            slot = %spec.slot_id, cwd = %spec.cwd, skipped = ghosts.len(),
            "conversations this seat would have resumed are not here"
        );
        let resumed = match &plan {
            ResumePlan::Resume(r) => format!("Resuming {r} instead, which is here."),
            _ => "Starting a NEW one, so the seat comes up and has something to continue \
                  next time."
                .to_string(),
        };
        let _ = ctx
            .send_hub(&proto::NodeToHub::Noted {
                agent_id: spec.agent_id.clone(),
                text: format!(
                    "[nexmesh] {} of this seat's recorded conversations {} not in {} on this \
                     machine ({}). {resumed}",
                    ghosts.len(),
                    if ghosts.len() == 1 { "is" } else { "are" },
                    spec.cwd,
                    ghosts.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", "),
                ),
            })
            .await;
    }
    match &plan {
        ResumePlan::Resume(_) => {
            for a in &mode.resume_args {
                let mut s = a.clone();
                for (k, v) in &vars {
                    s = s.replace(&format!("{{{k}}}"), v);
                }
                args.push(s);
            }
        }
        ResumePlan::NameFresh => {
            // A NEW conversation, NAMED -- so there is an id to resume next
            // time, and so a seat whose old conversation has gone still
            // comes up with one of its own rather than with nothing.
            let fresh = uuid::Uuid::new_v4().to_string();
            for a in &mode.session_id_args {
                args.push(a.replace("{session_uuid}", &fresh));
            }
            fresh_session = Some(fresh);
        }
        ResumePlan::ContinueLatest => {
            // For a CLI that will not name a conversation but will continue
            // its last one. Only safe because this seat has a store of its
            // own, so "the last one" is unambiguously the one THIS seat was
            // on; without that isolation it would be whichever conversation
            // another seat happened to finish most recently.
            for a in &mode.resume_latest_args {
                args.push(a.clone());
            }
            tracing::info!(slot = %spec.slot_id, cli = %spec.cli_id,
                           "continuing this seat's last conversation from its own store");
        }
        ResumePlan::Nothing => {}
    }

    // Set below by a CLI that takes its system prompt through the
    // environment; applied after the descriptor's own, so it wins.
    let mut prompt_env: Vec<(String, String)> = Vec::new();

    // THE SEAT'S OWN SYSTEM PROMPT.
    //
    // `append` adds to what the CLI already tells the model and is almost
    // always what is wanted: the CLI's own prompt carries how to use its
    // tools, and replacing it to add a sentence about what a seat is for
    // throws that away. `replace` is for when that prompt is the problem.
    //
    // A CLI that declares neither is told so out loud rather than starting
    // without it: a seat whose operator believes it has been given a prompt,
    // and has not, is worse than one that was refused.
    // The fleet's standing habits ride on the seat's own prompt, after it.
    // Alone, they are given only where this CLI can take them: they are the
    // fleet's, so a CLI that cannot is not worth a note to the seat.
    let own = spec.system_prompt.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let habits = spec.habits.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let replace = spec.system_prompt_mode.as_deref() == Some("replace");
    let can_take = if replace {
        !mode.system_prompt_args.is_empty()
    } else {
        !mode.append_system_prompt_args.is_empty() || !mode.append_system_prompt_env.is_empty()
    };
    let combined = match (own, habits) {
        (Some(o), Some(h)) if can_take => Some(format!("{o}\n\n{h}")),
        (Some(o), _) => Some(o.to_string()),
        (None, Some(h)) if can_take => Some(h.to_string()),
        _ => None,
    };
    if let Some(text) = combined.as_deref() {
        let chosen = if replace {
            (&mode.system_prompt_args, "replace")
        } else {
            (&mode.append_system_prompt_args, "append")
        };
        // Falling back the other way would silently do something other than
        // what was asked: appending when told to replace leaves the CLI's
        // own prompt in place, and replacing when told to append throws it
        // away. Both are worse than saying so.
        let by_env = if replace { None } else { Some(&mode.append_system_prompt_env) }
            .filter(|e| !e.is_empty());
        if chosen.0.is_empty() && by_env.is_none() {
            tracing::warn!(
                slot = %spec.slot_id, cli = %spec.cli_id, mode = chosen.1,
                "this seat has a system prompt but this CLI declares no way to {} one; \
                 starting without it", chosen.1
            );
            // Said once the agent EXISTS: the hub has no row for it until the
            // node reports the start, and a note sent before that is dropped.
            ctx.note_when_started(
                &spec.agent_id,
                format!(
                    "[nexmesh] this seat carries a system prompt to {}, but {} takes no \
                     argument for that, so it is running WITHOUT it. Set it on a seat \
                     whose CLI supports one, or put it in the seat's instructions \
                     instead, which go through the conversation.",
                    chosen.1, spec.cli_id
                ),
            );
        } else {
            let uses = |needle: &str| {
                chosen.0.iter().any(|a| a.contains(needle))
                    || by_env.is_some_and(|e| e.values().any(|v| v.contains(needle)))
            };
            let plugin = uses("{prompt_plugin_dir}");
            let wants_file = plugin || uses("{prompt_file}") || uses("{prompt_dir}");
            let file = if wants_file {
                write_prompt_file(&spec.cli_id, &spec.slot_id, text, plugin)
            } else {
                None
            };
            if wants_file && file.is_none() {
                tracing::warn!(slot = %spec.slot_id, cli = %spec.cli_id,
                    "could not write this seat's system prompt to a file; starting without it");
            } else {
                let (f, dir) = file.clone().unwrap_or_default();
                let fill = |t: &str| fill_prompt(t, text, &f, &dir);
                for a in chosen.0 {
                    args.push(fill(a));
                }
                if let Some(e) = by_env {
                    for (k, v) in e {
                        prompt_env.push((k.clone(), fill(v)));
                    }
                }
                tracing::info!(slot = %spec.slot_id, mode = chosen.1, bytes = text.len(),
                               by_env = by_env.is_some(), "gave this seat its own system prompt");
            }
        }
    }

    // How this CLI is told which model to run, when it takes a flag at all.
    // Codex and opencode are told in their own protocols instead, so their
    // descriptors declare no model_args and the driver carries it.
    if let Some(model) = &spec.model {
        if d.cli.model_args.is_empty() {
            if matches!(d.driver, DriverSpec::McpOnly | DriverSpec::StreamJson { .. }) {
                tracing::warn!(cli = %spec.cli_id, %model,
                    "a model was requested but this CLI declares no model_args and its \
                     driver cannot ask for one; it will run its default");
            }
        } else {
            for a in &d.cli.model_args {
                args.push(a.replace("{model}", model));
            }
        }
    }

    // Descriptor environment first, the shim's own afterwards so it wins.
    let mut env = descriptor_env(&d);
    env.extend(prompt_env);
    env.extend(shim_env(&spec, &ctx.sock_path));
    // Point the CLI at this seat's own data directory. Last, so it wins over
    // anything inherited -- and NOT over an explicit value in the descriptor
    // or the spawn spec, which are deliberate.
    if let Some((key, value, _)) = &store {
        if !env.iter().any(|(k, _)| k == key) && !spec.env.iter().any(|(k, _)| k == key) {
            env.push((key.clone(), value.clone()));
        }
    }

    tracing::info!(cli = %spec.cli_id, agent = %spec.agent_id, mode = mode_name,
        cwd = %spec.cwd, "spawning: {} {}", bin, args.join(" "));

    // KEEPING THE CLI ALIVE WHEN THE NODE RESTARTS.
    //
    // Normally the CLI is the node's own child on a pty the node owns, which
    // makes the node's lifetime the agent's: stop the node and the master
    // closes, the slave sends SIGHUP, and the conversation dies. So
    // deploying a node binary ends every conversation on that machine.
    //
    // With `[supervise] kind = "tmux"` the CLI lives in a tmux session
    // instead. The tmux server owns the real pty and outlives the node; the
    // node attaches a SECOND, grouped client under a pty of its own, which
    // is exactly what adoption already does, so everything downstream of the
    // pty is unchanged. Restart the node and it finds the session still
    // there and attaches again.
    //
    // If anything about that fails we fall through and start the CLI
    // directly. A seat that comes up the old way beats a seat that does not
    // come up.
    let mut held: Option<String> = None;
    let mut reattached = false;
    if matches!(spec.mode, RunMode::Interactive)
        && d.supervise.as_ref().is_some_and(|s| s.kind == "tmux")
    {
        let name = crate::tmux::held_session(&spec.slot_id);
        let already = crate::tmux::has_session(&name);
        // AM I ALREADY WATCHING THIS SEAT?
        //
        // A view session is this node's own grouped client, named after the
        // slot, so a client on one means a start of this seat already
        // succeeded. Starting a second collides on that name: the new client
        // dies in milliseconds, the exit is reported, the hub decides the
        // seat is empty, and `attach` -- which starts a seat that has no
        // process -- starts it again. That is a loop with no exit, and it
        // ran four times on the live fleet before anybody could type.
        //
        // So the answer is not a second view. If the agent behind that view
        // is still ours, the hub has merely lost sight of it and the remedy
        // is to say so again. If it is not ours, the client is an orphan
        // from a start that went wrong, and it has to go or this seat can
        // never be started again.
        let view = tmux_view(&spec.slot_id);
        if already && crate::tmux::view_has_client(&view) {
            match ctx.agent_in_slot(&spec.slot_id) {
                Some(mine) => {
                    tracing::info!(slot = %spec.slot_id, agent = %mine.agent_id, view = %view,
                        "this node is already watching this seat; re-announcing it \
                         instead of starting a second view");
                    // Re-announce rather than spawn. The hub keys an agent by
                    // id, so this restores the row it lost without giving one
                    // seat two agents -- and the caller's agent id is simply
                    // not used, which is correct: nothing was started.
                    reannounce(&ctx, &mine).await;
                    return Ok(());
                }
                None => {
                    crate::tmux::force_clear_view(&view);
                }
            }
        }
        let ready = if already {
            // THE WHOLE POINT: the CLI is still in there, from before this
            // node started. Attach to it rather than starting a second one.
            tracing::info!(slot = %spec.slot_id, session = %name,
                "the CLI is still held in tmux from before; attaching to it");
            // Say it again to a session this node did not create: `hold`
            // never ran for it, so nothing has told it that a CLI which
            // exits must take the session with it. Without this a seat held
            // across a node upgrade keeps whatever the host's own tmux
            // config says, which is exactly the seat most likely to
            // outlive several node binaries.
            crate::tmux::lock_held(&name);
            reattached = true;
            Ok(())
        } else {
            crate::tmux::hold(&name, &spec.cwd, &bin, &args, &env, &d.cli.env_unset)
        };
        match ready {
            Ok(()) => {
                // The node's child becomes a grouped client on that session.
                // `-t` groups rather than attaches, so this client has its
                // OWN size and a viewer on a small terminal does not resize
                // the CLI under its own feet.
                bin = "tmux".to_string();
                args = vec![];
                // Whichever tmux server this node is pointed at -- normally
                // the user's own, under test one of the suite's own. `-L`
                // has to come BEFORE the subcommand, and without it the
                // client talks to the default server, does not find the
                // session, and exits at once: the seat then looks alive
                // (the CLI really is held) with no pty behind it, so
                // nothing can attach to it or type at it.
                if let Some(sock) = crate::tmux::socket() {
                    args.extend(["-L".to_string(), sock]);
                }
                args.extend([
                    "new-session".to_string(),
                    "-t".to_string(),
                    name.clone(),
                    "-s".to_string(),
                    tmux_view(&spec.slot_id),
                ]);
                // Our own view from the previous node may still be there --
                // the client died with the node but tmux keeps the grouped
                // session -- and `new-session -s` collides with it, which is
                // exactly the case this feature exists for.
                crate::tmux::clear_stale_view(&tmux_view(&spec.slot_id));
                held = Some(name);
                // Reattaching means the ARGUMENTS ARE DISCARDED: the CLI in
                // that pane was started earlier, with whatever it was given
                // then. For most of them that changes nothing, but a seat
                // whose system prompt was set since is a seat the operator
                // believes they have changed and have not -- so it is said
                // out loud rather than left to be noticed in the agent's
                // behaviour.
                if reattached {
                    tracing::info!(slot = %spec.slot_id,
                        "reattached, so this start's arguments do not apply to the running CLI");
                    if spec
                        .system_prompt
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|t| !t.is_empty())
                    {
                        ctx.note_when_started(
                            &spec.agent_id,
                            "[nexmesh] this seat's CLI was still held in tmux from before, \
                             so it was reattached rather than started. It is running with \
                             whatever system prompt it was given then: this seat's current \
                             one reaches it only when the process itself is replaced."
                                .to_string(),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(slot = %spec.slot_id, error = %e,
                    "could not hold this CLI in tmux; starting it directly instead");
                ctx.note_when_started(
                    &spec.agent_id,
                    format!(
                        "[nexmesh] this seat was to be held in tmux so it survives a node \
                         restart, but that failed ({e}). It is running the ordinary way \
                         instead, which means a node restart will end it."
                    ),
                );
            }
        }
    }

    // An interactive agent runs under a pty so it can be WATCHED AND TAKEN OVER
    // from any machine. A headless agent is a protocol on a pipe; there is no
    // terminal to take over, so it gets pipes and its attach is view-only.
    // WHICH CONVERSATION THIS PROCESS IS IN, as far as this node can tell
    // from what it just put on the command line.
    //
    // Distinct from `fresh_session`, which is only a conversation this spawn
    // NAMED and which is reported to the hub once it exists. This one
    // includes a resumed conversation too, and it is not reported anywhere:
    // it is kept on the node, where it answers "which file do I read to see
    // how full this agent's context is".
    //
    // Known here and nowhere else. Waiting for the hub's record instead left
    // a second seat in the same directory unmeasurable, because the node
    // will not guess between two conversations in one project directory --
    // and rightly, since measuring one seat against another's context is
    // worse than not measuring it.
    let started_on = fresh_session.clone().or(match &plan {
        ResumePlan::Resume(h) => Some(h.clone()),
        _ => None,
    });
    if matches!(spec.mode, RunMode::Interactive) {
        spawn_on_pty(ctx, spec, bin, args, env, d.cli.env_unset.clone(),
                     mode.prompt_on_stdin, d.driver.clone(), fresh_session,
                     started_on, mode.session_path.clone(), held).await
    } else {
        spawn_on_pipes(ctx, spec, bin, args, env, d.cli.env_unset.clone(),
                       mode.prompt_on_stdin, d.driver.clone(), fresh_session,
                       started_on, mode.session_path.clone()).await
    }
}

/// Does this CLI's own descriptor say this mode is an agent's prompt?
///
/// Read from the descriptor at the moment the agent comes up rather than
/// threaded down through five spawn paths, and false whenever the CLI or the
/// mode is not there -- a seat that cannot be looked up is not one to type
/// at.
fn typed_delivery(ctx: &Arc<NodeCtx>, cli_id: &str, mode: RunMode) -> bool {
    let name = match mode {
        RunMode::Headless => "headless",
        RunMode::Interactive => "interactive",
    };
    ctx.descriptor(cli_id)
        .and_then(|d| d.modes.get(name).map(|m| m.typed_delivery))
        .unwrap_or(false)
}

/// Can this driver take a message mid-turn, or must it wait for a tool call?
///
/// Getting this wrong is silent: the hub simply never pushes, and messages sit
/// unacked while the agent looks idle. Every driver that is not the MCP floor
/// belongs here.
pub fn push_capable(driver: &DriverSpec) -> bool {
    match driver {
        DriverSpec::McpOnly => false,
        DriverSpec::StreamJson { .. }
        | DriverSpec::AppServer
        | DriverSpec::HttpServer { .. }
        | DriverSpec::Plugin { .. } => true,
    }
}

/// The node's own tmux session name for an adopted seat.
///
/// Deterministic on purpose: `nexmesh-<slot>` can be addressed later, which
/// an auto-named session cannot. It also makes the adoption visible in
/// `tmux ls`, where an unexplained second session is alarming.
/// Tell the hub again about an agent this node already has.
///
/// For the one case where a start must NOT start anything: the seat's CLI is
/// held in tmux, this node is already watching it, and the hub has lost the
/// row -- after its own restart, or after an earlier start of this seat
/// collided and reported an exit. Re-announcing is the whole remedy, and it
/// is the only thing that can be done without giving one seat two agents.
///
/// Deliberately a re-announcement of the EXISTING agent, with its existing
/// id and its existing identity triple, not a new registration: the id is
/// how the hub keys everything it knows about a running process.
async fn reannounce(ctx: &Arc<NodeCtx>, rec: &crate::ctx::AgentRec) {
    let (typable, io_kind) = match ctx.io.lock().unwrap().get(&rec.agent_id) {
        Some(io) => (io.typable(), io.io_kind_name().to_string()),
        // No I/O handle means the view is there but nothing is reading it,
        // which is not an agent anybody can reach. Say nothing rather than
        // announce a seat that cannot be typed at.
        None => {
            tracing::warn!(agent = %rec.agent_id, slot = %rec.slot_id,
                "asked to re-announce an agent with no live I/O; leaving it alone");
            return;
        }
    };
    let r = ctx
        .send_hub(&proto::NodeToHub::AgentUp {
            agent_id: rec.agent_id.clone(),
            slot_id: rec.slot_id.clone(),
            proc: rec.proc.clone(),
            provenance: Provenance::Managed,
            cwd: String::new(),
            resume_handle: None,
            push_capable: pushable(&rec.driver, RunMode::Interactive, typable),
            io_kind: Some(io_kind),
            typed_delivery: typable
                && typed_delivery(ctx, &rec.cli_id, RunMode::Interactive),
        })
        .await;
    if let Err(e) = r {
        tracing::warn!(agent = %rec.agent_id, error = %e, "could not re-announce this agent");
    }
}

pub fn tmux_view(slot_id: &str) -> String {
    format!("nexmesh-{slot_id}")
}

/// May the hub push a message to this agent rather than wait for its next
/// tool call -- and if so, is it safe to?
///
/// Two routes, one answer. A structured driver takes the message on stdin as
/// a frame. A pty takes it as keystrokes at the CLI's own prompt, which is
/// the only route an interactive seat has.
///
/// The gate is the DRIVER in both cases, and that is the safety property
/// worth stating plainly: an adopted tmux session also has a pty, but what is
/// in that pane may be a shell prompt, where typing a teammate's message
/// would EXECUTE it. Such a session is `mcp_only` and so excluded. A driver
/// that takes structured input is proof the process on the other end is the
/// CLI the node started.
pub fn pushable(driver: &DriverSpec, mode: RunMode, typable: bool) -> bool {
    push_capable(driver) && (matches!(mode, RunMode::Headless) || typable)
}

/// Everything both spawn paths do once the process exists: record the identity
/// triple, register it, and tell the hub.
async fn register_started(
    ctx: &Arc<NodeCtx>,
    spec: &SpawnSpec,
    pid: i32,
    io: Arc<crate::ctx::AgentIo>,
    driver: DriverSpec,
    // The conversation this process was started on, as far as the command
    // line says: what makes this agent's context measurable from the moment
    // it comes up rather than once the hub has a record of it.
    conversation: Option<String>,
    // Is this seat held in tmux, with our own child being a client onto it?
    supervised: bool,
) -> Result<ProcIdentity> {
    // Capture the identity triple IMMEDIATELY: if we waited, the pid could be
    // recycled and we would record an identity that is not this process.
    let start_ticks = proc::start_ticks(pid).unwrap_or(0);
    let identity = ProcIdentity { node_id: ctx.node_name.clone(), pid, start_ticks };

    ctx.agents.lock().unwrap().insert(
        spec.agent_id.clone(),
        AgentRec {
            agent_id: spec.agent_id.clone(),
            slot_id: spec.slot_id.clone(),
            session_id: spec.session_id.clone(),
            cli_id: spec.cli_id.clone(),
            proc: identity.clone(),
            driver: driver.clone(),
            // An adopted session is joined under a session name of our own
            // (see clis/tmux.toml), so the node can ask tmux where ITS view
            // is pointing rather than guess.
            // Set for an adopted terminal AND for a supervised one: both are
            // a grouped client onto a pane, and the checks that run before
            // anything is typed into a pane need to know which pane.
            tmux_view: (spec.tmux_target.is_some() || supervised)
                .then(|| tmux_view(&spec.slot_id)),
            // Where its conversations live, and which one it is in. Both are
            // what `context::Watcher` needs to find the file that says how
            // full this agent is; the conversation is refined later, when
            // this CLI's own shim registers and says what the CLI thinks.
            cwd: spec.cwd.clone(),
            // What this node just put on the command line. Refined later
            // if this CLI's own shim reports a different one -- which is
            // what `/clear` looks like from here.
            conversation,
            // The level the hub read when it decided to start this seat.
            // Unknown or absent parses to `Agent`: a hub from before this
            // sends nothing, and the plain-agent tool list is the safe
            // answer.
            authority: spec
                .authority
                .as_deref()
                .map(nexmesh_proto::authority::Authority::parse)
                .unwrap_or_default(),
        },
    );
    // Read before the handle is given away: the map owns it after this.
    let (typable, io_kind) = (io.typable(), io.io_kind_name().to_string());
    ctx.io.lock().unwrap().insert(spec.agent_id.clone(), io);

    ctx.send_hub(&proto::NodeToHub::AgentUp {
        agent_id: spec.agent_id.clone(),
        slot_id: spec.slot_id.clone(),
        proc: identity.clone(),
        provenance: Provenance::Managed,
        cwd: spec.cwd.clone(),
        resume_handle: None,
        // A driver's structured fast path is a HEADLESS mechanism. An
        // interactive slot has a pty instead -- and a pty is a keyboard, so
        // the message is typed at the CLI's own prompt exactly as a person
        // would type it. That is worth having: delivery there used to wait
        // for the agent to call a tool of its own accord, which for an
        // interactive seat can be a very long time, or never.
        //
        // The gate is the DRIVER, not the terminal: see `pushable`.
        push_capable: pushable(&driver, spec.mode, typable),
        io_kind: Some(io_kind),
        // A CLI with no driver can still be typed at, if its descriptor says
        // that its interactive mode is the agent's own prompt. Without this a
        // seat the fleet itself put in a terminal has no route at all: the
        // hub cannot push and nothing drives it, so a message waits for a
        // tool call the CLI may never make. The claim is the descriptor's,
        // never inferred -- `tmux` has an interactive mode too, and what is
        // at that prompt is a shell.
        typed_delivery: typable && typed_delivery(ctx, &spec.cli_id, spec.mode),
    })
    .await?;

    // An adopted terminal may already hold a CLI that registered earlier --
    // which is the usual order: you start the CLI, and adopt its pane
    // afterwards. Off the spawn path so tmux can take its time answering.
    if spec.tmux_target.is_some() {
        let ctx2 = ctx.clone();
        let agent = spec.agent_id.clone();
        tokio::spawn(async move {
            crate::termlink::on_terminal_adopted(&ctx2, &agent).await;
        });
    }

    // Anything the start path wanted to say about HOW this agent was started.
    // It is said here and not where it was decided, because until the line
    // above the hub had no row for this agent and a note about it was
    // discarded -- see `note_when_started`.
    ctx.flush_notes(&spec.agent_id).await;
    Ok(identity)
}

async fn announce_exit(ctx: &Arc<NodeCtx>, agent_id: &str, code: Option<i32>) {
    tracing::info!(%agent_id, ?code, "agent exited");
    ctx.agents.lock().unwrap().remove(agent_id);
    // Collect the viewers BEFORE any await: holding a std MutexGuard across an
    // await makes the future non-Send, and tokio::spawn then refuses it.
    let (attaches, stranded): (Vec<String>, Vec<i64>) = {
        let removed = ctx.io.lock().unwrap().remove(agent_id);
        match removed {
            Some(io) => (io.attach_ids(), io.drain_pending()),
            None => (vec![], vec![]),
        }
    };
    // Anything injected but never echoed never reached a model. Say so, or the
    // hub leaves it in flight forever and nobody ever sees it.
    if !stranded.is_empty() {
        tracing::info!(%agent_id, count = stranded.len(),
            "releasing injected messages the agent never confirmed");
        let _ = ctx
            .send_hub(&proto::NodeToHub::InjectionsLost {
                agent_id: agent_id.to_string(),
                message_ids: stranded,
            })
            .await;
    }
    // Tell every viewer the session is over before forgetting the agent.
    for attach_id in attaches {
        let _ = ctx
            .send_hub(&proto::NodeToHub::AttachClosed {
                attach_id,
                reason: "the agent exited".into(),
            })
            .await;
    }
    let _ = ctx
        .send_hub(&proto::NodeToHub::AgentExited {
            agent_id: agent_id.to_string(),
            code,
            signal: None,
        })
        .await;
}

async fn spawn_on_pty(
    ctx: Arc<NodeCtx>,
    spec: SpawnSpec,
    bin: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    env_unset: Vec<String>,
    prompt_on_stdin: bool,
    driver: DriverSpec,
    // The conversation id this process was TOLD to use, if the descriptor
    // says how. Reported to the hub so the next spawn can resume it.
    fresh_session: Option<String>,
    // The conversation this process is ACTUALLY in: the one just named, or
    // the one it was told to resume. Kept on the node to find the file that
    // says how full this agent's context is.
    started_on: Option<String>,
    // Where this CLI keeps conversations, so the report can wait until the
    // conversation really exists rather than overwriting a good record with
    // a handle the CLI never got as far as creating.
    session_path: Option<String>,
    // The tmux session HOLDING the CLI, when this seat is supervised. Our
    // own child is then a tmux client, not the CLI: the agent's identity has
    // to be the process in the pane, or every check that asks "is this still
    // the same CLI" is asking about the wrong process.
    held: Option<String>,
) -> Result<()> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pair = native_pty_system()
        .openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 })
        .context("opening a pty")?;

    let mut cmd = CommandBuilder::new(&bin);
    for a in &args {
        cmd.arg(a);
    }
    cmd.cwd(&spec.cwd);
    for k in &env_unset {
        cmd.env_remove(k);
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // A TUI needs to know what terminal it is talking to -- and the terminal
    // it is talking to is the PTY THIS NODE JUST CREATED, not whatever
    // terminal, if any, happened to launch the daemon.
    //
    // This used to fall back only when TERM was UNSET, which is not the case
    // that bites. Started by sysvinit, the node's own environment carries
    // `TERM=unknown`: that is "set", so the fallback never fired and every
    // agent inherited it. A TUI told it is talking to an unknown terminal
    // turns off colour, the mouse and the alternate screen, so every seat
    // respawned by a service restart came back monochrome and unable to
    // scroll -- while seats started earlier from somebody's shell were fine,
    // which is what made it look like a change in nexmesh rather than a
    // change in how the node was started.
    //
    // So it is set unconditionally, and only an explicit TERM from the
    // descriptor or the spawn spec wins: those are deliberate statements
    // about a particular CLI, and this is a default.
    let term_given = env.iter().any(|(k, _)| k == "TERM")
        || spec.env.iter().any(|(k, _)| k == "TERM");
    if !term_given {
        cmd.env("TERM", "xterm-256color");
    }

    let mut child = pair.slave.spawn_command(cmd).with_context(|| format!("spawning {bin} on a pty"))?;
    // The slave fd must go, or the master never sees EOF when the child exits.
    drop(pair.slave);

    let child_pid = child
        .process_id()
        .ok_or_else(|| anyhow::anyhow!("child exited before reporting a pid"))? as i32;

    // WHICH PROCESS IS THE AGENT.
    //
    // Normally our child IS the CLI. Under tmux supervision our child is a
    // tmux CLIENT and the CLI is the process in the pane, so the agent's
    // identity has to be that one -- otherwise every check that asks "is
    // this still the same CLI" (`is_same_process`), and the guard that asks
    // "is the CLI really inside the pane this view looks at" before typing a
    // teammate's message into it, are both asking about the wrong process.
    // The typing guard in particular would answer no, because a client is a
    // sibling of the pane's process rather than a descendant of it, and the
    // seat would silently stop receiving messages.
    let pid = match &held {
        None => child_pid,
        Some(name) => {
            // tmux has to have got as far as running the pane's command.
            let mut found = None;
            for _ in 0..40 {
                if let Some(p) = crate::tmux::pane_pid(name) {
                    found = Some(p);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            match found {
                Some(p) => p,
                None => {
                    // Nothing in the pane after two seconds means the CLI
                    // never started. Better to fail the spawn than to
                    // register an agent whose pid is a tmux client.
                    crate::tmux::release(name);
                    anyhow::bail!(
                        "tmux held the session but nothing is running in its pane; \
                         the CLI did not start"
                    );
                }
            }
        }
    };

    let reader = pair.master.try_clone_reader().context("cloning the pty reader")?;
    let mut writer = pair.master.take_writer().context("taking the pty writer")?;

    // pty I/O is blocking, so it lives on threads and is bridged with channels.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        use std::io::Write;
        while let Some(bytes) = in_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });

    let io = crate::ctx::AgentIo::new(crate::ctx::IoKind::Pty, Some(in_tx.clone()), Some(pair.master));
    // When this pty is a TMUX CLIENT -- a seat held in tmux, or an adopted
    // pane -- resizing it is not enough to resize what the CLI sees. tmux
    // learns of a client resize only by SIGWINCH, and one that arrives
    // before its handler is installed is gone: the default disposition of
    // SIGWINCH is to ignore it. So the size is stated to tmux as well. See
    // `AgentIo::set_view`.
    if held.is_some() || spec.tmux_target.is_some() {
        io.set_view(&tmux_view(&spec.slot_id));
        // A SEAT HELD IN TMUX IS ONE AGENT, NOT A WAY ONTO THE MACHINE.
        //
        // This client had tmux's whole keyboard, so `Ctrl-B %` in an attach
        // split the pane and handed whoever was attached a SHELL on this
        // host, and `Ctrl-B c` another. The lock is on the view the node
        // created, so the operator's own `tmux attach -t nexmesh-cli-<slot>`
        // is untouched -- that is their way in when they do want a terminal
        // here. Off this thread: tmux may take a moment to answer, and a
        // lock that fails is a log line, not a seat that will not start.
        let view = tmux_view(&spec.slot_id);
        tokio::task::spawn_blocking(move || crate::tmux::lock_view(&view));
    }
    register_started(&ctx, &spec, pid, io.clone(), driver, started_on.clone(),
                     held.is_some())
        .await?;
    report_session(&ctx, &spec, &fresh_session, session_path.clone()).await;

    if prompt_on_stdin {
        let _ = in_tx.send(format!("{}\n", spec.prompt).into_bytes()).await;
    }

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut reader = reader;
        let mut buf = vec![0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let io = io.clone();
        tokio::spawn(async move {
            let mut seq = 0u64;
            // WHAT THE MODEL IS DOING, read from the screen at most once a
            // second -- during output as well as in the pauses, because a
            // working CLI redraws its spinner without a break and the quiet
            // tick below never fires while it works.
            let mut looked = std::time::Instant::now();
            let mut known = false;
            loop {
                // A pause in the output is the only moment a screen can be
                // read for what it is: mid-burst it is half-drawn, and a
                // prompt half-drawn is not a prompt yet.
                let next = tokio::time::timeout(
                    std::time::Duration::from_millis(1200),
                    out_rx.recv(),
                )
                .await;
                let quiet = next.is_err();
                if quiet || looked.elapsed() >= std::time::Duration::from_secs(1) {
                    looked = std::time::Instant::now();
                    let screen = String::from_utf8_lossy(&io.scrollback(8192)).to_string();
                    if let Some(state) = crate::driver::screen_activity(&screen, io.idle_ms(), known) {
                        known |= state == proto::ActivityState::Active;
                        ctx.report_activity(&agent_id, &io, state).await;
                    }
                }
                match next {
                    Ok(Some(chunk)) => {
                        seq += 1;
                        emit_output(&ctx, &agent_id, &io, &chunk, seq).await;
                    }
                    // The pty closed: the exit watcher deals with the rest.
                    Ok(None) => break,
                    // Every quiet tick, not only the ones that follow new
                    // output: a prompt raised while somebody was attached,
                    // and then abandoned when they detached, produces no
                    // further output at all and would otherwise sit there
                    // for ever. The check is cheap and remembers what it
                    // has already decided about.
                    Err(_) => {
                        crate::prompts::manage_if_unattached(&ctx, &agent_id, &io).await;
                    }
                }
            }
        });
    }

    {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let held = held.clone();
        let slot_id = spec.slot_id.clone();
        tokio::spawn(async move {
            let code = tokio::task::spawn_blocking(move || {
                child.wait().ok().map(|s| s.exit_code() as i32)
            })
            .await
            .ok()
            .flatten();
            // A DEAD CLIENT IS NOT A DEAD CLI.
            //
            // Under supervision what just exited is our tmux client. The CLI
            // is still sitting in its session -- which is the entire point --
            // and announcing an exit here would mark a living agent dead,
            // throw its viewers off and invite recovery to start a SECOND
            // one in the same seat on top of it.
            if let Some(name) = &held {
                if crate::tmux::has_session(name) {
                    tracing::info!(
                        slot = %slot_id, session = %name,
                        "our view of this seat closed but the CLI is still held in tmux"
                    );
                    return;
                }
            }
            announce_exit(&ctx, &agent_id, code).await;
        });
    }
    Ok(())
}

async fn spawn_on_pipes(
    ctx: Arc<NodeCtx>,
    spec: SpawnSpec,
    bin: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    env_unset: Vec<String>,
    prompt_on_stdin: bool,
    driver: DriverSpec,
    // The conversation id this process was TOLD to use, if the descriptor
    // says how. Reported to the hub so the next spawn can resume it.
    fresh_session: Option<String>,
    // The conversation this process is ACTUALLY in: the one just named, or
    // the one it was told to resume. Kept on the node to find the file that
    // says how full this agent's context is.
    started_on: Option<String>,
    // Where this CLI keeps conversations, so the report can wait until the
    // conversation really exists rather than overwriting a good record with
    // a handle the CLI never got as far as creating.
    session_path: Option<String>,
) -> Result<()> {
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(&args)
        .current_dir(&spec.cwd)
        .kill_on_drop(false)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for k in &env_unset {
        cmd.env_remove(k);
    }
    for (k, v) in &env {
        cmd.env(k, v);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().with_context(|| format!("spawning {bin}"))?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("child exited before reporting a pid"))? as i32;

    // opencode runs its own HTTP server. Start it, read the URL it prints,
    // and drive it over that -- no stdio protocol, no pty.
    if let DriverSpec::HttpServer { basic_auth_user, password_env, attach_args } = &driver {
        // A per-agent password, so nobody else with a shell on this machine can
        // drive the agent through its loopback port.
        let password = password_env.as_ref().map(|_| {
            use std::fmt::Write;
            let mut rng = [0u8; 24];
            getrandom_bytes(&mut rng);
            rng.iter().fold(String::new(), |mut acc, b| {
                let _ = write!(acc, "{b:02x}");
                acc
            })
        });
        if let (Some(var), Some(pw)) = (password_env, &password) {
            cmd.env(var, pw);
        }

        let mut child = cmd.spawn().with_context(|| format!("spawning {bin}"))?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("child exited before reporting a pid"))? as i32;

        // The URL is only knowable from the startup line, because the port is
        // asked for as 0 so two agents cannot collide.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout from {bin}"))?;
        let (url_tx, url_rx) = tokio::sync::oneshot::channel::<String>();
        let (inject_tx, inject_rx) = tokio::sync::mpsc::channel::<Vec<proto::Envelope>>(64);

        let attach_cmd = (!attach_args.is_empty()).then(|| (bin.clone(), attach_args.clone()));
        let base_for_io = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

        // Register before the URL is known: the hub should see the agent as up
        // even if the server is slow to announce itself.
        let io = crate::ctx::AgentIo::new_push(inject_tx);
        register_started(&ctx, &spec, pid, io.clone(), driver.clone(), started_on.clone(),
                         false)
            .await?;

        {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            let io2 = io.clone();
            let base2 = base_for_io.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                let mut seq = 0u64;
                let mut sent = Some(url_tx);
                while let Ok(Some(line)) = lines.next_line().await {
                    seq += 1;
                    emit_output(&ctx2, &agent_id, &io2, format!("{line}\n").as_bytes(), seq).await;
                    // Only the FIRST usable URL counts, and a line that is not
                    // one must leave the channel alone -- the very first line
                    // opencode prints is a warning about the password.
                    if sent.is_some() {
                        if let Some(url) = crate::opencode::parse_server_url(&line) {
                            *base2.lock().unwrap() = url.clone();
                            if let Some(tx) = sent.take() {
                                let _ = tx.send(url);
                            }
                        }
                    }
                }
            });
        }

        if let Some(err) = child.stderr.take() {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            let io2 = io.clone();
            tokio::spawn(async move { pump_stderr(ctx2, agent_id, io2, err).await });
        }
        {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                announce_exit(&ctx2, &agent_id, code).await;
            });
        }

        {
            let ctx2 = ctx.clone();
            let spec2 = spec.clone();
            let user = basic_auth_user.clone();
            let io2 = io.clone();
            tokio::spawn(async move {
                let url = match tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    url_rx,
                )
                .await
                {
                    Ok(Ok(u)) => u,
                    _ => {
                        tracing::error!(agent = %spec2.agent_id,
                            "the agent's server never printed a URL");
                        return;
                    }
                };
                tracing::info!(agent = %spec2.agent_id, %url, "agent server is up");
                // Record where it is and how to open a terminal onto it, so an
                // attach can start a real TUI client on demand.
                io2.set_server(url.clone(), attach_cmd);
                let server = crate::opencode::Server::new(url.clone(), user, password);
                crate::opencode::run(
                    ctx2,
                    spec2.agent_id.clone(),
                    spec2.slot_id.clone(),
                    server,
                    inject_rx,
                    spec2.prompt.clone(),
                    spec2.cwd.clone(),
                    spec2.resume_handle.clone(),
                    spec2.model.clone(),
                )
                .await;
            });
        }
        return Ok(());
    }

    // Codex is a JSON-RPC peer, not a text stream: hand its stdio to the
    // app-server driver, which owns the handshake, threads and turns.
    if matches!(driver, DriverSpec::AppServer) {
        let (cstdin, cstdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(i), Some(o)) => (i, o),
            _ => anyhow::bail!("codex app-server gave us no stdio"),
        };
        let (inject_tx, inject_rx) = tokio::sync::mpsc::channel::<Vec<proto::Envelope>>(64);
        let io = crate::ctx::AgentIo::new_push(inject_tx);
        register_started(&ctx, &spec, pid, io.clone(), driver.clone(), started_on.clone(),
                         false)
            .await?;
        if let Some(err) = child.stderr.take() {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            let io2 = io.clone();
            tokio::spawn(async move { pump_stderr(ctx2, agent_id, io2, err).await });
        }
        {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                announce_exit(&ctx2, &agent_id, code).await;
            });
        }
        tokio::spawn(crate::codex::run(
            ctx.clone(),
            spec.agent_id.clone(),
            spec.slot_id.clone(),
            io,
            cstdin,
            cstdout,
            inject_rx,
            spec.prompt.clone(),
            spec.cwd.clone(),
            spec.resume_handle.clone(),
            spec.model.clone(),
        ));
        return Ok(());
    }

    // A plugin-driven CLI: the node still owns the process, and an external
    // helper does the talking. See `nexmesh_proto::plugin`.
    if let DriverSpec::Plugin { command, args } = &driver {
        if prompt_on_stdin {
            tracing::warn!(cli = %spec.cli_id,
                "prompt_on_stdin is ignored for a plugin driver: the plugin receives the \
                 prompt in its hello frame and decides how this CLI wants it");
        }
        let cli_stdout = match child.stdout.take() {
            Some(o) => o,
            None => anyhow::bail!("a plugin-driven CLI must give us its stdout"),
        };
        // The CLI's stdin stays open behind a channel, exactly as for a
        // stream-json agent: closing it would end the session.
        let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        if let Some(mut stdin) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(bytes) = in_rx.recv().await {
                    if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                        break;
                    }
                }
            });
        }
        let (inject_tx, inject_rx) = tokio::sync::mpsc::channel::<Vec<proto::Envelope>>(64);
        let io = crate::ctx::AgentIo::new_push(inject_tx);
        register_started(&ctx, &spec, pid, io.clone(), driver.clone(), started_on.clone(),
                         false)
            .await?;
        if let Some(err) = child.stderr.take() {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            let io2 = io.clone();
            tokio::spawn(async move { pump_stderr(ctx2, agent_id, io2, err).await });
        }
        {
            let ctx2 = ctx.clone();
            let agent_id = spec.agent_id.clone();
            // This clone keeps the CLI's stdin channel open for as long as the
            // CLI itself is alive, and NOT merely as long as the plugin is.
            // Without it, a plugin that exits drops the last sender, the
            // writer task closes stdin, the CLI reads EOF and ends -- so a
            // crashing plugin would take a working agent with it instead of
            // degrading to MCP delivery.
            let keep_stdin_open = in_tx.clone();
            tokio::spawn(async move {
                let code = child.wait().await.ok().and_then(|s| s.code());
                drop(keep_stdin_open);
                announce_exit(&ctx2, &agent_id, code).await;
            });
        }
        tokio::spawn(crate::plugin::run(
            ctx.clone(),
            crate::plugin::PluginRun {
                agent_id: spec.agent_id.clone(),
                slot_id: spec.slot_id.clone(),
                cli_id: spec.cli_id.clone(),
                command: command.clone(),
                args: args.clone(),
                cwd: spec.cwd.clone(),
                prompt: spec.prompt.clone(),
                resume: spec.resume_handle.clone(),
                model: spec.model.clone(),
            },
            io,
            in_tx,
            cli_stdout,
            inject_rx,
        ));
        return Ok(());
    }

    // Keep stdin OPEN behind a channel. Writing the prompt and then dropping
    // it would send EOF, which ends a stream-json session immediately -- and
    // with it any chance of injecting a teammate's message mid-turn.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(bytes) = in_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin here closes it, ending the session cleanly.
        });
    }

    let io = crate::ctx::AgentIo::new(crate::ctx::IoKind::Pipes, Some(in_tx.clone()), None);
    register_started(&ctx, &spec, pid, io.clone(), driver.clone(), started_on.clone(),
                         false)
            .await?;
    report_session(&ctx, &spec, &fresh_session, session_path.clone()).await;

    if prompt_on_stdin {
        let line = match &driver {
            // Claude's stream-json input wants one JSON user message per line.
            DriverSpec::StreamJson { .. } => {
                format!("{}\n", crate::driver::user_message(&spec.prompt))
            }
            _ => format!("{}\n", spec.prompt),
        };
        let _ = in_tx.send(line.into_bytes()).await;
    }

    if let Some(out) = child.stdout.take() {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let slot_id = spec.slot_id.clone();
        let io = io.clone();
        match &driver {
            // A driven CLI's stdout is a protocol, so parse it: acks, token
            // usage and the resume handle all come from there.
            DriverSpec::StreamJson { .. } => {
                tokio::spawn(async move {
                    crate::driver::run_stream_json(ctx, agent_id, slot_id, io, out).await
                });
            }
            _ => {
                tokio::spawn(async move { pump(ctx, agent_id, io, out).await });
            }
        }
    }
    // An agent CLI reports its failures on stderr. It goes to the transcript
    // AND to this node's log: when a spawn dies early, the node log is where
    // someone will look first, and a transcript they have to go fetch is one
    // step too far.
    if let Some(err) = child.stderr.take() {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let io = io.clone();
        tokio::spawn(async move { pump_stderr(ctx, agent_id, io, err).await });
    }

    {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            announce_exit(&ctx, &agent_id, code).await;
        });
    }
    Ok(())
}

/// One place where agent output goes: into scrollback, to the hub for the
/// transcript, and to every viewer currently attached.
/// Tell the hub the conversation id this process was told to use, so a
/// replacement can resume it rather than starting the thinking over.
async fn report_session(
    ctx: &Arc<NodeCtx>,
    spec: &SpawnSpec,
    fresh: &Option<String>,
    // Where this CLI keeps conversations, when it says. Without it nothing
    // is checkable and the handle is reported as it always was.
    session_path: Option<String>,
) {
    let Some(handle) = fresh.clone() else { return };
    let (agent_id, slot_id, cwd) =
        (spec.agent_id.clone(), spec.slot_id.clone(), spec.cwd.clone());
    let ctx = ctx.clone();

    // A HANDLE IS NOT REPORTED UNTIL THE CONVERSATION EXISTS.
    //
    // This used to fire the instant the process started, before the CLI had
    // written anything -- and it OVERWRITES whatever the seat had recorded
    // before. So a CLI that exited at once left the seat pointing at a
    // conversation that was never created, and the real one was gone from
    // every store the hub has. A crash loop did it twice in six seconds on
    // the live fleet, and a seat came back with no history at all.
    //
    // The existence check elsewhere stops a seat DYING on such a ghost. It
    // does nothing about the ghost displacing a good handle, which is this.
    //
    // So: wait for the file. If the CLI dies first, nothing is reported and
    // what the seat already had still stands.
    let Some(tpl) = session_path else {
        let _ = ctx
            .send_hub(&proto::NodeToHub::ResumeHandle {
                agent_id: agent_id.clone(),
                slot_id,
                handle: handle.clone(),
            })
            .await;
        tracing::info!(agent = %agent_id, %handle,
            "named this seat's conversation (this CLI does not say where they live)");
        return;
    };
    tokio::spawn(async move {
        for _ in 0..120 {
            if conversation_exists_at(&tpl, &handle, &cwd) {
                // Locally as well as at the hub: the node is what has to
                // FIND this conversation on disk to measure how full it is.
                ctx.note_conversation(&agent_id, &handle);
                let _ = ctx
                    .send_hub(&proto::NodeToHub::ResumeHandle {
                        agent_id: agent_id.clone(),
                        slot_id,
                        handle: handle.clone(),
                    })
                    .await;
                tracing::info!(agent = %agent_id, %handle,
                    "this seat's conversation exists now; recorded it");
                return;
            }
            // Gone before it wrote anything: say so, and leave the seat's
            // existing record alone rather than replacing it with a ghost.
            if ctx.agent(&agent_id).is_none() {
                tracing::warn!(agent = %agent_id, %handle,
                    "the CLI went before writing its conversation; \
                     leaving this seat's recorded one as it was");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        tracing::warn!(agent = %agent_id, %handle,
            "this conversation has not appeared in 60s; not recording it");
    });
}

pub(crate) async fn emit_output(
    ctx: &Arc<NodeCtx>,
    agent_id: &str,
    io: &Arc<crate::ctx::AgentIo>,
    data: &[u8],
    seq: u64,
) {
    io.push_output(data);
    io.note_output();
    // What the CLI has asked of its terminal, which changes how a message
    // must be typed at it.
    io.note_modes(data);
    let _ = ctx
        .send_hub(&proto::NodeToHub::Output {
            agent_id: agent_id.to_string(),
            seq,
            data: data.to_vec(),
        })
        .await;
    for attach_id in io.attach_ids() {
        let _ = ctx
            .send_hub(&proto::NodeToHub::AttachData { attach_id, data: data.to_vec() })
            .await;
    }
}

/// stderr: logged line by line here, and kept in the transcript.
async fn pump_stderr<R>(
    ctx: Arc<NodeCtx>,
    agent_id: String,
    io: Arc<crate::ctx::AgentIo>,
    reader: R,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let mut seq = 0u64;
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(agent = %agent_id, "stderr: {line}");
        seq += 1;
        emit_output(&ctx, &agent_id, &io, format!("{line}\n").as_bytes(), seq).await;
    }
}

async fn pump<R>(ctx: Arc<NodeCtx>, agent_id: String, io: Arc<crate::ctx::AgentIo>, reader: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut reader = reader;
    let mut buf = vec![0u8; 8192];
    let mut seq = 0u64;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                seq += 1;
                emit_output(&ctx, &agent_id, &io, &buf[..n], seq).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexmesh_proto::cli_descriptor::CliDescriptor;

    fn claude_like() -> nexmesh_proto::cli_descriptor::ModeSpec {
        nexmesh_proto::cli_descriptor::ModeSpec {
            resume_args: vec!["--resume".into(), "{resume}".into()],
            session_id_args: vec!["--session-id".into(), "{session_uuid}".into()],
            ..Default::default()
        }
    }

    /// A RESUME THAT CANNOT BE HONOURED STARTS A NEW CONVERSATION.
    ///
    /// Never nothing. The seat has to come up, and come up with a
    /// conversation of its own so the next restart has something to
    /// continue -- otherwise a stale handle leaves a seat that cannot be
    /// opened and an operator with nothing to do about it.
    #[test]
    fn a_resume_that_cannot_be_honoured_names_a_new_conversation() {
        let m = claude_like();

        // The live failure: a handle for a conversation that was never
        // written. Checked, absent -- so a NEW one is named.
        assert_eq!(
            resume_plan(&m, &["never-written".into()], |_| Some(false), false),
            ResumePlan::NameFresh
        );

        // The seat has moved machines, so the conversation is not here.
        // Same answer: come up, with a new conversation.
        assert_eq!(resume_plan(&m, &["on-another-machine".into()], |_| Some(false), true),
                   ResumePlan::NameFresh);

        // A handle whose conversation IS there is honoured.
        assert_eq!(
            resume_plan(&m, &["real".into()], |_| Some(true), false),
            ResumePlan::Resume("real".into())
        );

        // No handle at all: a new named conversation, as before.
        assert_eq!(resume_plan(&m, &[], |_| None, false), ResumePlan::NameFresh);

        // Unverifiable (the CLI does not say where conversations live): the
        // handle is still tried, because refusing one on a guess would
        // throw away every resume for those CLIs.
        assert_eq!(
            resume_plan(&m, &["unknown".into()], |_| None, false),
            ResumePlan::Resume("unknown".into())
        );
    }

    #[test]
    fn a_cli_that_cannot_be_told_to_resume_is_not_left_with_nothing() {
        // Holding a handle for a CLI with no `resume_args` used to skip the
        // naming branch, so the seat got no session argument at all -- it
        // could neither continue anything nor be continued next time.
        let m = nexmesh_proto::cli_descriptor::ModeSpec {
            session_id_args: vec!["--session-id".into(), "{session_uuid}".into()],
            ..Default::default()
        };
        assert_eq!(resume_plan(&m, &["ignored".into()], |_| None, false), ResumePlan::NameFresh);
    }

    #[test]
    fn a_cli_that_names_nothing_continues_its_last_only_from_its_own_store() {
        // codex and opencode: no naming, so "the last one" is the only
        // option -- and only trustworthy inside a store of this seat's own.
        let m = nexmesh_proto::cli_descriptor::ModeSpec {
            resume_latest_args: vec!["resume".into(), "--last".into()],
            ..Default::default()
        };
        assert_eq!(resume_plan(&m, &[], |_| None, true), ResumePlan::ContinueLatest);
        // Nothing in its store yet: say nothing rather than continue
        // whatever another seat finished most recently.
        assert_eq!(resume_plan(&m, &[], |_| None, false), ResumePlan::Nothing);
    }

    /// A GHOST IN FRONT OF A REAL CONVERSATION.
    ///
    /// This is the one that cost real work. The hub used to send a single
    /// handle; if it was a ghost the node declined it and started blank --
    /// even though another of the seat's recorded conversations was sitting
    /// on disk the whole time. Three seats on the live fleet, with 130MB,
    /// 61MB and 4.9MB of conversation between them, and it was caught by
    /// hand in pre-flight rather than by anything in the code.
    #[test]
    fn a_ghost_does_not_hide_a_real_conversation_behind_it() {
        let m = claude_like();
        let plan = resume_plan(
            &m,
            &["ghost".into(), "also-ghost".into(), "real".into()],
            |h| Some(h == "real"),
            false,
        );
        assert_eq!(plan, ResumePlan::Resume("real".into()));
    }

    /// And when every candidate is a ghost, it starts a NEW one rather than
    /// dying on the last of them.
    #[test]
    fn all_ghosts_means_a_fresh_conversation() {
        let m = claude_like();
        let plan =
            resume_plan(&m, &["ghost".into(), "also-ghost".into()], |_| Some(false), false);
        assert_eq!(plan, ResumePlan::NameFresh);
    }

    /// Order is preference: the first candidate that is really there wins,
    /// not the best-looking one further down. The hub offers them in the
    /// order it would have chosen anyway -- this machine's binding first.
    #[test]
    fn the_first_one_that_exists_wins() {
        let m = claude_like();
        let plan = resume_plan(
            &m,
            &["mine".into(), "also-real".into()],
            |_| Some(true),
            false,
        );
        assert_eq!(plan, ResumePlan::Resume("mine".into()));
    }

    /// A CLI that does not say where conversations live cannot be checked,
    /// so the first candidate is taken as before. Unknown is not refused.
    #[test]
    fn uncheckable_takes_the_first_candidate() {
        let m = claude_like();
        let plan = resume_plan(&m, &["a".into(), "b".into()], |_| None, false);
        assert_eq!(plan, ResumePlan::Resume("a".into()));
    }

    /// A handle is only worth passing if the conversation is really there.
    ///
    /// The live failure this prevents: nexmesh named a conversation with
    /// `--session-id 7a8a2910-...`, the start then failed before claude
    /// persisted anything, the hub recorded the handle anyway -- it is
    /// recorded at spawn, before the conversation exists -- and every later
    /// spawn ran `--resume 7a8a2910-...`, which claude answers with "No
    /// conversation found with session ID" and exits at once. A seat that
    /// cannot start, with an error nobody would trace back to nexmesh
    /// having named it. That id existed in the hub and in no project
    /// directory anywhere on the machine.
    #[test]
    fn a_conversation_that_was_never_written_is_not_resumed() {
        let dir = std::env::temp_dir().join(format!("nexmesh-conv-{}", std::process::id()));
        let cwd = "/home/someone/work";
        let slug = cwd.replace('/', "-");
        std::fs::create_dir_all(dir.join(&slug)).unwrap();
        let tpl = format!("{}/{{cwd_slug}}/{{id}}.jsonl", dir.display());
        let mode = nexmesh_proto::cli_descriptor::ModeSpec {
            session_path: Some(tpl),
            ..Default::default()
        };

        // Nothing on disk: refused, so the seat starts fresh instead of
        // dying on a resume that cannot work.
        assert_eq!(conversation_exists(&mode, "never-written", cwd), Some(false));

        // Written: resumable.
        std::fs::write(dir.join(&slug).join("real-one.jsonl"), b"{}").unwrap();
        assert_eq!(conversation_exists(&mode, "real-one", cwd), Some(true));

        // THE OTHER HALF of the same problem: `--resume` only finds a
        // conversation in the project directory it is launched in, so a
        // real conversation belonging to another directory is still not
        // resumable here -- which is why the check is per cwd.
        assert_eq!(
            conversation_exists(&mode, "real-one", "/home/someone/elsewhere"),
            Some(false)
        );

        // A CLI that does not say where conversations live cannot be
        // checked, and nothing is refused on a guess.
        let silent = nexmesh_proto::cli_descriptor::ModeSpec::default();
        assert_eq!(conversation_exists(&silent, "anything", cwd), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hub used to fill an empty cwd from its OWN `HOME`, naming a
    /// directory on the hub's machine owned by the hub's user. Every
    /// workspace-less spawn then failed on the node with a bare
    /// "Permission denied". The node resolves it now, because the node is
    /// the only component that knows where its user may write.
    #[test]
    fn an_empty_cwd_resolves_here_and_a_given_one_is_left_alone() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert_eq!(resolve_cwd(""), home);
        assert_eq!(resolve_cwd("   "), home, "whitespace is not a path");
        assert_eq!(
            resolve_cwd("/srv/project"),
            "/srv/project",
            "a workspace mount the hub did resolve must be used as given"
        );
    }

    #[test]
    fn a_prompt_reaches_codex_as_toml_and_opencode_as_a_file() {
        let text = "say \"hi\"\nthen C:\\path";
        let toml_arg = fill_prompt("developer_instructions={prompt_toml}", text, "", "");
        let v: toml::Value = toml::from_str(&toml_arg).expect("codex's -c value parses as TOML");
        assert_eq!(v["developer_instructions"].as_str(), Some(text));
        let env = fill_prompt(r#"{"instructions": ["{prompt_file}"]}"#, text, "/s/AGENTS.md", "/s");
        let j: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert_eq!(j["instructions"][0], "/s/AGENTS.md");
        assert_eq!(fill_prompt("{prompt_dir}", text, "/s/AGENTS.md", "/s"), "/s");
    }

    fn spec(cwd: &str) -> SpawnSpec {
        SpawnSpec {
            authority: None,
            slot_id: "sl-1".into(),
            agent_id: "ag-1".into(),
            session_id: "se-1".into(),
            cli_id: "claude".into(),
            mode: RunMode::Headless,
            cwd: cwd.into(),
            role: None,
            display_name: None,
            prompt: "do the thing".into(),
            env: vec![],
            resume_handle: None,
            resume_candidates: vec![],
            system_prompt: None,
            system_prompt_mode: None,
            habits: None,
            tmux_target: None,
            model: None,
        }
    }

    fn desc(name: &str) -> CliDescriptor {
        let p = format!("{}/../../clis/{name}.toml", env!("CARGO_MANIFEST_DIR"));
        CliDescriptor::parse(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexmesh-spawn-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn inline_flag_writes_a_private_config_and_returns_its_path() {
        let dir = tmp("inline");
        let got = write_mcp_config(&desc("claude"), &spec(dir.to_str().unwrap()),
            "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap();
        let path = got.expect("claude takes an inline config path");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["nexmesh"]["command"], "/usr/bin/nexmesh-mcpd");
        assert_eq!(json["mcpServers"]["nexmesh"]["env"]["NEXMESH_AGENT_ID"], "ag-1");
        assert_eq!(json["mcpServers"]["nexmesh"]["env"]["NEXMESH_SOCK"], "/run/nexmesh/node.sock");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_orchestrator_seat_also_gets_the_fleet_server() {
        let dir = tmp("fleet");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mcpd = bin.join("nexmesh-mcpd");
        std::fs::write(&mcpd, "").unwrap();
        std::fs::write(bin.join("nexmesh-orch"), "").unwrap();
        let ident = dir.join("identity");
        std::fs::create_dir_all(&ident).unwrap();
        std::fs::write(ident.join("identity.json"), "{}").unwrap();
        std::env::set_var("NEXMESH_IDENTITY", &ident);

        let mut s = spec(dir.to_str().unwrap());
        s.role = Some("orchestrator".into());
        let path = write_mcp_config(&desc("claude"), &s, mcpd.to_str().unwrap(),
            "/run/nexmesh/node.sock", &dir).unwrap().expect("inline config path");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(json["mcpServers"]["nexmesh"]["command"], mcpd.to_str().unwrap(),
            "its own session server is still there");
        assert_eq!(json["mcpServers"]["nexmesh-fleet"]["command"],
            bin.join("nexmesh-orch").to_str().unwrap());
        assert_eq!(json["mcpServers"]["nexmesh-fleet"]["env"]["NEXMESH_IDENTITY"],
            ident.to_str().unwrap(), "the identity directory, never a token");
        assert!(json["mcpServers"]["nexmesh-fleet"]["env"].get("NEXMESH_OPERATOR_TOKEN").is_none(),
            "no secret is copied into a per-seat file");

        std::env::remove_var("NEXMESH_IDENTITY");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_ordinary_seat_gets_no_fleet_server() {
        let dir = tmp("nofleet");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mcpd = bin.join("nexmesh-mcpd");
        std::fs::write(&mcpd, "").unwrap();
        std::fs::write(bin.join("nexmesh-orch"), "").unwrap();

        let path = write_mcp_config(&desc("claude"), &spec(dir.to_str().unwrap()),
            mcpd.to_str().unwrap(), "/run/nexmesh/node.sock", &dir).unwrap().unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(json["mcpServers"].get("nexmesh-fleet").is_none(),
            "a seat with no orchestrator role never gets fleet reach, \
             even with the binary sitting right there");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_project_config_merge_keeps_the_users_own_settings() {
        let dir = tmp("merge");
        let cfg = dir.join("opencode.json");
        std::fs::write(&cfg, r#"{"theme":"nord","mcp":{"servers":{"mine":{"type":"local","command":["keepme"]}}}}"#).unwrap();
        let got = write_mcp_config(&desc("opencode"), &spec(dir.to_str().unwrap()),
            "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap();
        assert!(got.is_none(), "opencode is configured by file, not by flag");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(json["theme"], "nord", "unrelated settings must survive");
        assert_eq!(json["mcp"]["servers"]["mine"]["command"][0], "keepme",
            "the user's own MCP server must survive");
        assert_eq!(json["mcp"]["servers"]["nexmesh"]["command"][0], "/usr/bin/nexmesh-mcpd");
        assert_eq!(json["mcp"]["servers"]["nexmesh"]["environment"]["NEXMESH_AGENT_ID"], "ag-1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_existing_config_is_refused_rather_than_clobbered() {
        // Silently replacing a config we failed to parse would destroy the
        // user's settings. Fail the spawn instead.
        let dir = tmp("corrupt");
        let cfg = dir.join("opencode.json");
        std::fs::write(&cfg, "{ this is not json").unwrap();
        let err = write_mcp_config(&desc("opencode"), &spec(dir.to_str().unwrap()),
            "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"), "{err}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ this is not json");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_existing_config_is_treated_as_fresh() {
        let dir = tmp("empty");
        let cfg = dir.join("opencode.json");
        std::fs::write(&cfg, "   \n").unwrap();
        write_mcp_config(&desc("opencode"), &spec(dir.to_str().unwrap()),
            "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(json["mcp"]["servers"]["nexmesh"].is_object());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_project_config_in_a_subdirectory_gets_that_subdirectory_made() {
        // freebuff reads `.agents/mcp.json` and cursor reads
        // `.cursor/mcp.json`, and neither directory exists in a fresh
        // workspace. Writing the file without making the directory first
        // leaves the seat with no team tools and nothing in the log beyond
        // one failed write.
        for (cli, rel) in [("freebuff", ".agents/mcp.json"), ("cursor", ".cursor/mcp.json")] {
            let dir = tmp(cli);
            let got = write_mcp_config(&desc(cli), &spec(dir.to_str().unwrap()),
                "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap();
            assert!(got.is_none(), "{cli} is configured by file, not by flag");
            let cfg = dir.join(rel);
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap())
                    .unwrap_or_else(|e| panic!("{cli}: {} -- {e}", cfg.display()));
            let entry = &json["mcpServers"]["nexmesh"];
            assert_eq!(entry["command"], "/usr/bin/nexmesh-mcpd", "{cli}");
            assert_eq!(entry["env"]["NEXMESH_AGENT_ID"], "ag-1", "{cli}");
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn cursor_gets_the_stdio_type_its_loader_insists_on() {
        let dir = tmp("cursor-type");
        write_mcp_config(&desc("cursor"), &spec(dir.to_str().unwrap()),
            "/usr/bin/nexmesh-mcpd", "/run/nexmesh/node.sock", &dir).unwrap();
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".cursor/mcp.json")).unwrap()).unwrap();
        assert_eq!(json["mcpServers"]["nexmesh"]["type"], "stdio");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod push_tests {
    use super::push_capable;
    use nexmesh_proto::cli_descriptor::DriverSpec;

    #[test]
    fn every_real_driver_is_push_capable() {
        // A driver missing from this list is never pushed to, and its messages
        // sit unacked while the agent looks idle -- a silent failure.
        assert!(push_capable(&DriverSpec::StreamJson { replay_user_messages: true }));
        assert!(push_capable(&DriverSpec::AppServer));
        assert!(push_capable(&DriverSpec::HttpServer {
            basic_auth_user: None, password_env: None, attach_args: vec![]
        }));
        assert!(push_capable(&DriverSpec::Plugin { command: "x".into(), args: vec![] }));
    }

    #[test]
    fn an_interactive_seat_is_pushed_to_through_its_terminal() {
        // An interactive seat used to be left to the MCP floor entirely, so a
        // task sent to one waited until the agent happened to call a tool --
        // minutes, or for a seat sitting at its prompt, never.
        use nexmesh_proto::RunMode;
        let driver = DriverSpec::StreamJson { replay_user_messages: true };
        assert!(super::pushable(&driver, RunMode::Headless, false));
        assert!(super::pushable(&driver, RunMode::Interactive, true));
        // No terminal and not headless is nothing to write to.
        assert!(!super::pushable(&driver, RunMode::Interactive, false));
    }

    #[test]
    fn an_adopted_terminal_is_never_typed_at() {
        // THE safety property of the pty route. An adopted tmux pane has a
        // real pty, and what is sitting in it may be a shell prompt, where a
        // teammate's message plus the Enter after it would be EXECUTED.
        // Adoption is `mcp_only` precisely because nothing vouches for what
        // is in the pane, so no amount of terminal makes it typeable.
        use nexmesh_proto::RunMode;
        assert!(!super::pushable(&DriverSpec::McpOnly, RunMode::Interactive, true));
        assert!(!super::pushable(&DriverSpec::McpOnly, RunMode::Headless, true));
    }

    #[test]
    fn the_mcp_floor_is_not_pushed_to() {
        // It has no input channel; delivery rides its next tool result.
        assert!(!push_capable(&DriverSpec::McpOnly));
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use nexmesh_proto::cli_descriptor::TrustPath;

    /// A directory the CLI will not start in unless it is told to trust it.
    ///
    /// claude asks a modal question nothing can answer, so the seat sits
    /// there and looks like a CLI that refuses to run -- which is how it was
    /// reported from the fleet.
    #[test]
    fn a_directory_is_recorded_as_trusted_and_only_once() {
        let dir = std::env::temp_dir().join(format!("nexmesh-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("cli.json");
        std::fs::write(&cfg, br#"{"projects":{"/other":{"hasTrustDialogAccepted":true}}}"#)
            .unwrap();
        let t = TrustPath {
            file: cfg.to_string_lossy().to_string(),
            pointer: "/projects/{cwd}/hasTrustDialogAccepted".into(),
        };

        // First time: it changes something and says which file.
        let said = ensure_trusted(&t, "/home/someone/work");
        assert_eq!(said.as_deref(), Some(cfg.to_string_lossy().as_ref()));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["projects"]["/home/someone/work"]["hasTrustDialogAccepted"],
            serde_json::json!(true)
        );
        // What was already there is untouched: this is somebody's live config.
        assert_eq!(
            v["projects"]["/other"]["hasTrustDialogAccepted"],
            serde_json::json!(true)
        );

        // Second time: nothing to change, so nothing is announced -- the
        // operator should not be told about it on every single spawn.
        assert_eq!(ensure_trusted(&t, "/home/someone/work"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_config_that_is_not_there_is_left_alone() {
        // Nothing is created from nothing: a CLI that has never run has no
        // config, and inventing one is not this function's business.
        let t = TrustPath {
            file: "/nonexistent/nexmesh-trust-test.json".into(),
            pointer: "/projects/{cwd}/hasTrustDialogAccepted".into(),
        };
        assert_eq!(ensure_trusted(&t, "/tmp"), None);
    }
}

#[cfg(test)]
mod handle_report_tests {
    use super::*;

    /// A HANDLE MUST NOT BE RECORDED BEFORE ITS CONVERSATION EXISTS.
    ///
    /// Reporting a freshly named conversation OVERWRITES whatever the seat
    /// had recorded before. Done at spawn time, a CLI that exits at once
    /// leaves the seat pointing at a conversation that was never created and
    /// the real one gone from every store the hub has. On the live fleet a
    /// crash loop did exactly that twice in six seconds, and the seat came
    /// back with no history.
    ///
    /// The existence check used when RESUMING stops a seat dying on such a
    /// ghost. It does nothing about the ghost displacing a good handle,
    /// which is what this guards.
    #[test]
    fn a_conversation_that_was_never_written_is_not_there_to_report() {
        let dir = std::env::temp_dir().join(format!("nexmesh-rep-{}", std::process::id()));
        let proj = dir.join("projects").join("-tmp-work");
        std::fs::create_dir_all(&proj).unwrap();
        let tpl = format!("{}/projects/{{cwd_slug}}/{{id}}.jsonl", dir.to_string_lossy());

        // The CLI was told to use this one and died before writing it.
        assert!(!conversation_exists_at(&tpl, "never-written", "/tmp/work"));

        // And one it did write is reportable.
        std::fs::write(proj.join("real.jsonl"), b"{}").unwrap();
        assert!(conversation_exists_at(&tpl, "real", "/tmp/work"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cwd becomes the project directory name by turning every slash
    /// into a dash, the leading one included. Getting this wrong would make
    /// every conversation look like a ghost, which would be worse than the
    /// bug: nothing would ever be recorded.
    #[test]
    fn the_project_directory_is_named_from_the_cwd() {
        let dir = std::env::temp_dir().join(format!("nexmesh-slug-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-someone-deep-work");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("c.jsonl"), b"{}").unwrap();
        let tpl = format!("{}/projects/{{cwd_slug}}/{{id}}.jsonl", dir.to_string_lossy());
        assert!(conversation_exists_at(&tpl, "c", "/home/someone/deep/work"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
