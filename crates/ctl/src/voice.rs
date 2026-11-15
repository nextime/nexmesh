// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Talking to the team, and being read to, from a terminal.
//!
//! The dashboard has had this since it was a page on a phone: press the
//! microphone, say the sentence, the fleet transcribes it. The console had
//! nothing, which is backwards -- the console is what runs on the machine
//! with the microphone attached to it.
//!
//! Two halves, and neither is this program's work:
//!
//! * the words come from, and go to, whichever teammate advertises
//!   `transcribe` and `tts` -- the capability bridge, normally -- through the
//!   same operator ask the dashboard uses;
//! * the sound itself is recorded and played by whatever the machine already
//!   has: PulseAudio, ALSA, sox or ffmpeg, in that order of preference.
//!
//! Nothing is bundled and nothing is required. A machine with no recorder
//! simply has no microphone key, said out loud in the footer rather than
//! left as a key that does nothing.

use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The keys an attach may intercept, and when.
///
/// Nothing is taken while command mode is OFF: every control character goes
/// to the agent, so a CLI that wants Ctrl-A, Ctrl-R or Ctrl-W gets them.
/// Turning the mode ON makes three of them nexmesh's -- and only three, so
/// you can leave it on and carry on typing into the CLI underneath.
///
/// `Ctrl-[` would have been the obvious partner for `Ctrl-]`, and is the one
/// key that absolutely cannot be used: it IS Escape. Taking it would break
/// every arrow key and every TUI inside the attach. `Ctrl-\` is its
/// neighbour, is almost never bound by a terminal program, and is what a
/// shell would otherwise turn into SIGQUIT -- which an attach does not pass
/// on anyway.
///
/// The defaults avoid the line-editing keys on purpose: Ctrl-A, Ctrl-E,
/// Ctrl-K, Ctrl-U, Ctrl-W and Ctrl-R are what a prompt is driven with, and a
/// mode you cannot type in is a mode you turn off. All of them are
/// configurable, from `~/.nexmesh/console.toml`:
///
/// ```toml
/// command_key = "ctrl-\\"   # into and out of command mode
/// detach_key  = "ctrl-]"    # always, in either mode
/// dictate_key = "ctrl-g"    # in command mode: say it instead of typing it
/// read_key    = "ctrl-o"    # in command mode: read a reply back
/// aloud_key   = "ctrl-t"    # in command mode: read every new one
/// send_key    = "ctrl-y"    # in command mode: send a file from here
/// photo_key   = "ctrl-p"    # in command mode: a photo from this camera
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    pub command: u8,
    pub detach: u8,
    pub dictate: u8,
    pub read: u8,
    pub aloud: u8,
    /// Send a file from HERE to the machine the agent is on.
    pub send: u8,
    /// Take a photo with this machine's camera and send that.
    pub photo: u8,
    /// Hands-free: keep listening, a sentence at a time, until told to stop.
    pub listen: u8,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            command: 0x1c,
            detach: 0x1d,
            // Ctrl-G, Ctrl-O, Ctrl-T: a bell, a rarely-bound key, and
            // transpose-characters. None of the three is how anybody edits a
            // line, which is the point.
            dictate: 0x07,
            read: 0x0f,
            aloud: 0x14,
            // Ctrl-Y and Ctrl-P: yank and previous-history in readline, and
            // neither is how a line gets written. Both are given back the
            // moment command mode is off.
            send: 0x19,
            photo: 0x10,
            // Ctrl-N: next-history in readline, which is not how a line is
            // written either, and free in every TUI checked here.
            listen: 0x0e,
        }
    }
}

impl Keys {
    /// What this key does while command mode is on, if anything. Anything
    /// else -- including every other control character and all text -- goes
    /// to the agent, so the mode can be left on while you type.
    pub fn act(&self, b: u8) -> Option<Act> {
        if b == self.dictate {
            Some(Act::Dictate)
        } else if b == self.read {
            Some(Act::Read)
        } else if b == self.aloud {
            Some(Act::Aloud)
        } else if b == self.send {
            Some(Act::Send)
        } else if b == self.photo {
            Some(Act::Photo)
        } else if b == self.listen {
            Some(Act::Listen)
        } else {
            None
        }
    }
}

/// What a key does once command mode is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Record, then transcribe into the agent's prompt.
    Dictate,
    /// Read a reply back; again walks further back; again while talking stops.
    Read,
    /// Read every new reply as it arrives.
    Aloud,
    /// Send a file from this machine to the agent's.
    Send,
    /// Take a photo here and send that.
    Photo,
    /// Keep listening until told to stop, a sentence at a time.
    Listen,
}

/// `ctrl-^`, `ctrl-g`, `^]`, `0x1e`, `30` -- whatever somebody writes down.
pub fn key_byte(spec: &str) -> Option<u8> {
    let s = spec.trim().to_lowercase();
    // `^` alone is a key; `^]` is a key with a prefix. A letter on its own is
    // NOT: `command_key = "x"` meaning Ctrl-X would be a surprise, and a
    // plain letter cannot be one of these anyway, since an attach forwards
    // every printable byte to the agent.
    let (s, prefixed) = match s.strip_prefix("ctrl-") {
        Some(rest) => (rest.to_string(), true),
        None if s.len() > 1 && s.starts_with('^') => (s[1..].to_string(), true),
        None => (s.clone(), false),
    };
    if let Some(hex) = s.strip_prefix("0x") {
        return u8::from_str_radix(hex, 16).ok().filter(|b| *b < 0x20);
    }
    if let Ok(n) = s.parse::<u8>() {
        return (n < 0x20).then_some(n);
    }
    let c = s.chars().next()?;
    if s.chars().count() != 1 {
        return None;
    }
    if !prefixed && c.is_ascii_alphanumeric() {
        return None;
    }
    // Ctrl-A is 1 ... Ctrl-Z is 26, and the six after that are the symbols.
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '-' | '/' => Some(0x1f),
        '@' | ' ' => Some(0x00),
        _ => None,
    }
}

/// How a key reads in a status bar.
pub fn key_name(b: u8) -> String {
    match b {
        0x00 => "Ctrl-@".into(),
        0x1b => "Esc".into(),
        0x1c => "Ctrl-\\".into(),
        0x1d => "Ctrl-]".into(),
        0x1e => "Ctrl-^".into(),
        0x1f => "Ctrl-_".into(),
        1..=26 => format!("Ctrl-{}", (b - 1 + b'A') as char),
        other => format!("0x{other:02x}"),
    }
}

/// Read the keys from the console's own preferences file.
///
/// Escape is refused outright: `command_key = "ctrl-["` is the one setting
/// that would take Escape away from every program inside the attach, and
/// somebody will try it because `Ctrl-[` looks like the partner of
/// `Ctrl-]`.
pub fn keys_from(toml_text: &str) -> Keys {
    let mut k = Keys::default();
    for line in toml_text.lines() {
        let line = line.trim();
        let Some((name, value)) = line.split_once('=') else { continue };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        let Some(b) = key_byte(value).filter(|b| *b != 0x1b) else { continue };
        match name.trim() {
            "command_key" => k.command = b,
            "detach_key" => k.detach = b,
            "dictate_key" => k.dictate = b,
            "read_key" => k.read = b,
            "aloud_key" => k.aloud = b,
            "send_key" => k.send = b,
            "photo_key" => k.photo = b,
            "listen_key" => k.listen = b,
            _ => {}
        }
    }
    // Two of these being the same key is one key and a puzzle. The two that
    // work in both modes win, since they are the way out of everything.
    let d = Keys::default();
    if k.command == k.detach {
        k.command = d.command;
    }
    for (field, fallback) in [
        (&mut k.dictate, d.dictate),
        (&mut k.read, d.read),
        (&mut k.aloud, d.aloud),
        (&mut k.send, d.send),
        (&mut k.photo, d.photo),
        (&mut k.listen, d.listen),
    ] {
        if *field == k.command || *field == k.detach {
            *field = fallback;
        }
    }
    k
}

/// What this machine can actually do with sound.
#[derive(Debug, Clone, Default)]
pub struct Audio {
    /// The recorder, as a command line with `{}` where the file goes.
    pub record: Option<Vec<String>>,
    /// A player for wav, and one for anything else (mp3, usually).
    pub play_wav: Option<Vec<String>>,
    pub play_any: Option<Vec<String>>,
}

impl Audio {
    pub fn can_hear(&self) -> bool {
        self.record.is_some()
    }

    pub fn can_speak(&self) -> bool {
        self.play_wav.is_some() || self.play_any.is_some()
    }

    /// What to say in the footer when there is nothing to work with.
    pub fn why_not(&self) -> &'static str {
        "no recorder on this machine: install pulseaudio-utils, alsa-utils, sox or ffmpeg"
    }
}

fn have(bin: &str) -> bool {
    // `which` is not guaranteed; PATH is.
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .any(|d| {
            let p = Path::new(d).join(bin);
            std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false)
        })
}

fn words(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Look once, at start-up. Cheap, and a machine does not grow a sound card
/// while the console is open.
pub fn detect() -> Audio {
    // 16 kHz mono wav: every transcription service takes it, it is a third
    // the size of 44.1 kHz stereo, and speech has nothing above 8 kHz worth
    // sending over somebody's VPN.
    let record = if have("parecord") {
        Some(words(&["parecord", "--channels=1", "--rate=16000", "--file-format=wav", "{}"]))
    } else if have("arecord") {
        Some(words(&["arecord", "-q", "-f", "S16_LE", "-c", "1", "-r", "16000", "{}"]))
    } else if have("rec") {
        Some(words(&["rec", "-q", "-c", "1", "-r", "16000", "{}"]))
    } else if have("ffmpeg") {
        Some(words(&[
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "pulse", "-i", "default",
            "-ac", "1", "-ar", "16000", "-y", "{}",
        ]))
    } else {
        None
    };
    let play_wav = if have("paplay") {
        Some(words(&["paplay", "{}"]))
    } else if have("aplay") {
        Some(words(&["aplay", "-q", "{}"]))
    } else if have("play") {
        Some(words(&["play", "-q", "{}"]))
    } else if have("ffplay") {
        Some(words(&["ffplay", "-nodisp", "-autoexit", "-loglevel", "error", "{}"]))
    } else {
        None
    };
    // mp3 is what a speech service usually answers with, and paplay cannot
    // read one.
    let play_any = if have("ffplay") {
        Some(words(&["ffplay", "-nodisp", "-autoexit", "-loglevel", "error", "{}"]))
    } else if have("mpg123") {
        Some(words(&["mpg123", "-q", "{}"]))
    } else if have("play") {
        Some(words(&["play", "-q", "{}"]))
    } else {
        None
    };
    Audio { record, play_wav, play_any }
}

fn run(cmd: &[String], file: &Path) -> Result<Child> {
    let mut it = cmd.iter();
    let bin = it.next().context("empty command")?;
    let args: Vec<String> = it
        .map(|a| {
            if a == "{}" {
                file.to_string_lossy().to_string()
            } else {
                a.clone()
            }
        })
        .collect();
    Command::new(bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {bin}"))
}

/// Start recording. The child keeps going until `stop` is called, which is
/// what makes this press-to-start, press-again-to-send rather than a fixed
/// number of seconds nobody can judge in advance.
/// A recorder that stops ITSELF when you stop talking.
///
/// Push-to-talk needs a key at each end. Hands-free needs the recorder to
/// know where the sentence ended, and only sox can be asked that from a
/// command line: `silence 1 0.1 3% 1 2.0 3%` means "start when there is
/// sound, stop after two seconds of quiet". The process exiting is the
/// end-of-utterance signal, which is why the caller watches for it rather
/// than timing anything.
///
/// Nothing else here can do it. arecord and parecord record until they are
/// killed, and ffmpeg's silencedetect reports silence without acting on it;
/// chopping the microphone into fixed six-second windows instead would cut
/// sentences in half, which is worse than saying so.
pub fn listen_cmd(audio: &Audio) -> Option<Vec<String>> {
    // Only sox. `audio.record` says what this machine has; this asks for one
    // specific tool because only that tool has the feature.
    if !have("rec") {
        return None;
    }
    let _ = audio;
    Some(words(&[
        "rec", "-q", "-c", "1", "-r", "16000", "{}",
        // Begin on the first sound above the floor, and end after 2s under
        // it. 3% is quiet enough for a room with a fan in it and loud enough
        // not to trigger on breathing.
        "silence", "1", "0.1", "3%", "1", "2.0", "3%",
    ]))
}

/// Start a recorder that will stop on its own. `None` when this machine has
/// no tool that can do that.
pub fn listen(audio: &Audio, dir: &Path) -> Result<(Child, PathBuf)> {
    let cmd = listen_cmd(audio).context(
        "hands-free listening needs sox (the `rec` command): every other recorder here \
         has to be told when to stop",
    )?;
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("heard-{}.wav", nexmesh_proto::now_ms()));
    let child = run(&cmd, &path)?;
    Ok((child, path))
}

/// Has a self-stopping recorder finished? The bytes, when it has.
pub fn listened(child: &mut Child, path: &Path) -> Option<Vec<u8>> {
    match child.try_wait() {
        Ok(Some(_)) => std::fs::read(path).ok(),
        _ => None,
    }
}

pub fn start(audio: &Audio, dir: &Path) -> Result<(Child, PathBuf)> {
    let cmd = audio.record.as_ref().context("nothing on this machine can record")?;
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("say-{}.wav", nexmesh_proto::now_ms()));
    let child = run(cmd, &path)?;
    Ok((child, path))
}

/// Stop recording and hand back the bytes.
pub fn stop(mut child: Child, path: &Path) -> Result<Vec<u8>> {
    // A recorder writes its wav header on the way out, so it is asked to
    // stop rather than killed outright: a SIGKILLed `parecord` leaves a file
    // whose length field says zero, which every decoder then believes.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    );
    // It has a moment to finish the file, and then it goes.
    for _ in 0..40 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let _ = std::fs::remove_file(path);
    // A wav header alone is 44 bytes; anything near that is a key pressed
    // twice, not a sentence.
    if bytes.len() < 1024 {
        anyhow::bail!("that was too short to hear");
    }
    Ok(bytes)
}

/// Ask the fleet to turn sound into words.
pub async fn transcribe(
    base: &str,
    client: &reqwest::Client,
    wav: &[u8],
    language: Option<&str>,
) -> Result<String> {
    let mut request = json!({ "audio_base64": b64(wav) });
    if let Some(l) = language {
        request["language"] = json!(l);
    }
    let out = ask(base, client, "transcribe", request, 180).await?;
    Ok(spoken_text(&out))
}

/// Somewhere to keep the player while it plays, so somebody else can stop
/// it. A voice that cannot be interrupted is one you switch off for good.
pub type Playing = std::sync::Arc<std::sync::Mutex<Option<Child>>>;

/// Stop whatever is being said, now.
pub fn hush(playing: &Playing) {
    if let Ok(mut slot) = playing.lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Ask the fleet to turn words into sound, and play it.
pub async fn speak(
    base: &str,
    client: &reqwest::Client,
    audio: &Audio,
    text: &str,
    voice: Option<&str>,
    dir: &Path,
    playing: &Playing,
) -> Result<()> {
    // A teammate's own voice, when the seat has been given one. The service
    // decides whether it has that voice; an unknown name comes back as an
    // error naming the ones it does have, which is more use than a silent
    // fallback to the default.
    let mut req = json!({ "input": text });
    if let Some(v) = voice.filter(|v| !v.trim().is_empty()) {
        req["voice"] = json!(v);
    }
    let out = ask(base, client, "tts", req, 120).await?;
    let (bytes, ext) = audio_of(base, client, &out).await?;
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(format!("said-{}.{ext}", nexmesh_proto::now_ms()));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    let cmd = if ext == "wav" {
        audio.play_wav.as_ref().or(audio.play_any.as_ref())
    } else {
        audio.play_any.as_ref().or(audio.play_wav.as_ref())
    }
    .context("nothing on this machine can play sound")?;
    let child = run(cmd, &path)?;
    if let Ok(mut slot) = playing.lock() {
        // Anything still playing is replaced rather than talked over.
        if let Some(mut old) = slot.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        *slot = Some(child);
    }
    // Waited on WITHOUT holding the lock, so `hush` can reach in and stop it.
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let done = match playing.lock() {
            Ok(mut slot) => match slot.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_)) | Err(_)),
                // Somebody stopped it.
                None => true,
            },
            Err(_) => true,
        };
        if done {
            if let Ok(mut slot) = playing.lock() {
                if let Some(c) = slot.as_mut() {
                    if matches!(c.try_wait(), Ok(Some(_)) | Err(_)) {
                        *slot = None;
                    }
                }
            }
            break;
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// The bytes behind a speech answer, whatever shape it came in: base64 in
/// the body, or a reference to fetch.
async fn audio_of(
    base: &str,
    client: &reqwest::Client,
    answer: &str,
) -> Result<(Vec<u8>, &'static str)> {
    let v: serde_json::Value = serde_json::from_str(answer).unwrap_or(serde_json::Value::Null);
    let inline = v["audio_base64"]
        .as_str()
        .or_else(|| v["data"]["audio_base64"].as_str())
        .or_else(|| v["audio"].as_str());
    if let Some(b) = inline {
        let bytes = unb64(b)?;
        let kind = kind_of(&bytes);
        return Ok((bytes, kind));
    }
    // A URL, or the bridge's own "files: ..." rendering of one.
    let url = v["url"]
        .as_str()
        .or_else(|| v["data"]["url"].as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            answer
                .split_whitespace()
                .find(|w| w.starts_with("http://") || w.starts_with("https://"))
                .map(|s| s.trim_end_matches(['.', ',']).to_string())
        })
        .or_else(|| {
            // A path on the hub's own side is served relative to it.
            v["path"].as_str().map(|p| format!("{base}{p}"))
        })
        .context("the answer had no sound in it")?;
    let bytes = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await?
        .to_vec();
    let kind = kind_of(&bytes);
    Ok((bytes, kind))
}

/// wav or not, from the bytes rather than from a file name somebody else
/// chose.
fn kind_of(b: &[u8]) -> &'static str {
    if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        "wav"
    } else {
        "mp3"
    }
}

/// Start an operator ask and wait for it, the same two calls the dashboard
/// makes.
async fn ask(
    base: &str,
    client: &reqwest::Client,
    capability: &str,
    request: serde_json::Value,
    seconds: u64,
) -> Result<String> {
    let started = crate::post(
        client,
        format!("{base}/v1/capabilities/ask"),
        json!({ "capability": capability, "request": request }),
    )
    .await?;
    if let Some(e) = started.get("error").and_then(|e| e.as_str()) {
        anyhow::bail!("{e}");
    }
    let id = started["ask_id"]
        .as_str()
        .context("the hub did not say which ask that was")?
        .to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let v = crate::get(client, format!("{base}/v1/capabilities/ask/{id}")).await?;
        if v["done"].as_bool() == Some(true) {
            let result = v["result"].as_str().unwrap_or("").to_string();
            if v["status"].as_str() != Some("answered") {
                anyhow::bail!("{}", if result.is_empty() { "it could not do that".into() } else { result });
            }
            return Ok(result);
        }
    }
    anyhow::bail!("timed out waiting for {capability}")
}

/// Terminal bytes as words.
///
/// What is on an agent's screen is text with escape sequences through it, and
/// a speech service asked to read `\x1b[1;32m` reads it out. Only the parts a
/// person would see survive: CSI and OSC sequences go, as do the control
/// characters, and a run of blanks becomes one space.
pub fn plain(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            match bytes.get(i) {
                // CSI: up to the first byte in 0x40..=0x7e.
                Some(b'[') => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                // OSC and the other string-terminated ones: to BEL or ESC \.
                Some(b']') | Some(b'P') | Some(b'^') | Some(b'_') => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                // A two-byte escape, or a stray one at the end.
                Some(_) => i += 2,
                None => break,
            }
            continue;
        }
        i += 1;
        match b {
            b'\n' | b'\r' | b'\t' => out.push(' '),
            0x20..=0x7e => out.push(b as char),
            // Anything multi-byte is passed through; the service reads UTF-8.
            0x80..=0xff => out.push(b as char),
            _ => {}
        }
    }
    // One space where there were many, which is most of a screen.
    let mut squashed = String::with_capacity(out.len());
    let mut space = false;
    for c in out.chars() {
        if c == ' ' {
            if !space {
                squashed.push(' ');
            }
            space = true;
        } else {
            space = false;
            squashed.push(c);
        }
    }
    squashed.trim().to_string()
}

/// The words out of whatever shape a transcription answers in.
pub fn spoken_text(out: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(out) {
        Ok(v) => v,
        Err(_) => return out.trim().to_string(),
    };
    for path in [["text"], ["transcript"]] {
        if let Some(t) = v[path[0]].as_str() {
            return t.trim().to_string();
        }
    }
    for key in ["text", "transcript"] {
        if let Some(t) = v["data"][key].as_str() {
            return t.trim().to_string();
        }
    }
    if let Some(segments) = v["segments"].as_array() {
        let joined: Vec<&str> = segments.iter().filter_map(|s| s["text"].as_str()).collect();
        if !joined.is_empty() {
            return joined.join(" ").trim().to_string();
        }
    }
    out.trim().to_string()
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unb64(s: &str) -> Result<Vec<u8>> {
    // A data URI is a perfectly ordinary thing for a service to answer with.
    let s = s.rsplit_once("base64,").map(|(_, b)| b).unwrap_or(s);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        // base64url, which half the world sends, maps onto the last two.
        let v = B64
            .iter()
            .position(|c| *c == ch)
            .or(match ch {
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            })
            .with_context(|| "that was not base64")? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_including_the_awkward_lengths() {
        for n in 0..300usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            let text = b64(&data);
            assert!(text.len().is_multiple_of(4), "{n}: {text}");
            assert_eq!(unb64(&text).unwrap(), data, "{n}");
        }
    }

    #[test]
    fn a_data_uri_and_base64url_are_both_read() {
        let plain = b64(b"hello there");
        assert_eq!(unb64(&format!("data:audio/mpeg;base64,{plain}")).unwrap(), b"hello there");
        let url_style = b64(&[251, 255, 190]).replace('+', "-").replace('/', "_");
        assert_eq!(unb64(&url_style).unwrap(), vec![251, 255, 190]);
    }

    #[test]
    fn the_words_are_found_in_whatever_shape_they_arrive() {
        assert_eq!(spoken_text(r#"{"text":" deploy it  "}"#), "deploy it");
        assert_eq!(spoken_text(r#"{"transcript":"deploy it"}"#), "deploy it");
        assert_eq!(spoken_text(r#"{"data":{"text":"deploy it"}}"#), "deploy it");
        assert_eq!(
            spoken_text(r#"{"segments":[{"text":"deploy"},{"text":"it"}]}"#),
            "deploy it"
        );
        // Not JSON at all: the service answered in plain words.
        assert_eq!(spoken_text("  deploy it "), "deploy it");
    }

    #[test]
    fn wav_is_told_from_mp3_by_its_bytes_not_its_name() {
        let mut wav = b"RIFF\0\0\0\0WAVE".to_vec();
        wav.extend_from_slice(&[0u8; 32]);
        assert_eq!(kind_of(&wav), "wav");
        assert_eq!(kind_of(b"ID3\x03\0\0\0\0\0\0"), "mp3");
        assert_eq!(kind_of(&[]), "mp3");
    }

    #[test]
    fn a_screen_is_read_as_what_a_person_would_see() {
        // A speech service asked to read an escape sequence reads it out.
        let screen = b"\x1b[2J\x1b[H\x1b[1;32mnode-a\x1b[0m ready\r\n  three    spaces\x07";
        assert_eq!(plain(screen), "node-a ready three spaces");
        // A title is not on the screen.
        assert_eq!(plain(b"\x1b]0;a title\x07hello"), "hello");
        assert_eq!(plain(b"\x1b]2;t\x1b\\after"), "after");
        // A sequence cut off at the end does not take anything with it.
        assert_eq!(plain(b"text\x1b["), "text");
        assert_eq!(plain(b"text\x1b"), "text");
    }

    #[test]
    fn a_key_can_be_written_down_several_ways() {
        assert_eq!(key_byte("ctrl-^"), Some(0x1e));
        assert_eq!(key_byte("^"), Some(0x1e));
        assert_eq!(key_byte("Ctrl-_"), Some(0x1f));
        assert_eq!(key_byte("ctrl-g"), Some(7));
        assert_eq!(key_byte("^]"), Some(0x1d));
        assert_eq!(key_byte("0x1c"), Some(0x1c));
        assert_eq!(key_byte("28"), Some(28));
        // A key that is not a control character cannot be one of ours: an
        // attach forwards everything else to the agent.
        assert_eq!(key_byte("x"), None);
        assert_eq!(key_byte("0x41"), None);
        assert_eq!(key_byte(""), None);
    }

    #[test]
    fn the_keys_come_from_the_console_preferences() {
        let k = keys_from("# nexmesh console\ntheme = \"deck\"\ncommand_key = \"ctrl-g\"\n");
        assert_eq!(k.command, 7, "the one that was set");
        assert_eq!(k.detach, Keys::default().detach, "and the other keeps its default");
        assert_eq!(key_name(7), "Ctrl-G");
        assert_eq!(key_name(0x1c), "Ctrl-\\");
    }

    #[test]
    fn escape_cannot_be_taken_however_it_is_asked_for() {
        // `ctrl-[` IS Escape. Binding it would break every arrow key and
        // every TUI inside the attach -- and it looks like the obvious
        // partner for `ctrl-]`, so somebody will try.
        let k = keys_from("command_key = \"ctrl-[\"\n");
        assert_eq!(k.command, Keys::default().command, "{k:?}");
        let k = keys_from("command_key = \"0x1b\"\n");
        assert_eq!(k.command, Keys::default().command, "{k:?}");
        // And two keys that are the same key is one key and a puzzle.
        let k = keys_from("command_key = \"ctrl-]\"\n");
        assert_eq!(k.command, Keys::default().command);
        assert_ne!(k.command, k.detach);
    }

    #[test]
    fn command_mode_takes_three_keys_and_leaves_the_keyboard_alone() {
        let k = Keys::default();
        assert_eq!(k.act(k.dictate), Some(Act::Dictate));
        assert_eq!(k.act(k.read), Some(Act::Read));
        assert_eq!(k.act(k.aloud), Some(Act::Aloud));
        // Everything else is the agent's, even in command mode: you can
        // leave the mode on and carry on typing. That includes the letters
        // and the line-editing keys a prompt is actually driven with.
        assert_eq!(k.act(k.send), Some(Act::Send));
        assert_eq!(k.act(k.photo), Some(Act::Photo));
        for b in [b'v', b'r', b'a', b'd', b'q', b'?', 0x01, 0x05, 0x0b, 0x12, 0x17, b'\r'] {
            assert_eq!(k.act(b), None, "0x{b:02x} must reach the agent");
        }
    }

    #[test]
    fn a_key_cannot_be_two_things_at_once() {
        // Setting the dictate key to the detach key would make one of them
        // unreachable, and which one would depend on the order of a match.
        let k = keys_from("dictate_key = \"ctrl-]\"\nread_key = \"ctrl-\\\\\"\n");
        assert_ne!(k.dictate, k.detach);
        assert_ne!(k.read, k.command);
        assert_eq!(k.dictate, Keys::default().dictate);
    }

    #[test]
    fn a_machine_with_nothing_says_so_rather_than_offering_a_dead_key() {
        let none = Audio::default();
        assert!(!none.can_hear() && !none.can_speak());
        assert!(none.why_not().contains("pulseaudio"));
        let some = Audio {
            record: Some(words(&["parecord", "{}"])),
            play_wav: Some(words(&["paplay", "{}"])),
            play_any: None,
        };
        assert!(some.can_hear() && some.can_speak());
    }
}
