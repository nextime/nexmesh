//! Asking tmux where its panes are.
//!
//! Only ever a question, never a command: nothing here sends keys. Typing at
//! an adopted session goes through the pty of the node's own tmux client, so
//! it is the ordinary terminal path and not a second mechanism with its own
//! rules. See clis/tmux.toml for why `send-keys` is deliberately unused.

use std::process::Command;

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
