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
    /// The CLI's own conversation id, when this seat was taken by a human's
    /// CLI rather than spawned. For claude and codex this IS a resume
    /// handle.
    cli_session: Option<String>,
    cli_session_own: bool,
    /// A system prompt this seat carries, and whether it adds to the CLI's
    /// own or replaces it. The seat's, not the conversation's, so it
    /// survives a respawn, a node restart and a move to another machine.
    system_prompt: Option<String>,
    system_prompt_mode: String,
    /// Said TO the agent as its first turn, every time a new process takes
    /// this seat.
    instructions: Option<String>,
    /// The TEAM'S standing rules, from the session. Read here rather than
    /// separately so one query answers "how do I start this seat".
    team_rules: Option<String>,
}

fn read_slot(st: &AppState, slot_id: &str) -> Result<SlotRow> {
    let db = st.db.lock().unwrap();
    db.query_row(
        "SELECT s.session_id, se.name, s.cli_id, s.mode, s.placement, s.display_name,
                s.role, s.role_description, s.workspace_id, s.resume_handle, s.tmux_target,
                s.model, s.cli_session, s.cli_session_own,
                s.system_prompt, COALESCE(s.system_prompt_mode, 'append'), s.instructions,
                se.instructions
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
                cli_session: r.get(12)?,
                cli_session_own: r.get::<_, i64>(13)? != 0,
                system_prompt: r.get(14)?,
                system_prompt_mode: r.get(15)?,
                instructions: r.get(16)?,
                team_rules: r.get(17)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("no slot '{slot_id}'"))
}

/// Launch (or relaunch) a slot. `handoff` is extra text prepended to the
/// prompt, used to tell a respawned agent that it is resuming.
/// How many folds a starting seat is handed. The rest are a `context_map`
/// away, and a handover that fills the window it exists to save is not one.
const FOLD_HANDOVER_MAX: i64 = 12;

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
            // No workspace: the node decides where its own user can work.
            // Reading HOME here reads the *hub's* environment.
            None => (String::new(), false),
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
    let team_rules = row.team_rules.clone().unwrap_or_default();
    let brief = prompt::AgentBrief {
        display_name: &name,
        role: &role_s,
        role_description: desc.split("\n\nTASK: ").next().unwrap_or(""),
        session_name: &row.session_name,
        node_name: &node_name,
        workspace_path: &cwd,
        shared_workspace: shared,
        task: &task,
        team_rules: team_rules.as_str(),
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
    // A seat taken by hand has no resume_handle -- nothing reported one,
    // because no node was driving it -- but it does have the CLI's own
    // conversation id, which for claude and codex is exactly what --resume
    // takes. Without this, restarting someone's closed conversation under
    // the hub's control gives them a BLANK one wearing the same name.
    //
    // Unless the last attempt died on it. `claude --resume <id>` fails with
    // "No conversation found with session ID" when that conversation is not
    // in the project directory it is launched in -- a different cwd, another
    // machine, a cleaned-up history -- and it exits at once. Retried, that is
    // a flap that never resolves and whose only visible symptom is a seat
    // restarting for ever. So a start that lasted no time at all spends the
    // next attempt on a FRESH conversation: one wasted launch instead of
    // three, and a seat that comes back rather than one that cannot.
    let resume_just_failed = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT started_at_ms, exited_at_ms FROM agents
              WHERE slot_id = ? AND provenance = 'managed'
              ORDER BY started_at_ms DESC LIMIT 1",
            [slot_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|(start, exit)| exit.map(|e| e - start < 10_000))
        .unwrap_or(false)
    };
    let mut candidates: Vec<String> = if resume_just_failed {
        tracing::info!(
            %slot_id,
            "the last start exited at once; trying a fresh conversation rather than resuming"
        );
        // Nothing offered at all: whatever it died on, it is not worth
        // handing back a second time.
        vec![]
    } else {
        // THE CONVERSATION THIS SEAT IS ON **HERE**.
        //
        // Looked up by where it is about to run -- this machine, this
        // directory, this CLI -- because a conversation is a local file and
        // does not follow a seat elsewhere. A seat that moved away and came
        // back finds the one it left; a seat on a machine it has never run
        // on gets none, deliberately, instead of being handed one that
        // cannot be there and dying on it.
        //
        // `slots.resume_handle` is the newest handle from anywhere and is
        // kept for display; it is NOT what a spawn uses, because "the last
        // conversation this seat had" and "the conversation it can resume
        // here" are different things the moment a seat moves.
        let (here, bound_anywhere) = {
            let db = st.db.lock().unwrap();
            let here = db
                .query_row(
                    "SELECT handle FROM slot_conversations
                      WHERE slot_id = ? AND node_id = ? AND cwd = ? AND cli_id = ?",
                    params![slot_id, &node_id, &cwd, row.cli_id.clone().unwrap_or_default()],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .ok()
                .flatten();
            let any: i64 = db
                .query_row(
                    "SELECT COUNT(*) FROM slot_conversations WHERE slot_id = ?",
                    [slot_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (here, any > 0)
        };
        if here.is_none() && bound_anywhere {
            tracing::info!(
                %slot_id, node = %node_id, cwd = %cwd,
                "this seat has conversations elsewhere but none here; starting a fresh one"
            );
        }
        // EVERY conversation this seat might be resuming, best first.
        //
        // This used to pick one and hope. It went wrong exactly as you would
        // expect: a GHOST handle in front of a real conversation meant the
        // real one was never tried. The node declined the ghost, started
        // something blank, and the work was still sitting on disk -- three
        // seats on the live fleet, with 130MB, 61MB and 4.9MB of
        // conversation between them, and it was caught by hand in
        // pre-flight rather than by anything here.
        //
        // The hub cannot tell which of these still exists, because the
        // files are on the node. So it offers all of them, in the order it
        // would have preferred them anyway, and the node -- the only thing
        // that can see the disk -- takes the first that is really there.
        let mut candidates: Vec<String> = vec![];
        let mut offer = |c: Option<String>| {
            if let Some(c) = c.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) {
                if !candidates.contains(&c) {
                    candidates.push(c);
                }
            }
        };
        // 1. The binding for THIS machine, this directory, this CLI: the
        //    conversation this seat was actually having here.
        offer(here);
        // 2. The seat's last handle from anywhere, but only when the seat
        //    has no bindings at all -- one from before these were recorded.
        //    A seat that HAS bindings and none for here is on a new machine,
        //    and another machine's conversation is not its to resume.
        if !bound_anywhere {
            offer(row.resume.clone());
        }
        // 3. The id the CLI itself reported, which for claude and codex is
        //    exactly what `--resume` takes.
        //
        //    This used to be refused unless the CLI had reported it
        //    (`cli_session_own`). That flag was a PROXY for "is this a real
        //    conversation id or a label somebody chose", and the node can
        //    now answer that properly by looking for the file. Keeping the
        //    proxy cost real work: three seats whose conversations existed
        //    were handed blank ones because an OLD shim had recorded their
        //    ids without the flag.
        offer(row.cli_session.clone());
        candidates
    };
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
            // 4. A conversation from an earlier session under this identity.
            //    Offered LAST and no longer only-when-nothing-else: if the
            //    better candidates turn out to be ghosts, this one is still
            //    worth trying before starting blank.
            if let Some(h) = handle.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) {
                if !candidates.contains(&h) {
                    tracing::info!(slot = %slot_id, cli = %cli_for_identity,
                        "offering a conversation from an earlier session too");
                    candidates.push(h);
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
    let seat_authority = {
        let db = st.db.lock().unwrap();
        crate::fleet::authority_of(&db, slot_id)
    };
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
        // The first candidate, so a node from before `resume_candidates`
        // behaves exactly as it did.
        resume_handle: candidates.first().cloned(),
        resume_candidates: candidates,
        // The shim's tool list is built from this on the node. It is read
        // here, at spawn, which is also why granting authority to a seat
        // that is already running does not give its model the fleet tools
        // until the seat restarts: an MCP client fixes its tool list at
        // startup.
        authority: Some(seat_authority.as_str().to_string()),
        system_prompt: row
            .system_prompt
            .clone()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty()),
        system_prompt_mode: Some(row.system_prompt_mode.clone()),
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

    let session_id = spec.session_id.clone();
    st.send_to_node(&node_id, &HubToNode::Spawn(spec)).await?;

    // THE SEAT'S STANDING INSTRUCTION, to whoever is in it now.
    //
    // Sent as a MESSAGE rather than folded into the first prompt, because the
    // first prompt only reaches a CLI whose mode declares `prompt_on_stdin`
    // -- which interactive claude does not. An instruction that arrived for a
    // headless seat and silently went nowhere for the interactive seat beside
    // it would be the worst of the two.
    //
    // As a message it goes by whatever route that seat has: pushed to a
    // driver, typed at a terminal, or waiting for the agent to ask. Delivery
    // already knows which, and already knows to wait until the CLI is ready
    // to be typed at -- which a write at spawn time does not.
    if let Some(i) = row.instructions.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let db = st.db.lock().unwrap();
        let r = db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES (?, NULL, ?, 'system', ?, ?)",
            params![
                session_id,
                slot_id,
                format!("[THIS SEAT'S STANDING INSTRUCTION]\n{i}"),
                now_ms()
            ],
        );
        match r {
            Ok(_) => tracing::info!(slot = %slot_id, bytes = i.len(),
                "gave the new occupant this seat's standing instruction"),
            // Worth saying out loud: the agent is starting either way, and
            // an operator who set an instruction should not have to infer
            // from its behaviour that it never got one.
            Err(e) => tracing::error!(slot = %slot_id, error = %e,
                "could not queue this seat's standing instruction"),
        }
    }
    Ok((agent_id, node_name))
}

#[cfg(test)]
mod binding_tests {
    use crate::state::AppState;
    use rusqlite::{params, OptionalExtension};

    /// What a spawn on a given machine would resume.
    ///
    /// The same three-part lookup `launch` does: this machine, this
    /// directory, this CLI -- with the legacy fallback to `slots.resume_handle`
    /// only when the seat has no binding at all.
    fn would_resume(st: &AppState, slot: &str, node: &str, cwd: &str, cli: &str)
        -> Option<String>
    {
        let db = st.db.lock().unwrap();
        let here = db
            .query_row(
                "SELECT handle FROM slot_conversations
                  WHERE slot_id = ? AND node_id = ? AND cwd = ? AND cli_id = ?",
                params![slot, node, cwd, cli],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten();
        let any: i64 = db
            .query_row("SELECT COUNT(*) FROM slot_conversations WHERE slot_id = ?",
                       [slot], |r| r.get(0))
            .unwrap_or(0);
        let legacy: Option<String> = db
            .query_row("SELECT resume_handle FROM slots WHERE id = ?", [slot], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        here.or(if any > 0 { None } else { legacy })
    }

    fn bind(st: &AppState, slot: &str, node: &str, cwd: &str, cli: &str, handle: &str) {
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO slot_conversations (slot_id,node_id,cwd,cli_id,handle,updated_at_ms)
             VALUES (?,?,?,?,?,1)
             ON CONFLICT(slot_id,node_id,cwd,cli_id) DO UPDATE SET handle = excluded.handle",
            params![slot, node, cwd, cli, handle],
        )
        .unwrap();
    }

    fn fleet() -> AppState {
        let st = AppState::in_memory().unwrap();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
             VALUES ('sl-1','s1','Dev','claude',1)",
            [],
        )
        .unwrap();
        drop(db);
        st
    }

    #[test]
    fn a_conversation_is_offered_only_where_it_exists() {
        // A conversation is a local file. Offering a seat one that lives on
        // another machine is how `--resume` comes back "no conversation
        // found" and the seat exits before it can be opened.
        let st = fleet();
        bind(&st, "sl-1", "workstation", "/srv/project", "claude", "conv-on-workstation");
        assert_eq!(
            would_resume(&st, "sl-1", "workstation", "/srv/project", "claude").as_deref(),
            Some("conv-on-workstation")
        );
        // Same seat, another machine: nothing, deliberately.
        assert_eq!(would_resume(&st, "sl-1", "laptop", "/srv/project", "claude"), None);
        // Same machine, another directory -- claude keys conversations by
        // project directory, so this is a different conversation too.
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/home/dev", "claude"), None);
        // Same place, another CLI: it cannot read the other's conversation.
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/srv/project", "codex"), None);
    }

    #[test]
    fn a_seat_that_moves_away_and_back_finds_what_it_left() {
        // The point of recording per place rather than keeping one handle:
        // moving a seat does not destroy the conversation it had there.
        let st = fleet();
        bind(&st, "sl-1", "workstation", "/w", "claude", "workstation-conv");
        bind(&st, "sl-1", "laptop", "/w", "claude", "laptop-conv");
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/w", "claude").as_deref(),
                   Some("workstation-conv"));
        assert_eq!(would_resume(&st, "sl-1", "laptop", "/w", "claude").as_deref(),
                   Some("laptop-conv"));
    }

    #[test]
    fn renaming_or_moving_a_seat_between_teams_changes_nothing() {
        // The binding is keyed on the SEAT, so neither its name nor which
        // team it is in is part of the identity of its conversation.
        let st = fleet();
        bind(&st, "sl-1", "workstation", "/w", "claude", "conv");
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "INSERT INTO sessions (id,name,created_at_ms,last_active_ms)
                 VALUES ('s2','Other',1,1)",
                [],
            )
            .unwrap();
            db.execute(
                "UPDATE slots SET display_name = 'Renamed', session_id = 's2' WHERE id = 'sl-1'",
                [],
            )
            .unwrap();
        }
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/w", "claude").as_deref(), Some("conv"));
    }

    #[test]
    fn a_seat_with_bindings_elsewhere_is_not_given_the_legacy_handle() {
        // `slots.resume_handle` is the newest handle from ANYWHERE and is
        // kept for display. Falling back to it when a seat has bindings but
        // none here would hand it another machine's conversation, which is
        // the whole thing being prevented.
        let st = fleet();
        {
            let db = st.db.lock().unwrap();
            db.execute("UPDATE slots SET resume_handle = 'from-somewhere' WHERE id = 'sl-1'", [])
                .unwrap();
        }
        // No bindings yet: a seat from before these were recorded still
        // resumes, so an upgrade does not throw away what people had.
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/w", "claude").as_deref(),
                   Some("from-somewhere"));
        // Once it has a binding somewhere, the legacy handle stops being a
        // fallback for places it does not cover.
        bind(&st, "sl-1", "laptop", "/w", "claude", "laptop-conv");
        assert_eq!(would_resume(&st, "sl-1", "workstation", "/w", "claude"), None);
        assert_eq!(would_resume(&st, "sl-1", "laptop", "/w", "claude").as_deref(),
                   Some("laptop-conv"));
    }
}
