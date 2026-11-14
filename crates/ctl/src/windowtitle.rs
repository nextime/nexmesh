// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! The window title, for as long as an attach owns the terminal.
//!
//! The title is what a window says about itself when it is not the window in
//! front of you: in a taskbar, in a window switcher, in a tab strip. An agent
//! CLI sets it to something true about ITSELF -- the conversation id, the
//! directory it was started in -- which is no help at all when the question
//! being asked of the title is "which seat is this?". Ten windows then say the
//! same thing, or say things that cannot be told apart.
//!
//! So an attach sets the title to `session:agent`, which is exactly how every
//! other part of nexmesh addresses that seat, and keeps it: the agent's own
//! attempts to set the title are taken out of the stream on the way through,
//! rather than fought over. What was there before is pushed onto the
//! terminal's title stack and popped back on the way out.

/// Push the current title onto the terminal's own stack (XTWINOPS 22;2), so
/// leaving can put back what was there without having to know what it was.
pub const PUSH: &[u8] = b"\x1b[22;2t";

/// Pop it back (XTWINOPS 23;2).
pub const POP: &[u8] = b"\x1b[23;2t";

/// OSC 2: set the window title. BEL-terminated, which is understood wherever
/// the sequence is understood at all.
///
/// Control bytes are dropped rather than escaped: a title is one line of text
/// and a seat name that contained an escape sequence would be writing to the
/// terminal, not naming itself.
pub fn set(title: &str) -> Vec<u8> {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x07").into_bytes()
}

/// How a seat is named in a window title: the same `session:agent` that every
/// command, message and log line uses.
pub fn of(session: &str, agent: &str) -> String {
    format!("{session}:{agent}")
}

#[derive(Default, PartialEq, Debug)]
enum St {
    #[default]
    Normal,
    /// Saw ESC; it may be the start of an OSC.
    Esc,
    /// Inside `ESC ] <digits>`, deciding whether this one sets a title.
    Osc,
    /// A title sequence: dropped, up to and including its terminator.
    Drop,
    /// ESC inside a sequence being dropped -- `ESC \` ends it.
    DropEsc,
    /// Some other OSC: passed through, up to and including its terminator.
    Pass,
    /// ESC inside a sequence being passed through.
    PassEsc,
}

/// Takes window-title sequences out of a byte stream.
///
/// The stream arrives in whatever chunks the network gives us, and a sequence
/// can be split across any two of them, so the state and the bytes held back
/// so far both survive between calls.
#[derive(Default)]
pub struct Strip {
    st: St,
    /// Bytes that may yet turn out to be the start of a title sequence. They
    /// are emitted if it turns out not to be one, and dropped if it is.
    held: Vec<u8>,
}

impl Strip {
    /// Everything in `data` that is not part of a title sequence.
    pub fn pass(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            match self.st {
                St::Normal => {
                    if b == 0x1b {
                        self.st = St::Esc;
                        self.held.push(b);
                    } else {
                        out.push(b);
                    }
                }
                St::Esc => {
                    if b == b']' {
                        self.st = St::Osc;
                        self.held.push(b);
                    } else {
                        // Not an OSC: the ESC and this byte are the agent's.
                        out.append(&mut self.held);
                        out.push(b);
                        self.st = St::Normal;
                    }
                }
                St::Osc => {
                    match b {
                        // OSC 0 (icon + title), 1 (icon), 2 (title) are ours
                        // to own. Anything else -- 8 for hyperlinks, 52 for
                        // the clipboard, 7 for the working directory -- is
                        // the agent talking to the terminal about something
                        // that is not the title, and none of our business.
                        b';' => {
                            let num = &self.held[2..];
                            if matches!(num, b"0" | b"1" | b"2") {
                                self.held.clear();
                                self.st = St::Drop;
                            } else {
                                self.held.push(b);
                                out.append(&mut self.held);
                                self.st = St::Pass;
                            }
                        }
                        b'0'..=b'9' if self.held.len() < 6 => self.held.push(b),
                        _ => {
                            self.held.push(b);
                            out.append(&mut self.held);
                            self.st = St::Pass;
                        }
                    }
                }
                St::Drop => match b {
                    0x07 => self.st = St::Normal,
                    0x1b => self.st = St::DropEsc,
                    _ => {}
                },
                St::DropEsc => {
                    // `ESC \` ends it. Anything else was not a terminator, so
                    // the sequence was malformed; stop dropping rather than
                    // swallow the rest of the screen.
                    self.st = St::Normal;
                    if b != b'\\' && b != 0x1b {
                        out.push(b);
                    }
                }
                St::Pass => {
                    out.push(b);
                    match b {
                        0x07 => self.st = St::Normal,
                        0x1b => self.st = St::PassEsc,
                        _ => {}
                    }
                }
                St::PassEsc => {
                    out.push(b);
                    self.st = St::Normal;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through(chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = Strip::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.pass(c));
        }
        out
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(through(&[b"hello\r\n"]), b"hello\r\n");
    }

    #[test]
    fn a_title_is_taken_out() {
        assert_eq!(through(&[b"a\x1b]2;claude -- /working\x07b"]), b"ab");
    }

    #[test]
    fn icon_and_title_forms_too() {
        assert_eq!(through(&[b"\x1b]0;both\x07x"]), b"x");
        assert_eq!(through(&[b"\x1b]1;icon\x07y"]), b"y");
    }

    #[test]
    fn st_terminated_titles_too() {
        assert_eq!(through(&[b"a\x1b]2;title\x1b\\b"]), b"ab");
    }

    #[test]
    fn split_across_chunks() {
        // Every byte on its own: the worst the network can do.
        let input = b"x\x1b]2;gone\x07y";
        let chunks: Vec<&[u8]> = input.iter().map(std::slice::from_ref).collect();
        assert_eq!(through(&chunks), b"xy");
    }

    #[test]
    fn other_oscs_pass_through() {
        // A hyperlink and a clipboard write are not titles.
        let link = b"\x1b]8;;https://example.invalid\x07text\x1b]8;;\x07";
        assert_eq!(through(&[link]), link);
        let clip = b"\x1b]52;c;aGk=\x07";
        assert_eq!(through(&[clip]), clip);
    }

    #[test]
    fn two_digit_oscs_are_not_mistaken_for_titles() {
        // 21 starts with the digit of a title sequence and is not one.
        let osc = b"\x1b]21;x\x07";
        assert_eq!(through(&[osc]), osc);
    }

    #[test]
    fn an_escape_that_is_not_an_osc_is_kept() {
        assert_eq!(through(&[b"\x1b[2Jclear"]), b"\x1b[2Jclear");
        assert_eq!(through(&[b"\x1b[31mred\x1b[0m"]), b"\x1b[31mred\x1b[0m");
    }

    #[test]
    fn a_malformed_title_does_not_eat_the_screen() {
        // ESC inside a title that is not followed by backslash: stop there.
        let out = through(&[b"\x1b]2;half\x1b[0mvisible"]);
        assert_eq!(out, b"[0mvisible");
    }

    #[test]
    fn the_title_we_set_names_the_seat() {
        assert_eq!(of("nexmesh", "Nexmesh-dev"), "nexmesh:Nexmesh-dev");
        assert_eq!(set("nexmesh:Nexmesh-dev"), b"\x1b]2;nexmesh:Nexmesh-dev\x07");
    }

    #[test]
    fn control_bytes_never_reach_the_title() {
        assert_eq!(set("a\x07b\x1bc"), b"\x1b]2;abc\x07");
    }
}
