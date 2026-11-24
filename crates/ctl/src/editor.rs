// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. This program is distributed WITHOUT ANY WARRANTY; see the LICENSE
// file, or <https://www.gnu.org/licenses/>, for the full terms.

//! Handing a file to the operator's own editor.
//!
//! A seat's system prompt is markdown and runs to pages, so neither the
//! console nor a shell command tries to be a text editor: both write a file
//! and open $EDITOR on it, the way `git commit` does. Which editor is the
//! operator's business and is already answered in their environment.

use anyhow::{Context, Result};

/// The editor to use, and why: $VISUAL, then $EDITOR, then whatever is
/// actually installed. `vi` is last because it is the one POSIX promises,
/// not because it is the one anybody wants.
pub fn pick() -> String {
    for key in ["VISUAL", "EDITOR"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    for cand in ["nano", "vim", "vi"] {
        if which(cand) {
            return cand.to_string();
        }
    }
    "vi".to_string()
}

fn which(bin: &str) -> bool {
    std::env::var("PATH").is_ok_and(|path| {
        std::env::split_paths(&path).any(|d| {
            let p = d.join(bin);
            p.is_file() && is_executable(&p)
        })
    })
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// Open `path` in that editor and wait for it.
///
/// $EDITOR may be a command with arguments (`code --wait`, `emacsclient -t`),
/// so it is split on whitespace rather than exec'd whole. The editor inherits
/// this process's terminal: whoever calls this has already given the screen
/// up.
pub fn edit(path: &std::path::Path) -> Result<()> {
    let ed = pick();
    let mut parts = ed.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(parts).arg(path);
    let status = cmd.status().with_context(|| {
        format!(
            "running '{ed}'. Set $EDITOR to an editor you have, or edit the \
             seat's prompt with `nexmesh seat-prompt --system <file>` instead"
        )
    })?;
    if !status.success() {
        // An editor that exits non-zero has usually been quit deliberately
        // (`:cq`), which means "do not use what I typed".
        anyhow::bail!("{ed} exited without saving; nothing was changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_beats_editor_and_both_beat_the_fallback() {
        // Set and read in one test, since the environment is process-wide and
        // two tests racing on it is a flake rather than a failure.
        std::env::set_var("VISUAL", "my-visual");
        std::env::set_var("EDITOR", "my-editor");
        assert_eq!(pick(), "my-visual");
        std::env::remove_var("VISUAL");
        assert_eq!(pick(), "my-editor");
        // An empty one is not an answer: a shell that exports EDITOR= should
        // not send the operator to a program called "".
        std::env::set_var("EDITOR", "   ");
        assert_ne!(pick(), "");
        std::env::remove_var("EDITOR");
    }

    #[test]
    fn an_editor_with_arguments_stays_one_command() {
        // `code --wait` is a real value for $EDITOR, and running a binary
        // called "code --wait" fails with ENOENT.
        let ed = "code --wait";
        let mut parts = ed.split_whitespace();
        assert_eq!(parts.next(), Some("code"));
        assert_eq!(parts.collect::<Vec<_>>(), vec!["--wait"]);
    }
}
