//! `nexmesh seat`: give one of your own conversations a keyboard.
//!
//! A CLI you started by hand has exactly one route in: its own next tool
//! call. Sitting at its prompt it may not make one for hours, so a message
//! from a teammate -- or a question from you in the chat -- simply waits. The
//! fleet looks like it is ignoring you.
//!
//! The cure already exists: a tmux pane the node has adopted is a terminal
//! the hub can type at, carefully (see `node/src/termlink.rs` for what has to
//! be proved first). What was missing was the three steps that get you there,
//! which nobody should have to remember:
//!
//!   1. a tmux session, in the directory that conversation was running in;
//!   2. `nexmesh adopt`, so the node holds a view of that pane;
//!   3. the CLI started INSIDE it, resuming the conversation the seat holds.
//!
//! Step 3 is the one that needs the hub: the seat knows which conversation is
//! its own -- that is what `cli_session` is -- so the resume handle does not
//! have to be found by hand in a list of twenty.
//!
//! This runs on the machine whose terminal it is: tmux is local, and so is
//! the node that will adopt it.

use anyhow::{bail, Context, Result};
use nexmesh_proto::cli_descriptor::CliDescriptor;
use serde_json::Value;

/// A shell at a prompt, as tmux names the command running in a pane. Anything
/// else means something is already in there and we leave it alone.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "ash", "tcsh", "csh"];

pub struct Opts<'a> {
    pub session: &'a str,
    pub who: &'a str,
    pub tmux: Option<&'a str>,
    pub clis_dir: &'a str,
    pub cmd: Option<&'a str>,
    pub attach: bool,
    pub dry_run: bool,
}

/// What we are going to do, so it can be tested without a tmux anywhere.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub tmux_name: String,
    pub node: String,
    pub cwd: String,
    /// The command to type into the pane. `None` when the caller passed
    /// `--cmd ''`, or when the seat holds no conversation to resume and no
    /// command was given.
    pub command: Option<String>,
    pub seat_name: String,
}

/// Derive the tmux session name from a seat's name.
///
/// tmux does not allow `.` or `:` in a session name (they address windows and
/// panes), and a name with a space in it is a trap in every shell line that
/// follows, so anything that is not a letter, digit or dash becomes a dash.
pub fn tmux_name_for(who: &str) -> String {
    let mut out = String::from("nexmesh-");
    let mut last_dash = false;
    for ch in who.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// How to start this CLI so it picks up a conversation it already has.
///
/// Taken from the CLI's own descriptor -- `resume_args` of its interactive
/// mode -- so a CLI nexmesh has never heard of needs a toml file and nothing
/// else. Deliberately WITHOUT the descriptor's `args`: those wire up
/// nexmesh's own MCP config for a spawned agent, and this is the user's own
/// CLI, with the seat already installed in its own configuration.
pub fn resume_command(d: &CliDescriptor, conversation: Option<&str>) -> Option<String> {
    let cmd = d.cli.command.clone();
    let Some(conv) = conversation.filter(|c| !c.trim().is_empty()) else {
        return Some(cmd);
    };
    let mode = d.modes.get("interactive")?;
    if mode.resume_args.is_empty() {
        return Some(cmd);
    }
    let args: Vec<String> =
        mode.resume_args.iter().map(|a| a.replace("{resume}", conv)).collect();
    Some(format!("{cmd} {}", args.join(" ")))
}

/// Everything the hub has to say about the seat, turned into a plan.
pub fn plan(o: &Opts, seat: &Value, descriptor: Option<&CliDescriptor>) -> Result<Plan> {
    let name = seat["name"].as_str().unwrap_or(o.who).to_string();
    let node = seat["host"].as_str().unwrap_or("").to_string();
    if node.is_empty() || node == "-" {
        bail!(
            "'{name}' has never run on a machine the fleet knows, so there is no \
             terminal to make for it"
        );
    }
    let cwd = seat["cwd"].as_str().filter(|c| !c.is_empty()).unwrap_or(".").to_string();
    let tmux_name =
        o.tmux.map(|t| t.to_string()).unwrap_or_else(|| tmux_name_for(&name));
    let command = match o.cmd {
        Some("") => None,
        Some(c) => Some(c.to_string()),
        None => {
            let conv = seat["cli_session"].as_str();
            descriptor.and_then(|d| resume_command(d, conv))
        }
    };
    Ok(Plan {
        tmux_name,
        node,
        cwd,
        command,
        seat_name: format!("term-{name}"),
    })
}

/// Is this pane empty enough to type a command into?
pub fn pane_is_idle(current_command: &str) -> bool {
    SHELLS.contains(&current_command.trim())
}

fn tmux(args: &[&str]) -> Result<std::process::Output> {
    // TMUX is removed for the same reason the node removes it: run from
    // inside a pane, tmux refuses to create a session ("sessions should be
    // nested with care") and every query answers about the WRONG pane.
    std::process::Command::new("tmux")
        .args(args)
        .env_remove("TMUX")
        .output()
        .context("running tmux (is it installed on this machine?)")
}

fn tmux_ok(args: &[&str]) -> Result<String> {
    let out = tmux(args)?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn session_exists(name: &str) -> bool {
    tmux(&["has-session", "-t", &format!("={name}")])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create the tmux session if it is not there, and say whether it was made.
pub fn ensure_session(name: &str, cwd: &str, label: &str) -> Result<bool> {
    if session_exists(name) {
        return Ok(false);
    }
    // A generous default size: the pane exists before any terminal attaches,
    // and a CLI that draws itself into 80x24 and is then attached at 200x50
    // leaves half a screen of debris.
    tmux_ok(&["new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50"])?;
    // A row that says whose terminal this is and how to get out of it. Only
    // on a session we made ourselves: a session somebody else set up has
    // their own status line, and it is not ours to overwrite.
    //
    // This is the same answer as the viewer's bar, in the other place the
    // same question gets asked. In here the way out is tmux's own key, not
    // Ctrl-], because in here you are talking to tmux.
    let _ = tmux_ok(&["set-option", "-t", name, "status-left",
                      &format!(" nexmesh \u{203a} {label} ")]);
    let _ = tmux_ok(&["set-option", "-t", name, "status-right",
                      " #{?client_prefix,PREFIX,Ctrl-B d detach} "]);
    let _ = tmux_ok(&["set-option", "-t", name, "status-left-length", "60"]);
    Ok(true)
}

/// What is running in the pane right now.
pub fn pane_command(name: &str) -> Option<String> {
    tmux_ok(&["display-message", "-p", "-t", name, "#{pane_current_command}"]).ok()
}

pub fn send_command(name: &str, command: &str) -> Result<()> {
    tmux_ok(&["send-keys", "-t", name, command, "C-m"]).map(|_| ())
}

pub fn load_descriptor(clis_dir: &str, cli_id: &str) -> Option<CliDescriptor> {
    let dir = match (clis_dir.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => clis_dir.to_string(),
    };
    let text = std::fs::read_to_string(format!("{dir}/{cli_id}.toml")).ok()?;
    CliDescriptor::parse(&text).ok()
}

/// `nexmesh seat <session> <who>`: the three steps, done.
pub async fn run(
    base: &str,
    c: &reqwest::Client,
    o: Opts<'_>,
) -> anyhow::Result<()> {
    let st = crate::get(c, format!("{base}/v1/sessions/{}/status", o.session)).await?;
    crate::bail_on_error(&st)?;
    let empty = vec![];
    let agents = st["agents"].as_array().unwrap_or(&empty);
    let want = o.who.to_lowercase();
    let seat = agents
        .iter()
        .find(|a| {
            a["name"].as_str().map(|n| n.to_lowercase()) == Some(want.clone())
                || a["slot_id"].as_str() == Some(o.who)
        })
        .ok_or_else(|| {
            let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
            anyhow::anyhow!(
                "no seat called '{}' in '{}'. There is: {}",
                o.who,
                o.session,
                if names.is_empty() { "nobody".into() } else { names.join(", ") }
            )
        })?;

    let name = seat["name"].as_str().unwrap_or(o.who).to_string();
    // A conversation that is still running somewhere else must not be opened
    // a second time: two CLIs on one conversation is how you lose half of it.
    // A dry run changes nothing, so it is allowed to show you the steps for
    // a seat that is still busy -- that is when you would ask.
    if !o.dry_run && seat["exited"].as_bool() == Some(false) && o.cmd.is_none() {
        let pid = seat["pid"].as_i64().map(|p| format!(" (pid {p})")).unwrap_or_default();
        anyhow::bail!(
            "{name} is still running{pid}. Quit it first -- then this puts the same \
             conversation back, in a terminal the fleet can type at.\n\
             (Or pass --cmd '' to make the terminal now and start it yourself later.)"
        );
    }
    if seat["typed_via_terminal"].as_bool() == Some(true) {
        println!(
            "{name} already has a terminal the fleet can type at{}.",
            seat["tmux_target"].as_str().map(|t| format!(" ({t})")).unwrap_or_default()
        );
    }

    let cli_id = seat["cli"].as_str().unwrap_or("").to_string();
    let descriptor = load_descriptor(o.clis_dir, &cli_id);
    if descriptor.is_none() && o.cmd.is_none() {
        println!(
            "note: no descriptor for '{cli_id}' in {} -- starting a plain shell in the \
             pane instead of the CLI. Pass --cmd to say what to run.",
            o.clis_dir
        );
    }
    let p = plan(&o, seat, descriptor.as_ref())?;

    // tmux is local, and so is the node that will adopt the pane.
    let here = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().split('.').next().unwrap_or("").to_string())
        .unwrap_or_default();
    if !here.is_empty() && here != p.node {
        anyhow::bail!(
            "{name} runs on {}, and a terminal has to be made there: run this on {} \
             (ssh {}, then `nexmesh seat {} {}`)",
            p.node, p.node, p.node, o.session, o.who
        );
    }

    if o.dry_run {
        println!("tmux new-session -d -s {} -c {}", p.tmux_name, p.cwd);
        println!("nexmesh adopt {} --node {} --tmux {} --name {}",
                 o.session, p.node, p.tmux_name, p.seat_name);
        if let Some(cmd) = &p.command {
            println!("tmux send-keys -t {} {cmd:?} C-m", p.tmux_name);
        }
        return Ok(());
    }

    let made = ensure_session(&p.tmux_name, &p.cwd, &name)?;
    println!(
        "{} tmux session {}",
        if made { "created" } else { "using the existing" },
        p.tmux_name
    );

    let v = crate::post(
        c,
        format!("{base}/v1/sessions/{}/adopt", o.session),
        serde_json::json!({
            "node": p.node, "tmux_target": p.tmux_name,
            "name": p.seat_name, "role": "terminal"
        }),
    )
    .await?;
    crate::bail_on_error(&v)?;
    println!(
        "{} {} as {}",
        if v["reused"].as_bool() == Some(true) { "re-adopted" } else { "adopted" },
        p.tmux_name,
        p.seat_name
    );

    // Only into a pane with nothing in it: typing a command over a running
    // CLI is the one thing this must never do.
    match (&p.command, pane_command(&p.tmux_name)) {
        (Some(cmd), Some(running)) if pane_is_idle(&running) => {
            send_command(&p.tmux_name, cmd)?;
            println!("started: {cmd}");
        }
        (Some(_), Some(running)) => println!(
            "left the pane alone: '{running}' is already running in it"
        ),
        (Some(_), None) => println!("could not read what is in the pane; left it alone"),
        (None, _) => {}
    }

    println!(
        "\nIt is yours and the fleet's at the same time:\n           tmux attach -t {}          # your own terminal\n           nexmesh attach {} {}   # from anywhere, Ctrl-] to leave\n\
         A message for {name} is now typed at it in seconds, rather than waiting \
         for its next tool call.",
        p.tmux_name, o.session, o.who
    );
    if o.attach && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("\nattaching your terminal to it now (Ctrl-B d to leave tmux)...");
        let err = std::process::Command::new("tmux")
            .args(["attach", "-t", &p.tmux_name])
            .env_remove("TMUX")
            .status();
        if let Err(e) = err {
            println!("could not attach: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude() -> CliDescriptor {
        CliDescriptor::parse(
            r#"
            [cli]
            id = "claude"
            display_name = "Claude Code"
            command = "claude"
            [modes.interactive]
            args = ["--mcp-config", "{mcp_config}"]
            resume_args = ["--resume", "{resume}"]
            attachable = true
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_tmux_name_is_safe_to_type_and_to_address() {
        assert_eq!(tmux_name_for("Orchestrator"), "nexmesh-orchestrator");
        assert_eq!(tmux_name_for("Claude-sissy"), "nexmesh-claude-sissy");
        // Dots and colons address windows and panes; spaces break every
        // shell line that follows.
        assert_eq!(tmux_name_for("Claude 2.0:dev"), "nexmesh-claude-2-0-dev");
        assert_eq!(tmux_name_for("odd!!!"), "nexmesh-odd");
    }

    #[test]
    fn the_resume_command_is_the_cli_plus_its_own_conversation() {
        let c = resume_command(&claude(), Some("abc-123")).unwrap();
        assert_eq!(c, "claude --resume abc-123");
        // No nexmesh MCP flags: this is the user's own CLI, with the seat
        // already installed in its own config.
        assert!(!c.contains("--mcp-config"), "{c}");
    }

    #[test]
    fn with_no_conversation_it_is_just_the_cli() {
        assert_eq!(resume_command(&claude(), None).unwrap(), "claude");
        assert_eq!(resume_command(&claude(), Some("  ")).unwrap(), "claude");
    }

    #[test]
    fn a_plan_comes_from_what_the_seat_says_about_itself() {
        let seat = json!({
            "name": "Orchestrator", "host": "machine-a", "cwd": "/home/me/work",
            "cli_session": "conv-7", "cli": "claude"
        });
        let o = Opts {
            session: "orchestrator", who: "Orchestrator", tmux: None,
            clis_dir: "/etc/nexmesh/clis", cmd: None, attach: true, dry_run: true,
        };
        let p = plan(&o, &seat, Some(&claude())).unwrap();
        assert_eq!(p.tmux_name, "nexmesh-orchestrator");
        assert_eq!(p.cwd, "/home/me/work");
        assert_eq!(p.node, "machine-a");
        assert_eq!(p.seat_name, "term-Orchestrator");
        assert_eq!(p.command.as_deref(), Some("claude --resume conv-7"));
    }

    #[test]
    fn a_seat_that_has_never_run_anywhere_has_no_terminal_to_make() {
        let seat = json!({ "name": "Ghost", "host": "-" });
        let o = Opts {
            session: "s", who: "Ghost", tmux: None, clis_dir: "x", cmd: None,
            attach: false, dry_run: true,
        };
        let e = plan(&o, &seat, None).unwrap_err().to_string();
        assert!(e.contains("no terminal"), "{e}");
    }

    #[test]
    fn an_explicit_command_wins_and_an_empty_one_means_just_a_shell() {
        let seat = json!({ "name": "Max", "host": "nd", "cli_session": "c1" });
        let mut o = Opts {
            session: "s", who: "Max", tmux: Some("mine"), clis_dir: "x",
            cmd: Some("qwen --resume c1"), attach: false, dry_run: true,
        };
        let p = plan(&o, &seat, Some(&claude())).unwrap();
        assert_eq!(p.tmux_name, "mine");
        assert_eq!(p.command.as_deref(), Some("qwen --resume c1"));
        o.cmd = Some("");
        assert_eq!(plan(&o, &seat, Some(&claude())).unwrap().command, None);
    }

    #[test]
    fn only_a_shell_prompt_is_safe_to_type_a_command_into() {
        assert!(pane_is_idle("bash"));
        assert!(pane_is_idle(" zsh "));
        // Something is already running in there: a second CLI typed over the
        // top of it is the thing this must never do.
        assert!(!pane_is_idle("claude"));
        assert!(!pane_is_idle("node"));
        assert!(!pane_is_idle("vim"));
    }
}
