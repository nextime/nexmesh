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
     \u{2022} /help\n\
     An agent is named by its name, its role or its seat id. Say something \
     starting with a slash by doubling it: //like this."
        .to_string()
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
            parse("/owns Max +digesta").unwrap(),
            Parsed::Do(Cmd::Owns { who: "Max".into(), patterns: vec!["digesta".into()], add: true })
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
