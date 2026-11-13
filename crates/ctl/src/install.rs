//! Registering one of our MCP servers inside whichever agent CLI you use.
//!
//! Two things get installed this way, and they are not the same thing:
//!
//! * the ORCHESTRATOR (`nexmesh-orch`), which gives a CLI the tools to drive
//!   the fleet -- create sessions, add agents, move them about;
//! * a SEAT (`nexmesh-mcpd`), which makes the CLI itself a member of a team.
//!   Every conversation that loads it takes its own seat, keyed on the CLI's
//!   own conversation id, and goes on answering from wherever it is running.
//!
//! Running a fleet should not require one particular CLI, so either can go
//! into any of them. Where a CLI offers its own `mcp add`, that is used in
//! preference to editing its config by hand: it is the CLI's own format to
//! change.

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
fn binary_path(given: Option<&str>, name: &str, flag: &str) -> Result<String> {
    if let Some(p) = given {
        let abs = std::fs::canonicalize(expand(p))
            .with_context(|| format!("{p} does not exist"))?;
        return Ok(abs.to_string_lossy().to_string());
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists());
    match sibling {
        Some(p) => Ok(std::fs::canonicalize(&p)?.to_string_lossy().to_string()),
        None => bail!(
            "cannot find {name} next to this binary; pass {flag} <path> \
             (it must be an absolute path the CLI can run)"
        ),
    }
}

pub struct Opts<'a> {
    pub cli_id: &'a str,
    pub clis_dir: &'a str,
    pub server_name: &'a str,
    /// Absolute path to the binary the CLI will launch as its MCP server.
    pub command: String,
    /// Environment the CLI must pass to it. Values are redacted when echoed.
    pub env: Vec<(String, String)>,
    pub dry_run: bool,
}

/// `nexmesh install-orchestrator`: the fleet's tools, inside your CLI.
pub struct OrchOpts<'a> {
    pub cli_id: &'a str,
    pub clis_dir: &'a str,
    pub server_name: &'a str,
    pub orch: Option<&'a str>,
    pub hub: &'a str,
    pub token: Option<&'a str>,
    pub ca_cert: Option<&'a str>,
    pub dry_run: bool,
}

pub fn install_orchestrator(o: OrchOpts) -> Result<String> {
    let mut env: Vec<(String, String)> = vec![("NEXMESH_HUB_HTTP".into(), o.hub.to_string())];
    if let Some(t) = o.token {
        env.push(("NEXMESH_OPERATOR_TOKEN".into(), t.to_string()));
    }
    if let Some(c) = o.ca_cert {
        env.push(("NEXMESH_CA_CERT".into(), expand(c)));
    }
    install(Opts {
        cli_id: o.cli_id,
        clis_dir: o.clis_dir,
        server_name: o.server_name,
        command: binary_path(o.orch, "nexmesh-orch", "--orch")?,
        env,
        dry_run: o.dry_run,
    })
}

/// `nexmesh install-seat`: this CLI becomes a team member.
///
/// Deliberately thin. The shim holds no credentials and never speaks to the
/// hub -- it finds the node on this machine over a unix socket, and the node
/// asks for the seat. So there is nothing here to leak, and a conversation
/// started by hand joins on exactly the same footing as one the hub spawned.
pub struct SeatOpts<'a> {
    pub cli_id: &'a str,
    pub clis_dir: &'a str,
    pub server_name: &'a str,
    pub mcpd: Option<&'a str>,
    pub sock: &'a str,
    /// Session id or name. Worth setting: without it a CLI started outside
    /// any workspace has to guess, and refuses when more than one session is
    /// active.
    pub session: Option<&'a str>,
    pub role: Option<&'a str>,
    /// Usually left alone. A fixed name makes every conversation in this CLI
    /// ask for the SAME seat, which is the duplication it looks like a fix
    /// for; unset, the hub names each one as it arrives.
    pub name: Option<&'a str>,
    pub dry_run: bool,
}

pub fn install_seat(o: SeatOpts) -> Result<String> {
    let mut env: Vec<(String, String)> = vec![
        ("NEXMESH_SOCK".into(), o.sock.to_string()),
        ("NEXMESH_CLI".into(), o.cli_id.to_string()),
    ];
    if let Some(s) = o.session {
        env.push(("NEXMESH_SESSION".into(), s.to_string()));
    }
    if let Some(r) = o.role {
        env.push(("NEXMESH_ROLE".into(), r.to_string()));
    }
    if let Some(n) = o.name {
        env.push(("NEXMESH_NAME".into(), n.to_string()));
    }
    install(Opts {
        cli_id: o.cli_id,
        clis_dir: o.clis_dir,
        server_name: o.server_name,
        command: binary_path(o.mcpd, "nexmesh-mcpd", "--mcpd")?,
        env,
        dry_run: o.dry_run,
    })
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

    let orch = o.command.clone();
    let env = o.env.clone();

    match install {
        UserInstall::Command { argv, env_flag } => {
            let bin = crate::tlstrust::noop_resolve(&d.cli.command, &d.cli.extra_paths)
                .with_context(|| format!("'{}' is not installed here", d.cli.command))?;
            // Environment flags go AFTER the server name, which is where
            // `claude mcp add`'s own help puts them -- and it is not a matter
            // of taste: its `-e` takes a VARIADIC value, so flags placed
            // before the name have the name swallowed as another variable and
            // the command fails with "missing required argument
            // 'commandOrUrl'". Verified against Claude Code 2.1.323.
            let mut rendered: Vec<String> = Vec::new();
            for a in &argv {
                if a == "{name}" {
                    rendered.push(o.server_name.to_string());
                    if let Some(flag) = &env_flag {
                        for (k, v) in &env {
                            rendered.push(flag.clone());
                            rendered.push(format!("{k}={v}"));
                        }
                    }
                } else {
                    rendered.push(a.replace("{command}", &orch).replace("{name}", o.server_name));
                }
            }
            // The descriptor's own environment has to be applied, because it
            // is what makes a named configuration a configuration: a second
            // Claude account is the same binary pointed at another config
            // directory, and `mcp add` run without that writes the
            // registration into the FIRST account's file -- reporting
            // success, in the wrong place, for an account that never gets a
            // seat.
            let mut cmd = std::process::Command::new(&bin);
            cmd.args(&rendered);
            for (k, v) in &d.cli.env {
                cmd.env(k, expand(v));
            }
            for k in &d.cli.env_unset {
                cmd.env_remove(k);
            }
            if o.dry_run {
                let shown = d
                    .cli
                    .env
                    .iter()
                    .map(|(k, v)| format!("{k}={} ", expand(v)))
                    .collect::<String>();
                return Ok(format!("would run: {shown}{bin} {}", redact(&rendered).join(" ")));
            }
            let out = cmd
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
