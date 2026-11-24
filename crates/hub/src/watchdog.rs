//! Looking after a seat that is filling up, or going round in circles.
//!
//! Two different failures with one shape. In both, the node MEASURES
//! something it alone can see -- how full a conversation is, what the last
//! few turns did -- and the hub decides whether it matters, because the
//! decision is a policy the operator set and policy lives here.
//!
//! BOTH ARE OFF UNTIL SOMEBODY TURNS THEM ON. Compacting a seat throws away
//! context it may need, and pausing one stops work that may be fine; doing
//! either to every seat on a fleet because a hub was upgraded is not a
//! default anybody chose. So the fleet default is off, and a threshold is
//! set per seat or per team by the person who knows what that seat is for.
//!
//! THREE STATES, ONE INTEGER. A threshold is read from the seat, then its
//! team, then the fleet. NULL at a level means "ask the next one"; 0 means
//! OFF, deliberately, however the levels above it are set. Two states could
//! not express a seat that opts out of its team's setting, and that is the
//! setting an operator reaches for first -- "this one, leave alone".

use crate::state::AppState;
use anyhow::Result;
use nexmesh_proto::now_ms;
use rusqlite::{params, Connection, OptionalExtension};

/// How long before a seat that is still too full is asked again.
///
/// Compaction is not instant -- the CLI has to summarise its own
/// conversation, which took around a minute on the live fleet -- and the
/// readings during it are all still over the threshold. Without a wait, one
/// seat over its limit would be sent a `/compact` every twenty seconds. Long
/// enough to let a compaction finish and be measured; short enough that one
/// refused because the screen was waiting for an answer is retried while the
/// operator is still around.
const COMPACT_AGAIN_AFTER_MS: i64 = 5 * 60 * 1000;

/// Resolve a per-seat / per-team / fleet-wide integer setting.
///
/// `column` must name a column that exists on both `slots` and `sessions`.
/// Returns 0 for off, which every caller treats as "do nothing".
pub fn setting(db: &Connection, slot_id: &str, column: &str, fleet: i64) -> Result<i64> {
    let row: Option<(Option<i64>, Option<i64>)> = db
        .query_row(
            &format!(
                "SELECT s.{column}, se.{column}
                   FROM slots s JOIN sessions se ON se.id = s.session_id
                  WHERE s.id = ?"
            ),
            [slot_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seat, team)) = row else { return Ok(0) };
    Ok(seat.or(team).unwrap_or(fleet).max(0))
}

/// An agent whose context has passed its threshold.
pub struct Full {
    pub agent_id: String,
    pub slot_id: String,
    /// What a person calls that seat. Nothing is SENT from here using it --
    /// the node says what it typed in the seat's own scrollback and in the
    /// chat -- but the hub's log and audit line are read by somebody who
    /// knows seats by name and not by id.
    pub name: String,
    pub node_id: String,
    pub used: i64,
    pub limit: i64,
    pub pct: i64,
    pub threshold: i64,
}

/// What a seat full of context asks for: nothing yet, a fold, or a compaction.
///
/// Two steps rather than one, and in this order. A compaction is the CLI
/// summarising its own context into its own context, where the next
/// compaction summarises the summary; what survives four generations of that
/// is the shape of the work without its facts, and none of it survives a
/// restart. A fold is the agent writing down what the work established,
/// kept by nexmesh, out of the context being thrown away. So the fold is
/// asked for first and the compaction happens anyway -- a full context is
/// worse than a fold nobody wrote.
pub enum Next {
    Nothing,
    Fold(Nudge),
    Compact(Full),
}

/// A seat asked to fold what it no longer needs.
pub struct Nudge {
    pub agent_id: String,
    pub slot_id: String,
    pub session_id: String,
    pub name: String,
    /// A compaction is waiting on this fold, so the agent is told that
    /// plainly. Otherwise the nudge says nothing about how full it is:
    /// billion-context's own finding is that this reads better as an
    /// efficiency habit than as an overflow warning, and a model told it is
    /// nearly out of room folds in a hurry and keeps the wrong things.
    pub urgent: bool,
    /// What it has folded so far, so the ask can suggest a tier when the
    /// seat is carrying many tier-1 folds.
    pub folds: i64,
}

/// How full a seat must be before folding is suggested at all, as a fraction
/// of its own compaction threshold.
///
/// Below this there is nothing worth folding: the recent zone of a
/// conversation is what the agent is working in, and summarising that is how
/// an agent loses the thread of what it is doing.
const FOLD_FLOOR_OF_THRESHOLD: i64 = 6; // sixths, so 6/10 of the threshold
const FOLD_FLOOR_DIVISOR: i64 = 10;

/// And how much it must have TAKEN ON since it was last asked.
///
/// A quarter of the window, which is billion-context's 50K on a 200K model
/// expressed as a proportion so it scales with whatever window a CLI has. The
/// growth gate is what stops this being a reminder every second: what matters
/// is not that a seat is full but that it has filled further since it was
/// last asked to do something about it.
fn fold_gate(limit: i64) -> i64 {
    (limit / 4).max(4000)
}

/// How long a compaction waits for the fold it asked for.
const FOLD_GRACE_MS: i64 = 90_000;

/// Over this, nothing waits for anything: the context is nearly gone and a
/// compaction that happens is worth more than a fold that might not.
const FOLD_EMERGENCY_PCT: i64 = 95;

/// After this, an earlier ask belongs to a different filling-up and the seat
/// is asked again rather than compacted on the strength of a request it may
/// have answered an hour ago.
const FOLD_ASK_STALE_MS: i64 = 600_000;

/// Record a reading, and say whether it calls for a fold or a compaction.
///
/// The recording happens either way: the number is worth having on the
/// dashboard whether or not anybody asked for anything to be done about it,
/// and a seat that is never compacted still answers "how full is it?".
pub fn record_context(
    db: &Connection,
    fleet_pct: i64,
    agent_id: &str,
    used: i64,
    limit: i64,
    source: &str,
) -> Result<Next> {
    db.execute(
        "UPDATE agents SET context_used_tokens = ?, context_limit_tokens = ?,
                           context_source = ?, context_at_ms = ?
          WHERE id = ?",
        params![used, limit, source, now_ms(), agent_id],
    )?;
    if limit <= 0 {
        return Ok(Next::Nothing);
    }
    #[allow(clippy::type_complexity)]
    let row: Option<(String, String, Option<String>, Option<String>, Option<i64>,
                     Option<i64>, Option<i64>)> = db
        .query_row(
            "SELECT s.id, s.session_id, s.display_name, a.node_id, a.compact_asked_at_ms,
                    a.fold_asked_at_ms, a.fold_baseline_tokens
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE a.id = ? AND a.exited_at_ms IS NULL",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?)),
        )
        .optional()?;
    let Some((slot_id, session_id, name, node_id, asked, fold_asked, baseline)) = row
    else {
        return Ok(Next::Nothing);
    };
    let threshold = setting(db, &slot_id, "compact_at_pct", fleet_pct)?;
    // Percent, rounded DOWN, so 84.9% of a window is not reported as 85 and
    // compacted by a threshold it has not reached.
    let pct = used.saturating_mul(100) / limit;

    if threshold <= 0 {
        return Ok(Next::Nothing);
    }

    // FOLD BEFORE THE THRESHOLD, not at it.
    //
    // Gated on growth rather than on fullness: a seat is asked once it has
    // taken on another quarter of a window since it was last asked, which is
    // what keeps this an occasional habit instead of a reminder on every
    // reading. The floor underneath it exists because the newest part of a
    // conversation is what the agent is working IN.
    let floor = threshold * FOLD_FLOOR_OF_THRESHOLD / FOLD_FLOOR_DIVISOR;
    let baseline = baseline.unwrap_or(0);
    if baseline == 0 {
        db.execute(
            "UPDATE agents SET fold_baseline_tokens = ? WHERE id = ?",
            params![used, agent_id],
        )?;
    }
    let grown = used - baseline;
    let folded_since_asked =
        crate::folds::folded_since(db, &slot_id, fold_asked.unwrap_or(0))?;
    if pct < threshold && pct >= floor && grown >= fold_gate(limit) && baseline > 0 {
        db.execute(
            "UPDATE agents SET fold_asked_at_ms = ?, fold_baseline_tokens = ?
              WHERE id = ?",
            params![now_ms(), used, agent_id],
        )?;
        return Ok(Next::Fold(Nudge {
            agent_id: agent_id.to_string(),
            slot_id: slot_id.clone(),
            session_id,
            name: name.unwrap_or_else(|| slot_id.clone()),
            urgent: false,
            folds: crate::folds::folded_since(db, &slot_id, 0)?,
        }));
    }

    if pct < threshold {
        // Back under: forget that it was ever asked, so the next time it
        // fills up it is asked again. This is what makes the stamp a
        // cooldown rather than a one-shot for the life of the process.
        if asked.is_some() {
            db.execute(
                "UPDATE agents SET compact_asked_at_ms = NULL WHERE id = ?",
                [agent_id],
            )?;
        }
        return Ok(Next::Nothing);
    }
    if let Some(at) = asked {
        if now_ms() - at < COMPACT_AGAIN_AFTER_MS {
            return Ok(Next::Nothing);
        }
    }
    // A seat with no node cannot be typed at, so there is nothing to ask.
    let Some(node_id) = node_id else { return Ok(Next::Nothing) };

    // OVER the threshold. Ask for a fold once, give it a little while, and
    // then compact whether or not one arrived: the fold is the better
    // outcome and the compaction is the one that has to happen.
    //
    // Three states, and the order of them is what keeps this from being
    // either an infinite ask or no ask at all:
    //
    //   * something was folded since we asked -- compact now, because what
    //     the agent wanted to keep is already out of harm's way;
    //   * never asked, or asked so long ago that it belongs to another
    //     filling-up -- ask, and come back on the next reading;
    //   * asked recently and nothing came -- wait out the grace, then
    //     compact anyway.
    if pct < FOLD_EMERGENCY_PCT && folded_since_asked == 0 {
        let recent = fold_asked.filter(|at| now_ms() - at < FOLD_ASK_STALE_MS);
        match recent {
            None => {
                db.execute(
                    "UPDATE agents SET fold_asked_at_ms = ?, fold_baseline_tokens = ?
                      WHERE id = ?",
                    params![now_ms(), used, agent_id],
                )?;
                return Ok(Next::Fold(Nudge {
                    agent_id: agent_id.to_string(),
                    slot_id: slot_id.clone(),
                    session_id,
                    name: name.unwrap_or_else(|| slot_id.clone()),
                    urgent: true,
                    folds: crate::folds::folded_since(db, &slot_id, 0)?,
                }));
            }
            Some(at) if now_ms() - at < FOLD_GRACE_MS => return Ok(Next::Nothing),
            Some(_) => {}
        }
    }
    db.execute(
        "UPDATE agents SET compact_asked_at_ms = ? WHERE id = ?",
        params![now_ms(), agent_id],
    )?;
    Ok(Next::Compact(Full {
        agent_id: agent_id.to_string(),
        slot_id: slot_id.clone(),
        name: name.unwrap_or(slot_id),
        node_id,
        used,
        limit,
        pct,
        threshold,
    }))
}

/// Ask a seat to fold what it no longer needs.
///
/// A message to the agent rather than a keystroke, because what is wanted is
/// a judgement only it can make: which stretch of its own work the step it is
/// on has finished with. The hub cannot know that, which is the whole reason
/// the model does the summarising in this design.
///
/// Delivered on the ordinary message path, so it reaches a driven seat within
/// a tick, a typed seat at its terminal, and a hand-run CLI at its next tool
/// call -- and so it is in the room's log afterwards, where an operator
/// wondering why a seat stopped to write summaries can see who asked.
pub async fn nudge(st: &AppState, n: Nudge) {
    let text = if n.urgent {
        format!(
            "[nexmesh] Your CLI is about to be asked to compact itself, which replaces your \
             context with a summary of it. Before that happens, call `fold_context` for each \
             stretch of this work the step you are on has finished with: what it \
             established, plus the raw material as `detail`. Folded, it is kept whole and \
             comes back with `unfold_context` -- compacted, it becomes a summary that the \
             next compaction will summarise again.\n\n{}",
            crate::folds::DOCTRINE
        )
    } else {
        // No percentage, deliberately. billion-context's own measurements
        // read better as an efficiency habit than as an overflow warning,
        // and an agent told it is nearly out of room folds in a hurry and
        // keeps the wrong things. `context_map` is there if it wants the
        // number.
        let tier = if n.folds >= 6 {
            " You are carrying several folds already: `fold_context` with `covers` puts one \
             summary above a group of them, which is what keeps a long job down to a handful."
        } else {
            ""
        };
        format!(
            "[nexmesh] A good moment to fold. Call `fold_context` for the stretches of this \
             work the step you are on no longer needs -- what each established, with the raw \
             material as `detail` -- and carry the summaries instead. Nothing is lost: \
             `search_context` finds a fold by its words and `unfold_context` gives the detail \
             back in full.{tier}\n\n{}",
            crate::folds::DOCTRINE
        )
    };
    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
             VALUES (?, NULL, ?, 'system', ?, ?)",
            params![n.session_id, n.slot_id, text, now_ms()],
        );
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, action, detail) VALUES (?,?,?,?)",
            params![
                now_ms(),
                "watchdog",
                "asked_for_a_fold",
                serde_json::json!({
                    "slot": n.slot_id, "seat": n.name, "urgent": n.urgent,
                    "folds_so_far": n.folds,
                })
                .to_string()
            ],
        );
    }
    tracing::info!(agent = %n.agent_id, slot = %n.slot_id, seat = %n.name,
                   urgent = n.urgent, folds = n.folds,
                   "asked this seat to fold what it no longer needs");
}

/// Ask that seat's CLI to compact itself, and say so where it will be read.
pub async fn compact(st: &AppState, full: Full) {
    tracing::info!(
        agent = %full.agent_id, slot = %full.slot_id, seat = %full.name,
        pct = full.pct, threshold = full.threshold, used = full.used,
        limit = full.limit,
        "context is over this seat's threshold; asking its CLI to compact"
    );
    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,?,?,?)",
            params![
                now_ms(),
                "watchdog",
                full.node_id,
                "compacted_on_context",
                serde_json::json!({
                    "slot": full.slot_id, "seat": full.name, "pct": full.pct,
                    "threshold": full.threshold, "used": full.used, "limit": full.limit,
                })
                .to_string()
            ],
        );
    }
    // The same path `/compact <agent>` uses, deliberately: the node resolves
    // what to type from the CLI's own descriptor, refuses while the screen
    // is waiting for an answer, and reports back into the chat either way.
    // `by` is what the seat is told, so an agent that finds itself compacted
    // knows it was a standing rule and not a person at a keyboard.
    // What the SEAT is told it was. "90% of 200000" reads as a fraction of
    // something the agent has no other way to see; "90% full" is the thing
    // it can act on.
    let by = format!(
        "nexmesh (context {}% full of {} tokens, this seat's threshold is {}%)",
        full.pct, full.limit, full.threshold
    );
    if let Err(e) = st
        .send_to_node(
            &full.node_id,
            &nexmesh_proto::HubToNode::Compact { agent_id: full.agent_id.clone(), by },
        )
        .await
    {
        tracing::warn!(agent = %full.agent_id, error = %e,
            "could not ask this seat to compact");
    }
}

/// An agent that has repeated itself past its threshold.
pub struct Looping {
    pub agent_id: String,
    pub slot_id: String,
    pub session_id: String,
    pub name: String,
    pub node_id: Option<String>,
    pub period: i64,
    pub repeats: i64,
    pub threshold: i64,
}

/// Record a cycle, and say whether it calls for stopping the agent.
pub fn record_loop(
    db: &Connection,
    fleet_repeats: i64,
    agent_id: &str,
    period: i64,
    repeats: i64,
) -> Result<Option<Looping>> {
    db.execute(
        "UPDATE agents SET loop_period = ?, loop_repeats = ? WHERE id = ?",
        params![period, repeats, agent_id],
    )?;
    let row: Option<(String, String, Option<String>, Option<String>, Option<i64>)> = db
        .query_row(
            "SELECT s.id, s.session_id, s.display_name, a.node_id, a.loop_stopped_at_ms
               FROM agents a JOIN slots s ON s.id = a.slot_id
              WHERE a.id = ? AND a.exited_at_ms IS NULL",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((slot_id, session_id, name, node_id, stopped)) = row else { return Ok(None) };
    let threshold = setting(db, &slot_id, "loop_guard_repeats", fleet_repeats)?;
    // Below two, "repeats" is one turn happening once, and a guard that
    // fires on that stops every agent on the fleet immediately.
    if threshold < 2 || repeats < threshold {
        return Ok(None);
    }
    // Once per agent. The seat is paused by this, so there is nothing to do
    // twice -- and an operator who looked at it and released it has made a
    // decision that the next reading must not overrule.
    if stopped.is_some() {
        return Ok(None);
    }
    db.execute(
        "UPDATE agents SET loop_stopped_at_ms = ? WHERE id = ?",
        params![now_ms(), agent_id],
    )?;
    Ok(Some(Looping {
        agent_id: agent_id.to_string(),
        slot_id: slot_id.clone(),
        session_id,
        name: name.unwrap_or(slot_id),
        node_id,
        period,
        repeats,
        threshold,
    }))
}

/// Stop an agent that is going round in circles, and tell somebody.
///
/// Three things, in this order, because each is useless without the one
/// before it: interrupt the turn it is on, so it stops spending; pause the
/// seat, so nothing starts it again; and put it in front of the operator,
/// because a paused seat nobody knows about is a team member who has
/// silently left.
///
/// Interrupting rather than killing. The conversation is worth keeping --
/// whatever drove it into a loop is in there, and it is the only way anybody
/// will work out what happened -- and an agent killed mid-edit takes its work
/// with it.
pub async fn halt(st: &AppState, it: Looping) {
    tracing::warn!(
        agent = %it.agent_id, slot = %it.slot_id, period = it.period,
        repeats = it.repeats, threshold = it.threshold,
        "this agent is repeating itself; interrupting and pausing its seat"
    );
    let what = if it.period == 1 {
        format!("the same turn {} times in a row", it.repeats)
    } else {
        format!(
            "the same {} turns over and over, {} times round",
            it.period, it.repeats
        )
    };
    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,?,?,?)",
            params![
                now_ms(),
                "watchdog",
                it.node_id,
                "stopped_on_loop",
                serde_json::json!({
                    "slot": it.slot_id, "period": it.period, "repeats": it.repeats,
                    "threshold": it.threshold,
                })
                .to_string()
            ],
        );
    }
    if let Some(node) = &it.node_id {
        if let Err(e) = st
            .send_to_node(
                node,
                &nexmesh_proto::HubToNode::Interrupt {
                    agent_id: it.agent_id.clone(),
                    by: format!("nexmesh (it did {what})"),
                },
            )
            .await
        {
            tracing::warn!(agent = %it.agent_id, error = %e,
                "could not interrupt a looping agent; pausing its seat anyway");
        }
    }
    let reason = format!("it did {what}");
    if let Err(e) = crate::control::set_paused(st, &it.session_id, &it.slot_id, true, Some(&reason))
        .await
    {
        tracing::error!(slot = %it.slot_id, error = %e,
            "could not pause a seat that is looping");
    }
    // To the operator, not to the room. A loop is not news the other agents
    // can act on, and it is the one thing the person running the fleet has
    // to decide about: whether the seat goes back to work.
    let body = format!(
        "{} was stopped: it did {what}, and this team's loop guard is set to {}. \
         Its seat is PAUSED and its conversation is intact -- look at what it was \
         repeating, then `/resume {}` when you want it back.",
        it.name, it.threshold, it.name
    );
    let db = st.db.lock().unwrap();
    // WHO gets told. The same rule the agents' own `notify_operator` uses:
    // a team with an operator token of its own has somebody driving it, and
    // this is theirs; a team with none is the fleet operator's business, and
    // `to_session IS NULL` is the inbox they read. Addressing it to the
    // session regardless would put it in a box nobody opens.
    //
    // Simpler than `notify_operator` in one way, on purpose: there is no
    // escalation here. An agent can judge that its orchestrator is the
    // problem; a watchdog cannot.
    let to_session: Option<String> = db
        .query_row(
            "SELECT session_id FROM operator_tokens
              WHERE session_id = ? AND revoked_at_ms IS NULL LIMIT 1",
            [&it.session_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let _ = db.execute(
        "INSERT INTO operator_messages
            (from_label, from_kind, from_session, to_session, body, sent_at_ms)
         VALUES (?, 'operator', ?, ?, ?, ?)",
        params!["nexmesh", it.session_id, to_session, body, now_ms()],
    );
}

// ------------------------------------------------------------- flooding ----

/// The window a pair's back-and-forth is counted over.
///
/// Ten minutes rather than one: a ping-pong is not fast, it is RELENTLESS.
/// Two agents answering each other every twenty seconds never trip a
/// per-minute rate -- each of them is sending perfectly reasonably -- and
/// between them they burn two turns a minute for as long as nobody looks.
const PAIR_WINDOW_MS: i64 = 10 * 60 * 1000;

/// How long before the operator is told about the same seat flooding again.
const FLOOD_QUIET_MS: i64 = 10 * 60 * 1000;

/// Which protection stopped a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flood {
    /// One seat is sending too much, to anybody.
    Rate,
    /// Two seats are talking only to each other.
    Pair,
}

/// A message that was not sent, and why.
pub struct Trip {
    pub kind: Flood,
    pub count: i64,
    pub limit: i64,
    /// The other seat, for a pair trip.
    pub other: Option<String>,
}

impl Trip {
    /// What the AGENT is told. It has to say what to do instead, or the next
    /// thing the agent does is rephrase and send it again.
    pub fn explain(&self, other_name: &str) -> String {
        match self.kind {
            Flood::Rate => format!(
                "Not sent: this seat has sent {} messages in the last minute and its \
                 limit is {}. That many messages is not a conversation -- it is a loop, \
                 and every one of them costs a teammate a turn. Do the work, or write \
                 what you have learnt down with `store_knowledge` where everybody can \
                 read it at once. If you are stuck on something the team cannot settle, \
                 `notify_operator` once.",
                self.count, self.limit
            ),
            Flood::Pair => format!(
                "Not sent: you and {other_name} have exchanged {} messages in the last \
                 ten minutes and the limit for one pair is {}. Two agents answering \
                 each other that many times have stopped working and are now only \
                 talking. Decide, or do your half and say so with `signal_done`; if the \
                 two of you genuinely cannot agree, `notify_operator` once and let the \
                 operator settle it.",
                self.count, self.limit
            ),
        }
    }
}

/// Would this message be a flood? `None` means send it.
///
/// Called on the one funnel every agent-sent message goes through, so there
/// is no way round it: `send_message`, `broadcast` and `reply` all end up
/// here. Counted from the `messages` table rather than from anything held in
/// memory, so a hub restart does not reset somebody's allowance and two hubs
/// could never disagree about it.
pub fn flood_check(
    db: &Connection,
    from_slot: &str,
    to_slot: Option<&str>,
    fleet_rate: i64,
    fleet_pair: i64,
) -> Result<Option<Trip>> {
    let now = now_ms();
    let rate = setting(db, from_slot, "msg_rate_per_min", fleet_rate)?;
    if rate > 0 {
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM messages WHERE from_slot = ? AND sent_at_ms > ?",
            params![from_slot, now - 60_000],
            |r| r.get(0),
        )?;
        if count >= rate {
            return Ok(Some(Trip { kind: Flood::Rate, count, limit: rate, other: None }));
        }
    }
    // A broadcast has no pair to be in. It is covered by the rate above, and
    // counting it against every possible partner would make one broadcast
    // trip every pair at once.
    let Some(to) = to_slot else { return Ok(None) };
    let pair = setting(db, from_slot, "msg_pair_limit", fleet_pair)?;
    if pair > 0 {
        // Both directions. A ping-pong is a property of the PAIR, and
        // counting only what this seat sent would let two agents have twice
        // the exchange anybody configured.
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM messages
              WHERE sent_at_ms > ?3
                AND ((from_slot = ?1 AND to_slot = ?2) OR (from_slot = ?2 AND to_slot = ?1))",
            params![from_slot, to, now - PAIR_WINDOW_MS],
            |r| r.get(0),
        )?;
        if count >= pair {
            return Ok(Some(Trip {
                kind: Flood::Pair,
                count,
                limit: pair,
                other: Some(to.to_string()),
            }));
        }
    }
    Ok(None)
}

/// Record a flood, and put it in front of the operator -- once.
///
/// Once because the agent will try again: it is in a loop, that is what a
/// loop is, and an operator sent one note per refused message would have a
/// flood of their own. The audit row is both the record and the "have we
/// said this recently" check, so there is no state to keep anywhere else.
pub fn note_flood(db: &Connection, session_id: &str, from_slot: &str, trip: &Trip, who: &str) {
    let kind = match trip.kind {
        Flood::Rate => "rate",
        Flood::Pair => "pair",
    };
    let said_recently: bool = db
        .query_row(
            "SELECT 1 FROM audit
              WHERE action = 'message_flood' AND at_ms > ?1
                AND detail LIKE ?2 LIMIT 1",
            params![now_ms() - FLOOD_QUIET_MS, format!("%\"{from_slot}\"%")],
            |_| Ok(true),
        )
        .optional()
        .unwrap_or(None)
        .unwrap_or(false);
    let _ = db.execute(
        "INSERT INTO audit (at_ms, actor, node_id, action, detail) VALUES (?,?,NULL,?,?)",
        params![
            now_ms(),
            "watchdog",
            "message_flood",
            serde_json::json!({
                "slot": from_slot, "kind": kind, "count": trip.count, "limit": trip.limit,
                "other": trip.other,
            })
            .to_string()
        ],
    );
    if said_recently {
        return;
    }
    tracing::warn!(slot = %from_slot, kind, count = trip.count, limit = trip.limit,
        "refusing messages from this seat: flood protection");
    let to_session: Option<String> = db
        .query_row(
            "SELECT session_id FROM operator_tokens
              WHERE session_id = ? AND revoked_at_ms IS NULL LIMIT 1",
            [session_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let body = match trip.kind {
        Flood::Rate => format!(
            "{who} is being refused messages: {} sent in the last minute, limit {}. \
             Its work is not blocked -- only its messages are -- but something has it \
             in a loop. Look at what it is sending.",
            trip.count, trip.limit
        ),
        Flood::Pair => format!(
            "{who} and one teammate have exchanged {} messages in ten minutes, limit \
             {}. They are talking rather than working; further messages between the \
             two of them are refused until it quietens down.",
            trip.count, trip.limit
        ),
    };
    let _ = db.execute(
        "INSERT INTO operator_messages
            (from_label, from_kind, from_session, to_session, body, sent_at_ms)
         VALUES (?, 'operator', ?, ?, ?, ?)",
        params!["nexmesh", session_id, to_session, body, now_ms()],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One team, one seat, one running agent.
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,first_seen_ms,last_seen_ms,
                                lease_expires_ms)
             VALUES ('machine-b','machine-b','linux','x86_64','t',1,1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,created_at_ms)
             VALUES ('sl1','s1','Ida',1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO agents (id,slot_id,node_id,provenance,status,started_at_ms,last_seen_ms)
             VALUES ('ag1','sl1','machine-b','managed','running',1,1)",
            [],
        )
        .unwrap();
        db
    }

    fn set(db: &Connection, table: &str, id: &str, column: &str, v: Option<i64>) {
        db.execute(
            &format!("UPDATE {table} SET {column} = ? WHERE id = ?"),
            params![v, id],
        )
        .unwrap();
    }

    // ---------------------------------------------------- the three states ----

    #[test]
    fn nothing_set_anywhere_is_off() {
        // The state every fleet is in after this hub is deployed.
        let db = fixture();
        assert_eq!(setting(&db, "sl1", "compact_at_pct", 0).unwrap(), 0);
    }

    #[test]
    fn the_fleet_default_applies_where_nothing_else_is_set() {
        let db = fixture();
        assert_eq!(setting(&db, "sl1", "compact_at_pct", 80).unwrap(), 80);
    }

    #[test]
    fn a_team_overrides_the_fleet_and_a_seat_overrides_its_team() {
        let db = fixture();
        set(&db, "sessions", "s1", "compact_at_pct", Some(70));
        assert_eq!(setting(&db, "sl1", "compact_at_pct", 80).unwrap(), 70);
        set(&db, "slots", "sl1", "compact_at_pct", Some(90));
        assert_eq!(setting(&db, "sl1", "compact_at_pct", 80).unwrap(), 90);
    }

    #[test]
    fn a_seat_can_opt_out_of_a_setting_the_fleet_and_its_team_have_on() {
        // The reason this is three states rather than two. "This one, leave
        // alone" is the first thing an operator wants and a plain integer
        // with no NULL cannot say it.
        let db = fixture();
        set(&db, "sessions", "s1", "compact_at_pct", Some(70));
        set(&db, "slots", "sl1", "compact_at_pct", Some(0));
        assert_eq!(setting(&db, "sl1", "compact_at_pct", 80).unwrap(), 0);
    }

    #[test]
    fn a_seat_that_is_not_there_is_off_rather_than_an_error() {
        let db = fixture();
        assert_eq!(setting(&db, "sl-gone", "compact_at_pct", 85).unwrap(), 0);
    }

    // ------------------------------------------------------------ context ----

    fn reading(db: &Connection, fleet: i64, used: i64, limit: i64) -> Next {
        record_context(db, fleet, "ag1", used, limit, "/tmp/conv.jsonl").unwrap()
    }

    /// The compaction, when a reading calls for one.
    fn ctx_reading(db: &Connection, fleet: i64, used: i64, limit: i64) -> Option<Full> {
        match reading(db, fleet, used, limit) {
            Next::Compact(f) => Some(f),
            _ => None,
        }
    }

    fn is_fold(n: &Next) -> bool {
        matches!(n, Next::Fold(_))
    }

    /// A seat over its threshold is asked to fold first, so a test about the
    /// COMPACTION has to get past that step. This is what a cooperative seat
    /// does: it is asked, it folds, and the compaction follows.
    fn fold_then(db: &Connection, fleet: i64, used: i64, limit: i64) -> Option<Full> {
        match reading(db, fleet, used, limit) {
            // Already past the asking -- it folded in an earlier episode, or
            // there was no room left to ask.
            Next::Compact(f) => Some(f),
            Next::Nothing => None,
            Next::Fold(_) => {
                crate::folds::fold(db, &crate::memory::Scope::Agent("sl1".into()),
                                   "sl1", Some("ag1"), "a stretch of work",
                                   "what it established", "the detail", &[], None)
                    .unwrap();
                ctx_reading(db, fleet, used, limit)
            }
        }
    }

    #[test]
    fn a_reading_is_recorded_even_when_nobody_asked_for_anything() {
        let db = fixture();
        assert!(matches!(reading(&db, 0, 150_000, 200_000), Next::Nothing), "off means off");
        let (used, limit, src): (i64, i64, String) = db
            .query_row(
                "SELECT context_used_tokens, context_limit_tokens, context_source
                   FROM agents WHERE id = 'ag1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((used, limit, src.as_str()), (150_000, 200_000, "/tmp/conv.jsonl"));
    }

    #[test]
    fn under_the_threshold_is_left_alone() {
        let db = fixture();
        assert!(ctx_reading(&db, 85, 169_000, 200_000).is_none());
    }

    #[test]
    fn a_seat_is_asked_to_fold_long_before_it_is_compacted() {
        // The point of the whole thing. A compaction replaces a context with
        // a summary of itself; a fold is written down somewhere that
        // outlives the context. So the fold is asked for first, well under
        // the threshold, and only once the seat has actually taken on more
        // work since it was last asked.
        let db = fixture();
        // The first reading only establishes where it started from: nothing
        // has grown yet, so nothing is asked.
        assert!(matches!(reading(&db, 85, 60_000, 200_000), Next::Nothing));
        // A little more is not another quarter of the window.
        assert!(matches!(reading(&db, 85, 70_000, 200_000), Next::Nothing),
                "a nudge on every reading is a reminder, not a habit");
        // Another quarter of the window IS.
        let n = reading(&db, 85, 115_000, 200_000);
        assert!(is_fold(&n), "it should have been asked to fold");
        if let Next::Fold(f) = n {
            assert!(!f.urgent, "nothing is waiting on it yet");
            assert_eq!(f.name, "Ida");
        }
        // And not again until it has grown that much AGAIN.
        assert!(matches!(reading(&db, 85, 120_000, 200_000), Next::Nothing));
    }

    #[test]
    fn nothing_is_asked_of_a_seat_that_is_barely_started() {
        // The newest part of a conversation is what the agent is working IN,
        // and summarising that is how it loses the thread.
        let db = fixture();
        reading(&db, 85, 1_000, 200_000);
        assert!(matches!(reading(&db, 85, 60_000, 200_000), Next::Nothing),
                "29% of a window has nothing in it worth folding");
    }

    #[test]
    fn over_the_threshold_the_fold_is_asked_for_urgently_and_then_it_compacts() {
        // Asked once, given a moment, and compacted whether or not anything
        // came of it: a full context is worse than a fold nobody wrote.
        let db = fixture();
        let n = reading(&db, 85, 180_000, 200_000);
        assert!(is_fold(&n), "the fold comes first");
        if let Next::Fold(f) = n {
            assert!(f.urgent, "a compaction is waiting on this one");
        }
        // Still inside the grace: nothing happens yet.
        assert!(matches!(reading(&db, 85, 181_000, 200_000), Next::Nothing));
        // The grace runs out with nothing folded, and the compaction goes
        // ahead regardless.
        db.execute(
            "UPDATE agents SET fold_asked_at_ms = ? WHERE id = 'ag1'",
            params![now_ms() - FOLD_GRACE_MS - 1],
        )
        .unwrap();
        assert!(ctx_reading(&db, 85, 181_000, 200_000).is_some(),
                "it must not wait for ever for a fold that is not coming");
    }

    #[test]
    fn a_seat_that_does_fold_is_compacted_at_once_rather_than_made_to_wait() {
        // Its work is already out of harm's way, so there is nothing left to
        // wait for.
        let db = fixture();
        assert!(is_fold(&reading(&db, 85, 180_000, 200_000)));
        crate::folds::fold(&db, &crate::memory::Scope::Agent("sl1".into()),
                           "sl1", Some("ag1"), "the migration",
                           "tenant_id, in batches of 5000", "the long version", &[], None)
            .unwrap();
        assert!(ctx_reading(&db, 85, 181_000, 200_000).is_some(),
                "it folded; there is nothing to wait for");
    }

    #[test]
    fn a_context_that_is_nearly_gone_is_compacted_without_asking_first() {
        // The emergency valve. At 95% there is no room left to write a fold
        // in, and a compaction that happens beats a fold that might not.
        let db = fixture();
        let n = reading(&db, 85, 191_000, 200_000);
        assert!(!is_fold(&n), "nothing is asked at 95%");
        assert!(matches!(n, Next::Compact(_)));
    }

    #[test]
    fn a_percentage_is_rounded_down_so_a_threshold_is_not_reached_early() {
        // 84.9% is not 85%.
        let db = fixture();
        assert!(ctx_reading(&db, 85, 169_999, 200_000).is_none());
        assert!(fold_then(&db, 85, 170_000, 200_000).is_some());
    }

    #[test]
    fn over_the_threshold_asks_once_and_not_again_straight_away() {
        // Compaction takes about a minute, during which every reading is
        // still over. Without the wait, one full seat is sent a /compact
        // every twenty seconds for as long as it takes.
        let db = fixture();
        let hit = fold_then(&db, 85, 180_000, 200_000).expect("over");
        assert_eq!((hit.pct, hit.threshold), (90, 85));
        assert_eq!(hit.name, "Ida");
        assert_eq!(hit.node_id, "machine-b");
        assert!(ctx_reading(&db, 85, 181_000, 200_000).is_none(), "not again yet");
    }

    #[test]
    fn a_seat_that_drops_back_under_is_asked_again_next_time_it_fills_up() {
        let db = fixture();
        assert!(fold_then(&db, 85, 180_000, 200_000).is_some());
        // It compacted.
        assert!(ctx_reading(&db, 85, 15_000, 200_000).is_none());
        // And filled up again, within the cooldown. This must still fire:
        // the cooldown exists to avoid asking twice for ONE filling-up, not
        // to cap how often a seat may be compacted.
        assert!(fold_then(&db, 85, 180_000, 200_000).is_some());
    }

    #[test]
    fn the_window_comes_from_the_node_and_zero_is_not_divided_by() {
        let db = fixture();
        assert!(matches!(reading(&db, 85, 180_000, 0), Next::Nothing));
    }

    #[test]
    fn a_seat_on_no_node_cannot_be_typed_at_so_it_is_not_asked() {
        let db = fixture();
        db.execute("UPDATE agents SET node_id = NULL WHERE id = 'ag1'", []).unwrap();
        assert!(matches!(reading(&db, 85, 199_000, 200_000), Next::Nothing));
    }

    #[test]
    fn an_agent_that_has_exited_is_not_asked_to_compact() {
        let db = fixture();
        db.execute("UPDATE agents SET exited_at_ms = 2 WHERE id = 'ag1'", []).unwrap();
        assert!(ctx_reading(&db, 85, 199_000, 200_000).is_none());
    }

    // -------------------------------------------------------------- loops ----

    #[test]
    fn a_cycle_is_recorded_even_with_the_guard_off() {
        let db = fixture();
        assert!(record_loop(&db, 0, "ag1", 1, 9).unwrap().is_none());
        let (p, r): (i64, i64) = db
            .query_row("SELECT loop_period, loop_repeats FROM agents WHERE id = 'ag1'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((p, r), (1, 9), "worth showing even when nothing is done about it");
    }

    #[test]
    fn fewer_repeats_than_the_threshold_is_not_a_loop() {
        let db = fixture();
        assert!(record_loop(&db, 5, "ag1", 1, 4).unwrap().is_none());
        assert!(record_loop(&db, 5, "ag1", 1, 5).unwrap().is_some());
    }

    #[test]
    fn a_threshold_below_two_is_refused_rather_than_obeyed() {
        // `1` would mean "one turn is a loop", which stops every agent on
        // the fleet the moment it is set.
        let db = fixture();
        set(&db, "slots", "sl1", "loop_guard_repeats", Some(1));
        assert!(record_loop(&db, 0, "ag1", 1, 50).unwrap().is_none());
    }

    #[test]
    fn an_agent_is_stopped_once_and_an_operators_release_stands() {
        let db = fixture();
        assert!(record_loop(&db, 3, "ag1", 1, 3).unwrap().is_some());
        assert!(
            record_loop(&db, 3, "ag1", 1, 9).unwrap().is_none(),
            "it is already paused; deciding again is the operator's job"
        );
    }

    // ----------------------------------------------------------- floods ----

    fn second_seat(db: &Connection) {
        db.execute(
            "INSERT INTO slots (id,session_id,display_name,created_at_ms)
             VALUES ('sl2','s1','Bob',1)",
            [],
        )
        .unwrap();
    }

    fn sent(db: &Connection, from: &str, to: Option<&str>, n: usize, ago_ms: i64) {
        for _ in 0..n {
            db.execute(
                "INSERT INTO messages (session_id, from_slot, to_slot, kind, body, sent_at_ms)
                 VALUES ('s1', ?, ?, 'text', 'x', ?)",
                params![from, to, now_ms() - ago_ms],
            )
            .unwrap();
        }
    }

    fn check(db: &Connection, rate: i64, pair: i64, to: Option<&str>) -> Option<Trip> {
        flood_check(db, "sl1", to, rate, pair).unwrap()
    }

    #[test]
    fn with_nothing_set_a_seat_may_send_whatever_it_likes() {
        // The state every fleet is in after this hub is deployed.
        let db = fixture();
        second_seat(&db);
        sent(&db, "sl1", Some("sl2"), 500, 100);
        assert!(check(&db, 0, 0, Some("sl2")).is_none());
    }

    #[test]
    fn a_seat_past_its_rate_is_refused() {
        let db = fixture();
        second_seat(&db);
        sent(&db, "sl1", Some("sl2"), 29, 100);
        assert!(check(&db, 30, 0, Some("sl2")).is_none(), "29 is under 30");
        sent(&db, "sl1", Some("sl2"), 1, 100);
        let t = check(&db, 30, 0, Some("sl2")).expect("30 is at the limit");
        assert_eq!((t.kind, t.count, t.limit), (Flood::Rate, 30, 30));
    }

    #[test]
    fn the_rate_is_the_last_minute_and_not_all_time() {
        // Otherwise a long-running seat is silenced for ever by messages it
        // sent this morning.
        let db = fixture();
        second_seat(&db);
        sent(&db, "sl1", Some("sl2"), 100, 120_000);
        assert!(check(&db, 10, 0, Some("sl2")).is_none());
    }

    #[test]
    fn two_seats_talking_only_to_each_other_are_stopped() {
        let db = fixture();
        second_seat(&db);
        // Ten each way over the window: neither of them is sending fast.
        sent(&db, "sl1", Some("sl2"), 10, 300_000);
        sent(&db, "sl2", Some("sl1"), 10, 200_000);
        assert!(
            check(&db, 1000, 0, Some("sl2")).is_none(),
            "with the pair limit off, a slow ping-pong is nobody's business"
        );
        let t = check(&db, 1000, 20, Some("sl2")).expect("20 exchanged, limit 20");
        assert_eq!((t.kind, t.count, t.limit), (Flood::Pair, 20, 20));
        assert_eq!(t.other.as_deref(), Some("sl2"));
    }

    #[test]
    fn a_pair_counts_both_directions() {
        // Counting only what this seat sent would let a pair have twice the
        // exchange anybody configured.
        let db = fixture();
        second_seat(&db);
        sent(&db, "sl1", Some("sl2"), 3, 1000);
        sent(&db, "sl2", Some("sl1"), 3, 1000);
        assert!(check(&db, 0, 6, Some("sl2")).is_some());
    }

    #[test]
    fn a_pair_limit_does_not_catch_a_seat_talking_to_the_whole_team() {
        // Nine messages to nine different seats is a coordinator working,
        // not a ping-pong.
        let db = fixture();
        second_seat(&db);
        for i in 0..9 {
            let other = format!("sl-o{i}");
            db.execute(
                "INSERT INTO slots (id,session_id,created_at_ms) VALUES (?,'s1',1)",
                [&other],
            )
            .unwrap();
            sent(&db, "sl1", Some(&other), 1, 1000);
        }
        assert!(check(&db, 0, 3, Some("sl2")).is_none());
    }

    #[test]
    fn a_broadcast_is_not_in_a_pair() {
        // It has no partner, and counting it against every possible one
        // would make a single broadcast trip every pair at once. The rate
        // still covers it.
        let db = fixture();
        sent(&db, "sl1", None, 50, 1000);
        assert!(check(&db, 0, 2, None).is_none());
        assert!(check(&db, 10, 2, None).is_some(), "the rate still applies");
    }

    #[test]
    fn a_seat_can_be_given_a_looser_rate_than_its_team() {
        // A coordinator legitimately sends more than anybody else.
        let db = fixture();
        second_seat(&db);
        set(&db, "sessions", "s1", "msg_rate_per_min", Some(5));
        set(&db, "slots", "sl1", "msg_rate_per_min", Some(100));
        sent(&db, "sl1", Some("sl2"), 20, 1000);
        assert!(check(&db, 0, 0, Some("sl2")).is_none());
    }

    #[test]
    fn the_refusal_tells_the_agent_what_to_do_instead() {
        // Without that, the next thing it does is rephrase and send again.
        let rate = Trip { kind: Flood::Rate, count: 40, limit: 30, other: None };
        let text = rate.explain("Bob");
        assert!(text.contains("store_knowledge") && text.contains("notify_operator"));
        let pair = Trip {
            kind: Flood::Pair,
            count: 20,
            limit: 20,
            other: Some("sl2".into()),
        };
        let text = pair.explain("Bob");
        assert!(text.contains("Bob"), "names the other seat: {text}");
        assert!(text.contains("signal_done") || text.contains("notify_operator"));
    }

    #[test]
    fn the_operator_is_told_once_and_not_once_per_refusal() {
        // The agent WILL try again -- that is what a loop is -- and an
        // operator sent one note per refused message has a flood of their
        // own.
        let db = fixture();
        let trip = Trip { kind: Flood::Rate, count: 40, limit: 30, other: None };
        for _ in 0..5 {
            note_flood(&db, "s1", "sl1", &trip, "Ida");
        }
        let notes: i64 = db
            .query_row("SELECT COUNT(*) FROM operator_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(notes, 1);
        // But every refusal is still recorded.
        let audited: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM audit WHERE action = 'message_flood'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audited, 5);
    }

    #[test]
    fn a_seat_can_turn_the_loop_guard_off_while_its_team_has_it_on() {
        let db = fixture();
        set(&db, "sessions", "s1", "loop_guard_repeats", Some(3));
        set(&db, "slots", "sl1", "loop_guard_repeats", Some(0));
        assert!(
            record_loop(&db, 0, "ag1", 1, 20).unwrap().is_none(),
            "a seat polling something until it changes is supposed to repeat itself"
        );
    }
}
