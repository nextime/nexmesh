//! The operator console: everything the dashboard shows, in a terminal.
//!
//! `nexmesh` with no subcommand, run from a terminal, opens this. From a
//! script or a pipe it prints help as it always did -- a program that clears
//! somebody's screen because it was called without arguments is a program
//! that gets wrapped in `</dev/null` for ever.
//!
//! It polls the same `/v1/dashboard/state` the browser does, so there is one
//! view of the fleet and one place for it to be computed. What differs is the
//! ink: panes instead of tabs, a context panel on the right, braille for the
//! code graph, and a keyboard rather than a mouse.

mod draw;
mod map;
mod theme;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, ExecutableCommand};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::Value;
use std::io::stdout;
use std::time::{Duration, Instant};
use theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Deck,
    Agents,
    Chat,
    Plan,
    Memory,
    Graph,
    Files,
    Fleet,
}

impl Pane {
    pub const ALL: [Pane; 8] = [
        Pane::Deck, Pane::Agents, Pane::Chat, Pane::Plan,
        Pane::Memory, Pane::Graph, Pane::Files, Pane::Fleet,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Pane::Deck => "deck",
            Pane::Agents => "agents",
            Pane::Chat => "chat",
            Pane::Plan => "plan",
            Pane::Memory => "memory",
            Pane::Graph => "graph",
            Pane::Files => "files",
            Pane::Fleet => "fleet",
        }
    }

    fn index(self) -> usize {
        Pane::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// Where the console's own preferences live. Not in the identity directory:
/// that holds credentials, and a theme is not one.
fn pref_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".nexmesh").join("console.toml")
}

fn load_theme() -> Theme {
    let raw = std::fs::read_to_string(pref_path()).unwrap_or_default();
    for line in raw.lines() {
        if let Some(v) = line.trim().strip_prefix("theme") {
            let v = v.trim_start().trim_start_matches('=').trim().trim_matches('"');
            return Theme::from_name(v);
        }
    }
    Theme::Deck
}

fn save_theme(t: Theme) {
    let p = pref_path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("# nexmesh console\ntheme = \"{}\"\n", t.name()));
}

pub struct App {
    base: String,
    client: reqwest::Client,
    ca_cert: Option<String>,
    identity: Option<(String, String)>,

    snap: Option<Value>,
    err: Option<String>,
    note: Option<(String, Instant)>,

    pane: Pane,
    /// The row cursor, per pane: moving in one pane must not scramble the
    /// others, which is what a single shared index does.
    sel: [usize; 8],
    ws: usize,
    chan: String,
    input: String,
    editing: bool,
    panel: bool,
    help: bool,
    theme: Theme,
    scope: String,
    last_fetch: Instant,
    quit: bool,
    /// Set when a key asks for something that leaves the console -- an
    /// attach takes the whole screen, so the loop has to put the terminal
    /// back first and pick it up again afterwards.
    attach_to: Option<(String, String)>,
}

impl App {
    fn detail(&self) -> Option<&Value> {
        self.snap.as_ref().and_then(|s| s.get("detail"))
    }

    fn rows(&self, key: &str) -> Vec<Value> {
        self.detail()
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn sessions(&self) -> Vec<Value> {
        self.snap
            .as_ref()
            .and_then(|s| s.get("sessions"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn nodes(&self) -> Vec<Value> {
        self.snap
            .as_ref()
            .and_then(|s| s.get("nodes"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    fn cursor(&self) -> usize {
        self.sel[self.pane.index()]
    }

    fn set_cursor(&mut self, n: usize) {
        let i = self.pane.index();
        self.sel[i] = n;
    }

    /// How many rows the current pane has, so the cursor cannot walk off it.
    fn len(&self) -> usize {
        match self.pane {
            Pane::Agents => self.rows("agents").len(),
            Pane::Chat => self.channels().len(),
            Pane::Plan => self.rows("plan").len(),
            Pane::Memory => self.rows("knowledge").len(),
            Pane::Graph => {
                let g = self.rows("graph");
                g.get(self.ws)
                    .and_then(|w| w["subsystems"].as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            }
            Pane::Files => self.rows("locks").len() + self.rows("zones").len(),
            Pane::Fleet => self.nodes().len(),
            Pane::Deck => 0,
        }
    }

    fn channels(&self) -> Vec<(String, String)> {
        let mut out = vec![("*".to_string(), "#all".to_string())];
        for s in self.sessions() {
            let name = s["name"].as_str().unwrap_or("?").to_string();
            let id = s["id"].as_str().unwrap_or("").to_string();
            out.push((id, format!("#{name}")));
        }
        out
    }

    fn notice(&mut self, s: impl Into<String>) {
        self.note = Some((s.into(), Instant::now()));
    }

    async fn fetch(&mut self) {
        let url = format!(
            "{}/v1/dashboard/state?session={}",
            self.base,
            urlish(&self.scope)
        );
        match crate::get(&self.client, url).await {
            Ok(v) => {
                if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                    self.err = Some(e.to_string());
                } else {
                    self.err = None;
                    self.snap = Some(v);
                }
            }
            Err(e) => self.err = Some(trim_err(&e.to_string())),
        }
        self.last_fetch = Instant::now();
    }

    /// Send what is in the composer to the channel the chat pane is on.
    async fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let targets: Vec<String> = if self.chan == "*" {
            self.sessions()
                .iter()
                .filter_map(|s| s["id"].as_str().map(|x| x.to_string()))
                .collect()
        } else {
            vec![self.chan.clone()]
        };
        if targets.is_empty() {
            self.notice("no session to send to");
            return;
        }
        self.input.clear();
        let mut failed = 0;
        for t in &targets {
            let url = format!("{}/v1/sessions/{}/messages", self.base, urlish(t));
            match crate::post(&self.client, url, serde_json::json!({ "text": text })).await {
                Ok(v) if v.get("error").is_some() => failed += 1,
                Err(_) => failed += 1,
                _ => {}
            }
        }
        if failed == targets.len() {
            self.input = text;
            self.notice("could not send that");
        } else if failed > 0 {
            self.notice(format!("sent, except to {failed} session(s)"));
        } else if targets.len() > 1 {
            self.notice(format!("sent to all {} sessions", targets.len()));
        }
        self.fetch().await;
    }

    /// The agent the cursor is on, as (session, name) -- what `attach` wants.
    fn selected_agent(&self) -> Option<(String, String)> {
        let rows = self.rows("agents");
        let a = rows.get(self.sel[Pane::Agents.index()])?;
        let name = a["name"].as_str()?.to_string();
        let session = a["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                self.detail()
                    .and_then(|d| d["session"]["id"].as_str())
                    .map(|s| s.to_string())
            })?;
        Some((session, name))
    }
}

/// Percent-encode the few characters a session id or name can contain that a
/// query string would otherwise read as structure.
fn urlish(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// An error in a footer has one line. `anyhow`'s chain is for a log.
fn trim_err(s: &str) -> String {
    s.lines().next().unwrap_or(s).chars().take(120).collect()
}

pub async fn run(
    base: String,
    client: reqwest::Client,
    ca_cert: Option<String>,
    identity: Option<(String, String)>,
) -> Result<()> {
    let mut app = App {
        base,
        client,
        ca_cert,
        identity,
        snap: None,
        err: None,
        note: None,
        pane: Pane::Deck,
        sel: [0; 8],
        ws: 0,
        chan: "*".into(),
        input: String::new(),
        editing: false,
        panel: true,
        help: false,
        theme: load_theme(),
        // Everything, by default: a console that shows one team is a window,
        // and this is meant to be the whole fleet at a glance.
        scope: "*".into(),
        last_fetch: Instant::now() - Duration::from_secs(10),
        quit: false,
        attach_to: None,
    };

    enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut term = Terminal::new(backend)?;
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(120));

    let result = loop {
        if app.quit {
            break Ok(());
        }
        // An attach owns the screen. Put the terminal back the way it was,
        // run the real attach client, then come back in -- rather than
        // reimplementing a pty viewer that would be a worse one.
        if let Some((session, who)) = app.attach_to.take() {
            if let Err(e) = leave() {
                break Err(e);
            }
            let id = app
                .identity
                .as_ref()
                .map(|(c, k)| (c.as_str(), k.as_str()));
            let r = crate::attach(
                &app.base, &app.client, &session, &who, 4000,
                app.ca_cert.as_deref(), id,
            )
            .await;
            if let Err(e) = enter() {
                break Err(e);
            }
            if let Err(e) = term.clear() {
                break Err(e.into());
            }
            match r {
                Ok(()) => app.notice(format!("detached from {who}")),
                Err(e) => app.notice(trim_err(&e.to_string())),
            }
            app.fetch().await;
        }

        if app.last_fetch.elapsed() >= Duration::from_secs(1) {
            app.fetch().await;
        }
        if let Err(e) = term.draw(|f| draw::all(f, &app)) {
            break Err(e.into());
        }

        tokio::select! {
            _ = ticker.tick() => {}
            ev = events.next() => {
                match ev {
                    Some(Ok(ev)) => {
                        if let Err(e) = handle(&mut app, ev).await {
                            app.notice(trim_err(&e.to_string()));
                        }
                    }
                    Some(Err(e)) => break Err(e.into()),
                    None => break Ok(()),
                }
            }
        }
    };

    let _ = leave();
    result
}

fn enter() -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, crossterm::cursor::Hide)?;
    Ok(())
}

fn leave() -> Result<()> {
    let mut out = stdout();
    let _ = out.execute(crossterm::cursor::Show);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
    Ok(())
}

async fn handle(app: &mut App, ev: Event) -> Result<()> {
    let Event::Key(k) = ev else { return Ok(()) };
    // Windows sends press AND release; without this every key acts twice.
    if k.kind != KeyEventKind::Press {
        return Ok(());
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(k.code, KeyCode::Char('c')) {
        app.quit = true;
        return Ok(());
    }

    // While typing, almost everything is text. Only Esc and Enter are keys.
    if app.editing {
        match k.code {
            KeyCode::Esc => app.editing = false,
            KeyCode::Enter => app.send().await,
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        }
        return Ok(());
    }

    match k.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('?') | KeyCode::F(1) => app.help = !app.help,
        KeyCode::Esc => {
            if app.help {
                app.help = false;
            }
        }
        KeyCode::Char('t') => {
            app.theme = app.theme.next();
            save_theme(app.theme);
            app.notice(format!("theme: {}", app.theme.name()));
        }
        KeyCode::Char('p') => app.panel = !app.panel,
        KeyCode::Char('r') => app.fetch().await,
        KeyCode::Tab | KeyCode::Right => {
            let i = (app.pane.index() + 1) % Pane::ALL.len();
            app.pane = Pane::ALL[i];
        }
        KeyCode::BackTab | KeyCode::Left => {
            let i = (app.pane.index() + Pane::ALL.len() - 1) % Pane::ALL.len();
            app.pane = Pane::ALL[i];
        }
        KeyCode::Char(c @ '1'..='8') => {
            let i = (c as u8 - b'1') as usize;
            app.pane = Pane::ALL[i];
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.len();
            if n > 0 {
                let c = (app.cursor() + 1).min(n - 1);
                app.set_cursor(c);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let c = app.cursor().saturating_sub(1);
            app.set_cursor(c);
        }
        KeyCode::Char('g') => app.set_cursor(0),
        KeyCode::Char('G') => {
            let n = app.len();
            app.set_cursor(n.saturating_sub(1));
        }
        KeyCode::Char('w') if app.pane == Pane::Graph => {
            let n = app.rows("graph").len().max(1);
            app.ws = (app.ws + 1) % n;
            app.set_cursor(0);
        }
        KeyCode::Char('s') => {
            // Cycle the poll's scope: every session, then each in turn.
            let ids: Vec<String> = std::iter::once("*".to_string())
                .chain(
                    app.sessions()
                        .iter()
                        .filter_map(|s| s["id"].as_str().map(|x| x.to_string())),
                )
                .collect();
            let at = ids.iter().position(|x| *x == app.scope).unwrap_or(0);
            app.scope = ids[(at + 1) % ids.len()].clone();
            app.fetch().await;
        }
        KeyCode::Char('i') if app.pane == Pane::Chat => app.editing = true,
        KeyCode::Enter => match app.pane {
            Pane::Chat => {
                // The cursor is on the channel list; Enter joins that channel.
                let chans = app.channels();
                if let Some((id, _)) = chans.get(app.cursor()) {
                    app.chan = id.clone();
                }
                app.editing = true;
            }
            Pane::Agents => {
                if let Some((s, w)) = app.selected_agent() {
                    app.attach_to = Some((s, w));
                }
            }
            _ => {}
        },
        KeyCode::Char('a') => {
            if let Some((s, w)) = app.selected_agent() {
                app.attach_to = Some((s, w));
            }
        }
        KeyCode::Char('[') if app.pane == Pane::Chat => {
            let chans = app.channels();
            let at = chans.iter().position(|(id, _)| *id == app.chan).unwrap_or(0);
            app.chan = chans[(at + chans.len() - 1) % chans.len()].0.clone();
        }
        KeyCode::Char(']') if app.pane == Pane::Chat => {
            let chans = app.channels();
            let at = chans.iter().position(|(id, _)| *id == app.chan).unwrap_or(0);
            app.chan = chans[(at + 1) % chans.len()].0.clone();
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_name_is_escaped_into_the_query() {
        // A name is whatever somebody typed. Unescaped, a space or an
        // ampersand in one silently changes which session is asked for.
        assert_eq!(urlish("se-1a2b3c4d"), "se-1a2b3c4d");
        assert_eq!(urlish("*"), "*");
        assert_eq!(urlish("pay ments"), "pay%20ments");
        assert_eq!(urlish("a&b=c"), "a%26b%3Dc");
        // Multi-byte characters are escaped per byte, not per character.
        assert_eq!(urlish("é"), "%C3%A9");
    }

    #[test]
    fn an_error_in_the_footer_is_one_line() {
        let e = "hub unreachable\ncaused by: connection refused\nand more";
        let t = trim_err(e);
        assert_eq!(t, "hub unreachable");
        assert!(!t.contains('\n'));
    }

    #[test]
    fn every_pane_has_a_number_key() {
        // The footer promises 1-8. An array of nine would make one pane
        // unreachable, and the cursor array is sized to match.
        assert_eq!(Pane::ALL.len(), 8);
        for (i, p) in Pane::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }

    #[test]
    fn a_theme_round_trips_through_the_preferences_file() {
        assert_eq!(Theme::from_name(Theme::Lcars.name()), Theme::Lcars);
        assert_eq!(Theme::from_name(Theme::Deck.name()), Theme::Deck);
    }
}
