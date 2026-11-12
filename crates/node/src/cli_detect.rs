//! Find which agent CLIs this machine can actually run.
//!
//! Agent CLIs install themselves in awkward places -- on this laptop claude is
//! in ~/.local/bin, opencode in ~/.opencode/bin and qwen in ~/.npm-global/bin,
//! none of which are on a non-login shell's PATH. Each descriptor therefore
//! carries its own `extra_paths`.

use anyhow::{Context, Result};
use nexmesh_proto::cli_descriptor::CliDescriptor;
use nexmesh_proto::DetectedCli;
use std::time::Duration;

/// Agent CLIs are Node/Bun programs whose `--version` can take seconds. Probing
/// them serially delayed node startup -- and therefore every log line -- long
/// enough to look like a hang. Probes run concurrently and each is bounded.
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn load_descriptors(dir: &str) -> Result<Vec<CliDescriptor>> {
    let dir = expand(dir);
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).with_context(|| format!("reading {dir}"))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        match CliDescriptor::parse(&text) {
            // A broken descriptor must not take the node down: skip it loudly.
            Err(e) => tracing::error!(path = %path.display(), error = %e, "ignoring bad descriptor"),
            Ok(d) => out.push(d),
        }
    }
    out.sort_by(|a, b| a.cli.id.cmp(&b.cli.id));
    Ok(out)
}

pub async fn detect(descriptors: &[CliDescriptor]) -> Vec<DetectedCli> {
    let probes = descriptors.iter().filter_map(|d| {
        let path = resolve(&d.cli.command, &d.cli.extra_paths)?;
        Some(async move {
            let version = if d.cli.version_args.is_empty() {
                None
            } else {
                probe_version(&path, &d.cli.version_args, VERSION_PROBE_TIMEOUT).await
            };
            DetectedCli {
                cli_id: d.cli.id.clone(),
                version,
                path: Some(path),
            }
        })
    });
    futures_util::future::join_all(probes).await
}

fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Look up a command on PATH, then in the descriptor's extra paths.
pub fn resolve(command: &str, extra: &[String]) -> Option<String> {
    if command.contains('/') {
        let p = expand(command);
        return is_executable(&p).then_some(p);
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    let dirs = path_var
        .split(':')
        .map(|s| s.to_string())
        .chain(extra.iter().map(|e| expand(e)));
    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let candidate = format!("{}/{}", dir.trim_end_matches('/'), command);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// A CLI that is present but cannot report a version is still usable, so every
/// failure here degrades to `None` rather than dropping the CLI.
pub async fn probe_version(path: &str, args: &[String], timeout: Duration) -> Option<String> {
    let run = tokio::process::Command::new(path)
        .args(args)
        .kill_on_drop(true) // a timed-out probe must not leak a child
        .output();
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string()),
        Ok(Err(e)) => {
            tracing::warn!(path, error = %e, "version probe failed");
            None
        }
        Err(_) => {
            tracing::warn!(path, ?timeout, "version probe timed out");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_command_that_is_certainly_on_path() {
        assert!(resolve("sh", &[]).is_some());
    }

    #[test]
    fn does_not_resolve_a_nonexistent_command() {
        assert!(resolve("nexmesh-definitely-not-a-real-binary", &[]).is_none());
    }

    #[test]
    fn searches_extra_paths_when_not_on_path() {
        let dir = std::env::temp_dir().join(format!("nexmesh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fakecli");
        std::fs::write(&bin, "#!/bin/sh\necho 1.0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(resolve("fakecli", &[]).is_none(), "must not be on PATH");
        let found = resolve("fakecli", &[dir.to_string_lossy().to_string()]);
        assert_eq!(found.as_deref(), Some(bin.to_string_lossy().as_ref()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_slow_version_probe_times_out_instead_of_hanging() {
        let got = probe_version(
            "/bin/sh",
            &["-c".into(), "sleep 30".into()],
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn a_working_version_probe_returns_the_first_line() {
        let got = probe_version(
            "/bin/sh",
            &["-c".into(), "echo 1.2.3; echo extra".into()],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(got.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn a_non_executable_file_is_not_a_cli() {
        let dir = std::env::temp_dir().join(format!("nexmesh-test-ne-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("notexec");
        std::fs::write(&bin, "data").unwrap();
        assert!(resolve("notexec", &[dir.to_string_lossy().to_string()]).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
