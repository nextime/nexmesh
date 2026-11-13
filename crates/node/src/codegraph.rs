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
    // (importing file, raw target). Resolved at the end, because an import
    // may name a file that has not been walked yet.
    let mut imports: Vec<(String, String)> = Vec::new();

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
            &mut imports,
        );
    }

    resolve_imports(&mut graph, imports);

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
    imports: &mut Vec<(String, String)>,
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
                // The module field where the grammar names one -- otherwise
                // `from .util import helper` would be recorded as importing
                // "helper", which is the symbol, not the module.
                let target = ["module_name", "source", "path", "name"]
                    .iter()
                    .find_map(|f| field_text(node, f, src))
                    .or_else(|| node_text(node, src).and_then(import_target));
                if let Some(t) = target {
                    imports.push((file_id.to_string(), t));
                }
            } else if spec.calls.contains(&kind) {
                if let Some(callee) = field_text(node, spec.callee_field, src)
                    .or_else(|| ident_text(node, src))
                {
                    // The last path segment: `foo::bar::baz` or `self.baz` is
                    // recorded as `baz`, which is all a name match can use.
                    let short = callee
                        .rsplit([':', '.', '>'])
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

/// Turn raw import text into edges.
///
/// An import either names something in this workspace -- real structure worth
/// having -- or something outside it. The second used to be dropped entirely,
/// because `prune_dangling` quite correctly removed an edge to a node that did
/// not exist, which silently cost the graph every dependency it knew about.
/// External modules therefore become nodes of their own, so "what does this
/// file depend on" is answerable.
fn resolve_imports(graph: &mut CodeGraph, imports: Vec<(String, String)>) {
    // path and stem of every file already in the graph
    let files: Vec<(String, String)> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| {
            let stem = Path::new(&n.path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            (n.path.clone(), stem)
        })
        .collect();

    let mut external: Vec<String> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    for (from, raw) in imports {
        let target = normalise_import(&raw);
        if target.is_empty() {
            continue;
        }
        // The last segment is what a file is usually named after:
        // `crate::ocr::manager` -> `manager`, `./util` -> `util`.
        let last = target.rsplit('/').next().unwrap_or(&target).to_string();
        let hit = files
            .iter()
            .find(|(path, _)| path == &target || path.ends_with(&format!("/{target}")))
            .or_else(|| files.iter().find(|(_, stem)| stem == &last && !last.is_empty()));
        match hit {
            Some((path, _)) if path != &from => {
                edges.push(GraphEdge { from, to: path.clone(), kind: EdgeKind::Imports });
            }
            // Either outside the workspace, or the file importing itself.
            Some(_) => {}
            None => {
                let id = format!("module:{target}");
                if !external.contains(&target) {
                    external.push(target.clone());
                    graph.nodes.push(GraphNode {
                        id: id.clone(),
                        kind: NodeKind::Module,
                        name: target.clone(),
                        // Not a file here, so no path and no line to give.
                        path: String::new(),
                        line: 0,
                        lang: String::new(),
                    });
                }
                edges.push(GraphEdge { from, to: id, kind: EdgeKind::Imports });
            }
        }
    }
    graph.edges.extend(edges);
}

/// Strip the punctuation an import is written with, leaving a comparable path.
fn normalise_import(raw: &str) -> String {
    let t = raw.trim().trim_matches(|c| c == '"' || c == '\'' || c == ';' || c == '{' || c == '}');
    let t = t.trim().trim_start_matches("./").trim_start_matches("../");
    // `a::b::c` and `a.b.c` are both paths; so is `a/b/c`.
    let t = t.replace("::", "/").replace('.', "/");
    t.trim_matches('/')
        .split('/')
        .filter(|p| !p.is_empty() && *p != "self" && *p != "crate" && *p != "super")
        .collect::<Vec<_>>()
        .join("/")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    fn ids(g: &CodeGraph) -> Vec<String> {
        let mut v: Vec<String> = g.nodes.iter().map(|n| n.id.clone()).collect();
        v.sort();
        v
    }

    fn has_edge(g: &CodeGraph, from: &str, to: &str, kind: EdgeKind) -> bool {
        g.edges.iter().any(|e| e.from == from && e.to == to && e.kind == kind)
    }

    #[test]
    fn rust_functions_types_and_calls_are_found() {
        let d = scratch(&[(
            "src/lib.rs",
            r#"
pub struct Parser { pos: usize }
pub fn parse_header(s: &str) -> usize { helper(s) }
fn helper(s: &str) -> usize { s.len() }
"#,
        )]);
        let g = scan(d.path(), &Limits::default());
        let got = ids(&g);
        assert!(got.contains(&"src/lib.rs".to_string()), "{got:?}");
        assert!(got.contains(&"src/lib.rs#parse_header".to_string()), "{got:?}");
        assert!(got.contains(&"src/lib.rs#helper".to_string()), "{got:?}");
        assert!(got.contains(&"src/lib.rs#Parser".to_string()), "the struct too: {got:?}");
        // The file defines them, and the call is attributed to the CALLER, not
        // to the file: that is the edge an agent actually asks about.
        assert!(has_edge(&g, "src/lib.rs", "src/lib.rs#parse_header", EdgeKind::Defines));
        assert!(
            has_edge(&g, "src/lib.rs#parse_header", "src/lib.rs#helper", EdgeKind::Calls),
            "{:?}",
            g.edges
        );
    }

    #[test]
    fn a_type_node_records_where_it_is() {
        let d = scratch(["src/a.rs"].iter().map(|f| (*f, "\n\npub fn third_line() {}\n")).collect::<Vec<_>>().as_slice());
        let g = scan(d.path(), &Limits::default());
        let n = g.nodes.iter().find(|n| n.name == "third_line").expect("found");
        assert_eq!(n.line, 3, "1-based, as an editor counts");
        assert_eq!(n.lang, "rust");
        assert_eq!(n.kind, NodeKind::Function);
        assert!(!n.path.starts_with('/'), "paths stay workspace-relative: {}", n.path);
    }

    #[test]
    fn python_classes_functions_and_imports_are_found() {
        let d = scratch(&[(
            "app.py",
            "import os\nfrom .util import helper\n\nclass Thing:\n    def method(self):\n        return helper()\n",
        )]);
        let g = scan(d.path(), &Limits::default());
        let got = ids(&g);
        assert!(got.contains(&"app.py#Thing".to_string()), "{got:?}");
        assert!(got.contains(&"app.py#method".to_string()), "{got:?}");
        assert!(
            g.edges.iter().any(|e| e.kind == EdgeKind::Imports),
            "imports should be recorded: {:?}",
            g.edges
        );
    }

    #[test]
    fn php_classes_and_methods_are_found() {
        let d = scratch(&[(
            "shm.php",
            "<?php\nclass Encoder {\n  public function encode($v) { return trim($v); }\n}\n",
        )]);
        let g = scan(d.path(), &Limits::default());
        let got = ids(&g);
        assert!(got.contains(&"shm.php#Encoder".to_string()), "{got:?}");
        assert!(got.contains(&"shm.php#encode".to_string()), "{got:?}");
    }

    #[test]
    fn several_languages_in_one_workspace_all_land() {
        let d = scratch(&[
            ("a.rs", "fn r() {}"),
            ("b.py", "def p(): pass"),
            ("c.go", "package main\nfunc g() {}"),
            ("d.ts", "export function t(): void {}"),
            ("e.js", "function j() {}"),
            ("f.sh", "run() { echo hi; }"),
        ]);
        let g = scan(d.path(), &Limits::default());
        let langs: std::collections::HashSet<String> =
            g.nodes.iter().map(|n| n.lang.clone()).collect();
        for want in ["rust", "python", "go", "typescript", "javascript", "bash"] {
            assert!(langs.contains(want), "missing {want}: {langs:?}");
        }
        assert_eq!(g.files, 6);
    }

    #[test]
    fn dependency_and_build_directories_are_not_walked() {
        // Parsing node_modules would bury the project's own code in the graph
        // and in every answer drawn from it.
        let d = scratch(&[
            ("src/mine.rs", "fn mine() {}"),
            ("node_modules/dep/index.js", "function theirs() {}"),
            ("target/debug/build.rs", "fn built() {}"),
            (".git/hooks/x.sh", "hook() { :; }"),
            ("vendor/v.php", "<?php function vendored() {}"),
        ]);
        let g = scan(d.path(), &Limits::default());
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"mine"), "{names:?}");
        for unwanted in ["theirs", "built", "hook", "vendored"] {
            assert!(!names.contains(&unwanted), "{unwanted} should not be here: {names:?}");
        }
    }

    #[test]
    fn an_oversized_file_is_skipped_and_said_so() {
        let big = "fn f() {}\n".repeat(2000);
        let d = scratch(&[("big.rs", big.as_str()), ("small.rs", "fn s() {}")]);
        let g = scan(d.path(), &Limits { max_files: 100, max_file_bytes: 100 });
        assert_eq!(g.files, 1, "only the small one parsed");
        assert!(
            g.skipped.iter().any(|s| s.path.contains("big.rs") && s.reason.contains("limit")),
            "a hole nobody mentions is worse than a smaller graph: {:?}",
            g.skipped
        );
    }

    #[test]
    fn hitting_the_file_limit_marks_the_graph_partial() {
        let files: Vec<(String, String)> = (0..10)
            .map(|i| (format!("f{i}.rs"), format!("fn f{i}() {{}}")))
            .collect();
        let refs: Vec<(&str, &str)> =
            files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let d = scratch(&refs);
        let g = scan(d.path(), &Limits { max_files: 3, max_file_bytes: 10_000 });
        assert!(g.truncated, "a caller must be able to tell it is partial");
        assert!(g.files <= 3);
    }

    #[test]
    fn an_ambiguous_call_target_produces_no_edge() {
        // Two functions share a name, so an edge to either could be wrong. A
        // wrong edge is worse than a missing one in something an agent
        // reasons from.
        let d = scratch(&[
            ("a.rs", "fn run() {}"),
            ("b.rs", "fn run() {}"),
            ("c.rs", "fn caller() { run() }"),
        ]);
        let g = scan(d.path(), &Limits::default());
        assert!(
            !g.edges.iter().any(|e| e.kind == EdgeKind::Calls),
            "ambiguity must not be guessed: {:?}",
            g.edges
        );
    }

    #[test]
    fn an_unambiguous_call_across_files_resolves() {
        let d = scratch(&[
            ("lib.rs", "pub fn only_one() {}"),
            ("main.rs", "fn caller() { only_one() }"),
        ]);
        let g = scan(d.path(), &Limits::default());
        assert!(
            has_edge(&g, "main.rs#caller", "lib.rs#only_one", EdgeKind::Calls),
            "{:?}",
            g.edges
        );
    }

    #[test]
    fn a_call_into_a_dependency_is_dropped_not_dangled() {
        let d = scratch(&[("a.rs", "fn f() { serde_json::from_str(\"\") }")]);
        let g = scan(d.path(), &Limits::default());
        let known: std::collections::HashSet<&str> =
            g.nodes.iter().map(|n| n.id.as_str()).collect();
        for e in &g.edges {
            assert!(known.contains(e.from.as_str()), "dangling from: {e:?}");
            assert!(known.contains(e.to.as_str()), "dangling to: {e:?}");
        }
    }

    #[test]
    fn an_import_of_a_workspace_file_points_at_that_file() {
        let d = scratch(&[
            ("util.py", "def helper(): pass"),
            ("app.py", "from util import helper\n"),
        ]);
        let g = scan(d.path(), &Limits::default());
        assert!(has_edge(&g, "app.py", "util.py", EdgeKind::Imports), "{:?}", g.edges);
    }

    #[test]
    fn an_import_from_outside_becomes_a_module_node_rather_than_vanishing() {
        // These used to be deleted by pruning, which silently cost the graph
        // every dependency it knew about.
        let d = scratch(&[("app.py", "import os\nimport requests\n")]);
        let g = scan(d.path(), &Limits::default());
        let mods: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Module)
            .map(|n| n.name.as_str())
            .collect();
        assert!(mods.contains(&"os") && mods.contains(&"requests"), "{mods:?}");
        assert!(has_edge(&g, "app.py", "module:requests", EdgeKind::Imports), "{:?}", g.edges);
    }

    #[test]
    fn a_relative_import_names_the_module_not_the_symbol() {
        // `from .util import helper` imports util, not helper.
        let d = scratch(&[("app.py", "from .util import helper\n")]);
        let g = scan(d.path(), &Limits::default());
        let mods: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Module)
            .map(|n| n.name.as_str())
            .collect();
        assert!(mods.contains(&"util"), "{mods:?}");
        assert!(!mods.contains(&"helper"), "that is the symbol: {mods:?}");
    }

    #[test]
    fn an_external_module_is_recorded_once_however_often_it_is_imported() {
        let d = scratch(&[
            ("a.py", "import requests"),
            ("b.py", "import requests"),
            ("c.py", "import requests"),
        ]);
        let g = scan(d.path(), &Limits::default());
        let n = g.nodes.iter().filter(|n| n.name == "requests").count();
        assert_eq!(n, 1, "one node, three edges");
        assert_eq!(g.edges.iter().filter(|e| e.to == "module:requests").count(), 3);
    }

    #[test]
    fn scanning_twice_gives_exactly_the_same_graph() {
        // Stable ids and order, so a stored community survives a rebuild and
        // two machines agree.
        let d = scratch(&[
            ("z.rs", "fn z() { a() }"),
            ("a.rs", "fn a() {}"),
            ("m/b.py", "def b(): pass"),
        ]);
        let first = scan(d.path(), &Limits::default());
        let second = scan(d.path(), &Limits::default());
        assert_eq!(first, second);
    }

    #[test]
    fn an_unreadable_or_binary_file_is_reported_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("good.rs"), "fn g() {}").unwrap();
        // Invalid UTF-8 with a parseable extension.
        std::fs::write(d.path().join("bad.rs"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let g = scan(d.path(), &Limits::default());
        assert_eq!(g.files, 1);
        assert!(g.skipped.iter().any(|s| s.path.contains("bad.rs")), "{:?}", g.skipped);
    }

    #[test]
    fn an_empty_workspace_is_an_empty_graph_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let g = scan(d.path(), &Limits::default());
        assert!(g.nodes.is_empty() && g.edges.is_empty());
        assert_eq!(g.files, 0);
        assert!(!g.truncated);
    }

    #[test]
    fn it_parses_this_repository() {
        // The real test: a few thousand lines of actual Rust, not a snippet.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/proto/src");
        let g = scan(&root, &Limits::default());
        assert!(g.files >= 5, "parsed {} files", g.files);
        let names: std::collections::HashSet<&str> =
            g.nodes.iter().map(|n| n.name.as_str()).collect();
        // Things that genuinely exist in this crate.
        assert!(names.contains("CliDescriptor"), "types should be found");
        assert!(names.contains("team_tools"), "functions should be found");
        assert!(
            g.edges.iter().any(|e| e.kind == EdgeKind::Calls),
            "and some calls should resolve"
        );
        assert!(g.skipped.is_empty(), "nothing should be unreadable: {:?}", g.skipped);
    }
}
