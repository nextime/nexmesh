//! Coordinating a shared workspace across machines.
//!
//! The hub does not move any bytes: git does, through whatever remote the
//! workspace declares. What the hub adds is knowing *who* has a checkout of
//! what, and being able to ask each of them to publish or refresh at a
//! sensible moment -- which is the part no individual node can do.

use crate::state::AppState;
use anyhow::{Context, Result};
use nexmesh_proto::{HubToNode, SyncOp};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

pub struct Workspace {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub repo_url: Option<String>,
    pub branch: String,
    pub sync: String,
}

pub fn load(st: &AppState, workspace_id: &str) -> Result<Workspace> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT id, name, kind, repo_url, COALESCE(repo_branch, 'main'), sync
         FROM workspaces WHERE id = ?",
        [workspace_id],
        |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                repo_url: r.get(3)?,
                branch: r.get(4)?,
                sync: r.get(5)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("no workspace '{workspace_id}'"))
}

/// Which nodes hold a checkout of this workspace, and where.
pub fn mounts(st: &AppState, workspace_id: &str) -> Vec<(String, String, String)> {
    let db = st.db.lock().unwrap();
    db.prepare(
        "SELECT m.node_id, n.name, m.path FROM workspace_mounts m
         JOIN nodes n ON n.id = m.node_id WHERE m.workspace_id = ?",
    )
    .and_then(|mut s| {
        s.query_map([workspace_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .and_then(|m| m.collect())
    })
    .unwrap_or_default()
}

/// Ask one node to act on its checkout and wait for the answer.
pub async fn run_on(
    st: &AppState,
    ws: &Workspace,
    node_id: &str,
    path: &str,
    op: SyncOp,
    author: &str,
    message: &str,
) -> Result<Value> {
    anyhow::ensure!(
        ws.kind == "repo",
        "'{}' is a node-local workspace: its files live on one machine, so there is nothing to \
         sync. Shared work needs a repo workspace.",
        ws.name
    );
    anyhow::ensure!(
        ws.sync == "git",
        "workspace '{}' is set to sync = '{}'. Only 'git' is coordinated here; 'shared_fs' means \
         the filesystem already does it.",
        ws.name,
        ws.sync
    );
    let repo_url = ws
        .repo_url
        .clone()
        .with_context(|| format!("workspace '{}' has no repo_url to sync through", ws.name))?;

    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(
        node_id,
        &HubToNode::SyncWorkspace {
            req_id,
            workspace_id: ws.id.clone(),
            path: path.to_string(),
            repo_url,
            branch: ws.branch.clone(),
            op,
            author: author.to_string(),
            message: message.to_string(),
        },
    )
    .await?;

    match tokio::time::timeout(std::time::Duration::from_secs(180), rx).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => anyhow::bail!("the node disconnected while syncing"),
        // git can be slow on a big repository, but not forever.
        Err(_) => anyhow::bail!("the node did not finish syncing within three minutes"),
    }
}

/// Act on every machine holding this workspace.
pub async fn run_everywhere(
    st: &AppState,
    workspace_id: &str,
    op: SyncOp,
    author: &str,
    message: &str,
) -> Result<Value> {
    let ws = load(st, workspace_id)?;
    let mut results = vec![];
    for (node_id, node_name, path) in mounts(st, workspace_id) {
        if !st.node_is_connected(&node_id) {
            results.push(json!({ "node": node_name, "ok": false,
                                 "detail": "that machine is not connected" }));
            continue;
        }
        match run_on(st, &ws, &node_id, &path, op, author, message).await {
            Ok(mut v) => {
                v["node"] = json!(node_name);
                results.push(v);
            }
            // One machine failing must not hide what happened on the others.
            Err(e) => results.push(json!({ "node": node_name, "ok": false,
                                           "detail": format!("{e:#}") })),
        }
    }
    Ok(json!({ "workspace": ws.name, "branch": ws.branch, "results": results }))
}

/// The workspace a slot works in, with where it is mounted on its node.
pub fn slot_workspace(st: &AppState, slot_id: &str) -> Option<(String, String, String)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT w.id, a.node_id, m.path
         FROM slots s
         JOIN workspaces w ON w.id = s.workspace_id
         JOIN agents a ON a.slot_id = s.id AND a.exited_at_ms IS NULL
         JOIN workspace_mounts m ON m.workspace_id = w.id AND m.node_id = a.node_id
         WHERE s.id = ? AND w.kind = 'repo'",
        [slot_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .ok()
    .flatten()
}

/// A workspace and a machine that can read it, for anything that needs the
/// FILES rather than git.
///
/// Two differences from `slot_workspace`, both of which were bugs when the
/// code graph used that instead. It does not require a repo workspace, since
/// parsing only needs a directory. And it resolves the machine from the
/// WORKSPACE's mounts rather than from the asking agent's own node: an
/// external participant has no node at all, so asking where *it* runs gave
/// "you have no workspace" in answer to a question about the code.
///
/// Ordered, so repeated calls pick the same machine and the graph stays
/// consistent between questions.
pub fn workspace_host(st: &AppState, slot_id: &str) -> Option<(String, String, String)> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT w.id, m.node_id, m.path
         FROM slots s
         JOIN workspaces w ON w.id = s.workspace_id
         JOIN workspace_mounts m ON m.workspace_id = w.id
         JOIN nodes n ON n.id = m.node_id
         WHERE s.id = ? AND n.status = 'online'
         ORDER BY m.node_id LIMIT 1",
        [slot_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
    .ok()
    .flatten()
}

/// Render a sync result as something to tell an agent.
pub fn describe(v: &Value) -> String {
    if let Some(results) = v["results"].as_array() {
        let mut out = format!(
            "Workspace {} ({}):\n",
            v["workspace"].as_str().unwrap_or("?"),
            v["branch"].as_str().unwrap_or("?")
        );
        for r in results {
            out.push_str(&format!(
                "- {}: {}{}\n",
                r["node"].as_str().unwrap_or("?"),
                r["detail"].as_str().unwrap_or(""),
                if r["conflict"].as_bool() == Some(true) { "  [CONFLICT]" } else { "" }
            ));
        }
        return out;
    }
    v["detail"].as_str().unwrap_or("done").to_string()
}

/// Record what a sync did, so the dashboard can show it.
pub fn record(st: &AppState, workspace_id: &str, node_id: &str, v: &Value) {
    let db = st.db.lock().unwrap();
    let _ = db.execute(
        "INSERT INTO workspace_sync (workspace_id, node_id, at_ms, ok, conflict, head, detail)
         VALUES (?,?,?,?,?,?,?)
         ON CONFLICT(workspace_id, node_id) DO UPDATE SET
            at_ms = excluded.at_ms, ok = excluded.ok, conflict = excluded.conflict,
            head = excluded.head, detail = excluded.detail",
        params![
            workspace_id,
            node_id,
            nexmesh_proto::now_ms(),
            v["ok"].as_bool().unwrap_or(false) as i64,
            v["conflict"].as_bool().unwrap_or(false) as i64,
            v["head"].as_str(),
            v["detail"].as_str().unwrap_or("")
        ],
    );
}
