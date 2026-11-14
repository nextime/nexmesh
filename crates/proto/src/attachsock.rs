// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Attaching to a terminal on the machine you are standing on, without going
//! through the hub.
//!
//! The hub is how an attach is *authorised*: it knows who you are, which agent
//! you asked for and which machine holds its terminal, and nothing here
//! changes that. What it should not have to be is the *wire*. Attaching from
//! sissy to an agent on sissy used to send every keystroke to another machine
//! and back -- two LAN hops and a relay through a hub on a Celeron, measured
//! at 2.8ms per round trip on an idle network, for bytes that never needed to
//! leave the host. The same is true of sissy to zeiss whenever the two can see
//! each other directly.
//!
//! So the hub hands out a ticket, and the data goes straight to the node:
//!
//! 1. the operator asks the hub for an attach to an agent;
//! 2. the hub mints a one-time ticket, pushes it to that agent's node over the
//!    connection it already holds, and returns it to the operator;
//! 3. the operator connects to the node and presents the ticket;
//! 4. the node matches it against what the hub told it, spends it, and the
//!    terminal is relayed over that connection.
//!
//! No shared secret to design and no new trust: the ticket is worth nothing
//! unless the hub itself sent the same ticket to the node, it is good once,
//! and it expires. A ticket that is never spent costs nothing. Anything that
//! does not work falls back to relaying through the hub, which is what keeps
//! attach working from outside the LAN.
//!
//! The framing is deliberately dull: a kind byte, a big-endian length, and the
//! payload. Raw pty bytes stay raw -- base64 through JSON, which is what the
//! hub leg uses, costs a third more bytes and a parse per chunk for a stream
//! that is mostly screen repaints.

use serde::{Deserialize, Serialize};

/// The socket a node listens on for direct attaches, beside the one the agent
/// shims use. Derived rather than configured: one address for both would mean
/// two protocols on one socket, and a second setting to get wrong.
pub fn socket_path_for(shim_socket: &str) -> String {
    match shim_socket.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/attach.sock"),
        None => "attach.sock".to_string(),
    }
}

/// How long a ticket is worth anything. Long enough for a slow link and a
/// fallback attempt, short enough that one left in a shell history is already
/// dead.
pub const TICKET_TTL_MS: i64 = 30_000;

/// What the operator says first, and nothing else until it is answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Open {
    pub ticket: String,
    pub agent_id: String,
    pub cols: u16,
    pub rows: u16,
    /// Bytes of scrollback to replay before live output.
    #[serde(default)]
    pub replay_bytes: usize,
}

/// What either side says about the attach itself, as opposed to the bytes on
/// the terminal. The same shapes the hub leg uses, so a viewer handles one set
/// of answers however it connected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    /// Node -> operator: the terminal is yours.
    Opened { read_only: bool, cols: u16, rows: u16 },
    /// Node -> operator: it is not, and why.
    Error { message: String },
    /// Node -> operator: it has ended, and why.
    Closed { reason: String },
    /// Operator -> node: the window changed size.
    Resize { cols: u16, rows: u16 },
    /// Operator -> node: done here.
    Close,
}

/// A frame on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Terminal bytes, in whichever direction.
    Data(Vec<u8>),
    /// Anything that is not terminal bytes.
    Control(Control),
    /// The first frame of a connection.
    Open(Open),
}

const KIND_DATA: u8 = 1;
const KIND_CONTROL: u8 = 2;
const KIND_OPEN: u8 = 3;

/// Bigger than any pty read and small enough that a wrong length is refused
/// rather than allocated.
pub const MAX_FRAME: usize = 1 << 20;

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Frame::Data(d) => (KIND_DATA, d.clone()),
            Frame::Control(c) => {
                (KIND_CONTROL, serde_json::to_vec(c).unwrap_or_else(|_| b"{}".to_vec()))
            }
            Frame::Open(o) => {
                (KIND_OPEN, serde_json::to_vec(o).unwrap_or_else(|_| b"{}".to_vec()))
            }
        };
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// One frame from the front of `buf`, and how many bytes it used. `None`
    /// means not yet: the caller reads more and asks again.
    pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>, String> {
        if buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(format!("frame of {len} bytes is beyond the limit"));
        }
        if buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = &buf[5..5 + len];
        let frame = match buf[0] {
            KIND_DATA => Frame::Data(payload.to_vec()),
            KIND_CONTROL => Frame::Control(
                serde_json::from_slice(payload).map_err(|e| format!("bad control frame: {e}"))?,
            ),
            KIND_OPEN => Frame::Open(
                serde_json::from_slice(payload).map_err(|e| format!("bad open frame: {e}"))?,
            ),
            other => return Err(format!("unknown frame kind {other}")),
        };
        Ok(Some((frame, 5 + len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attach_socket_sits_beside_the_shim_socket() {
        assert_eq!(socket_path_for("/run/nexmesh/node.sock"), "/run/nexmesh/attach.sock");
        assert_eq!(socket_path_for("/tmp/x/n.sock"), "/tmp/x/attach.sock");
        assert_eq!(socket_path_for("node.sock"), "attach.sock");
    }

    #[test]
    fn a_frame_survives_the_wire() {
        for f in [
            Frame::Data(vec![0, 1, 2, 27, b'[', b'A']),
            Frame::Data(vec![]),
            Frame::Control(Control::Opened { read_only: false, cols: 80, rows: 24 }),
            Frame::Control(Control::Resize { cols: 200, rows: 50 }),
            Frame::Control(Control::Close),
            Frame::Control(Control::Closed { reason: "the agent exited".into() }),
            Frame::Open(Open {
                ticket: "t".into(),
                agent_id: "ag-1".into(),
                cols: 80,
                rows: 24,
                replay_bytes: 4096,
            }),
        ] {
            let bytes = f.encode();
            let (back, used) = Frame::decode(&bytes).unwrap().unwrap();
            assert_eq!(back, f);
            assert_eq!(used, bytes.len());
        }
    }

    #[test]
    fn frames_arrive_in_whatever_pieces_the_socket_gives() {
        // A pty stream is read in chunks that have nothing to do with frame
        // boundaries, so decode must hold its peace until a frame is whole.
        let wire: Vec<u8> = [Frame::Data(b"hello".to_vec()), Frame::Control(Control::Close)]
            .iter()
            .flat_map(|f| f.encode())
            .collect();
        for cut in 1..wire.len() {
            let mut buf = wire[..cut].to_vec();
            let mut seen = Vec::new();
            loop {
                match Frame::decode(&buf).unwrap() {
                    Some((f, used)) => {
                        seen.push(f);
                        buf.drain(..used);
                    }
                    None => break,
                }
            }
            // Whatever was cut, nothing is invented and nothing is corrupt.
            if let Some(Frame::Data(d)) = seen.first() {
                assert_eq!(d, b"hello");
            }
            assert!(seen.len() <= 2);
        }
        // All of it, and both frames are there.
        let mut buf = wire.clone();
        let mut seen = Vec::new();
        while let Some((f, used)) = Frame::decode(&buf).unwrap() {
            seen.push(f);
            buf.drain(..used);
        }
        assert_eq!(seen.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_length_beyond_the_limit_is_refused_not_allocated() {
        let mut bytes = vec![KIND_DATA];
        bytes.extend_from_slice(&(u32::MAX).to_be_bytes());
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn an_unknown_kind_is_an_error_rather_than_a_guess() {
        let bytes = vec![99, 0, 0, 0, 0];
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn half_a_header_is_not_yet_a_frame() {
        assert_eq!(Frame::decode(&[KIND_DATA, 0, 0]).unwrap(), None);
    }
}
