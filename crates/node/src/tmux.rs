//! Asking tmux where its panes are.
//!
//! Only ever a question, never a command: nothing here sends keys. Typing at
//! an adopted session goes through the pty of the node's own tmux client, so
//! it is the ordinary terminal path and not a second mechanism with its own
//! rules. See clis/tmux.toml for why `send-keys` is deliberately unused.

use std::process::Command;

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
    let Some(out) = Command::new("tmux")
        .env_remove("TMUX")
        .args(["list-clients", "-F", "#{client_tty} #{client_session}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((tty, session)) = line.split_once(' ') {
            if session == view {
                let done = Command::new("tmux")
                    .env_remove("TMUX")
                    .args(["refresh-client", "-t", tty])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                tracing::info!(%view, tty, done, "asked tmux to repaint an adopted pane");
            }
        }
    }
}

/// The pid of the process in the pane the given client view is looking at.
///
/// `view` is the node's OWN grouped session (`nexmesh-<slot>`), not the
/// operator's -- so this answers "where would a keystroke from this node
/// land", which is the only question worth asking before typing.
pub fn pane_pid(view: &str) -> Option<i32> {
    let out = Command::new("tmux")
        // $TMUX names the socket of the server we are INSIDE, and a node
        // started from within tmux passes it down. Left in place, this asks
        // the wrong server -- or a socket that does not exist -- and the
        // answer is silence, which reads exactly like "no such pane". The
        // spawned client clears it for the same reason (clis/tmux.toml).
        .env_remove("TMUX")
        .args(["display-message", "-t", view, "-p", "#{pane_pid}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}
