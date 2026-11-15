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

mod act;
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
use crate::voice;
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

/// The line being typed, and where in it the next character goes.
///
/// A composer that can only append and backspace is not one you can correct
/// a word in the middle of, and indexing a line of text by byte panics the
/// first time somebody types an accent -- so the caret counts characters and
/// every edit goes through here.
#[derive(Default, Debug)]
pub struct Composer {
    pub text: String,
    pub caret: usize,
}

impl Composer {
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// The byte offset of a character position.
    fn byte_at(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_at(self.caret.min(self.len()));
        self.text.insert(at, c);
        self.caret = self.caret.min(self.len()) + 1;
    }

    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let at = self.byte_at(self.caret - 1);
        self.text.remove(at);
        self.caret -= 1;
    }

    fn delete(&mut self) {
        let at = self.byte_at(self.caret);
        if at < self.text.len() {
            self.text.remove(at);
        }
    }

    /// Ctrl-W: back over the spaces, then back over the word.
    fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut at = self.caret.min(chars.len());
        while at > 0 && chars[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !chars[at - 1].is_whitespace() {
            at -= 1;
        }
        let (from, to) = (self.byte_at(at), self.byte_at(self.caret));
        self.text.replace_range(from..to, "");
        self.caret = at;
    }

    fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.len());
    }

    fn home(&mut self) {
        self.caret = 0;
    }

    fn end(&mut self) {
        self.caret = self.len();
    }

    fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    fn set(&mut self, t: String) {
        self.caret = t.chars().count();
        self.text = t;
    }
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
    /// Talking to ONE teammate rather than the room: the same log, filtered
    /// to what passed between you, and everything typed addressed to them.
    /// Not a second inbox -- a private channel with its own storage would
    /// be a second place to look for a message.
    with_them: Option<String>,
    /// Which name in the room list the cursor is on, for picking one.
    roster: usize,
    input: Composer,
    editing: bool,
    /// How many lines back the chat feed is scrolled from the newest.
    scroll: usize,
    /// Which agent column the list is sorted by, and which way round.
    sort_col: usize,
    sort_rev: bool,
    /// Whether the row under the cursor is open full-size. Memory values are
    /// paragraphs; a column in a table is not where one gets read.
    zoom: bool,
    /// A long answer to show in the box: `/help`, mostly. Separate from the
    /// memory the Memory pane zooms, because a command can be run from any
    /// pane and its answer still has to be readable.
    zoom_text: Option<String>,
    /// What this machine can do with sound, looked at once.
    audio: voice::Audio,
    /// The recorder, while it is running.
    recording: Option<(std::process::Child, std::path::PathBuf)>,
    /// Whether new messages are read aloud, and how far that has got.
    speaking: bool,
    spoken_up_to: i64,
    /// The player, while it is playing, so it can be stopped.
    saying: voice::Playing,
    /// Results from the work that must not block the screen: a transcription
    /// takes seconds, and a console that freezes while it waits looks broken.
    voice_tx: tokio::sync::mpsc::Sender<Heard>,
    /// Set when the camera should take the screen: the session a picture is
    /// for, and the agent it is for when the cursor was on one.
    camera_for: Option<(String, Option<String>)>,
    /// A question waiting for an answer: where to move a seat, what to call
    /// it, whether to really remove it. One at a time, deliberately.
    dialog: Option<act::Dialog>,
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

/// Something the voice side has finished doing.
pub enum Heard {
    /// Words, for the composer.
    Words(String),
    /// Anything worth a line in the footer.
    Note(String),
}

/// The first link in a message, if it has one.
///
/// A capability bridge answers with what the work produced, and a picture or
/// a video comes back as a URL. A terminal cannot show it -- so the console
/// shows the link, and `o` opens it in a browser, which is the honest
/// division of labour between a terminal and a page.
pub fn first_link(body: &str) -> Option<String> {
    let at = body.find("http://").or_else(|| body.find("https://"))?;
    let rest = &body[at..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', ']', '}']);
    (url.len() > 8).then(|| url.to_string())
}

/// Hand a URL to whatever the desktop uses. Never waits for it: `xdg-open`
/// on a machine with no browser blocks for ever, and a console that freezes
/// because somebody pressed a key is worse than one that does nothing.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
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

    /// Scroll the feed back, with a stop at the oldest message it holds.
    /// Two lines per message is a loose bound and that is the point: the
    /// draw clamps it to the real line count, and this only keeps PgUp from
    /// counting up for ever and PgDn from taking twenty presses to come back.
    fn scroll_back(&mut self, by: usize) {
        let ceiling = self.rows("messages").len() * 2 + 8;
        self.scroll = (self.scroll + by).min(ceiling);
    }

    fn say_sort(&mut self) {
        let col = draw::AGENT_COLS.get(self.sort_col).copied().unwrap_or("name");
        let way = if self.sort_rev { "descending" } else { "ascending" };
        self.notice(format!("agents by {col}, {way}"));
    }

    /// Every session, as a pick list. `without` leaves out the one a seat is
    /// already in, which is never a useful place to move it to.
    fn session_choices(&self, without: Option<&str>) -> Vec<act::Choice> {
        self.sessions()
            .iter()
            .filter_map(|x| {
                let name = x["name"].as_str()?.to_string();
                if without == Some(name.as_str()) {
                    return None;
                }
                let id = x["id"].as_str().unwrap_or("").to_string();
                let seats = x["slots"].as_i64().unwrap_or(0);
                Some(act::Choice {
                    value: if id.is_empty() { name.clone() } else { id },
                    label: format!("#{name}  ({seats} seat(s))"),
                })
            })
            .collect()
    }

    /// Every CLI any connected machine has installed, with the machines that
    /// have it. A seat for a CLI nothing can run is a seat that will never
    /// start, so the list comes from the fleet rather than from a guess.
    fn cli_choices(&self) -> Vec<act::Choice> {
        let mut by: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for n in self.nodes() {
            let name = n["name"].as_str().unwrap_or("?").to_string();
            for c in n["clis"].as_array().into_iter().flatten() {
                if let Some(c) = c.as_str() {
                    by.entry(c.to_string()).or_default().push(name.clone());
                }
            }
        }
        by.into_iter()
            .map(|(cli, on)| act::Choice {
                label: format!("{cli}  \u{2014} on {}", on.join(", ")),
                value: cli,
            })
            .collect()
    }

    /// Whether anything on the fleet offers a capability, from the seats'
    /// own lists -- the same question the dashboard asks before it draws a
    /// microphone.
    fn fleet_has(&self, cap: &str) -> bool {
        self.rows("agents").iter().any(|a| {
            a["capabilities"]
                .as_array()
                .is_some_and(|caps| {
                    caps.iter().any(|c| {
                        c.as_str().is_some_and(|c| c.eq_ignore_ascii_case(cap))
                    })
                })
                // An older hub does not send the list; the summary is what
                // there is.
                || (a["capabilities"].is_null()
                    && a["summary"].as_str().is_some_and(|s| s.contains(cap)))
        })
    }

    fn scratch(&self) -> std::path::PathBuf {
        std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("nexmesh-voice")
    }

    /// Press once to start, again to send. A fixed number of seconds is a
    /// guess nobody can make before they have said the sentence.
    fn listen(&mut self) {
        if !self.audio.can_hear() {
            self.notice(self.audio.why_not());
            return;
        }
        if !self.fleet_has("transcribe") {
            self.notice("nothing on the fleet offers 'transcribe' -- a capability bridge does");
            return;
        }
        match self.recording.take() {
            None => match voice::start(&self.audio, &self.scratch()) {
                Ok(r) => {
                    self.recording = Some(r);
                    self.notice("listening \u{2014} v again to send it to be transcribed");
                }
                Err(e) => self.notice(trim_err(&e.to_string())),
            },
            Some((child, path)) => {
                let bytes = match voice::stop(child, &path) {
                    Ok(b) => b,
                    Err(e) => {
                        self.notice(trim_err(&e.to_string()));
                        return;
                    }
                };
                self.notice(format!("transcribing {} KiB\u{2026}", bytes.len() / 1024));
                // On a task, so the screen keeps drawing while the fleet
                // does the work.
                let (base, client, tx) =
                    (self.base.clone(), self.client.clone(), self.voice_tx.clone());
                let lang = std::env::var("LANG").ok().and_then(|l| {
                    l.split(['.', '_']).next().filter(|s| s.len() == 2).map(|s| s.to_string())
                });
                tokio::spawn(async move {
                    let said = voice::transcribe(&base, &client, &bytes, lang.as_deref()).await;
                    let _ = match said {
                        Ok(words) if !words.trim().is_empty() => tx.send(Heard::Words(words)).await,
                        Ok(_) => tx.send(Heard::Note("nothing was said in that".into())).await,
                        Err(e) => tx.send(Heard::Note(trim_err(&format!("{e:#}")))).await,
                    };
                });
            }
        }
    }

    /// Stop whatever is being said, now.
    fn hush(&mut self) {
        voice::hush(&self.saying);
        self.speaking = false;
    }

    /// Read new traffic aloud, in the channel being watched. Off unless
    /// asked: a console that starts talking is a console you close.
    fn read_aloud(&mut self) {
        if !self.speaking {
            return;
        }
        let mut msgs: Vec<Value> = self
            .rows("messages")
            .into_iter()
            .filter(|m| m["id"].as_i64().unwrap_or(0) > self.spoken_up_to)
            .filter(|m| self.pane != Pane::Chat || self.in_channel(m))
            .collect();
        if msgs.is_empty() {
            return;
        }
        msgs.sort_by_key(|m| m["id"].as_i64().unwrap_or(0));
        self.spoken_up_to = msgs
            .iter()
            .filter_map(|m| m["id"].as_i64())
            .max()
            .unwrap_or(self.spoken_up_to);
        let fleet = self.fleet_has("tts");
        for m in msgs {
            let from = m["from"].as_str().unwrap_or("someone").to_string();
            let body = m["body"].as_str().unwrap_or("").to_string();
            if body.trim().is_empty() {
                continue;
            }
            let line = format!("{from} says: {body}");
            if !fleet {
                self.notice("nothing on the fleet offers 'tts'");
                self.speaking = false;
                return;
            }
            // The speaker's own voice, when their seat has been given one
            // with `/voice`. A team read aloud in one voice is a wall of
            // text with a sound card.
            let voice = self
                .rows("agents")
                .into_iter()
                .find(|a| a["name"].as_str() == Some(from.as_str()))
                .and_then(|a| a["voice"].as_str().map(|v| v.to_string()))
                .filter(|v| !v.trim().is_empty());
            let (base, client, tx, audio, dir, playing) = (
                self.base.clone(),
                self.client.clone(),
                self.voice_tx.clone(),
                self.audio.clone(),
                self.scratch(),
                self.saying.clone(),
            );
            tokio::spawn(async move {
                if let Err(e) =
                    voice::speak(&base, &client, &audio, &line, voice.as_deref(), &dir, &playing)
                        .await
                {
                    let _ = tx.send(Heard::Note(trim_err(&format!("{e:#}")))).await;
                }
            });
        }
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
        let text = self.input.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // A command goes to ONE session -- the one the chat is on -- because
        // `/remove Dev` typed into #all would otherwise mean it in every
        // team at once. #all with a command sends it to the session the
        // cursor is on, and says which.
        let command = text.starts_with('/') && !text.starts_with("//");
        let targets: Vec<String> = if self.chan == "*" {
            if command {
                self.channels()
                    .get(self.cursor().max(1))
                    .map(|(id, _)| vec![id.clone()])
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|id| id != "*")
                    .collect()
            } else {
                self.sessions()
                    .iter()
                    .filter_map(|s| s["id"].as_str().map(|x| x.to_string()))
                    .collect()
            }
        } else {
            vec![self.chan.clone()]
        };
        if targets.is_empty() {
            self.notice(if command {
                "pick a channel first: a command needs one session"
            } else {
                "no session to send to"
            });
            return;
        }
        self.input.clear();
        self.scroll = 0;
        let mut failed = 0;
        let mut refused: Option<String> = None;
        let mut said: Option<String> = None;
        for t in &targets {
            let url = format!("{}/v1/sessions/{}/messages", self.base, urlish(t));
            // In a private channel every line is addressed to that teammate.
            // A command is not: `/rename` is housekeeping, and sending it to
            // one agent would be asking that agent to rename itself.
            let mut body = serde_json::json!({ "text": text });
            if !command {
                if let Some(who) = &self.with_them {
                    body["to"] = serde_json::json!(who);
                }
            }
            match crate::post(&self.client, url, body).await {
                Ok(v) => match v.get("error").and_then(|e| e.as_str()) {
                    // The hub refuses a mistyped command with what it should
                    // have been; saying "could not send that" instead threw
                    // away the only useful part of the answer.
                    Some(e) => {
                        failed += 1;
                        refused = Some(e.to_string());
                    }
                    None => {
                        if let Some(s) = v.get("said").and_then(|s| s.as_str()) {
                            said = Some(s.to_string());
                        }
                    }
                },
                Err(e) => {
                    failed += 1;
                    refused = Some(trim_err(&e.to_string()));
                }
            }
        }
        if failed == targets.len() {
            self.input.set(text);
            self.notice(
                refused.unwrap_or_else(|| "could not send that".to_string()),
            );
        } else if failed > 0 {
            self.notice(format!("sent, except to {failed} session(s)"));
        } else if let Some(s) = said {
            // A command's answer, which may be the whole help.
            self.notice(s.lines().next().unwrap_or("done").to_string());
            if s.lines().count() > 1 {
                self.zoom_text = Some(s);
                self.zoom = true;
            }
        } else if targets.len() > 1 {
            self.notice(format!("sent to all {} sessions", targets.len()));
        }
        self.fetch().await;
    }

    /// Agents in the order the pane shows them, which is the order the
    /// cursor counts in: sorting the drawn rows and not these would attach
    /// to whichever agent used to be on that line.
    pub fn agents_in_order(&self) -> Vec<Value> {
        let mut rows = self.rows("agents");
        draw::sort_agents(&mut rows, self.sort_col, self.sort_rev);
        rows
    }

    /// Whether a message belongs to the channel the chat pane is on.
    pub fn in_channel(&self, m: &Value) -> bool {
        self.chan == "*" || {
            let name = m["session"].as_str().unwrap_or("");
            self.sessions().iter().any(|x| {
                x["id"].as_str() == Some(self.chan.as_str())
                    && x["name"].as_str() == Some(name)
            })
        }
    }

    /// The session name behind the current channel, or `None` for `#all`.
    /// Who is in the room on screen, in the order the panel lists them.
    pub fn room(&self) -> Vec<serde_json::Value> {
        let room = self.channel_session();
        self.rows("agents")
            .into_iter()
            .filter(|a| {
                room.as_deref()
                    .is_none_or(|r| a["session"].as_str().unwrap_or_default() == r)
            })
            .collect()
    }

    /// The name under the room cursor.
    fn room_pick(&self) -> Option<String> {
        let here = self.room();
        here.get(self.roster.min(here.len().saturating_sub(1)))
            .and_then(|a| a["name"].as_str().map(|x| x.to_string()))
    }

    /// The agent id behind that name, for attaching to its terminal: the
    /// seat's own process when it has one, else the pane it sits in.
    fn room_pick_agent(&self) -> Option<(String, String)> {
        let here = self.room();
        let a = here.get(self.roster.min(here.len().saturating_sub(1)))?;
        let id = a["attach_agent_id"]
            .as_str()
            .or_else(|| a["agent_id"].as_str())?;
        Some((id.to_string(), a["name"].as_str().unwrap_or("agent").to_string()))
    }

    pub fn channel_session(&self) -> Option<String> {
        if self.chan == "*" {
            return None;
        }
        self.sessions()
            .iter()
            .find(|x| x["id"].as_str() == Some(self.chan.as_str()))
            .and_then(|x| x["name"].as_str())
            .map(|x| x.to_string())
    }

    /// The memory row under the cursor, for the detail box.
    pub fn selected_memory(&self) -> Option<Value> {
        self.rows("knowledge").get(self.sel[Pane::Memory.index()]).cloned()
    }

    /// Up and down in the chat move between channels, typing or not.
    fn step_channel(&mut self, by: i64) {
        let chans = self.channels();
        if chans.is_empty() {
            return;
        }
        let at = chans.iter().position(|(id, _)| *id == self.chan).unwrap_or(0) as i64;
        let n = chans.len() as i64;
        let next = ((at + by) % n + n) % n;
        self.chan = chans[next as usize].0.clone();
        self.sel[Pane::Chat.index()] = next as usize;
        self.scroll = 0;
    }

    /// The seat the cursor is on: (seat id, name, session name, has a live
    /// process). Everything an action needs to say what it is about to do.
    fn seat_under_cursor(&self) -> Option<(String, String, String, bool)> {
        let rows = self.agents_in_order();
        let a = rows.get(self.sel[Pane::Agents.index()])?;
        let slot = a["slot_id"].as_str()?.to_string();
        let name = a["name"].as_str().unwrap_or(&slot).to_string();
        let session = a["session"].as_str().unwrap_or("").to_string();
        let live = matches!(a["status"].as_str(), Some("running") | Some("waiting"));
        Some((slot, name, session, live))
    }

    /// A name nobody in the fleet is using yet, from the CLI's own name:
    /// `Claude`, then `Claude-2`. A seat named after what it runs is what
    /// every other part of nexmesh suggests, and a name already taken is
    /// refused by the hub rather than silently doubled.
    fn free_seat_name(&self, cli: &str) -> String {
        let short = cli.split('-').next().unwrap_or(cli);
        let mut ch = short.chars();
        let base = match ch.next() {
            Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
            None => "Seat".to_string(),
        };
        let taken: Vec<String> = self
            .rows("agents")
            .iter()
            .filter_map(|a| a["name"].as_str().map(|x| x.to_string()))
            .collect();
        crate::runcmd::free_name(&base, &taken)
    }

    /// The session the console is narrowed to, by name, or `None` for all.
    fn scope_session(&self) -> Option<String> {
        if self.scope == "*" {
            return None;
        }
        self.sessions()
            .iter()
            .find(|x| x["id"].as_str() == Some(self.scope.as_str()))
            .and_then(|x| x["name"].as_str())
            .map(|x| x.to_string())
            .or_else(|| Some(self.scope.clone()))
    }

    /// The agent the cursor is on, as (session, name) -- what `attach` wants.
    fn selected_agent(&self) -> Option<(String, String)> {
        let rows = self.agents_in_order();
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
    let (voice_tx, mut voice_rx) = tokio::sync::mpsc::channel::<Heard>(16);
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
        with_them: None,
        roster: 0,
        input: Composer::default(),
        editing: false,
        scroll: 0,
        sort_col: 0,
        sort_rev: false,
        zoom: false,
        zoom_text: None,
        camera_for: None,
        audio: voice::detect(),
        recording: None,
        speaking: false,
        spoken_up_to: 0,
        saying: Default::default(),
        voice_tx,
        dialog: None,
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
                // The node keeps 256 KiB of scrollback per agent; ask for
                // all of it, so attaching from the console shows what has
                // been happening rather than the last few lines of it.
                &app.base, &app.client, &session, &who, 262_144,
                app.ca_cert.as_deref(), id,
            )
            .await;
            if let Err(e) = enter() {
                break Err(e);
            }
            if let Err(e) = term.clear() {
                break Err(e.into());
            }
            // The viewer hands back why it ended -- a detach, an agent that
            // exited, a refusal -- and the console says it on its own screen.
            // Printed by the viewer it landed under the next redraw, so a
            // refused attach looked like a key that did nothing.
            match r {
                // One line: the status row is one row. The viewer's second
                // line is advice for a terminal you can scroll back in.
                Ok(reason) => app.notice(format!(
                    "{who}: {}",
                    reason.lines().next().unwrap_or("detached")
                )),
                Err(e) => app.notice(trim_err(&e.to_string())),
            }
            app.fetch().await;
        }

        // The camera takes the whole screen, like an attach: the console
        // puts its own terminal back first and picks it up again after.
        if let Some((session, to)) = app.camera_for.take() {
            if let Err(e) = leave() {
                break Err(e);
            }
            let cam = crate::camera::detect();
            let dir = app.scratch();
            let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
            let mut kb = crate::Keyboard::open();
            let took = crate::camui::run(&mut kb, &cam, &dir, cols, rows).await;
            drop(kb);
            if let Err(e) = enter() {
                break Err(e);
            }
            if let Err(e) = term.clear() {
                break Err(e.into());
            }
            match took {
                Ok(crate::camui::Took::File(path, what)) => {
                    let say = format!("A {what} for the team to look at, taken just now.");
                    let r = crate::send_file(
                        &app.base, &app.client, &session, &path, to.as_deref(), Some(&say),
                    )
                    .await;
                    let _ = std::fs::remove_file(&path);
                    match r {
                        Ok(said) => app.notice(said),
                        Err(e) => app.notice(trim_err(&format!("{e:#}"))),
                    }
                }
                Ok(crate::camui::Took::Failed(why)) => app.notice(why),
                Ok(crate::camui::Took::Nothing) => {}
                Err(e) => app.notice(trim_err(&e.to_string())),
            }
            app.fetch().await;
        }

        if app.last_fetch.elapsed() >= Duration::from_secs(1) {
            app.fetch().await;
            app.read_aloud();
        }
        if let Err(e) = term.draw(|f| draw::all(f, &app)) {
            break Err(e.into());
        }

        tokio::select! {
            _ = ticker.tick() => {}
            heard = voice_rx.recv() => {
                match heard {
                    Some(Heard::Words(text)) => {
                        app.pane = Pane::Chat;
                        for c in text.chars() {
                            app.input.insert(c);
                        }
                        app.editing = true;
                        app.notice("transcribed; Enter sends it");
                    }
                    Some(Heard::Note(n)) => app.notice(n),
                    None => {}
                }
            }
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
    execute!(
        stdout(),
        EnterAlternateScreen,
        crossterm::cursor::Hide,
        // Without this the wheel does nothing here at all. A full-screen
        // program is on the alternate screen, where the terminal has no
        // scrollback to give you, so unless the console asks to be told
        // about the mouse and scrolls itself, turning the wheel is silence.
        //
        // The cost is that the terminal stops handling the mouse: dragging
        // to select text selects nothing while this is running. Every
        // terminal worth using bypasses that with SHIFT held down, which is
        // what the footer says.
        crossterm::event::EnableMouseCapture,
    )?;
    Ok(())
}

/// One notch of the wheel, on whatever is on the screen.
///
/// The wheel means "move the content", which is a different thing in each
/// pane: a list moves its cursor, a room of text moves its scrollback, and an
/// opened memory moves the paragraph. Three lines per notch rather than the
/// ten a PgUp moves, because a wheel is turned repeatedly and a page at a
/// time overshoots.
///
/// A question on the screen owns the keyboard, and the wheel with it.
fn wheel(app: &mut App, up: bool) {
    const STEP: usize = 3;
    if app.dialog.is_some() {
        return;
    }
    // An opened memory is a paragraph being read: the wheel moves the text,
    // not the row behind it.
    if app.zoom {
        app.scroll = if up { app.scroll.saturating_add(STEP) } else { app.scroll.saturating_sub(STEP) };
        return;
    }
    match app.pane {
        // The chat scrolls its room. Up the wheel goes back through what was
        // said, which is the direction PgUp goes.
        Pane::Chat | Pane::Deck => {
            if up {
                app.scroll_back(STEP);
            } else {
                app.scroll = app.scroll.saturating_sub(STEP);
            }
        }
        // Everything else is a list, and a list scrolls by moving its cursor:
        // the view follows it.
        _ => {
            let n = app.len();
            if n == 0 {
                return;
            }
            let c = if up {
                app.cursor().saturating_sub(STEP)
            } else {
                (app.cursor() + STEP).min(n - 1)
            };
            app.set_cursor(c);
            app.scroll = 0;
        }
    }
}

fn leave() -> Result<()> {
    let mut out = stdout();
    let _ = out.execute(crossterm::event::DisableMouseCapture);
    let _ = out.execute(crossterm::cursor::Show);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = disable_raw_mode();
    Ok(())
}

async fn handle(app: &mut App, ev: Event) -> Result<()> {
    if let Event::Mouse(m) = ev {
        match m.kind {
            crossterm::event::MouseEventKind::ScrollUp => wheel(app, true),
            crossterm::event::MouseEventKind::ScrollDown => wheel(app, false),
            _ => {}
        }
        return Ok(());
    }
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

    // A question on the screen owns the keyboard until it is answered: a
    // stray `q` while being asked whether to remove a seat should not quit
    // the console, and must certainly not remove the seat.
    if app.dialog.is_some() {
        return dialog_key(app, k.code, ctrl).await;
    }

    // While typing, printable keys are text -- but moving around is not
    // typing. Having to press Esc to read the channel above, or to look at
    // another pane, and then `i` again to carry on, made the composer a
    // mode you got stuck in; everything that is not a character still works.
    if app.editing {
        match k.code {
            KeyCode::Esc => app.editing = false,
            KeyCode::Enter => app.send().await,
            KeyCode::Backspace => app.input.backspace(),
            KeyCode::Delete => app.input.delete(),
            KeyCode::Left => app.input.left(),
            KeyCode::Right => app.input.right(),
            KeyCode::Home => app.input.home(),
            KeyCode::End => app.input.end(),
            KeyCode::Up => app.step_channel(-1),
            KeyCode::Down => app.step_channel(1),
            KeyCode::PageUp => app.scroll_back(10),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
            // Leaving the chat leaves the composer: the keys in another pane
            // are not text, and what was typed is still here on the way back.
            KeyCode::Tab | KeyCode::BackTab => {
                app.editing = false;
                let n = Pane::ALL.len();
                let step = if k.code == KeyCode::Tab { 1 } else { n - 1 };
                app.pane = Pane::ALL[(app.pane.index() + step) % n];
            }
            // Dictation into the composer, without leaving it.
            KeyCode::Char('v') if ctrl => app.listen(),
            KeyCode::Char('u') if ctrl => app.input.clear(),
            KeyCode::Char('w') if ctrl => app.input.delete_word(),
            KeyCode::Char('a') if ctrl => app.input.home(),
            KeyCode::Char('e') if ctrl => app.input.end(),
            KeyCode::Char(c) if !ctrl => app.input.insert(c),
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
            } else if app.zoom {
                app.zoom = false;
                app.zoom_text = None;
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
        // In the chat the cursor IS the channel: moving it and then having
        // to press Enter to actually go there was a step nobody wants.
        // In the chat, j/k are the CHANNELS and the arrows are the ROOM: two
        // lists on one screen, and the one you are picking a teammate from
        // is the one under the arrows.
        KeyCode::Char('j') if app.pane == Pane::Chat => app.step_channel(1),
        KeyCode::Char('k') if app.pane == Pane::Chat => app.step_channel(-1),
        KeyCode::Down if app.pane == Pane::Chat => {
            let n = app.room().len();
            if n > 0 {
                app.roster = (app.roster + 1).min(n - 1);
            }
        }
        KeyCode::Up if app.pane == Pane::Chat => {
            app.roster = app.roster.saturating_sub(1);
        }
        // Enter on a name is a private channel with that teammate; Enter
        // again is back to the room.
        KeyCode::Enter if app.pane == Pane::Chat && !app.editing => {
            if app.with_them.is_some() {
                app.with_them = None;
                app.notice("back to the room");
            } else if let Some(who) = app.room_pick() {
                app.notice(format!("talking to {who} alone -- Enter goes back"));
                app.with_them = Some(who);
            }
            app.scroll = 0;
        }
        // And the keyboard of whoever is picked, which is the other thing
        // you want from a name in a chat.
        KeyCode::Char('a') if app.pane == Pane::Chat => {
            match app.room_pick_agent() {
                Some((agent, name)) => app.attach_to = Some((agent, name)),
                None => app.notice("nobody there has a terminal"),
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.len();
            if n > 0 {
                let c = (app.cursor() + 1).min(n - 1);
                app.set_cursor(c);
            }
            app.scroll = 0;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let c = app.cursor().saturating_sub(1);
            app.set_cursor(c);
            app.scroll = 0;
        }
        KeyCode::PageUp if app.zoom => app.scroll = app.scroll.saturating_sub(10),
        KeyCode::PageDown if app.zoom => {
            // Bounded by the value itself, so PgUp comes straight back.
            let bound = app
                .selected_memory()
                .and_then(|k| k["value"].as_str().map(|v| v.len() / 20 + 10))
                .unwrap_or(10);
            app.scroll = (app.scroll + 10).min(bound);
        }
        KeyCode::PageUp => app.scroll_back(10),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
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
        // A picture for the team, from this machine's camera.
        KeyCode::Char('P') => {
            let cam = crate::camera::detect();
            if !cam.ready() {
                app.notice(cam.why_not());
            } else {
                // To the agent under the cursor when there is one, so it
                // lands on that agent's machine rather than anywhere.
                let to = app.seat_under_cursor().map(|(_, name, _, _)| name);
                let session = match (&to, app.scope_session()) {
                    (Some(_), _) => app
                        .agents_in_order()
                        .iter()
                        .find(|a| a["name"].as_str().map(|n| n.to_string()) == to)
                        .and_then(|a| a["session"].as_str().map(|s| s.to_string())),
                    (None, s) => s,
                };
                match session {
                    Some(session) => app.camera_for = Some((session, to)),
                    None => app.notice("pick a session first: s narrows the console to one"),
                }
            }
        }
        // Say it rather than type it, and have it read back.
        KeyCode::Char('v') => app.listen(),
        KeyCode::Char('V') => {
            if !app.audio.can_speak() {
                app.notice("nothing on this machine can play sound");
            } else if app.speaking && app.saying.lock().map(|s| s.is_some()).unwrap_or(false) {
                // Talking right now: V is a stop. A voice you cannot
                // interrupt is one you turn off and never turn on again.
                app.hush();
                app.notice("stopped talking");
            } else {
                app.speaking = !app.speaking;
                // From here, not from the backlog.
                app.spoken_up_to = app
                    .rows("messages")
                    .iter()
                    .filter_map(|m| m["id"].as_i64())
                    .max()
                    .unwrap_or(0);
                app.notice(if app.speaking {
                    "reading new messages aloud"
                } else {
                    "stopped reading aloud"
                });
            }
        }
        KeyCode::Char(',') | KeyCode::Char('<') if app.pane == Pane::Agents => {
            let n = draw::AGENT_COLS.len();
            app.sort_col = (app.sort_col + n - 1) % n;
            app.say_sort();
        }
        KeyCode::Char('.') | KeyCode::Char('>') if app.pane == Pane::Agents => {
            app.sort_col = (app.sort_col + 1) % draw::AGENT_COLS.len();
            app.say_sort();
        }
        // Doing something about a seat, rather than only looking at it.
        KeyCode::Char('m') if app.pane == Pane::Agents => {
            if let Some((slot, name, session, _)) = app.seat_under_cursor() {
                let options = app.session_choices(Some(&session));
                if options.is_empty() {
                    app.notice("there is no other session to move it to");
                } else {
                    app.dialog = Some(act::Dialog::pick(
                        format!("move {name}"),
                        format!("it is in #{session} now"),
                        act::Step::Move { slot, name },
                        options,
                    ));
                }
            }
        }
        KeyCode::Char('e') if app.pane == Pane::Agents => {
            if let Some((slot, name, _, _)) = app.seat_under_cursor() {
                let prefill = name.clone();
                app.dialog = Some(act::Dialog::text(
                    format!("rename {name}"),
                    "teammates address it by this name",
                    act::Step::Rename { slot, name },
                    &prefill,
                ));
            }
        }
        KeyCode::Char('x') if app.pane == Pane::Agents => {
            if let Some((slot, name, session, live)) = app.seat_under_cursor() {
                app.dialog = Some(act::Dialog::confirm(
                    format!("remove {name}"),
                    if live {
                        format!(
                            "it is RUNNING in #{session}. Removing the seat kills nothing -- \
                             the process is left where it is, orphaned."
                        )
                    } else {
                        format!("the seat and its history leave #{session}. This cannot be undone.")
                    },
                    act::Step::Remove { slot, name, live },
                ));
            }
        }
        KeyCode::Char('S') if app.pane == Pane::Agents => {
            if let Some((slot, name, _, live)) = app.seat_under_cursor() {
                if live {
                    app.notice(format!("{name} already has a process; a is attach"));
                } else {
                    let mut modes = act::modes();
                    modes.truncate(2);
                    app.dialog = Some(act::Dialog::pick(
                        format!("start {name}"),
                        "the seat keeps its conversation; this starts a process for it",
                        act::Step::Start { slot, name },
                        modes,
                    ));
                }
            }
        }
        KeyCode::Char('n') if app.pane == Pane::Agents || app.pane == Pane::Fleet => {
            // Which team, unless the console is already looking at one.
            match app.scope_session() {
                Some(session) => {
                    let clis = app.cli_choices();
                    if clis.is_empty() {
                        app.notice("no machine has reported a CLI it can run");
                    } else {
                        app.dialog = Some(act::Dialog::pick(
                            "new seat: which CLI",
                            format!("it will join #{session}"),
                            act::Step::NewCli { session },
                            clis,
                        ));
                    }
                }
                None => {
                    let options = app.session_choices(None);
                    if options.is_empty() {
                        app.notice("there is no session yet: nexmesh team <name>");
                    } else {
                        app.dialog = Some(act::Dialog::pick(
                            "new seat: which session",
                            "s narrows the console to one, and then this step is skipped",
                            act::Step::NewSession,
                            options,
                        ));
                    }
                }
            }
        }
        KeyCode::Char('R') if app.pane == Pane::Agents => {
            app.sort_rev = !app.sort_rev;
            app.say_sort();
        }
        KeyCode::Char('o') => {
            // The newest link in what is on screen. A terminal cannot show a
            // picture; it can hand you the one thing that can.
            let link = app
                .rows("messages")
                .iter()
                .filter(|m| {
                    app.pane != Pane::Chat
                        || app.chan == "*"
                        || app.sessions().iter().any(|x| {
                            x["id"].as_str() == Some(app.chan.as_str())
                                && x["name"].as_str() == m["session"].as_str()
                        })
                })
                .find_map(|m| m["body"].as_str().and_then(first_link));
            match link {
                Some(url) => match open_in_browser(&url) {
                    Ok(()) => app.notice(format!("opened {url}")),
                    Err(e) => app.notice(format!("could not open it: {e}")),
                },
                None => app.notice("nothing on screen has a link in it"),
            }
        }
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
            // A memory is a paragraph in a column six words wide. Enter
            // opens the whole of it, Esc or Enter again puts it away.
            Pane::Memory => {
                app.zoom_text = None;
                app.zoom = !app.zoom;
                app.scroll = 0;
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

/// The keys of a dialog, and the one place a flow steps forward.
///
/// Each answer either asks the next question or does the thing. Nothing is
/// done on the way through: a half-answered creation leaves no seat behind.
async fn dialog_key(app: &mut App, code: KeyCode, ctrl: bool) -> Result<()> {
    let Some(d) = app.dialog.as_mut() else { return Ok(()) };
    match code {
        KeyCode::Esc => {
            app.dialog = None;
            app.notice("left alone");
            return Ok(());
        }
        KeyCode::Down | KeyCode::Tab => {
            d.down();
            return Ok(());
        }
        KeyCode::Up | KeyCode::BackTab => {
            d.up();
            return Ok(());
        }
        KeyCode::Char('n') | KeyCode::Char('N') if matches!(d.kind, act::Kind::Confirm) => {
            app.dialog = None;
            app.notice("left alone");
            return Ok(());
        }
        KeyCode::Char('y') | KeyCode::Char('Y') if matches!(d.kind, act::Kind::Confirm) => {}
        KeyCode::Enter => {}
        // A list is walked with j and k everywhere else in the console.
        KeyCode::Char('j') if matches!(d.kind, act::Kind::Pick(_)) => {
            d.down();
            return Ok(());
        }
        KeyCode::Char('k') if matches!(d.kind, act::Kind::Pick(_)) => {
            d.up();
            return Ok(());
        }
        other => {
            // Anything else is text, if this dialog takes any.
            if let Some(c) = d.composer_mut() {
                match other {
                    KeyCode::Backspace => c.backspace(),
                    KeyCode::Delete => c.delete(),
                    KeyCode::Left => c.left(),
                    KeyCode::Right => c.right(),
                    KeyCode::Home => c.home(),
                    KeyCode::End => c.end(),
                    KeyCode::Char('u') if ctrl => c.clear(),
                    KeyCode::Char('w') if ctrl => c.delete_word(),
                    KeyCode::Char(ch) if !ctrl => c.insert(ch),
                    _ => {}
                }
            }
            return Ok(());
        }
    }

    // Answered. Either the next question, or the deed.
    let step = d.step.clone();
    let answer = match &d.kind {
        act::Kind::Pick(_) => d.chosen().map(|c| c.value.clone()).unwrap_or_default(),
        act::Kind::Text(_) => d.typed(),
        act::Kind::Confirm => "yes".to_string(),
    };
    let label = d.chosen().map(|c| c.label.clone()).unwrap_or_default();
    app.dialog = None;

    match step {
        act::Step::NewSession => {
            let clis = app.cli_choices();
            if clis.is_empty() {
                app.notice("no machine has reported a CLI it can run");
                return Ok(());
            }
            app.dialog = Some(act::Dialog::pick(
                "new seat: which CLI",
                format!("it will join {}", label.trim()),
                act::Step::NewCli { session: answer },
                clis,
            ));
        }
        act::Step::NewCli { session } => {
            let suggestion = app.free_seat_name(&answer);
            app.dialog = Some(act::Dialog::text(
                "new seat: what to call it",
                "teammates address it by this name",
                act::Step::NewName { session, cli: answer },
                &suggestion,
            ));
        }
        act::Step::NewName { session, cli } => {
            if answer.is_empty() {
                app.notice("a seat needs a name");
                return Ok(());
            }
            app.dialog = Some(act::Dialog::pick(
                format!("start {answer}?"),
                format!("{cli} in #{session}"),
                act::Step::NewStart { session, cli, name: answer },
                act::modes(),
            ));
        }
        done => {
            let base = app.base.clone();
            let client = app.client.clone();
            match act::perform(&base, &client, &done, &answer).await {
                Ok(said) => app.notice(said),
                Err(e) => app.notice(trim_err(&e.to_string())),
            }
            app.fetch().await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An App with nothing behind it, for the parts that are pure state.
    fn bare_app() -> App {
        let (voice_tx, _rx) = tokio::sync::mpsc::channel(1);
        App {
            base: "http://127.0.0.1:1".into(),
            client: reqwest::Client::new(),
            ca_cert: None,
            identity: None,
            snap: None,
            err: None,
            note: None,
            pane: Pane::Deck,
            sel: [0; 8],
            ws: 0,
            chan: "*".into(),
            with_them: None,
            roster: 0,
            input: Composer::default(),
            editing: false,
            scroll: 0,
            sort_col: 0,
            sort_rev: false,
            zoom: false,
            zoom_text: None,
            camera_for: None,
            audio: voice::Audio::default(),
            recording: None,
            speaking: false,
            spoken_up_to: 0,
            saying: Default::default(),
            voice_tx,
            dialog: None,
            panel: true,
            help: false,
            theme: Theme::Deck,
            scope: "*".into(),
            last_fetch: Instant::now(),
            quit: false,
            attach_to: None,
        }
    }

    /// The wheel has to DO something. On the alternate screen the terminal
    /// has no scrollback to offer, so a console that ignores mouse events
    /// is a console where scrolling is silence -- which is exactly what it
    /// was, twice.
    #[test]
    fn the_wheel_scrolls_the_chat_and_knows_which_way_is_back() {
        let mut app = bare_app();
        app.pane = Pane::Chat;
        wheel(&mut app, true);
        assert!(app.scroll > 0, "up the wheel goes BACK through what was said");
        let back = app.scroll;
        wheel(&mut app, false);
        assert!(app.scroll < back, "down comes forward again");
    }

    /// A list scrolls by moving its cursor, and must not run off either end.
    #[test]
    fn the_wheel_moves_a_list_without_leaving_it() {
        let mut app = bare_app();
        app.pane = Pane::Agents;
        app.snap = Some(serde_json::json!({ "detail": {
            "agents": (0..10).map(|i| serde_json::json!({ "name": format!("a{i}") }))
                             .collect::<Vec<_>>()
        }}));
        wheel(&mut app, false);
        assert_eq!(app.cursor(), 3);
        for _ in 0..20 {
            wheel(&mut app, false);
        }
        assert_eq!(app.cursor(), 9, "stops at the last row, never past it");
        for _ in 0..20 {
            wheel(&mut app, true);
        }
        assert_eq!(app.cursor(), 0, "and at the first");
    }

    /// A question on the screen owns the keyboard, and the wheel with it:
    /// scrolling the list behind a dialog moves a cursor the dialog is
    /// about to act on.
    #[test]
    fn a_dialog_takes_the_wheel_too() {
        let mut app = bare_app();
        app.pane = Pane::Chat;
        app.dialog = Some(act::Dialog::confirm(
            "remove the seat?", "y / n", act::Step::NewSession));
        wheel(&mut app, true);
        assert_eq!(app.scroll, 0);
    }

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
    fn the_composer_edits_where_the_caret_is() {
        let mut c = Composer::default();
        for ch in "helo world".chars() {
            c.insert(ch);
        }
        assert_eq!(c.caret, 10);
        // Back to the typo and fix it, without retyping the rest.
        for _ in 0..7 {
            c.left();
        }
        assert_eq!(c.caret, 3);
        c.insert('l');
        assert_eq!(c.text, "hello world");
        c.end();
        c.backspace();
        assert_eq!(c.text, "hello worl");
        c.home();
        c.delete();
        assert_eq!(c.text, "ello worl");
        assert_eq!(c.caret, 0);
        // Left at the start and right at the end go nowhere rather than wrap.
        c.left();
        assert_eq!(c.caret, 0);
        c.end();
        c.right();
        assert_eq!(c.caret, c.text.chars().count());
    }

    #[test]
    fn an_accent_does_not_take_the_console_down() {
        // The caret counts characters; the string is indexed in bytes. A
        // composer that confuses the two panics on the first "perché".
        let mut c = Composer::default();
        for ch in "perché".chars() {
            c.insert(ch);
        }
        assert_eq!(c.caret, 6);
        c.backspace();
        assert_eq!(c.text, "perch");
        for ch in "é sì".chars() {
            c.insert(ch);
        }
        c.home();
        c.delete();
        assert_eq!(c.text, "erché sì");
    }

    #[test]
    fn ctrl_w_takes_the_last_word_and_its_spaces() {
        let mut c = Composer::default();
        c.set("deploy the hub   ".to_string());
        c.delete_word();
        assert_eq!(c.text, "deploy the ");
        c.delete_word();
        assert_eq!(c.text, "deploy ");
        c.delete_word();
        assert_eq!(c.text, "");
        // And on an empty line it is not an error.
        c.delete_word();
        assert_eq!(c.caret, 0);
    }

    #[test]
    fn a_failed_send_hands_the_line_back_with_the_caret_at_the_end() {
        let mut c = Composer::default();
        c.set("the hub said no".to_string());
        assert_eq!(c.caret, 15);
        c.clear();
        assert_eq!((c.text.as_str(), c.caret), ("", 0));
    }

    #[test]
    fn a_theme_round_trips_through_the_preferences_file() {
        assert_eq!(Theme::from_name(Theme::Lcars.name()), Theme::Lcars);
        assert_eq!(Theme::from_name(Theme::Deck.name()), Theme::Deck);
    }
}
