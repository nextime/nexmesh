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
    cli_descriptor::{render_entry, set_pointer, CliDescriptor, McpRegistration},
    ProcIdentity, Provenance, RunMode, SpawnSpec,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// The environment an mcpd inherits so it can identify itself to the node.
fn shim_env(spec: &SpawnSpec, sock_path: &str) -> Vec<(String, String)> {
    vec![
        ("NEXMESH_AGENT_ID".into(), spec.agent_id.clone()),
        ("NEXMESH_SLOT".into(), spec.slot_id.clone()),
        ("NEXMESH_SESSION".into(), spec.session_id.clone()),
        ("NEXMESH_SOCK".into(), sock_path.to_string()),
        ("NEXMESH_CLI".into(), spec.cli_id.clone()),
    ]
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

pub async fn spawn_agent(ctx: Arc<NodeCtx>, spec: SpawnSpec) -> Result<()> {
    let d = ctx
        .descriptor(&spec.cli_id)
        .ok_or_else(|| anyhow::anyhow!("no descriptor for CLI '{}'", spec.cli_id))?
        .clone();
    let bin = crate::cli_detect::resolve(&d.cli.command, &d.cli.extra_paths)
        .ok_or_else(|| anyhow::anyhow!("'{}' is not installed on {}", d.cli.command, ctx.node_name))?;

    let mode_name = match spec.mode {
        RunMode::Headless => "headless",
        RunMode::Interactive => "interactive",
    };
    let mode = d
        .modes
        .get(mode_name)
        .ok_or_else(|| anyhow::anyhow!("'{}' has no {mode_name} mode", spec.cli_id))?
        .clone();

    let state_dir = PathBuf::from(&spec.cwd).join(".nexmesh");
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
    let mut args = d.render_args(mode_name, &vars);
    if spec.resume_handle.is_some() {
        for a in &mode.resume_args {
            let mut s = a.clone();
            for (k, v) in &vars {
                s = s.replace(&format!("{{{k}}}"), v);
            }
            args.push(s);
        }
    }

    std::fs::create_dir_all(&state_dir).ok();
    let env = shim_env(&spec, &ctx.sock_path);

    tracing::info!(cli = %spec.cli_id, agent = %spec.agent_id, mode = mode_name,
        cwd = %spec.cwd, "spawning: {} {}", bin, args.join(" "));

    // An interactive agent runs under a pty so it can be WATCHED AND TAKEN OVER
    // from any machine. A headless agent is a protocol on a pipe; there is no
    // terminal to take over, so it gets pipes and its attach is view-only.
    if matches!(spec.mode, RunMode::Interactive) {
        spawn_on_pty(ctx, spec, bin, args, env, mode.prompt_on_stdin).await
    } else {
        spawn_on_pipes(ctx, spec, bin, args, env, mode.prompt_on_stdin).await
    }
}

/// Everything both spawn paths do once the process exists: record the identity
/// triple, register it, and tell the hub.
async fn register_started(
    ctx: &Arc<NodeCtx>,
    spec: &SpawnSpec,
    pid: i32,
    io: Arc<crate::ctx::AgentIo>,
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
        },
    );
    ctx.io.lock().unwrap().insert(spec.agent_id.clone(), io);

    ctx.send_hub(&proto::NodeToHub::AgentUp {
        agent_id: spec.agent_id.clone(),
        slot_id: spec.slot_id.clone(),
        proc: identity.clone(),
        provenance: Provenance::Managed,
        cwd: spec.cwd.clone(),
        resume_handle: None,
    })
    .await?;
    Ok(identity)
}

async fn announce_exit(ctx: &Arc<NodeCtx>, agent_id: &str, code: Option<i32>) {
    tracing::info!(%agent_id, ?code, "agent exited");
    ctx.agents.lock().unwrap().remove(agent_id);
    // Collect the viewers BEFORE any await: holding a std MutexGuard across an
    // await makes the future non-Send, and tokio::spawn then refuses it.
    let attaches: Vec<String> = {
        let removed = ctx.io.lock().unwrap().remove(agent_id);
        removed.map(|io| io.attach_ids()).unwrap_or_default()
    };
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
    prompt_on_stdin: bool,
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
    for (k, v) in &env {
        cmd.env(k, v);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // A TUI needs to know what terminal it is talking to.
    if std::env::var("TERM").is_err() {
        cmd.env("TERM", "xterm-256color");
    }

    let mut child = pair.slave.spawn_command(cmd).with_context(|| format!("spawning {bin} on a pty"))?;
    // The slave fd must go, or the master never sees EOF when the child exits.
    drop(pair.slave);

    let pid = child
        .process_id()
        .ok_or_else(|| anyhow::anyhow!("child exited before reporting a pid"))? as i32;

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

    let io = crate::ctx::AgentIo::new(Some(in_tx.clone()), Some(pair.master));
    register_started(&ctx, &spec, pid, io.clone()).await?;

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
            while let Some(chunk) = out_rx.recv().await {
                seq += 1;
                emit_output(&ctx, &agent_id, &io, &chunk, seq).await;
            }
        });
    }

    {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        tokio::spawn(async move {
            let code = tokio::task::spawn_blocking(move || {
                child.wait().ok().map(|s| s.exit_code() as i32)
            })
            .await
            .ok()
            .flatten();
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
    prompt_on_stdin: bool,
) -> Result<()> {
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(&args)
        .current_dir(&spec.cwd)
        .kill_on_drop(false)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
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

    if prompt_on_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            // Claude's stream-json input wants one JSON user message per line.
            let line = serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": spec.prompt }] }
            });
            let _ = stdin.write_all(format!("{line}\n").as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }

    // No input channel: a pipe-run agent cannot be typed into, so its attach is
    // view-only rather than pretending otherwise.
    let io = crate::ctx::AgentIo::new(None, None);
    register_started(&ctx, &spec, pid, io.clone()).await?;

    if let Some(out) = child.stdout.take() {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let io = io.clone();
        tokio::spawn(async move { pump(ctx, agent_id, io, out).await });
    }
    // An agent CLI reports its failures on stderr. Piping it and never reading
    // it loses exactly the message needed to diagnose a spawn that dies, and
    // risks the child blocking once the pipe buffer fills.
    if let Some(err) = child.stderr.take() {
        let ctx = ctx.clone();
        let agent_id = spec.agent_id.clone();
        let io = io.clone();
        tokio::spawn(async move { pump(ctx, agent_id, io, err).await });
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
async fn emit_output(
    ctx: &Arc<NodeCtx>,
    agent_id: &str,
    io: &Arc<crate::ctx::AgentIo>,
    data: &[u8],
    seq: u64,
) {
    io.push_output(data);
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

    fn spec(cwd: &str) -> SpawnSpec {
        SpawnSpec {
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
}
