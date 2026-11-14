//! The code graph, drawn in braille.
//!
//! The dashboard's map answers the question a code graph exists for: not
//! "what is in this repository" but "what is it SHAPED like". Thirty thousand
//! symbols drawn at once is a hairball; twenty subsystems with the
//! dependencies between them is a map. The same aggregation is what the hub
//! already computes for the browser, so this draws the same picture with
//! different ink.
//!
//! The layout is a circle, deliberately. A force simulation in a terminal
//! would be prettier for one graph and unreadable for the next, and worse,
//! it would move: a map you are pointing at should be in the same place the
//! next time you look.

use crate::tui::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Borders};
use serde_json::Value;

/// How wide one character is in canvas units, for a canvas 2.0 units across
/// on a terminal around 160 columns wide. Only used to make a label end where
/// it should; being a little out simply shifts it a character.
const CHAR_W: f64 = 0.0125;

/// One subsystem, placed.
pub struct Node {
    pub id: i64,
    pub label: String,
    pub n: i64,
    pub x: f64,
    pub y: f64,
}

/// Lay the subsystems of one workspace out on a circle, largest first so the
/// big ones land in a predictable order rather than wherever the query
/// happened to return them.
pub fn layout(graph: &Value) -> Vec<Node> {
    let subs = graph["subsystems"].as_array().cloned().unwrap_or_default();
    let n = subs.len().max(1);
    subs.iter()
        .enumerate()
        .map(|(i, s)| {
            // Start at the top and go clockwise: the first subsystem is the
            // biggest, and that is where an eye starts.
            let t = std::f64::consts::TAU * (i as f64) / (n as f64) - std::f64::consts::FRAC_PI_2;
            // Room is left outside the ring for the labels, which sit beside
            // their blob rather than on it: a label drawn at the node covers
            // the node, and twenty of them cover each other.
            let r = if n <= 1 { 0.0 } else { 0.60 };
            Node {
                id: s["id"].as_i64().unwrap_or(0),
                label: label_of(s),
                n: s["n"].as_i64().unwrap_or(0),
                x: r * t.cos(),
                y: r * t.sin() * 0.78, // terminals are wider than they are tall
            }
        })
        .collect()
}

/// What to call a subsystem nobody named: the file most of it lives in. The
/// hub works this out when it stores the graph; this only shortens it.
fn label_of(s: &Value) -> String {
    let path = s["path"].as_str().unwrap_or("");
    if !path.is_empty() {
        let base = path.rsplit('/').next().unwrap_or(path);
        // The extension is the same on every label in a Python tree and
        // costs three characters of a line that has none to spare.
        let base = base
            .strip_suffix(".py")
            .or_else(|| base.strip_suffix(".rs"))
            .or_else(|| base.strip_suffix(".ts"))
            .or_else(|| base.strip_suffix(".js"))
            .or_else(|| base.strip_suffix(".go"))
            .unwrap_or(base);
        let mut out = base.to_string();
        if out.chars().count() > 15 {
            out = out.chars().take(14).collect::<String>() + "\u{2026}";
        }
        return out;
    }
    s["members"]
        .as_array()
        .and_then(|m| m.first())
        .and_then(|m| m["name"].as_str())
        .unwrap_or("—")
        .to_string()
}

/// Draw it. `selected` is an index into the laid-out nodes, not a community
/// id, because that is what the cursor moves through.
pub fn canvas<'a>(
    graph: &'a Value,
    nodes: &'a [Node],
    selected: usize,
    th: Theme,
    title: String,
) -> impl ratatui::widgets::Widget + 'a {
    let links = graph["links"].as_array().cloned().unwrap_or_default();
    let by_id: std::collections::HashMap<i64, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
    let biggest = nodes.iter().map(|n| n.n).max().unwrap_or(1).max(1);
    let sel_id = nodes.get(selected).map(|n| n.id).unwrap_or(-1);
    // Keep roughly the heaviest forty edges, whatever the absolute numbers
    // happen to be in this workspace.
    let floor = {
        let mut w: Vec<i64> = links.iter().filter_map(|l| l["n"].as_i64()).collect();
        w.sort_unstable_by(|a, b| b.cmp(a));
        w.get(40).copied().unwrap_or(0)
    };
    // Only the big ones are named, and whatever the cursor is on. Every
    // label drawn is a label over something else.
    let named: std::collections::HashSet<i64> = {
        let mut by_size: Vec<(i64, i64)> = nodes.iter().map(|n| (n.n, n.id)).collect();
        by_size.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        by_size.into_iter().take(12).map(|(_, id)| id).collect()
    };

    Canvas::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(th.line()))
                .title(Span::styled(title, th.title())),
        )
        .background_color(th.bg())
        .marker(Marker::Braille)
        .x_bounds([-1.0, 1.0])
        .y_bounds([-1.0, 1.0])
        .paint(move |ctx| {
            // Edges first, so a label is never drawn under a line -- and not
            // all of them. A real workspace has a dependency between almost
            // every pair of subsystems, and drawing three hundred lines on a
            // braille canvas produces a solid grey disc that says nothing.
            // The heaviest few, plus everything touching the cursor: that is
            // the shape, and the rest is texture.
            for l in &links {
                let keep = l["n"].as_i64().unwrap_or(0) >= floor
                    || l["a"].as_i64() == Some(sel_id)
                    || l["b"].as_i64() == Some(sel_id);
                if !keep {
                    continue;
                }
                let (Some(a), Some(b)) = (l["a"].as_i64(), l["b"].as_i64()) else { continue };
                let (Some(&i), Some(&j)) = (by_id.get(&a), by_id.get(&b)) else { continue };
                if i == j {
                    continue;
                }
                // `defines` and `imports` are read off the tree; a call
                // resolved by name is a guess, and the drawing says which is
                // which -- the same distinction the browser map makes.
                let certain = l["certain"].as_bool().unwrap_or(false);
                let touches_sel = a == sel_id || b == sel_id;
                let colour = if touches_sel {
                    th.second()
                } else if certain {
                    th.line()
                } else {
                    th.faint()
                };
                ctx.draw(&CanvasLine {
                    x1: nodes[i].x,
                    y1: nodes[i].y,
                    x2: nodes[j].x,
                    y2: nodes[j].y,
                    color: colour,
                });
            }
            ctx.layer();
            for (i, nd) in nodes.iter().enumerate() {
                let share = (nd.n as f64 / biggest as f64).sqrt();
                // A blob whose size says how much is in it. Three sizes is
                // all a braille canvas can honestly distinguish.
                let glyph = if share > 0.66 {
                    "███"
                } else if share > 0.33 {
                    "██"
                } else {
                    "▪"
                };
                let on = i == selected;
                let style = if on {
                    Style::default().fg(th.accent()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(th.dim())
                };
                ctx.print(nd.x, nd.y, TextLine::from(Span::styled(glyph, style)));
                if !on && !named.contains(&nd.id) {
                    continue;
                }
                // Beside the blob, on the outside of the ring: a label to the
                // left of a left-hand node has to START far enough left to
                // END at the node, since a canvas prints rightwards from
                // where it is told.
                let tag = format!("{} {}", nd.label, nd.n);
                let chars = tag.chars().count() as f64;
                let lx = if nd.x < -0.08 {
                    nd.x - 0.04 - chars * CHAR_W
                } else if nd.x > 0.08 {
                    nd.x + 0.04
                } else {
                    nd.x - chars * CHAR_W / 2.0
                };
                // Staggered by position in the ring. Neighbours at the top
                // and bottom of a circle are close in x, so without this
                // their labels sit on the same row and overwrite each other.
                let step = if i % 2 == 0 { 0.055 } else { 0.115 };
                let ly = if nd.y.abs() < 0.08 {
                    nd.y
                } else if nd.y > 0.0 {
                    nd.y + step
                } else {
                    nd.y - step
                };
                ctx.print(
                    lx,
                    ly,
                    TextLine::from(Span::styled(
                        tag,
                        if on {
                            Style::default().fg(th.fg()).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(th.faint())
                        },
                    )),
                );
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn g(n: usize) -> Value {
        json!({
            "subsystems": (0..n).map(|i| json!({
                "id": i as i64, "n": (100 - i) as i64,
                "path": format!("src/very/deep/module_number_{i}.rs"),
                "members": [{ "name": format!("Thing{i}"), "kind": "type" }],
            })).collect::<Vec<_>>(),
            "links": [],
        })
    }

    #[test]
    fn everything_lands_inside_the_canvas() {
        // Bounds are [-1, 1]. A node outside them is simply not drawn, which
        // looks like a missing subsystem rather than a layout bug.
        for n in [1usize, 2, 7, 24] {
            for nd in layout(&g(n)) {
                assert!(nd.x.abs() <= 1.0 && nd.y.abs() <= 1.0, "{} {}", nd.x, nd.y);
            }
        }
    }

    #[test]
    fn one_subsystem_sits_in_the_middle() {
        let nodes = layout(&g(1));
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].x.abs() < 1e-9 && nodes[0].y.abs() < 1e-9);
    }

    #[test]
    fn the_layout_does_not_move_between_redraws() {
        // A map you are pointing at must be in the same place next time you
        // look, which is why this is a circle and not a simulation.
        let a = layout(&g(9));
        let b = layout(&g(9));
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.x.to_bits(), y.x.to_bits());
            assert_eq!(x.y.to_bits(), y.y.to_bits());
        }
    }

    #[test]
    fn a_label_is_short_enough_to_draw() {
        // A path is long and has no spaces in it; a label the width of the
        // canvas covers everything else on it.
        for nd in layout(&g(5)) {
            assert!(nd.label.chars().count() <= 15, "{}", nd.label);
            assert!(!nd.label.contains('/'), "{}", nd.label);
            assert!(!nd.label.ends_with(".rs"), "{}", nd.label);
        }
    }

    #[test]
    fn a_community_of_external_imports_is_not_labelled_blank() {
        // An imported module is a node with no file of its own, and a
        // subsystem full of them would otherwise be labelled "".
        let v = json!({
            "subsystems": [{ "id": 1, "n": 4, "path": "",
                             "members": [{ "name": "serde", "kind": "module" }] }],
            "links": [],
        });
        assert_eq!(layout(&v)[0].label, "serde");
    }
}
