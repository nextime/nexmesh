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
}

/// Modes an attach refuses to let anything enable. Alternate scroll is the
/// one that bites; the list is a list so the next one is a line of data.
const REFUSED: &[u16] = &[1007];

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
            let refused = enabling
                && params
                    .split(|b| *b == b';')
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .filter_map(|p| p.trim().parse::<u16>().ok())
                    .any(|n| REFUSED.contains(&n));
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
    fn a_stream_that_only_looks_like_a_sequence_is_not_swallowed() {
        // An ESC [ ? with no final byte for a long time is not a sequence
        // anybody is going to finish: it must not hold the screen hostage.
        let junk = [b"\x1b[?".to_vec(), vec![b'1'; 200]].concat();
        let got = through(&[&junk]);
        assert!(got.len() >= 200, "the output must not be held for ever: {}", got.len());
    }
}
