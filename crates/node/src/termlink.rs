//! Earning a keyboard for a seat that has none.
//!
//! An agent a human started by hand has exactly one delivery route: its own
//! next tool call. Sitting at its prompt, it may not make one for hours, so a
//! task handed to it simply waits. If its terminal happens to be a tmux pane
//! this node has adopted, there IS a keyboard -- and this module is what
//! establishes, carefully, that it may be used.
//!
//! Two facts are needed, neither sufficient alone:
//!
//!   - **a shim registered over this node's socket**, which proves an agent
//!     CLI exists and tells us its process;
//!   - **that process is inside the pane** the node's own tmux view is
//!     looking at, which proves which terminal it is sitting in.
//!
//! Without the second, typing could reach a shell prompt, where a teammate's
//! message plus the Enter after it would be executed. Without the first, a
//! pane is just a pane. `clis/tmux.toml` has the longer version of why this
//! is not `send-keys`.
//!
//! Both orders have to work, because both happen: adopting a pane that
//! already holds a registered CLI, and a CLI registering from inside a pane
//! that was adopted earlier. The first is what an operator actually does.
//!
//! What is reported is the CLI's identity, never the shim's. The shim is the
//! CLI's child, so recording the parent means the link outlives a shim that
//! has died or lost its connection -- which is the case this exists for.

use crate::ctx::NodeCtx;
use nexmesh_proto as proto;
use std::sync::Arc;

/// A CLI's shim has just registered. Is it sitting in a terminal of ours?
pub async fn on_shim_registered(ctx: &Arc<NodeCtx>, agent_id: &str, slot_id: &str, shim_pid: i32) {
    let Some(cli_pid) = cli_of(shim_pid) else { return };
    let views: Vec<(String, String)> = adopted_views(ctx);
    for (via, view) in views {
        if pane_holds(&view, cli_pid) {
            report(ctx, agent_id, slot_id, &via, cli_pid).await;
            return;
        }
    }
}

/// A terminal has just been adopted. Does it already hold registered CLIs?
///
/// tmux needs a moment to have a client and a current pane to answer about,
/// so this is tried a few times before giving up. It costs nothing when there
/// is nothing in the pane.
pub async fn on_terminal_adopted(ctx: &Arc<NodeCtx>, via_agent_id: &str) {
    let Some(view) = ctx.agents.lock().unwrap().get(via_agent_id).and_then(|r| r.tmux_view.clone())
    else {
        return;
    };
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let Some(pane) = crate::tmux::pane_pid(&view) else {
            tracing::debug!(%view, attempt, "tmux has no pane pid for this view yet");
            continue;
        };
        // Seats on this node that nothing drives, with the shim pid each
        // registered under.
        let candidates: Vec<(String, String, i32)> = ctx
            .agents
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.tmux_view.is_none() && r.proc.pid > 0)
            .map(|r| (r.agent_id.clone(), r.slot_id.clone(), r.proc.pid))
            .collect();
        tracing::debug!(%view, pane, candidates = candidates.len(),
                        "looking for registered CLIs in an adopted pane");
        let mut found = false;
        for (agent_id, slot_id, shim_pid) in candidates {
            let Some(cli_pid) = cli_of(shim_pid) else { continue };
            tracing::debug!(%agent_id, shim_pid, cli_pid, pane,
                            inside = crate::proc::is_descendant_of(cli_pid, pane),
                            "checking a seat against the pane");
            if crate::proc::is_descendant_of(cli_pid, pane) {
                report(ctx, &agent_id, &slot_id, via_agent_id, cli_pid).await;
                found = true;
            }
        }
        if found {
            return;
        }
    }
}

/// The CLI a shim belongs to: its parent. A shim is started BY the CLI as its
/// MCP server, so the parent is the program whose terminal we are after.
fn cli_of(shim_pid: i32) -> Option<i32> {
    crate::proc::ppid(shim_pid).ok().filter(|p| *p > 1)
}

fn adopted_views(ctx: &Arc<NodeCtx>) -> Vec<(String, String)> {
    ctx.agents
        .lock()
        .unwrap()
        .values()
        .filter_map(|r| r.tmux_view.clone().map(|v| (r.agent_id.clone(), v)))
        .collect()
}

fn pane_holds(view: &str, cli_pid: i32) -> bool {
    crate::tmux::pane_pid(view)
        .map(|pane| crate::proc::is_descendant_of(cli_pid, pane))
        .unwrap_or(false)
}

async fn report(ctx: &Arc<NodeCtx>, agent_id: &str, slot_id: &str, via: &str, cli_pid: i32) {
    let cli_ticks = crate::proc::start_ticks(cli_pid).unwrap_or(0);
    tracing::info!(
        %agent_id, %via, cli_pid,
        "this seat is sitting in an adopted terminal; it can be typed at"
    );
    let _ = ctx
        .send_hub(&proto::NodeToHub::TerminalLink {
            agent_id: agent_id.to_string(),
            slot_id: slot_id.to_string(),
            via_agent_id: via.to_string(),
            cli: proto::ProcIdentity {
                node_id: ctx.node_name.clone(),
                pid: cli_pid,
                start_ticks: cli_ticks,
            },
        })
        .await;
}
