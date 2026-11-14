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
