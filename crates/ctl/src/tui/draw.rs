//! Every pane, and the chrome around them.
//!
//! One rule throughout: a number on this screen must mean the same thing it
//! means on the dashboard, because they are the same numbers from the same
//! snapshot. Where this shows less, it shows less of the same -- never
//! something computed differently.

use crate::tui::map;
use crate::tui::theme::Theme;
use crate::tui::{App, Pane};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use serde_json::Value;

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn n(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// Token counts get large; 12400 reads better as 12.4k.
fn human(x: i64) -> String {
    if x >= 1_000_000 {
        format!("{:.1}M", x as f64 / 1e6)
    } else if x >= 1_000 {
        format!("{:.1}k", x as f64 / 1e3)
    } else {
        x.to_string()
    }
}

fn hhmmss(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

fn ago(now: i64, then: i64) -> String {
    let d = ((now - then) / 1000).max(0);
    if d < 60 {
        format!("{d}s")
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86400 {
        format!("{}h", d / 3600)
    } else {
        format!("{}d", d / 86400)
    }
}

fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn framed<'a>(title: String, th: Theme, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { th.accent() } else { th.line() }))
        .title(Span::styled(title, th.title()))
        .style(Style::default().bg(th.panel()))
}

pub fn all(f: &mut Frame, app: &App) {
    let th = app.theme;
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(th.bg()).fg(th.fg())),
        area,
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // the bar
            Constraint::Length(1), // panes
            Constraint::Min(3),    // the work
            Constraint::Length(1), // keys and complaints
        ])
        .split(area);

    bar(f, rows[0], app);
    panes(f, rows[1], app);

    let body = if app.panel && rows[2].width > 92 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(38)])
            .split(rows[2])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10)])
            .split(rows[2])
    };

    match app.pane {
        Pane::Deck => deck(f, body[0], app),
        Pane::Agents => agents(f, body[0], app),
        Pane::Chat => chat(f, body[0], app),
        Pane::Plan => plan(f, body[0], app),
        Pane::Memory => memory(f, body[0], app),
        Pane::Graph => graph(f, body[0], app),
        Pane::Files => files(f, body[0], app),
        Pane::Fleet => fleet(f, body[0], app),
    }
    if body.len() > 1 {
        side(f, body[1], app);
    }
    footer(f, rows[3], app);
    if app.help {
        help(f, area, app);
    }
}

fn bar(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let hub = app
        .snap
        .as_ref()
        .and_then(|s| s.get("hub"))
        .cloned()
        .unwrap_or(Value::Null);
    let scope = if app.scope == "*" {
        "every session".to_string()
    } else {
        app.sessions()
            .iter()
            .find(|x| s(x, "id") == app.scope)
            .map(|x| s(x, "name"))
            .unwrap_or_else(|| app.scope.clone())
    };
    let live = app.err.is_none();
    let mut spans = vec![
        Span::styled(" nex", Style::default().fg(th.fg()).add_modifier(Modifier::BOLD)),
        Span::styled("mesh ", Style::default().fg(th.accent()).add_modifier(Modifier::BOLD)),
        Span::styled(
            th.label(" console "),
            Style::default().fg(th.bg()).bg(th.second()).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(th.label("scope "), Style::default().fg(th.faint())),
        Span::styled(clip(&scope, 24), Style::default().fg(th.fg())),
    ];
    if let Some(v) = hub.get("version").and_then(|v| v.as_str()) {
        spans.push(Span::styled(format!("   hub {v}"), Style::default().fg(th.faint())));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        if live { "\u{25cf} live" } else { "\u{25cf} stale" },
        Style::default().fg(if live { th.ok() } else { th.bad() }),
    ));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(th.panel())),
        area,
    );
}

fn panes(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let mut spans = vec![Span::raw(" ")];
    for (i, p) in Pane::ALL.iter().enumerate() {
        let on = *p == app.pane;
        let label = format!(" {}·{} ", i + 1, th.label(p.name()));
        spans.push(Span::styled(
            label,
            if on {
                th.selected()
            } else {
                Style::default().fg(th.dim())
            },
        ));
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The deck: what you want to know before you know what you are looking for.
fn deck(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let agents = app.rows("agents");
    let msgs = app.rows("messages");
    let nodes = app.nodes();
    let running = agents.iter().filter(|a| s(a, "status") == "running").count();
    let adrift = agents
        .iter()
        .filter(|a| s(a, "status") != "running" && s(a, "kind") != "external")
        .count();
    let unclaimed = msgs.iter().filter(|m| !b(m, "claimed")).count();
    let undelivered = msgs.iter().filter(|m| !b(m, "delivered")).count();
    let online = nodes.iter().filter(|x| b(x, "connected")).count();
    let tin: i64 = agents.iter().map(|a| n(a, "input_tokens")).sum();
    let tout: i64 = agents.iter().map(|a| n(a, "output_tokens")).sum();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(area);

    // Tiles.
    let tiles = [
        (format!("{}", app.sessions().len()), "sessions", false),
        (format!("{running}"), "running", false),
        (format!("{adrift}"), "no process", adrift > 0),
        (format!("{unclaimed}"), "unclaimed", unclaimed > 0),
        (format!("{undelivered}"), "undelivered", undelivered > 0),
        (format!("{online}/{}", nodes.len()), "machines", online < nodes.len()),
        (format!("{}/{}", human(tin), human(tout)), "tokens in/out", false),
    ];
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, tiles.len() as u32); tiles.len()])
        .split(rows[0]);
    for (i, (v, k, alert)) in tiles.iter().enumerate() {
        let (ns, ks) = th.tile(*alert);
        let body = Text::from(vec![
            Line::from(Span::styled(format!(" {v}"), ns)),
            Line::from(Span::styled(format!(" {}", th.label(k)), ks)),
        ]);
        f.render_widget(
            Paragraph::new(body).block(framed(String::new(), th, false)),
            cells[i],
        );
    }

    // What just happened, and who is adrift -- the two things a glance is for.
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[1]);

    let w = halves[0].width.saturating_sub(4) as usize;
    let lines: Vec<Line> = msgs
        .iter()
        .take(halves[0].height.saturating_sub(2) as usize)
        .map(|m| {
            let from = s(m, "from");
            Line::from(vec![
                Span::styled(hhmmss(n(m, "at_ms")), Style::default().fg(th.faint())),
                Span::raw(" "),
                Span::styled(
                    format!("{:>12}", clip(&from, 12)),
                    Style::default().fg(th.nick(&from)),
                ),
                Span::styled(" \u{2192} ", Style::default().fg(th.faint())),
                Span::styled(
                    clip(&s(m, "body"), w.saturating_sub(28)),
                    Style::default().fg(th.fg()),
                ),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(framed(th.label("latest"), th, false)),
        halves[0],
    );

    let mut right: Vec<Line> = Vec::new();
    for a in agents.iter().filter(|a| s(a, "status") != "running") {
        right.push(Line::from(vec![
            Span::styled(
                format!("{:<16}", clip(&s(a, "name"), 16)),
                Style::default().fg(th.nick(&s(a, "name"))),
            ),
            Span::styled(
                format!("{:<13}", s(a, "status")),
                Style::default().fg(th.warn()),
            ),
            Span::styled(s(a, "host"), Style::default().fg(th.faint())),
        ]));
    }
    if right.is_empty() {
        right.push(Line::from(Span::styled(
            " every seat has a process behind it",
            Style::default().fg(th.ok()),
        )));
    }
    f.render_widget(
        Paragraph::new(Text::from(right)).block(framed(th.label("needs a look"), th, false)),
        halves[1],
    );
}

fn table<'a>(
    f: &mut Frame,
    area: Rect,
    app: &App,
    title: String,
    head: Vec<&'a str>,
    widths: Vec<Constraint>,
    rows: Vec<Vec<String>>,
) {
    let th = app.theme;
    let cur = app.cursor();
    let header = Row::new(head.into_iter().map(|h| th.label(h)).collect::<Vec<_>>())
        .style(th.header());
    // The cursor is drawn on the row rather than with a highlight state, so
    // the same table renders identically whether or not it has focus.
    let body: Vec<Row> = rows
        .into_iter()
        .enumerate()
        .map(|(i, cells)| {
            let style = if i == cur {
                th.selected()
            } else {
                Style::default().fg(th.fg())
            };
            Row::new(cells).style(style)
        })
        .collect();
    let t = Table::new(body, widths)
        .header(header)
        .block(framed(title, th, true))
        .column_spacing(1);
    f.render_widget(t, area);
}

fn agents(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Vec<String>> = app
        .rows("agents")
        .iter()
        .map(|a| {
            let tokens = if n(a, "input_tokens") + n(a, "output_tokens") == 0 {
                "-".to_string()
            } else {
                format!("{}/{}", human(n(a, "input_tokens")), human(n(a, "output_tokens")))
            };
            vec![
                s(a, "name"),
                s(a, "slot_id"),
                s(a, "cli"),
                s(a, "role"),
                s(a, "host"),
                s(a, "task_state"),
                s(a, "status"),
                match a.get("pid").and_then(|p| p.as_i64()) {
                    Some(p) => p.to_string(),
                    None => "-".into(),
                },
                a.get("io_kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("-")
                    .to_string(),
                tokens,
            ]
        })
        .collect();
    table(
        f,
        area,
        app,
        format!("{} ({})", app.theme.label("agents"), rows.len()),
        vec!["name", "seat", "cli", "role", "where", "state", "agent", "pid", "io", "tokens"],
        vec![
            Constraint::Length(16), Constraint::Length(13), Constraint::Length(10),
            Constraint::Length(11), Constraint::Length(9), Constraint::Length(11),
            Constraint::Length(13), Constraint::Length(7), Constraint::Length(6),
            Constraint::Min(9),
        ],
        rows,
    );
}

/// IRC, because IRC got this right: channels, a room, and one line each.
fn chat(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(20), Constraint::Min(20)])
        .split(area);

    let chans = app.channels();
    let unread = |id: &str| -> usize {
        app.rows("messages")
            .iter()
            .filter(|m| {
                !b(m, "claimed")
                    && (id == "*" || {
                        let name = s(m, "session");
                        app.sessions()
                            .iter()
                            .any(|x| s(x, "id") == id && s(x, "name") == name)
                    })
            })
            .count()
    };
    let items: Vec<Line> = chans
        .iter()
        .enumerate()
        .map(|(i, (id, label))| {
            let on = *id == app.chan;
            let cursor = i == app.cursor();
            let u = unread(id);
            let mut st = if on {
                Style::default().fg(th.accent()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.dim())
            };
            if cursor {
                st = th.selected();
            }
            Line::from(vec![
                Span::styled(format!(" {:<13}", clip(label, 13)), st),
                Span::styled(
                    if u > 0 { format!("{u:>3}") } else { "   ".into() },
                    Style::default().fg(th.warn()),
                ),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(Text::from(items)).block(framed(th.label("channels"), th, true)),
        cols[0],
    );

    // The room itself, oldest first like a conversation.
    let here: Vec<Value> = app
        .rows("messages")
        .into_iter()
        .filter(|m| {
            app.chan == "*" || {
                let name = s(m, "session");
                app.sessions()
                    .iter()
                    .any(|x| s(x, "id") == app.chan && s(x, "name") == name)
            }
        })
        .collect();
    let inner = cols[1].height.saturating_sub(2) as usize;
    let w = cols[1].width.saturating_sub(4) as usize;
    let lines: Vec<Line> = here
        .iter()
        .rev()
        .take(inner)
        .rev()
        .map(|m| {
            let from = s(m, "from");
            let mut spans = vec![
                Span::styled(hhmmss(n(m, "at_ms")), Style::default().fg(th.faint())),
                Span::styled(
                    if b(m, "claimed") { "  " } else { " \u{25cf}" },
                    Style::default().fg(th.warn()),
                ),
                Span::raw(" "),
            ];
            if app.chan == "*" && !s(m, "session").is_empty() {
                spans.push(Span::styled(
                    format!("#{} ", s(m, "session")),
                    Style::default().fg(th.accent()),
                ));
            }
            spans.push(Span::styled(
                format!("<{}>", from),
                Style::default().fg(th.nick(&from)).add_modifier(Modifier::BOLD),
            ));
            let to = s(m, "to");
            if !to.is_empty() && to != "(team)" {
                spans.push(Span::styled(
                    format!(" \u{2192}{to}"),
                    Style::default().fg(th.dim()),
                ));
            }
            spans.push(Span::raw(" "));
            // A message carrying a link says so, because the terminal
            // cannot show what is behind it and `o` can.
            let body = s(m, "body");
            if crate::tui::first_link(&body).is_some() {
                spans.push(Span::styled("\u{1F517} ", Style::default().fg(th.second())));
            }
            spans.push(Span::styled(
                clip(&body, w.saturating_sub(30)),
                Style::default().fg(th.fg()),
            ));
            Line::from(spans)
        })
        .collect();

    let name = chans
        .iter()
        .find(|(id, _)| *id == app.chan)
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| "#all".into());
    let title = if app.editing {
        format!("{name}  \u{2014}  typing, Esc to stop")
    } else {
        format!("{name}  \u{2014}  i to type")
    };
    let feed = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[1]);
    f.render_widget(
        Paragraph::new(Text::from(lines)).block(framed(title, th, true)),
        feed[0],
    );
    let prompt = if app.editing {
        format!(" > {}\u{2588}", app.input)
    } else {
        format!(" > {}", app.input)
    };
    f.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(th.fg()))
            .block(framed(th.label("say"), th, app.editing)),
        feed[1],
    );
}

fn plan(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Vec<String>> = app
        .rows("plan")
        .iter()
        .map(|p| {
            vec![
                n(p, "ord").to_string(),
                s(p, "label"),
                s(p, "status"),
                s(p, "owner"),
                s(p, "note"),
            ]
        })
        .collect();
    table(
        f,
        area,
        app,
        format!("{} ({})", app.theme.label("plan"), rows.len()),
        vec!["#", "step", "status", "owner", "note"],
        vec![
            Constraint::Length(4), Constraint::Percentage(40), Constraint::Length(14),
            Constraint::Length(14), Constraint::Min(10),
        ],
        rows,
    );
}

fn memory(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Vec<String>> = app
        .rows("knowledge")
        .iter()
        .map(|k| {
            vec![
                s(k, "key"),
                s(k, "category"),
                s(k, "scope"),
                s(k, "author"),
                clip(&s(k, "value").replace('\n', " "), 120),
            ]
        })
        .collect();
    table(
        f,
        area,
        app,
        format!("{} ({})", app.theme.label("memory"), rows.len()),
        vec!["key", "kind", "scope", "by", "value"],
        vec![
            Constraint::Length(22), Constraint::Length(12), Constraint::Length(10),
            Constraint::Length(14), Constraint::Min(20),
        ],
        rows,
    );
}

/// The code graph. The map is the point; the table under it is the index.
fn graph(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let graphs = app.rows("graph");
    if graphs.is_empty() {
        f.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No workspace has been parsed yet.",
                    Style::default().fg(th.dim()),
                )),
                Line::from(Span::styled(
                    "  Ask an agent for a code graph, or run: nexmesh graph <session> <workspace>",
                    Style::default().fg(th.faint()),
                )),
            ]))
            .block(framed(th.label("graph"), th, true)),
            area,
        );
        return;
    }
    let g = &graphs[app.ws.min(graphs.len() - 1)];
    let nodes = map::layout(g);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Min(4)])
        .split(area);

    let truncated = match (b(g, "truncated"), s(g, "truncated_reason")) {
        (false, _) => String::new(),
        // An older graph was stored before the hub recorded WHICH limit cut
        // the scan short, so "partial" on its own is all there is to say.
        (true, r) if r.is_empty() => "  \u{2014}  partial".to_string(),
        (true, r) => format!("  \u{2014}  partial: {r}"),
    };
    let title = format!(
        "{}  {} files, {} subsystems{}  \u{2014}  w: next workspace ({}/{})",
        s(g, "workspace"),
        n(g, "files"),
        n(g, "communities"),
        truncated,
        app.ws.min(graphs.len() - 1) + 1,
        graphs.len()
    );
    f.render_widget(
        map::canvas(g, &nodes, app.cursor(), th, title),
        rows[0],
    );

    let table_rows: Vec<Vec<String>> = g["subsystems"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|x| {
            let members = x["members"]
                .as_array()
                .map(|m| {
                    m.iter()
                        .filter_map(|y| y["name"].as_str())
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            vec![
                n(x, "n").to_string(),
                x["path"].as_str().unwrap_or("\u{2014}").to_string(),
                members,
            ]
        })
        .collect();
    table(
        f,
        rows[1],
        app,
        app.theme.label("subsystems"),
        vec!["size", "mostly in", "for example"],
        vec![Constraint::Length(6), Constraint::Percentage(45), Constraint::Min(20)],
        table_rows,
    );
}

fn files(f: &mut Frame, area: Rect, app: &App) {
    let mut rows: Vec<Vec<String>> = app
        .rows("locks")
        .iter()
        .map(|l| {
            vec![
                "lock".to_string(),
                s(l, "path"),
                s(l, "holder"),
                s(l, "mode"),
                s(l, "note"),
            ]
        })
        .collect();
    for z in app.rows("zones") {
        rows.push(vec![
            "zone".to_string(),
            s(&z, "glob"),
            s(&z, "owner"),
            String::new(),
            s(&z, "note"),
        ]);
    }
    table(
        f,
        area,
        app,
        format!("{} ({})", app.theme.label("files"), rows.len()),
        vec!["what", "path", "who", "mode", "note"],
        vec![
            Constraint::Length(6), Constraint::Percentage(40), Constraint::Length(16),
            Constraint::Length(8), Constraint::Min(10),
        ],
        rows,
    );
}

fn fleet(f: &mut Frame, area: Rect, app: &App) {
    let now = app
        .snap
        .as_ref()
        .and_then(|x| x["hub"]["now_ms"].as_i64())
        .unwrap_or(0);
    let rows: Vec<Vec<String>> = app
        .nodes()
        .iter()
        .map(|x| {
            let clis = x["clis"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            vec![
                s(x, "name"),
                if b(x, "connected") { "online".into() } else { "offline".into() },
                ago(now, n(x, "last_seen_ms")),
                clis,
            ]
        })
        .collect();
    table(
        f,
        area,
        app,
        format!("{} ({})", app.theme.label("fleet"), rows.len()),
        vec!["machine", "state", "seen", "what it has"],
        vec![
            Constraint::Length(16), Constraint::Length(9), Constraint::Length(7),
            Constraint::Min(20),
        ],
        rows,
    );
}

/// The right panel: whatever the cursor is on, in full.
fn side(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let mut out: Vec<Line> = Vec::new();
    let kv = |k: &str, v: String| -> Line {
        Line::from(vec![
            Span::styled(format!(" {:<11}", k), Style::default().fg(th.faint())),
            Span::styled(v, Style::default().fg(th.fg())),
        ])
    };
    match app.pane {
        Pane::Agents => {
            let rows = app.rows("agents");
            if let Some(a) = rows.get(app.cursor()) {
                out.push(Line::from(Span::styled(
                    format!(" {}", s(a, "name")),
                    Style::default().fg(th.nick(&s(a, "name"))).add_modifier(Modifier::BOLD),
                )));
                out.push(Line::from(""));
                out.push(kv("seat", s(a, "slot_id")));
                out.push(kv("session", s(a, "session")));
                out.push(kv("role", s(a, "role")));
                out.push(kv("cli", format!("{} · {}", s(a, "cli"), s(a, "mode"))));
                out.push(kv("machine", s(a, "host")));
                out.push(kv("kind", s(a, "provenance")));
                out.push(kv("work", s(a, "task_state")));
                out.push(kv("process", s(a, "status")));
                out.push(kv(
                    "io",
                    format!(
                        "{} {}",
                        a.get("pid").and_then(|p| p.as_i64()).map(|p| p.to_string()).unwrap_or("-".into()),
                        a.get("io_kind").and_then(|x| x.as_str()).unwrap_or("-")
                    ),
                ));
                if b(a, "push_capable") {
                    out.push(kv("delivery", "pushed mid-turn".into()));
                }
                out.push(kv(
                    "tokens",
                    format!(
                        "{} in · {} out",
                        human(n(a, "input_tokens")),
                        human(n(a, "output_tokens"))
                    ),
                ));
                let summary = s(a, "summary");
                if !summary.is_empty() {
                    out.push(Line::from(""));
                    out.push(Line::from(Span::styled(
                        " says",
                        Style::default().fg(th.faint()),
                    )));
                    for l in textwrap(&summary, area.width.saturating_sub(3) as usize) {
                        out.push(Line::from(Span::styled(
                            format!(" {l}"),
                            Style::default().fg(th.dim()),
                        )));
                    }
                }
                out.push(Line::from(""));
                out.push(Line::from(Span::styled(
                    " Enter or a: attach to this one",
                    Style::default().fg(th.second()),
                )));
            }
        }
        Pane::Graph => {
            let graphs = app.rows("graph");
            if let Some(g) = graphs.get(app.ws.min(graphs.len().saturating_sub(1))) {
                let subs = g["subsystems"].as_array().cloned().unwrap_or_default();
                if let Some(x) = subs.get(app.cursor()) {
                    out.push(Line::from(Span::styled(
                        format!(" subsystem {}", n(x, "id")),
                        th.title(),
                    )));
                    out.push(Line::from(""));
                    out.push(kv("symbols", n(x, "n").to_string()));
                    out.push(kv("mostly in", x["path"].as_str().unwrap_or("—").to_string()));
                    out.push(Line::from(""));
                    out.push(Line::from(Span::styled(
                        " members",
                        Style::default().fg(th.faint()),
                    )));
                    for m in x["members"].as_array().cloned().unwrap_or_default() {
                        out.push(Line::from(vec![
                            Span::styled(
                                format!(" {:<6}", s(&m, "kind")),
                                Style::default().fg(th.faint()),
                            ),
                            Span::styled(s(&m, "name"), Style::default().fg(th.fg())),
                        ]));
                    }
                }
                out.push(Line::from(""));
                for c in g["counts"].as_array().cloned().unwrap_or_default() {
                    out.push(kv(
                        s(&c, "kind").as_str(),
                        n(&c, "n").to_string(),
                    ));
                }
            }
        }
        Pane::Chat => {
            out.push(Line::from(Span::styled(" in the room", th.title())));
            out.push(Line::from(""));
            for a in app.rows("agents") {
                let name = s(&a, "name");
                out.push(Line::from(vec![
                    Span::styled(
                        if s(&a, "status") == "running" { " \u{25cf} " } else { " \u{25cb} " },
                        Style::default().fg(if s(&a, "status") == "running" {
                            th.ok()
                        } else {
                            th.faint()
                        }),
                    ),
                    Span::styled(clip(&name, 18), Style::default().fg(th.nick(&name))),
                    Span::styled(
                        format!(" {}", clip(&s(&a, "role"), 12)),
                        Style::default().fg(th.faint()),
                    ),
                ]));
            }
        }
        _ => {
            out.push(Line::from(Span::styled(" fleet", th.title())));
            out.push(Line::from(""));
            for x in app.nodes() {
                out.push(Line::from(vec![
                    Span::styled(
                        if b(&x, "connected") { " \u{25cf} " } else { " \u{25cb} " },
                        Style::default().fg(if b(&x, "connected") { th.ok() } else { th.bad() }),
                    ),
                    Span::styled(s(&x, "name"), Style::default().fg(th.fg())),
                ]));
            }
            out.push(Line::from(""));
            for s2 in app.sessions() {
                out.push(Line::from(vec![
                    Span::styled(" #", Style::default().fg(th.accent())),
                    Span::styled(s(&s2, "name"), Style::default().fg(th.dim())),
                    Span::styled(
                        format!("  {} seats", n(&s2, "slots")),
                        Style::default().fg(th.faint()),
                    ),
                ]));
            }
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(out))
            .wrap(Wrap { trim: false })
            .block(framed(th.label("detail"), th, false)),
        area,
    );
}

/// Wrap on spaces. `textwrap` would be a dependency for eleven lines.
fn textwrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        // `!line.is_empty()`: a word wider than the panel -- a path, usually
        // -- would otherwise push a blank line out ahead of itself and then
        // run long anyway.
        if !line.is_empty() && line.chars().count() + word.chars().count() + 1 > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    if let Some(e) = &app.err {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" hub: ", Style::default().fg(th.bad()).add_modifier(Modifier::BOLD)),
                Span::styled(e.clone(), Style::default().fg(th.bad())),
            ]))
            .style(Style::default().bg(th.panel())),
            area,
        );
        return;
    }
    if let Some((msg, at)) = &app.note {
        if at.elapsed() < std::time::Duration::from_secs(6) {
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {msg}"),
                    Style::default().fg(th.second()),
                ))
                .style(Style::default().bg(th.panel())),
                area,
            );
            return;
        }
    }
    let keys = match app.pane {
        Pane::Chat => " 1-8 panes · [ ] channel · i type · o open a link · p panel · t theme · ? help · q quit",
        Pane::Graph => " 1-8 panes · j/k subsystem · w workspace · p panel · t theme · ? help · q quit",
        Pane::Agents => " 1-8 panes · j/k move · a attach · s scope · p panel · t theme · ? help · q quit",
        _ => " 1-8 panes · j/k move · s scope · p panel · t theme · r refresh · ? help · q quit",
    };
    f.render_widget(
        Paragraph::new(Span::styled(keys, Style::default().fg(th.faint())))
            .style(Style::default().bg(th.panel())),
        area,
    );
}

fn help(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let w = 64u16.min(area.width.saturating_sub(4));
    let h = 20u16.min(area.height.saturating_sub(2));
    let box_ = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let row = |k: &str, v: &str| -> Line {
        Line::from(vec![
            Span::styled(format!("  {:<12}", k), Style::default().fg(th.accent())),
            Span::styled(v.to_string(), Style::default().fg(th.fg())),
        ])
    };
    let text = Text::from(vec![
        Line::from(""),
        row("1 \u{2013} 8", "go to a pane"),
        row("Tab", "next pane"),
        row("j / k", "move the cursor"),
        row("g / G", "first / last"),
        row("a, Enter", "attach to the agent under the cursor"),
        row("i", "type in the chat; Esc stops"),
        row("o", "open the newest link on screen in a browser"),
        row("[ / ]", "previous / next channel"),
        row("w", "next workspace, in the graph"),
        row("s", "change what the poll covers"),
        row("p", "show or hide the detail panel"),
        row("t", "switch theme (deck / lcars)"),
        row("r", "refresh now"),
        row("q", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  Everything here is the same snapshot the dashboard reads.",
            Style::default().fg(th.faint()),
        )),
    ]);
    f.render_widget(Clear, box_);
    f.render_widget(
        Paragraph::new(text).block(framed(th.label("keys"), th, true)),
        box_,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_numbers_are_readable() {
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(12_400), "12.4k");
        assert_eq!(human(2_500_000), "2.5M");
    }

    #[test]
    fn clipping_never_widens_a_column() {
        // A cell one character over its column pushes a table sideways and
        // the last column off the screen.
        for w in 1..20usize {
            assert!(clip("an extremely long summary line", w).chars().count() <= w);
        }
        assert_eq!(clip("short", 20), "short");
        assert!(clip("much longer than that", 8).ends_with('\u{2026}'));
    }

    #[test]
    fn wrapping_keeps_every_word() {
        let src = "a message from a teammate that has to be read in a narrow panel";
        let lines = textwrap(src, 20);
        assert!(lines.iter().all(|l| l.chars().count() <= 20), "{lines:?}");
        assert_eq!(lines.join(" "), src);
    }

    #[test]
    fn a_word_longer_than_the_panel_still_appears() {
        // A path has no spaces in it. Dropping it would be worse than
        // letting one line run long.
        let lines = textwrap("/very/long/path/with/no/spaces/at/all.rs", 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("all.rs"));
    }

    #[test]
    fn missing_fields_read_as_empty_rather_than_panicking() {
        // The snapshot is JSON from a hub that may be older than this
        // console: a field that is not there must not take the screen down.
        let v = serde_json::json!({ "a": "x", "n": 3, "b": true });
        assert_eq!(s(&v, "a"), "x");
        assert_eq!(s(&v, "nope"), "");
        assert_eq!(n(&v, "n"), 3);
        assert_eq!(n(&v, "nope"), 0);
        assert!(b(&v, "b"));
        assert!(!b(&v, "nope"));
    }
}
