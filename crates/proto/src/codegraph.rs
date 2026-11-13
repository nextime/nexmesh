//! The code graph: what a workspace contains and how it hangs together.
//!
//! Every agent that joins a codebase starts by reading it, and a cheap seat
//! spends its context doing badly what an AST does exactly. So the node parses
//! the workspace once, deterministically, and the hub keeps the result for the
//! whole team to query.
//!
//! Deliberately **derived, not authored**: this is regenerated from the files
//! and carries no opinions. Anything a human or an agent concluded belongs in
//! `memory`, which is authored, attributed and scoped. Keeping the two apart
//! is what lets the graph be thrown away and rebuilt without losing anything.
//!
//! Parsing is tree-sitter, so there is no model involved and no network: the
//! same tree on every machine, and nothing leaves the node.

use serde::{Deserialize, Serialize};

/// What a node in the graph is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    /// A function, method or procedure.
    Function,
    /// A struct, class, enum, interface or trait.
    Type,
    /// A module, namespace or package.
    Module,
    /// A constant or top-level variable worth naming.
    Constant,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Function => "function",
            NodeKind::Type => "type",
            NodeKind::Module => "module",
            NodeKind::Constant => "constant",
        }
    }
}

/// How two nodes relate.
///
/// `Defines` and `Imports` come straight from the syntax and are facts.
/// `Calls` and `References` are resolved by name, which is a guess wherever a
/// language allows two things to share one -- so they are reported as what
/// they are rather than dressed up as certainties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// This file or type defines that symbol. From the syntax tree.
    Defines,
    /// This file imports that module. From the syntax tree.
    Imports,
    /// This function names that symbol in a call position. Resolved by name.
    Calls,
    /// This symbol names that one somewhere else. Resolved by name.
    References,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Defines => "defines",
            EdgeKind::Imports => "imports",
            EdgeKind::Calls => "calls",
            EdgeKind::References => "references",
        }
    }

    /// Is this edge read off the tree, or inferred by matching a name?
    ///
    /// Worth carrying all the way to the agent: "x calls y" deserves less
    /// trust than "x is defined in y", and conflating them is how a confident
    /// wrong answer gets built on.
    pub fn is_certain(&self) -> bool {
        matches!(self, EdgeKind::Defines | EdgeKind::Imports)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Stable within one workspace: `<relative path>#<symbol>` for a symbol,
    /// the relative path alone for a file. Stable so that a rebuild of an
    /// unchanged file produces the same ids and a stored community survives.
    pub id: String,
    pub kind: NodeKind,
    /// The bare name, for lookup: `parse_header`, not `src/a.rs#parse_header`.
    pub name: String,
    /// Workspace-relative, so a graph built on one machine reads correctly on
    /// another with a different mount point.
    pub path: String,
    /// 1-based, as an editor counts.
    pub line: u32,
    /// Language id as the parser knows it, e.g. "rust".
    pub lang: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

/// One workspace's graph, as the node hands it over.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Files actually parsed.
    pub files: u32,
    /// Files skipped, and why -- unreadable, too large, no grammar. Reported
    /// rather than silently dropped: a graph with a hole in it that nobody
    /// mentions is worse than a smaller one that says so.
    pub skipped: Vec<SkippedFile>,
    /// True when a limit stopped the walk, so a caller knows the graph is
    /// partial.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: String,
}

impl CodeGraph {
    /// Drop edges whose endpoints are not in the graph.
    ///
    /// Name resolution produces edges to things that turn out not to exist
    /// here -- a call into a dependency, a method on a type from elsewhere.
    /// Keeping them would make every query answer with ghosts.
    pub fn prune_dangling(&mut self) {
        let ids: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        self.edges.retain(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()));
    }

    /// Remove duplicate edges, keeping the most certain kind between a pair.
    pub fn dedup_edges(&mut self) {
        self.edges.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then(a.to.cmp(&b.to))
                .then(b.kind.is_certain().cmp(&a.kind.is_certain()))
                .then(a.kind.as_str().cmp(b.kind.as_str()))
        });
        self.edges.dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            kind: NodeKind::Function,
            name: name.into(),
            path: "src/a.rs".into(),
            line: 1,
            lang: "rust".into(),
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
        GraphEdge { from: from.into(), to: to.into(), kind }
    }

    #[test]
    fn a_syntax_fact_outranks_a_resolved_guess() {
        assert!(EdgeKind::Defines.is_certain());
        assert!(EdgeKind::Imports.is_certain());
        assert!(!EdgeKind::Calls.is_certain(), "a call is resolved by name");
        assert!(!EdgeKind::References.is_certain());
    }

    #[test]
    fn edges_into_nothing_are_pruned() {
        // Calls into a dependency resolve to names this graph does not hold.
        let mut g = CodeGraph {
            nodes: vec![node("src/a.rs#f", "f"), node("src/a.rs#g", "g")],
            edges: vec![
                edge("src/a.rs#f", "src/a.rs#g", EdgeKind::Calls),
                edge("src/a.rs#f", "serde::from_str", EdgeKind::Calls),
            ],
            ..Default::default()
        };
        g.prune_dangling();
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].to, "src/a.rs#g");
    }

    #[test]
    fn duplicate_edges_collapse() {
        let mut g = CodeGraph {
            nodes: vec![node("a", "a"), node("b", "b")],
            edges: vec![
                edge("a", "b", EdgeKind::Calls),
                edge("a", "b", EdgeKind::Calls),
                edge("a", "b", EdgeKind::References),
            ],
            ..Default::default()
        };
        g.dedup_edges();
        assert_eq!(g.edges.len(), 2, "one per kind, not one per observation");
    }

    #[test]
    fn a_partial_graph_says_so() {
        // A hole nobody mentions is worse than a smaller graph that admits it.
        let g = CodeGraph {
            truncated: true,
            skipped: vec![SkippedFile { path: "big.bin".into(), reason: "too large".into() }],
            ..Default::default()
        };
        assert!(g.truncated);
        assert_eq!(g.skipped[0].reason, "too large");
    }

    #[test]
    fn it_survives_a_round_trip_through_json() {
        let g = CodeGraph {
            nodes: vec![node("src/a.rs#f", "f")],
            edges: vec![edge("src/a.rs#f", "src/a.rs#f", EdgeKind::Calls)],
            files: 1,
            skipped: vec![],
            truncated: false,
        };
        let back: CodeGraph = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn node_ids_are_workspace_relative_so_a_graph_travels() {
        // A graph built on one machine must read correctly on another with a
        // different mount point.
        let n = node("src/a.rs#f", "f");
        assert!(!n.path.starts_with('/'), "{}", n.path);
        assert!(n.id.starts_with(&n.path));
    }
}
