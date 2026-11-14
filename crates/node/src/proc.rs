//! Process identity, done properly.
//!
//! The JS original treats a pid as an agent's identity and has the broker call
//! `process.kill(pid, 0)` on pids read from its database. On one machine that
//! is merely racy -- pids are recycled. Across machines it is actively unsafe:
//! every node has a pid 4242, so a broker probing or signalling a remote
//! agent's pid hits an unrelated LOCAL process. This module is the fix, and it
//! lives on the node because only the node shares a kernel with the process.

use anyhow::{Context, Result};

/// Read field 22 of `/proc/<pid>/stat`: process start time in clock ticks
/// since boot. Together with the pid and the node it is a durable identity.
pub fn start_ticks(pid: i32) -> Result<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("reading /proc/{pid}/stat"))?;
    parse_start_ticks(&raw)
}

/// `stat` field 2 is the comm name in parentheses and MAY CONTAIN SPACES AND
/// PARENTHESES (`(my prog (x))`), so fields cannot be split naively. Scan to
/// the LAST ')' and count from there -- field 22 is the 20th token after it.
pub fn parse_start_ticks(raw: &str) -> Result<u64> {
    let close = raw
        .rfind(')')
        .context("malformed /proc stat: no ')' terminating comm")?;
    let rest = &raw[close + 1..];
    let mut fields = rest.split_whitespace();
    // After comm, the next token is field 3 (state). starttime is field 22,
    // i.e. the 20th token from here.
    let tok = fields
        .nth(19)
        .context("malformed /proc stat: fewer than 22 fields")?;
    tok.parse::<u64>()
        .with_context(|| format!("starttime not an integer: {tok:?}"))
}

/// `stat` field 4: the parent pid. Same parsing hazard as field 22 -- comm
/// may contain spaces and parentheses -- so it is counted from the last ')'.
pub fn parse_ppid(raw: &str) -> Result<i32> {
    let close = raw
        .rfind(')')
        .context("malformed /proc stat: no ')' terminating comm")?;
    let mut fields = raw[close + 1..].split_whitespace();
    // The first token after comm is field 3 (state); ppid is field 4.
    let tok = fields.nth(1).context("malformed /proc stat: no ppid field")?;
    tok.parse::<i32>()
        .with_context(|| format!("ppid not an integer: {tok:?}"))
}

pub fn ppid(pid: i32) -> Result<i32> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("reading /proc/{pid}/stat"))?;
    parse_ppid(&raw)
}

/// The pids between this one and init, nearest first.
///
/// Bounded: a corrupted or cyclic chain must not spin a node. 64 is far more
/// than any real tree -- a shim under a CLI under a shell under a tmux pane
/// is four.
pub fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..64 {
        match ppid(cur) {
            Ok(p) if p > 0 && p != cur => {
                out.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    out
}

/// Is `pid` running inside `ancestor`'s subtree?
///
/// This is what makes typing at an adopted terminal safe to allow: a shim
/// registering over this node's socket proves an agent CLI exists, and its
/// ancestry proves WHICH terminal that CLI is sitting in. Neither alone is
/// enough -- a pane may hold a shell, and a CLI may be nowhere near tmux.
pub fn is_descendant_of(pid: i32, ancestor: i32) -> bool {
    pid == ancestor || ancestors(pid).contains(&ancestor)
}

/// Is this exactly the process we started, or merely something wearing its pid?
pub fn is_same_process(pid: i32, expected_start_ticks: u64) -> bool {
    matches!(start_ticks(pid), Ok(t) if t == expected_start_ticks)
}

/// Signal a process only after confirming its identity. Every kill path on the
/// node goes through here, so a stale pid in the hub's database can never
/// cause an unrelated local process to be signalled.
pub fn signal_if_same(pid: i32, expected_start_ticks: u64, sig: nix::sys::signal::Signal) -> Result<bool> {
    if !is_same_process(pid, expected_start_ticks) {
        tracing::warn!(pid, expected_start_ticks, "refusing to signal: identity mismatch");
        return Ok(false);
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), sig)
        .with_context(|| format!("signalling pid {pid}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ppid_survives_a_comm_full_of_parentheses() {
        // The same hazard as starttime, and the same fix: count from the LAST
        // ')'. A program called `(my prog) (x)` is legal and has been seen.
        let raw = "4242 ((my prog) (x)) S 991 4242 991 34816 4242                    4194304 100 0 0 0 1 2 3 4 20 0 1 0 987654 0 0";
        assert_eq!(parse_ppid(raw).unwrap(), 991);
        assert_eq!(parse_start_ticks(raw).unwrap(), 987654);
    }

    #[test]
    fn ppid_of_a_missing_process_is_an_error_not_a_zero() {
        // A zero would read as "child of init", which is a claim.
        assert!(ppid(i32::MAX).is_err());
    }

    #[test]
    fn this_process_is_a_descendant_of_its_own_parent() {
        // The real thing, against the real /proc: the ancestry walk is what
        // decides whether a CLI may be typed at, so it is worth exercising
        // rather than mocking.
        let me = std::process::id() as i32;
        let parent = ppid(me).expect("a test process has a parent");
        assert!(is_descendant_of(me, parent));
        assert!(is_descendant_of(me, me), "a process is in its own subtree");
        // pid 1 is everyone's ancestor; a process is not the ancestor of its
        // own parent.
        assert!(is_descendant_of(me, 1));
        assert!(!is_descendant_of(parent, me));
    }

    #[test]
    fn the_ancestry_walk_is_bounded() {
        // A corrupted or cyclic chain must not spin the node.
        assert!(ancestors(std::process::id() as i32).len() <= 64);
    }

    #[test]
    fn parses_a_plain_stat_line() {
        // pid comm state ppid pgrp session tty tpgid flags minflt cminflt
        // majflt cmajflt utime stime cutime cstime priority nice threads
        // itrealvalue starttime=12345
        let line = "123 (bash) S 1 123 123 0 -1 4194304 1000 0 0 0 10 5 0 0 20 0 1 0 12345 \
                    4096 100 18446744073709551615";
        assert_eq!(parse_start_ticks(line).unwrap(), 12345);
    }

    #[test]
    fn survives_a_comm_containing_spaces_and_parens() {
        // This is the case that breaks naive whitespace splitting.
        let line = "123 (weird (prog) name) S 1 123 123 0 -1 4194304 1000 0 0 0 10 5 0 0 20 0 1 0 \
                    99999 4096 100 0";
        assert_eq!(parse_start_ticks(line).unwrap(), 99999);
    }

    #[test]
    fn rejects_a_truncated_stat_line() {
        assert!(parse_start_ticks("123 (bash) S 1 123").is_err());
    }

    #[test]
    fn reads_our_own_start_time_from_proc() {
        let me = std::process::id() as i32;
        let ticks = start_ticks(me).expect("should read own stat");
        assert!(ticks > 0);
        assert!(is_same_process(me, ticks));
        // A different start time for the same pid must not match.
        assert!(!is_same_process(me, ticks + 1));
    }
}
