// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! What an agent just SAID, out of a screen it also draws itself on.
//!
//! "Read that back to me" is a question about one reply. A terminal running
//! an agent CLI is not a transcript of replies: it is a screen, with the
//! reply on it somewhere between an input box, a status row, a spinner, a
//! list of files the agent read and whatever the CLI wants to say about
//! updates. Reading the screen aloud reads all of that -- which is what the
//! dashboard did, line by line, with a full stop inserted between every
//! line, so a wrapped paragraph came out as a dozen one-word sentences and
//! then recited the input box.
//!
//! So the screen is sorted into two kinds of line:
//!
//! * the agent's own words, which are what somebody asked to hear;
//! * the CLI's FURNITURE -- boxes, rules, prompts, spinners, counters, tool
//!   calls and their output -- which is there to be looked at, not listened
//!   to.
//!
//! Then the reply is the last run of the first kind, read in screen order.
//!
//! ## Why this is a table and not a parser
//!
//! Every CLI draws its own furniture, each one renames its glyphs between
//! releases, and none of them has a machine-readable "this is my answer"
//! marker. Keeping the furniture as DATA means a CLI whose spinner changes
//! is a one-line edit here, in the same spirit as `clis/*.toml` and the
//! language table in the code graph. The generic set alone already does
//! something sensible for a CLI nobody has described yet, because every one
//! of them draws boxes and none of them says "esc to interrupt" in prose.
//!
//! Nothing here is specific to speech: the same extraction answers "what did
//! it say" for a notification or a log line. Speech is simply where getting
//! it wrong is unmissable.

/// What a CLI draws around its own words.
#[derive(Debug, Clone, Default)]
pub struct Chrome {
    /// A line whose first non-blank characters are one of these is furniture.
    pub starts: &'static [&'static str],
    /// A line containing one of these is furniture, wherever it appears.
    pub contains: &'static [&'static str],
    /// A line that marks the start of one of the agent's own turns. Walking
    /// back from the bottom stops HERE, including this line's own words.
    pub says: &'static [&'static str],
    /// A line that ends a reply going backwards without belonging to it: the
    /// operator's own echoed message, or the output of a tool call.
    pub boundary: &'static [&'static str],
}

/// Lines made only of these are a rule or the edge of a box, in any CLI.
const DRAWING: &str = "\u{2500}\u{2501}\u{2502}\u{2503}\u{250c}\u{250d}\u{250e}\u{250f}\
\u{2510}\u{2511}\u{2512}\u{2513}\u{2514}\u{2515}\u{2516}\u{2517}\u{2518}\u{2519}\u{251a}\
\u{251b}\u{251c}\u{2524}\u{252c}\u{2534}\u{253c}\u{2550}\u{2551}\u{2554}\u{2557}\u{255a}\
\u{255d}\u{2560}\u{2563}\u{2566}\u{2569}\u{256c}\u{256d}\u{256e}\u{256f}\u{2570}\u{2571}\
\u{2572}\u{2574}\u{2576}\u{2578}\u{257a}\u{2580}\u{2584}\u{2588}\u{258c}\u{2590}\u{2591}\
\u{2592}\u{2593}\u{25ac}\u{25ad}=-_~\u{00b7}\u{2022}*# ";

/// The furniture every one of these CLIs draws, named once.
const GENERIC: Chrome = Chrome {
    starts: &[
        // An input box, a quoted prompt, a diff gutter.
        "\u{2502}", "\u{2503}", "\u{2551}", "\u{250a}",
        // A spinner or a progress line.
        "\u{2733}", "\u{273d}", "\u{2722}", "\u{2736}", "\u{2734}", "\u{2739}",
        "\u{25cf}", "\u{25cb}", "\u{25d0}", "\u{25d3}", "\u{2807}", "\u{28ff}",
        // A tool call's own output.
        "\u{23bf}", "\u{2514}\u{2500}", "\u{251c}\u{2500}",
        // The CLI announcing itself: an update, a tip, a warning banner.
        "\u{2714}", "\u{2713}", "\u{2717}", "\u{2716}", "\u{26a0}", "\u{1f4a1}",
        // A keyboard hint row.
        "\u{25b6}\u{25b6}", "\u{25b5}\u{25b5}",
    ],
    contains: &[
        "esc to interrupt",
        "for shortcuts",
        "ctrl+c to",
        "ctrl-c to",
        "shift+tab",
        "auto mode on",
        "Restart to update",
        "Update installed",
        "update available",
        "context left",
        "Context left",
        "tokens used",
        "to accept",
        "to reject",
        "bypass permissions",
        "Press up to",
    ],
    says: &["\u{23fa}", "\u{25cf} ", "\u{2022} ", "\u{23f5}"],
    boundary: &[">", "\u{276f}", "\u{23bf}", "\u{2514}\u{2500}", "\u{250a}"],
};

/// claude's own furniture, on top of the generic set.
///
/// `\u{23fa}` (the filled circle) heads each of its own turns, `\u{23bf}`
/// heads the output of a tool call, and `\u{276f}` is its prompt. The status
/// row counts tokens and names a clock, which is why a fingerprint of the
/// screen cannot include it either (see the node's prompt reader).
const CLAUDE: Chrome = Chrome {
    starts: &["\u{276f}", "\u{23bf}", "\u{2733}", "\u{2726}", "\u{273d}"],
    contains: &["Brewed for", "Thinking\u{2026}", "(ctrl+o to", "ctrl+o to see"],
    says: &["\u{23fa}"],
    boundary: &["\u{276f}", "\u{23bf}", ">"],
};

/// codex and the OpenAI-style TUIs: a heavy left bar for its own words, and
/// a bordered composer at the bottom.
const CODEX: Chrome = Chrome {
    starts: &["\u{258c}", "\u{2590}", "\u{2502}"],
    contains: &["send q or ctrl+c", "Esc to interrupt", "tokens used"],
    says: &["\u{2022} ", "codex"],
    boundary: &["user", "\u{2502}"],
};

/// Which table to use for a CLI id. A descriptor id carries a profile --
/// `claude-dev` -- so it is matched by family rather than equality, and
/// anything unknown gets the generic set rather than nothing.
pub fn chrome_for(cli: &str) -> (Chrome, Chrome) {
    let id = cli.trim().to_lowercase();
    let family = if id.starts_with("claude") {
        CLAUDE
    } else if id.starts_with("codex") || id.starts_with("qwen") || id.starts_with("gemini") {
        CODEX
    } else {
        Chrome::default()
    };
    (GENERIC, family)
}

/// Does this line contain `n` drawing characters in a row?
///
/// The space is not one of them here: a gap in a sentence is not a rule.
fn run_of_drawing(text: &str, n: usize) -> bool {
    let mut run = 0usize;
    for c in text.chars() {
        if c != ' ' && DRAWING.contains(c) {
            run += 1;
            if run >= n {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

fn trimmed(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || c == '\u{a0}')
}

/// Is this line part of what the CLI draws, rather than what it said?
pub fn is_chrome(line: &str, cli: &str) -> bool {
    let (generic, family) = chrome_for(cli);
    let text = trimmed(line);
    if text.is_empty() {
        return true;
    }
    // A rule, a border, or a row of dots: nothing in it is a word.
    if text.chars().all(|c| DRAWING.contains(c)) {
        return true;
    }
    // A TITLED RULE is still a rule. claude draws its compaction separators
    // as a line of box characters with a couple of words in the middle, and
    // reading "Email sweep" out of forty dashes is not reading a reply.
    //
    // Found by the RUN of drawing characters rather than by what fraction of
    // the line they are: a sentence padded to the width of a screen is still
    // a sentence, while four box characters in a row are never prose.
    if run_of_drawing(text, 4) {
        return true;
    }
    for set in [&generic, &family] {
        if set.starts.iter().any(|p| text.starts_with(p)) {
            return true;
        }
        if set.contains.iter().any(|p| line.contains(p)) {
            return true;
        }
    }
    false
}

/// Does this line end a reply, going backwards, without being part of it?
fn is_boundary(line: &str, cli: &str) -> bool {
    let (generic, family) = chrome_for(cli);
    let text = trimmed(line);
    [&generic, &family]
        .iter()
        .any(|set| set.boundary.iter().any(|p| text.starts_with(p)))
}

/// Does this line start one of the agent's own turns?
fn is_said(line: &str, cli: &str) -> bool {
    let (generic, family) = chrome_for(cli);
    let text = trimmed(line);
    [&generic, &family]
        .iter()
        .any(|set| set.says.iter().any(|p| text.starts_with(p)))
}

/// Take the marker off the front of a line the agent said.
fn unmarked(line: &str, cli: &str) -> String {
    let (generic, family) = chrome_for(cli);
    let text = trimmed(line);
    for set in [&generic, &family] {
        for m in set.says {
            if let Some(rest) = text.strip_prefix(*m) {
                return trimmed(rest).to_string();
            }
        }
    }
    text.to_string()
}

/// `Bash(ls -la)`, `Read(src/main.rs)`, `Update(Cargo.toml)`: a name in
/// title case, immediately followed by a bracket. Deliberately strict --
/// prose does not start with a capitalised word welded to an open bracket,
/// and anything looser would start eating sentences.
fn looks_like_a_tool_call(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_uppercase() => {}
        _ => return false,
    }
    let mut seen = 1;
    for c in chars {
        if c == '(' {
            return seen > 1;
        }
        if !c.is_alphanumeric() || seen > 24 {
            return false;
        }
        seen += 1;
    }
    false
}

/// THE LAST THING THE AGENT SAID, out of the screen it is sitting on.
///
/// `screen` is the terminal as a person sees it -- lines, no escape
/// sequences -- and `cli` is the descriptor id of whatever is drawing it.
/// The answer is one paragraph of prose, wrapped lines joined back together
/// with a space, ready to be read out or shown.
///
/// Empty when the screen holds nothing but furniture, which is the honest
/// answer for a CLI that has not said anything yet: the caller says so
/// rather than reading the input box aloud to prove it tried.
pub fn last_reply(screen: &str, cli: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let mut kept: Vec<String> = Vec::new();
    // Backwards, because the last reply is at the bottom and everything
    // above it is somebody else's turn.
    for line in lines.iter().rev() {
        if is_chrome(line, cli) {
            // Furniture INSIDE a reply -- a status row that redrew over it,
            // a blank line -- does not end the reply. Furniture before one
            // has started is simply skipped.
            continue;
        }
        if is_said(line, cli) {
            let text = unmarked(line, cli);
            // A tool CALL is marked the same way an answer is -- claude
            // heads both with the same glyph -- so "Bash(ls -la)" would be
            // read out as the last thing the agent said. What it said is
            // above it.
            if looks_like_a_tool_call(&text) {
                if kept.is_empty() {
                    continue;
                }
                break;
            }
            kept.push(text);
            break;
        }
        if is_boundary(line, cli) {
            // The operator's own words, or a tool's output: whatever is
            // above this belongs to a different turn.
            if kept.is_empty() {
                continue;
            }
            break;
        }
        kept.push(trimmed(line).to_string());
    }
    kept.reverse();
    let joined = kept.join(" ");
    // One space where there were many: a screen is padded to its width, and
    // a reader pauses on every one of those.
    let mut out = String::with_capacity(joined.len());
    let mut space = false;
    for c in joined.chars() {
        if c.is_whitespace() || c == '\u{a0}' {
            if !space && !out.is_empty() {
                out.push(' ');
            }
            space = true;
        } else {
            space = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A claude screen as it actually comes off a seat: the reply, then the
    /// things it draws under it. Taken from a live fleet, where pressing the
    /// speaker read every one of these lines aloud.
    const CLAUDE_SCREEN: &str = "\
\u{23fa} One thing is still open, not urgent: the report has never\n\
\u{a0}\u{a0}been sent to the client. That's the two files, recounted and in\n\
\u{a0}\u{a0}line with the new totals.\n\
\n\
  Tell me if you want me to prepare that email.\n\
\n\
\u{2733} Brewed for 7s \u{b7} done 22:16\n\
\u{276f} /compact\n\
  \u{23bf} Compacted (ctrl+o to see full summary)\n\
  \u{23bf} Read ../../../bin/backup.sh (77 lines)\n\
\u{2714} Update installed \u{b7} Restart to update\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Email sweep \u{2500}\n\
\u{276f}\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  \u{25b6}\u{25b6} auto mode on (shift+tab to cycle) \u{b7} 6 agents\n";

    #[test]
    fn the_reply_is_what_it_said_and_not_what_it_draws() {
        let said = last_reply(CLAUDE_SCREEN, "claude");
        assert!(said.starts_with("One thing is still open"), "{said}");
        assert!(said.ends_with("prepare that email."), "{said}");
        // The things that were being read out before.
        for furniture in [
            "Brewed", "/compact", "Compacted", "backup", "Update installed",
            "Email sweep", "auto mode on", "\u{2500}",
        ] {
            assert!(!said.contains(furniture), "{furniture} is furniture: {said}");
        }
    }

    #[test]
    fn a_wrapped_paragraph_comes_back_as_a_paragraph() {
        // The bug this replaces joined every screen line with ". ", so a
        // wrapped sentence was read as "the report has never. been sent".
        let said = last_reply(CLAUDE_SCREEN, "claude");
        assert!(said.contains("has never been sent to the client"), "{said}");
        assert!(!said.contains("  "), "no padding survives: {said}");
    }

    #[test]
    fn a_profile_is_still_the_cli_it_is_a_profile_of() {
        // Seats run `claude-dev`, not `claude`.
        let a = last_reply(CLAUDE_SCREEN, "claude-dev");
        let b = last_reply(CLAUDE_SCREEN, "claude");
        assert_eq!(a, b);
    }

    #[test]
    fn the_operators_own_words_are_not_the_agents_reply() {
        let screen = "\
\u{23fa} The tests pass.\n\
> and now do the other thing\n\
\u{23fa} Done: the other thing is done.\n\
\u{276f}\n";
        let said = last_reply(screen, "claude");
        assert_eq!(said, "Done: the other thing is done.");
    }

    #[test]
    fn a_screen_with_nothing_but_furniture_says_nothing() {
        let screen = "\u{276f}\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{25b6}\u{25b6} auto mode on\n";
        assert_eq!(last_reply(screen, "claude"), "");
    }

    #[test]
    fn a_cli_nobody_has_described_still_loses_its_boxes() {
        // The generic set alone: every one of these draws boxes, and none of
        // them says "esc to interrupt" in prose.
        let screen = "\
\u{2502} some input box \u{2502}\n\
I finished the migration.\n\
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
  press esc to interrupt\n";
        assert_eq!(last_reply(screen, "somethingelse"), "I finished the migration.");
    }

    #[test]
    fn padding_and_punctuation_do_not_make_a_sentence_furniture() {
        // A screen is padded to its width, and prose has dashes in it.
        let screen = "\
\u{23fa} The migration is done \u{2014} two tables, no data loss.\n\
  Next: the index, which takes a while.\n\
\u{276f}\n";
        let said = last_reply(screen, "claude");
        assert!(said.contains("two tables, no data loss"), "{said}");
        assert!(said.contains("Next: the index"), "{said}");
    }

    #[test]
    fn a_tool_calls_output_is_not_the_reply() {
        let screen = "\
\u{23fa} Running the suite now.\n\
  \u{23bf} 41 suites passed\n\
  \u{23bf} EXIT=0\n\
\u{276f}\n";
        // What it SAID was that it was running the suite; the numbers are
        // the tool's output, which is on screen to be looked at.
        let said = last_reply(screen, "claude");
        assert_eq!(said, "Running the suite now.");
    }

    #[test]
    fn a_tool_call_is_not_the_last_thing_it_said() {
        // claude heads a tool call with the same glyph as an answer, so
        // without this "Bash(cargo test)" is what gets read out.
        let screen = "\
\u{23fa} Running the suite now; I will report what fails.\n\
\u{23fa} Bash(cargo test --workspace)\n\
  \u{23bf} 910 passed\n\
\u{276f}\n";
        assert_eq!(
            last_reply(screen, "claude"),
            "Running the suite now; I will report what fails."
        );
    }

    #[test]
    fn a_sentence_that_merely_mentions_brackets_is_still_a_sentence() {
        let screen = "\u{23fa} Done (both tests pass).\n\u{276f}\n";
        assert_eq!(last_reply(screen, "claude"), "Done (both tests pass).");
    }

    #[test]
    fn several_lines_of_prose_with_no_marker_at_all_still_come_back() {
        // A CLI that does not mark its turns: everything above the prompt
        // that is not furniture is what it said.
        let screen = "I looked at the file.\nIt needs two changes.\n\u{276f}\n";
        assert_eq!(
            last_reply(screen, "claude"),
            "I looked at the file. It needs two changes."
        );
    }
}
