//! Noticing that an agent is going round in circles.
//!
//! An agent in a loop is not a stuck process -- it is a busy one. It answers
//! heartbeats, it burns tokens, its status line moves, and from outside it is
//! indistinguishable from an agent working hard. What gives it away is that
//! it keeps doing the SAME THING: the same tool with the same arguments, or
//! the same answer, again and again.
//!
//! So this works on SIGNATURES, not content. Every turn an agent takes is
//! reduced to one number -- what it did, not how it phrased it -- and a loop
//! is a repeating cycle in that sequence. That makes the detector short, and
//! it makes it honest about what it cannot see: an agent making genuine
//! progress by repeating a tool with DIFFERENT arguments has different
//! signatures and is never flagged, which is the case a content-based guess
//! would get wrong.
//!
//! Cycles, not just repeats. An agent alternating between two failing steps
//! -- edit, test, edit, test, all identical -- is as stuck as one repeating
//! a single step, and only looking for runs of one signature would miss it.

use serde_json::Value;

/// The longest cycle looked for. Beyond this a "cycle" stops being evidence
/// of a loop and starts being a description of ordinary work: plan, read,
/// edit, test, commit is a five-step cycle every productive agent repeats.
const MAX_PERIOD: usize = 4;

/// How many signatures are kept per agent. Enough for the longest cycle
/// repeated as many times as anybody would set the threshold to.
pub const RING: usize = 48;

/// What a detected loop looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    /// How many turns the repeating unit is: 1 is the same step over and
    /// over, 2 an agent flipping between two.
    pub period: usize,
    /// How many times that unit has repeated, back to back, counting the
    /// most recent turn.
    pub repeats: usize,
}

/// What one turn DID, as a number.
///
/// Tool calls are the signature when there are any: a tool name and its
/// arguments are what an agent actually does to the world, and two turns
/// with the same ones are the same turn however differently they are
/// narrated. Text-only turns fall back to their text, which is what catches
/// an agent repeating itself at a person.
///
/// `None` for a turn with nothing in it to compare -- those are skipped
/// rather than hashed to a constant, which would make every empty turn look
/// like a repeat of every other.
pub fn signature(v: &Value) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    // A subagent's turns are its own conversation. They would otherwise
    // interleave with the seat's and break up a cycle that is really there.
    if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let msg = v.get("message")?;
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = msg.get("content")?.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input").map(ToString::to_string).unwrap_or_default();
                parts.push(format!("t:{name}:{input}"));
            }
            // Thinking is deliberately ignored. It is the most variable part
            // of a turn and the least connected to what the turn did: an
            // agent repeating one failing command while musing differently
            // about it each time is in a loop.
            Some("text") => {
                let t = block.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if !t.is_empty() {
                    parts.push(format!("s:{t}"));
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    Some(h.finish())
}

/// Is the tail of this sequence a cycle repeated at least `want` times?
///
/// Returns the SHORTEST period that qualifies, so an agent repeating one
/// step is reported as period 1 rather than as two repeats of a two-step
/// cycle -- the shortest description of a loop is the one an operator can
/// act on.
pub fn detect(sigs: &[u64], want: usize) -> Option<Cycle> {
    if want < 2 {
        // One "repeat" is a single turn, which is just an agent working.
        return None;
    }
    for period in 1..=MAX_PERIOD {
        if sigs.len() < period * want {
            continue;
        }
        let unit = &sigs[sigs.len() - period..];
        let mut repeats = 1;
        loop {
            let end = sigs.len() - period * repeats;
            if end < period {
                break;
            }
            if &sigs[end - period..end] != unit {
                break;
            }
            repeats += 1;
        }
        if repeats >= want {
            return Some(Cycle { period, repeats });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn(blocks: Value) -> Value {
        json!({"message": {"role": "assistant", "content": blocks}})
    }

    #[test]
    fn the_same_tool_with_the_same_arguments_is_the_same_turn() {
        let a = turn(json!([{"type": "tool_use", "name": "Bash", "input": {"command": "make"}}]));
        let b = turn(json!([{"type": "tool_use", "name": "Bash", "input": {"command": "make"}}]));
        assert_eq!(signature(&a), signature(&b));
    }

    #[test]
    fn the_same_tool_with_different_arguments_is_not() {
        // The case a content-based guess gets wrong: an agent reading forty
        // files is not looping.
        let a = turn(json!([{"type": "tool_use", "name": "Read", "input": {"path": "a.rs"}}]));
        let b = turn(json!([{"type": "tool_use", "name": "Read", "input": {"path": "b.rs"}}]));
        assert_ne!(signature(&a), signature(&b));
    }

    #[test]
    fn how_a_turn_was_narrated_does_not_change_what_it_did() {
        let a = turn(json!([
            {"type": "thinking", "thinking": "let me try once more"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
        ]));
        let b = turn(json!([
            {"type": "thinking", "thinking": "a completely different thought"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}},
        ]));
        assert_eq!(signature(&a), signature(&b));
    }

    #[test]
    fn a_users_turn_has_no_signature() {
        let u = json!({"message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}});
        assert!(signature(&u).is_none());
    }

    #[test]
    fn a_subagents_turn_has_no_signature() {
        let mut t = turn(json!([{"type": "tool_use", "name": "Bash", "input": {}}]));
        t["isSidechain"] = json!(true);
        assert!(signature(&t).is_none());
    }

    #[test]
    fn an_empty_turn_is_skipped_rather_than_hashed_to_a_constant() {
        // Otherwise every empty turn is a repeat of every other one, and an
        // agent that emits a few in a row is reported as looping.
        assert!(signature(&turn(json!([]))).is_none());
        assert!(signature(&turn(json!([{"type": "text", "text": "   "}]))).is_none());
    }

    #[test]
    fn one_step_repeated_is_a_loop_of_period_one() {
        let c = detect(&[9, 9, 9, 9], 4).unwrap();
        assert_eq!(c, Cycle { period: 1, repeats: 4 });
    }

    #[test]
    fn two_steps_alternating_is_a_loop_too() {
        // edit, test, edit, test, edit, test -- as stuck as repeating one
        // step, and invisible to a detector that only looks for runs.
        let c = detect(&[1, 2, 1, 2, 1, 2], 3).unwrap();
        assert_eq!(c, Cycle { period: 2, repeats: 3 });
    }

    #[test]
    fn the_shortest_period_wins() {
        // [7,7,7,7] is also two repeats of [7,7]. Period 1 is the useful
        // answer.
        assert_eq!(detect(&[7, 7, 7, 7], 2).unwrap().period, 1);
    }

    #[test]
    fn work_that_merely_rhymes_is_not_a_loop() {
        let c = detect(&[1, 2, 3, 4, 5, 6, 7, 8], 3);
        assert!(c.is_none());
    }

    #[test]
    fn a_cycle_that_has_not_repeated_enough_times_yet_is_not_reported() {
        assert!(detect(&[5, 5, 5], 4).is_none());
        assert!(detect(&[5, 5, 5, 5], 4).is_some());
    }

    #[test]
    fn a_long_cycle_is_ordinary_work_and_is_not_looked_for() {
        // plan, read, edit, test, commit, repeated: a productive agent.
        let seq: Vec<u64> = (0..5).cycle().take(30).collect();
        assert!(detect(&seq, 3).is_none());
    }

    #[test]
    fn a_threshold_below_two_never_fires() {
        // Guards the settings path: 1 would mean "one turn is a loop".
        assert!(detect(&[4, 4, 4, 4, 4, 4], 1).is_none());
        assert!(detect(&[4, 4, 4, 4, 4, 4], 0).is_none());
    }

    #[test]
    fn the_loop_must_be_happening_NOW() {
        // It looped earlier and has since moved on. Reporting that would
        // interrupt an agent that recovered by itself.
        assert!(detect(&[3, 3, 3, 3, 3, 8, 9], 3).is_none());
    }
}
