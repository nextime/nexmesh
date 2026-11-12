//! Keeping a shared workspace in step across machines.
//!
//! A `repo` workspace is the same logical tree checked out on several
//! machines. Locks decide who may edit a path; this decides how the bytes
//! travel. Three choices worth stating plainly:
//!
//! **Git is the transport, and the declared remote is the authority.** The hub
//! coordinates but is not a git server: making it one would mean implementing
//! smart HTTP and holding everybody's code, when every machine already has git
//! and a remote they can reach.
//!
//! **Ownership zones are the conflict-avoidance strategy.** Agents are given
//! disjoint globs, so in normal operation two of them never touch the same
//! file and a rebase is trivial.
//!
//! **A conflict is reported, never resolved.** Guessing at a merge would
//! silently destroy somebody's work. The agent is told, in its own messages,
//! and decides.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

pub struct GitOutcome {
    pub ok: bool,
    pub head: Option<String>,
    pub detail: String,
    pub conflict: bool,
    pub changed: Vec<String>,
}

impl GitOutcome {
    fn fail(detail: impl Into<String>) -> Self {
        Self { ok: false, head: None, detail: detail.into(), conflict: false, changed: vec![] }
    }
    fn conflicted(detail: impl Into<String>, files: Vec<String>) -> Self {
        Self { ok: false, head: None, detail: detail.into(), conflict: true, changed: files }
    }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == 0
    }
    fn text(&self) -> String {
        format!("{}{}", self.out.trim(), self.err.trim()).trim().to_string()
    }
}

async fn git(dir: &str, args: &[&str]) -> Result<Run> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    // Never block on a credential prompt: a node runs unattended, and a
    // hanging git is indistinguishable from a wedged agent.
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env("GIT_ASKPASS", "echo");
    c.args(args);
    let o = c.output().await.with_context(|| format!("running git {}", args.join(" ")))?;
    Ok(Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).to_string(),
        err: String::from_utf8_lossy(&o.stderr).to_string(),
    })
}

async fn head(dir: &str) -> Option<String> {
    let r = git(dir, &["rev-parse", "HEAD"]).await.ok()?;
    r.ok().then(|| r.out.trim().to_string())
}

/// Files differing from the branch tip, for reporting what a sync moved.
async fn changed_since(dir: &str, from: &str) -> Vec<String> {
    match git(dir, &["diff", "--name-only", &format!("{from}..HEAD")]).await {
        Ok(r) if r.ok() => r.out.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect(),
        _ => vec![],
    }
}

/// Does this checkout have any commits at all?
///
/// A brand-new repository has an "unborn" HEAD, which is how every shared
/// workspace starts. Rebasing or pushing from that state fails in ways that
/// read like a broken remote, so it is checked for explicitly.
async fn has_commits(dir: &str) -> bool {
    matches!(git(dir, &["rev-parse", "--verify", "HEAD"]).await, Ok(r) if r.ok())
}

async fn remote_has_branch(dir: &str, branch: &str) -> bool {
    matches!(
        git(dir, &["rev-parse", "--verify", &format!("origin/{branch}")]).await,
        Ok(r) if r.ok()
    )
}

/// A checkout refused because local files would be overwritten is a conflict,
/// not a malfunction: the agent has work that collides with what is already on
/// the branch. git names the files, so pass them on.
fn overwrite_conflict(text: &str) -> Option<Vec<String>> {
    let marker = text.find("would be overwritten")?;
    let files: Vec<String> = text[marker..]
        .lines()
        .skip(1)
        .map(|l| l.trim())
        .take_while(|l| !l.is_empty() && !l.starts_with("Please") && !l.starts_with("Aborting"))
        .map(|l| l.to_string())
        .collect();
    Some(files)
}

async fn conflicted_files(dir: &str) -> Vec<String> {
    match git(dir, &["diff", "--name-only", "--diff-filter=U"]).await {
        Ok(r) if r.ok() => r.out.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect(),
        _ => vec![],
    }
}

/// Make sure the checkout exists and is on the right branch.
pub async fn ensure(path: &str, repo_url: &str, branch: &str) -> GitOutcome {
    let exists = Path::new(path).join(".git").exists();
    if !exists {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut c = Command::new("git");
        c.env("GIT_TERMINAL_PROMPT", "0").env("GIT_ASKPASS", "echo");
        c.args(["clone", "--branch", branch, repo_url, path]);
        match c.output().await {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                // A brand-new repository has no branch to clone yet; that is a
                // normal starting state, not a failure.
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                let mut c2 = Command::new("git");
                c2.env("GIT_TERMINAL_PROMPT", "0").env("GIT_ASKPASS", "echo");
                c2.args(["clone", repo_url, path]);
                match c2.output().await {
                    Ok(o2) if o2.status.success() => {
                        let _ = git(path, &["checkout", "-B", branch]).await;
                    }
                    _ => return GitOutcome::fail(format!("cloning {repo_url}: {}", err.trim())),
                }
            }
            Err(e) => return GitOutcome::fail(format!("cloning {repo_url}: {e}")),
        }
    } else {
        let _ = git(path, &["remote", "set-url", "origin", repo_url]).await;
        let f = git(path, &["fetch", "origin"]).await;
        if let Ok(r) = &f {
            if !r.ok() {
                return GitOutcome::fail(format!("fetching origin: {}", r.text()));
            }
        }
        // Get onto the branch, creating it from the remote tip if need be.
        let on = git(path, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
        let current = on.map(|r| r.out.trim().to_string()).unwrap_or_default();
        if current != branch {
            let tracked = format!("origin/{branch}");
            let r = git(path, &["checkout", "-B", branch, &tracked]).await;
            if matches!(&r, Ok(x) if !x.ok()) {
                let _ = git(path, &["checkout", "-B", branch]).await;
            }
        }
    }
    // An agent commits as itself; without an identity git refuses outright.
    let _ = git(path, &["config", "user.name", "nexmesh"]).await;
    let _ = git(path, &["config", "user.email", "nexmesh@localhost"]).await;
    GitOutcome {
        ok: true,
        head: head(path).await,
        detail: format!("workspace ready on {branch}"),
        conflict: false,
        changed: vec![],
    }
}

/// Bring this checkout up to date with the branch, keeping local edits.
pub async fn refresh(path: &str, branch: &str) -> GitOutcome {
    let before = head(path).await.unwrap_or_default();
    let f = git(path, &["fetch", "origin"]).await;
    if let Ok(r) = &f {
        if !r.ok() {
            return GitOutcome::fail(format!("fetching origin: {}", r.text()));
        }
    }

    // Nothing committed here yet: adopt the branch rather than rebase onto it.
    if !has_commits(path).await {
        if !remote_has_branch(path, branch).await {
            return GitOutcome {
                ok: true,
                head: None,
                detail: format!("nothing on {branch} yet; nothing to pull"),
                conflict: false,
                changed: vec![],
            };
        }
        let r = r_adopt(path, branch).await;
        return match r {
            Ok((true, _)) => GitOutcome {
                ok: true,
                head: head(path).await,
                detail: format!("took the current state of {branch}"),
                conflict: false,
                changed: vec![],
            },
            Ok((false, text)) => match overwrite_conflict(&text) {
                Some(files) => GitOutcome::conflicted(
                    format!(
                        "your workspace already has {} that {branch} also contains; nothing was \
                         changed. Move or remove them, then refresh again.",
                        if files.is_empty() { "files".into() } else { files.join(", ") }
                    ),
                    files,
                ),
                None => GitOutcome::fail(format!("adopting {branch}: {text}")),
            },
            Err(e) => GitOutcome::fail(e.to_string()),
        };
    }
    // --autostash keeps work in progress: an agent mid-edit must not have its
    // uncommitted changes refused or thrown away.
    let r = match git(path, &["rebase", "--autostash", &format!("origin/{branch}")]).await {
        Ok(r) => r,
        Err(e) => return GitOutcome::fail(e.to_string()),
    };
    if !r.ok() {
        let files = conflicted_files(path).await;
        let _ = git(path, &["rebase", "--abort"]).await;
        return GitOutcome::conflicted(
            format!(
                "refresh hit a conflict and was rolled back; the workspace is untouched. \
                 Conflicting: {}",
                if files.is_empty() { "see git status".into() } else { files.join(", ") }
            ),
            files,
        );
    }
    let now = head(path).await;
    let changed = if before.is_empty() { vec![] } else { changed_since(path, &before).await };
    GitOutcome {
        ok: true,
        detail: if now.as_deref() == Some(before.as_str()) {
            "already up to date".into()
        } else {
            format!("updated; {} file(s) changed", changed.len())
        },
        head: now,
        conflict: false,
        changed,
    }
}

/// Take the remote branch as this checkout's starting point.
/// Returns (succeeded, git's own words).
async fn r_adopt(path: &str, branch: &str) -> Result<(bool, String)> {
    let r = git(path, &["checkout", "-B", branch, &format!("origin/{branch}")]).await?;
    Ok((r.ok(), r.text()))
}

/// Commit everything in this checkout and share it on the branch.
pub async fn publish(path: &str, branch: &str, author: &str, message: &str) -> GitOutcome {
    let _ = git(path, &["fetch", "origin"]).await;
    // With no commits here but a branch on the remote, start from the remote
    // rather than creating a second, unrelated history that can never merge.
    if !has_commits(path).await && remote_has_branch(path, branch).await {
        if let Ok(x) = &r_adopt(path, branch).await {
            if !x.0 {
                let text = &x.1;
                return match overwrite_conflict(text) {
                    Some(files) => GitOutcome::conflicted(
                        format!(
                            "your workspace already has {} that {branch} also contains, so \
                             nothing was published and your files are untouched. Refresh after \
                             moving or removing them, or publish from a clean checkout.",
                            if files.is_empty() { "files".into() } else { files.join(", ") }
                        ),
                        files,
                    ),
                    None => GitOutcome::fail(format!("could not start from {branch}: {text}")),
                };
            }
        }
    }

    let status = match git(path, &["status", "--porcelain"]).await {
        Ok(r) => r,
        Err(e) => return GitOutcome::fail(e.to_string()),
    };
    let dirty = !status.out.trim().is_empty();
    if dirty {
        let _ = git(path, &["add", "-A"]).await;
        let who = format!("{author} <nexmesh@localhost>");
        let r = git(path, &["commit", "--author", &who, "-m", message]).await;
        if matches!(&r, Ok(x) if !x.ok()) {
            let detail = r.map(|x| x.text()).unwrap_or_default();
            if !detail.contains("nothing to commit") {
                return GitOutcome::fail(format!("committing: {detail}"));
            }
        }
    }

    // Still nothing committed anywhere: there is genuinely nothing to share.
    if !has_commits(path).await {
        return GitOutcome {
            ok: true,
            head: None,
            detail: format!("nothing of yours to publish; {branch} is empty"),
            conflict: false,
            changed: vec![],
        };
    }

    // Rebase onto the branch tip and push. Two agents publishing at the same
    // moment is the normal case, not an exception, so retry the race.
    for attempt in 0..3 {
        let f = git(path, &["fetch", "origin"]).await;
        if let Ok(r) = &f {
            if !r.ok() && attempt == 0 {
                return GitOutcome::fail(format!("fetching origin: {}", r.text()));
            }
        }
        let remote = format!("origin/{branch}");
        if remote_has_branch(path, branch).await {
            let r = git(path, &["rebase", "--autostash", &remote]).await;
            if matches!(&r, Ok(x) if !x.ok()) {
                let files = conflicted_files(path).await;
                let _ = git(path, &["rebase", "--abort"]).await;
                return GitOutcome::conflicted(
                    format!(
                        "your work conflicts with what is already on {branch}, so nothing was \
                         published and your checkout is unchanged. Conflicting: {}. Resolve it, \
                         then publish again.",
                        if files.is_empty() { "see git status".into() } else { files.join(", ") }
                    ),
                    files,
                );
            }
        }
        let p = git(path, &["push", "origin", &format!("HEAD:{branch}")]).await;
        match p {
            Ok(r) if r.ok() => {
                return GitOutcome {
                    ok: true,
                    head: head(path).await,
                    detail: if dirty {
                        format!("published to {branch}")
                    } else {
                        format!("nothing of yours to publish; {branch} is up to date")
                    },
                    conflict: false,
                    changed: status
                        .out
                        .lines()
                        .filter_map(|l| l.get(3..).map(|s| s.to_string()))
                        .collect(),
                };
            }
            Ok(r) => {
                if attempt == 2 {
                    return GitOutcome::fail(format!("pushing to {branch}: {}", r.text()));
                }
                // Somebody else pushed first; go round again.
            }
            Err(e) => return GitOutcome::fail(e.to_string()),
        }
    }
    GitOutcome::fail("could not publish after three attempts")
}

/// What is going on in this checkout, without changing anything.
pub async fn status(path: &str, branch: &str) -> GitOutcome {
    let st = match git(path, &["status", "--porcelain"]).await {
        Ok(r) => r,
        Err(e) => return GitOutcome::fail(e.to_string()),
    };
    let local: Vec<String> = st
        .out
        .lines()
        .filter_map(|l| l.get(3..).map(|s| s.to_string()))
        .collect();
    let _ = git(path, &["fetch", "origin"]).await;
    let behind = git(path, &["rev-list", "--count", &format!("HEAD..origin/{branch}")]).await
        .map(|r| r.out.trim().to_string())
        .unwrap_or_default();
    let ahead = git(path, &["rev-list", "--count", &format!("origin/{branch}..HEAD")]).await
        .map(|r| r.out.trim().to_string())
        .unwrap_or_default();
    GitOutcome {
        ok: true,
        head: head(path).await,
        detail: format!(
            "{} uncommitted file(s); {} commit(s) ahead, {} behind {branch}",
            local.len(),
            if ahead.is_empty() { "?".into() } else { ahead },
            if behind.is_empty() { "?".into() } else { behind }
        ),
        conflict: false,
        changed: local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &str, args: &[&str]) {
        let _ = git(dir, args).await;
    }

    fn tmp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!("nexmesh-git-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.to_string_lossy().to_string()
    }

    /// A bare repository standing in for the shared remote.
    async fn remote() -> String {
        let d = tmp("remote");
        let o = Command::new("git")
            .args(["init", "--bare", "--initial-branch", "main", &d])
            .output()
            .await
            .unwrap();
        assert!(o.status.success());
        d
    }

    #[tokio::test]
    async fn two_checkouts_exchange_work_through_the_remote() {
        let origin = remote().await;
        let a = tmp("a");
        let b = tmp("b");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();

        let r = ensure(&a, &origin, "main").await;
        assert!(r.ok, "{}", r.detail);
        let r = ensure(&b, &origin, "main").await;
        assert!(r.ok, "{}", r.detail);

        // A writes a file in its own zone and publishes.
        std::fs::write(format!("{a}/from-a.txt"), "hello from A").unwrap();
        let r = publish(&a, "main", "Agent A", "A: add a file").await;
        assert!(r.ok, "{}", r.detail);

        // B has not seen it yet.
        assert!(!Path::new(&format!("{b}/from-a.txt")).exists());
        let r = refresh(&b, "main").await;
        assert!(r.ok, "{}", r.detail);
        assert_eq!(
            std::fs::read_to_string(format!("{b}/from-a.txt")).unwrap(),
            "hello from A",
            "B should now have A's work"
        );

        // And back the other way, with both having diverged.
        std::fs::write(format!("{b}/from-b.txt"), "hello from B").unwrap();
        let r = publish(&b, "main", "Agent B", "B: add a file").await;
        assert!(r.ok, "{}", r.detail);
        let r = refresh(&a, "main").await;
        assert!(r.ok, "{}", r.detail);
        assert!(Path::new(&format!("{a}/from-b.txt")).exists(), "A should have B's work");

        for d in [origin, a, b] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn a_conflict_is_reported_and_nothing_is_destroyed() {
        let origin = remote().await;
        let a = tmp("ca");
        let b = tmp("cb");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
        ensure(&a, &origin, "main").await;
        ensure(&b, &origin, "main").await;

        std::fs::write(format!("{a}/shared.txt"), "A's version").unwrap();
        assert!(publish(&a, "main", "A", "A").await.ok);
        // B edits the same file without refreshing first.
        std::fs::write(format!("{b}/shared.txt"), "B's version").unwrap();
        let r = publish(&b, "main", "B", "B").await;

        assert!(!r.ok, "publishing a conflicting change must not succeed");
        assert!(r.conflict, "it should be reported AS a conflict: {}", r.detail);
        assert!(r.detail.contains("shared.txt"), "name the file: {}", r.detail);
        // The point: B's work is still there, unmangled.
        assert_eq!(std::fs::read_to_string(format!("{b}/shared.txt")).unwrap(), "B's version");

        for d in [origin, a, b] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn refreshing_keeps_uncommitted_work() {
        let origin = remote().await;
        let a = tmp("ra");
        let b = tmp("rb");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
        ensure(&a, &origin, "main").await;
        ensure(&b, &origin, "main").await;
        std::fs::write(format!("{a}/seed.txt"), "seed").unwrap();
        publish(&a, "main", "A", "seed").await;

        // B is mid-edit on a file nobody else touches.
        std::fs::write(format!("{b}/mine.txt"), "work in progress").unwrap();
        let r = refresh(&b, "main").await;
        assert!(r.ok, "{}", r.detail);
        assert!(Path::new(&format!("{b}/seed.txt")).exists(), "should have pulled seed.txt");
        assert_eq!(
            std::fs::read_to_string(format!("{b}/mine.txt")).unwrap(),
            "work in progress",
            "an agent's uncommitted work must survive a refresh"
        );

        for d in [origin, a, b] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn publishing_nothing_is_not_an_error() {
        let origin = remote().await;
        let a = tmp("na");
        std::fs::remove_dir_all(&a).ok();
        ensure(&a, &origin, "main").await;
        let r = publish(&a, "main", "A", "empty").await;
        assert!(r.ok, "{}", r.detail);
        assert!(r.detail.contains("nothing"), "{}", r.detail);
        for d in [origin, a] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn an_unreachable_remote_fails_without_hanging() {
        // A node runs unattended: git must never sit at a credential prompt.
        let a = tmp("unreach");
        std::fs::remove_dir_all(&a).ok();
        let r = ensure(&a, "https://127.0.0.1:1/nope.git", "main").await;
        assert!(!r.ok);
        assert!(r.detail.contains("cloning"), "{}", r.detail);
        std::fs::remove_dir_all(&a).ok();
    }

    #[tokio::test]
    async fn status_reports_without_changing_anything() {
        let origin = remote().await;
        let a = tmp("sa");
        std::fs::remove_dir_all(&a).ok();
        ensure(&a, &origin, "main").await;
        std::fs::write(format!("{a}/x.txt"), "x").unwrap();
        let r = status(&a, "main").await;
        assert!(r.ok, "{}", r.detail);
        assert!(r.detail.contains("1 uncommitted"), "{}", r.detail);
        // Still uncommitted afterwards: status must not commit anything.
        let st = git(&a, &["status", "--porcelain"]).await.unwrap();
        assert!(st.out.contains("x.txt"));
        for d in [origin, a] {
            std::fs::remove_dir_all(d).ok();
        }
        let _ = sh(".", &["--version"]).await;
    }
}
