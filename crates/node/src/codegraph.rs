//! Building a workspace's code graph, on the machine that holds the files.
//!
//! tree-sitter, so parsing is deterministic and nothing leaves the node: the
//! same tree on every machine, no model, no network. The result goes to the
//! hub, which keeps it for the whole team -- so a codebase is read once rather
//! than once per agent.
//!
//! ## Languages are a table, not code
//!
//! Each grammar names its syntax nodes differently, and those names change
//! between grammar releases. Keeping the mapping as DATA means supporting a
//! new language is a row plus a grammar crate, and tracking a rename is a
//! one-line edit -- the same reasoning as the CLI descriptors.
//!
//! ## What it deliberately does not do
//!
//! No type inference and no cross-file resolution beyond matching names. A
//! `Calls` edge means "this function names that symbol in a call position",
//! which is why `EdgeKind::is_certain` exists: the hub carries that
//! distinction to the agent rather than presenting a guess as a fact.

use nexmesh_proto::codegraph::{CodeGraph, EdgeKind, GraphEdge, GraphNode, NodeKind, SkippedFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How much of a workspace to read. A graph of a monorepo is not worth an
/// unbounded walk, and a partial graph that says so beats a hung node.
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_files: 4000, max_file_bytes: 1_500_000 }
    }
}

/// Directories never worth parsing: dependencies, build output, version
/// control. Walking `node_modules` would bury the project's own code in the
/// graph and in every answer drawn from it.
const SKIP_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", "target", "node_modules", "vendor", "dist", "build",
    ".venv", "venv", "__pycache__", ".mypy_cache", ".pytest_cache", ".tox",
    ".next", ".nuxt", ".cargo", ".rustup", "site-packages", ".terraform",
    "coverage", ".gradle", ".idea", ".vscode", ".nexmesh",
];

/// One language's syntax-node names.
struct LangSpec {
    id: &'static str,
    exts: &'static [&'static str],
    /// Node kinds that declare something callable.
    functions: &'static [&'static str],
    /// Node kinds that declare a type-like thing.
    types: &'static [&'static str],
    /// Node kinds that bring in another module.
    imports: &'static [&'static str],
    /// Node kinds that are a call.
    calls: &'static [&'static str],
    /// Field holding the callee inside a call node.
    callee_field: &'static str,
}

/// The languages this build can parse.
///
/// Extending it needs a grammar crate and a row. The set here is what the
/// fleet actually runs; a file in anything else is reported as skipped rather
/// than silently ignored.
const LANGS: &[LangSpec] = &[
    LangSpec {
        id: "rust",
        exts: &["rs"],
        functions: &["function_item"],
        types: &["struct_item", "enum_item", "trait_item", "type_item", "union_item"],
        imports: &["use_declaration", "mod_item"],
        calls: &["call_expression", "macro_invocation"],
        callee_field: "function",
    },
    LangSpec {
        id: "python",
        exts: &["py", "pyi"],
        functions: &["function_definition"],
        types: &["class_definition"],
        imports: &["import_statement", "import_from_statement"],
        calls: &["call"],
        callee_field: "function",
    },
    LangSpec {
        id: "javascript",
        exts: &["js", "mjs", "cjs", "jsx"],
        functions: &["function_declaration", "method_definition", "generator_function_declaration"],
        types: &["class_declaration"],
        imports: &["import_statement"],
        calls: &["call_expression"],
        callee_field: "function",
    },
    LangSpec {
        id: "typescript",
        exts: &["ts", "tsx", "mts"],
        functions: &["function_declaration", "method_definition", "method_signature"],
        types: &["class_declaration", "interface_declaration", "type_alias_declaration", "enum_declaration"],
        imports: &["import_statement"],
        calls: &["call_expression"],
        callee_field: "function",
    },
    LangSpec {
        id: "go",
        exts: &["go"],
        functions: &["function_declaration", "method_declaration"],
        types: &["type_declaration"],
        imports: &["import_declaration"],
        calls: &["call_expression"],
        callee_field: "function",
    },
    LangSpec {
        id: "php",
        exts: &["php"],
        functions: &["function_definition", "method_declaration"],
        types: &["class_declaration", "interface_declaration", "trait_declaration", "enum_declaration"],
        imports: &["namespace_use_declaration", "require_expression", "include_expression"],
        calls: &["function_call_expression", "member_call_expression", "scoped_call_expression"],
        callee_field: "function",
    },
    LangSpec {
        id: "bash",
        exts: &["sh", "bash"],
        functions: &["function_definition"],
        types: &[],
        imports: &[],
        calls: &["command"],
        callee_field: "name",
    },
];

fn spec_for(path: &Path) -> Option<&'static LangSpec> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    LANGS.iter().find(|l| l.exts.contains(&ext.as_str()))
}

fn language_of(id: &str) -> Option<tree_sitter::Language> {
    Some(match id {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        _ => return None,
    })
}

/// Walk a workspace and build its graph.
pub fn scan(root: &Path, limits: &Limits) -> CodeGraph {
    let mut graph = CodeGraph::default();
    // name -> ids, for resolving a call by the only thing we have: its name.
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    let mut pending: Vec<(String, String)> = Vec::new(); // (caller id, callee name)

    let files = walk(root, limits, &mut graph);
    for abs in files {
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let Some(spec) = spec_for(&abs) else { continue };
        let Some(lang) = language_of(spec.id) else {
            graph.skipped.push(SkippedFile {
                path: rel.clone(),
                reason: format!("no grammar for {} in this build", spec.id),
            });
            continue;
        };
        let source = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) => {
                // Binary or unreadable. Said out loud, not dropped.
                graph.skipped.push(SkippedFile { path: rel.clone(), reason: e.to_string() });
                continue;
            }
        };
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&lang).is_err() {
            graph.skipped.push(SkippedFile {
                path: rel.clone(),
                reason: "grammar rejected by this tree-sitter".into(),
            });
            continue;
        }
        let Some(tree) = parser.parse(&source, None) else {
            graph.skipped.push(SkippedFile { path: rel.clone(), reason: "unparseable".into() });
            continue;
        };

        graph.files += 1;
        let file_id = rel.clone();
        graph.nodes.push(GraphNode {
            id: file_id.clone(),
            kind: NodeKind::File,
            name: Path::new(&rel)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| rel.clone()),
            path: rel.clone(),
            line: 1,
            lang: spec.id.to_string(),
        });

        collect(
            tree.root_node(),
            source.as_bytes(),
            spec,
            &rel,
            &file_id,
            &mut graph,
            &mut by_name,
            &mut pending,
        );
    }

    // Resolve calls by name. Ambiguity is dropped rather than guessed: two
    // functions sharing a name means an edge to either could be wrong, and a
    // wrong edge is worse than a missing one in something an agent reasons
    // from.
    for (caller, callee_name) in pending {
        if let Some(ids) = by_name.get(&callee_name) {
            if ids.len() == 1 && ids[0] != caller {
                graph.edges.push(GraphEdge {
                    from: caller,
                    to: ids[0].clone(),
                    kind: EdgeKind::Calls,
                });
            }
        }
    }

    graph.prune_dangling();
    graph.dedup_edges();
    graph
}

#[allow(clippy::too_many_arguments)]
fn collect(
    root: tree_sitter::Node,
    src: &[u8],
    spec: &LangSpec,
    rel: &str,
    file_id: &str,
    graph: &mut CodeGraph,
    by_name: &mut HashMap<String, Vec<String>>,
    pending: &mut Vec<(String, String)>,
) {
    // The innermost declaration we are inside, so a call is attributed to the
    // function that makes it rather than to the file.
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut cursor = root.walk();
    let mut visited_children = false;

    loop {
        let node = cursor.node();
        if !visited_children {
            while let Some((end, _)) = stack.last() {
                if node.start_byte() >= *end {
                    stack.pop();
                } else {
                    break;
                }
            }

            let kind = node.kind();
            let declares = if spec.functions.contains(&kind) {
                Some(NodeKind::Function)
            } else if spec.types.contains(&kind) {
                Some(NodeKind::Type)
            } else {
                None
            };

            if let Some(nk) = declares {
                if let Some(name) = field_text(node, "name", src).or_else(|| ident_text(node, src))
                {
                    let id = format!("{rel}#{name}");
                    graph.nodes.push(GraphNode {
                        id: id.clone(),
                        kind: nk,
                        name: name.clone(),
                        path: rel.to_string(),
                        line: node.start_position().row as u32 + 1,
                        lang: spec.id.to_string(),
                    });
                    graph.edges.push(GraphEdge {
                        from: file_id.to_string(),
                        to: id.clone(),
                        kind: EdgeKind::Defines,
                    });
                    by_name.entry(name).or_default().push(id.clone());
                    stack.push((node.end_byte(), id));
                }
            } else if spec.imports.contains(&kind) {
                if let Some(text) = node_text(node, src) {
                    if let Some(target) = import_target(text) {
                        graph.edges.push(GraphEdge {
                            from: file_id.to_string(),
                            to: target,
                            kind: EdgeKind::Imports,
                        });
                    }
                }
            } else if spec.calls.contains(&kind) {
                if let Some(callee) = field_text(node, spec.callee_field, src)
                    .or_else(|| ident_text(node, src))
                {
                    // The last path segment: `foo::bar::baz` or `self.baz` is
                    // recorded as `baz`, which is all a name match can use.
                    let short = callee
                        .rsplit(|c| c == ':' || c == '.' || c == '>')
                        .next()
                        .unwrap_or(&callee)
                        .trim()
                        .trim_start_matches('!')
                        .to_string();
                    if !short.is_empty() {
                        let from = stack
                            .last()
                            .map(|(_, id)| id.clone())
                            .unwrap_or_else(|| file_id.to_string());
                        pending.push((from, short));
                    }
                }
            }
        }

        if !visited_children && cursor.goto_first_child() {
            visited_children = false;
        } else if cursor.goto_next_sibling() {
            visited_children = false;
        } else if cursor.goto_parent() {
            visited_children = true;
        } else {
            break;
        }
    }
}

fn node_text(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    node.utf8_text(src).ok().map(|s| s.to_string())
}

fn field_text(node: tree_sitter::Node, field: &str, src: &[u8]) -> Option<String> {
    node.child_by_field_name(field).and_then(|c| node_text(c, src))
}

/// First identifier-ish child, for grammars that do not name the field.
fn ident_text(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k.contains("identifier") || k == "word" || k == "name" {
            return node_text(child, src);
        }
    }
    None
}

/// Pull a module path out of an import statement's source text.
fn import_target(text: String) -> Option<String> {
    let t = text.trim().trim_end_matches(';').trim();
    // Quoted path: `import x from "./a"`, `require 'b'`, `use "c"`.
    if let Some(start) = t.find(['"', '\'']) {
        let q = t.as_bytes()[start] as char;
        if let Some(end) = t[start + 1..].find(q) {
            let inner = &t[start + 1..start + 1 + end];
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
    }
    // Otherwise the last word: `use crate::a::b`, `import os.path`.
    t.split_whitespace()
        .last()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '.' && c != '/'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
}

/// Collect candidate files, honouring the limits.
fn walk(root: &Path, limits: &Limits, graph: &mut CodeGraph) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                // Symlinked directories are not followed: a link to `/` would
                // walk the machine, and a link back up would never terminate.
                if meta.file_type().is_symlink() || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                dirs.push(path);
            } else if meta.is_file() {
                if spec_for(&path).is_none() {
                    continue;
                }
                if meta.len() > limits.max_file_bytes {
                    let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
                    graph.skipped.push(SkippedFile {
                        path: rel.to_string(),
                        reason: format!("{} bytes, over the limit", meta.len()),
                    });
                    continue;
                }
                if out.len() >= limits.max_files {
                    graph.truncated = true;
                    return out;
                }
                out.push(path);
            }
        }
    }
    out.sort(); // deterministic order, so ids and communities are stable
    out
}
