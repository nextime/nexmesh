// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! What counts as "a reply" on a terminal, so one can be read aloud.
//!
//! A chat has messages; an attached terminal has a byte stream, and asking
//! it to "read the last message" means deciding where one ended. Two things
//! decide it, and the first one alone was not enough.
//!
//! **Where one reply ends and the next begins** is the operator's own
//! Enter: everything the agent printed since you last submitted something
//! is its answer to that. It needs nothing from the CLI, and it survives a
//! CLI that redraws its whole screen -- which a blank-line rule does not.
//!
//! **Which of those bytes are words** is the part this got wrong. An agent
//! CLI is a full-screen program: it repaints its spinner several times a
//! second, repaints the input box on every keystroke, and repaints the whole
//! frame on a resize. Everything it has ever drawn arrives in the stream, so
//! a reply accumulated from the stream is the reply PLUS the spinner forty
//! times, the box, the status row, the token counter and the line about an
//! update being available. Read aloud, that is what you hear.
//!
//! So lines are kept rather than bytes: the furniture is dropped as it
//! arrives (`nexmesh_proto::screenread`, which the dashboard shares), a line
//! identical to one already in this reply is a repaint rather than a
//! sentence, and what is read out is the agent's own words out of what is
//! left.
//!
//! From there, selecting one is a cursor: the newest reply, then the one
//! before it, and so on. Anything new puts the cursor back at the newest,
//! because a reply that arrives while you are reading an old one is the thing
//! you actually wanted.

use std::collections::VecDeque;

/// How many replies to keep. Enough to walk back through a conversation,
/// bounded so a long-lived attach does not grow for ever.
const KEEP: usize = 24;

/// How many lines of one reply to keep. A `cat` of a large file is not a
/// reply anybody wants read out, and the tail is the part that matters.
const MAX_LINES: usize = 400;

#[derive(Debug, Default)]
pub struct Replies {
    done: VecDeque<Vec<String>>,
    /// What the agent has printed since the last Enter, a line at a time,
    /// with the CLI's own furniture already out of it.
    current: Vec<String>,
    /// 0 is the newest reply, 1 the one before it.
    cursor: usize,
    /// Which CLI is drawing this screen, so its furniture is known. Empty is
    /// fine: the generic rules still take the boxes and the status rows out.
    cli: String,
}

impl Replies {
    /// Whose screen this is. Known from the seat, so the furniture of the
    /// CLI actually running is the furniture that gets dropped.
    pub fn watching(cli: &str) -> Self {
        Self { cli: cli.to_string(), ..Default::default() }
    }

    /// Terminal output, split into the lines a person would see.
    ///
    /// A line that is the CLI's own furniture never joins a reply, and a
    /// line identical to one already in this reply is a repaint: a spinner
    /// ticking for thirty seconds is one line thirty times over, and read
    /// aloud it is thirty seconds of the same word.
    pub fn saw_lines(&mut self, lines: &[String]) {
        let mut added = false;
        for line in lines {
            if nexmesh_proto::screenread::is_chrome(line, &self.cli) {
                continue;
            }
            let text = line.trim();
            if text.is_empty() || self.current.iter().any(|k| k == text) {
                continue;
            }
            // A LINE THAT ARRIVED IN TWO READS IS ONE LINE.
            //
            // The stream is chopped wherever the network chopped it, so half
            // a sentence can arrive, and then the CLI repaints and the whole
            // sentence arrives behind it. Both kept, that is read out as
            // "The ans The answer is 42". So a kept line that is the start
            // of this one is the fragment, and this one replaces it.
            //
            // Long enough to be a sentence rather than a word: "ok" is the
            // start of "okay, done" and is not a fragment of it.
            const FRAGMENT: usize = 12;
            if let Some(at) = self
                .current
                .iter()
                .position(|k| k.len() >= FRAGMENT && text.starts_with(k.as_str()))
            {
                self.current[at] = text.to_string();
                added = true;
                continue;
            }
            // And the other way round: the whole line came first, and what
            // has just arrived is the start of it drawn again.
            if self
                .current
                .iter()
                .any(|k| text.len() >= FRAGMENT && k.starts_with(text))
            {
                continue;
            }
            self.current.push(text.to_string());
            added = true;
            while self.current.len() > MAX_LINES {
                self.current.remove(0);
            }
        }
        // Something new is the thing you wanted read.
        if added {
            self.cursor = 0;
        }
    }

    /// The words out of a run of lines: see the module doc.
    fn words(&self, lines: &[String]) -> String {
        nexmesh_proto::screenread::last_reply(&lines.join("\n"), &self.cli)
    }

    /// The operator pressed Enter: whatever the agent printed up to now was
    /// its answer to the last thing, and what comes next answers this one.
    pub fn submitted(&mut self) {
        let lines = std::mem::take(&mut self.current);
        if !lines.is_empty() {
            self.done.push_back(lines);
            while self.done.len() > KEEP {
                self.done.pop_front();
            }
        }
        self.cursor = 0;
    }

    /// The newest reply: what is being printed now, or the last complete one.
    pub fn latest(&self) -> Option<String> {
        if !self.current.is_empty() {
            let said = self.words(&self.current);
            if !said.is_empty() {
                return Some(said);
            }
            // Printing, but nothing said yet: a tool's output, a file being
            // listed. The last thing it SAID is still the one before.
            return self.done.back().map(|l| self.words(l)).filter(|s| !s.is_empty());
        }
        self.done.back().map(|l| self.words(l)).filter(|s| !s.is_empty())
    }

    /// The one the cursor is on, counting back from the newest.
    pub fn selected(&self) -> Option<String> {
        if self.cursor == 0 {
            return self.latest();
        }
        // Past the newest: index back through what is finished. When the
        // live one has nothing said in it the newest IS the last finished
        // one, so the offset accounts for that rather than reading it twice.
        let live = self.latest_is_live();
        let back = if live { self.cursor } else { self.cursor + 1 };
        if back > self.done.len() {
            return None;
        }
        self.done
            .get(self.done.len() - back)
            .map(|l| self.words(l))
            .filter(|s| !s.is_empty())
    }

    /// Whether what is being printed right now has anything said in it.
    fn latest_is_live(&self) -> bool {
        !self.current.is_empty() && !self.words(&self.current).is_empty()
    }

    /// Step one reply further back, if there is one. Says whether it moved.
    pub fn older(&mut self) -> bool {
        let next = self.cursor + 1;
        let was = self.cursor;
        self.cursor = next;
        if self.selected().is_none() {
            self.cursor = was;
            return false;
        }
        true
    }

    /// Back to the newest. Used when something else resets the selection --
    /// and by the tests, which is why it is allowed to look unused.
    #[allow(dead_code)]
    pub fn newest(&mut self) {
        self.cursor = 0;
    }

    /// How far back the cursor is, for the status bar.
    pub fn back(&self) -> usize {
        self.cursor
    }

    /// Nothing has been said by anybody yet.
    #[allow(dead_code)]
    /// The newest link the agent has printed, across everything kept.
    ///
    /// An attached terminal is a place links go past: an agent says where
    /// it published something, or what it was reading, and in a terminal
    /// inside tmux inside an attach there is no click that reaches a
    /// browser. So the viewer finds the link itself.
    pub fn newest_link(&self) -> Option<String> {
        // Newest first: the current reply, then backwards through the kept
        // ones. Within a reply, the LAST link, because that is the one most
        // recently printed. A URL is looked for in EVERY line, furniture
        // included -- a CLI that prints a link in its own status row has
        // still printed a link.
        std::iter::once(&self.current)
            .chain(self.done.iter().rev())
            .find_map(|lines| last_url(&lines.join(" ")))
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.current.is_empty()
    }

    /// For reading aloud as it arrives: the live reply, once it has settled.
    /// Taking it closes it off, so the same words are never read twice.
    pub fn take_settled(&mut self) -> Option<String> {
        if self.current.is_empty() {
            return None;
        }
        let lines = std::mem::take(&mut self.current);
        let said = self.words(&lines);
        self.done.push_back(lines);
        while self.done.len() > KEEP {
            self.done.pop_front();
        }
        self.cursor = 0;
        // Nothing SAID in it -- a tool's output, a file listing -- is not
        // something to read out. It is still kept, so walking back through
        // the conversation reaches what came before it.
        (!said.is_empty()).then_some(said)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as they arrive off a terminal: the caller has already split
    /// them and taken the escape sequences out.
    fn saw(r: &mut Replies, lines: &[&str]) {
        let owned: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        r.saw_lines(&owned);
    }

    #[test]
    fn a_reply_is_what_arrived_since_you_last_pressed_enter() {
        let mut r = Replies::watching("claude");
        assert!(r.is_empty());
        saw(&mut r, &["\u{23fa} thinking about it"]);
        saw(&mut r, &["the answer is 42"]);
        assert_eq!(r.latest().as_deref(), Some("thinking about it the answer is 42"));
        r.submitted(); // you type the next thing
        saw(&mut r, &["\u{23fa} and the second answer"]);
        assert_eq!(r.latest().as_deref(), Some("and the second answer"));
        // The one before it is still reachable.
        assert!(r.older());
        assert_eq!(r.selected().as_deref(), Some("thinking about it the answer is 42"));
    }

    #[test]
    fn walking_back_stops_at_the_oldest_rather_than_going_silent() {
        let mut r = Replies::watching("claude");
        for n in 1..=3 {
            saw(&mut r, &[&format!("reply {n}")]);
            r.submitted();
        }
        // Nothing live: the newest is the last finished one.
        assert_eq!(r.selected().as_deref(), Some("reply 3"));
        assert!(r.older());
        assert_eq!(r.selected().as_deref(), Some("reply 2"));
        assert!(r.older());
        assert_eq!(r.selected().as_deref(), Some("reply 1"));
        assert!(!r.older(), "there is no fourth one back");
        assert_eq!(r.selected().as_deref(), Some("reply 1"), "and the cursor did not move");
        r.newest();
        assert_eq!(r.selected().as_deref(), Some("reply 3"));
    }

    #[test]
    fn anything_new_puts_the_cursor_back_at_the_newest() {
        let mut r = Replies::watching("claude");
        saw(&mut r, &["one"]);
        r.submitted();
        saw(&mut r, &["two"]);
        r.submitted();
        assert!(r.older());
        assert_eq!(r.back(), 1);
        saw(&mut r, &["three"]);
        assert_eq!(r.back(), 0, "a new reply is the one you wanted");
        assert_eq!(r.selected().as_deref(), Some("three"));
    }

    #[test]
    fn reading_aloud_as_it_arrives_never_repeats_itself() {
        let mut r = Replies::watching("claude");
        assert_eq!(r.take_settled(), None, "nothing to say yet");
        saw(&mut r, &["the first answer"]);
        assert_eq!(r.take_settled().as_deref(), Some("the first answer"));
        assert_eq!(r.take_settled(), None, "and not twice");
        saw(&mut r, &["the second"]);
        assert_eq!(r.take_settled().as_deref(), Some("the second"));
        // Both are still there to walk back through.
        assert_eq!(r.selected().as_deref(), Some("the second"));
        assert!(r.older());
        assert_eq!(r.selected().as_deref(), Some("the first answer"));
    }

    #[test]
    fn a_spinner_ticking_for_a_minute_is_not_a_minute_of_speech() {
        // The fault this was all rewritten for: a full-screen CLI repaints,
        // the whole stream arrives, and what was read aloud was the reply
        // plus its spinner forty times plus the input box.
        let mut r = Replies::watching("claude");
        for _ in 0..40 {
            saw(&mut r, &[
                "\u{23fa} Deploying the hub now.",
                "\u{2733} Brewed for 7s \u{b7} done 22:16",
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                "\u{276f} ",
                "  \u{25b6}\u{25b6} auto mode on (shift+tab to cycle)",
            ]);
        }
        let said = r.latest().unwrap();
        assert_eq!(said, "Deploying the hub now.", "{said}");
    }

    #[test]
    fn the_box_the_prompt_is_drawn_in_is_never_part_of_a_reply() {
        let mut r = Replies::watching("claude");
        saw(&mut r, &[
            "\u{23fa} Two files changed.",
            "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}",
            "\u{2502} > what next \u{2502}",
            "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}",
        ]);
        assert_eq!(r.latest().as_deref(), Some("Two files changed."));
    }

    #[test]
    fn half_a_line_and_then_the_whole_line_is_one_line() {
        // The stream is chopped wherever the network chopped it, and then
        // the CLI repaints: both kept, it reads as "The ans The answer is".
        let mut r = Replies::watching("claude");
        saw(&mut r, &["\u{23fa} The answer is fo"]);
        saw(&mut r, &["\u{23fa} The answer is forty-two, give or take."]);
        assert_eq!(r.latest().as_deref(), Some("The answer is forty-two, give or take."));
        // And in the other order, which is what a resize does.
        let mut r2 = Replies::watching("claude");
        saw(&mut r2, &["\u{23fa} The answer is forty-two, give or take."]);
        saw(&mut r2, &["\u{23fa} The answer is fo"]);
        assert_eq!(r2.latest().as_deref(), Some("The answer is forty-two, give or take."));
    }

    #[test]
    fn a_short_line_is_not_treated_as_a_fragment_of_a_longer_one() {
        let mut r = Replies::watching("claude");
        saw(&mut r, &["ok", "okay, done"]);
        let said = r.latest().unwrap();
        assert!(said.contains("ok okay, done"), "{said}");
    }

    #[test]
    fn a_reply_with_nothing_but_tool_output_in_it_reads_the_one_before() {
        // Pressing the read key while the agent is running a command should
        // not read out a file listing; the last thing it SAID is the answer.
        let mut r = Replies::watching("claude");
        saw(&mut r, &["\u{23fa} Running the suite."]);
        r.submitted();
        saw(&mut r, &["  \u{23bf} 41 suites passed", "  \u{23bf} EXIT=0"]);
        assert_eq!(r.latest().as_deref(), Some("Running the suite."));
    }

    #[test]
    fn one_enormous_dump_is_kept_by_its_tail() {
        let mut r = Replies::watching("claude");
        let lines: Vec<String> = (0..MAX_LINES * 2).map(|n| format!("line {n}")).collect();
        r.saw_lines(&lines);
        let live = r.latest().unwrap();
        assert!(live.contains(&format!("line {}", MAX_LINES * 2 - 1)), "the tail is kept");
        assert!(!live.contains("line 0 "), "and the head is not");
    }

    #[test]
    fn blank_output_is_not_a_reply() {
        let mut r = Replies::watching("claude");
        saw(&mut r, &["   ", ""]);
        assert!(r.is_empty());
        assert_eq!(r.latest(), None);
    }
}

/// The last http(s) URL in a piece of text, if there is one.
///
/// Deliberately simple: a terminal's idea of a URL is whatever somebody
/// printed, and anything cleverer starts eating the punctuation around it.
fn last_url(text: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for (i, _) in text.match_indices("http") {
        let rest = &text[i..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`' || c == '<')
            .unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ',', ')', ']', '}', ';', ':']);
        if url.len() > 10 {
            found = Some(url.to_string());
        }
    }
    found
}

#[cfg(test)]
mod link_tests {
    use super::*;

    #[test]
    fn the_newest_link_is_the_one_opened() {
        let mut r = Replies::watching("claude");
        r.saw_lines(&["published at https://example.com/one and also http://example.com/two."
            .to_string()]);
        r.submitted();
        r.saw_lines(&["see https://example.net/three, it is the new one".to_string()]);
        assert_eq!(r.newest_link().as_deref(), Some("https://example.net/three"));
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_a_url() {
        assert_eq!(last_url("go to https://example.com/x.").as_deref(),
                   Some("https://example.com/x"));
        assert_eq!(last_url("(https://example.com/y)").as_deref(),
                   Some("https://example.com/y"));
        assert_eq!(last_url("nothing here").as_deref(), None);
        // A bare host is not a link: the terminal would not have made one
        // either, and guessing produces a browser window for `node.js`.
        assert_eq!(last_url("www.example.com").as_deref(), None);
    }
}
