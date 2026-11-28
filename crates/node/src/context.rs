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
use nexmesh_proto as proto;
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
    /// The window, when the record itself says (codex's
    /// `model_context_window`). Wins over the descriptor.
    window: Option<i64>,
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
    /// What the newest API call in the file came to: refused for want of
    /// allowance or a login (`Some(Some(block))`), or answered
    /// (`Some(None)`). `None` until a call has been seen.
    outcome: Option<Option<proto::UsageBlock>>,
    /// What the hub was last told about it, so it is told once.
    told_block: Option<proto::UsageBlock>,
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
    /// Files read in turns appended since the last report. Never counted
    /// from a file's first look, which reads its tail -- that is history,
    /// not what the seat is doing now.
    reads: Activity,
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
    /// Agents that can no longer reach their model, or (`None`) can again.
    pub blocks: Vec<(String, Option<proto::UsageBlock>)>,
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
    /// Files each agent has read since the last pass. See `file_reads`.
    pub reads: Vec<(String, Activity)>,
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
                let limit = last.window.filter(|w| *w > 0)
                    .unwrap_or_else(|| spec.limit_for(last.model.as_deref()));
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

            // WHETHER IT CAN STILL REACH ITS MODEL. Told once when it cannot,
            // and once when a call goes through again. A refusal whose reset
            // has already passed is history, not news -- the first look at a
            // file reads its tail, which can hold one from days ago.
            let now_blocked = match &watch.outcome {
                Some(Some(b)) if b.reset_at_ms.map_or(true, |at| at > proto::now_ms()) => {
                    Some(b.clone())
                }
                _ => None,
            };
            if now_blocked != watch.told_block {
                match (&now_blocked, &watch.told_block) {
                    (Some(b), _) => out.blocks.push((rec.agent_id.clone(), Some(b.clone()))),
                    (None, Some(_)) => out.blocks.push((rec.agent_id.clone(), None)),
                    (None, None) => {}
                }
                watch.told_block = now_blocked;
            }

            if !watch.reads.is_empty() {
                out.reads.push((rec.agent_id.clone(), std::mem::take(&mut watch.reads)));
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
        match spec.source {
            ContextSource::ClaudeTranscript => {}
            ContextSource::CodexRollout | ContextSource::OpencodeDb => {
                return self.follow_store(rec, spec);
            }
            ContextSource::None => return None,
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
                        outcome: None,
                        told_block: None,
                        sigs: Vec::new(),
                        reported_loop: None,
                        told_hub: None,
                        evidenced,
                        reads: Activity::default(),
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
                if let Some(o) = scanned.outcome {
                    watch.outcome = Some(o);
                }
                watch.sigs.extend(scanned.signatures);
                if watch.offset > 0 {
                    watch.reads.add(scanned.reads);
                }
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

    /// Occupancy for a CLI whose seat has a store of its own (codex,
    /// opencode): the newest usage record in it.
    ///
    /// Simpler than a claude transcript, on purpose. The store is this
    /// seat's alone, so there is no question of WHICH conversation is its;
    /// and only occupancy is read -- no reads, no loop signatures -- because
    /// those are counted from claude's tool records, which these CLIs do not
    /// write in that shape. Re-read only when the file's size or time moved.
    fn follow_store(&mut self, rec: &AgentRec, spec: &ContextSpec) -> Option<ContextSpec> {
        let base = crate::spawn::seat_store_base(&rec.cli_id, &rec.slot_id)?;
        let path = match spec.source {
            ContextSource::CodexRollout => newest_rollout(Path::new(&base).join("sessions").as_path())?,
            _ => Path::new(&base).join("opencode").join("opencode.db"),
        };
        // The database is written through its WAL: the main file's stamp
        // stands still for minutes while every message lands in -wal.
        let stamp_of = |p: &Path| {
            std::fs::metadata(p).ok().map(|m| {
                let t = m.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                m.len() ^ t
            })
        };
        let mut stamp = stamp_of(&path)?;
        if spec.source == ContextSource::OpencodeDb {
            let wal = path.with_file_name("opencode.db-wal");
            stamp ^= stamp_of(&wal).unwrap_or(0).rotate_left(17);
        }
        let watch = self.seen.entry(rec.agent_id.clone()).or_insert_with(|| Watch {
            path: path.clone(),
            offset: u64::MAX,
            last: None,
            reported: None,
            outcome: None,
            told_block: None,
            sigs: Vec::new(),
            reported_loop: None,
            told_hub: None,
            // Never told to the hub as a conversation: see `poll`.
            evidenced: false,
            reads: Activity::default(),
            resolved_for: None,
        });
        if watch.path == path && watch.offset == stamp {
            return Some(spec.clone());
        }
        watch.path = path.clone();
        watch.offset = stamp;
        let found = match spec.source {
            ContextSource::CodexRollout => codex_usage(&path),
            _ => opencode_usage(&path),
        };
        if found.is_some() {
            watch.last = found;
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
    /// How the newest API call in it ended: see `Watch::outcome`.
    outcome: Option<Option<proto::UsageBlock>>,
    /// What each turn in it DID, oldest first. See `loopguard`.
    signatures: Vec<u64>,
    /// What the seat did to files in it: see `file_activity`.
    reads: Activity,
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
    let mut out =
        Scan { newest: None, outcome: None, signatures: Vec::new(), reads: Activity::default(), consumed: 0 };
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
        out.reads.add(file_activity(&v));
        if let Some(b) = api_block(&v) {
            out.outcome = Some(Some(b));
        } else if let Some(r) = usage_of(&v) {
            out.newest = Some(r);
            out.outcome = Some(None);
        }
    }
    out
}

/// Whether one record is claude saying it could not reach its model, and why.
///
/// From claude's own error class and its `quotaLimits`, never its wording,
/// which is written for a person and changes. Real records, from this
/// fleet's transcripts:
///   - `error: "rate_limit"`, `quotaLimits.status: "rejected"`, `resetsAt`
///     (seconds), `rateLimitType: "five_hour"` -- "You've hit your session
///     limit · resets 6:50pm"; the weekly one is the same with its own type.
///   - `error: "oauth_org_not_allowed"` -- a payment past due, or the
///     organisation has switched subscription access off.
///   - `error: "authentication_failed"` -- "Login expired · Please run /login".
/// A transient failure -- 529 overloaded, a dropped stream -- is not here:
/// claude retries those itself and they say nothing about the account.
fn api_block(v: &serde_json::Value) -> Option<proto::UsageBlock> {
    if v.get("isApiErrorMessage").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    if v.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let text = match &v["message"]["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let q = &v["quotaLimits"];
    let (kind, reset, window) = match v["error"].as_str()? {
        "rate_limit" => {
            // A per-minute throttle is a rate_limit too, and claude says
            // which with the quota's own status: only a REJECTED allowance
            // holds a seat.
            if q.is_object() && q["status"].as_str() != Some("rejected") {
                return None;
            }
            (
                "quota",
                q["resetsAt"].as_i64().map(|s| if s < 100_000_000_000 { s * 1000 } else { s }),
                q["rateLimitType"].as_str().map(str::to_string),
            )
        }
        "oauth_org_not_allowed" => ("billing", None, None),
        "authentication_failed" => ("auth", None, None),
        _ => return None,
    };
    Some(proto::UsageBlock { kind: kind.into(), reset_at_ms: reset, window, text })
}

/// What one stretch of a conversation shows the seat doing to files.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Files read: see `file_reads`.
    pub n: u32,
    /// The files read by path, newest last, capped at `PATHS_KEPT`.
    pub paths: Vec<String>,
    /// Files changed: Edit, Write, MultiEdit, NotebookEdit.
    pub edited: u32,
}

/// How many read paths are carried to the hub per report. Enough for the
/// graph reminder to answer about what the seat is working in; more is
/// bytes on every report for nothing.
const PATHS_KEPT: usize = 6;

impl Activity {
    fn add(&mut self, other: Activity) {
        self.n = self.n.saturating_add(other.n);
        self.edited = self.edited.saturating_add(other.edited);
        for p in other.paths {
            self.paths.retain(|q| *q != p);
            self.paths.push(p);
        }
        let over = self.paths.len().saturating_sub(PATHS_KEPT);
        self.paths.drain(..over);
    }
    fn is_empty(&self) -> bool {
        self.n == 0 && self.edited == 0
    }
}

/// What one record shows the seat doing to files: reads (as `file_reads`
/// counts them), the paths of the files it Read, and edits.
fn file_activity(v: &serde_json::Value) -> Activity {
    let mut a = Activity { n: file_reads(v), ..Activity::default() };
    if a.n == 0 && v["type"].as_str() != Some("assistant") {
        return a;
    }
    if v.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
        return a;
    }
    let Some(blocks) = v["message"]["content"].as_array() else { return a };
    for b in blocks {
        if b["type"].as_str() != Some("tool_use") {
            continue;
        }
        match b["name"].as_str().unwrap_or("") {
            "Read" | "NotebookRead" => {
                let p = b["input"]["file_path"]
                    .as_str()
                    .or_else(|| b["input"]["notebook_path"].as_str())
                    .unwrap_or("");
                if !p.is_empty() {
                    a.add(Activity { n: 0, paths: vec![p.to_string()], edited: 0 });
                }
            }
            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => a.edited += 1,
            _ => {}
        }
    }
    a
}

/// How many files one record shows the seat reading.
///
/// claude's own file tools -- Read, Grep, Glob -- and a shell command that
/// is a read by its first word (`cat`, `rg`, `grep`, `find`, ...). A
/// subagent's turn is left out: it has its own context and, usually, no
/// nexmesh tools to have asked instead. What this is for is a comparison
/// against the seat's code-graph queries, so a rough count is enough.
fn file_reads(v: &serde_json::Value) -> u32 {
    if v.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
        return 0;
    }
    if v["type"].as_str() != Some("assistant") {
        return 0;
    }
    let Some(blocks) = v["message"]["content"].as_array() else { return 0 };
    let mut n = 0;
    for b in blocks {
        if b["type"].as_str() != Some("tool_use") {
            continue;
        }
        match b["name"].as_str().unwrap_or("") {
            "Read" | "Grep" | "Glob" | "NotebookRead" => n += 1,
            "Bash" => {
                let cmd = b["input"]["command"].as_str().unwrap_or("").trim_start();
                let first = cmd
                    .split_whitespace()
                    .find(|w| !w.contains('=') && *w != "sudo" && *w != "cd")
                    .unwrap_or("");
                if matches!(
                    first.rsplit('/').next().unwrap_or(first),
                    "cat" | "head" | "tail" | "less" | "grep" | "rg" | "find" | "fd" | "ag"
                        | "sed" | "awk" | "wc" | "tree" | "ls" | "bat"
                ) {
                    n += 1;
                }
            }
            _ => {}
        }
    }
    n
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
    Some(Record { tokens, model, window: None })
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

    /// A refusal as claude writes it into the transcript: a synthetic
    /// assistant message, the error class, and -- for the allowance -- the
    /// quota it ran into. Shape copied from this fleet's own transcripts.
    fn refusal_line(error: &str, text: &str, quota: Option<serde_json::Value>) -> String {
        let mut v = serde_json::json!({
            "type": "assistant",
            "isSidechain": false,
            "isApiErrorMessage": true,
            "apiErrorStatus": 429,
            "error": error,
            "message": { "model": "<synthetic>", "role": "assistant",
                         "content": [{ "type": "text", "text": text }],
                         "usage": { "input_tokens": 0, "output_tokens": 0 } },
        });
        if let Some(q) = quota {
            v["quotaLimits"] = q;
        }
        v.to_string() + "\n"
    }

    #[test]
    fn file_reads_are_counted_from_the_seats_own_tool_calls() {
        let line = |name: &str, input: serde_json::Value, side: bool| {
            serde_json::json!({
                "type": "assistant", "isSidechain": side,
                "message": { "content": [
                    { "type": "text", "text": "looking" },
                    { "type": "tool_use", "name": name, "input": input }
                ]}
            })
        };
        assert_eq!(file_reads(&line("Read", serde_json::json!({"file_path": "/a"}), false)), 1);
        assert_eq!(file_reads(&line("Grep", serde_json::json!({}), false)), 1);
        assert_eq!(
            file_reads(&line("Bash", serde_json::json!({"command": "cd x && rg foo"}), false)),
            0,
            "only the first command is read, and `cd` is skipped, not `&&`"
        );
        assert_eq!(file_reads(&line("Bash", serde_json::json!({"command": "cat a.rs"}), false)), 1);
        assert_eq!(file_reads(&line("Bash", serde_json::json!({"command": "cargo build"}), false)), 0);
        assert_eq!(file_reads(&line("Edit", serde_json::json!({}), false)), 0);
        assert_eq!(file_reads(&line("Read", serde_json::json!({}), true)), 0, "a subagent's");
        let mut chunk = String::new();
        for _ in 0..3 {
            chunk.push_str(&line("Read", serde_json::json!({}), false).to_string());
            chunk.push('\n');
        }
        assert_eq!(scan(chunk.as_bytes(), false).reads.n, 3);
    }

    #[test]
    fn a_used_up_allowance_is_read_with_its_reset_and_window() {
        let line = refusal_line(
            "rate_limit",
            "You've hit your weekly limit \u{b7} resets 10pm (UTC)",
            Some(serde_json::json!({ "status": "rejected", "resetsAt": 1_791_046_200,
                                     "rateLimitType": "seven_day" })),
        );
        let s = scan(line.as_bytes(), false);
        let b = s.outcome.expect("a call was seen").expect("it was refused");
        assert_eq!(b.kind, "quota");
        assert_eq!(b.reset_at_ms, Some(1_791_046_200_000));
        assert_eq!(b.window.as_deref(), Some("seven_day"));
        assert!(b.text.contains("weekly limit"), "{}", b.text);
        assert!(s.newest.is_none(), "a refusal is not a context reading");
    }

    #[test]
    fn a_throttle_that_is_not_a_rejected_allowance_holds_nothing() {
        let line = refusal_line("rate_limit", "slow down",
                                Some(serde_json::json!({ "status": "allowed_warning" })));
        assert!(scan(line.as_bytes(), false).outcome.is_none());
        for transient in ["server_error", "invalid_request", "unknown"] {
            let line = refusal_line(transient, "API Error: 529 Overloaded", None);
            assert!(scan(line.as_bytes(), false).outcome.is_none(), "{transient}");
        }
    }

    #[test]
    fn a_lost_login_and_a_billing_stop_are_told_apart() {
        let auth = refusal_line("authentication_failed", "Login expired \u{b7} Please run /login", None);
        assert_eq!(scan(auth.as_bytes(), false).outcome.unwrap().unwrap().kind, "auth");
        let bill = refusal_line("oauth_org_not_allowed", "a payment is past due", None);
        assert_eq!(scan(bill.as_bytes(), false).outcome.unwrap().unwrap().kind, "billing");
    }

    /// The NEWEST call decides: a refusal followed by an answered call is a
    /// seat whose allowance came back.
    #[test]
    fn an_answered_call_after_a_refusal_clears_it() {
        let chunk = refusal_line("rate_limit", "hit your session limit",
                                 Some(serde_json::json!({ "status": "rejected", "resetsAt": 1 })))
            + &usage_line("claude-opus-4", 1000, 0, 10, false);
        assert_eq!(scan(chunk.as_bytes(), false).outcome, Some(None));
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

    #[test]
    fn codex_occupancy_is_the_last_call_and_its_window() {
        let text = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"output_tokens":10},"model_context_window":475000}}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":72467,"output_tokens":148},"model_context_window":475000}}}"#, "\n",
        );
        let r = codex_usage_in(text).expect("a reading");
        assert_eq!(r.tokens, 72615);
        assert_eq!(r.window, Some(475000));
        assert_eq!(r.model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn opencode_occupancy_counts_cache_and_skips_empty_messages() {
        assert!(opencode_record(r#"{"role":"assistant","tokens":{"input":0,"output":0,"cache":{"read":0,"write":0}}}"#).is_none());
        let r = opencode_record(r#"{"role":"assistant","modelID":"m1","tokens":{"input":100,"output":20,"cache":{"read":5000,"write":30}}}"#).unwrap();
        assert_eq!(r.tokens, 5150);
        assert_eq!(r.model.as_deref(), Some("m1"));
    }

    #[test]
    fn reads_carry_their_paths_and_edits_are_counted() {
        let v = serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"/w/a.rs"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/w/a.rs"}},
            {"type":"tool_use","name":"Read","input":{"file_path":"/w/b.rs"}}]}});
        let a = file_activity(&v);
        assert_eq!(a.n, 2);
        assert_eq!(a.edited, 1);
        assert_eq!(a.paths, vec!["/w/a.rs".to_string(), "/w/b.rs".to_string()]);
    }

}

/// The most recently written `rollout-*.jsonl` under a codex sessions tree
/// (`YYYY/MM/DD/`). The seat's store is its own, so the newest is the one
/// it is in.
fn newest_rollout(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![(dir.to_path_buf(), 0u8)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                if depth < 3 {
                    stack.push((p, depth + 1));
                }
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("rollout-") && n.ends_with(".jsonl")
            }) {
                let t = m.modified().unwrap_or(std::time::UNIX_EPOCH);
                if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
                    best = Some((t, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// How much of a rollout's tail is read for its newest usage record.
const ROLLOUT_TAIL_BYTES: u64 = 512 * 1024;

/// The newest `token_count` in a codex rollout: the last call's input plus
/// output is what the next turn carries, and the record names the window.
fn codex_usage(path: &Path) -> Option<Record> {
    let size = std::fs::metadata(path).ok()?.len();
    let from = size.saturating_sub(ROLLOUT_TAIL_BYTES);
    let chunk = read_from(path, from, size)?;
    codex_usage_in(&String::from_utf8_lossy(&chunk))
}

fn codex_usage_in(text: &str) -> Option<Record> {
    let mut model: Option<String> = None;
    let mut found: Option<Record> = None;
    for line in text.lines() {
        if !line.contains("token_count") && !line.contains("\"model\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(m) = v["payload"]["model"].as_str() {
            model = Some(m.to_string());
        }
        if v["payload"]["type"].as_str() != Some("token_count") {
            continue;
        }
        let info = &v["payload"]["info"];
        let last = &info["last_token_usage"];
        let (Some(i), o) = (last["input_tokens"].as_i64(), last["output_tokens"].as_i64().unwrap_or(0))
        else {
            continue;
        };
        found = Some(Record {
            tokens: i + o,
            model: model.clone(),
            window: info["model_context_window"].as_i64(),
        });
    }
    found
}

/// The newest assistant message with token counts in an opencode database.
///
/// Read-only and briefly: opencode holds this file open and writes through
/// its WAL, which a read-only connection reads without blocking it.
fn opencode_usage(path: &Path) -> Option<Record> {
    let uri = format!("file:{}?mode=ro", path.to_string_lossy());
    let db = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    let _ = db.busy_timeout(std::time::Duration::from_millis(500));
    let mut q = db
        .prepare(
            "SELECT data FROM message
              WHERE json_extract(data, '$.role') = 'assistant'
              ORDER BY time_created DESC LIMIT 20",
        )
        .ok()?;
    let rows = q.query_map([], |r| r.get::<_, String>(0)).ok()?;
    for data in rows.flatten() {
        if let Some(r) = opencode_record(&data) {
            return Some(r);
        }
    }
    None
}

fn opencode_record(data: &str) -> Option<Record> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    let t = &v["tokens"];
    let n = |k: &serde_json::Value| k.as_i64().unwrap_or(0);
    let total = n(&t["input"]) + n(&t["output"]) + n(&t["cache"]["read"]) + n(&t["cache"]["write"]);
    if total <= 0 {
        // A message still being answered, or one that failed: says nothing.
        return None;
    }
    Some(Record { tokens: total, model: v["modelID"].as_str().map(str::to_string), window: None })
}
