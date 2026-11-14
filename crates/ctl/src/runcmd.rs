// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! `nexmesh run <cli>`: start an agent CLI here, in this directory, on a
//! terminal the fleet owns.
//!
//! The problem it solves: a CLI started by hand is a full member of its team
//! -- it registers, takes a seat, sends and receives, holds locks -- and it
//! cannot be attached to, because the pty behind it belongs to the terminal
//! emulator it was typed into and nothing outside that process can read or
//! write it. There is no way around that from outside: the one kernel
//! facility that would (`TIOCSTI`) exists switched off precisely to stop it.
//!
//! So the pty is made on the other side of the fence. The node starts the CLI
//! on a terminal it owns and this command relays that terminal to the one you
//! are sitting at, which leaves you typing at a CLI in your own directory that
//! is also, from the first keystroke, attachable from any machine in the
//! fleet, drivable by the hub, and recorded. It outlives the window: close the
//! terminal and it is still there, which is what `attach` is for.
//!
//! Put it behind a shell function and the difference disappears:
//!
//! ```sh
//! claude() { nexmesh run claude; }
//! ```

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Which CLI descriptor on this machine the operator meant.
///
/// A machine may have several configurations of one program -- `claude-nextime`
/// and `claude-thinkdifferent` are two logins for claude, each its own
/// descriptor -- and nobody types the suffix. So an exact id wins, a single
/// configuration of that program wins, and several is the one case that has to
/// be asked about rather than guessed: picking a login for somebody is picking
/// whose account the work is billed to.
pub fn resolve_cli(want: &str, available: &[String]) -> std::result::Result<String, String> {
    resolve_cli_as(want, available, &std::env::var("USER").unwrap_or_default())
}

/// As above, told who is asking: a configuration named after the user running
/// the command is that user's own login, and choosing it is not guessing.
pub fn resolve_cli_as(
    want: &str,
    available: &[String],
    user: &str,
) -> std::result::Result<String, String> {
    if available.iter().any(|c| c == want) {
        return Ok(want.to_string());
    }
    let prefix = format!("{want}-");
    let matches: Vec<&String> = available.iter().filter(|c| c.starts_with(&prefix)).collect();
    if matches.len() > 1 && !user.is_empty() {
        let mine = format!("{prefix}{user}");
        if let Some(m) = matches.iter().find(|c| **c == &mine) {
            return Ok((*m).clone());
        }
    }
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(format!(
            "this machine has no '{want}'. It has: {}.",
            if available.is_empty() { "nothing".into() } else { available.join(", ") }
        )),
        _ => Err(format!(
            "'{want}' is installed here in {} configurations: {}. Name the one you want.",
            matches.len(),
            matches.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub struct Opts<'a> {
    pub cli: &'a str,
    pub session: Option<&'a str>,
    pub name: Option<&'a str>,
    pub node: Option<&'a str>,
    pub role: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub dry_run: bool,
    pub attach: bool,
}

/// What to do about the session this directory belongs to.
#[derive(Debug, PartialEq, Clone)]
pub enum Team {
    /// A session already owns this directory. The second field is the
    /// workspace whose mount contains it, when one does: a seat without a
    /// workspace has no directory, and the node then starts the CLI in its own
    /// home -- which is how `run` in /working/nexmesh first put a CLI in
    /// /home/nextime.
    Join(String, Option<String>),
    /// Nothing owns it: make one, and mount this directory in it.
    Make(String),
}

/// What to do about the seat.
#[derive(Debug, PartialEq, Clone)]
pub enum Seat {
    /// A seat of this name is here and empty: take it, which also means the
    /// conversation it was last on is the one that comes back.
    Reuse { slot: String, name: String },
    /// No free seat of that name: a new one.
    Make { name: String },
}

/// A seat name from the CLI and the machine, which is how the fleet's seats
/// are already named: `Claude-sissy`, `Codex-zeiss`.
pub fn seat_name(cli: &str, host: &str) -> String {
    // The cli id may carry a suffix for the account it is installed under
    // (`claude-nextime`); the seat is named after the program.
    let short = cli.split('-').next().unwrap_or(cli);
    let mut c = short.chars();
    let cli = match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => short.to_string(),
    };
    format!("{cli}-{host}")
}

/// `name`, or the first of `name-2`, `name-3`, ... that nobody has.
pub fn free_name(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_string();
    }
    (2..1000)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
        // A thousand seats of one name on one machine is not a case to
        // design for; it is a case not to loop for ever over.
        .unwrap_or_else(|| format!("{name}-1000"))
}

/// What a session covering this directory should be called: the directory's
/// own name, which is how a person refers to it. `$HOME` is not "nextime".
pub fn session_name(cwd: &str, home: &str) -> String {
    let t = cwd.trim_end_matches('/');
    if !home.is_empty() && t == home.trim_end_matches('/') {
        return "home".into();
    }
    t.rsplit('/').find(|p| !p.is_empty()).unwrap_or("work").to_string()
}

/// Whether a seat is free to be taken: a seat whose last process has gone.
/// A seat somebody is sitting in is not offered, because two CLIs in one seat
/// is the bug `seat` and `spawn` already refuse.
pub fn pick_seat(seats: &[Value], want: &str) -> Seat {
    let taken: Vec<String> = seats
        .iter()
        .filter_map(|a| a["name"].as_str().map(String::from))
        .collect();
    for a in seats {
        if a["name"].as_str() != Some(want) {
            continue;
        }
        let live = a["exited"].as_bool() != Some(true)
            && a["agent_status"].as_str() != Some("exited")
            && a["agent_status"].as_str() != Some("disconnected");
        if !live {
            if let Some(slot) = a["slot_id"].as_str() {
                return Seat::Reuse { slot: slot.to_string(), name: want.to_string() };
            }
        }
    }
    Seat::Make { name: free_name(want, &taken) }
}

/// This machine's node name: what was asked for, what the node service on
/// this machine calls itself, or the hostname.
pub fn this_node(asked: Option<&str>) -> String {
    if let Some(n) = asked {
        return n.to_string();
    }
    if let Ok(n) = std::env::var("NEXMESH_NAME") {
        if !n.trim().is_empty() {
            return n.trim().to_string();
        }
    }
    if let Ok(text) = std::fs::read_to_string("/etc/nexmesh/node.env") {
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("NEXMESH_NAME=") {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

/// `nexmesh run`: the steps, done.
pub async fn run(
    base: &str,
    c: &reqwest::Client,
    o: Opts<'_>,
    ca_cert: Option<&str>,
    identity: Option<(&str, &str)>,
) -> Result<()> {
    let cwd = match o.cwd {
        Some(d) => d.to_string(),
        None => std::env::current_dir()
            .context("asking where we are")?
            .to_string_lossy()
            .to_string(),
    };
    let node = this_node(o.node);
    if node.is_empty() {
        anyhow::bail!(
            "cannot tell which machine this is: pass --node with the name \
             `nexmesh nodes` lists for it"
        );
    }
    // A node that is not there cannot start anything, and the reason should
    // not arrive as a spawn failure three steps later.
    let nodes = crate::get(c, format!("{base}/v1/nodes")).await?;
    let known: Vec<String> = nodes["nodes"]
        .as_array()
        .map(|rows| {
            rows.iter().filter_map(|n| n["name"].as_str().map(String::from)).collect()
        })
        .unwrap_or_default();
    if !known.iter().any(|n| n == &node) {
        anyhow::bail!(
            "no node named '{node}' in this fleet. Known: {}.\n\
             Install the node service on this machine, or pass --node.",
            if known.is_empty() { "none".into() } else { known.join(", ") }
        );
    }
    let row = nodes["nodes"]
        .as_array()
        .and_then(|rows| rows.iter().find(|n| n["name"].as_str() == Some(node.as_str())).cloned())
        .unwrap_or(Value::Null);
    if row["connected"].as_bool() != Some(true) {
        anyhow::bail!(
            "node '{node}' is not connected to the hub right now, so nothing can be \
             started on it"
        );
    }
    // Which descriptor to start, settled HERE: a wrong answer three steps
    // later would have left a session and a seat behind it.
    let installed: Vec<String> = row["clis"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let cli = resolve_cli(o.cli, &installed).map_err(anyhow::Error::msg)?;
    if cli != o.cli {
        println!("cli        {cli}");
    }

    // Which team this directory belongs to.
    let owner = crate::get(
        c,
        format!(
            "{base}/v1/session-for?node={}&path={}",
            urlencoding(&node),
            urlencoding(&cwd)
        ),
    )
    .await?;
    let covering = owner["workspace"].as_str().map(String::from);
    let team = match o.session {
        // Named explicitly: that team, with the workspace covering this
        // directory only if it is that team's.
        Some(s) => Team::Join(
            s.to_string(),
            covering.filter(|_| owner["session"].as_str() == Some(s) || owner["session_id"].as_str() == Some(s)),
        ),
        None => match owner["session"].as_str().filter(|s| !s.is_empty()) {
            Some(name) => Team::Join(name.to_string(), covering),
            None => {
                Team::Make(session_name(&cwd, &std::env::var("HOME").unwrap_or_default()))
            }
        },
    };

    let host_label = node.clone();
    let want = o.name.map(String::from).unwrap_or_else(|| seat_name(&cli, &host_label));

    if o.dry_run {
        match &team {
            Team::Join(s, ws) => println!(
                "session:   join {s}{}",
                match ws {
                    Some(w) => format!(", working in its \"{w}\" workspace"),
                    None => format!(", mounting {cwd} into it"),
                }
            ),
            Team::Make(s) => println!("session:   create {s}, with {cwd} mounted on {node}"),
        }
        println!("seat:      {want} (reused if it is here and empty)");
        println!("spawn:     {cli} on {node}, interactive, in {cwd}");
        println!("then:      attach this terminal to it");
        return Ok(());
    }

    // The session, made if this directory belongs to nobody -- and in either
    // case a workspace that covers this directory, because that is what tells
    // the node where to start the CLI.
    let (session, workspace) = match team {
        Team::Join(s, ws) => {
            println!("session    {s}");
            let ws = match ws {
                Some(w) => w,
                None => {
                    // The team does not know about this directory yet. Mount
                    // it rather than starting the CLI somewhere else.
                    let name = session_name(&cwd, &std::env::var("HOME").unwrap_or_default());
                    let v = crate::post(
                        c,
                        format!("{base}/v1/sessions/{s}/workspaces"),
                        json!({ "name": name, "kind": "node_local",
                                "node": node, "path": cwd }),
                    )
                    .await?;
                    crate::bail_on_error(&v)?;
                    println!("workspace  {name} (mounting {cwd} on {node})");
                    name
                }
            };
            (s, ws)
        }
        Team::Make(name) => {
            let v = crate::post(
                c,
                format!("{base}/v1/sessions"),
                json!({
                    "name": name,
                    "workspaces": [{
                        "name": name, "kind": "node_local",
                        "node": node, "path": cwd,
                    }],
                }),
            )
            .await?;
            crate::bail_on_error(&v)?;
            println!("session    {name} (new, owning {cwd} on {node})");
            (name.clone(), name)
        }
    };

    // The seat: the one of this name that is free, or a new one.
    let status = crate::get(c, format!("{base}/v1/sessions/{session}/status")).await?;
    let seats = status["agents"].as_array().cloned().unwrap_or_default();
    let (slot, name) = match pick_seat(&seats, &want) {
        Seat::Reuse { slot, name } => {
            println!("seat       {name} ({slot}, taking the seat it left)");
            (slot, name)
        }
        Seat::Make { name } => {
            let mut body = json!({
                "cli_id": cli, "display_name": name, "mode": "interactive",
                "placement": { "node": node },
                // Which directory the CLI starts in, on that machine.
                "workspace": workspace,
            });
            if let Some(r) = o.role {
                body["role"] = json!(r);
            }
            let v = crate::post(c, format!("{base}/v1/sessions/{session}/slots"), body).await?;
            crate::bail_on_error(&v)?;
            let slot = v["slot_id"].as_str().unwrap_or_default().to_string();
            println!("seat       {name} ({slot}, new)");
            (slot, name)
        }
    };

    // And the CLI, on a terminal the node owns.
    let v = crate::post(
        c,
        format!("{base}/v1/slots/{slot}/spawn"),
        json!({ "cli": cli, "mode": "interactive" }),
    )
    .await?;
    crate::bail_on_error(&v)?;
    println!("started    {cli} on {node} in {cwd}");

    if !o.attach {
        println!("\nAttach when you want it:  nexmesh attach {session} {name}");
        return Ok(());
    }
    // A CLI needs a moment to put something on its screen; attaching into an
    // empty pty looks like nothing happened.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let reason = crate::attach(base, c, &session, &name, 0, ca_cert, identity).await?;
    println!("{reason}");
    println!("It is still here: nexmesh attach {session} {name}");
    Ok(())
}

/// Percent-encoding for the two characters a path and a name can contain that
/// a query string cannot carry as-is. Not a general encoder: a node name is
/// `[A-Za-z0-9_-]` by the time the hub has it, and a path is absolute.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => out.push(ch),
            _ => out.push_str(&format!("%{:02X}", ch as u32 as u8)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_configuration_of_a_program_is_the_one_meant() {
        let have = vec!["claude-nextime".to_string(), "codex".to_string()];
        assert_eq!(resolve_cli("claude", &have).unwrap(), "claude-nextime");
        assert_eq!(resolve_cli("codex", &have).unwrap(), "codex");
    }

    #[test]
    fn two_logins_for_one_program_is_asked_about_not_guessed() {
        // Picking a login is picking whose account the work is billed to --
        // unless one of them is named after whoever is asking.
        let have = vec![
            "claude-nextime".to_string(),
            "claude-thinkdifferent".to_string(),
        ];
        let e = resolve_cli_as("claude", &have, "someoneelse").unwrap_err();
        assert!(e.contains("claude-nextime") && e.contains("claude-thinkdifferent"), "{e}");
        assert!(e.contains("2 configurations"), "{e}");
        // Named exactly, there is nothing to ask.
        assert_eq!(resolve_cli_as("claude-nextime", &have, "x").unwrap(), "claude-nextime");
    }

    #[test]
    fn my_own_login_is_the_default_among_several() {
        let have = vec![
            "claude-nextime".to_string(),
            "claude-thinkdifferent".to_string(),
        ];
        assert_eq!(resolve_cli_as("claude", &have, "nextime").unwrap(), "claude-nextime");
        // And a user who has no configuration of their own is still asked.
        assert!(resolve_cli_as("claude", &have, "").is_err());
    }

    #[test]
    fn a_program_the_machine_does_not_have_says_what_it_has() {
        let have = vec!["codex".to_string()];
        let e = resolve_cli("qwen", &have).unwrap_err();
        assert!(e.contains("no 'qwen'") && e.contains("codex"), "{e}");
        let e = resolve_cli("qwen", &[]).unwrap_err();
        assert!(e.contains("nothing"), "{e}");
    }

    #[test]
    fn a_seat_is_named_after_the_program_and_the_machine() {
        assert_eq!(seat_name("claude", "sissy"), "Claude-sissy");
        // The cli id carries the account it is installed under; the seat does
        // not.
        assert_eq!(seat_name("claude-nextime", "zeiss"), "Claude-zeiss");
        assert_eq!(seat_name("codex", "lisa"), "Codex-lisa");
    }

    #[test]
    fn a_taken_name_gets_the_next_one() {
        let taken = vec!["Claude-sissy".to_string(), "Claude-sissy-2".to_string()];
        assert_eq!(free_name("Claude-sissy", &taken), "Claude-sissy-3");
        assert_eq!(free_name("Codex-sissy", &taken), "Codex-sissy");
    }

    #[test]
    fn a_session_is_named_after_the_directory_but_home_is_home() {
        assert_eq!(session_name("/working/nexmesh", "/home/nextime"), "nexmesh");
        assert_eq!(session_name("/working/nexmesh/", "/home/nextime"), "nexmesh");
        assert_eq!(session_name("/home/nextime", "/home/nextime"), "home");
        assert_eq!(session_name("/home/nextime/", "/home/nextime"), "home");
        assert_eq!(session_name("/", "/home/nextime"), "work");
    }

    #[test]
    fn an_empty_seat_of_the_right_name_is_taken_not_duplicated() {
        // The whole point of a stable name: run it again tomorrow and the
        // conversation that seat was on is the one that comes back.
        let seats = vec![json!({
            "name": "Claude-sissy", "slot_id": "sl-1", "exited": true,
            "agent_status": "exited",
        })];
        assert_eq!(
            pick_seat(&seats, "Claude-sissy"),
            Seat::Reuse { slot: "sl-1".into(), name: "Claude-sissy".into() }
        );
    }

    #[test]
    fn a_seat_somebody_is_sitting_in_is_left_alone() {
        let seats = vec![json!({
            "name": "Claude-sissy", "slot_id": "sl-1", "exited": false,
            "agent_status": "running",
        })];
        assert_eq!(
            pick_seat(&seats, "Claude-sissy"),
            Seat::Make { name: "Claude-sissy-2".into() }
        );
    }

    #[test]
    fn a_seat_between_turns_is_not_free_either() {
        // A one-shot CLI is due back in that seat; its process really has
        // ended, and the seat is not empty.
        let seats = vec![json!({
            "name": "Hermes-lisa", "slot_id": "sl-h", "exited": true,
            "agent_status": "exited", "status": "waiting",
        })];
        // Reused -- which is right: `run` would start the next turn there
        // itself. What must not happen is a second seat beside it.
        assert_eq!(
            pick_seat(&seats, "Hermes-lisa"),
            Seat::Reuse { slot: "sl-h".into(), name: "Hermes-lisa".into() }
        );
    }

    #[test]
    fn nothing_of_that_name_means_a_new_seat_of_that_name() {
        let seats = vec![json!({
            "name": "Codex-sissy", "slot_id": "sl-c", "exited": false,
            "agent_status": "running",
        })];
        assert_eq!(
            pick_seat(&seats, "Claude-sissy"),
            Seat::Make { name: "Claude-sissy".into() }
        );
    }

    #[test]
    fn a_path_survives_the_query_string() {
        assert_eq!(urlencoding("/working/nexmesh"), "/working/nexmesh");
        assert_eq!(urlencoding("/home/a b"), "/home/a%20b");
        assert_eq!(urlencoding("sissy"), "sissy");
    }
}
