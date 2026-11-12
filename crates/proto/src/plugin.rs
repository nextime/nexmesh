//! The driver plugin protocol.
//!
//! Three drivers are built in, and each one exists because a particular CLI
//! speaks a particular protocol: `stream_json` for Claude Code, `app_server`
//! for Codex, `http_server` for opencode. Every new CLI with its own shape
//! would otherwise mean another module in `nexmesh-node` and a release of
//! nexmesh before anyone could use it.
//!
//! A plugin moves that work outside. It is an ordinary executable, in any
//! language, that translates between one CLI's quirks and the four things a
//! driver has to provide:
//!
//!   1. get a teammate's message in **mid-turn**, not at the next tool call
//!   2. confirm that it **arrived**, so the message can be marked delivered
//!   3. report **token usage**
//!   4. hand back a **resume handle**, so a crashed agent can be continued
//!
//! Frames are JSON, one per line, on the plugin's stdin and stdout. Anything
//! the plugin writes to **stderr** goes to the node's log, which is where to
//! put diagnostics -- a plugin that logs to stdout corrupts the stream.
//!
//! What a plugin deliberately does NOT get is the CLI process itself. The
//! node spawns the CLI, owns its pid and is the only component that may
//! signal it; the plugin only ever sees its output and asks for bytes to be
//! written to its input. A plugin cannot kill an agent, spawn anything, or
//! reach another machine, so running one is not the same as trusting it with
//! the fleet.
//!
//! A plugin that dies is not fatal. Injection then fails, the hub puts those
//! messages back in the queue, and the agent picks them up through MCP at its
//! next tool call -- the universal floor. Slower, never silent.

use crate::Envelope;
use serde::{Deserialize, Serialize};

/// Bumped only on an incompatible change. A plugin should refuse a version it
/// does not know rather than guess at frames.
pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;

/// Node -> plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPlugin {
    /// Always first. Carries the task, because for most plugin-driven CLIs the
    /// prompt has to go in through the protocol rather than on the command
    /// line -- so the plugin, not the node, decides how the first prompt is
    /// delivered.
    Hello {
        protocol: u32,
        agent_id: String,
        slot_id: String,
        cli_id: String,
        cwd: String,
        prompt: String,
        /// Set when this agent is being continued after a crash: whatever the
        /// plugin last reported as its resume handle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
    },
    /// A chunk of the CLI's stdout, exactly as it arrived. Not necessarily a
    /// whole line: a plugin that needs lines must buffer.
    Stdout { data: String },
    /// Teammate messages to get in front of the model now. `text` is the same
    /// rendering the MCP floor uses, so an agent sees one convention whichever
    /// path a message took; a plugin may use it as-is or build its own from
    /// `messages`.
    Deliver { text: String, messages: Vec<Envelope> },
    /// The CLI has exited. No more `Stdout` will follow, and the plugin should
    /// finish what it is doing and exit.
    Exit { code: Option<i32> },
}

/// Plugin -> node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromPlugin {
    /// Write these bytes to the CLI's stdin. The plugin adds its own newline
    /// if the CLI wants one; nothing is appended here.
    Stdin { data: String },
    /// Those messages reached the model. Until this arrives they count as in
    /// flight, and a crash releases them for redelivery, so a plugin that
    /// never acks turns delivery into a silent loss.
    Ack { message_ids: Vec<i64> },
    /// Cumulative token usage for this agent -- totals, not deltas, so a
    /// dropped frame costs accuracy for a moment rather than for ever.
    Tokens {
        #[serde(default)]
        input: i64,
        #[serde(default)]
        output: i64,
        #[serde(default)]
        cache_read: i64,
    },
    /// The CLI's own handle for continuing this conversation, whatever it
    /// calls it. Reported as soon as it is known, because a crash before that
    /// means the agent restarts from nothing.
    Resume { handle: String },
    /// A line for the node's log. For a plugin that would rather not write to
    /// stderr.
    Log {
        #[serde(default = "default_level")]
        level: String,
        text: String,
    },
}

fn default_level() -> String {
    "info".into()
}

impl ToPlugin {
    /// Serialise as one NDJSON line, newline included.
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageKind;

    fn env(id: i64) -> Envelope {
        Envelope {
            id,
            from_slot: Some("sl-a".into()),
            from_name: Some("Iris".into()),
            to_slot: Some("sl-b".into()),
            kind: MessageKind::Text,
            body: "look at this".into(),
            sent_at_ms: 7,
        }
    }

    #[test]
    fn frames_are_tagged_by_type_so_a_plugin_can_switch_on_one_field() {
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ToPlugin::Stdout {
                data: "x".into(),
            })
            .unwrap())
            .unwrap();
        assert_eq!(v["type"], "stdout");
        assert_eq!(v["data"], "x");
    }

    #[test]
    fn hello_omits_resume_when_there_is_none() {
        // A plugin checking `"resume" in frame` should not see a null.
        let s = serde_json::to_string(&ToPlugin::Hello {
            protocol: PLUGIN_PROTOCOL_VERSION,
            agent_id: "ag-1".into(),
            slot_id: "sl-1".into(),
            cli_id: "quirk".into(),
            cwd: "/tmp".into(),
            prompt: "do the thing".into(),
            resume: None,
        })
        .unwrap();
        assert!(!s.contains("resume"), "{s}");
        let s2 = serde_json::to_string(&ToPlugin::Hello {
            protocol: 1,
            agent_id: "ag-1".into(),
            slot_id: "sl-1".into(),
            cli_id: "quirk".into(),
            cwd: "/tmp".into(),
            prompt: "p".into(),
            resume: Some("thread-9".into()),
        })
        .unwrap();
        assert!(s2.contains("\"resume\":\"thread-9\""), "{s2}");
    }

    #[test]
    fn a_line_is_exactly_one_line() {
        // Awkward text is the norm, and an embedded newline would desync the
        // stream for every frame after it.
        let f = ToPlugin::Stdout { data: "two\nlines\tand \"quotes\" τ 🎉".into() };
        let line = f.line();
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert!(line.ends_with('\n'));
        let back: ToPlugin = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn deliver_carries_both_the_rendering_and_the_envelopes() {
        // The rendering keeps plugins trivial; the envelopes let a plugin that
        // cares build its own and still know which ids to ack.
        let f = ToPlugin::Deliver {
            text: "[PENDING MESSAGES]\n- from Iris: look at this".into(),
            messages: vec![env(1), env(2)],
        };
        let back: ToPlugin = serde_json::from_str(f.line().trim()).unwrap();
        assert_eq!(back, f);
        let ToPlugin::Deliver { messages, .. } = back else { panic!() };
        assert_eq!(messages.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn plugin_frames_parse_from_what_a_script_would_actually_print() {
        // Hand-written JSON, as a plugin in python or bash would emit it.
        let cases = [
            (r#"{"type":"stdin","data":"hello\n"}"#, FromPlugin::Stdin { data: "hello\n".into() }),
            (r#"{"type":"ack","message_ids":[4,5]}"#, FromPlugin::Ack { message_ids: vec![4, 5] }),
            (
                r#"{"type":"resume","handle":"t-1"}"#,
                FromPlugin::Resume { handle: "t-1".into() },
            ),
        ];
        for (text, want) in cases {
            assert_eq!(serde_json::from_str::<FromPlugin>(text).unwrap(), want, "{text}");
        }
    }

    #[test]
    fn token_fields_are_optional_so_a_plugin_can_report_what_it_knows() {
        let f: FromPlugin = serde_json::from_str(r#"{"type":"tokens","input":10}"#).unwrap();
        assert_eq!(f, FromPlugin::Tokens { input: 10, output: 0, cache_read: 0 });
    }

    #[test]
    fn a_log_frame_defaults_to_info() {
        let f: FromPlugin = serde_json::from_str(r#"{"type":"log","text":"hi"}"#).unwrap();
        assert_eq!(f, FromPlugin::Log { level: "info".into(), text: "hi".into() });
    }

    #[test]
    fn an_unknown_frame_is_an_error_not_a_silent_no_op() {
        // A plugin built against a newer nexmesh must fail loudly here.
        assert!(serde_json::from_str::<FromPlugin>(r#"{"type":"teleport"}"#).is_err());
        assert!(serde_json::from_str::<ToPlugin>(r#"{"type":"teleport"}"#).is_err());
    }
}
