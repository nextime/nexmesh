// nexmesh -- multi-agent orchestration for agent CLIs across machines.
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the GNU General
// Public License as published by the Free Software Foundation, either version 3
// of the License, or (at your option) any later version. This program is
// distributed WITHOUT ANY WARRANTY; see the LICENSE file, or
// <https://www.gnu.org/licenses/>, for the full terms.

//! Putting a file where an agent can read it.
//!
//! An agent reads files; an operator has a screenshot, a PDF, a recording. So
//! the file lands in the workspace on a machine that holds it, and the team is
//! told the path. Only a node ever writes there -- the hub has no file system
//! of anybody else's, by design.
//!
//! Everything about the destination is checked HERE as well as at the hub,
//! because this is the side that actually touches the disk: a path that
//! escapes the workspace is the one bug in an upload feature that matters.

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// What a single upload may weigh. Generous for a screenshot or a document,
/// small enough that a hub carrying it in one JSON message stays honest; a
/// film belongs in the workspace by other means, with the path handed over.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;

/// The destination for `rel` under `root`, refusing anything that leaves it.
///
/// `..`, absolute paths and symlinked parents are all ways out of a directory,
/// and an upload endpoint that can write anywhere is a remote shell.
pub fn safe_join(root: &str, rel: &str) -> Result<PathBuf> {
    let root = Path::new(root);
    if !root.is_absolute() {
        bail!("the workspace root must be an absolute path");
    }
    let rel = Path::new(rel);
    for c in rel.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => bail!("'{}' must stay inside the workspace", rel.display()),
        }
    }
    let joined = root.join(rel);
    // The parent must resolve to somewhere inside the root once symlinks are
    // followed. The file itself need not exist yet.
    let parent = joined.parent().unwrap_or(root).to_path_buf();
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let real_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolving {}", root.display()))?;
    let real_parent = std::fs::canonicalize(&parent)
        .with_context(|| format!("resolving {}", parent.display()))?;
    if !real_parent.starts_with(&real_root) {
        bail!("that path leaves the workspace");
    }
    Ok(real_parent.join(joined.file_name().unwrap_or_default()))
}

/// Write it, and say where it went and how big it is.
pub async fn write(root: &str, rel: &str, data_b64: &str) -> Result<(String, u64)> {
    let bytes = decode(data_b64)?;
    if bytes.len() > MAX_BYTES {
        bail!(
            "that file is {} bytes, over the {MAX_BYTES}-byte limit for one upload",
            bytes.len()
        );
    }
    let path = safe_join(root, rel)?;
    tokio::fs::write(&path, &bytes)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok((path.to_string_lossy().to_string(), bytes.len() as u64))
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// base64, including the data-URI and URL-safe shapes a browser produces.
fn decode(s: &str) -> Result<Vec<u8>> {
    let s = s.rsplit_once("base64,").map(|(_, b)| b).unwrap_or(s);
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        let v = B64
            .iter()
            .position(|c| *c == ch)
            .or(match ch {
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            })
            .context("that was not base64")? as u32;
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

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nexmesh-putfile-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn a_path_cannot_leave_the_workspace() {
        let root = tmp();
        let r = root.to_string_lossy().to_string();
        assert!(safe_join(&r, "uploads/shot.png").is_ok());
        assert!(safe_join(&r, "a/b/c.txt").is_ok());
        // Every way out of a directory.
        assert!(safe_join(&r, "../escape.txt").is_err());
        assert!(safe_join(&r, "uploads/../../escape.txt").is_err());
        assert!(safe_join(&r, "/etc/passwd").is_err());
        // A relative root is a configuration error, not a path to guess at.
        assert!(safe_join("relative/root", "x.txt").is_err());
    }

    #[test]
    fn a_symlinked_parent_does_not_become_a_way_out() {
        let root = tmp();
        let out = root.join("outside");
        std::fs::create_dir_all(&out).unwrap();
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let link = ws.join("sneaky");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&out, &link).unwrap();
        let r = ws.to_string_lossy().to_string();
        assert!(safe_join(&r, "sneaky/x.txt").is_err(), "a symlink out is still out");
    }

    #[tokio::test]
    async fn a_file_arrives_where_it_was_asked_for() {
        let root = tmp();
        let r = root.to_string_lossy().to_string();
        // "hello" in base64, as a data URI, which is what a browser hands over.
        let (path, n) = write(&r, "uploads/hi.txt", "data:text/plain;base64,aGVsbG8=")
            .await
            .unwrap();
        assert_eq!(n, 5);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(path.ends_with("uploads/hi.txt"), "{path}");
    }

    #[test]
    fn base64_from_a_browser_is_read_in_every_shape_it_arrives() {
        assert_eq!(decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode("data:image/png;base64,aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode("aGVs\nbG8=").unwrap(), b"hello");
        assert!(decode("not base64 at all!!").is_err());
    }
}
