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
