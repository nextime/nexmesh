//! Reminding a seat to keep what it learns, and to look before it reads.
//!
//! Telling a model in its tool descriptions that the team has a memory and a
//! code graph is not enough, and that is measured, not guessed: across a
//! fleet of a dozen working seats over thirty hours, every memory, recall and
//! knowledge tool was called ZERO times while the seats sent a hundred
//! messages. A description is read once, at the start, and then the work
//! takes over. What does change behaviour is being asked at the moment it
//! applies -- which is what the fold nudge already does for context.
//!
//! So the hub asks, on the ordinary message path, at two moments:
//!
//!   * **keep it** -- a seat that has finished several turns of real work
//!     without storing, keeping or folding anything is asked, once, whether
//!     any of it is worth keeping, and where. `signal_done` with nothing kept
//!     asks at once: finishing a piece of work is when what it established is
//!     clearest and about to be forgotten.
//!   * **look it up** -- a seat that has read many files since it last asked
//!     the code graph anything is told, once, that the graph would have
//!     answered where-is and who-calls in one call.
//!
//! Each is rate-limited per agent, and either one goes quiet as soon as the
//! seat does the thing: a write resets the turn count and a graph query
//! resets the read count. A reminder asks for no reply, because a reminder
//! that makes six models answer "noted" is worse than none. It does not offer
//! "nothing worth keeping" as an answer either: seats took that exit after
//! stretches that had found real causes. It is never sent to a seat whose
//! tools the node reports gone (`mcp_link` = 0); that seat can keep nothing.
//!
//! BOTH ARE ON UNLESS SOMEBODY TURNS THEM OFF -- the opposite of the
//! watchdogs, deliberately. They cost a short message every few hours at
//! most, they change nothing in a seat's context the seat did not choose,
//! and the fleet default being off is how a memory ends up with 21 entries.
//! The switch is per seat, then per team, then the fleet (`--remind-memory`,
//! `--remind-graph`), in the same three states as the watchdogs: NULL
//! inherits, 0 is off, anything else is on.

use anyhow::Result;
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

/// Turns of work with nothing kept before a seat is asked.
///
/// A turn is one model turn ending -- active to idle -- so this is "a stretch
/// of work", not "a while". Small enough that a session of real work is
/// asked at least once; large enough that a seat answering one quick
/// question is never asked about it.
pub const MEMORY_AFTER_TURNS: i64 = 8;

/// The fewest turns that make a `signal_done` worth asking about. One turn
/// that finished something small has nothing in it the next one needs.
pub const MEMORY_ON_DONE_TURNS: i64 = 2;

/// How long before the same agent is asked again.
pub const MEMORY_EVERY_MS: i64 = 3 * 3600 * 1000;

/// Files read without a graph query before a seat is told the graph exists.
pub const GRAPH_AFTER_READS: i64 = 25;

/// How long before the same agent is told again.
pub const GRAPH_EVERY_MS: i64 = 2 * 3600 * 1000;

/// The tools that count as keeping something.
pub fn is_keeping(tool: &str) -> bool {
    matches!(tool, "store_memory" | "store_knowledge" | "fold_context" | "save_context")
}

/// Is this reminder on for this seat? Seat, then team, then the fleet.
pub fn enabled(db: &Connection, slot_id: &str, column: &str, fleet: bool) -> Result<bool> {
    let fleet = if fleet { 1 } else { 0 };
    Ok(crate::watchdog::setting(db, slot_id, column, fleet)? > 0)
}

/// What to send, and to whom.
#[derive(Debug, Clone, PartialEq)]
pub struct Reminder {
    pub slot_id: String,
    pub session_id: String,
    pub kind: &'static str,
    pub text: String,
}

/// The seat an agent fills, if it is one that can be reminded at all.
///
/// Not an observer, which may not write; not a paused or held seat, which
/// would get the reminder hours later about work it no longer remembers; not
/// an external participant, which is a program and not a model.
fn remindable(db: &Connection, agent_id: &str) -> Result<Option<(String, String)>> {
    let row: Option<(String, String, Option<String>, i64, Option<String>, Option<i64>)> = db
        .query_row(
            "SELECT s.id, s.session_id, s.authority, COALESCE(s.paused, 0), s.participant_kind,
                    a.mcp_link
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE a.id = ? AND a.exited_at_ms IS NULL",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((slot, session, authority, paused, kind, link)) = row else { return Ok(None) };
    if paused != 0 || kind.as_deref() == Some("external") {
        return Ok(None);
    }
    // A seat whose node says its nexmesh tools are gone cannot call any of
    // them: asking it to is a turn spent on "the tools are down anyway".
    if link == Some(0) {
        return Ok(None);
    }
    if authority.as_deref().map(nexmesh_proto::authority::Authority::parse).map(|a| a.is_read_only())
        == Some(true)
    {
        return Ok(None);
    }
    Ok(Some((slot, session)))
}

/// The agent whose MODEL a reading is about.
///
/// A seat adopted in a terminal (tmux) is two rows: the seat with its CLI,
/// and the terminal it sits in. The screen is read through the terminal, so
/// its turns arrive under the terminal's agent -- and a terminal is not a
/// model, cannot keep anything, and must not be reminded. The seat records
/// which terminal it is in (`terminal_via`); that seat's own live agent is
/// the one whose turns these are. A terminal nobody sits in is nobody.
pub fn model_agent(db: &Connection, agent_id: &str) -> Result<Option<String>> {
    let cli: Option<Option<String>> = db
        .query_row(
            "SELECT s.cli_id FROM agents a JOIN slots s ON s.id = a.slot_id WHERE a.id = ?",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(cli) = cli else { return Ok(None) };
    if cli.as_deref() != Some("tmux") {
        return Ok(Some(agent_id.to_string()));
    }
    Ok(db
        .query_row(
            "SELECT a.id FROM slots o JOIN agents a ON a.slot_id = o.id
              WHERE o.terminal_via = ? AND a.exited_at_ms IS NULL
              ORDER BY a.started_at_ms DESC LIMIT 1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// An agent's turn ended (its activity went from active to idle).
pub fn turn_ended(db: &Connection, agent_id: &str, fleet_memory: bool) -> Result<Option<Reminder>> {
    let Some(agent) = model_agent(db, agent_id)? else { return Ok(None) };
    let agent_id = agent.as_str();
    db.execute(
        "UPDATE agents SET turns_unkept = COALESCE(turns_unkept, 0) + 1 WHERE id = ?",
        [agent_id],
    )?;
    memory_due(db, agent_id, MEMORY_AFTER_TURNS, fleet_memory, false)
}

/// The agent called `signal_done`.
pub fn work_done(db: &Connection, agent_id: &str, fleet_memory: bool) -> Result<Option<Reminder>> {
    memory_due(db, agent_id, MEMORY_ON_DONE_TURNS, fleet_memory, true)
}

fn memory_due(
    db: &Connection,
    agent_id: &str,
    after: i64,
    fleet: bool,
    done: bool,
) -> Result<Option<Reminder>> {
    let Some((slot, session)) = remindable(db, agent_id)? else { return Ok(None) };
    if !enabled(db, &slot, "remind_memory", fleet)? {
        return Ok(None);
    }
    let (turns, last): (i64, Option<i64>) = db.query_row(
        "SELECT COALESCE(a.turns_unkept, 0), o.memory_reminded_at_ms
           FROM agents a JOIN slots o ON o.id = a.slot_id WHERE a.id = ?",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if turns < after || last.is_some_and(|at| now_ms() - at < MEMORY_EVERY_MS) {
        return Ok(None);
    }
    db.execute(
        "UPDATE slots SET memory_reminded_at_ms = ? WHERE id = ?",
        params![now_ms(), &slot],
    )?;
    let when = if done {
        "You have just finished a piece of work".to_string()
    } else {
        format!("You have worked through {turns} turns")
    };
    let text = format!(
        "[nexmesh] {when} without keeping anything from it. Your nexmesh tools are connected \
         and working. Look back over that stretch: a cause you found, a number you measured, \
         where something lives, a command that worked, a decision and why, what a teammate \
         owns. Whatever of that you would have to rediscover after a restart, keep now, a \
         line or two each:\n\
         \u{2022} `store_memory` with scope 'agent' for your own working knowledge: it stands \
         in for your CLI's own memory and outlives your restarts (`recall` brings it back);\n\
         \u{2022} `store_knowledge` for what the team should know;\n\
         \u{2022} `fold_context` for a finished stretch of work you no longer need to carry.\n\
         Then carry on with what you were doing. This note needs no answer: do not write \
         about it to anyone."
    );
    Ok(Some(Reminder { slot_id: slot, session_id: session, kind: "memory", text }))
}

/// What a seat is told, in its SYSTEM PROMPT, about keeping what it learns
/// and asking the graph -- the standing half of the reminders.
///
/// A reminder is a message: it arrives at one moment, is compacted away and
/// was being brushed off. The system prompt is read on every turn and is
/// what a model takes as how it works, so the habit lives there and the
/// reminders are the backstop. Each half follows its reminder's switch, so a
/// seat or team that turned memory or graph reminders off gets neither.
///
/// None for a seat that would be told nothing: an external participant (a
/// program), or one with both switches off.
pub fn habits(db: &Connection, slot_id: &str, fleet_memory: bool, fleet_graph: bool) -> Result<Option<String>> {
    let row: Option<(Option<String>, Option<String>, bool)> = db
        .query_row(
            "SELECT participant_kind, authority, workspace_id IS NOT NULL FROM slots WHERE id = ?",
            [slot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, authority, has_tree)) = row else { return Ok(None) };
    if kind.as_deref() == Some("external") {
        return Ok(None);
    }
    let read_only = authority
        .as_deref()
        .map(nexmesh_proto::authority::Authority::parse)
        .is_some_and(|a| a.is_read_only());
    let memory = !read_only && enabled(db, slot_id, "remind_memory", fleet_memory)?;
    let graph = has_tree && enabled(db, slot_id, "remind_graph", fleet_graph)?;
    Ok(habits_text(memory, graph))
}

/// The text itself, apart from the database so it can be read in a test.
pub fn habits_text(memory: bool, graph: bool) -> Option<String> {
    if !memory && !graph {
        return None;
    }
    let mut out = String::from(
        "# How you work in this nexmesh team\n\n\
         These are standing habits, not suggestions. Your nexmesh tools are how you keep and \
         find things; your CLI's own memory does not reach your teammates or survive your \
         restarts the same way.\n",
    );
    if memory {
        out.push_str(
            "\n## Memory\n\
             - When you start, after a restart and after your context is compacted, call \
             `recall` (scope 'agent') and `query_knowledge` about the task in front of you \
             BEFORE you plan. What you and the team worked out earlier is there, and nothing \
             brings it back unless you ask.\n\
             - Keep things AS you learn them, in the same turn, a line or two each: a cause \
             you found, a number you measured, where something lives, a command that worked, \
             a decision and why. Your own working knowledge goes in `store_memory` with scope \
             'agent'; what the team needs goes in `store_knowledge`. Not at the end: the end \
             is where it gets lost.\n\
             - When a stretch of work is finished and you no longer need its detail, \
             `fold_context` it, before your context fills rather than after.\n",
        );
    }
    if graph {
        out.push_str(
            "\n## Code graph\n\
             - To find where something is defined, who calls it, or what a file depends on, \
             ask `query_code_graph` FIRST, before grep or opening files. One call answers \
             across the whole tree; then read only the files it points you to.\n\
             - The graph is a map, not the files: it holds where things are, never their \
             contents. Read a file from disk before you change it, and if the graph and the \
             disk disagree, the disk wins -- say so in the chat. It follows edits on its own \
             within a minute or two.\n",
        );
    }
    Some(out)
}

/// The node counted `n` more files read by this agent.
pub fn files_read(
    db: &Connection,
    agent_id: &str,
    n: i64,
    fleet_graph: bool,
) -> Result<Option<Reminder>> {
    files_read_with(db, agent_id, n, &[], fleet_graph)
}

/// The same, knowing WHICH files: the reminder then carries what the graph
/// already says about them, instead of saying that it exists.
pub fn files_read_with(
    db: &Connection,
    agent_id: &str,
    n: i64,
    paths: &[String],
    fleet_graph: bool,
) -> Result<Option<Reminder>> {
    if n <= 0 {
        return Ok(None);
    }
    let Some(agent) = model_agent(db, agent_id)? else { return Ok(None) };
    let agent_id = agent.as_str();
    db.execute(
        "UPDATE agents SET reads_unguided = COALESCE(reads_unguided, 0) + ? WHERE id = ?",
        params![n, agent_id],
    )?;
    let Some((slot, session)) = remindable(db, agent_id)? else { return Ok(None) };
    if !enabled(db, &slot, "remind_graph", fleet_graph)? {
        return Ok(None);
    }
    // A seat with no workspace has no tree of its own to parse; the graph
    // would only answer it about other teams' code.
    let workspace: Option<String> = db
        .query_row("SELECT workspace_id FROM slots WHERE id = ?", [&slot], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(workspace) = workspace else { return Ok(None) };
    let (reads, last): (i64, Option<i64>) = db.query_row(
        "SELECT COALESCE(a.reads_unguided, 0), o.graph_reminded_at_ms
           FROM agents a JOIN slots o ON o.id = a.slot_id WHERE a.id = ?",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if reads < GRAPH_AFTER_READS || last.is_some_and(|at| now_ms() - at < GRAPH_EVERY_MS) {
        return Ok(None);
    }
    db.execute(
        "UPDATE slots SET graph_reminded_at_ms = ? WHERE id = ?",
        params![now_ms(), &slot],
    )?;
    let shown = graph_on(db, &workspace, paths);
    let text = if shown.is_empty() {
        format!(
            "[nexmesh] You have opened {reads} files since you last asked the code graph \
             anything. `query_code_graph` answers where a symbol is defined, who calls it and \
             what a file depends on in one call, across the whole tree, without reading it -- \
             then read only the files it points you to. It is a map, not the files: read a \
             file from disk before changing it, and if the two disagree the disk wins (say so \
             in the chat). This note needs no answer."
        )
    } else {
        format!(
            "[nexmesh] You have opened {reads} files since you last asked the code graph \
             anything. Here is what it already knew about the ones you opened last, from \
             one query each:\n{shown}\n\
             `query_code_graph` (find, defines, callers, importers, subsystem) gives this for \
             any symbol or file in one call; then read only what it points you to. It is a \
             map, not the files: read a file from disk before changing it, and if the two \
             disagree the disk wins. This note needs no answer."
        )
    };
    Ok(Some(Reminder { slot_id: slot, session_id: session, kind: "graph", text }))
}

/// What the graph says about up to two of `paths` (newest first): what each
/// defines and what depends on it, cut short. Empty when none of them is in
/// this workspace's graph -- a reminder then advertises, which is all it can.
///
/// The seat reads by absolute path and the graph keeps paths relative to the
/// tree, so a file matches when its graph path ends the absolute one.
fn graph_on(db: &Connection, workspace: &str, paths: &[String]) -> String {
    const EACH: usize = 420;
    let cut = |s: &str| -> String {
        let s = s.trim();
        if s.chars().count() <= EACH {
            return s.replace('\n', "\n    ");
        }
        let short: String = s.chars().take(EACH).collect();
        format!("{}...", short.trim_end()).replace('\n', "\n    ")
    };
    let mut out = String::new();
    let mut done = 0;
    for p in paths.iter().rev() {
        if done == 2 {
            break;
        }
        let rel: Option<String> = db
            .query_row(
                "SELECT path FROM code_graph_nodes
                  WHERE workspace_id = ?1 AND kind = 'file' AND path <> ''
                    AND (?2 = path OR ?2 LIKE '%/' || path)
                  ORDER BY LENGTH(path) DESC LIMIT 1",
                params![workspace, p],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let Some(rel) = rel else { continue };
        let defines = crate::graph::query(db, workspace, "defines", &rel, "").unwrap_or_default();
        let users = crate::graph::query(db, workspace, "importers", &rel, "").unwrap_or_default();
        out.push_str(&format!("\u{2022} {rel}\n    {}\n", cut(&defines)));
        if !users.trim().is_empty() {
            out.push_str(&format!("    {}\n", cut(&users)));
        }
        done += 1;
    }
    out.trim_end().to_string()
}

/// A tool call went through: reset whatever it answers.
pub fn tool_called(db: &Connection, agent_id: &str, tool: &str) -> Result<()> {
    if is_keeping(tool) {
        db.execute("UPDATE agents SET turns_unkept = 0 WHERE id = ?", [agent_id])?;
    } else if tool == "query_code_graph" {
        db.execute("UPDATE agents SET reads_unguided = 0 WHERE id = ?", [agent_id])?;
    }
    Ok(())
}

/// Put a reminder on the seat's message path, and in the audit log.
///
/// The same path as the fold nudge: a driven seat gets it within a tick, a
/// typed seat at its terminal, a hand-run CLI at its next tool call -- and it
/// is in the room's log afterwards, so an operator can see who asked.
pub fn send(db: &Connection, r: &Reminder) -> Result<()> {
    db.execute(
        "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
         VALUES (?, NULL, ?, 'nudge', ?, ?)",
        params![r.session_id, r.slot_id, r.text, now_ms()],
    )?;
    let _ = db.execute(
        "INSERT INTO audit (at_ms, actor, action, detail) VALUES (?,?,?,?)",
        params![
            now_ms(),
            "reminders",
            format!("reminded_{}", r.kind),
            serde_json::json!({ "slot": r.slot_id }).to_string()
        ],
    );
    tracing::info!(slot = %r.slot_id, kind = r.kind, "reminded a seat");
    Ok(())
}

/// Run a check and send what it asks for; a failure is logged, never fatal.
pub fn deliver(db: &Connection, found: Result<Option<Reminder>>) {
    match found {
        Ok(Some(r)) => {
            if let Err(e) = send(db, &r) {
                tracing::warn!(error = %e, slot = %r.slot_id, "could not send a reminder");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "reminder check failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        crate::state::migrate(&db).unwrap();
        db.execute_batch(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);
             INSERT INTO workspaces (id,session_id,name,kind) VALUES ('w1','s1','w','repo');
             INSERT INTO slots (id,session_id,workspace_id,display_name,cli_id,created_at_ms)
               VALUES ('sl1','s1','w1','Ada','claude',1);
             INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
               VALUES ('ag1','sl1','managed','running',1,1);",
        )
        .unwrap();
        db
    }

    fn turns(db: &Connection, n: i64) -> Option<Reminder> {
        let mut last = None;
        for _ in 0..n {
            last = turn_ended(db, "ag1", true).unwrap();
            if last.is_some() {
                break;
            }
        }
        last
    }

    #[test]
    fn a_stretch_of_work_with_nothing_kept_is_asked_about_once() {
        let db = setup();
        assert!(turns(&db, MEMORY_AFTER_TURNS - 1).is_none());
        let r = turn_ended(&db, "ag1", true).unwrap().expect("asked at the threshold");
        assert_eq!(r.slot_id, "sl1");
        assert!(r.text.contains("store_memory"));
        // And not again inside the cooldown, however many turns follow.
        assert!(turns(&db, 30).is_none());
    }

    #[test]
    fn a_respawn_does_not_restart_the_cooldown() {
        let db = setup();
        assert!(turns(&db, MEMORY_AFTER_TURNS).is_some());
        // The seat comes back as a new agent, as a node restart does.
        db.execute_batch(
            "UPDATE agents SET exited_at_ms = 2, status = 'exited' WHERE id = 'ag1';
             INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
               VALUES ('ag2','sl1','managed','running',3,3);",
        )
        .unwrap();
        for _ in 0..30 {
            assert!(turn_ended(&db, "ag2", true).unwrap().is_none(), "reminded again after a respawn");
        }
    }

    #[test]
    fn a_seat_whose_tools_are_gone_is_not_asked() {
        let db = setup();
        db.execute("UPDATE agents SET mcp_link = 0 WHERE id = 'ag1'", []).unwrap();
        assert!(turns(&db, 30).is_none(), "asked a seat that cannot call store_memory");
        db.execute("UPDATE agents SET mcp_link = 1 WHERE id = 'ag1'", []).unwrap();
        assert!(turn_ended(&db, "ag1", true).unwrap().is_some());
    }

    #[test]
    fn the_standing_habits_follow_their_switches() {
        assert!(habits_text(false, false).is_none());
        let m = habits_text(true, false).unwrap();
        assert!(m.contains("recall") && m.contains("store_memory") && !m.contains("query_code_graph"));
        let g = habits_text(false, true).unwrap();
        assert!(g.contains("query_code_graph") && g.contains("disk wins") && !g.contains("store_memory"));
        let db = setup();
        assert!(habits(&db, "sl1", true, true).unwrap().is_some());
        db.execute("UPDATE slots SET remind_memory = 0, remind_graph = 0 WHERE id = 'sl1'", []).unwrap();
        assert!(habits(&db, "sl1", true, true).unwrap().is_none(), "both switched off");
    }

    #[test]
    fn keeping_something_resets_the_count() {
        let db = setup();
        turns(&db, MEMORY_AFTER_TURNS - 1);
        tool_called(&db, "ag1", "store_memory").unwrap();
        assert!(turns(&db, MEMORY_AFTER_TURNS - 1).is_none());
    }

    #[test]
    fn finishing_work_asks_sooner() {
        let db = setup();
        turn_ended(&db, "ag1", true).unwrap();
        assert!(work_done(&db, "ag1", true).unwrap().is_none(), "one turn is too little");
        turn_ended(&db, "ag1", true).unwrap();
        assert!(work_done(&db, "ag1", true).unwrap().is_some());
    }

    #[test]
    fn the_switch_is_seat_then_team_then_fleet() {
        let db = setup();
        assert!(enabled(&db, "sl1", "remind_memory", true).unwrap());
        assert!(!enabled(&db, "sl1", "remind_memory", false).unwrap());
        db.execute("UPDATE sessions SET remind_memory = 0", []).unwrap();
        assert!(!enabled(&db, "sl1", "remind_memory", true).unwrap());
        db.execute("UPDATE slots SET remind_memory = 1", []).unwrap();
        assert!(enabled(&db, "sl1", "remind_memory", true).unwrap());
        // Off for the seat: never asked.
        db.execute("UPDATE slots SET remind_memory = 0", []).unwrap();
        assert!(turns(&db, 40).is_none());
    }

    #[test]
    fn a_terminals_turns_are_the_seat_inside_it() {
        let db = setup();
        db.execute_batch(
            "INSERT INTO slots (id,session_id,display_name,cli_id,created_at_ms)
               VALUES ('slt','s1','term-Ada','tmux',1), ('slx','s1','term-empty','tmux',1);
             INSERT INTO agents (id,slot_id,provenance,status,started_at_ms,last_seen_ms)
               VALUES ('agt','slt','managed','running',1,1), ('agx','slx','managed','running',1,1);
             UPDATE slots SET terminal_via = 'agt' WHERE id = 'sl1';",
        )
        .unwrap();
        let mut got = None;
        for _ in 0..MEMORY_AFTER_TURNS {
            got = turn_ended(&db, "agt", true).unwrap();
        }
        let r = got.expect("the seat in the terminal is asked");
        assert_eq!(r.slot_id, "sl1", "never the terminal itself");
        for _ in 0..40 {
            assert!(turn_ended(&db, "agx", true).unwrap().is_none(), "an empty terminal is nobody");
        }
    }

    #[test]
    fn a_paused_seat_is_not_reminded() {
        let db = setup();
        db.execute("UPDATE slots SET paused = 1", []).unwrap();
        assert!(turns(&db, 40).is_none());
    }

    #[test]
    fn reading_the_tree_without_the_graph_is_pointed_at_it_once() {
        let db = setup();
        assert!(files_read(&db, "ag1", GRAPH_AFTER_READS - 1, true).unwrap().is_none());
        let r = files_read(&db, "ag1", 1, true).unwrap().expect("told at the threshold");
        assert!(r.text.contains("query_code_graph"));
        assert!(files_read(&db, "ag1", 100, true).unwrap().is_none(), "cooldown");
    }

    #[test]
    fn a_graph_query_resets_the_reads() {
        let db = setup();
        files_read(&db, "ag1", GRAPH_AFTER_READS - 1, true).unwrap();
        tool_called(&db, "ag1", "query_code_graph").unwrap();
        assert!(files_read(&db, "ag1", GRAPH_AFTER_READS - 1, true).unwrap().is_none());
    }

    #[test]
    fn graph_reminders_can_be_switched_off() {
        let db = setup();
        db.execute("UPDATE slots SET remind_graph = 0", []).unwrap();
        assert!(files_read(&db, "ag1", 500, true).unwrap().is_none());
    }

    #[test]
    fn a_reminder_lands_on_the_seats_message_path() {
        let db = setup();
        deliver(&db, files_read(&db, "ag1", GRAPH_AFTER_READS, true));
        let n: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE to_slot = 'sl1' AND kind = 'nudge'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}
