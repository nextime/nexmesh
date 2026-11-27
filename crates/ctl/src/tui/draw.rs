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
    if app.zoom && (app.pane == Pane::Memory || app.zoom_text.is_some()) {
        zoomed(f, area, app);
    }
    if app.dialog.is_some() {
        dialog(f, area, app);
    }
    if app.help {
        help(f, area, app);
    }
}

/// The question on the screen: a list to choose from, a line to type, or a
/// yes. Drawn over everything, because it owns the keyboard while it is up.
fn dialog(f: &mut Frame, area: Rect, app: &App) {
    use crate::tui::act::Kind;
    let th = app.theme;
    let Some(d) = app.dialog.as_ref() else { return };
    let rows = match &d.kind {
        Kind::Pick(v) => v.len().min(12),
        Kind::Text(_) => 1,
        Kind::Confirm => 1,
    } as u16;
    let hint = textwrap(&d.hint, 60);
    let w = 66u16.min(area.width.saturating_sub(4));
    let h = (rows + hint.len() as u16 + 5).min(area.height.saturating_sub(2));
    let box_ = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let mut text: Vec<Line> = vec![Line::from(Span::styled(
        format!(" {}", d.title),
        th.title().add_modifier(Modifier::BOLD),
    ))];
    for l in &hint {
        text.push(Line::from(Span::styled(
            format!(" {l}"),
            Style::default().fg(th.faint()),
        )));
    }
    text.push(Line::from(""));
    match &d.kind {
        Kind::Pick(options) => {
            // A window over the list, so a fleet with twenty sessions in it
            // does not need a box taller than the screen.
            let room = rows as usize;
            let first = d.at.saturating_sub(room.saturating_sub(1));
            for (i, c) in options.iter().enumerate().skip(first).take(room) {
                let on = i == d.at;
                text.push(Line::from(Span::styled(
                    format!(" {} {}", if on { "\u{25b8}" } else { " " }, clip(&c.label, w as usize - 5)),
                    if on { th.selected() } else { Style::default().fg(th.fg()) },
                )));
            }
        }
        Kind::Text(c) => {
            let shown = clip(&c.text, w as usize - 6);
            text.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(th.faint())),
                Span::styled(shown, Style::default().fg(th.fg())),
                Span::styled("\u{2588}", Style::default().fg(th.accent())),
            ]));
        }
        Kind::Confirm => {
            text.push(Line::from(Span::styled(
                "  y, or Enter, to go ahead \u{b7} n or Esc to leave it",
                Style::default().fg(th.second()),
            )));
        }
    }
    if !matches!(d.kind, Kind::Confirm) {
        text.push(Line::from(Span::styled(
            "  Enter to go on \u{b7} Esc to cancel",
            Style::default().fg(th.faint()),
        )));
    }
    f.render_widget(Clear, box_);
    f.render_widget(
        Paragraph::new(Text::from(text)).block(framed(th.label("do"), th, true)),
        box_,
    );
}

/// One memory, all of it. A value is a paragraph and the pane is a table, so
/// the table shows the first line and this shows the rest.
fn zoomed(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    // Either a command's answer, or the memory under the cursor.
    let k = match &app.zoom_text {
        Some(t) => serde_json::json!({ "key": "answer", "value": t }),
        None => match app.selected_memory() {
            Some(k) => k,
            None => return,
        },
    };
    // Wide enough to cover the table behind it: a modal narrower than the
    // rows it sits on leaves the tail of a long value sticking out to the
    // right, which reads as a drawing fault rather than as a window.
    let w = (area.width.saturating_sub(6)).min(120).max(40);
    let h = (area.height.saturating_sub(4)).min(24);
    let box_ = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let inner = w.saturating_sub(4) as usize;
    let mut text: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(s(&k, "key"), th.title().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled(
            // A command's answer has no author or revision; a memory has
            // both, and saying so is the difference between reading one and
            // wondering where it came from.
            if app.zoom_text.is_some() {
                " Esc to put it away".to_string()
            } else {
                format!(
                    " {} \u{b7} {} \u{b7} by {} \u{b7} rev {} \u{b7} {}",
                    s(&k, "category"),
                    s(&k, "scope"),
                    s(&k, "who"),
                    n(&k, "revision"),
                    hhmmss(n(&k, "at_ms"))
                )
            },
            Style::default().fg(th.faint()),
        )),
        Line::from(""),
    ];
    // Paragraphs are kept: a memory written as a list reads as one.
    let body = s(&k, "value");
    let mut wrapped: Vec<Line> = Vec::new();
    for para in body.split('\n') {
        if para.trim().is_empty() {
            wrapped.push(Line::from(""));
            continue;
        }
        for l in textwrap(para, inner) {
            wrapped.push(Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(th.fg()),
            )));
        }
    }
    let room = (h as usize).saturating_sub(5);
    let back = app.scroll.min(wrapped.len().saturating_sub(room.max(1)));
    let more = wrapped.len() > room + back;
    text.extend(wrapped.into_iter().skip(back).take(room));
    if more {
        text.push(Line::from(Span::styled(
            " \u{25bc} more \u{2014} PgDn",
            Style::default().fg(th.second()),
        )));
    }
    // Clear the whole band, not just the box: a table row behind it is wider
    // than the box, and its tail peeking out at the edge reads as a fault.
    f.render_widget(
        Clear,
        Rect { x: area.x, y: box_.y, width: area.width, height: box_.height },
    );
    f.render_widget(
        Paragraph::new(Text::from(text)).block(framed(
            th.label(if app.zoom_text.is_some() { "answer" } else { "memory" }),
            th,
            true,
        )),
        box_,
    );
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
    if app.recording.is_some() {
        spans.push(Span::styled(
            "   \u{25cf} listening",
            Style::default().fg(th.bad()).add_modifier(Modifier::BOLD),
        ));
    } else if app.audio.can_hear() {
        spans.push(Span::styled("   v say it", Style::default().fg(th.faint())));
    }
    if app.speaking {
        spans.push(Span::styled("   \u{1F50A} reading aloud", Style::default().fg(th.second())));
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

/// A seat with somebody in it, or with somebody due back.
///
/// `waiting` is the hub's word for a seat whose CLI is one process per turn:
/// a cron-driven loop takes the seat, says its piece and exits, and the next
/// turn is along in a minute. Counting it as adrift put a perfectly healthy
/// teammate in the "no process" tile once a minute, for ever.
fn present(a: &serde_json::Value) -> bool {
    matches!(s(a, "status").as_str(), "running" | "waiting")
}

/// The deck: what you want to know before you know what you are looking for.
fn deck(f: &mut Frame, area: Rect, app: &App) {
    let th = app.theme;
    let agents = app.rows("agents");
    let msgs = app.rows("messages");
    let nodes = app.nodes();
    // Strictly a process that is up now; `waiting` is neither running nor
    // adrift, so it is counted in neither tile.
    let running = agents.iter().filter(|a| s(a, "status") == "running").count();
    let adrift = agents
        .iter()
        .filter(|a| !present(a) && s(a, "kind") != "external")
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

    // Who is adrift goes in a strip across the top, and what just happened
    // gets everything underneath it. The other way round -- a tall list
    // beside a narrow feed -- spent half the deck on a column that is
    // usually empty, and cut every message in the one that is not.
    let adrift_rows: Vec<&Value> = agents.iter().filter(|a| !present(a)).collect();
    let stack = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2 + adrift_rows.len().clamp(1, 3) as u16),
            Constraint::Min(3),
        ])
        .split(rows[1]);

    // Three lines at the most. Past that the third line says how many are
    // not shown, because the list is a prompt to open the agents pane, not
    // the place to read it.
    let shown = if adrift_rows.len() > 3 { 2 } else { adrift_rows.len() };
    let mut look: Vec<Line> = Vec::new();
    for a in adrift_rows.iter().take(shown) {
        look.push(Line::from(vec![
            Span::styled(
                format!(" {:<18}", clip(&s(a, "name"), 18)),
                Style::default().fg(th.nick(&s(a, "name"))),
            ),
            Span::styled(format!("{:<13}", s(a, "status")), Style::default().fg(th.warn())),
            Span::styled(format!("{:<10}", clip(&s(a, "session"), 10)), Style::default().fg(th.dim())),
            Span::styled(s(a, "host"), Style::default().fg(th.faint())),
        ]));
    }
    if adrift_rows.len() > shown {
        look.push(Line::from(Span::styled(
            format!(
                " and {} more \u{2014} 2 for the whole list",
                adrift_rows.len() - shown
            ),
            Style::default().fg(th.faint()),
        )));
    }
    if look.is_empty() {
        look.push(Line::from(Span::styled(
            " every seat has a process behind it",
            Style::default().fg(th.ok()),
        )));
    }
    f.render_widget(
        Paragraph::new(Text::from(look)).block(framed(th.label("needs a look"), th, false)),
        stack[0],
    );

    f.render_widget(
        Paragraph::new(Text::from(feed_lines(
            &msgs,
            th,
            stack[1].width.saturating_sub(2) as usize,
            stack[1].height.saturating_sub(2) as usize,
        )))
        .block(framed(th.label("latest"), th, false)),
        stack[1],
    );
}

/// The message feed, wrapped rather than cut off.
///
/// `msgs` is newest first, which is how it is read here: the top line is what
/// just happened. A message too long for one line is continued underneath,
/// indented past the sender, and capped -- one 4000-character paste must not
/// become the whole pane.
fn feed_lines<'a>(msgs: &[Value], th: Theme, width: usize, height: usize) -> Vec<Line<'a>> {
    const INDENT: usize = 24;
    const MAX_PER_MESSAGE: usize = 6;
    let avail = width.saturating_sub(INDENT);
    let mut lines: Vec<Line> = Vec::new();
    for m in msgs {
        if lines.len() >= height {
            break;
        }
        let from = s(m, "from");
        let chunks = wrap_shaped(&s(m, "body"), avail);
        let cut = chunks.len() > MAX_PER_MESSAGE;
        for (i, c) in chunks.iter().take(MAX_PER_MESSAGE).enumerate() {
            if lines.len() >= height {
                return lines;
            }
            let tail = if cut && i + 1 == MAX_PER_MESSAGE {
                format!("{c} \u{2026}")
            } else {
                c.clone()
            };
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(hhmmss(n(m, "at_ms")), Style::default().fg(th.faint())),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:>12}", clip(&from, 12)),
                        Style::default().fg(th.nick(&from)),
                    ),
                    Span::styled(" \u{2192} ", Style::default().fg(th.faint())),
                    Span::styled(tail, Style::default().fg(th.fg())),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(INDENT)),
                    Span::styled(tail, Style::default().fg(th.fg())),
                ]));
            }
        }
    }
    lines
}

/// `&str` column names, as `table` wants them.
fn heads(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn table(
    f: &mut Frame,
    area: Rect,
    app: &App,
    title: String,
    head: Vec<String>,
    widths: Vec<Constraint>,
    rows: Vec<Vec<String>>,
) {
    let th = app.theme;
    let cur = app.cursor();
    let header = Row::new(head.into_iter().map(|h| th.label(&h)).collect::<Vec<_>>())
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

/// The agent pane's columns, in the order they are drawn.
///
/// One list for the header, the cells and the sort key, so the three cannot
/// drift apart: a table sorted by a column other than the one with the arrow
/// on it is worse than one that cannot be sorted at all.
pub(crate) const AGENT_COLS: [&str; 11] =
    ["name", "session", "seat", "cli", "role", "where", "state", "now", "agent", "pid", "io"];

/// What a seat's model is doing this moment, beside the work it is on: a
/// turn running, idle, stopped on a question -- or held, because its account
/// cannot reach the model.
fn now_cell(a: &Value) -> String {
    match (
        a.get("hold_reason").and_then(|x| x.as_str()),
        a.get("activity").and_then(|x| x.as_str()),
    ) {
        (Some(r), _) => format!("HELD:{r}"),
        (None, Some("active")) => "thinking".into(),
        (None, Some("blocked")) => "needs-you".into(),
        (None, Some("idle")) => "idle".into(),
        _ => "-".into(),
    }
}

pub(crate) fn agent_cell(a: &Value, col: usize) -> String {
    match col {
        0 => s(a, "name"),
        1 => s(a, "session"),
        2 => s(a, "slot_id"),
        3 => s(a, "cli"),
        4 => s(a, "role"),
        5 => s(a, "host"),
        6 => s(a, "task_state"),
        7 => now_cell(a),
        8 => s(a, "status"),
        9 => match a.get("pid").and_then(|p| p.as_i64()) {
            Some(p) => p.to_string(),
            None => "-".into(),
        },
        _ => a.get("io_kind").and_then(|x| x.as_str()).unwrap_or("-").to_string(),
    }
}

/// Agents in the order the pane draws them.
///
/// Empty and `-` sort last whichever way round the rest is: a column gets
/// sorted to find the rows that have something in it, and an ascending sort
/// that opens with eight dashes answers a different question. Numbers in a
/// column are compared as numbers -- pid 9 before pid 10 -- and name breaks
/// every tie, so the order is stable while the fleet is not.
pub(crate) fn sort_agents(rows: &mut [Value], col: usize, rev: bool) {
    let key = |a: &Value| -> (u8, i64, String) {
        let cell = agent_cell(a, col);
        let missing = u8::from(cell.is_empty() || cell == "-");
        let num = cell.trim().parse::<i64>().unwrap_or(0);
        (missing, num, cell.to_lowercase())
    };
    rows.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        ka.0.cmp(&kb.0)
            .then_with(|| {
                let body = (ka.1, &ka.2).cmp(&(kb.1, &kb.2));
                if rev {
                    body.reverse()
                } else {
                    body
                }
            })
            .then_with(|| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()))
    });
}

fn agents(f: &mut Frame, area: Rect, app: &App) {
    let ordered = app.agents_in_order();
    let rows: Vec<Vec<String>> = ordered
        .iter()
        .map(|a| (0..AGENT_COLS.len()).map(|c| agent_cell(a, c)).collect())
        .collect();
    // The arrow is on the column being sorted by, pointing the way the rows
    // run. Tokens used to live here and told you nothing you could act on;
    // which session a seat is in is the first thing you want when the fleet
    // has seven of them.
    let head: Vec<String> = AGENT_COLS
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == app.sort_col {
                format!("{c}{}", if app.sort_rev { '\u{25be}' } else { '\u{25b4}' })
            } else {
                c.to_string()
            }
        })
        .collect();
    table(
        f,
        area,
        app,
        format!(
            "{} ({})  \u{2014}  , . sort \u{b7} R reverse",
            app.theme.label("agents"),
            rows.len()
        ),
        head,
        // Wide enough for what actually goes in them: a role reading
        // "code revie" and a machine reading "machine-" is a table that has
        // been made narrow for no one's benefit.
        vec![
            Constraint::Length(17), Constraint::Length(13), Constraint::Length(12),
            Constraint::Length(10), Constraint::Length(15), Constraint::Length(12),
            Constraint::Length(14), Constraint::Length(11), Constraint::Length(13),
            Constraint::Length(6), Constraint::Min(5),
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

    // The room itself: oldest at the top, newest at the bottom, which is
    // the way a conversation reads and the way every chat client since
    // `irc` has drawn one. It used to take the OLDEST screenful and print it
    // newest-first -- two reverses that cancelled the wrong way round, so the
    // pane showed the start of the day upside down and never the last line.
    let here: Vec<Value> = app
        .rows("messages")
        .into_iter()
        .filter(|m| app.in_channel(m))
        // Talking to one teammate shows what passed between the two of you
        // and nothing else. The same log, read narrowly.
        .filter(|m| match &app.with_them {
            None => true,
            Some(who) => s(m, "from") == *who || s(m, "to") == *who,
        })
        .collect();

    let name = match &app.with_them {
        Some(who) => format!("@{who}"),
        None => chans
            .iter()
            .find(|(id, _)| *id == app.chan)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| "#all".into()),
    };
    let feed = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(cols[1]);
    let height = feed[0].height.saturating_sub(2) as usize;
    let w = feed[0].width.saturating_sub(2) as usize;

    // Every line of the channel, then the window the operator is looking at.
    // Building all of them costs nothing at 120 messages and is the only way
    // a wrapped feed can be scrolled by lines rather than by messages.
    let mut lines: Vec<Line> = Vec::new();
    for m in here.iter().rev() {
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
            format!("<{from}>"),
            Style::default().fg(th.nick(&from)).add_modifier(Modifier::BOLD),
        ));
        let to = s(m, "to");
        if !to.is_empty() && to != "(team)" {
            spans.push(Span::styled(format!(" \u{2192}{to}"), Style::default().fg(th.dim())));
        }
        spans.push(Span::raw(" "));
        // A message carrying a link says so, because the terminal cannot
        // show what is behind it and `o` can.
        let body = s(m, "body");
        if crate::tui::first_link(&body).is_some() {
            spans.push(Span::styled("\u{1F517} ", Style::default().fg(th.second())));
        }
        let pre: usize = spans.iter().map(|sp| sp.content.chars().count()).sum();
        // The continuation indent is the sender's width, but capped: a long
        // nick in a narrow pane would otherwise leave three characters for
        // the message.
        let indent = pre.min(w / 3);
        let chunks = wrap_shaped(&body, w.saturating_sub(indent));
        let mut rest = chunks.as_slice();
        if let Some(first) = chunks.first() {
            // The first line only joins the header if it fits beside it.
            if pre + first.chars().count() <= w {
                spans.push(Span::styled(first.clone(), Style::default().fg(th.fg())));
                rest = &chunks[1..];
            }
        }
        lines.push(Line::from(spans));
        for c in rest {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(c.clone(), Style::default().fg(th.fg())),
            ]));
        }
    }
    let total = lines.len();
    let back = app.scroll.min(total.saturating_sub(height));
    let at = total.saturating_sub(height + back);
    let view: Vec<Line> = lines.into_iter().skip(at).take(height.max(1)).collect();

    let title = if back > 0 {
        format!("{name}  \u{2014}  \u{25b2} {back} line(s) back, PgDn to catch up")
    } else if app.editing {
        format!("{name}  \u{2014}  typing; Esc stops, \u{2191}\u{2193} changes channel, /help for commands")
    } else {
        format!("{name}  \u{2014}  i to type, /help for commands")
    };
    f.render_widget(
        Paragraph::new(Text::from(view)).block(framed(title, th, true)),
        feed[0],
    );

    // The composer, with a caret you can move. The window slides so the
    // caret is always on screen, however long the line is.
    let inner = feed[1].width.saturating_sub(5) as usize;
    let chars: Vec<char> = app.input.text.chars().collect();
    let caret = app.input.caret.min(chars.len());
    let from = caret.saturating_sub(inner.saturating_sub(1));
    let shown: String = chars[from..].iter().take(inner).collect();
    let at_caret = caret - from;
    let mut prompt = vec![
        Span::styled(" > ", Style::default().fg(th.faint())),
        Span::styled(
            shown.chars().take(at_caret).collect::<String>(),
            Style::default().fg(th.fg()),
        ),
    ];
    if app.editing {
        let under: String = shown.chars().skip(at_caret).take(1).collect();
        prompt.push(Span::styled(
            if under.is_empty() { "\u{2588}".to_string() } else { under },
            Style::default().fg(th.accent()).add_modifier(Modifier::REVERSED),
        ));
        prompt.push(Span::styled(
            shown.chars().skip(at_caret + 1).collect::<String>(),
            Style::default().fg(th.fg()),
        ));
    } else {
        prompt.push(Span::styled(
            shown.chars().skip(at_caret).collect::<String>(),
            Style::default().fg(th.fg()),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(prompt)).block(framed(th.label("say"), th, app.editing)),
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
        heads(&["#", "step", "status", "owner", "note"]),
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
                s(k, "who"),
                clip(&s(k, "value").replace('\n', " "), 120),
            ]
        })
        .collect();
    table(
        f,
        area,
        app,
        format!(
            "{} ({})  \u{2014}  Enter opens the one under the cursor",
            app.theme.label("memory"),
            rows.len()
        ),
        heads(&["key", "kind", "scope", "by", "value"]),
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
        heads(&["size", "mostly in", "for example"]),
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
        heads(&["what", "path", "who", "mode", "note"]),
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
        heads(&["machine", "state", "seen", "what it has"]),
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
            let rows = app.agents_in_order();
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
            // Who is in THIS room. The panel used to list every agent on
            // the fleet whichever channel you were in, which made it a
            // second copy of the agents pane rather than a roster.
            let room = app.channel_session();
            let here: Vec<Value> = app
                .rows("agents")
                .into_iter()
                .filter(|a| room.as_deref().is_none_or(|r| s(a, "session") == r))
                .collect();
            out.push(Line::from(Span::styled(
                match &room {
                    Some(r) => format!(" in #{r}"),
                    None => " everywhere".to_string(),
                },
                th.title(),
            )));
            out.push(Line::from(Span::styled(
                format!(
                    " {} seat(s), {} with a process",
                    here.len(),
                    here.iter().filter(|a| present(a)).count()
                ),
                Style::default().fg(th.faint()),
            )));
            out.push(Line::from(Span::styled(
                match &app.with_them {
                    Some(w) => format!(" talking to {w} alone \u{2014} Enter: back to the room"),
                    None => " \u{2191}\u{2193} a name \u{00b7} Enter talks to it \u{00b7} a attaches"
                        .to_string(),
                },
                Style::default().fg(th.second()),
            )));
            out.push(Line::from(""));
            let picked = app.roster.min(here.len().saturating_sub(1));
            for (ri, a) in here.into_iter().enumerate() {
                let name = s(&a, "name");
                let on = ri == picked;
                out.push(Line::from(vec![
                    Span::styled(
                        if present(&a) { " \u{25cf} " } else { " \u{25cb} " },
                        Style::default().fg(if present(&a) {
                            th.ok()
                        } else {
                            th.faint()
                        }),
                    ),
                    Span::styled(
                        clip(&name, 16),
                        if on {
                            Style::default().fg(th.nick(&name)).add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default().fg(th.nick(&name))
                        },
                    ),
                    Span::styled(
                        format!(" {}", clip(&s(&a, "session"), 14)),
                        Style::default().fg(th.dim()),
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

/// Wrap a message, keeping the shape it was written in.
///
/// `textwrap` alone wraps on `split_whitespace`, which is a way of saying it
/// deletes every newline, blank line and indent it is given. Both chat panes
/// used to hand it a body with the newlines already replaced by spaces on top
/// of that, so a teammate's numbered list, diff or indented snippet reached
/// the reader as one run-on paragraph -- and the formatting is most of what a
/// snippet IS.
///
/// Each line keeps its own indent, and a wrapped line keeps it too, so a
/// block shifts without reflowing. Runs of blank lines collapse to one and
/// the ends are trimmed, because vertical room in a pane is scarce.
fn wrap_shaped(s: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in s.replace('\r', "\n").replace('\t', "    ").split('\n') {
        if raw.trim().is_empty() {
            // One blank line between paragraphs, never two, and never at
            // the top where it would push the message off its own header.
            if !out.is_empty() && !out.last().map(|l| l.is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            continue;
        }
        let indent: String = raw.chars().take_while(|c| *c == ' ').collect();
        let pad = indent.chars().count();
        for piece in textwrap(raw.trim_start_matches(' '), w.saturating_sub(pad)) {
            out.push(format!("{indent}{piece}"));
        }
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
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
        Pane::Chat => " j/k channel · ↑↓ a name · Enter talks to it · a attach · i type · v say · ? help",
        Pane::Graph => " 1-8 panes · j/k subsystem · w workspace · p panel · t theme · ? help · q quit",
        Pane::Agents => " a attach · n new · S start · m move · e rename · E prompt · x remove · P camera · , . sort · ? help",
        Pane::Memory => " j/k move · Enter opens it · PgDn scrolls · s scope · ? help · q quit",
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
    let w = 68u16.min(area.width.saturating_sub(4));
    let h = 24u16.min(area.height.saturating_sub(2));
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
        row("n", "a new seat: CLI, name, and whether to start it"),
        row("S", "start a process for the seat under the cursor"),
        row("m", "move that seat into another session"),
        row("e", "rename it"),
        row("E", "edit what it tells its agent: system prompt, arrival instruction"),
        row("x", "remove it, seat and history"),
        row(", .", "sort the agents by the previous / next column"),
        row("R", "reverse the agent order"),
        row("Enter", "in memory: open the whole of one"),
        row("PgUp/PgDn", "scroll the chat back, or a memory down"),
        row("wheel", "the same, three lines at a time"),
        row("i", "type in the chat; arrows and Tab still work"),
        row("v", "say it instead: record, then the fleet transcribes it"),
        row("P", "the camera: a photo or a clip, into the workspace"),
        row("V", "read new messages aloud, if the fleet has a voice"),
        row("o", "open the newest link on screen in a browser"),
        row("[ / ]", "previous / next channel"),
        row("Enter", "in chat: talk to the name under the cursor, alone"),
        row("a", "in chat: attach to that one's terminal"),
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
        Line::from(Span::styled(
            "  The mouse is the console's while it runs: hold SHIFT to select.",
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

    fn flat(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn the_latest_feed_wraps_instead_of_cutting() {
        let long = "the node on the second machine lost its connection to the hub \
                    and came back with a different pid, which is why the seat went \
                    through disconnected on the way";
        let msgs = vec![serde_json::json!({
            "at_ms": 1_700_000_000_000i64, "from": "Orchestrator", "body": long,
        })];
        let lines = feed_lines(&msgs, Theme::Deck, 80, 10);
        assert!(lines.len() > 1, "a long message must continue underneath");
        let all = lines.iter().map(flat).collect::<Vec<_>>().join(" ");
        // Every word of it is on the screen somewhere.
        for word in ["disconnected", "different", "connection"] {
            assert!(all.contains(word), "{word} missing from {all}");
        }
        for l in &lines {
            assert!(flat(l).chars().count() <= 80, "{}", flat(l));
        }
    }

    #[test]
    fn the_feed_never_draws_more_lines_than_the_pane_has() {
        let msgs: Vec<Value> = (0..40)
            .map(|i| {
                serde_json::json!({
                    "at_ms": 1_700_000_000_000i64 + i,
                    "from": "x",
                    "body": "a line that is long enough to need wrapping in a narrow pane",
                })
            })
            .collect();
        for height in 0..12usize {
            assert!(feed_lines(&msgs, Theme::Deck, 60, height).len() <= height);
        }
    }

    #[test]
    fn one_enormous_message_does_not_become_the_whole_pane() {
        let body = "word ".repeat(2000);
        let msgs = vec![
            serde_json::json!({ "at_ms": 1i64, "from": "a", "body": body }),
            serde_json::json!({ "at_ms": 2i64, "from": "b", "body": "and the next one" }),
        ];
        let lines = feed_lines(&msgs, Theme::Deck, 80, 40);
        assert!(lines.len() <= 7, "{} lines", lines.len());
        assert!(
            lines.iter().any(|l| flat(l).contains("and the next one")),
            "the message after a long one has to be reachable"
        );
    }

    #[test]
    fn agents_sort_by_the_column_asked_for() {
        let rows = |v: &[Value]| -> Vec<String> {
            v.iter().map(|a| s(a, "name")).collect()
        };
        let mut v = vec![
            serde_json::json!({ "name": "b", "session": "zeta", "pid": 9 }),
            serde_json::json!({ "name": "a", "session": "alpha", "pid": 100 }),
            serde_json::json!({ "name": "c", "session": "alpha" }),
        ];
        sort_agents(&mut v, 0, false);
        assert_eq!(rows(&v), ["a", "b", "c"]);
        sort_agents(&mut v, 0, true);
        assert_eq!(rows(&v), ["c", "b", "a"]);
        // By session: alphabetical, and name breaks the tie inside one.
        sort_agents(&mut v, 1, false);
        assert_eq!(rows(&v), ["a", "c", "b"]);
        // By pid: as numbers, so 9 is before 100 -- and the row with no
        // process at all is last whichever way round the rest is.
        let pid = AGENT_COLS.iter().position(|c| *c == "pid").unwrap();
        sort_agents(&mut v, pid, false);
        assert_eq!(rows(&v), ["b", "a", "c"]);
        sort_agents(&mut v, pid, true);
        assert_eq!(rows(&v), ["a", "b", "c"]);
    }

    #[test]
    fn every_agent_column_has_a_cell_and_a_name() {
        // The header, the cells and the sort key are the same list; a column
        // the cell function does not know about would sort by nothing.
        let a = serde_json::json!({
            "name": "n", "session": "s", "slot_id": "sl-1", "cli": "claude",
            "role": "dev", "host": "machine-a", "task_state": "working",
            "status": "running", "pid": 7, "io_kind": "pty",
        });
        let cells: Vec<String> = (0..AGENT_COLS.len()).map(|c| agent_cell(&a, c)).collect();
        assert_eq!(cells.len(), AGENT_COLS.len());
        assert!(cells.iter().all(|c| !c.is_empty()), "{cells:?}");
        assert_eq!(cells[1], "s");
        assert!(!cells.contains(&"tokens".to_string()));
        // Past the last column nothing panics: io is the fallback.
        assert_eq!(agent_cell(&a, 99), "pty");
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

#[cfg(test)]
mod shape_tests {
    use super::wrap_shaped;

    #[test]
    fn a_list_keeps_its_lines_and_its_indent() {
        // The chat panes used to replace every newline with a space and then
        // wrap on whitespace, so this arrived as one run-on line.
        let got = wrap_shaped("do these:\n  1. rebase\n  2. test", 40);
        assert_eq!(got, vec!["do these:", "  1. rebase", "  2. test"]);
    }

    #[test]
    fn a_wrapped_line_keeps_the_indent_it_started_with() {
        // Otherwise the continuation of an indented bullet lands under the
        // left margin and reads as a new one.
        let got = wrap_shaped("    aaa bbb ccc ddd", 12);
        assert!(got.iter().all(|l| l.starts_with("    ")), "{got:?}");
        assert!(got.len() > 1, "{got:?}");
    }

    #[test]
    fn blank_lines_are_kept_but_not_hoarded() {
        // One blank between paragraphs is structure; four is a pane with no
        // room left for the next message.
        let got = wrap_shaped("one\n\n\n\ntwo", 40);
        assert_eq!(got, vec!["one", "", "two"]);
    }

    #[test]
    fn the_ends_are_trimmed() {
        let got = wrap_shaped("\n\n  hi  \n\n", 40);
        assert_eq!(got, vec!["  hi"]);
    }

    #[test]
    fn a_tab_becomes_an_indent_rather_than_vanishing() {
        let got = wrap_shaped("fn a() {\n\tb();\n}", 40);
        assert_eq!(got, vec!["fn a() {", "    b();", "}"]);
    }
}
