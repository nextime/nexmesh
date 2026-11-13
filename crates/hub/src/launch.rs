//! Placing a slot on a node and asking that node to start it.
//!
//! Shared by the operator endpoint and by crash recovery, so a respawn goes
//! through exactly the same path as a first launch -- including placement,
//! which means a slot whose original machine is gone can come back elsewhere.

use crate::{prompt, state::AppState};
use anyhow::{Context, Result};
use nexmesh_proto::{now_ms, HubToNode, Placement, RunMode, SpawnSpec};
use rusqlite::{params, OptionalExtension};
use serde_json::json;

#[allow(clippy::type_complexity)]
struct SlotRow {
    session_id: String,
    session_name: String,
    cli_id: Option<String>,
    mode: String,
    placement: String,
    display_name: Option<String>,
    role: Option<String>,
    role_desc: Option<String>,
    workspace_id: Option<String>,
    resume: Option<String>,
    tmux_target: Option<String>,
    model: Option<String>,
}

fn read_slot(st: &AppState, slot_id: &str) -> Result<SlotRow> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT s.session_id, se.name, s.cli_id, s.mode, s.placement, s.display_name,
                s.role, s.role_description, s.workspace_id, s.resume_handle, s.tmux_target,
                s.model
         FROM slots s JOIN sessions se ON se.id = s.session_id WHERE s.id = ?",
        [slot_id],
        |r| {
            Ok(SlotRow {
                session_id: r.get(0)?,
                session_name: r.get(1)?,
                cli_id: r.get(2)?,
                mode: r.get(3)?,
                placement: r.get(4)?,
                display_name: r.get(5)?,
                role: r.get(6)?,
                role_desc: r.get(7)?,
                workspace_id: r.get(8)?,
                resume: r.get(9)?,
                tmux_target: r.get(10)?,
                model: r.get(11)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("no slot '{slot_id}'"))
}

/// Launch (or relaunch) a slot. `handoff` is extra text prepended to the
/// prompt, used to tell a respawned agent that it is resuming.
pub async fn launch_slot(
    st: &AppState,
    slot_id: &str,
    handoff: Option<&str>,
) -> Result<(String, String)> {
    let row = read_slot(st, slot_id)?;
    let placement: Placement = serde_json::from_str(&row.placement).unwrap_or_default();
    let node_id = st.place(row.cli_id.as_deref(), &placement)?;

    let (node_name, cwd, shared) = {
        let db = st.db.lock().unwrap();
        let node_name: String =
            db.query_row("SELECT name FROM nodes WHERE id = ?", [&node_id], |r| r.get(0))?;
        let (cwd, shared) = match &row.workspace_id {
            Some(w) => {
                let path: Option<String> = db
                    .query_row(
                        "SELECT path FROM workspace_mounts WHERE workspace_id = ? AND node_id = ?",
                        params![w, node_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let path = path.with_context(|| {
                    format!(
                        "workspace has no mount on node '{node_name}': add one so the agent \
                         knows where to work on that machine"
                    )
                })?;
                let kind: String =
                    db.query_row("SELECT kind FROM workspaces WHERE id = ?", [w], |r| r.get(0))?;
                (path, kind == "repo")
            }
            None => (std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()), false),
        };
        (node_name, cwd, shared)
    };

    let agent_id = format!("ag-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let name = row.display_name.clone().unwrap_or_else(|| "agent".into());
    let role_s = row.role.clone().unwrap_or_else(|| "team member".into());
    let desc = row.role_desc.unwrap_or_default();
    let task = desc
        .split("TASK: ")
        .nth(1)
        .unwrap_or("Coordinate with your team.")
        .to_string();
    let brief = prompt::AgentBrief {
        display_name: &name,
        role: &role_s,
        role_description: desc.split("\n\nTASK: ").next().unwrap_or(""),
        session_name: &row.session_name,
        node_name: &node_name,
        workspace_path: &cwd,
        shared_workspace: shared,
        task: &task,
    };
    let mut prompt_text = prompt::first_prompt(&brief);
    if let Some(h) = handoff {
        prompt_text = format!("{h}\n\n{prompt_text}");
    }
    drop(brief);

    // Continuity across sessions. slots.resume_handle only ever covers THIS
    // session, so a seat that worked here yesterday would otherwise start
    // from nothing. The identity is the CLI, the workspace and the name --
    // what makes tomorrow's agent the same agent.
    let mut resume = row.resume.clone();
    let cli_for_identity = row.cli_id.clone().unwrap_or_default();
    let name_for_identity = row.display_name.clone().unwrap_or_default();
    if !cli_for_identity.is_empty() && !name_for_identity.is_empty() {
        let found = {
            let db = st.db.lock().unwrap();
            crate::memory::continuity(&db, &cli_for_identity, &cwd, &name_for_identity)
                .ok()
                .flatten()
        };
        if let Some((handle, context)) = found {
            if resume.is_none() {
                if let Some(h) = handle {
                    tracing::info!(slot = %slot_id, cli = %cli_for_identity,
                        "resuming a conversation from an earlier session");
                    resume = Some(h);
                }
            }
            // The summary goes in whether or not the handle did: a CLI that
            // cannot resume at all can still be told what it was doing, and a
            // handle is opaque so it cannot be read by anything else.
            if !context.trim().is_empty() {
                prompt_text = format!(
                    "[CONTINUING EARLIER WORK]\nWhen you last worked here you wrote: \
                     {}\n[END]\n\n{prompt_text}",
                    context.trim()
                );
            }
        }
    }

    let workspace_path = cwd.clone();
    let spec = SpawnSpec {
        slot_id: slot_id.to_string(),
        agent_id: agent_id.clone(),
        session_id: row.session_id,
        cli_id: row.cli_id.unwrap_or_default(),
        mode: if row.mode == "interactive" { RunMode::Interactive } else { RunMode::Headless },
        cwd,
        role: row.role,
        display_name: row.display_name,
        prompt: prompt_text,
        env: vec![],
        resume_handle: resume,
        tmux_target: row.tmux_target,
        model: row.model,
    };

    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?, ?, ?, ?, ?)",
            params![
                now_ms(),
                if handoff.is_some() { "recovery" } else { "operator" },
                node_id,
                if handoff.is_some() { "respawn" } else { "spawn" },
                json!({ "slot": slot_id, "agent": agent_id }).to_string()
            ],
        );
    }
    // A machine joining a shared workspace needs the checkout before the agent
    // starts, or its first act is to look at an empty directory. Failing to
    // sync is reported but does not block the launch: an agent that cannot
    // reach the remote can still talk to its team.
    if let Some(ws_id) = &row.workspace_id {
        if let Ok(ws) = crate::sync::load(st, ws_id) {
            if ws.kind == "repo" && ws.sync == "git" {
                match crate::sync::run_on(
                    st, &ws, &node_id, &workspace_path, nexmesh_proto::SyncOp::Ensure,
                    "nexmesh", "prepare workspace",
                )
                .await
                {
                    Ok(v) => {
                        crate::sync::record(st, ws_id, &node_id, &v);
                        tracing::info!(workspace = %ws.name, node = %node_name,
                            detail = %v["detail"].as_str().unwrap_or(""), "prepared workspace");
                    }
                    Err(e) => tracing::warn!(workspace = %ws.name, node = %node_name,
                        error = %format!("{e:#}"), "could not prepare the workspace"),
                }
            }
        }
    }

    st.send_to_node(&node_id, &HubToNode::Spawn(spec)).await?;
    Ok((agent_id, node_name))
}
