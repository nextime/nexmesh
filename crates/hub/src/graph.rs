//! Storing and querying a workspace's code graph.
//!
//! The node parses (it has the files); the hub keeps the result and answers
//! questions, so a codebase is read once for the whole team rather than once
//! per agent. For a cheap seat especially, "what calls this" answered from an
//! AST is worth more than the same question answered by reading files into a
//! context window.
//!
//! Communities are computed here with [`crate::leiden`], because the hub is
//! the only place that holds the whole graph. They are the closest thing to a
//! subsystem map that can be derived without anyone labelling anything.

use crate::{leiden, state::AppState};
use anyhow::{Context, Result};
use nexmesh_proto::{
    self as proto,
    codegraph::{CodeGraph, EdgeKind},
    now_ms,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

/// How long after the tree moves to rebuild.
///
/// Not immediately: an agent saving a file a few times in a row, or a team
/// publishing a series of commits, would otherwise start a rebuild per save.
/// Long enough to coalesce a burst; short, because a node replays the parse of
/// every unchanged file and a rebuild after an edit costs that edit's files,
/// not the tree's -- and a graph behind the file an agent just changed is a
/// graph that agent stops trusting and stops asking.
const REBUILD_AFTER_MS: i64 = 15_000;

/// Don't retry a FAILING rebuild in a tight loop.
const RETRY_AFTER_MS: i64 = 600_000;

/// The least time between two rebuilds of one graph...
const MIN_REBUILD_GAP_MS: i64 = 30_000;

/// ...and the multiple of the last build's own duration that must pass too.
///
/// Rebuilds are paced by what they cost, not by a fixed clock. On a node that
/// replays unchanged files a rebuild is milliseconds to a few seconds, and
/// the floor above is what applies. On one that still parses the whole tree
/// every time -- measured at two minutes for a 6,275-file tree -- the gap
/// grows with it, so that node is never kept parsing more than a quarter of
/// the time however often the files move.
const REBUILD_GAP_FACTOR: i64 = 3;

/// The ONE selection of a graph that is due a rebuild, shared by `stale_tick`
/// and the tests so the two cannot drift apart. ?1 = stale since at least,
/// ?2 = failed no later than, ?3 = now.
const DUE_SQL: &str = "SELECT m.workspace_id, w.name, wm.node_id, wm.path
       FROM code_graph_meta m
       JOIN workspaces w ON w.id = m.workspace_id
       JOIN workspace_mounts wm ON wm.workspace_id = m.workspace_id
       JOIN nodes n ON n.id = wm.node_id
      WHERE m.stale_at_ms IS NOT NULL
        AND m.stale_at_ms <= ?1
        AND n.status = 'online'
        AND (m.failed_at_ms IS NULL OR m.failed_at_ms <= ?2)
        AND (m.built_at_ms IS NULL
             OR m.built_at_ms + MAX(?4, ?5 * COALESCE(m.build_ms, 0)) <= ?3)
      ORDER BY m.stale_at_ms
      LIMIT 1";

fn due_params(now: i64) -> [i64; 5] {
    [now - REBUILD_AFTER_MS, now - RETRY_AFTER_MS, now, MIN_REBUILD_GAP_MS, REBUILD_GAP_FACTOR]
}

/// The tree this graph was built from has moved.
///
/// The graph used to be built on demand and never again, so every team that
/// asked it a structural question after the first day got an answer about a
/// codebase that no longer existed -- and from inside, an agent cannot tell a
/// stale answer from a wrong one.
pub fn mark_stale(db: &Connection, workspace_id: &str) {
    let _ = db.execute(
        "UPDATE code_graph_meta SET stale_at_ms = COALESCE(stale_at_ms, ?2)
          WHERE workspace_id = ?1",
        params![workspace_id, now_ms()],
    );
}

/// Rebuild one graph whose tree has moved.
///
/// One per tick on purpose. Parsing is minutes of somebody else's CPU, and a
/// fleet that published everywhere at once would otherwise ask every node to
/// parse at the same moment.
pub async fn stale_tick(st: &AppState) {
    let now = now_ms();
    let due: Option<(String, String, String, String)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            // The node that actually holds this workspace, and where. A
            // repo workspace is on several; any of them will do.
            DUE_SQL,
            rusqlite::params_from_iter(due_params(now)),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .ok()
        .flatten()
    };
    let Some((ws_id, ws_name, node_id, path)) = due else { return };
    tracing::info!(workspace = %ws_id, name = %ws_name, node = %node_id,
                   "the tree moved; rebuilding the code graph");
    let began = now_ms();
    match build(st, &ws_id, &node_id, &path).await {
        Ok(said) => {
            let db = st.db.lock().unwrap();
            // `build` -> `store` sets built_at_ms, so clearing the mark here
            // is what says the graph matches what was last shared. What it
            // cost is kept, because that is what paces the next one.
            let _ = db.execute(
                "UPDATE code_graph_meta SET stale_at_ms = NULL, stale_reason = NULL,
                        build_ms = ?2, failed_at_ms = NULL
                  WHERE workspace_id = ?1",
                params![ws_id, now_ms() - began],
            );
            tracing::info!(workspace = %ws_id, %said, "code graph rebuilt");
        }
        Err(e) => {
            // Recorded, and shown WITH the graph. A rebuild that quietly
            // fails leaves a stale graph being served as if it were current,
            // which is the one outcome worth more than an error message.
            let why = e.to_string();
            let db = st.db.lock().unwrap();
            // failed_at_ms, NOT built_at_ms: built_at is when the graph was
            // last parsed, which every answer reports, and it is not this.
            let _ = db.execute(
                "UPDATE code_graph_meta SET failed_at_ms = ?2, stale_reason = ?3
                  WHERE workspace_id = ?1",
                params![ws_id, now_ms(), why],
            );
            tracing::warn!(workspace = %ws_id, error = %why, "could not rebuild the code graph");
        }
    }
}

/// How often one workspace may be asked what its tree looks like.
///
/// A fingerprint is a walk with no parsing: seconds, not minutes. But it is
/// still somebody else's disk. Measured: 2 ms, 34 ms and 458 ms for trees of
/// 141, 799 and 6,275 source files -- so once a minute is noise on even the
/// largest, and it is what bounds how far the graph can fall behind the
/// files. Ten minutes, the old figure, was long enough for an agent to edit
/// a file, ask the graph about it, and be answered about the old version.
const FINGERPRINT_EVERY_MS: i64 = 60_000;

/// How long to wait for a node to answer before deciding it cannot.
///
/// A node from before this frame existed logs "bad hub frame" and carries
/// on, so the hub never hears back at all. That is not a failure to report
/// -- it is a fleet mid-upgrade -- and the age rule below is what keeps
/// those workspaces fresh until the node is restarted.
const FINGERPRINT_WAIT_MS: i64 = 300_000;

/// Both of the above, in milliseconds, overridable from the environment.
///
/// A suite cannot wait ten minutes to see a second question asked, and a
/// test that reaches into the database to fake one is testing the test. The
/// knob is real rather than test-only: a fleet that wants its graphs checked
/// every minute, or every hour, is making a choice about somebody else's
/// disk, and that is theirs to make.
fn ms_from_env(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn fingerprint_every_ms() -> i64 {
    ms_from_env("NEXMESH_FINGERPRINT_EVERY_MS", FINGERPRINT_EVERY_MS)
}

fn fingerprint_wait_ms() -> i64 {
    ms_from_env("NEXMESH_FINGERPRINT_WAIT_MS", FINGERPRINT_WAIT_MS)
}

/// When a graph is old enough to rebuild without being told the tree moved.
///
/// Only for workspaces whose node cannot answer a fingerprint. With one that
/// can, an untouched tree is never reparsed however old its graph is, and a
/// tree touched a minute ago is reparsed at the next tick.
pub const UNCHECKABLE_MAX_AGE_MS: i64 = 6 * 3_600_000;

/// Ask one node whether a workspace's files have moved since its graph was
/// built, and rebuild on the strength of the answer rather than the clock.
///
/// The graph used to go stale only when work was PUBLISHED into a workspace
/// -- a git sync, which a `node_local` workspace never has. On a real fleet
/// that meant every graph was built once, on the day its seat was made, and
/// never again: five graphs between eight and ten days old while the trees
/// under them changed every hour. An agent asking a structural question got
/// an answer about a codebase that no longer existed, and could not tell.
pub async fn fresh_tick(st: &AppState) {
    let now = now_ms();
    // One per tick, oldest check first: the fingerprint is cheap but it is
    // not free, and a fleet with a dozen workspaces should not walk all of
    // them in the same second.
    let due: Option<(String, String, String, Option<i64>, Option<i64>, i64)> = {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT m.workspace_id, wm.node_id, wm.path, m.fp_at_ms, m.fp_asked_at_ms,
                    m.built_at_ms
               FROM code_graph_meta m
               JOIN workspace_mounts wm ON wm.workspace_id = m.workspace_id
               JOIN nodes n ON n.id = wm.node_id
              WHERE n.status = 'online'
                AND m.stale_at_ms IS NULL
                AND COALESCE(m.fp_asked_at_ms, 0) <= ?1
              -- A repo workspace is checked out on several machines, and
              -- two checkouts fingerprint differently. Always asking the
              -- same one keeps a changed fingerprint meaning the tree
              -- changed, rather than meaning the hub asked a different
              -- machine this time.
              ORDER BY COALESCE(m.fp_asked_at_ms, 0), wm.node_id
              LIMIT 1",
            params![now - fingerprint_every_ms()],
            |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            },
        )
        .optional()
        .ok()
        .flatten()
    };
    let Some((ws_id, node_id, path, fp_at, fp_asked, built_at)) = due else { return };

    // Asked before and never answered: this node is older than the frame.
    // Fall back to the clock, which is the whole reason the age rule exists.
    let unanswered = fp_at.is_none()
        && fp_asked.is_some_and(|at| now - at > fingerprint_wait_ms());
    if unanswered && built_at <= now - UNCHECKABLE_MAX_AGE_MS {
        let db = st.db.lock().unwrap();
        mark_stale(&db, &ws_id);
        let _ = db.execute(
            "UPDATE code_graph_meta
                SET stale_reason = 'this node cannot say whether the tree has moved, and the \
                     graph was getting old', fp_asked_at_ms = ?2
              WHERE workspace_id = ?1",
            params![ws_id, now],
        );
        tracing::info!(workspace = %ws_id,
            "rebuilding on age: the node does not answer fingerprints");
        return;
    }

    {
        let db = st.db.lock().unwrap();
        let _ = db.execute(
            "UPDATE code_graph_meta SET fp_asked_at_ms = ?2 WHERE workspace_id = ?1",
            params![ws_id, now],
        );
    }
    // Fire and forget: the answer comes back as its own frame, because a
    // walk of somebody's tree is not something to hold a tick open for.
    let sent = st
        .send_to_node(
            &node_id,
            &proto::HubToNode::TreeFingerprint {
                req_id: st.next_req_id(),
                workspace_id: ws_id.clone(),
                path: path.clone(),
            },
        )
        .await;
    if let Err(e) = sent {
        tracing::debug!(workspace = %ws_id, error = %e, "could not ask for a tree fingerprint");
    }
}

/// A node said what a workspace's tree looks like now.
///
/// First answer for a graph records the fingerprint and nothing else: the
/// graph was built before anybody was fingerprinting, so there is nothing to
/// compare it with and assuming it moved would reparse every workspace on
/// the fleet at once. Every answer after that is the comparison.
pub fn fingerprint_arrived(
    db: &Connection,
    workspace_id: &str,
    fingerprint: &str,
    files: u64,
    partial: bool,
) {
    let now = now_ms();
    let known: Option<Option<String>> = db
        .query_row(
            "SELECT tree_fingerprint FROM code_graph_meta WHERE workspace_id = ?",
            [workspace_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let Some(known) = known else { return };
    let _ = db.execute(
        "UPDATE code_graph_meta SET tree_fingerprint = ?2, fp_at_ms = ?3, fp_files = ?4
          WHERE workspace_id = ?1",
        params![workspace_id, fingerprint, now, files as i64],
    );
    match known.as_deref() {
        // Nothing to compare with yet: this is the baseline.
        None => {
            tracing::info!(%workspace_id, %fingerprint, files,
                "recorded what this workspace's tree looks like");
        }
        Some(was) if was == fingerprint => {
            tracing::debug!(%workspace_id, "the tree has not moved; the graph stands");
        }
        Some(was) => {
            // A walk that did not finish cannot say the tree changed: half a
            // tree fingerprints differently every time the limit lands
            // somewhere else, and reparsing on that would never stop.
            if partial {
                tracing::debug!(%workspace_id,
                    "the fingerprint changed but the walk was partial; not rebuilding on it");
                return;
            }
            mark_stale(db, workspace_id);
            let _ = db.execute(
                "UPDATE code_graph_meta SET stale_reason = 'the files changed'
                  WHERE workspace_id = ?1",
                [workspace_id],
            );
            tracing::info!(%workspace_id, was, now = fingerprint,
                "the tree moved; the graph is stale");
        }
    }
}

/// Ask the node holding a workspace to parse it, then store what comes back.
pub async fn build(st: &AppState, workspace_id: &str, node_id: &str, path: &str) -> Result<String> {
    let (req_id, rx) = st.new_sync_request();
    st.send_to_node(
        node_id,
        &proto::HubToNode::BuildCodeGraph {
            req_id,
            workspace_id: workspace_id.to_string(),
            path: path.to_string(),
        },
    )
    .await
    .with_context(|| format!("asking {node_id} to parse {path}"))?;

    // Parsing a large tree takes a while; a bounded wait beats a tool call
    // that never returns.
    let answer = tokio::time::timeout(std::time::Duration::from_secs(420), rx)
        .await
        .map_err(|_| anyhow::anyhow!("{node_id} did not finish parsing within seven minutes"))?
        .map_err(|_| anyhow::anyhow!("the node went away while parsing"))?;

    let graph: CodeGraph =
        serde_json::from_value(answer).context("the node sent a graph we cannot read")?;
    store_off_thread(st, workspace_id, graph).await
}

/// Store a graph without blocking the runtime.
///
/// Grouping tens of thousands of symbols into subsystems is seconds of
/// solid CPU, and running that on an async worker is how a single large
/// workspace stopped the hub answering anything at all -- including
/// `/healthz`, including the dashboard, for every other machine on the
/// fleet. It belongs on a blocking thread.
pub async fn store_off_thread(
    st: &AppState,
    workspace_id: &str,
    graph: CodeGraph,
) -> Result<String> {
    let st = st.clone();
    let ws = workspace_id.to_string();
    tokio::task::spawn_blocking(move || store(&st, &ws, &graph))
        .await
        .context("storing a code graph")?
}

/// A fingerprint of the MAP itself: every symbol, where it is, and every
/// relationship. Not of the files -- an edit inside a function body changes a
/// file and leaves this exactly as it was.
///
/// FNV-1a over the serialised nodes and edges, written out here rather than
/// taken from `std`'s hasher, whose output is allowed to change between Rust
/// releases: this one is stored, and a hub upgrade must not read every
/// workspace's map as changed.
pub fn map_hash(graph: &CodeGraph) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(serde_json::to_string(&graph.nodes).unwrap_or_default().as_bytes());
    eat(b"|");
    eat(serde_json::to_string(&graph.edges).unwrap_or_default().as_bytes());
    eat(if graph.truncated { b"t" } else { b"f" });
    format!("{h:016x}")
}

/// Replace a workspace's graph and recompute its communities -- unless the
/// map has not changed.
///
/// Most edits do not change it: a fix inside a function moves no symbol and
/// adds no call. The node's reparse after such an edit is already cheap (it
/// replays every unchanged file from its cache); what is NOT cheap is this
/// side, which rewrote every row and regrouped the whole tree each time. So a
/// rebuild whose map is the one already stored only says the graph is
/// current again.
pub fn store(st: &AppState, workspace_id: &str, graph: &CodeGraph) -> Result<String> {
    let hash = map_hash(graph);
    {
        let db = st.db.lock().unwrap();
        let same: bool = db
            .query_row(
                "SELECT map_hash = ?2 FROM code_graph_meta WHERE workspace_id = ?1",
                params![workspace_id, &hash],
                |r| r.get::<_, Option<bool>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(false);
        if same {
            db.execute(
                "UPDATE code_graph_meta SET built_at_ms = ?2, files = ?3, skipped = ?4,
                        stale_at_ms = NULL, stale_reason = NULL
                  WHERE workspace_id = ?1",
                params![
                    workspace_id,
                    now_ms(),
                    graph.files,
                    serde_json::to_string(&graph.skipped).unwrap_or_else(|_| "[]".into()),
                ],
            )?;
            tracing::info!(%workspace_id, "reparsed: the map did not change, so the graph stands");
            return Ok(format!(
                "Parsed {} file(s): the map is unchanged ({} symbols, {} relationships), so \
                 the stored graph stands and is current.",
                graph.files,
                graph.nodes.len(),
                graph.edges.len()
            ));
        }
    }
    let (communities, grouped_fully) = assign_communities(graph);
    let db = st.db.lock().unwrap();
    // One transaction for the lot. A large workspace is 200,000 symbols, and
    // committing each of those separately is the difference between seconds
    // and minutes -- minutes during which this lock is held and the rest of
    // the hub waits.
    let tx = db.unchecked_transaction()?;
    // Replace wholesale rather than merge: the graph is derived, so a stale
    // node left behind would be a symbol that no longer exists being offered
    // to an agent as though it did.
    tx.execute("DELETE FROM code_graph_nodes WHERE workspace_id = ?", [workspace_id])?;
    tx.execute("DELETE FROM code_graph_edges WHERE workspace_id = ?", [workspace_id])?;
    for (i, n) in graph.nodes.iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO code_graph_nodes
                (workspace_id, id, kind, name, path, line, lang, community)
             VALUES (?,?,?,?,?,?,?,?)",
            params![
                workspace_id, n.id, n.kind.as_str(), n.name, n.path, n.line, n.lang,
                communities.get(i).and_then(|c| c.map(|c| c as i64))
            ],
        )?;
    }
    for e in &graph.edges {
        tx.execute(
            "INSERT OR IGNORE INTO code_graph_edges (workspace_id, from_id, to_id, kind)
             VALUES (?,?,?,?)",
            params![workspace_id, e.from, e.to, e.kind.as_str()],
        )?;
    }
    let distinct = {
        let mut c: Vec<usize> = communities.iter().filter_map(|c| *c).collect();
        c.sort_unstable();
        c.dedup();
        c.len()
    };
    tx.execute(
        "INSERT OR REPLACE INTO code_graph_meta
            (workspace_id, built_at_ms, files, skipped, truncated, truncated_reason, communities)
         VALUES (?,?,?,?,?,?,?)",
        params![
            workspace_id,
            now_ms(),
            graph.files,
            serde_json::to_string(&graph.skipped).unwrap_or_else(|_| "[]".into()),
            graph.truncated as i64,
            graph.truncated_reason,
            distinct as i64
        ],
    )?;
    tx.execute(
        "UPDATE code_graph_meta SET map_hash = ? WHERE workspace_id = ?",
        params![&hash, workspace_id],
    )?;
    tx.commit()?;
    // The dashboard's view of this graph, worked out now rather than on
    // every poll of a page that asks every few seconds.
    if let Some(view) = summarise(&db, workspace_id, "") {
        db.execute(
            "UPDATE code_graph_meta SET summary_json = ? WHERE workspace_id = ?",
            params![view.to_string(), workspace_id],
        )
        .ok();
    }
    let mut out = format!(
        "Parsed {} file(s): {} symbols, {} relationships, {} subsystem(s).",
        graph.files,
        graph.nodes.len(),
        graph.edges.len(),
        distinct
    );
    if !grouped_fully {
        out.push_str(
            " Grouping into subsystems ran out of time, so the subsystems are provisional.",
        );
    }
    if graph.truncated {
        out.push_str(&format!(
            " This graph is partial: {}.",
            graph.truncated_reason.as_deref().unwrap_or("a limit stopped the scan")
        ));
    }
    if !graph.skipped.is_empty() {
        out.push_str(&format!(" {} file(s) skipped.", graph.skipped.len()));
    }
    Ok(out)
}

/// Group the graph into subsystems with Leiden.
///
/// Only `Calls` and `Imports` carry weight: `Defines` would tie every symbol
/// to its own file and so make each file a community, which says nothing a
/// directory listing does not already say.
fn assign_communities(graph: &CodeGraph) -> (Vec<Option<usize>>, bool) {
    let n = graph.nodes.len();
    if n == 0 {
        return (Vec::new(), true);
    }
    let index: std::collections::HashMap<&str, usize> =
        graph.nodes.iter().enumerate().map(|(i, nd)| (nd.id.as_str(), i)).collect();
    let mut g = leiden::Graph::with_nodes(n);
    for e in &graph.edges {
        let w = match e.kind {
            EdgeKind::Calls => 1.0,
            EdgeKind::Imports => 0.75,
            EdgeKind::References => 0.5,
            // Structural, not relational: see the note above.
            EdgeKind::Defines => continue,
        };
        if let (Some(&a), Some(&b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            g.add_edge(a, b, w);
        }
    }
    // Bounded, and the bound is wall-clock. See `leiden::within`: a graph
    // that keeps this running is not a graph worth waiting for, and it used
    // to take the hub off the air entirely.
    let started = std::time::Instant::now();
    let (comms, complete) = leiden::within(
        &g,
        &leiden::Options::default(),
        Some(started + std::time::Duration::from_secs(45)),
    );
    tracing::info!(
        nodes = n,
        edges = graph.edges.len(),
        complete,
        took_ms = started.elapsed().as_millis() as u64,
        "grouped a code graph into subsystems"
    );
    // A node with no relational edge is not in a subsystem, and saying it is
    // one of its own would fill the answer with noise: a file node, once
    // Defines is excluded, connects to nothing.
    let mut degree = vec![0usize; n];
    for e in &graph.edges {
        if matches!(e.kind, EdgeKind::Defines) {
            continue;
        }
        if let (Some(&a), Some(&b)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            degree[a] += 1;
            degree[b] += 1;
        }
    }
    let out = comms
        .into_iter()
        .enumerate()
        .map(|(i, c)| if degree[i] > 0 { Some(c) } else { None })
        .collect();
    (out, complete)
}

fn kind_label(k: &str) -> &str {
    k
}


/// Everything the dashboard shows about one workspace's graph, as JSON.
///
/// Computed once, when the graph is stored, and kept -- because it used to
/// be computed on every poll of the dashboard. For a workspace of fifty
/// thousand symbols that is half a minute of SQL per poll, against a page
/// that polls every few seconds, so the hub spent all of its time rebuilding
/// an answer that only changes when somebody re-parses. It stopped serving
/// anything else at all.
pub fn summarise(db: &Connection, ws_id: &str, ws_name: &str) -> Option<Value> {
    let (built, files, truncated, truncated_why, communities, stale_at, stale_why):
        (i64, i64, i64, Option<String>, i64, Option<i64>, Option<String>) = db
        .query_row(
            "SELECT built_at_ms, files, truncated, truncated_reason, communities,
                    stale_at_ms, stale_reason
               FROM code_graph_meta WHERE workspace_id = ?",
            [ws_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let counts: Vec<Value> = db
        .prepare(
            "SELECT kind, COUNT(*) FROM code_graph_nodes
              WHERE workspace_id = ? GROUP BY kind ORDER BY 2 DESC",
        )
        .and_then(|mut st| {
            st.query_map([ws_id], |r| {
                Ok(json!({ "kind": r.get::<_, String>(0)?,
                           "n": r.get::<_, i64>(1)? }))
            })
            .and_then(|m| m.collect())
        })
        .unwrap_or_default();
    // Largest first, with a few recognisable members each:
    // a list of numbered communities tells a reader nothing.
    let subsystems: Vec<Value> = db
        .prepare(
            "SELECT community, COUNT(*) FROM code_graph_nodes
              WHERE workspace_id = ? AND community IS NOT NULL
              GROUP BY community ORDER BY 2 DESC LIMIT 24",
        )
        .and_then(|mut st| {
            st.query_map([ws_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(c, n)| {
            let members: Vec<Value> = db
                .prepare(
                    "SELECT name, path, kind FROM code_graph_nodes
                      WHERE workspace_id = ? AND community = ?
                      ORDER BY CASE kind WHEN 'type' THEN 0
                                         WHEN 'function' THEN 1 ELSE 2 END,
                               name LIMIT 8",
                )
                .and_then(|mut st| {
                    st.query_map(params![ws_id, c], |r| {
                        Ok(json!({ "name": r.get::<_, String>(0)?,
                                   "path": r.get::<_, String>(1)?,
                                   "kind": r.get::<_, String>(2)? }))
                    })
                    .and_then(|m| m.collect())
                })
                .unwrap_or_default();
            // What to call a subsystem nobody named. The
            // file most of it lives in beats the alphabetically
            // first function in it, which is how a community
            // came to be labelled after a test.
            let path: Option<String> = db
                .query_row(
                    // Skip the empty path: an external
                    // import is a `module:` node with no file
                    // of its own, and a community full of them
                    // would otherwise be labelled "".
                    "SELECT path FROM code_graph_nodes
                      WHERE workspace_id = ? AND community = ?
                        AND path <> ''
                      GROUP BY path ORDER BY COUNT(*) DESC, path LIMIT 1",
                    params![ws_id, c],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            json!({ "id": c, "n": n, "members": members, "path": path })
        })
        .collect();
    // How the subsystems depend on each other. A table of
    // communities says what exists; only this says what the
    // thing is SHAPED like, which is the question a person
    // opens a code graph to answer.
    //
    // Aggregated to community level deliberately: 1400 symbols
    // drawn at once is a hairball, 19 subsystems is a map.
    let mut agg: std::collections::BTreeMap<(i64, i64), (i64, bool)> =
        Default::default();
    if let Ok(mut st) = db.prepare(
        "SELECT a.community, b.community, e.kind, COUNT(*)
           FROM code_graph_edges e
           JOIN code_graph_nodes a
             ON a.workspace_id = e.workspace_id AND a.id = e.from_id
           JOIN code_graph_nodes b
             ON b.workspace_id = e.workspace_id AND b.id = e.to_id
          WHERE e.workspace_id = ?
            AND a.community IS NOT NULL AND b.community IS NOT NULL
            AND a.community <> b.community
          GROUP BY 1, 2, 3",
    ) {
        let rows = st.query_map([ws_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        });
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (a, b, kind, n) = row;
                // `defines` and `imports` are read off the
                // tree; a call resolved by name is a guess,
                // and the drawing says which is which.
                let certain = kind == "defines" || kind == "imports";
                let e = agg.entry((a, b)).or_insert((0, false));
                e.0 += n;
                e.1 |= certain;
            }
        }
    }
    let links: Vec<Value> = agg
        .into_iter()
        .map(|((a, b), (n, certain))| {
            json!({ "a": a, "b": b, "n": n, "certain": certain })
        })
        .collect();

    Some(json!({
        "workspace": ws_name,
        "workspace_id": ws_id,
        "built_at_ms": built,
        // WHETHER THIS DESCRIBES THE TREE THAT IS THERE NOW.
        //
        // Work has been published into this workspace since the graph was
        // parsed. A rebuild is coming, but until it lands these answers are
        // about a codebase that has moved -- and from inside an agent cannot
        // tell a stale answer from a wrong one, so it is told.
        "stale": stale_at.is_some(),
        "stale_since_ms": stale_at,
        // Why the automatic rebuild has not caught up, when it has tried and
        // failed. A rebuild that fails silently leaves a stale graph being
        // served as if it were current.
        "stale_reason": stale_why,
        "files": files,
        "truncated": truncated != 0,
        "truncated_reason": truncated_why,
        "communities": communities,
        "counts": counts,
        "subsystems": subsystems,
        "links": links,
    }))
}

/// "three hours ago", for an answer an agent reads rather than parses.
pub fn ago(ms: i64) -> String {
    let mins = ms / 60_000;
    if mins < 90 {
        return format!("{mins} minute(s) ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours} hour(s) ago");
    }
    format!("{} day(s) ago", hours / 24)
}

/// Answer one question about the graph.
pub fn query(db: &Connection, workspace_id: &str, what: &str, target: &str, to: &str)
    -> Result<String>
{
    #[allow(clippy::type_complexity)]
    let meta: Option<(i64, i64, i64, Option<String>, i64, Option<i64>)> = db
        .query_row(
            "SELECT built_at_ms, files, truncated, truncated_reason, communities, stale_at_ms
               FROM code_graph_meta WHERE workspace_id = ?",
            [workspace_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((built, files, truncated, truncated_why, communities, stale_at)) = meta else {
        anyhow::bail!(
            "this workspace has no code graph yet. Call query_code_graph with rebuild=true to \
             parse it."
        );
    };

    let mut out = String::new();
    match what {
        "find" => {
            let rows = find(db, workspace_id, target)?;
            if rows.is_empty() {
                return Ok(format!("Nothing in this workspace is called '{target}'."));
            }
            out.push_str(&format!("{} match(es) for '{target}':\n", rows.len()));
            for (id, kind, path, line, _) in rows {
                out.push_str(&format!("- {} [{}] {}:{}\n", id, kind_label(&kind), path, line));
            }
        }
        "callers" => {
            out = related(db, workspace_id, target, "calls", true, "called by")?;
        }
        "calls" => {
            out = related(db, workspace_id, target, "calls", false, "calls")?;
        }
        "imports" => {
            out = related(db, workspace_id, target, "imports", false, "imports")?;
        }
        "importers" => {
            out = related(db, workspace_id, target, "imports", true, "imported by")?;
        }
        "defines" => {
            out = related(db, workspace_id, target, "defines", false, "defines")?;
        }
        "subsystem" => {
            out = subsystem(db, workspace_id, target)?;
        }
        "subsystems" => {
            out = subsystems(db, workspace_id)?;
        }
        "overview" => {
            out.push_str(&format!(
                "{files} file(s) parsed, {communities} subsystem(s).\n"
            ));
            let mut stmt = db.prepare(
                "SELECT kind, COUNT(*) FROM code_graph_nodes WHERE workspace_id = ?
                  GROUP BY kind ORDER BY 2 DESC",
            )?;
            let rows: Vec<(String, i64)> = stmt
                .query_map([workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (k, n) in rows {
                out.push_str(&format!("- {n} {k}(s)\n"));
            }
            out.push_str(&subsystems(db, workspace_id)?);
        }
        "path" => {
            out = shortest_path(db, workspace_id, target, to)?;
        }
        other => anyhow::bail!(
            "unknown question '{other}'. Ask for: overview, find, callers, calls, imports, \
             importers, defines, subsystem, subsystems or path."
        ),
    }
    if truncated != 0 {
        out.push_str(&format!(
            "\n(This graph is partial: {}.)\n",
            truncated_why.as_deref().unwrap_or("a limit stopped the scan")
        ));
    }
    // HOW OLD THIS ANSWER IS.
    //
    // The graph is derived, and from inside an agent cannot tell a stale
    // answer from a wrong one -- so every answer says when it was parsed
    // once it is old enough for that to matter. Said in hours rather than
    // milliseconds because it is read by somebody deciding whether to open
    // the file instead.
    let age = now_ms() - built;
    if stale_at.is_some() {
        out.push_str(
            "\n(The files have changed since this was parsed; a rebuild is on its way, usually \
             within a minute or two.              Read the file if the answer matters.)\n",
        );
    } else if age > 2 * 3_600_000 {
        out.push_str(&format!("\n(Parsed {}.)\n", crate::graph::ago(age)));
    }
    Ok(out)
}

type Found = (String, String, String, i64, Option<i64>);

fn find(db: &Connection, ws: &str, target: &str) -> Result<Vec<Found>> {
    let mut stmt = db.prepare(
        "SELECT id, kind, path, line, community FROM code_graph_nodes
          WHERE workspace_id = ?1 AND (id = ?2 OR name = ?2)
          ORDER BY path, line LIMIT 50",
    )?;
    let mut rows: Vec<Found> = stmt
        .query_map(params![ws, target], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        // Fall back to a substring, so a half-remembered name still lands.
        let mut stmt = db.prepare(
            "SELECT id, kind, path, line, community FROM code_graph_nodes
              WHERE workspace_id = ?1 AND LOWER(name) LIKE '%' || LOWER(?2) || '%'
              ORDER BY LENGTH(name), path LIMIT 20",
        )?;
        rows = stmt
            .query_map(params![ws, target], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
    }
    Ok(rows)
}

/// One symbol's neighbours along an edge kind, in one direction.
fn related(db: &Connection, ws: &str, target: &str, kind: &str, incoming: bool, verb: &str)
    -> Result<String>
{
    let anchors = find(db, ws, target)?;
    if anchors.is_empty() {
        return Ok(format!("Nothing in this workspace is called '{target}'."));
    }
    let mut out = String::new();
    for (id, _, _, _, _) in anchors.iter().take(5) {
        let sql = if incoming {
            "SELECT n.id, n.kind, n.path, n.line FROM code_graph_edges e
               JOIN code_graph_nodes n ON n.workspace_id = e.workspace_id AND n.id = e.from_id
              WHERE e.workspace_id = ?1 AND e.to_id = ?2 AND e.kind = ?3
              ORDER BY n.path, n.line LIMIT 100"
        } else {
            "SELECT n.id, n.kind, n.path, n.line FROM code_graph_edges e
               JOIN code_graph_nodes n ON n.workspace_id = e.workspace_id AND n.id = e.to_id
              WHERE e.workspace_id = ?1 AND e.from_id = ?2 AND e.kind = ?3
              ORDER BY n.path, n.line LIMIT 100"
        };
        let mut stmt = db.prepare(sql)?;
        let rows: Vec<(String, String, String, i64)> = stmt
            .query_map(params![ws, id, kind], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        if rows.is_empty() {
            out.push_str(&format!("{id} is {verb} nothing here.\n"));
            continue;
        }
        out.push_str(&format!("{id} is {verb}:\n"));
        for (rid, rkind, path, line) in rows {
            out.push_str(&format!("- {rid} [{}] {}:{}\n", kind_label(&rkind), path, line));
        }
    }
    // Said once, at the end: a resolved-by-name edge must not read as a fact.
    if kind == "calls" {
        out.push_str(
            "(Call edges are resolved by name, so they are a strong hint rather than proof.)\n",
        );
    }
    Ok(out)
}

fn subsystem(db: &Connection, ws: &str, target: &str) -> Result<String> {
    let anchors = find(db, ws, target)?;
    let Some((id, _, _, _, community)) = anchors.into_iter().next() else {
        return Ok(format!("Nothing in this workspace is called '{target}'."));
    };
    let Some(c) = community else {
        return Ok(format!("{id} is not part of any subsystem (nothing connects to it)."));
    };
    let mut stmt = db.prepare(
        "SELECT id, kind, path, line FROM code_graph_nodes
          WHERE workspace_id = ? AND community = ? ORDER BY path, line LIMIT 200",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt
        .query_map(params![ws, c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = format!("{id} belongs to subsystem {c}, with {} member(s):\n", rows.len());
    for (rid, rkind, path, line) in rows {
        out.push_str(&format!("- {rid} [{}] {}:{}\n", kind_label(&rkind), path, line));
    }
    Ok(out)
}

fn subsystems(db: &Connection, ws: &str) -> Result<String> {
    let mut stmt = db.prepare(
        "SELECT community, COUNT(*) FROM code_graph_nodes
          WHERE workspace_id = ? AND community IS NOT NULL
          GROUP BY community ORDER BY 2 DESC LIMIT 30",
    )?;
    let rows: Vec<(i64, i64)> = stmt
        .query_map([ws], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        return Ok("No subsystems: nothing in this workspace connects to anything else.\n".into());
    }
    let mut out = format!("{} subsystem(s), largest first:\n", rows.len());
    for (c, n) in rows {
        // A few members and the files they sit in, which is what makes a
        // community recognisable to a human or an agent.
        let mut stmt = db.prepare(
            "SELECT name, path FROM code_graph_nodes
              WHERE workspace_id = ? AND community = ?
              ORDER BY CASE kind WHEN 'type' THEN 0 WHEN 'function' THEN 1 ELSE 2 END,
                       name LIMIT 5",
        )?;
        let members: Vec<(String, String)> = stmt
            .query_map(params![ws, c], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let names: Vec<String> = members.iter().map(|(n, _)| n.clone()).collect();
        let mut files: Vec<String> = members
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| !p.is_empty())
            .collect();
        files.sort();
        files.dedup();
        out.push_str(&format!(
            "- subsystem {c}: {n} member(s) — {}{}\n",
            names.join(", "),
            if files.is_empty() { String::new() } else { format!(" (in {})", files.join(", ")) }
        ));
    }
    Ok(out)
}

/// Shortest path between two symbols, ignoring edge direction.
///
/// Undirected on purpose: the question behind it is "how are these two
/// related", and a caller/callee chain that happens to run the other way is
/// still the relationship.
fn shortest_path(db: &Connection, ws: &str, from: &str, to: &str) -> Result<String> {
    if to.trim().is_empty() {
        anyhow::bail!("a path needs two ends: pass 'to' as well as 'target'");
    }
    let a = find(db, ws, from)?;
    let b = find(db, ws, to)?;
    let (Some(a), Some(b)) = (a.into_iter().next(), b.into_iter().next()) else {
        return Ok(format!("Could not find both '{from}' and '{to}' in this workspace."));
    };
    let start = a.0;
    let goal = b.0;
    if start == goal {
        return Ok(format!("{start} is the same symbol."));
    }

    let mut stmt = db.prepare(
        "SELECT from_id, to_id FROM code_graph_edges WHERE workspace_id = ?",
    )?;
    let edges: Vec<(String, String)> = stmt
        .query_map([ws], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut adj: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (f, t) in edges {
        adj.entry(f.clone()).or_default().push(t.clone());
        adj.entry(t).or_default().push(f);
    }

    let mut prev: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut queue = std::collections::VecDeque::from([start.clone()]);
    let mut seen: std::collections::HashSet<String> = [start.clone()].into_iter().collect();
    while let Some(v) = queue.pop_front() {
        if v == goal {
            break;
        }
        for u in adj.get(&v).into_iter().flatten() {
            if seen.insert(u.clone()) {
                prev.insert(u.clone(), v.clone());
                queue.push_back(u.clone());
            }
        }
    }
    if !seen.contains(&goal) {
        return Ok(format!("{start} and {goal} are not connected in this workspace."));
    }
    let mut chain = vec![goal.clone()];
    let mut cur = goal;
    while let Some(p) = prev.get(&cur) {
        chain.push(p.clone());
        cur = p.clone();
    }
    chain.reverse();
    Ok(format!("{} step(s):\n{}\n", chain.len() - 1, chain.join("\n  -> ")))
}

/// Has this workspace been parsed?
pub fn exists(db: &Connection, workspace_id: &str) -> bool {
    db.query_row(
        "SELECT 1 FROM code_graph_meta WHERE workspace_id = ?",
        [workspace_id],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexmesh_proto::codegraph::{GraphEdge, GraphNode, NodeKind};

    fn n(id: &str, name: &str, kind: NodeKind) -> GraphNode {
        GraphNode {
            id: id.into(),
            kind,
            name: name.into(),
            path: id.split('#').next().unwrap_or(id).into(),
            line: 1,
            lang: "rust".into(),
        }
    }

    fn e(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
        GraphEdge { from: from.into(), to: to.into(), kind }
    }

    /// Two clusters of functions that call within themselves, joined by one
    /// call -- the shape a real codebase has.
    fn sample() -> CodeGraph {
        let mut g = CodeGraph { files: 2, ..Default::default() };
        for i in 0..4 {
            g.nodes.push(n(&format!("a.rs#a{i}"), &format!("a{i}"), NodeKind::Function));
        }
        for i in 0..4 {
            g.nodes.push(n(&format!("b.rs#b{i}"), &format!("b{i}"), NodeKind::Function));
        }
        g.nodes.push(n("a.rs", "a.rs", NodeKind::File));
        g.nodes.push(n("b.rs", "b.rs", NodeKind::File));
        for i in 0..4 {
            for j in 0..4 {
                if i != j {
                    g.edges.push(e(
                        &format!("a.rs#a{i}"),
                        &format!("a.rs#a{j}"),
                        EdgeKind::Calls,
                    ));
                    g.edges.push(e(
                        &format!("b.rs#b{i}"),
                        &format!("b.rs#b{j}"),
                        EdgeKind::Calls,
                    ));
                }
            }
            g.edges.push(e("a.rs", &format!("a.rs#a{i}"), EdgeKind::Defines));
            g.edges.push(e("b.rs", &format!("b.rs#b{i}"), EdgeKind::Defines));
        }
        g.edges.push(e("a.rs#a0", "b.rs#b0", EdgeKind::Calls));
        g
    }

    fn stored() -> (AppState, CodeGraph) {
        let st = AppState::in_memory().unwrap();
        let g = sample();
        store(&st, "w1", &g).unwrap();
        (st, g)
    }

    /// A workspace with a graph, a mount on an online node, and a name --
    /// everything `stale_tick` needs to find it.
    fn mounted() -> AppState {
        let st = AppState::in_memory().unwrap();
        store(&st, "w1", &sample()).unwrap();
        let db = st.db.lock().unwrap();
        db.execute(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO workspaces (id,session_id,name,kind) VALUES ('w1','s1','app','repo')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO nodes (id,name,os,arch,node_version,status,first_seen_ms,
                                last_seen_ms,lease_expires_ms)
             VALUES ('nd1','machine-a','linux','x86_64','0.1.0','online',1,1,9999999999999)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO workspace_mounts (workspace_id,node_id,path) VALUES ('w1','nd1','/srv')",
            [],
        )
        .unwrap();
        drop(db);
        st
    }

    /// What `stale_tick` would pick up, without asking a node to parse
    /// anything: the selection is the whole decision, and it needs a live
    /// node and a real parse to exercise end to end.
    fn due_now(st: &AppState, now: i64) -> Option<String> {
        let db = st.db.lock().unwrap();
        db.query_row(DUE_SQL, rusqlite::params_from_iter(due_params(now)), |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// What `fresh_tick` would ask about, which is the whole of its
    /// decision: the question itself needs a live node to answer it.
    fn fp_due(st: &AppState, now: i64) -> Option<String> {
        let db = st.db.lock().unwrap();
        db.query_row(
            "SELECT m.workspace_id
               FROM code_graph_meta m
               JOIN workspace_mounts wm ON wm.workspace_id = m.workspace_id
               JOIN nodes n ON n.id = wm.node_id
              WHERE n.status = 'online' AND m.stale_at_ms IS NULL
                AND COALESCE(m.fp_asked_at_ms, 0) <= ?1
              ORDER BY COALESCE(m.fp_asked_at_ms, 0), wm.node_id LIMIT 1",
            params![now - FINGERPRINT_EVERY_MS],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    #[test]
    fn the_first_fingerprint_is_a_baseline_and_reparses_nothing() {
        // Every graph on a live fleet predates fingerprinting. Treating the
        // first answer as a change would reparse every workspace at once.
        let st = mounted();
        let db = st.db.lock().unwrap();
        fingerprint_arrived(&db, "w1", "12:3400:1790000000", 12, false);
        let stale: Option<i64> = db
            .query_row("SELECT stale_at_ms FROM code_graph_meta WHERE workspace_id='w1'", [],
                       |r| r.get(0))
            .unwrap();
        assert!(stale.is_none(), "a baseline is not a change");
        let kept: String = db
            .query_row("SELECT tree_fingerprint FROM code_graph_meta WHERE workspace_id='w1'",
                       [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, "12:3400:1790000000");
    }

    #[test]
    fn a_tree_that_moved_makes_the_graph_stale_and_one_that_did_not_does_nothing() {
        let st = mounted();
        let db = st.db.lock().unwrap();
        fingerprint_arrived(&db, "w1", "12:3400:1790000000", 12, false);
        fingerprint_arrived(&db, "w1", "12:3400:1790000000", 12, false);
        let same: Option<i64> = db
            .query_row("SELECT stale_at_ms FROM code_graph_meta WHERE workspace_id='w1'", [],
                       |r| r.get(0))
            .unwrap();
        assert!(same.is_none(), "the same fingerprint is not a change");
        fingerprint_arrived(&db, "w1", "13:3600:1790000900", 13, false);
        let moved: Option<i64> = db
            .query_row("SELECT stale_at_ms FROM code_graph_meta WHERE workspace_id='w1'", [],
                       |r| r.get(0))
            .unwrap();
        assert!(moved.is_some(), "a tree that moved is a graph to rebuild");
        let why: String = db
            .query_row("SELECT stale_reason FROM code_graph_meta WHERE workspace_id='w1'", [],
                       |r| r.get(0))
            .unwrap();
        assert_eq!(why, "the files changed");
    }

    #[test]
    fn a_walk_that_did_not_finish_cannot_say_the_tree_moved() {
        // Half a tree fingerprints differently every time the limit lands
        // somewhere else, and rebuilding on that would never stop.
        let st = mounted();
        let db = st.db.lock().unwrap();
        fingerprint_arrived(&db, "w1", "4000:11:12", 4000, true);
        fingerprint_arrived(&db, "w1", "4000:99:77", 4000, true);
        let stale: Option<i64> = db
            .query_row("SELECT stale_at_ms FROM code_graph_meta WHERE workspace_id='w1'", [],
                       |r| r.get(0))
            .unwrap();
        assert!(stale.is_none());
    }

    #[test]
    fn a_workspace_is_asked_about_once_and_then_left_alone_for_a_while() {
        let st = mounted();
        let now = now_ms();
        assert_eq!(fp_due(&st, now).as_deref(), Some("w1"), "never asked");
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE code_graph_meta SET fp_asked_at_ms = ?1 WHERE workspace_id='w1'",
                params![now],
            )
            .unwrap();
        }
        assert!(fp_due(&st, now).is_none(), "just asked");
        assert_eq!(
            fp_due(&st, now + FINGERPRINT_EVERY_MS + 1).as_deref(),
            Some("w1"),
            "and asked again later"
        );
    }

    #[test]
    fn an_answer_is_not_asked_for_while_a_rebuild_is_already_due() {
        let st = mounted();
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
        }
        assert!(fp_due(&st, now_ms()).is_none(),
                "it is already going to be reparsed; walking the tree twice adds nothing");
    }

    #[test]
    fn an_old_graph_on_a_node_that_cannot_answer_is_rebuilt_on_age() {
        // The fallback that keeps a fleet mid-upgrade honest: a node from
        // before the frame existed never answers, and the graph would
        // otherwise stand for ever.
        let st = mounted();
        let now = now_ms();
        {
            let db = st.db.lock().unwrap();
            db.execute(
                "UPDATE code_graph_meta
                    SET fp_asked_at_ms = ?1, fp_at_ms = NULL, built_at_ms = ?2
                  WHERE workspace_id='w1'",
                params![now - FINGERPRINT_WAIT_MS - 1, now - UNCHECKABLE_MAX_AGE_MS - 1],
            )
            .unwrap();
        }
        // The condition `fresh_tick` applies, without a node to ask.
        let (fp_at, fp_asked, built): (Option<i64>, Option<i64>, i64) = {
            let db = st.db.lock().unwrap();
            db.query_row(
                "SELECT fp_at_ms, fp_asked_at_ms, built_at_ms FROM code_graph_meta
                  WHERE workspace_id='w1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        let unanswered =
            fp_at.is_none() && fp_asked.is_some_and(|at| now - at > FINGERPRINT_WAIT_MS);
        assert!(unanswered && built <= now - UNCHECKABLE_MAX_AGE_MS);
    }

    #[test]
    fn an_answer_says_how_old_it_is_once_that_matters() {
        let st = mounted();
        let db = st.db.lock().unwrap();
        // Fresh: no age line, because every answer carrying one is noise.
        let fresh = query(&db, "w1", "overview", "", "").unwrap();
        assert!(!fresh.contains("Parsed"), "{fresh}");
        db.execute(
            "UPDATE code_graph_meta SET built_at_ms = ?1 WHERE workspace_id='w1'",
            params![now_ms() - 9 * 24 * 3_600_000],
        )
        .unwrap();
        let old = query(&db, "w1", "overview", "", "").unwrap();
        assert!(old.contains("Parsed 9 day(s) ago"), "{old}");
        // And once the files have moved it says that instead, which is the
        // thing an agent cannot find out for itself.
        mark_stale(&db, "w1");
        let moved = query(&db, "w1", "overview", "", "").unwrap();
        assert!(moved.contains("files have changed"), "{moved}");
    }

    #[test]
    fn a_fresh_graph_is_not_stale_and_does_not_say_it_is() {
        let st = mounted();
        let db = st.db.lock().unwrap();
        let s = summarise(&db, "w1", "app").unwrap();
        assert_eq!(s["stale"], false);
        assert!(s["stale_since_ms"].is_null());
    }

    #[test]
    fn publishing_marks_it_stale_and_the_graph_says_so() {
        // An agent cannot tell a stale answer from a wrong one, so it is
        // told rather than left to find out.
        let st = mounted();
        let db = st.db.lock().unwrap();
        mark_stale(&db, "w1");
        let s = summarise(&db, "w1", "app").unwrap();
        assert_eq!(s["stale"], true);
        assert!(s["stale_since_ms"].as_i64().unwrap() > 0);
    }

    #[test]
    fn the_mark_keeps_the_first_time_the_tree_moved() {
        // Otherwise a team publishing every thirty seconds pushes the
        // rebuild deadline out for ever and the graph is never rebuilt.
        let st = mounted();
        let db = st.db.lock().unwrap();
        db.execute(
            "UPDATE code_graph_meta SET stale_at_ms = 1000 WHERE workspace_id = 'w1'",
            [],
        )
        .unwrap();
        mark_stale(&db, "w1");
        let at: i64 = db
            .query_row("SELECT stale_at_ms FROM code_graph_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(at, 1000);
    }

    #[test]
    fn a_burst_of_publishes_coalesces_into_one_rebuild() {
        // Nothing is due for the first minute and a half: parsing a large
        // tree is minutes, and each parse throws away the last.
        let st = mounted();
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
            db.execute("UPDATE code_graph_meta SET built_at_ms = 0", []).unwrap();
        }
        let now = now_ms();
        assert!(due_now(&st, now).is_none(), "it should wait for the burst to settle");
        assert_eq!(due_now(&st, now + REBUILD_AFTER_MS + 1).as_deref(), Some("w1"));
    }

    #[test]
    fn nothing_is_due_while_no_node_holding_it_is_online() {
        let st = mounted();
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
            db.execute("UPDATE code_graph_meta SET built_at_ms = 0", []).unwrap();
            db.execute("UPDATE nodes SET status = 'offline'", []).unwrap();
        }
        assert!(due_now(&st, now_ms() + REBUILD_AFTER_MS + 1).is_none());
    }

    #[test]
    fn a_failing_rebuild_is_not_retried_in_a_tight_loop() {
        // `failed_at_ms` is what holds it off -- not built_at_ms, which is
        // when the graph was last PARSED and is reported with every answer.
        let st = mounted();
        let now = now_ms();
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
            db.execute(
                "UPDATE code_graph_meta SET built_at_ms = 0, failed_at_ms = ?1,
                        stale_reason = 'no node'",
                params![now],
            )
            .unwrap();
        }
        assert!(due_now(&st, now + REBUILD_AFTER_MS + 1).is_none());
        assert_eq!(due_now(&st, now + RETRY_AFTER_MS + 1).as_deref(), Some("w1"));
    }

    /// THE BUG THIS REPLACES: every successful build held off the next one
    /// for ten minutes, because the failure guard read built_at_ms. A cheap
    /// rebuild is due again once its floor has passed.
    #[test]
    fn a_cheap_build_does_not_hold_off_the_next_one_for_ten_minutes() {
        let st = mounted();
        let now = now_ms();
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
            db.execute(
                "UPDATE code_graph_meta SET built_at_ms = ?1, build_ms = 400, stale_at_ms = ?1",
                params![now],
            )
            .unwrap();
        }
        assert!(due_now(&st, now + MIN_REBUILD_GAP_MS - 1).is_none(), "the floor holds");
        assert_eq!(due_now(&st, now + MIN_REBUILD_GAP_MS + 1).as_deref(), Some("w1"),
                   "a 400 ms build is due again after the floor, not ten minutes");
    }

    /// A node that still parses the whole tree is paced by what that costs.
    #[test]
    fn an_expensive_build_is_paced_by_its_own_cost() {
        let st = mounted();
        let now = now_ms();
        let took = 120_000; // two minutes, a full parse of a large tree
        {
            let db = st.db.lock().unwrap();
            mark_stale(&db, "w1");
            db.execute(
                "UPDATE code_graph_meta SET built_at_ms = ?1, build_ms = ?2, stale_at_ms = ?1",
                params![now, took],
            )
            .unwrap();
        }
        assert!(due_now(&st, now + MIN_REBUILD_GAP_MS + 1).is_none());
        assert!(due_now(&st, now + REBUILD_GAP_FACTOR * took - 1).is_none());
        assert_eq!(due_now(&st, now + REBUILD_GAP_FACTOR * took + 1).as_deref(), Some("w1"));
    }

    #[test]
    fn a_failed_rebuild_is_reported_with_the_graph_it_left_behind() {
        // Serving a stale graph as if it were current is the one outcome
        // worse than an error message.
        let st = mounted();
        let db = st.db.lock().unwrap();
        mark_stale(&db, "w1");
        db.execute(
            "UPDATE code_graph_meta SET stale_reason = 'machine-a did not finish parsing'",
            [],
        )
        .unwrap();
        let s = summarise(&db, "w1", "app").unwrap();
        assert_eq!(s["stale"], true);
        assert_eq!(s["stale_reason"], "machine-a did not finish parsing");
    }

    #[test]
    fn storing_reports_what_it_found() {
        let st = AppState::in_memory().unwrap();
        let msg = store(&st, "w1", &sample()).unwrap();
        assert!(msg.contains("2 file(s)"), "{msg}");
        assert!(msg.contains("subsystem"), "{msg}");
    }

    #[test]
    fn the_two_clusters_come_out_as_two_subsystems() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "subsystems", "", "").unwrap();
        assert!(out.contains("2 subsystem(s)"), "{out}");
        assert!(out.contains("a0") && out.contains("b0"), "{out}");
    }

    #[test]
    fn a_node_with_no_relationships_is_in_no_subsystem() {
        // Otherwise every unconnected file node becomes a "subsystem" of one
        // and buries the real ones.
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let c: Option<i64> = db
            .query_row(
                "SELECT community FROM code_graph_nodes WHERE workspace_id='w1' AND id='a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(c.is_none(), "a file node has no relational edges: {c:?}");
        let out = query(&db, "w1", "subsystem", "a.rs", "").unwrap();
        assert!(out.contains("not part of any subsystem"), "{out}");
    }

    #[test]
    fn defines_edges_do_not_decide_subsystems() {
        // Weighting them would make every file its own community, which says
        // nothing a directory listing does not.
        let g = sample();
        let (comms, complete) = assign_communities(&g);
        assert!(complete, "a graph this small has no excuse for running out of time");
        let distinct: std::collections::HashSet<usize> = comms.iter().filter_map(|c| *c).collect();
        assert_eq!(distinct.len(), 2, "got {distinct:?} from {comms:?}");
    }

    #[test]
    fn grouping_stops_when_its_time_is_up_and_says_so() {
        // The grouping used to be bounded only by iteration count, which let
        // one real workspace hold the hub at full CPU until it was
        // restarted. Whatever it has when the clock runs out is what the
        // caller gets, labelled as provisional.
        let mut g = leiden::Graph::with_nodes(64);
        for i in 0..64u64 {
            g.add_edge(i as usize, ((i + 1) % 64) as usize, 1.0);
        }
        let (comms, complete) = leiden::within(
            &g,
            &leiden::Options::default(),
            Some(std::time::Instant::now()),
        );
        assert!(!complete, "no time at all means it cannot have finished");
        assert_eq!(comms.len(), 64, "every node still gets an answer");
    }

    #[test]
    fn callers_and_calls_are_different_questions() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let callers = query(&db, "w1", "callers", "b0", "").unwrap();
        assert!(callers.contains("a.rs#a0"), "{callers}");
        let calls = query(&db, "w1", "calls", "a0", "").unwrap();
        assert!(calls.contains("b.rs#b0"), "{calls}");
    }

    #[test]
    fn a_resolved_edge_is_labelled_as_a_hint() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "callers", "b0", "").unwrap();
        assert!(out.contains("resolved by name"), "a guess must not read as proof: {out}");
    }

    #[test]
    fn finding_a_symbol_gives_its_file_and_line() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "find", "a2", "").unwrap();
        assert!(out.contains("a.rs#a2") && out.contains("a.rs:1"), "{out}");
    }

    #[test]
    fn a_half_remembered_name_still_lands() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "find", "A1", "").unwrap();
        assert!(out.contains("a1"), "case and substring should both work: {out}");
    }

    #[test]
    fn a_name_that_is_not_there_says_so_plainly() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "find", "no_such_symbol", "").unwrap();
        assert!(out.contains("Nothing in this workspace"), "{out}");
    }

    #[test]
    fn a_path_between_two_symbols_is_found() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "path", "a1", "b1").unwrap();
        assert!(out.contains("step(s)"), "{out}");
        assert!(out.contains("a.rs#a0") && out.contains("b.rs#b0"), "via the bridge: {out}");
    }

    #[test]
    fn a_path_needs_two_ends() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        assert!(query(&db, "w1", "path", "a1", "").is_err());
    }

    #[test]
    fn a_subsystem_lists_what_is_in_it() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "subsystem", "a1", "").unwrap();
        assert!(out.contains("member(s)"), "{out}");
        assert!(out.contains("a.rs#a2"), "its neighbours: {out}");
    }

    #[test]
    fn an_overview_summarises_without_being_asked_for_specifics() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "overview", "", "").unwrap();
        assert!(out.contains("file(s) parsed"), "{out}");
        assert!(out.contains("function(s)"), "{out}");
        assert!(out.contains("subsystem"), "{out}");
    }

    #[test]
    fn an_unparsed_workspace_says_how_to_parse_it() {
        let st = AppState::in_memory().unwrap();
        let db = st.db.lock().unwrap();
        let err = query(&db, "never-built", "overview", "", "").unwrap_err().to_string();
        assert!(err.contains("rebuild=true"), "{err}");
    }

    #[test]
    fn an_unknown_question_lists_the_real_ones() {
        let (st, _) = stored();
        let db = st.db.lock().unwrap();
        let err = query(&db, "w1", "wat", "", "").unwrap_err().to_string();
        assert!(err.contains("callers") && err.contains("subsystems"), "{err}");
    }

    #[test]
    fn rebuilding_replaces_rather_than_accumulates() {
        // A symbol that no longer exists must not be offered to an agent as
        // though it did.
        let (st, _) = stored();
        let mut smaller = CodeGraph { files: 1, ..Default::default() };
        smaller.nodes.push(n("a.rs#only", "only", NodeKind::Function));
        store(&st, "w1", &smaller).unwrap();
        let db = st.db.lock().unwrap();
        let n: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM code_graph_nodes WHERE workspace_id = 'w1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let out = query(&db, "w1", "find", "a1", "").unwrap();
        assert!(out.contains("Nothing"), "the old symbols must be gone: {out}");
    }

    #[test]
    fn a_partial_graph_says_so_in_every_answer() {
        let st = AppState::in_memory().unwrap();
        let mut g = sample();
        g.truncated = true;
        store(&st, "w1", &g).unwrap();
        let db = st.db.lock().unwrap();
        let out = query(&db, "w1", "overview", "", "").unwrap();
        assert!(out.contains("partial"), "{out}");
    }

    #[test]
    fn an_empty_graph_is_storable_and_queryable() {
        let st = AppState::in_memory().unwrap();
        store(&st, "w1", &CodeGraph::default()).unwrap();
        let db = st.db.lock().unwrap();
        assert!(exists(&db, "w1"));
        let out = query(&db, "w1", "subsystems", "", "").unwrap();
        assert!(out.contains("No subsystems"), "{out}");
    }
}

/// Which workspaces a graph question covers.
///
/// A code graph is derived from the files in ONE directory on ONE machine, so
/// a workspace has always been its natural unit. That stopped being enough
/// the moment a seat got a workspace of its own: a team of four in one
/// repository is now four workspaces, and "where is this function" asked in
/// one of them could not see the other three. And a fleet that has parsed
/// several repositories knows perfectly well where a symbol lives, while
/// every agent on it had to be in the right seat to ask.
///
/// So the question can be asked of this seat's own workspace (the default,
/// and what it has always done), of every workspace this team has, or of
/// every workspace on the fleet that has been parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphScope {
    /// This seat's own workspace.
    Workspace,
    /// Every workspace belonging to this seat's team.
    Session,
    /// Every workspace on the fleet that has a graph.
    Global,
}

impl GraphScope {
    pub fn parse(scope: Option<&str>) -> Result<Self> {
        match scope.unwrap_or("workspace") {
            "workspace" | "mine" | "" => Ok(GraphScope::Workspace),
            "session" | "team" => Ok(GraphScope::Session),
            "global" | "fleet" => Ok(GraphScope::Global),
            other => anyhow::bail!(
                "unknown graph scope '{other}'; use workspace (your own, the default), \
                 session (every workspace this team has) or global (every parsed \
                 workspace on the fleet)"
            ),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            GraphScope::Workspace => "workspace",
            GraphScope::Session => "session",
            GraphScope::Global => "global",
        }
    }
}

/// The workspaces a wider question should be asked of: id and name, ONLY
/// those that have been parsed.
///
/// Parsing is minutes of somebody's machine, so a wider question answers from
/// what is already there and says what it skipped. Building four graphs
/// because an agent asked a question in the wrong scope is not a thing to do
/// on somebody's laptop without being asked.
pub fn parsed_in_scope(
    db: &Connection,
    scope: GraphScope,
    session_id: &str,
    own_ws: &str,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    match scope {
        GraphScope::Workspace => {
            let name: Option<String> = db
                .query_row("SELECT name FROM workspaces WHERE id = ?", [own_ws], |r| r.get(0))
                .optional()?;
            out.push((own_ws.to_string(), name.unwrap_or_else(|| own_ws.to_string())));
        }
        GraphScope::Session => {
            let mut stmt = db.prepare(
                "SELECT w.id, w.name FROM workspaces w
                  JOIN code_graph_meta g ON g.workspace_id = w.id
                 WHERE w.session_id = ? ORDER BY w.name",
            )?;
            for row in stmt.query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))? {
                out.push(row?);
            }
        }
        GraphScope::Global => {
            let mut stmt = db.prepare(
                "SELECT w.id, w.name FROM workspaces w
                  JOIN code_graph_meta g ON g.workspace_id = w.id
                 ORDER BY w.name",
            )?;
            for row in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
                out.push(row?);
            }
        }
    }
    Ok(out)
}

/// Ask the same question of several workspaces and label each answer.
///
/// `path` is left out of this deliberately: a route from one symbol to
/// another is a walk through one graph, and stitching two parsed trees
/// together by name would invent edges that are not there.
pub fn query_across(
    db: &Connection,
    places: &[(String, String)],
    what: &str,
    target: &str,
    to: &str,
) -> Result<String> {
    if places.len() == 1 {
        return query(db, &places[0].0, what, target, to);
    }
    if what == "path" {
        anyhow::bail!(
            "a path between two symbols is a walk through ONE parsed tree, so ask it of a \
             single workspace: leave the scope out, or say which workspace you mean."
        );
    }
    let mut found = 0usize;
    let mut out = String::new();
    for (id, name) in places {
        let answer = match query(db, id, what, target, to) {
            Ok(a) => a,
            // One workspace whose graph is unreadable must not take the
            // other answers down with it.
            Err(e) => format!("could not be read: {e}"),
        };
        // "Nothing in this workspace is called X" from eleven workspaces is
        // noise; the count at the end is the useful part.
        if nothing_found(&answer) {
            continue;
        }
        found += 1;
        out.push_str(&format!("=== {name} ===\n{answer}\n"));
    }
    if found == 0 {
        return Ok(format!(
            "Nothing matching '{target}' in any of the {} parsed workspace(s): {}.",
            places.len(),
            places.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    out.push_str(&format!(
        "({found} of {} parsed workspace(s) had something; the rest had nothing matching)\n",
        places.len()
    ));
    Ok(out)
}

/// Does this answer mean "no"? Matched on the sentences `query` itself
/// writes, so a wider search can leave the empty ones out.
fn nothing_found(answer: &str) -> bool {
    let a = answer.trim();
    a.starts_with("Nothing in this workspace")
        || a.starts_with("Nothing calls")
        || a.starts_with("Nothing imports")
        || a.starts_with("No ")
        || a.is_empty()
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn a_scope_is_one_of_three_and_says_so_when_it_is_not() {
        assert_eq!(GraphScope::parse(None).unwrap(), GraphScope::Workspace);
        assert_eq!(GraphScope::parse(Some("team")).unwrap(), GraphScope::Session);
        assert_eq!(GraphScope::parse(Some("fleet")).unwrap(), GraphScope::Global);
        let e = GraphScope::parse(Some("everything")).unwrap_err().to_string();
        assert!(e.contains("workspace"), "{e}");
        assert!(e.contains("global"), "{e}");
    }

    /// A wider scope lists only PARSED workspaces, and the seat's own scope
    /// lists its own whether it has been parsed or not -- because that is the
    /// one `query_code_graph` will build on demand.
    #[test]
    fn a_wider_scope_offers_only_what_has_been_parsed() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(nexmesh_proto::SCHEMA_SQL).unwrap();
        crate::state::migrate(&db).unwrap();
        db.execute_batch(
            "INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s1','T',1,1);
             INSERT INTO sessions (id,name,created_at_ms,last_active_ms) VALUES ('s2','U',1,1);
             INSERT INTO workspaces (id,session_id,name,kind)
               VALUES ('w1','s1','mine','node_local');
             INSERT INTO workspaces (id,session_id,name,kind)
               VALUES ('w2','s1','theirs','node_local');
             INSERT INTO workspaces (id,session_id,name,kind)
               VALUES ('w3','s2','other-team','node_local');
             INSERT INTO code_graph_meta (workspace_id,built_at_ms,files) VALUES ('w2',1,3);
             INSERT INTO code_graph_meta (workspace_id,built_at_ms,files) VALUES ('w3',1,3);",
        )
        .unwrap();

        let own = parsed_in_scope(&db, GraphScope::Workspace, "s1", "w1").unwrap();
        assert_eq!(own, vec![("w1".to_string(), "mine".to_string())]);

        // w1 is this team's and is NOT parsed, so a team-wide question does
        // not include it -- and must not quietly parse it either.
        let team = parsed_in_scope(&db, GraphScope::Session, "s1", "w1").unwrap();
        assert_eq!(team.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(), vec!["w2"]);

        let fleet = parsed_in_scope(&db, GraphScope::Global, "s1", "w1").unwrap();
        let mut ids: Vec<&str> = fleet.iter().map(|(i, _)| i.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["w2", "w3"], "a fleet question crosses teams");
    }
}
