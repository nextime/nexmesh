// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Commands typed into the chat.
//!
//! The chat is where an operator already is -- on a phone, in the console, in
//! the dashboard -- and until now the only thing it could do was talk. Moving
//! a seat meant leaving it for a shell. So a line that starts with `/` is read
//! as an instruction instead of an utterance.
//!
//! The parsing lives here, apart from the hub's HTTP layer, for two reasons:
//! it is the part worth testing exhaustively, and it is the part every client
//! gets for free. The console, the dashboard and the PWA all post a chat line
//! to the same endpoint, so none of them needs to know these exist -- which
//! is the whole point of doing it in the hub rather than three times in three
//! front ends.
//!
//! A line that is not a command is an utterance, including `//this one`, which
//! is how you say something that begins with a slash.

/// A chat line, once read.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// Say it to the team, as ever. The string is the text to send, which is
    /// not always what was typed: `//hello` is said as `/hello`.
    Say(String),
    Do(Cmd),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Help,
    Rename { who: String, name: String },
    Move { who: String, session: String },
    Remove { who: String, force: bool },
    Start { who: String, mode: Option<String> },
    Owns { who: String, patterns: Vec<String>, add: bool },
    New { cli: String, name: String, mode: Option<String> },
    /// Which voice reads this agent's words out. A seat, not a machine:
    /// whoever is listening hears the same team.
    Voice { who: String, voice: String },
    /// Mark a seat as supervising the fleet: what agents raise to the
    /// supervisors is delivered to it as an ordinary message, in its own
    /// session, with a return address it can answer.
    Supervisor { who: String, on: bool },
    /// Grant or take away what a seat may do TO THE FLEET. Not `role`, which
    /// is the seat's job and confers nothing.
    Authority { who: String, level: String },
    /// Answer this agent's own prompts without asking. `who` may be an
    /// agent, or `session`/`team` for every seat in the room.
    AutoApprove { who: String, on: bool },
    /// Have a seat from somewhere else also answer in THIS room, or stop.
    /// For service seats -- a capability bridge, a reviewer -- that one team
    /// should not have to own a private copy of.
    Guest { who: String, join: bool },
    /// Whether the hub brings this seat back by itself when it exits.
    AutoRespawn { who: String, on: bool },
    /// Make an agent's CLI compact its own context, now.
    Compact { who: String },
    /// Ask a seat to fold what it no longer needs, now. Works on any CLI:
    /// it is a message to the agent, not a keystroke, so it needs nothing
    /// of that CLI except that it reads its messages.
    Fold { who: String },
    /// How full a seat's context may get before the hub compacts it for
    /// them. `who` may be an agent or `session`/`team` for the whole room.
    /// `pct` of 0 is off, and None restores "inherit".
    CompactAt { who: String, pct: Option<i64> },
    /// How many times a seat may repeat the same turn before the hub
    /// interrupts it and pauses it. Same shape as `CompactAt`.
    LoopGuard { who: String, repeats: Option<i64> },
    /// Whether the hub reminds a seat (or `session`) to keep what it learns
    /// (`which` = remind_memory) or to use the code graph (remind_graph).
    /// None restores "inherit".
    Remind { who: String, which: &'static str, on: Option<bool> },
    /// Read or set the TEAM's standing rules: what everybody here is told,
    /// over and above their own brief. `text` of None is a read.
    Rules { text: Option<String> },
    /// How much a seat may send: messages a minute, and messages exchanged
    /// with one other seat in ten minutes. Same three states again.
    Flood { who: String, per_min: Option<i64>, per_pair: Option<i64> },
    /// Answer a permission prompt that is waiting. `who` narrows it to one
    /// agent when several are stopped at once.
    Decide { who: Option<String>, allow: bool },
}

/// What the chat can be told to do, as the chat itself explains it.
pub fn help() -> String {
    "Commands, typed into the chat:\n\
     \u{2022} /rename <agent> <new name>\n\
     \u{2022} /move <agent> <session>        \u{2014} a session name, with or without the #\n\
     \u{2022} /remove <agent> [force]        \u{2014} force only if it still has a process\n\
     \u{2022} /start <agent> [interactive|headless]\n\
     \u{2022} /new <cli> <name> [interactive|headless]\n\
     \u{2022} /owns <agent> <pattern>...     \u{2014} which parts of the workspace are theirs\n\
     \u{2022} /owns <agent> +<pattern>       \u{2014} add one instead of replacing the list\n\
     \u{2022} /voice <agent> <voice|none>   \u{2014} which voice reads that agent aloud\n\
     \u{2022} /supervisor <agent> [off]      \u{2014} this seat hears what agents raise to the supervisors\n\
     \u{2022} /allow [agent] \u{2502} /deny [agent]  \u{2014} answer a permission prompt that is waiting\n\
     \u{2022} /autoapprove <agent|session> [off]\n\
     \u{2022} /fold <agent>\n\
     \u{2003} \u{2014} answer that seat's (or the whole room's) prompts without asking\n\
     \u{2022} /compact <agent>             \u{2014} tell that agent's CLI to compact its \
     own context now, by typing its own compaction command at it\n\
     \u{2022} /compact-at <agent|session> <pct|off|default>\n\
     \u{2003} \u{2014} compact that seat by itself once its context passes that \
     percentage of its window. `off` for never, `default` to follow the \
     team's or the fleet's setting. Off everywhere unless somebody sets it.\n\
     \u{2022} /loop-guard <agent|session> <repeats|off|default>\n\
     \u{2003} \u{2014} stop that seat if it repeats the same turn that many times \
     in a row: it is interrupted, paused and reported to you, with its \
     conversation intact. Off everywhere unless somebody sets it.\n\
     \u{2022} /remind-memory <agent|session> <on|off|default>\n\
     \u{2003} \u{2014} remind that seat to store what it learns after a stretch of \
     work in which it kept nothing. On everywhere unless somebody turns it off.\n\
     \u{2022} /remind-graph <agent|session> <on|off|default>\n\
     \u{2003} \u{2014} point a seat that reads many files at the code graph. On \
     everywhere unless somebody turns it off.\n\
     \u{2022} /rules [text]                \u{2014} what everybody in this team is told, \
     over and above their own brief: the conventions, what not to touch. With no \
     text it shows them; `clear` removes them. Only you and an orchestrating \
     seat can set them.\n\
     \u{2022} /flood <agent|session> <per-minute> [per-pair]\n\
     \u{2003} \u{2014} refuse messages from that seat beyond that many a minute, or \
     beyond that many exchanged with any ONE other seat in ten minutes. The \
     refusal goes to the agent and says what to do instead; its work is not \
     stopped, only its messages. `-` leaves one of the two alone, `off` clears \
     both. Off everywhere unless somebody sets it.\n\
     \u{2022} /autorespawn <agent> [off]\n\
     \u{2003} \u{2014} whether the hub brings that seat back by itself when its process \
     exits; it prepares the seat for you to attach to, it does not reattach your terminal\n\
     \u{2022} /invite <agent> \u{2502} /uninvite <agent>\n\
     \u{2003} \u{2014} have a seat from another team also answer here, for a service \
     every team needs (a capability bridge, a reviewer) rather than one copy each\n\
     \u{2022} /authority <agent> <observer|agent|orchestrator|super>\n\
     \u{2003} \u{2014} what that seat may do to the FLEET, not its job: an observer \
     reads and reports and cannot steer anything, an agent is the ordinary case, \
     an orchestrator runs its own session and can elect others there, and a \
     superorchestrator runs the fleet. `/elect` is the same command.\n\
     \u{2022} /help\n\
     An agent is named by its name, its role or its seat id. Say something \
     starting with a slash by doubling it: //like this."
        .to_string()
}

/// A setting that is a number, or `off`, or "whatever the level above says".
///
/// Three states, one argument, because that is what the setting is: a seat
/// can be off while its team is on, which is the first thing an operator
/// reaches for and is not expressible with a number alone. `None` is the
/// "inherit" state and is what `default` means.
fn policy_number(
    arg: Option<&str>,
    what: &str,
    low: i64,
    high: i64,
) -> Result<Option<i64>, String> {
    let Some(arg) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        return Err(format!("say {what}, or `off`, or `default`"));
    };
    match arg.to_lowercase().as_str() {
        "off" | "no" | "none" | "never" => Ok(Some(0)),
        "default" | "inherit" | "clear" | "unset" => Ok(None),
        n => {
            let n: i64 = n
                .trim_end_matches('%')
                .parse()
                .map_err(|_| format!("'{arg}' is not {what}"))?;
            if n < low || n > high {
                return Err(format!(
                    "{what} has to be between {low} and {high}, or `off`"
                ));
            }
            Ok(Some(n))
        }
    }
}

/// Read a chat line.
///
/// `Err` is a complaint to show the person who typed it, and nothing is sent
/// or done: a mistyped command must not land in the team's chat as if it had
/// been said on purpose.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let text = line.trim_end();
    let Some(rest) = text.strip_prefix('/') else {
        return Ok(Parsed::Say(line.to_string()));
    };
    // `//` is the escape, and `/ ` was never a command.
    if let Some(literal) = rest.strip_prefix('/') {
        return Ok(Parsed::Say(format!("/{literal}")));
    }
    if rest.starts_with(char::is_whitespace) || rest.is_empty() {
        return Ok(Parsed::Say(line.to_string()));
    }

    let mut words = rest.split_whitespace();
    let verb = words.next().unwrap_or("").to_lowercase();
    let args: Vec<&str> = words.collect();
    let one = |what: &str| -> Result<String, String> {
        args.first()
            .map(|w| w.to_string())
            .ok_or_else(|| format!("/{verb} needs {what}. {}", usage(&verb)))
    };

    match verb.as_str() {
        "help" | "?" | "commands" => Ok(Parsed::Do(Cmd::Help)),
        // Answering a prompt from the console, which is where an operator
        // watching a terminal already is.
        "allow" | "approve" | "yes" => Ok(Parsed::Do(Cmd::Decide {
            who: args.first().map(|s| s.to_string()),
            allow: true,
        })),
        "deny" | "reject" | "no" => Ok(Parsed::Do(Cmd::Decide {
            who: args.first().map(|s| s.to_string()),
            allow: false,
        })),
        "invite" | "guest" => {
            let who = one("an agent")?;
            Ok(Parsed::Do(Cmd::Guest { who, join: true }))
        }
        "uninvite" | "release" => {
            let who = one("an agent")?;
            Ok(Parsed::Do(Cmd::Guest { who, join: false }))
        }
        "compact" | "squash" => {
            let who = one("an agent")?;
            Ok(Parsed::Do(Cmd::Compact { who }))
        }
        "fold" => {
            let who = one("an agent")?;
            Ok(Parsed::Do(Cmd::Fold { who }))
        }
        // The REST OF THE LINE, not words: these are sentences somebody
        // wrote, and splitting and rejoining them on whitespace would
        // quietly reflow what the team is told.
        "rules" | "team-rules" | "teamrules" => {
            let text = rest
                .splitn(2, char::is_whitespace)
                .nth(1)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            let text = match text.as_deref() {
                // `off`/`clear` is how every other setting here is cleared,
                // and somebody typing it means it rather than meaning to
                // make it the team's only rule.
                Some("off") | Some("clear") | Some("none") => Some(String::new()),
                _ => text,
            };
            Ok(Parsed::Do(Cmd::Rules { text }))
        }
        "compact-at" | "compactat" => {
            let who = one("an agent, or 'session'")?;
            let pct = policy_number(args.get(1).copied(), "a percentage", 1, 100)?;
            Ok(Parsed::Do(Cmd::CompactAt { who, pct }))
        }
        "flood" | "rate" => {
            let who = one("an agent, or 'session'")?;
            // Two numbers, in order, either of which may be `-` to leave
            // alone: `/flood Ida 30 20`, `/flood Ida - 20`, `/flood Ida off`.
            let first = args.get(1).copied();
            let second = args.get(2).copied();
            if first
                .map(|a| matches!(a.to_lowercase().as_str(), "off" | "no" | "none" | "never"))
                .unwrap_or(false)
            {
                return Ok(Parsed::Do(Cmd::Flood {
                    who,
                    per_min: Some(0),
                    per_pair: Some(0),
                }));
            }
            if first.is_none() {
                return Err("say how many messages a minute, then optionally how many \
                            between one pair in ten minutes \u{2014} or `off`, or \
                            `default`"
                    .to_string());
            }
            // `-` means "leave that one as it is", which is the only way to
            // set the pair limit without restating the rate.
            fn given<'a>(a: Option<&'a str>) -> Option<&'a str> {
                a.filter(|x| *x != "-" && !x.is_empty())
            }
            let per_min = match given(first) {
                None => None,
                a => policy_number(a, "a number of messages a minute", 1, 1000)?,
            };
            let per_pair = match given(second) {
                None => None,
                a => policy_number(a, "a number of messages for one pair", 2, 1000)?,
            };
            Ok(Parsed::Do(Cmd::Flood { who, per_min, per_pair }))
        }
        "loop-guard" | "loopguard" | "loop" => {
            let who = one("an agent, or 'session'")?;
            // Two is the floor: one "repeat" is a single turn, and a guard
            // that fires on that stops every agent the moment it is set.
            let repeats = policy_number(args.get(1).copied(), "a number of repeats", 2, 99)?;
            Ok(Parsed::Do(Cmd::LoopGuard { who, repeats }))
        }
        "remind-memory" | "remindmemory" | "remind-graph" | "remindgraph" => {
            let which = if verb.contains("graph") { "remind_graph" } else { "remind_memory" };
            let who = one("an agent, or 'session'")?;
            let on = match args.get(1).map(|a| a.to_lowercase()) {
                Some(a) if matches!(a.as_str(), "on" | "yes" | "true") => Some(true),
                Some(a) if matches!(a.as_str(), "off" | "no" | "false" | "never") => Some(false),
                Some(a) if matches!(a.as_str(), "default" | "inherit" | "clear") => None,
                _ => return Err(format!("/{verb} {who} on, off or default")),
            };
            Ok(Parsed::Do(Cmd::Remind { who, which, on }))
        }
        "autorespawn" | "respawn" => {
            let who = one("an agent")?;
            let off = args[1..]
                .iter()
                .any(|a| matches!(a.to_lowercase().as_str(), "off" | "no" | "none" | "never"));
            Ok(Parsed::Do(Cmd::AutoRespawn { who, on: !off }))
        }
        "autoapprove" | "auto" => {
            let who = one("an agent, or 'session'")?;
            let off = args[1..]
                .iter()
                .any(|a| matches!(a.to_lowercase().as_str(), "off" | "no" | "none" | "clear"));
            Ok(Parsed::Do(Cmd::AutoApprove { who, on: !off }))
        }
        // `/elect` reads naturally for promoting someone and `/authority`
        // for inspecting or demoting; they are the same command.
        "authority" | "elect" | "promote" => {
            let who = one("an agent and a level")?;
            let level = args[1..].join(" ").trim().to_lowercase();
            if level.is_empty() {
                return Err(format!(
                    "/{verb} needs a level: agent, orchestrator or super. {}",
                    usage("authority")
                ));
            }
            Ok(Parsed::Do(Cmd::Authority { who, level }))
        }
        "supervisor" | "supervise" => {
            let who = one("an agent")?;
            let off = args[1..]
                .iter()
                .any(|a| matches!(a.to_lowercase().as_str(), "off" | "no" | "none" | "clear"));
            Ok(Parsed::Do(Cmd::Supervisor { who, on: !off }))
        }
        "rename" | "name" => {
            let who = one("an agent and a new name")?;
            let name = args[1..].join(" ");
            if name.trim().is_empty() {
                return Err(format!("/{verb} needs a new name. {}", usage(&verb)));
            }
            Ok(Parsed::Do(Cmd::Rename { who, name: name.trim().to_string() }))
        }
        "move" => {
            let who = one("an agent and a session")?;
            let session = args.get(1).map(|s| s.trim_start_matches('#').to_string());
            match session.filter(|s| !s.is_empty()) {
                Some(session) => Ok(Parsed::Do(Cmd::Move { who, session })),
                None => Err(format!("/move needs a session. {}", usage("move"))),
            }
        }
        "remove" | "rm" | "delete" => {
            let who = one("an agent")?;
            let force = args[1..]
                .iter()
                .any(|a| matches!(a.to_lowercase().as_str(), "force" | "--force" | "-f"));
            Ok(Parsed::Do(Cmd::Remove { who, force }))
        }
        "start" | "spawn" | "launch" => {
            let who = one("an agent")?;
            Ok(Parsed::Do(Cmd::Start { who, mode: mode_of(args.get(1).copied())? }))
        }
        "voice" | "speaks" => {
            let who = one("an agent and a voice")?;
            let voice = args[1..].join(" ");
            if voice.trim().is_empty() {
                return Err(format!("/{verb} needs a voice. {}", usage("voice")));
            }
            // `none` is how a setting is taken off: an empty argument is one
            // the parser cannot see.
            let voice = match voice.trim().to_lowercase().as_str() {
                "none" | "default" | "off" => String::new(),
                _ => voice.trim().to_string(),
            };
            Ok(Parsed::Do(Cmd::Voice { who, voice }))
        }
        "owns" | "own" | "assign" => {
            let who = one("an agent and what it owns")?;
            if args.len() < 2 {
                return Err(format!(
                    "/{verb} needs at least one pattern. {}",
                    usage("owns")
                ));
            }
            // A leading + on the first pattern means "as well as", which is
            // how you give somebody a second container without having to
            // repeat the first.
            let add = args[1].starts_with('+');
            let patterns: Vec<String> = args[1..]
                .iter()
                .map(|p| p.trim_start_matches('+').to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if patterns.is_empty() {
                return Err(format!("/{verb} needs at least one pattern. {}", usage("owns")));
            }
            Ok(Parsed::Do(Cmd::Owns { who, patterns, add }))
        }
        "new" | "seat" => {
            let cli = one("a cli and a name")?;
            let name = args
                .get(1)
                .map(|s| s.to_string())
                .ok_or_else(|| format!("/{verb} needs a name. {}", usage("new")))?;
            Ok(Parsed::Do(Cmd::New { cli, name, mode: mode_of(args.get(2).copied())? }))
        }
        other => Err(format!(
            "there is no /{other}. {}",
            help().lines().next().unwrap_or_default()
        )),
    }
}

/// The modes a seat can be started in, and nothing else: a typo here would
/// otherwise be refused much later by the node, in a message about a field.
fn mode_of(given: Option<&str>) -> Result<Option<String>, String> {
    match given.map(|m| m.trim().to_lowercase()) {
        None => Ok(None),
        Some(m) if m.is_empty() => Ok(None),
        Some(m) if m == "interactive" || m == "headless" => Ok(Some(m)),
        Some(other) => Err(format!(
            "'{other}' is not a mode: say interactive or headless, or leave it out"
        )),
    }
}

fn usage(verb: &str) -> String {
    let line = match verb {
        "rename" | "name" => "/rename <agent> <new name>",
        "move" => "/move <agent> <session>",
        "remove" | "rm" | "delete" => "/remove <agent> [force]",
        "start" | "spawn" | "launch" => "/start <agent> [interactive|headless]",
        "new" | "seat" => "/new <cli> <name> [interactive|headless]",
        "owns" | "own" | "assign" => "/owns <agent> <pattern>... (prefix + to add)",
        "voice" | "speaks" => "/voice <agent> <voice|none>",
        "authority" | "elect" | "promote" =>
            "/authority <agent> <observer|agent|orchestrator|super>",
        "allow" | "approve" | "yes" | "deny" | "reject" | "no" => "/allow [agent]  |  /deny [agent]",
        "autoapprove" | "auto" => "/autoapprove <agent|session> [off]",
        "autorespawn" | "respawn" => "/autorespawn <agent> [off]",
        "compact" | "squash" => "/compact <agent>",
        "fold" => "/fold <agent>",
        "rules" | "team-rules" | "teamrules" => "/rules [the team's rules, or 'clear']",
        "compact-at" | "compactat" => "/compact-at <agent|session> <pct|off|default>",
        "flood" | "rate" => "/flood <agent|session> <per-minute|-|off> [per-pair]",
        "loop-guard" | "loopguard" | "loop" => {
            "/loop-guard <agent|session> <repeats|off|default>"
        }
        "invite" | "guest" => "/invite <agent>",
        "uninvite" | "release" => "/uninvite <agent>",
        _ => "/help",
    };
    format!("Usage: {line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_line_is_still_an_ordinary_line() {
        assert_eq!(parse("deploy it").unwrap(), Parsed::Say("deploy it".into()));
        // A lone slash, or a slash with a space, is somebody typing a path
        // or thinking aloud -- not a command that does not exist.
        assert_eq!(parse("/").unwrap(), Parsed::Say("/".into()));
        assert_eq!(parse("/ etc").unwrap(), Parsed::Say("/ etc".into()));
        assert!(matches!(parse("/usr/bin is on PATH"), Err(_)), "that IS a mistyped command");
    }

    #[test]
    fn a_seat_can_be_asked_to_fold_whatever_cli_it_is_on() {
        // A message, not a keystroke, so this is the one way of asking that
        // works on every CLI on the fleet -- including the ones whose
        // conversation nexmesh cannot read and whose fullness it therefore
        // cannot measure.
        assert_eq!(parse("/fold Max").unwrap(), Parsed::Do(Cmd::Fold { who: "Max".into() }));
        let e = parse("/fold").unwrap_err();
        assert!(e.contains("an agent"), "{e}");
        assert!(e.contains("Usage"), "a complaint must say the shape: {e}");
        // And it is in the list, or nobody finds it.
        assert!(help().contains("/fold"), "{}", help());
    }

    #[test]
    fn a_doubled_slash_says_the_line_itself() {
        assert_eq!(parse("//rename is a command").unwrap(),
                   Parsed::Say("/rename is a command".into()));
    }

    #[test]
    fn rename_takes_a_name_with_spaces_in_it() {
        assert_eq!(
            parse("/rename Dev the new dev").unwrap(),
            Parsed::Do(Cmd::Rename { who: "Dev".into(), name: "the new dev".into() })
        );
        let e = parse("/rename Dev").unwrap_err();
        assert!(e.contains("new name"), "{e}");
        assert!(e.contains("Usage"), "a complaint must say the shape: {e}");
    }

    #[test]
    fn move_accepts_the_hash_an_operator_types() {
        assert_eq!(
            parse("/move Dev #nexmesh").unwrap(),
            Parsed::Do(Cmd::Move { who: "Dev".into(), session: "nexmesh".into() })
        );
        assert_eq!(
            parse("/move Dev nexmesh").unwrap(),
            Parsed::Do(Cmd::Move { who: "Dev".into(), session: "nexmesh".into() })
        );
        assert!(parse("/move Dev").is_err());
    }

    #[test]
    fn remove_is_careful_and_force_is_explicit() {
        assert_eq!(
            parse("/remove Dev").unwrap(),
            Parsed::Do(Cmd::Remove { who: "Dev".into(), force: false })
        );
        for word in ["force", "--force", "-f"] {
            assert_eq!(
                parse(&format!("/remove Dev {word}")).unwrap(),
                Parsed::Do(Cmd::Remove { who: "Dev".into(), force: true })
            );
        }
    }

    #[test]
    fn a_mode_is_one_of_two_words_or_none_at_all() {
        assert_eq!(
            parse("/start Dev").unwrap(),
            Parsed::Do(Cmd::Start { who: "Dev".into(), mode: None })
        );
        assert_eq!(
            parse("/start Dev headless").unwrap(),
            Parsed::Do(Cmd::Start { who: "Dev".into(), mode: Some("headless".into()) })
        );
        let e = parse("/start Dev quickly").unwrap_err();
        assert!(e.contains("not a mode"), "{e}");
    }

    #[test]
    fn new_needs_a_cli_and_a_name() {
        assert_eq!(
            parse("/new claude Dev interactive").unwrap(),
            Parsed::Do(Cmd::New {
                cli: "claude".into(),
                name: "Dev".into(),
                mode: Some("interactive".into())
            })
        );
        assert!(parse("/new claude").is_err());
        assert!(parse("/new").is_err());
    }

    #[test]
    fn owns_assigns_parts_of_the_workspace() {
        assert_eq!(
            parse("/owns Max verti/** ombia").unwrap(),
            Parsed::Do(Cmd::Owns {
                who: "Max".into(),
                patterns: vec!["verti/**".into(), "ombia".into()],
                add: false
            })
        );
        // A leading + adds instead of replacing.
        assert_eq!(
            parse("/owns Max +team").unwrap(),
            Parsed::Do(Cmd::Owns { who: "Max".into(), patterns: vec!["team".into()], add: true })
        );
        let e = parse("/owns Max").unwrap_err();
        assert!(e.contains("pattern"), "{e}");
    }

    #[test]
    fn the_verbs_have_the_aliases_an_operator_will_try() {
        assert_eq!(parse("/help").unwrap(), Parsed::Do(Cmd::Help));
        assert_eq!(parse("/HELP").unwrap(), Parsed::Do(Cmd::Help));
        assert!(matches!(parse("/rm Dev").unwrap(), Parsed::Do(Cmd::Remove { .. })));
        assert!(matches!(parse("/spawn Dev").unwrap(), Parsed::Do(Cmd::Start { .. })));
        assert!(matches!(parse("/name Dev x").unwrap(), Parsed::Do(Cmd::Rename { .. })));
        assert!(matches!(parse("/seat claude Dev").unwrap(), Parsed::Do(Cmd::New { .. })));
    }

    #[test]
    fn the_help_lists_every_command_the_parser_takes() {
        let h = help();
        for verb in ["/rename", "/move", "/remove", "/start", "/new", "/owns", "/help"] {
            assert!(h.contains(verb), "{verb} missing from the help");
        }
    }
}
