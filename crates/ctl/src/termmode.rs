// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Keeping one terminal mode out of an attach.
//!
//! ALTERNATE SCROLL (`CSI ? 1007 h`) makes the wheel send cursor keys. With
//! it on, scrolling an agent's terminal walks its input history instead of
//! its scrollback -- Claude Code notices and says "scroll wheel is sending
//! arrow keys, use PgUp/Down", and the only cure anybody finds is `reset`.
//!
//! Turning it off when the attach starts is not enough: things turn it back
//! on in the middle. tmux enables it when the program inside takes the
//! alternate screen, and it reaches the outer terminal through us, which is
//! why it comes back "sometimes" -- that sometimes is whenever the agent is
//! sitting in an adopted tmux pane.
//!
//! So it is taken out of the stream. Everything else passes through byte for
//! byte, including the rest of a mode sequence that merely mentions 1007
//! among other parameters: only the enabling of that one mode is dropped,
//! and a sequence split across two reads is held until it is whole.

#[derive(Debug, Default)]
pub struct NoAltScroll {
    /// A partial `ESC [ ? ... ` being gathered across reads.
    pending: Vec<u8>,
    /// Whether the AGENT has asked for mouse events itself. While it has,
    /// the viewer stops translating the wheel and hands the mouse over --
    /// a program that asked for clicks should get clicks.
    pub agent_wants_mouse: bool,
}

/// Modes an attach refuses to let anything enable. Alternate scroll is the
/// one that bites; the list is a list so the next one is a line of data.
const REFUSED: &[u16] = &[1007];

/// The modes a program sets when it wants to be told about the mouse.
const MOUSE: &[u16] = &[1000, 1002, 1003];

/// Mouse reporting the VIEWER turns on, so it can translate the wheel into
/// the keys a full-screen program actually scrolls with. SGR (1006) because
/// its reports are unambiguous past column 95, which the old encoding is
/// not.
pub const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";
pub const MOUSE_OFF: &[u8] = b"\x1b[?1000l\x1b[?1006l";

/// What a mouse report from the terminal means to us.
#[derive(Debug, PartialEq, Eq)]
pub enum Wheel {
    Up,
    Down,
    /// A mouse event that is not the wheel: swallowed, since the program
    /// did not ask for the mouse and would not know what to do with it.
    Other,
}

/// Read SGR mouse reports (`ESC [ < b ; x ; y M|m`) out of the keyboard
/// stream, saying which were wheel turns.
///
/// Everything else is passed back untouched, including a report that is
/// only half here yet -- a wheel turn split across two reads must not
/// become a stray `[<64` typed into somebody's prompt.
#[derive(Debug, Default)]
pub struct MouseReports {
    pending: Vec<u8>,
}

impl MouseReports {
    pub fn read(&mut self, data: &[u8]) -> (Vec<u8>, Vec<Wheel>) {
        let mut keys = Vec::with_capacity(data.len());
        let mut wheels = Vec::new();
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut i = 0;
        while i < buf.len() {
            // ESC [ <
            if buf[i] != 0x1b {
                keys.push(buf[i]);
                i += 1;
                continue;
            }
            if i + 2 >= buf.len() {
                self.pending = buf[i..].to_vec();
                return (keys, wheels);
            }
            if buf[i + 1] != b'[' || buf[i + 2] != b'<' {
                keys.push(buf[i]);
                i += 1;
                continue;
            }
            let mut j = i + 3;
            while j < buf.len() && buf[j] != b'M' && buf[j] != b'm' {
                j += 1;
            }
            if j >= buf.len() {
                if buf.len() - i > 32 {
                    keys.extend_from_slice(&buf[i..]);
                    return (keys, wheels);
                }
                self.pending = buf[i..].to_vec();
                return (keys, wheels);
            }
            let button = std::str::from_utf8(&buf[i + 3..j])
                .ok()
                .and_then(|p| p.split(';').next().and_then(|b| b.trim().parse::<u32>().ok()));
            match button {
                // 64 and 65 are the wheel; the rest is somebody clicking.
                Some(64) => wheels.push(Wheel::Up),
                Some(65) => wheels.push(Wheel::Down),
                _ => wheels.push(Wheel::Other),
            }
            i = j + 1;
        }
        (keys, wheels)
    }
}

/// DROP THE RIGHT BUTTON, so tmux cannot offer a way out of the agent.
///
/// tmux's own right-click menu -- the one it draws when the program in the
/// pane has not asked for the mouse -- offers "Horizontal Split" and
/// "Vertical Split", and a new pane in a held session is a SHELL on that
/// machine. The prefix key can be taken away per session (the node does
/// that), but tmux key bindings are per-SERVER: unbinding the mouse menu
/// there would change the operator's own tmux, which is not this program's
/// to do.
///
/// So the report never reaches tmux. Only when the agent itself has not
/// asked for the mouse -- an agent that has is being given its clicks, and
/// tmux shows no menu while that is true.
///
/// SGR reports only (`ESC [ < b ; x ; y M|m`), which is what every terminal
/// still in use sends once an application asks for the mouse at all. A
/// half-arrived report is held rather than passed on, for the same reason
/// the wheel reader holds one: `[<2` typed into somebody's prompt is worse
/// than a click that does nothing.
#[derive(Debug, Default)]
pub struct RightClicks {
    pending: Vec<u8>,
}

impl RightClicks {
    pub fn pass(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            if i + 2 >= buf.len() {
                self.pending = buf[i..].to_vec();
                return out;
            }
            if buf[i + 1] != b'[' || buf[i + 2] != b'<' {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            let mut j = i + 3;
            while j < buf.len() && buf[j] != b'M' && buf[j] != b'm' {
                j += 1;
            }
            if j >= buf.len() {
                // Not a mouse report at all, just a long escape sequence:
                // the wheel reader draws the same line in the same place.
                if buf.len() - i > 32 {
                    out.extend_from_slice(&buf[i..]);
                    return out;
                }
                self.pending = buf[i..].to_vec();
                return out;
            }
            let button = std::str::from_utf8(&buf[i + 3..j])
                .ok()
                .and_then(|p| p.split(';').next().and_then(|b| b.trim().parse::<u32>().ok()));
            // The low two bits are which button; 64 and up are the wheel and
            // motion, which are nobody's menu. Modifiers live in the bits
            // between, so a Meta-right-click is still a right-click -- which
            // matters, because tmux binds that one to the same menu.
            let is_right = button.is_some_and(|b| b < 64 && b & 0b11 == 2);
            if !is_right {
                out.extend_from_slice(&buf[i..=j]);
            }
            i = j + 1;
        }
        out
    }
}

impl NoAltScroll {
    /// Hand on what the viewer should write, having taken out what it must
    /// not. `pending` keeps a half-arrived sequence for the next call.
    pub fn pass(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pending.len() + data.len());
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);

        let mut i = 0;
        while i < buf.len() {
            // Anything that is not the start of ESC [ ? goes straight out.
            if buf[i] != 0x1b {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            // ESC at the very end, or ESC [ at the very end: hold it.
            if i + 1 >= buf.len() {
                self.pending = buf[i..].to_vec();
                return out;
            }
            if buf[i + 1] != b'[' {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            if i + 2 >= buf.len() {
                self.pending = buf[i..].to_vec();
                return out;
            }
            if buf[i + 2] != b'?' {
                out.push(buf[i]);
                i += 1;
                continue;
            }
            // ESC [ ? <params> <final>. Find the final byte.
            let mut j = i + 3;
            while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                j += 1;
            }
            if j >= buf.len() {
                // Not finished yet. Hold it rather than guess -- but do not
                // hold for ever on a stream that is not a sequence at all.
                if buf.len() - i > 64 {
                    out.extend_from_slice(&buf[i..]);
                    return out;
                }
                self.pending = buf[i..].to_vec();
                return out;
            }
            let params = &buf[i + 3..j];
            let enabling = buf[j] == b'h';
            let modes: Vec<u16> = params
                .split(|b| *b == b';')
                .filter_map(|p| std::str::from_utf8(p).ok())
                .filter_map(|p| p.trim().parse::<u16>().ok())
                .collect();
            // Whether the agent is asking for the mouse, so the viewer can
            // stop competing for it.
            if modes.iter().any(|n| MOUSE.contains(n)) {
                self.agent_wants_mouse = enabling;
            }
            let refused = enabling && modes.iter().any(|n| REFUSED.contains(n));
            if !refused {
                out.extend_from_slice(&buf[i..=j]);
            }
            i = j + 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_right_click_never_reaches_tmux_and_everything_else_does() {
        let mut r = RightClicks::default();
        // Left press and release, pass through.
        assert_eq!(r.pass(b"\x1b[<0;10;5M\x1b[<0;10;5m"), b"\x1b[<0;10;5M\x1b[<0;10;5m");
        // Right press: gone, menu and all.
        assert_eq!(r.pass(b"\x1b[<2;10;5M"), b"");
        // Meta-right-click is the same menu, so it goes too.
        assert_eq!(r.pass(b"\x1b[<10;10;5M"), b"");
        // The wheel is not a button.
        assert_eq!(r.pass(b"\x1b[<64;1;1M"), b"\x1b[<64;1;1M");
        // And ordinary keys are untouched.
        assert_eq!(r.pass(b"hello\r"), b"hello\r");
    }

    #[test]
    fn a_report_split_across_two_reads_is_not_typed_into_the_prompt() {
        let mut r = RightClicks::default();
        assert_eq!(r.pass(b"\x1b[<2;10"), b"");
        assert_eq!(r.pass(b";5M"), b"", "the rest of it goes with it");
        assert_eq!(r.pass(b"\x1b[<0;1"), b"");
        assert_eq!(r.pass(b";1M"), b"\x1b[<0;1;1M", "and a left click still arrives whole");
    }

    use super::*;

    fn through(chunks: &[&[u8]]) -> Vec<u8> {
        let mut g = NoAltScroll::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(g.pass(c));
        }
        out
    }

    #[test]
    fn alternate_scroll_never_reaches_the_terminal() {
        assert_eq!(through(&[b"before\x1b[?1007hafter"]), b"beforeafter".to_vec());
        // Turning it OFF is fine: that is what we want to happen.
        assert_eq!(through(&[b"\x1b[?1007l"]), b"\x1b[?1007l".to_vec());
    }

    #[test]
    fn everything_else_passes_through_byte_for_byte() {
        let samples: &[&[u8]] = &[
            b"plain text",
            b"\x1b[?1049h",                 // the alternate screen is legitimate
            b"\x1b[?25l\x1b[?25h",          // the cursor is the agent's business
            b"\x1b[31mcolour\x1b[0m",
            b"\x1b[2J\x1b[H",
            b"\x1b[?1000h\x1b[?1006h",      // mouse reporting, which an app may want
            b"\x1b]0;a title\x07",
        ];
        for s in samples {
            assert_eq!(through(&[s]), s.to_vec(), "{:?}", String::from_utf8_lossy(s));
        }
    }

    #[test]
    fn a_sequence_split_across_reads_is_still_caught() {
        // A pty hands over bytes in whatever pieces it feels like.
        let whole = b"a\x1b[?1007hb";
        for cut in 1..whole.len() {
            let got = through(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(got, b"ab".to_vec(), "cut at {cut}");
        }
    }

    #[test]
    fn one_mode_among_several_only_loses_that_mode() {
        // `CSI ? 1007 ; 1049 h` enables both; the alternate screen must
        // survive, so the whole sequence is dropped and nothing is invented
        // -- a terminal that gets half a sequence is worse off than one that
        // gets none.
        assert_eq!(through(&[b"x\x1b[?1007;1049hy"]), b"xy".to_vec());
        assert_eq!(through(&[b"x\x1b[?1049;1000hy"]), b"x\x1b[?1049;1000hy".to_vec());
    }

    #[test]
    fn the_agent_asking_for_the_mouse_is_noticed() {
        let mut g = NoAltScroll::default();
        assert!(!g.agent_wants_mouse);
        g.pass(b"\x1b[?1000h");
        assert!(g.agent_wants_mouse, "a program that asked for clicks should get them");
        g.pass(b"\x1b[?1000l");
        assert!(!g.agent_wants_mouse, "and stops getting them when it stops asking");
        g.pass(b"\x1b[?1003h");
        assert!(g.agent_wants_mouse);
    }

    #[test]
    fn wheel_turns_are_read_out_of_the_keyboard_stream() {
        let mut m = MouseReports::default();
        let (keys, wheels) = m.read(b"a\x1b[<64;10;20Mb");
        assert_eq!(keys, b"ab".to_vec(), "typing is not eaten");
        assert_eq!(wheels, vec![Wheel::Up]);

        let (_, wheels) = m.read(b"\x1b[<65;1;1M");
        assert_eq!(wheels, vec![Wheel::Down]);

        // A click is swallowed rather than typed at a program that never
        // asked for the mouse.
        let (keys, wheels) = m.read(b"\x1b[<0;5;5M");
        assert!(keys.is_empty());
        assert_eq!(wheels, vec![Wheel::Other]);
    }

    #[test]
    fn a_report_split_across_reads_never_lands_in_the_prompt() {
        let whole = b"x\x1b[<64;10;20My";
        for cut in 1..whole.len() {
            let mut m = MouseReports::default();
            let (mut keys, mut wheels) = m.read(&whole[..cut]);
            let (k2, w2) = m.read(&whole[cut..]);
            keys.extend(k2);
            wheels.extend(w2);
            assert_eq!(keys, b"xy".to_vec(), "cut at {cut}: {:?}", String::from_utf8_lossy(&keys));
            assert_eq!(wheels, vec![Wheel::Up], "cut at {cut}");
        }
    }

    #[test]
    fn a_stream_that_only_looks_like_a_sequence_is_not_swallowed() {
        // An ESC [ ? with no final byte for a long time is not a sequence
        // anybody is going to finish: it must not hold the screen hostage.
        let junk = [b"\x1b[?".to_vec(), vec![b'1'; 200]].concat();
        let got = through(&[&junk]);
        assert!(got.len() >= 200, "the output must not be held for ever: {}", got.len());
    }
}
