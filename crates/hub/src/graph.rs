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
    let answer = tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| anyhow::anyhow!("{node_id} did not finish parsing within 5 minutes"))?
        .map_err(|_| anyhow::anyhow!("the node went away while parsing"))?;

    let graph: CodeGraph =
        serde_json::from_value(answer).context("the node sent a graph we cannot read")?;
    store(st, workspace_id, &graph)
}

/// Replace a workspace's graph and recompute its communities.
pub fn store(st: &AppState, workspace_id: &str, graph: &CodeGraph) -> Result<String> {
    let communities = assign_communities(graph);
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
    tx.commit()?;
    let mut out = format!(
        "Parsed {} file(s): {} symbols, {} relationships, {} subsystem(s).",
        graph.files,
        graph.nodes.len(),
        graph.edges.len(),
        distinct
    );
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
fn assign_communities(graph: &CodeGraph) -> Vec<Option<usize>> {
    let n = graph.nodes.len();
    if n == 0 {
        return Vec::new();
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
    let comms = leiden::communities(&g, &leiden::Options::default());
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
    comms
        .into_iter()
        .enumerate()
        .map(|(i, c)| if degree[i] > 0 { Some(c) } else { None })
        .collect()
}

fn kind_label(k: &str) -> &str {
    k
}

/// Answer one question about the graph.
pub fn query(db: &Connection, workspace_id: &str, what: &str, target: &str, to: &str)
    -> Result<String>
{
    let meta: Option<(i64, i64, i64, Option<String>, i64)> = db
        .query_row(
            "SELECT built_at_ms, files, truncated, truncated_reason, communities
               FROM code_graph_meta WHERE workspace_id = ?",
            [workspace_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((_built, files, truncated, truncated_why, communities)) = meta else {
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
        let comms = assign_communities(&g);
        let distinct: std::collections::HashSet<usize> = comms.iter().filter_map(|c| *c).collect();
        assert_eq!(distinct.len(), 2, "got {distinct:?} from {comms:?}");
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
