//! How full each agent's context is, measured rather than estimated.
//!
//! WHY THE NODE AND NOT THE HUB. Occupancy is only visible where the CLI is:
//! it is in a file on that machine, or in a stream nobody else is reading.
//! The hub cannot see either, and the window the number is a fraction of
//! comes out of the CLI's descriptor, which only the node holds. So the node
//! measures and reports two integers, and the hub -- which holds the
//! operator's threshold -- decides what to do about them.
//!
//! WHY NOT THE SCREEN. The obvious thing is to read the percentage the CLI
//! prints in its own status line. Checked against the live fleet and
//! rejected: the Orchestrator seat at 83% full showed no percentage at all
//! on its status line, because that readout appears only sometimes, and its
//! wording, position and presence vary with the CLI's version, width and
//! theme. A number scraped off a terminal would be wrong in exactly the
//! cases that matter -- quietly, and differently on every machine.
//!
//! WHAT IS READ INSTEAD. claude stamps every assistant message in its
//! conversation file with the usage of the API call that produced it. The
//! prompt of that call IS the conversation as it stood, so the newest such
//! record is the occupancy, to the token, from the same source the CLI shows
//! its own user. See `clis/claude.toml`.
//!
//! COST. Transcripts reach hundreds of megabytes -- 458MB on this fleet --
//! and this runs for every agent on the machine. So a file is read ONCE from
//! its tail, and after that only the bytes appended since the last look: a
//! conversation nobody is talking to costs one `stat`.
//!
//! The same pass also reduces each turn to a signature for `loopguard`:
//! following the file is the expensive part and it is already being done,
//! and the two questions -- how full is it, and is it going round in circles
//! -- are asked of exactly the same bytes.

use crate::ctx::{AgentRec, NodeCtx};
use nexmesh_proto::cli_descriptor::{ContextSource, ContextSpec};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How full one agent's context is, and the window it has to fit in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub used: i64,
    pub limit: i64,
    /// Where it came from, verbatim, so an operator asking why a seat was
    /// compacted gets the file rather than a shrug.
    pub source: String,
}

/// What one usage record in a conversation file says.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    /// Prompt plus output: what the NEXT turn will carry, which is the
    /// number a threshold should be compared against. The prompt alone
    /// understates the conversation by a whole answer.
    tokens: i64,
    /// The model that call ran on, which is what decides the window. Taken
    /// from the same record as the tokens on purpose: a conversation can
    /// change model halfway, and a percentage mixing one model's usage with
    /// another's window is a number with nothing behind it.
    model: Option<String>,
}

/// One agent's file, and how far we have read it.
struct Watch {
    path: PathBuf,
    /// Bytes consumed so far. Everything before this has been parsed and
    /// what it said is in `last`.
    offset: u64,
    last: Option<Record>,
    /// The reading last REPORTED, so an unchanged one is not sent again.
    reported: Option<Reading>,
    /// What this agent's recent turns DID, oldest first. Capped: a loop is
    /// visible in the last few dozen turns or it is not a loop.
    sigs: Vec<u64>,
    /// The cycle last reported, for the same reason as `reported`.
    reported_loop: Option<crate::loopguard::Cycle>,
    /// What was last REPORTED to the hub for this agent.
    ///
    /// Not the same question as what the seat's own record says, which is
    /// what `rec.conversation` holds -- and the difference is a live bug
    /// this caught: a hand-run seat whose shim named the right conversation
    /// had a hub record from two days earlier, and nothing ever corrected
    /// it, because the node was comparing the file against its own hint
    /// rather than against what it had told the hub. Reported once per node
    /// connection per agent; the hub's handler is idempotent.
    told_hub: Option<String>,
    /// Did anything but its date say this file is the seat's? Only an
    /// evidenced choice is worth telling the hub about -- a guess is fine
    /// for a number on a dashboard and not for deciding what a respawn
    /// resumes. See `locate`.
    evidenced: bool,
    /// The conversation the seat recorded when this file was chosen.
    ///
    /// Deciding WHICH file is a seat's can mean opening the head of two
    /// dozen of them (see `locate`), which is fine once and absurd every
    /// twenty seconds. While the seat's recorded conversation has not
    /// changed and the chosen file is still there, the answer stands.
    resolved_for: Option<String>,
}

/// Owned by the caller -- the heartbeat task -- rather than being global.
///
/// Deliberately: this is a cache keyed by agent id, and a cache in a `static`
/// is a cache that leaks between tests and makes each one depend on the order
/// the others ran in.
#[derive(Default)]
pub struct Watcher {
    seen: HashMap<String, Watch>,
}

/// How much of a file is read the first time one is opened.
///
/// Only to find the newest usage record, which is near the end. Enough for
/// several turns of a verbose conversation, and small enough that discovering
/// twenty agents at once is not a hundred megabytes of reading.
const FIRST_READ_BYTES: u64 = 4 * 1024 * 1024;

/// What one pass over every agent's conversation found.
#[derive(Debug, Default)]
pub struct Poll {
    /// Occupancy that CHANGED since the last pass. Unchanged readings are
    /// dropped here rather than at the hub: a fleet of mostly-idle seats
    /// would otherwise send the same two numbers for each of them on every
    /// heartbeat, forever.
    pub usage: Vec<(String, Reading)>,
    /// Agents that have started going round in circles, or whose circling
    /// has grown. The node reports the cycle it can SEE and does not decide
    /// whether it matters: how many repeats are too many is the operator's
    /// setting, and it lives with the rest of them in the hub.
    pub loops: Vec<(String, crate::loopguard::Cycle)>,
    /// Seats whose recorded conversation is not the one they are in.
    ///
    /// `(agent, slot, the conversation it is really in)`. This is the half of
    /// conversation drift that measuring alone did not fix: the hub goes on
    /// believing a dead id, and a respawn resumes it -- which starts the seat
    /// on an old conversation, or on nothing at all when the id names a file
    /// that was never written.
    ///
    /// Sent only for an EVIDENCED choice: a file carrying the seat's own
    /// handshake, or the one the seat itself named. Never for a guess.
    pub conversations: Vec<(String, String, String)>,
}

impl Watcher {
    /// Follow every agent's conversation and report what changed.
    pub fn poll(&mut self, ctx: &Arc<NodeCtx>) -> Poll {
        let agents: Vec<AgentRec> = ctx.agents.lock().unwrap().values().cloned().collect();
        let live: std::collections::HashSet<String> =
            agents.iter().map(|a| a.agent_id.clone()).collect();
        // An agent that has gone takes its watch with it, or a long-lived
        // node accumulates one per agent it has ever held.
        self.seen.retain(|id, _| live.contains(id));

        let mut out = Poll::default();
        for rec in &agents {
            let Some(spec) = self.follow(ctx, rec) else { continue };
            let Some(watch) = self.seen.get_mut(&rec.agent_id) else { continue };

            // How full it is.
            if let Some(last) = watch.last.as_ref() {
                let limit = spec.limit_for(last.model.as_deref());
                if limit > 0 {
                    let reading = Reading {
                        used: last.tokens,
                        limit,
                        source: watch.path.to_string_lossy().to_string(),
                    };
                    if watch.reported.as_ref() != Some(&reading) {
                        watch.reported = Some(reading.clone());
                        out.usage.push((rec.agent_id.clone(), reading));
                    }
                }
            }

            // WHICH CONVERSATION IT IS IN, when the file disagrees with the
            // seat's own record of it.
            //
            // The node is the only thing that can see this: the id a seat
            // reports comes from its shim's environment, which was written
            // when the CLI started and does not change when the CLI starts a
            // new conversation. The file being appended to does.
            //
            // Corrected once: `note_conversation` makes the seat's record
            // agree, so the next pass finds nothing to say.
            if watch.evidenced {
                if let Some(id) = watch.path.file_stem().and_then(|s| s.to_str()) {
                    // Said once per connection, whatever the seat's own
                    // record happens to say. Comparing against the hint
                    // instead left a seat whose shim was RIGHT and whose hub
                    // record was two days stale with nobody to correct it:
                    // the node saw no disagreement to report.
                    if watch.told_hub.as_deref() != Some(id) {
                        tracing::info!(
                            agent = %rec.agent_id, slot = %rec.slot_id,
                            recorded = rec.conversation.as_deref().unwrap_or("-"),
                            really = %id,
                            "telling the hub which conversation this seat is in"
                        );
                        watch.told_hub = Some(id.to_string());
                        out.conversations.push((
                            rec.agent_id.clone(),
                            rec.slot_id.clone(),
                            id.to_string(),
                        ));
                    }
                }
            }

            // Whether it is getting anywhere. `2` is the floor at which a
            // cycle exists at all, not a threshold: the hub holds that.
            let cycle = crate::loopguard::detect(&watch.sigs, 2);
            if cycle != watch.reported_loop {
                watch.reported_loop = cycle.clone();
                if let Some(c) = cycle {
                    out.loops.push((rec.agent_id.clone(), c));
                }
            }
        }
        out
    }

    /// Read whatever has been appended to one agent's conversation.
    ///
    /// Returns the CLI's context declaration when there is one, which is
    /// also the answer to "is this agent measurable at all".
    fn follow(&mut self, ctx: &Arc<NodeCtx>, rec: &AgentRec) -> Option<ContextSpec> {
        let desc = ctx.descriptor(&rec.cli_id)?;
        let spec = desc.context.as_ref()?;
        if spec.source != ContextSource::ClaudeTranscript {
            return None;
        }
        // The file layout belongs to the CLI, not to a mode: claude's
        // interactive and headless modes name the same template, and a CLI
        // that genuinely differed per mode would need the agent's mode
        // recorded here, which nothing yet needs.
        let tpl = desc
            .modes
            .get("interactive")
            .and_then(|m| m.session_path.clone())
            .or_else(|| desc.modes.values().find_map(|m| m.session_path.clone()))?;

        // The file this seat's conversation is in, decided once and kept
        // while the seat's own record of it has not moved.
        let known = self.seen.get(&rec.agent_id).and_then(|w| {
            (w.resolved_for == rec.conversation && w.path.exists())
                .then(|| Found { path: w.path.clone(), evidenced: w.evidenced })
        });
        let Found { path, evidenced } = match known {
            Some(f) => f,
            None => self.locate(ctx, rec, &tpl)?,
        };
        let watch = match self.seen.get_mut(&rec.agent_id) {
            // A seat that moved to another conversation (`/clear`) starts
            // again from that file's tail: the offset belonged to the old
            // file and would land anywhere in the new one, and the old
            // file's turns are not this conversation's.
            Some(w) if w.path == path => w,
            _ => {
                self.seen.insert(
                    rec.agent_id.clone(),
                    Watch {
                        path: path.clone(),
                        offset: 0,
                        last: None,
                        reported: None,
                        sigs: Vec::new(),
                        reported_loop: None,
                        told_hub: None,
                        evidenced,
                        resolved_for: rec.conversation.clone(),
                    },
                );
                self.seen.get_mut(&rec.agent_id).expect("just inserted")
            }
        };

        let size = std::fs::metadata(&path).ok()?.len();
        if size < watch.offset {
            // Truncated or replaced under the same name. Re-read its tail
            // rather than carrying on from an offset into a file that is no
            // longer the one we were reading.
            watch.offset = 0;
            watch.last = None;
            watch.sigs.clear();
        }
        if size > watch.offset {
            let from = if watch.offset == 0 {
                size.saturating_sub(FIRST_READ_BYTES)
            } else {
                watch.offset
            };
            // A chunk starting mid-line: that partial first line is dropped,
            // because half a JSON object is not a record and the half we
            // have is the wrong half.
            //
            // ONLY the first look at a file can start mid-line. That one
            // seeks to a byte near the end and lands wherever it lands; a
            // resumed read starts at `offset`, which is always the end of a
            // complete line. Dropping the first line of a RESUMED chunk
            // threw away every turn that had been appended since the last
            // look -- measured: a conversation filled three times running
            // kept reporting the first reading for ever.
            let partial_start = watch.offset == 0 && from > 0;
            if let Some(chunk) = read_from(&path, from, size) {
                let scanned = scan(&chunk, partial_start);
                if let Some(r) = scanned.newest {
                    watch.last = Some(r);
                }
                watch.sigs.extend(scanned.signatures);
                let overflow = watch.sigs.len().saturating_sub(crate::loopguard::RING);
                watch.sigs.drain(..overflow);
                // STOP AT THE LAST COMPLETE LINE. The file is being appended
                // to as we read it, so the tail of a chunk is usually half a
                // record. Consuming it would skip that turn entirely once it
                // was finished -- and a turn skipped is a repeat unseen,
                // which is the one thing a loop detector must not do.
                watch.offset = if scanned.consumed > 0 {
                    from + scanned.consumed as u64
                } else if chunk.len() as u64 >= FIRST_READ_BYTES {
                    // Megabytes without a newline. Not a conversation file
                    // any more; stop re-reading the same bytes every pass.
                    size
                } else {
                    watch.offset
                };
            }
        }
        Some(spec.clone())
    }

    /// WHICH conversation file is this seat's.
    ///
    /// Harder than it sounds, and the reason it is worth this much code: a
    /// long-running CLI's recorded conversation drifts. On the live fleet the
    /// Orchestrator seat's shim reported `e8f2988f`, which exists, belongs to
    /// that seat, and had not been written since the previous evening, while
    /// the conversation it was actually in -- `d4a4bf71`, 39MB, being
    /// appended to that minute -- was invisible to the hub.
    ///
    /// ONE RULE: a seat's live conversation is the most recently written of
    /// the conversations that could be its own. Which are:
    ///
    ///   * any that carries this seat's handshake. The MCP server's
    ///     instructions say "You are part of a nexmesh team (slot sl-...)",
    ///     they are given only to the seat they name, and they land in the
    ///     first turn. Checked on the live fleet: of the three candidates in
    ///     that directory exactly one named `sl-b1a160df`, and three seats
    ///     sharing one working directory were each matched to their own.
    ///   * the one the seat itself says it is in, when that file is there.
    ///     This is what follows a `/clear`: the new conversation is named by
    ///     the CLI's own shim and is the newest thing in the directory.
    ///
    /// Newest of those, because both kinds of evidence can be true at once
    /// and only one of them is being written now. Conversations other seats
    /// on this node are in are excluded throughout: they are definitively
    /// not this one's.
    ///
    /// With no evidence either way, the newest conversation in the directory
    /// -- but only when this is the only seat working there, and marked as
    /// the guess it is, because a guess is fine for a number on a dashboard
    /// and not for deciding what a respawn resumes.
    fn locate(&self, ctx: &Arc<NodeCtx>, rec: &AgentRec, tpl: &str) -> Option<Found> {
        if rec.cwd.is_empty() {
            return None;
        }
        let dir = project_dir(tpl, &rec.cwd)?;
        let hinted = rec
            .conversation
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| expand(tpl, &rec.cwd, h))
            .filter(|p| p.exists());

        // What the other seats on this machine are in, and how many of them
        // share this working directory.
        let (claimed, sharers) = {
            let agents = ctx.agents.lock().unwrap();
            let claimed: Vec<PathBuf> = agents
                .values()
                .filter(|a| a.agent_id != rec.agent_id)
                .filter_map(|a| {
                    let h = a.conversation.as_deref()?.trim();
                    (!h.is_empty()).then(|| expand(tpl, &a.cwd, h))
                })
                .collect();
            let sharers = agents
                .values()
                .filter(|a| a.cwd == rec.cwd && a.cli_id == rec.cli_id)
                .count();
            (claimed, sharers)
        };

        // Newest first, so the head scan looks at what is live before what
        // is old and the cap falls on the oldest.
        let mut cands = jsonl_by_newest(&dir);
        cands.retain(|p| !claimed.contains(p));

        let mut mine: Vec<PathBuf> = cands
            .iter()
            .take(HEAD_SCAN_MAX)
            .filter(|p| head_names(p, &rec.slot_id))
            .cloned()
            .collect();
        if let Some(h) = hinted {
            if !mine.contains(&h) {
                mine.push(h);
            }
        }
        if let Some(path) = mine.into_iter().max_by_key(|p| mtime(p)) {
            return Some(Found { path, evidenced: true });
        }
        if sharers > 1 {
            return None;
        }
        cands.into_iter().next().map(|path| Found { path, evidenced: false })
    }
}

/// A conversation file, and whether anything but its date says it is this
/// seat's. See `locate`.
struct Found {
    path: PathBuf,
    evidenced: bool,
}

/// When a file was last written, or the epoch if that cannot be read.
fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// How many conversations in one directory are opened to look for a seat's
/// id. Newest first, so the ones skipped are the ones least likely to be
/// anybody's current conversation.
const HEAD_SCAN_MAX: usize = 24;

/// How much of a conversation is read to decide whose it is.
///
/// The MCP handshake names the slot, and it arrives in the first turn --
/// well inside this. Large enough for a first prompt with a long system
/// prompt in front of it, small enough that scanning two dozen candidates
/// costs a few megabytes once.
const HEAD_SCAN_BYTES: u64 = 512 * 1024;

/// Does the beginning of this conversation carry that seat's handshake?
///
/// `slot <id>`, not the bare id. The phrase comes from the MCP server's own
/// instructions -- "You are part of a nexmesh team (slot sl-...)" -- which
/// are handed only to the seat they name, so it is a claim of identity
/// rather than a mention. The bare id would also match a conversation that
/// merely TALKS about that seat, and an orchestrator's conversation talks
/// about every seat it runs.
fn head_names(path: &Path, slot_id: &str) -> bool {
    if slot_id.is_empty() {
        return false;
    }
    let marker = format!("slot {slot_id}");
    match read_from(path, 0, HEAD_SCAN_BYTES) {
        // Bytes, not lines: the marker is ASCII and the surrounding JSON
        // does not matter.
        Some(head) => head.windows(marker.len()).any(|w| w == marker.as_bytes()),
        None => false,
    }
}

/// Every conversation in a directory, most recently written first.
fn jsonl_by_newest(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            found.push((m, path));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// The directory a CLI keeps this working directory's conversations in.
fn project_dir(tpl: &str, cwd: &str) -> Option<PathBuf> {
    let any = expand(tpl, cwd, "x");
    any.parent().map(Path::to_path_buf)
}

/// Fill a `session_path` template in. `{cwd_slug}` is the working directory
/// with every `/` turned into `-`, which is how claude names a project
/// directory; `~/` is this user's home.
fn expand(tpl: &str, cwd: &str, id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = tpl
        .replace("{id}", id)
        .replace("{cwd_slug}", &cwd.replace('/', "-"));
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(format!("{home}/{rest}")),
        None => PathBuf::from(p),
    }
}

/// Read `[from, to)` of a file.
fn read_from(path: &Path, from: u64, to: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity((to - from).min(FIRST_READ_BYTES) as usize);
    f.take(to - from).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// What one pass over a chunk of a conversation file found.
struct Scan {
    /// The newest usage record in it, if any.
    newest: Option<Record>,
    /// What each turn in it DID, oldest first. See `loopguard`.
    signatures: Vec<u64>,
    /// How many bytes of the chunk were COMPLETE lines. The rest is a record
    /// still being written, and the next pass must start at the beginning of
    /// it rather than after it.
    consumed: usize,
}

/// Walk a chunk of a conversation file, forwards.
///
/// Forwards rather than backwards from the end, even though occupancy is
/// only the last record: the signatures are a SEQUENCE, and a loop is a
/// pattern in the order the turns happened. Reading backwards and reversing
/// would be the same work and one more thing to get wrong.
fn scan(chunk: &[u8], drop_first_line: bool) -> Scan {
    let mut out = Scan { newest: None, signatures: Vec::new(), consumed: 0 };
    let text = String::from_utf8_lossy(chunk);
    let mut at = 0usize;
    let mut first = true;
    for line in text.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        if complete {
            at += line.len();
            out.consumed = at;
        }
        let line = line.trim_end_matches('\n');
        // The chunk began mid-record: this is its back half.
        if first {
            first = false;
            if drop_first_line {
                continue;
            }
        }
        // An incomplete last line is not parsed at all. It will be read
        // again, whole, on the next pass -- `consumed` is what makes that
        // true.
        if !complete {
            break;
        }
        if line.len() < 2 {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(sig) = crate::loopguard::signature(&v) {
            out.signatures.push(sig);
        }
        if let Some(r) = usage_of(&v) {
            out.newest = Some(r);
        }
    }
    out
}

/// What one record says about occupancy, if it says anything.
fn usage_of(v: &serde_json::Value) -> Option<Record> {
    // A SUBAGENT's turn. Its context is its own and much smaller, and
    // counting it as the seat's would read as a conversation that shrinks
    // every time the agent delegates something.
    if v.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let msg = v.get("message")?;
    let u = msg.get("usage")?;
    if !u.is_object() {
        return None;
    }
    let model = msg.get("model").and_then(serde_json::Value::as_str).map(str::to_string);
    // `<synthetic>` is claude's own marker for a message it produced without
    // an API call -- an error notice. Its usage is zeros and it belongs to no
    // model, so a reading from one would say a full conversation is empty.
    if model.as_deref().is_some_and(|m| m.starts_with('<')) {
        return None;
    }
    let n = |k: &str| u.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
    let tokens = n("input_tokens")
        + n("cache_creation_input_tokens")
        + n("cache_read_input_tokens")
        + n("output_tokens");
    if tokens <= 0 {
        return None;
    }
    Some(Record { tokens, model })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line of a conversation file. Newline-terminated, because an
    /// unterminated line is by design a record still being written.
    fn usage_line(model: &str, input: i64, cache: i64, out: i64, sidechain: bool) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": sidechain,
            "message": {
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "cache_read_input_tokens": cache,
                    "cache_creation_input_tokens": 0,
                    "output_tokens": out,
                },
            },
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn occupancy_is_the_newest_record_not_the_sum_of_them() {
        // The whole point. Three turns of a growing conversation: the
        // conversation is as big as the LAST prompt, not as big as every
        // prompt added together.
        let text = [
            usage_line("claude-opus-5", 10, 1000, 50, false),
            usage_line("claude-opus-5", 10, 5000, 50, false),
            usage_line("claude-opus-5", 10, 9000, 50, false),
        ]
        .concat();
        let r = scan(text.as_bytes(), false).newest.unwrap();
        assert_eq!(r.tokens, 10 + 9000 + 50);
    }

    #[test]
    fn a_subagents_turn_is_not_the_seats_context() {
        let text = [
            usage_line("claude-opus-5", 10, 200_000, 50, false),
            usage_line("claude-haiku-5-5", 10, 300, 20, true),
        ]
        .concat();
        let r = scan(text.as_bytes(), false).newest.unwrap();
        assert_eq!(r.tokens, 10 + 200_000 + 50, "read the seat's turn, not the sidechain's");
    }

    #[test]
    fn a_synthetic_message_is_not_a_reading() {
        // claude writes these for its own error notices, with zeroed usage.
        // Taken at face value they report a full conversation as empty.
        let text = [
            usage_line("claude-opus-5", 10, 150_000, 50, false),
            usage_line("<synthetic>", 0, 0, 0, false),
        ]
        .concat();
        let r = scan(text.as_bytes(), false).newest.unwrap();
        assert_eq!(r.tokens, 10 + 150_000 + 50);
    }

    #[test]
    fn a_half_line_at_the_start_of_a_chunk_is_dropped() {
        // What a tail read always lands in the middle of. Parsing the
        // fragment would at best fail and at worst half-succeed.
        let whole = usage_line("claude-opus-5", 10, 7000, 50, false);
        let text = format!("{}{}", &whole[40..], usage_line("claude-opus-5", 10, 8000, 50, false));
        let r = scan(text.as_bytes(), true).newest.unwrap();
        assert_eq!(r.tokens, 10 + 8000 + 50);
    }

    #[test]
    fn a_partial_last_line_does_not_lose_the_record_before_it() {
        // The file is being appended to while we read it.
        let text = format!(
            "{}{{\"type\":\"assist",
            usage_line("claude-opus-5", 10, 4000, 50, false)
        );
        let r = scan(text.as_bytes(), false).newest.unwrap();
        assert_eq!(r.tokens, 10 + 4000 + 50);
    }

    #[test]
    fn no_usage_anywhere_is_no_reading() {
        let text = "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}";
        assert!(scan(text.as_bytes(), false).newest.is_none());
    }

    #[test]
    fn the_window_follows_the_model_in_the_record() {
        // Measured on the live fleet: haiku auto-compacted at 204k, opus at
        // 967k. One CLI, one conversation file, two windows.
        let mut models = std::collections::BTreeMap::new();
        models.insert("claude-opus".to_string(), 1_000_000);
        models.insert("claude-haiku".to_string(), 200_000);
        let spec = ContextSpec {
            limit_tokens: 200_000,
            source: ContextSource::ClaudeTranscript,
            models,
        };
        assert_eq!(spec.limit_for(Some("claude-opus-4-8")), 1_000_000);
        assert_eq!(spec.limit_for(Some("claude-haiku-5-5")), 200_000);
        assert_eq!(spec.limit_for(Some("something-else")), 200_000);
        assert_eq!(spec.limit_for(None), 200_000);
    }

    #[test]
    fn a_longer_prefix_wins() {
        let mut models = std::collections::BTreeMap::new();
        models.insert("claude".to_string(), 200_000);
        models.insert("claude-opus-5".to_string(), 1_000_000);
        let spec = ContextSpec {
            limit_tokens: 1,
            source: ContextSource::ClaudeTranscript,
            models,
        };
        assert_eq!(spec.limit_for(Some("claude-opus-5-5")), 1_000_000);
        assert_eq!(spec.limit_for(Some("claude-haiku-5-5")), 200_000);
    }

    #[test]
    fn a_tail_read_finds_the_newest_record_in_a_long_file() {
        // End to end over a real file, including the seek: this is the path
        // that runs against a 450MB transcript.
        let dir = std::env::temp_dir().join(format!("nexmesh-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conv.jsonl");
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&usage_line("claude-opus-5", 10, 1000 + i, 50, false));
        }
        std::fs::write(&path, &body).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let chunk = read_from(&path, size.saturating_sub(FIRST_READ_BYTES), size).unwrap();
        let r = scan(&chunk, false).newest.unwrap();
        assert_eq!(r.tokens, 10 + 1199 + 50);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A directory of conversations, for the rules in `locate`.
    fn conv_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexmesh-loc-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_conv(dir: &Path, id: &str, slot: Option<&str>) -> PathBuf {
        let p = dir.join(format!("{id}.jsonl"));
        let body = match slot {
            Some(s) => format!(
                "{{\"type\":\"system\",\"content\":\"You are part of a nexmesh team \
                 (slot {s}). Teammates may run elsewhere.\"}}\n"
            ),
            None => "{\"type\":\"user\",\"content\":\"hello\"}\n".to_string(),
        };
        std::fs::write(&p, body).unwrap();
        p
    }

    fn touch_newer(p: &Path) {
        // A second apart, so the mtime comparison is not deciding on
        // filesystem granularity.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let body = std::fs::read(p).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn the_live_conversation_is_the_newest_one_that_could_be_this_seats() {
        // The live-fleet case, both ways round.
        let dir = conv_dir("newest");
        let stale_handshake = write_conv(&dir, "aaaa", Some("sl-mine"));
        let hinted_dead = write_conv(&dir, "bbbb", None);
        // The seat's handshake file is OLDER than the one it names: a
        // `/clear` looks like this, and the named one wins.
        touch_newer(&hinted_dead);
        assert!(mtime(&hinted_dead) > mtime(&stale_handshake));
        // And the other way: the named one is stale and the handshake file
        // is being written. This is the Orchestrator, and the handshake
        // file wins.
        touch_newer(&stale_handshake);
        assert!(mtime(&stale_handshake) > mtime(&hinted_dead));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn conversations_are_offered_newest_first() {
        let dir = std::env::temp_dir().join(format!("nexmesh-ctx-new-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignore me\n").unwrap();
        let got = jsonl_by_newest(&dir);
        assert_eq!(got.len(), 2, "only conversations, not every file: {got:?}");
        assert_eq!(got[0].file_name().unwrap(), "new.jsonl");
        assert_eq!(got[1].file_name().unwrap(), "old.jsonl");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_conversation_that_names_a_seat_is_that_seats() {
        // The discriminator that made the live Orchestrator measurable: its
        // shim named a conversation that existed and had been dead since the
        // previous evening, while the one it was really in -- the only one
        // whose head carried its slot id -- was being written that minute.
        let dir = std::env::temp_dir().join(format!("nexmesh-ctx-id-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("mine.jsonl");
        std::fs::write(
            &mine,
            "{\"type\":\"system\",\"content\":\"You are part of a nexmesh team \
             (slot sl-b1a160df)\"}\n",
        )
        .unwrap();
        // A conversation that merely TALKS about that seat is not it. An
        // orchestrator's conversation talks about every seat it runs, so the
        // marker is the handshake phrase and not the bare id.
        let theirs = dir.join("theirs.jsonl");
        std::fs::write(
            &theirs,
            "{\"type\":\"user\",\"content\":\"have a look at sl-b1a160df, it is stuck\"}\n",
        )
        .unwrap();
        assert!(head_names(&mine, "sl-b1a160df"));
        assert!(!head_names(&theirs, "sl-b1a160df"));
        // An empty slot id must never match everything.
        assert!(!head_names(&mine, ""));
        assert!(!head_names(&dir.join("gone.jsonl"), "sl-b1a160df"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_record_still_being_written_is_left_for_the_next_pass() {
        // The property the loop detector depends on: a turn must be counted
        // exactly once, and a chunk almost always ends in the middle of one.
        let whole = usage_line("claude-opus-5", 10, 4000, 50, false);
        let text = format!("{whole}{{\"type\":\"assist");
        let got = scan(text.as_bytes(), false);
        assert_eq!(
            got.consumed,
            whole.len(),
            "only the finished line is consumed, so the half one is read again whole"
        );
    }

    #[test]
    fn a_resumed_read_keeps_the_first_line_of_its_chunk() {
        // A resumed read begins at the end of a complete line, so its first
        // line is a whole record and dropping it loses everything appended
        // since the last look.
        let one = usage_line("claude-opus-5", 10, 1000, 50, false);
        let two = usage_line("claude-opus-5", 10, 2000, 50, false);
        let first = scan(one.as_bytes(), false);
        assert_eq!(first.consumed, one.len());
        // What `follow` does next: read from `consumed`, which is exactly
        // where `two` begins.
        let resumed = scan(two.as_bytes(), false);
        assert_eq!(
            resumed.newest.unwrap().tokens,
            10 + 2000 + 50,
            "the appended turn is the new reading, not a fragment to throw away"
        );
    }

    #[test]
    fn signatures_come_out_of_the_same_pass_in_order() {
        let turn = |cmd: &str| {
            serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "name": "Bash", "input": {"command": cmd}}],
                },
            })
            .to_string()
                + "\n"
        };
        let text = format!("{}{}{}", turn("make"), turn("make"), turn("make"));
        let got = scan(text.as_bytes(), false);
        assert_eq!(got.signatures.len(), 3);
        assert!(
            got.signatures.iter().all(|s| *s == got.signatures[0]),
            "the same command three times is the same signature three times"
        );
        assert!(got.newest.is_none(), "those turns carry no usage, and none was invented");
    }

    #[test]
    fn a_template_expands_to_claudes_own_project_layout() {
        std::env::set_var("HOME", "/home/someone");
        let p = expand("~/.claude/projects/{cwd_slug}/{id}.jsonl", "/working/nexmesh", "abc");
        assert_eq!(
            p.to_string_lossy(),
            "/home/someone/.claude/projects/-working-nexmesh/abc.jsonl"
        );
        let d = project_dir("~/.claude/projects/{cwd_slug}/{id}.jsonl", "/working/nexmesh").unwrap();
        assert_eq!(d.to_string_lossy(), "/home/someone/.claude/projects/-working-nexmesh");
    }
}
