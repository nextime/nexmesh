//! Asking tmux where its panes are.
//!
//! Only ever a question, never a command: nothing here sends keys. Typing at
//! an adopted session goes through the pty of the node's own tmux client, so
//! it is the ordinary terminal path and not a second mechanism with its own
//! rules. See clis/tmux.toml for why `send-keys` is deliberately unused.

use std::process::Command;

/// The tmux server to talk to.
///
/// A tmux server is shared by everything one user runs, which meant the test
/// suite and the live fleet shared one -- and a suite that creates, adopts
/// and kills sessions on the same server as somebody's real work can break
/// the thing it is testing. Setting `NEXMESH_TMUX_SOCKET` puts this node on a
/// server of its own (`tmux -L <name>`), which is what the suites now do.
/// Unset -- the normal case -- it is the user's own server, where their
/// sessions are.
pub fn socket() -> Option<String> {
    std::env::var("NEXMESH_TMUX_SOCKET").ok().filter(|s| !s.trim().is_empty())
}

/// `tmux`, pointed at the right server, with $TMUX cleared.
///
/// $TMUX names the socket of the server we are INSIDE, and a node started
/// from within tmux passes it down. Left in place, this asks the wrong server
/// -- or a socket that does not exist -- and the answer is silence, which
/// reads exactly like "no such pane". The spawned client clears it for the
/// same reason (clis/tmux.toml).
pub fn cmd() -> Command {
    let mut c = Command::new("tmux");
    c.env_remove("TMUX");
    if let Some(sock) = socket() {
        c.args(["-L", &sock]);
    }
    c
}

/// Make tmux paint the whole screen into our own client's pty.
///
/// tmux is a differential renderer: it writes what CHANGED. A pane that has
/// been sitting still since before a viewer arrived therefore sends nothing,
/// and the viewer -- which starts from a replay of the last few kilobytes of
/// that pty -- shows an empty screen with a status line ticking away at the
/// bottom. Worse, tmux rewrites that status line every second, so on a pane
/// nobody is using those ticks are the ONLY thing in the recent scrollback:
/// the replay is full of them and the screen itself has scrolled out of it.
///
/// This is still only a question put to tmux, in the spirit of the rest of
/// this module: `refresh-client` writes nothing into the pane and sends no
/// keys. It asks tmux to repaint the client the NODE owns, which is the
/// screen the viewer is waiting for.
/// TAKE TMUX'S OWN KEYS AWAY FROM THIS VIEW.
///
/// A seat held in tmux is reached through a second, grouped client that the
/// node attaches -- and that client had tmux's whole keyboard. `Ctrl-B %`
/// split the pane and gave whoever was attached a SHELL on that machine,
/// `Ctrl-B c` opened a window with another, and `Ctrl-B :` was a tmux
/// command prompt. An attach is a way into one agent; it is not a way onto
/// the host it runs on, and nothing about pressing "attach" says otherwise.
///
/// `prefix` is a SESSION option, so this takes the keys away from the view
/// the node created and changes nothing about the operator's own
/// `tmux attach -t nexmesh-cli-<slot>`, which is their way in when they do
/// want a terminal on that machine. That is deliberate: the lock is on the
/// door nexmesh opens, not on tmux.
///
/// Not airtight on its own -- tmux's right-click menu offers to split a pane
/// too, and key bindings are per-SERVER rather than per-session, so they
/// cannot be unbound here without touching the operator's own tmux. The
/// viewer drops button-3 reports before they reach tmux for that half.
pub fn lock_view(view: &str) {
    for args in [
        // No prefix at all: `Ctrl-B` goes to the CLI like any other key,
        // which is also what a CLI using Ctrl-B for anything would want.
        ["set-option", "-t", view, "prefix", "None"],
        ["set-option", "-t", view, "prefix2", "None"],
        // Nothing to click on, either: tmux's status line carries menus of
        // its own, and the viewer draws its own row anyway.
        ["set-option", "-t", view, "status", "off"],
    ] {
        let out = cmd().args(args).output();
        if let Ok(o) = out {
            if !o.status.success() {
                tracing::debug!(%view, stderr = %String::from_utf8_lossy(&o.stderr),
                    "could not lock a tmux view down");
            }
        }
    }
}

pub fn repaint(view: &str) {
    // Our client is the one attached to the session the node created for
    // this view. tmux addresses clients by their tty, not by session.
    let Some(out) = cmd()
        .args(["list-clients", "-F", "#{client_tty} #{client_session}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return;
    };
    // And tmux's own status line goes, for this client only.
    //
    // A viewer attaching to an adopted pane saw TWO rows at the bottom:
    // tmux's status line and the viewer's own. The viewer's is the one that
    // says whose screen this is and how to leave, so tmux's is noise -- and
    // it is set on the session the NODE created, so it changes nothing about
    // what the operator sees in their own `tmux attach`.
    let _ = cmd().args(["set-option", "-t", view, "status", "off"]).output();
    // And tmux's own keys, which are not the operator's way onto this
    // machine. Done here as well as at the start, because `repaint` is what
    // runs when a viewer arrives -- including at a view the node adopted
    // before this existed.
    lock_view(view);
    // And tmux takes the wheel, for this view only.
    //
    // A pane's history belongs to tmux: the terminal the operator sits at
    // has no scrollback for a full-screen program, and the viewer in
    // between has none either. With the mouse off, the wheel reaches
    // nothing at all, which is simply a terminal you cannot scroll.
    //
    // The cost is that dragging to select needs SHIFT held, as it does in
    // every mouse-enabled TUI. That is a thing to know, not a thing to be
    // stuck in; `Ctrl-F` in the viewer turns it off for anyone who would
    // rather have the plain terminal back.
    //
    // Set on the session the NODE created, so the operator's own
    // `tmux attach` keeps whatever they had.
    let _ = cmd().args(["set-option", "-t", view, "mouse", "on"]).output();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((tty, session)) = line.split_once(' ') {
            if session == view {
                let done = cmd()
                    .args(["refresh-client", "-t", tty])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                tracing::info!(%view, tty, done, "asked tmux to repaint an adopted pane");
            }
        }
    }
}

/// Clear our own leftover view of a previous adoption.
///
/// A tmux SESSION outlives its clients. When a node stops -- a deploy, a
/// reboot -- its tmux client dies but the grouped session it created,
/// `nexmesh-<slot>`, stays on the server with nobody attached. The next
/// adoption then runs `new-session -s nexmesh-<slot>` against a name that
/// already exists, tmux says "duplicate session", the spawn dies at once,
/// and after four of those the hub writes the seat off as flapping. So an
/// adopted terminal never came back on its own after a node restart, and the
/// only cure was re-adopting it by hand -- which nobody knows to do.
///
/// Only ever OUR name: `nexmesh-<slot>` is created by this code and by
/// nothing else, and only when nothing is attached to it, which is what
/// makes it a corpse rather than somebody's window.
/// Does one of OUR view sessions still have a client on it?
///
/// A view session is created only by a node, so a client on one is a node's
/// grouped client -- normally this node's own, from an earlier start of the
/// same seat. That makes it the answer to "am I already watching this
/// seat", which is a different question from "is there a leftover to clear".
pub fn view_has_client(view: &str) -> bool {
    let exact = format!("={view}");
    cmd()
        .args(["list-clients", "-t", &exact, "-F", "#{client_tty}"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Kill a view session even though something is still on it.
///
/// Only for a view whose agent this node no longer has: the client is then
/// an orphan from a start that went wrong, and leaving it there makes the
/// seat unstartable for ever, because every later start collides with the
/// name. Never used on a view whose agent is still ours.
pub fn force_clear_view(view: &str) -> bool {
    let exact = format!("={view}");
    let killed = cmd()
        .args(["kill-session", "-t", &exact])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    tracing::warn!(%view, killed,
        "cleared a view session that still had a client but no agent of ours behind it");
    killed
}

pub fn clear_stale_view(view: &str) -> bool {
    // `=name` is tmux's exact match; without it a prefix match could resolve
    // to a different session entirely.
    let exact = format!("={view}");
    let exists = cmd()
        .args(["has-session", "-t", &exact])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return false;
    }
    let attached = cmd()
        .args(["list-clients", "-t", &exact, "-F", "#{client_tty}"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    if attached {
        tracing::warn!(%view, "a view session of ours still has a client; leaving it alone");
        return false;
    }
    let killed = cmd()
        .args(["kill-session", "-t", &exact])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    tracing::info!(%view, killed, "cleared a leftover view session from a previous adoption");
    killed
}

/// The pid of the process in the pane the given client view is looking at.
///
/// `view` is the node's OWN grouped session (`nexmesh-<slot>`), not the
/// operator's -- so this answers "where would a keystroke from this node
/// land", which is the only question worth asking before typing.
pub fn pane_pid(view: &str) -> Option<i32> {
    let out = cmd()
        .args(["display-message", "-t", view, "-p", "#{pane_pid}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Hand the wheel to a tmux view, or take it back.
///
/// Per SESSION, and the session is the one the node created, so this never
/// changes what somebody else's `tmux attach` does. Returns what to tell
/// the person who asked, because "it did nothing" is not an answer.
pub fn set_mouse(view: &str, on: bool) -> (bool, String) {
    let out = cmd()
        .args(["set-option", "-t", view, "mouse", if on { "on" } else { "off" }])
        .output();
    match out {
        Ok(o) if o.status.success() => (true, String::new()),
        Ok(o) => (false, String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => (false, e.to_string()),
    }
}

/// The tmux session that HOLDS a supervised CLI.
///
/// Distinct from `nexmesh-<slot>`, which is the node's own grouped client
/// (see `spawn::tmux_view`): one is the CLI's home and outlives the node,
/// the other is a window onto it that comes and goes with the node.
/// State a view's size to tmux, instead of relying on the client noticing.
///
/// A pty resize reaches a tmux client only as SIGWINCH, and a signal that
/// arrives before the client has a handler for it is simply gone -- the
/// default disposition of SIGWINCH is to ignore it. tmux then keeps the size
/// it read when it started, sizes the window to that, and the CLI draws rows
/// the viewer cannot show. Nothing recovers it, because a TUI repaints only
/// the cells it believes changed.
///
/// `resize-window` puts the window into MANUAL sizing, which is the right
/// state here: the node owns this view -- it created it, it turns the status
/// line off, it owns the mouse -- and the size it computes from the viewers
/// is the size that is wanted. Automatic sizing is what was getting it
/// wrong.
pub fn resize_window(view: &str, cols: u16, rows: u16) -> bool {
    if cols == 0 || rows == 0 {
        return false;
    }
    cmd()
        .args([
            "resize-window",
            "-t",
            view,
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn held_session(slot_id: &str) -> String {
    format!("nexmesh-cli-{slot_id}")
}

/// Is that session there?
pub fn has_session(name: &str) -> bool {
    cmd()
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Put a CLI in a tmux session of its own, detached, and leave it there.
///
/// The point is that the tmux SERVER owns the pty, not the node. The node
/// then attaches a grouped client under its own pty and everything
/// downstream is the ordinary terminal path; when the node goes away the
/// client dies and the CLI does not notice.
///
/// Environment goes through `env` rather than tmux's own `-e`, which not
/// every tmux in the field has, and `remain-on-exit` is left off on purpose:
/// a CLI that exits should take its seat down, not leave a dead pane that
/// looks like a running agent.
pub fn hold(
    name: &str,
    cwd: &str,
    bin: &str,
    args: &[String],
    env: &[(String, String)],
    env_unset: &[String],
) -> Result<(), String> {
    let mut c = cmd();
    // A size that is replaced the moment a client attaches, but a detached
    // session with no size at all makes some CLIs lay out for 80x24 and keep
    // it. Large enough not to wrap the first screen it draws.
    c.args(["new-session", "-d", "-s", name, "-c", cwd, "-x", "200", "-y", "50", "--"]);
    // `env` carries the environment INTO the session, because the tmux
    // server the session is created on has its own, inherited from whenever
    // it started -- not ours.
    c.arg("env");
    for k in env_unset {
        c.args(["-u", k]);
    }
    for (k, v) in env {
        c.arg(format!("{k}={v}"));
    }
    c.arg(bin);
    c.args(args);
    let out = c.output().map_err(|e| format!("running tmux: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tmux would not hold it: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    lock_held(name);
    Ok(())
}

/// WHEN THE CLI EXITS, SO DOES ITS SEAT.
///
/// `remain-on-exit` keeps a dead pane on screen with "Pane is dead" in it.
/// tmux's own default is off, but it is an option an operator may have
/// turned on in their own config -- and with it on, a CLI that crashed left
/// a session that still existed, a node client still attached to it, and
/// therefore a seat the hub believed was running. The viewer sat looking at
/// a dead screen with no way to tell.
///
/// Set on the session's WINDOWS, with `-w`, which is the scope the option
/// actually lives in. Without that flag tmux takes the command, exits 0 and
/// stores nothing -- a silent no-op that read as a success on a fleet whose
/// global value happened to be off already, and would have left a dead pane
/// standing on any host where it was not. `-w` against a session target
/// sets it for every window in that session, so this is the seat's own
/// behaviour and not a change to anybody's tmux.
///
/// Called when a seat is first held AND every time one is re-attached,
/// because `hold` runs only on the session that does not exist yet: a seat
/// held before this node started -- the whole point of holding one -- would
/// otherwise never be told, and a fleet upgraded in place would carry the
/// old behaviour until every seat happened to be respawned.
///
/// The held session's PREFIX is deliberately left alone: it is the
/// operator's own way in, and the lock in `lock_view` is on the door
/// nexmesh opens.
pub fn lock_held(name: &str) {
    let _ = cmd().args(["set-option", "-w", "-t", name, "remain-on-exit", "off"]).output();
    // Nothing in here is anybody's shell, and a pane that exits must not be
    // replaced by one. A session option, so no `-w`.
    let _ = cmd().args(["set-option", "-t", name, "destroy-unattached", "off"]).output();
}

/// Stop holding it: used when a seat is genuinely being ended rather than
/// paused, and to clear a session whose CLI has already gone.
pub fn release(name: &str) {
    let _ = cmd().args(["kill-session", "-t", name]).output();
}
