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
//! A chat has messages; an attached terminal has a byte stream, and asking it
//! to "read the last message" means deciding where one ended. The boundary
//! that matches what a person means is **their own Enter**: everything the
//! agent printed since you last submitted something is its answer to that.
//! It survives a TUI redrawing its whole screen, which a blank-line rule does
//! not, and it needs nothing from the CLI.
//!
//! From there, selecting one is a cursor: the newest reply, then the one
//! before it, and so on. Anything new puts the cursor back at the newest,
//! because a reply that arrives while you are reading an old one is the thing
//! you actually wanted.

use std::collections::VecDeque;

/// How many replies to keep. Enough to walk back through a conversation,
/// bounded so a long-lived attach does not grow for ever.
const KEEP: usize = 24;

/// How much of one reply to keep. A `cat` of a large file is not a reply
/// anybody wants read out, and the tail is the part that matters.
const MAX_REPLY: usize = 6_000;

#[derive(Debug, Default)]
pub struct Replies {
    done: VecDeque<String>,
    /// What the agent has printed since the last Enter.
    current: String,
    /// 0 is the newest reply, 1 the one before it.
    cursor: usize,
}

impl Replies {
    /// Terminal output, already stripped of escape sequences.
    pub fn saw(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if !self.current.is_empty() && !self.current.ends_with(' ') {
            self.current.push(' ');
        }
        self.current.push_str(text.trim());
        if self.current.len() > MAX_REPLY {
            let cut = self.current.len() - MAX_REPLY;
            let at = self
                .current
                .char_indices()
                .map(|(i, _)| i)
                .find(|i| *i >= cut)
                .unwrap_or(0);
            self.current = self.current[at..].to_string();
        }
        // Something new is the thing you wanted read.
        self.cursor = 0;
    }

    /// The operator pressed Enter: whatever the agent printed up to now was
    /// its answer to the last thing, and what comes next answers this one.
    pub fn submitted(&mut self) {
        let text = std::mem::take(&mut self.current);
        let text = text.trim().to_string();
        if !text.is_empty() {
            self.done.push_back(text);
            while self.done.len() > KEEP {
                self.done.pop_front();
            }
        }
        self.cursor = 0;
    }

    /// The newest reply: what is being printed now, or the last complete one.
    pub fn latest(&self) -> Option<&str> {
        let live = self.current.trim();
        if !live.is_empty() {
            return Some(live);
        }
        self.done.back().map(|s| s.as_str())
    }

    /// The one the cursor is on, counting back from the newest.
    pub fn selected(&self) -> Option<&str> {
        if self.cursor == 0 {
            return self.latest();
        }
        // Past the newest: index back through what is finished. When the
        // live one is empty the newest IS the last finished one, so the
        // offset accounts for that rather than reading it twice.
        let live = !self.current.trim().is_empty();
        let back = if live { self.cursor } else { self.cursor + 1 };
        if back > self.done.len() {
            return None;
        }
        self.done.get(self.done.len() - back).map(|s| s.as_str())
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
    pub fn is_empty(&self) -> bool {
        self.done.is_empty() && self.current.trim().is_empty()
    }

    /// For reading aloud as it arrives: the live reply, once it has settled.
    /// Taking it closes it off, so the same words are never read twice.
    pub fn take_settled(&mut self) -> Option<String> {
        let text = self.current.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.current.clear();
        self.done.push_back(text.clone());
        while self.done.len() > KEEP {
            self.done.pop_front();
        }
        self.cursor = 0;
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_what_arrived_since_you_last_pressed_enter() {
        let mut r = Replies::default();
        assert!(r.is_empty());
        r.saw("thinking");
        r.saw("the answer is 42");
        assert_eq!(r.latest(), Some("thinking the answer is 42"));
        r.submitted(); // you type the next thing
        r.saw("and the second answer");
        assert_eq!(r.latest(), Some("and the second answer"));
        // The one before it is still reachable.
        assert!(r.older());
        assert_eq!(r.selected(), Some("thinking the answer is 42"));
    }

    #[test]
    fn walking_back_stops_at_the_oldest_rather_than_going_silent() {
        let mut r = Replies::default();
        for n in 1..=3 {
            r.saw(&format!("reply {n}"));
            r.submitted();
        }
        // Nothing live: the newest is the last finished one.
        assert_eq!(r.selected(), Some("reply 3"));
        assert!(r.older());
        assert_eq!(r.selected(), Some("reply 2"));
        assert!(r.older());
        assert_eq!(r.selected(), Some("reply 1"));
        assert!(!r.older(), "there is no fourth one back");
        assert_eq!(r.selected(), Some("reply 1"), "and the cursor did not move");
        r.newest();
        assert_eq!(r.selected(), Some("reply 3"));
    }

    #[test]
    fn anything_new_puts_the_cursor_back_at_the_newest() {
        let mut r = Replies::default();
        r.saw("one");
        r.submitted();
        r.saw("two");
        r.submitted();
        assert!(r.older());
        assert_eq!(r.back(), 1);
        r.saw("three");
        assert_eq!(r.back(), 0, "a new reply is the one you wanted");
        assert_eq!(r.selected(), Some("three"));
    }

    #[test]
    fn reading_aloud_as_it_arrives_never_repeats_itself() {
        let mut r = Replies::default();
        assert_eq!(r.take_settled(), None, "nothing to say yet");
        r.saw("the first answer");
        assert_eq!(r.take_settled().as_deref(), Some("the first answer"));
        assert_eq!(r.take_settled(), None, "and not twice");
        r.saw("the second");
        assert_eq!(r.take_settled().as_deref(), Some("the second"));
        // Both are still there to walk back through.
        assert_eq!(r.selected(), Some("the second"));
        assert!(r.older());
        assert_eq!(r.selected(), Some("the first answer"));
    }

    #[test]
    fn one_enormous_dump_is_kept_by_its_tail() {
        let mut r = Replies::default();
        r.saw(&"x".repeat(20_000));
        let live = r.latest().unwrap();
        assert!(live.len() <= MAX_REPLY + 8, "{}", live.len());
        assert!(live.ends_with('x'));
    }

    #[test]
    fn blank_output_is_not_a_reply() {
        let mut r = Replies::default();
        r.saw("   ");
        r.saw("\n");
        assert!(r.is_empty());
        assert_eq!(r.latest(), None);
    }
}
