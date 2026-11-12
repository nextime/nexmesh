//! `nexmesh install-orchestrator` -- make the orchestrator available inside
//! whichever agent CLI you actually use.
//!
//! Running a fleet should not require one particular CLI, so this registers
//! `nexmesh-orch` as an MCP server in any of them. Where a CLI offers its own
//! `mcp add`, that is used in preference to editing its config by hand: it is
//! the CLI's own format to change.

use anyhow::{bail, Context, Result};
use nexmesh_proto::cli_descriptor::{render_entry, set_pointer, CliDescriptor, UserInstall};
use std::path::PathBuf;

fn expand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Absolute, because the CLI that launches it will not share our directory.
fn orch_path(given: Option<&str>) -> Result<String> {
    if let Some(p) = given {
        let abs = std::fs::canonicalize(expand(p))
            .with_context(|| format!("{p} does not exist"))?;
        return Ok(abs.to_string_lossy().to_string());
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("nexmesh-orch")))
        .filter(|p| p.exists());
    match sibling {
        Some(p) => Ok(std::fs::canonicalize(&p)?.to_string_lossy().to_string()),
        None => bail!(
            "cannot find nexmesh-orch next to this binary; pass --orch <path> \
             (it must be an absolute path the CLI can run)"
        ),
    }
}

pub struct Opts<'a> {
    pub cli_id: &'a str,
    pub clis_dir: &'a str,
    pub server_name: &'a str,
    pub orch: Option<&'a str>,
    pub hub: &'a str,
    pub token: Option<&'a str>,
    pub ca_cert: Option<&'a str>,
    pub dry_run: bool,
}

pub fn install(o: Opts) -> Result<String> {
    let dir = expand(o.clis_dir);
    let path = format!("{dir}/{}.toml", o.cli_id);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!("no descriptor for '{}' in {dir} (try --clis <dir>)", o.cli_id)
    })?;
    let d = CliDescriptor::parse(&text)?;
    let install = d
        .user_install()
        .with_context(|| format!("'{}' does not say how to register a server in its own config",
                                 o.cli_id))?
        .clone();

    let orch = orch_path(o.orch)?;
    let mut env: Vec<(String, String)> = vec![("NEXMESH_HUB_HTTP".into(), o.hub.to_string())];
    if let Some(t) = o.token {
        env.push(("NEXMESH_OPERATOR_TOKEN".into(), t.to_string()));
    }
    if let Some(c) = o.ca_cert {
        env.push(("NEXMESH_CA_CERT".into(), expand(c)));
    }

    match install {
        UserInstall::Command { argv, env_flag } => {
            let bin = crate::tlstrust::noop_resolve(&d.cli.command, &d.cli.extra_paths)
                .with_context(|| format!("'{}' is not installed here", d.cli.command))?;
            // Environment flags must precede the server name, which is a
            // positional argument.
            let mut rendered: Vec<String> = Vec::new();
            for a in &argv {
                if a == "{name}" {
                    if let Some(flag) = &env_flag {
                        for (k, v) in &env {
                            rendered.push(flag.clone());
                            rendered.push(format!("{k}={v}"));
                        }
                    }
                    rendered.push(o.server_name.to_string());
                } else {
                    rendered.push(a.replace("{command}", &orch).replace("{name}", o.server_name));
                }
            }
            if o.dry_run {
                return Ok(format!("would run: {bin} {}", redact(&rendered).join(" ")));
            }
            let out = std::process::Command::new(&bin)
                .args(&rendered)
                .output()
                .with_context(|| format!("running {bin} {}", redact(&rendered).join(" ")))?;
            if !out.status.success() {
                bail!(
                    "{} refused the registration: {}{}",
                    d.cli.display_name,
                    String::from_utf8_lossy(&out.stderr).trim(),
                    String::from_utf8_lossy(&out.stdout).trim()
                );
            }
            Ok(format!(
                "Registered '{}' with {} (via its own `mcp add`).",
                o.server_name, d.cli.display_name
            ))
        }

        UserInstall::JsonFile { path, pointer, entry, env_key } => {
            let full = PathBuf::from(expand(&path));
            let mut root: serde_json::Value = match std::fs::read_to_string(&full) {
                Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t).with_context(|| {
                    format!("{} is not valid JSON; refusing to overwrite it", full.display())
                })?,
                _ => serde_json::json!({}),
            };
            let value = render_entry(&entry, env_key.as_deref(), &orch, &env);
            let ptr = pointer.replace("{name}", o.server_name);
            set_pointer(&mut root, &ptr, value).map_err(|e| anyhow::anyhow!(e))?;
            if o.dry_run {
                return Ok(format!("would write {} at {ptr}", full.display()));
            }
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::write(&full, serde_json::to_string_pretty(&root)?)
                .with_context(|| format!("writing {}", full.display()))?;
            Ok(format!(
                "Registered '{}' with {} in {}.",
                o.server_name,
                d.cli.display_name,
                full.display()
            ))
        }

        UserInstall::TomlFile { path, key, entry, env_key } => {
            let full = PathBuf::from(expand(&path));
            let mut root: toml::Value = match std::fs::read_to_string(&full) {
                Ok(t) if !t.trim().is_empty() => t.parse().with_context(|| {
                    format!("{} is not valid TOML; refusing to overwrite it", full.display())
                })?,
                _ => toml::Value::Table(Default::default()),
            };
            let json = render_entry(&entry, env_key.as_deref(), &orch, &env);
            let as_toml: toml::Value = toml::Value::try_from(json)?;
            let key = key.replace("{name}", o.server_name);
            let parts: Vec<&str> = key.split('.').collect();
            let mut cur = &mut root;
            for p in &parts[..parts.len() - 1] {
                cur = cur
                    .as_table_mut()
                    .with_context(|| format!("{key} traverses a non-table"))?
                    .entry(p.to_string())
                    .or_insert_with(|| toml::Value::Table(Default::default()));
            }
            cur.as_table_mut()
                .with_context(|| format!("{key} traverses a non-table"))?
                .insert(parts[parts.len() - 1].to_string(), as_toml);
            if o.dry_run {
                return Ok(format!("would write {} at {key}", full.display()));
            }
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::write(&full, toml::to_string_pretty(&root)?)
                .with_context(|| format!("writing {}", full.display()))?;
            Ok(format!(
                "Registered '{}' with {} in {}.",
                o.server_name,
                d.cli.display_name,
                full.display()
            ))
        }
    }
}

/// Never print a token back at the user, even in a dry run.
fn redact(argv: &[String]) -> Vec<String> {
    argv.iter()
        .map(|a| match a.split_once('=') {
            Some((k, _)) if k.ends_with("TOKEN") => format!("{k}=<redacted>"),
            _ => a.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_never_echoed() {
        let argv = vec![
            "-e".into(),
            "NEXMESH_OPERATOR_TOKEN=supersecret".into(),
            "-e".into(),
            "NEXMESH_HUB_HTTP=http://h:7870".into(),
        ];
        let r = redact(&argv).join(" ");
        assert!(!r.contains("supersecret"), "{r}");
        assert!(r.contains("<redacted>"), "{r}");
        assert!(r.contains("http://h:7870"), "the harmless one should survive: {r}");
    }
}
