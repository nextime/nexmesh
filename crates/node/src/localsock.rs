//! The unix socket mcpd connects to.
//!
//! Each agent's shim registers, then relays tool calls. The node forwards them
//! to the hub and routes the reply back. mcpd never holds a credential and
//! never speaks to the hub directly.

use crate::ctx::{JoinOutcome, NodeCtx};
use anyhow::Result;
use nexmesh_proto::{
    self as proto,
    localsock::{McpdToNode, NodeToMcpd},
};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

/// How long an agent's tool call may wait on the hub before failing. Long
/// enough to ride out a reconnect, short enough that the model is not left
/// hanging forever.
const TOOL_TIMEOUT: Duration = Duration::from_secs(90);

pub async fn serve(ctx: Arc<NodeCtx>) -> Result<()> {
    let path = ctx.sock_path.clone();
    if let Some(dir) = Path::new(&path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    // A leftover socket from a crashed node would make bind() fail.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Only this user may talk to the node: anyone who can would be able to act
    // as any agent on this machine.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    tracing::info!(socket = %path, "listening for agent shims");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(ctx, stream).await {
                        tracing::debug!(error = %e, "shim connection ended");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
            }
        }
    }
}

async fn connection(ctx: Arc<NodeCtx>, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut agent_id: Option<String> = None;

    async fn reply(w: &mut tokio::net::unix::OwnedWriteHalf, frame: &NodeToMcpd) -> Result<()> {
        let mut s = serde_json::to_string(frame)?;
        s.push('\n');
        w.write_all(s.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame: McpdToNode = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(e) => {
                reply(&mut write, &NodeToMcpd::Error { req_id: None, message: format!("bad frame: {e}") }).await?;
                continue;
            }
        };

        match frame {
            McpdToNode::Register { cli_id, pid, cwd, agent_hint, slot_hint, session_hint, role, name } => {
                match register(&ctx, cli_id, pid, cwd, agent_hint, slot_hint, session_hint, role, name).await {
                    Ok(frame) => {
                        if let NodeToMcpd::Registered { agent_id: ref a, .. } = frame {
                            agent_id = Some(a.clone());
                        }
                        reply(&mut write, &frame).await?;
                    }
                    Err(e) => {
                        reply(&mut write, &NodeToMcpd::Error { req_id: None, message: e.to_string() }).await?
                    }
                }
            }

            McpdToNode::Tool { req_id, tool, args } => {
                let Some(aid) = agent_id.clone() else {
                    reply(&mut write, &NodeToMcpd::Error {
                        req_id: Some(req_id),
                        message: "register before calling tools".into(),
                    }).await?;
                    continue;
                };
                let call_id = format!("{aid}-{}", ctx.next_seq());
                let rx = ctx.register_tool_waiter(call_id.clone());
                if let Err(e) = ctx.send_hub(&proto::NodeToHub::ToolCall {
                    agent_id: aid.clone(),
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    args,
                }).await {
                    ctx.drop_tool_waiter(&call_id);
                    reply(&mut write, &NodeToMcpd::Error {
                        req_id: Some(req_id),
                        message: format!("hub unreachable: {e}"),
                    }).await?;
                    continue;
                }
                match tokio::time::timeout(TOOL_TIMEOUT, rx).await {
                    Ok(Ok(outcome)) => {
                        reply(&mut write, &NodeToMcpd::ToolReply {
                            req_id,
                            result: outcome.result,
                            deliver: outcome.deliver,
                        }).await?
                    }
                    Ok(Err(_)) => {
                        reply(&mut write, &NodeToMcpd::Error {
                            req_id: Some(req_id),
                            message: "hub connection dropped while the call was in flight".into(),
                        }).await?
                    }
                    Err(_) => {
                        ctx.drop_tool_waiter(&call_id);
                        reply(&mut write, &NodeToMcpd::Error {
                            req_id: Some(req_id),
                            message: format!("tool '{tool}' timed out after {}s", TOOL_TIMEOUT.as_secs()),
                        }).await?
                    }
                }
            }

            McpdToNode::Ack { message_ids } => {
                if let Some(aid) = &agent_id {
                    // The shim has written the messages back to its CLI, so
                    // they have reached the model. Only now are they delivered.
                    let _ = ctx.send_hub(&proto::NodeToHub::MessageAcked {
                        agent_id: aid.clone(),
                        message_ids,
                    }).await;
                }
            }

            McpdToNode::Bye => break,
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn register(
    ctx: &Arc<NodeCtx>,
    cli_id: String,
    pid: i32,
    cwd: String,
    agent_hint: Option<String>,
    _slot_hint: Option<String>,
    session_hint: Option<String>,
    role: Option<String>,
    name: Option<String>,
) -> Result<NodeToMcpd> {
    let tools = proto::tools::team_tools();

    // A managed agent was told its ids at spawn time.
    if let Some(hint) = agent_hint {
        if let Some(rec) = ctx.agent(&hint) {
            tracing::info!(agent = %hint, slot = %rec.slot_id, "managed shim registered");
            return Ok(NodeToMcpd::Registered {
                agent_id: rec.agent_id,
                slot_id: rec.slot_id,
                session_id: rec.session_id,
                display_name: name,
                role,
                tools,
            });
        }
        // The node restarted and lost the agent. Do NOT silently create a
        // second slot for it: that would duplicate a team member.
        anyhow::bail!(
            "agent '{hint}' is not known to this node (it may have restarted); \
             restart the agent so it can be re-placed"
        );
    }

    // Nobody spawned this: a human ran the CLI by hand. Ask the hub for a seat.
    let req_id = ctx.next_seq();
    let rx = ctx.register_join_waiter(req_id);
    let start_ticks = crate::proc::start_ticks(pid).unwrap_or(0);
    ctx.send_hub(&proto::NodeToHub::JoinRequest {
        req_id,
        session_hint,
        cli_id: Some(cli_id),
        role: role.clone(),
        name: name.clone(),
        cwd: Some(cwd),
        proc: Some(proto::ProcIdentity {
            node_id: ctx.node_name.clone(),
            pid,
            start_ticks,
        }),
    })
    .await?;

    let outcome: JoinOutcome = tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .map_err(|_| anyhow::anyhow!("the hub did not answer the join request"))?
        .map_err(|_| anyhow::anyhow!("hub connection dropped during join"))?;

    match outcome.granted {
        Ok((slot_id, session_id, agent_id, display_name, role)) => {
            ctx.agents.lock().unwrap().insert(
                agent_id.clone(),
                crate::ctx::AgentRec {
                    agent_id: agent_id.clone(),
                    slot_id: slot_id.clone(),
                    session_id: session_id.clone(),
                    cli_id: String::new(),
                    proc: proto::ProcIdentity {
                        node_id: ctx.node_name.clone(),
                        pid,
                        start_ticks,
                    },
                },
            );
            tracing::info!(%agent_id, %slot_id, "attached agent joined the team");
            Ok(NodeToMcpd::Registered { agent_id, slot_id, session_id, display_name, role, tools })
        }
        Err(reason) => anyhow::bail!("the hub refused the join: {reason}"),
    }
}
