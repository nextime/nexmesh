//! Running a driver plugin.
//!
//! The protocol is in `nexmesh_proto::plugin`, along with the reasoning for
//! its shape. This module is the node's half: spawn the helper, relay the
//! CLI's output to it, and carry out the four things it can ask for.
//!
//! The division of labour is the point. The node keeps the CLI process -- its
//! pid, its lifecycle, the right to signal it -- and the plugin only
//! translates. So a plugin is a parser, not a privilege: the worst a bad one
//! can do to the fleet is fail to deliver messages, which the hub notices and
//! recovers from through the MCP floor.

use crate::ctx::{AgentIo, Injected, NodeCtx};
use nexmesh_proto::{
    self as proto,
    plugin::{FromPlugin, ToPlugin, PLUGIN_PROTOCOL_VERSION},
    Envelope,
};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Everything `run` needs, because this many positional arguments is how a
/// call site becomes a bug.
pub struct PluginRun {
    pub agent_id: String,
    pub slot_id: String,
    pub cli_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub prompt: String,
    pub resume: Option<String>,
    pub model: Option<String>,
}

/// Drive one agent through a plugin.
///
/// `cli_stdin` writes to the agent CLI; `cli_stdout` is its output. Returns
/// when the plugin exits, which is also when injection starts failing and the
/// hub falls back to delivering through MCP.
pub async fn run<R>(
    ctx: Arc<NodeCtx>,
    spec: PluginRun,
    io: Arc<AgentIo>,
    cli_stdin: mpsc::Sender<Vec<u8>>,
    cli_stdout: R,
    mut inject_rx: mpsc::Receiver<Vec<Envelope>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let PluginRun { agent_id, slot_id, cli_id, command, args, cwd, prompt, resume, model } =
        spec;

    let mut child = match tokio::process::Command::new(&command)
        .args(&args)
        .current_dir(if std::path::Path::new(&cwd).is_dir() { &cwd } else { "." })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // The agent is already running; it just has no fast path. Say so
            // loudly, because the symptom otherwise is messages arriving late
            // for no visible reason.
            tracing::error!(agent = %agent_id, plugin = %command, error = %e,
                "could not start the driver plugin; this agent falls back to MCP delivery");
            return;
        }
    };
    tracing::info!(agent = %agent_id, plugin = %command, pid = child.id(),
        "driver plugin started");

    let (pin_tx, mut pin_rx) = mpsc::channel::<String>(256);
    if let Some(mut pstdin) = child.stdin.take() {
        tokio::spawn(async move {
            while let Some(line) = pin_rx.recv().await {
                if pstdin.write_all(line.as_bytes()).await.is_err()
                    || pstdin.flush().await.is_err()
                {
                    break;
                }
            }
        });
    }

    // The plugin is told the task before anything else, so it can decide how
    // this CLI wants its first prompt.
    let hello = ToPlugin::Hello {
        protocol: PLUGIN_PROTOCOL_VERSION,
        agent_id: agent_id.clone(),
        slot_id: slot_id.clone(),
        cli_id,
        cwd: cwd.clone(),
        prompt,
        resume,
        model,
    };
    if pin_tx.send(hello.line()).await.is_err() {
        tracing::error!(agent = %agent_id, "the driver plugin closed its input immediately");
        return;
    }

    // The CLI's output: to the transcript, and to the plugin to interpret.
    {
        let ctx = ctx.clone();
        let agent_id = agent_id.clone();
        let io = io.clone();
        let pin_tx = pin_tx.clone();
        tokio::spawn(async move {
            let mut reader = cli_stdout;
            let mut buf = vec![0u8; 8192];
            let mut seq = 0u64;
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        seq += 1;
                        crate::spawn::emit_output(&ctx, &agent_id, &io, &buf[..n], seq).await;
                        // Output is bytes and the protocol is JSON text, so
                        // anything not valid UTF-8 is replaced rather than
                        // dropped: a plugin reading a partly binary stream
                        // still sees where its markers are.
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        if pin_tx.send(ToPlugin::Stdout { data }.line()).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = pin_tx.send(ToPlugin::Exit { code: None }.line()).await;
        });
    }

    // Teammate messages, handed over whole.
    {
        let pin_tx = pin_tx.clone();
        let io = io.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            while let Some(messages) = inject_rx.recv().await {
                let text = crate::driver::render_injection(&messages);
                // Recorded as pending before it is handed over, so a plugin
                // that dies mid-delivery releases them for redelivery instead
                // of losing them.
                io.push_pending(Injected {
                    text: text.clone(),
                    message_ids: messages.iter().map(|m| m.id).collect(),
                });
                if pin_tx.send(ToPlugin::Deliver { text, messages }.line()).await.is_err() {
                    tracing::warn!(agent = %agent_id, "the driver plugin is gone; \
                        messages go back to the queue for MCP delivery");
                    break;
                }
            }
        });
    }

    // A plugin's stderr is its log, and it lands in this node's log where
    // someone debugging a spawn will actually look.
    if let Some(err) = child.stderr.take() {
        let agent_id = agent_id.clone();
        let command = command.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(agent = %agent_id, plugin = %command, "plugin: {line}");
            }
        });
    }

    let Some(pstdout) = child.stdout.take() else {
        tracing::error!(agent = %agent_id, "the driver plugin gave us no stdout");
        return;
    };
    let mut lines = BufReader::new(pstdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let frame: FromPlugin = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(e) => {
                // Not fatal: a plugin that prints a stray line should lose
                // that line, not the agent. But say exactly what was wrong,
                // since this is the commonest mistake when writing one.
                tracing::warn!(agent = %agent_id, plugin = %command, error = %e,
                    "ignoring an unparseable plugin frame: {}",
                    line.chars().take(200).collect::<String>());
                continue;
            }
        };
        match frame {
            FromPlugin::Stdin { data } => {
                if cli_stdin.send(data.into_bytes()).await.is_err() {
                    tracing::warn!(agent = %agent_id, "the agent's input channel is closed");
                    break;
                }
            }
            FromPlugin::Ack { message_ids } => {
                let ids = io.ack_ids(&message_ids);
                if !ids.is_empty() {
                    tracing::info!(agent = %agent_id, acked = ids.len(),
                        "plugin confirmed delivery");
                    let _ = ctx
                        .send_hub(&proto::NodeToHub::MessageAcked {
                            agent_id: agent_id.clone(),
                            message_ids: ids,
                        })
                        .await;
                }
            }
            FromPlugin::Tokens { input, output, cache_read } => {
                let _ = ctx
                    .send_hub(&proto::NodeToHub::TokenUsage {
                        agent_id: agent_id.clone(),
                        input,
                        output,
                        cache_read,
                    })
                    .await;
            }
            FromPlugin::Resume { handle } => {
                tracing::info!(agent = %agent_id, %handle, "plugin reported a resume handle");
                let _ = ctx
                    .send_hub(&proto::NodeToHub::ResumeHandle {
                        agent_id: agent_id.clone(),
                        slot_id: slot_id.clone(),
                        handle,
                    })
                    .await;
            }
            FromPlugin::Model { model } => {
                tracing::info!(agent = %agent_id, %model, "plugin reported the model");
                let _ = ctx
                    .send_hub(&proto::NodeToHub::AgentModel {
                        agent_id: agent_id.clone(),
                        model,
                    })
                    .await;
            }
            FromPlugin::Log { level, text } => match level.as_str() {
                "error" => tracing::error!(agent = %agent_id, plugin = %command, "{text}"),
                "warn" => tracing::warn!(agent = %agent_id, plugin = %command, "{text}"),
                "debug" => tracing::debug!(agent = %agent_id, plugin = %command, "{text}"),
                _ => tracing::info!(agent = %agent_id, plugin = %command, "{text}"),
            },
        }
    }

    let code = child.wait().await.ok().and_then(|s| s.code());
    // Whatever it never acked goes back, or those messages are never seen
    // again by anyone.
    let lost = io.drain_pending();
    if !lost.is_empty() {
        tracing::warn!(agent = %agent_id, count = lost.len(),
            "the plugin exited holding undelivered messages; releasing them");
        let _ = ctx
            .send_hub(&proto::NodeToHub::InjectionsLost {
                agent_id: agent_id.clone(),
                message_ids: lost,
            })
            .await;
    }
    tracing::warn!(agent = %agent_id, plugin = %command, ?code,
        "the driver plugin exited; this agent now relies on MCP delivery");
}
