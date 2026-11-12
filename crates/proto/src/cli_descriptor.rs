//! Declarative agent-CLI support.
//!
//! Adding a CLI must not mean writing Rust. A descriptor file says how to
//! detect the binary, how to register our MCP shim with it, and how to launch
//! it in each mode. Delivery then has three tiers:
//!
//!   `mcp_only`     -- works for ANY CLI that speaks MCP, zero bespoke code.
//!                     Messages ride in on the next tool result.
//!   built-in       -- `stream_json` (Claude), `app_server` (Codex): mid-turn
//!                     injection, exact token accounting.
//!   `plugin`       -- an external helper binary speaking our driver protocol
//!                     on stdio, for a CLI with its own quirks.
//!
//! The floor is what matters: a CLI nobody has ever heard of still works at
//! `mcp_only`, and the fast paths are an optimisation, not a requirement.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliDescriptor {
    pub cli: CliMeta,
    #[serde(default)]
    pub mcp: McpRegistration,
    #[serde(default)]
    pub modes: BTreeMap<String, ModeSpec>,
    #[serde(default)]
    pub driver: DriverSpec,
    /// How to add a server to this CLI's own configuration, for the
    /// orchestrator. Absent means nexmesh cannot install itself there.
    #[serde(default)]
    pub user_install: Option<UserInstall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliMeta {
    pub id: String,
    pub display_name: String,
    /// Binary name, looked up on PATH; may be an absolute path.
    pub command: String,
    #[serde(default)]
    pub version_args: Vec<String>,
    /// Extra places to look when the binary is not on a login-shell PATH
    /// (npm-global, ~/.local/bin, ~/.opencode/bin -- all real cases here).
    #[serde(default)]
    pub extra_paths: Vec<String>,
    /// Environment variables to CLEAR for this CLI. A node started from inside
    /// tmux inherits `$TMUX`, and a tmux client that sees it refuses to run
    /// ("sessions should be nested with care"), so adoption would fail
    /// depending on how the node happened to be launched.
    #[serde(default)]
    pub env_unset: Vec<String>,
}

/// How to make this CLI load our MCP shim.
///
/// Each CLI wants a different JSON/TOML shape in a different place, and those
/// shapes change. Keeping the shape here, as DATA, means supporting a new CLI
/// or tracking a schema change is a descriptor edit -- never a Rust change and
/// never a guess baked into code. (opencode, for instance, takes
/// `mcp.servers.<name>` with `command` as an ARRAY and env under
/// `environment`, while Claude takes `mcpServers.<name>` with `command` as a
/// STRING plus `args`, and env under `env`.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum McpRegistration {
    /// Pass a generated config file on the command line. Always preferred: it
    /// cannot corrupt the user's own CLI configuration.
    InlineFlag {
        flag: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Merge into a JSON config. A relative `path` is a project file inside the
    /// workspace; a `~/` path is the user's global config.
    JsonFile {
        path: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Merge into a TOML config under a dotted key (Codex).
    TomlFile {
        path: String,
        key: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    #[default]
    None,
}

/// Convert a descriptor's TOML entry template into JSON, substituting
/// `{mcpd}` with the shim's path and injecting environment variables under
/// whichever key this CLI expects.
pub fn render_entry(
    entry: &toml::Value,
    env_key: Option<&str>,
    mcpd_path: &str,
    env: &[(String, String)],
) -> serde_json::Value {
    let mut json = toml_to_json(entry, mcpd_path);
    if let (Some(key), Some(obj)) = (env_key, json.as_object_mut()) {
        let map: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        obj.insert(key.to_string(), serde_json::Value::Object(map));
    }
    json
}

fn toml_to_json(v: &toml::Value, mcpd_path: &str) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        toml::Value::String(s) => J::String(s.replace("{mcpd}", mcpd_path)),
        toml::Value::Integer(i) => J::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(a) => J::Array(a.iter().map(|x| toml_to_json(x, mcpd_path)).collect()),
        toml::Value::Table(t) => J::Object(
            t.iter().map(|(k, x)| (k.clone(), toml_to_json(x, mcpd_path))).collect(),
        ),
    }
}

/// Set a value at an RFC 6901 JSON Pointer, creating intermediate objects.
/// Used to merge our entry into a config we do not own, leaving the rest of
/// the user's settings untouched.
pub fn set_pointer(
    root: &mut serde_json::Value,
    pointer: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let parts: Vec<String> = pointer
        .trim_start_matches('/')
        .split('/')
        .map(|p| p.replace("~1", "/").replace("~0", "~"))
        .collect();
    if parts.is_empty() || parts[0].is_empty() {
        return Err("empty JSON pointer".into());
    }
    if !root.is_object() {
        *root = serde_json::json!({});
    }
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()
            .ok_or("pointer traverses a non-object")?
            .entry(part.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
    if !cur.is_object() {
        *cur = serde_json::json!({});
    }
    cur.as_object_mut()
        .ok_or("pointer traverses a non-object")?
        .insert(parts[parts.len() - 1].clone(), value);
    Ok(())
}

/// How to register a server in the USER's own CLI configuration, so it is
/// there every time they start that CLI by hand.
///
/// Distinct from `McpRegistration`, which is about the throwaway config a node
/// writes for an agent it is about to spawn. This one edits something the user
/// owns, so where a CLI offers its own command we use it rather than guess at
/// a file format that may change under us.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum UserInstall {
    /// Let the CLI do it: `<cli> mcp add --scope user <name> -- <command>`.
    Command {
        argv: Vec<String>,
        /// Flag used to pass one environment variable, repeated per variable
        /// (`-e KEY=value`). Absent means the CLI takes no env.
        #[serde(default)]
        env_flag: Option<String>,
    },
    /// Merge into a JSON config ourselves, for a CLI whose `mcp add` is
    /// interactive and so unusable from a script.
    JsonFile {
        path: String,
        pointer: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
    /// Same, for TOML.
    TomlFile {
        path: String,
        key: String,
        entry: toml::Value,
        #[serde(default)]
        env_key: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeSpec {
    /// Argv after the command. `{prompt}`, `{cwd}`, `{session}`, `{resume}`
    /// and `{mcp_config}` are substituted.
    #[serde(default)]
    pub args: Vec<String>,
    /// Write the prompt to stdin instead of passing it as an argument.
    #[serde(default)]
    pub prompt_on_stdin: bool,
    /// Argv fragment added when resuming a previous session.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Does this mode give a human-usable terminal when attached?
    #[serde(default)]
    pub attachable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriverSpec {
    /// Universal floor: delivery piggybacks on MCP tool results.
    #[default]
    McpOnly,
    /// Newline-delimited JSON on stdio, Claude Code's `stream-json` shape.
    StreamJson {
        #[serde(default)]
        replay_user_messages: bool,
    },
    /// JSON-RPC app-server with threads and turns, Codex's shape.
    AppServer,
    /// The CLI runs its own HTTP server and we drive it over that.
    ///
    /// opencode works this way: `opencode serve` exposes a documented API with
    /// native mid-turn steering, so there is no stdio protocol to parse and no
    /// pty to hold. It also means the same agent can be attached to as a real
    /// TUI on demand, which a stdio-driven agent cannot offer.
    HttpServer {
        /// Username for HTTP Basic auth on that server.
        #[serde(default)]
        basic_auth_user: Option<String>,
        /// Environment variable through which to hand it a password. A
        /// per-agent secret stops anyone else with a shell on the machine
        /// driving the agent through its own loopback port.
        #[serde(default)]
        password_env: Option<String>,
        /// Command that opens a terminal onto a running server, for attach.
        #[serde(default)]
        attach_args: Vec<String>,
    },
    /// External helper binary speaking the driver protocol on stdio.
    Plugin { command: String, #[serde(default)] args: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum DescriptorError {
    #[error("parsing cli descriptor: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("descriptor {0} declares no launch modes")]
    NoModes(String),
    #[error("descriptor {0}: a plugin driver needs a command to run")]
    NoPluginCommand(String),
    #[error("descriptor {0}: headless mode '{1}' has no way to deliver the prompt \
(needs a {{prompt}} placeholder, prompt_on_stdin, or a driver with its own prompt channel)")]
    NoPromptPath(String, String),
}

impl CliDescriptor {
    pub fn parse(text: &str) -> Result<Self, DescriptorError> {
        let d: CliDescriptor = toml::from_str(text)?;
        if d.modes.is_empty() {
            return Err(DescriptorError::NoModes(d.cli.id.clone()));
        }
        // Caught here rather than at spawn time, where the symptom would be an
        // agent that starts and then quietly has no driver.
        if let DriverSpec::Plugin { command, .. } = &d.driver {
            if command.trim().is_empty() {
                return Err(DescriptorError::NoPluginCommand(d.cli.id.clone()));
            }
        }
        // An interactive mode may legitimately take no arguments at all --
        // bare `codex` or `gemini` opens the TUI. Only a HEADLESS mode has to
        // prove it can get the first prompt in: via a {prompt} placeholder,
        // via stdin, or through a driver protocol that carries prompts itself.
        for (name, m) in &d.modes {
            if name != "headless" {
                continue;
            }
            let driver_carries_prompt = !matches!(d.driver, DriverSpec::McpOnly);
            let via_argv = m.args.iter().any(|a| a.contains("{prompt}"));
            if !driver_carries_prompt && !via_argv && !m.prompt_on_stdin {
                return Err(DescriptorError::NoPromptPath(d.cli.id.clone(), name.clone()));
            }
        }
        Ok(d)
    }

    /// The MCP registration for this CLI, if any.
    pub fn mcp(&self) -> &McpRegistration {
        &self.mcp
    }

    pub fn user_install(&self) -> Option<&UserInstall> {
        self.user_install.as_ref()
    }

    /// Can a human take over a session in this mode, or only watch it?
    pub fn attachable(&self, mode: &str) -> bool {
        self.modes.get(mode).map(|m| m.attachable).unwrap_or(false)
    }

    /// Expand an argv template. Unknown placeholders are left alone so a CLI's
    /// own `{}` syntax is not mangled.
    pub fn render_args(&self, mode: &str, vars: &BTreeMap<&str, String>) -> Vec<String> {
        let Some(spec) = self.modes.get(mode) else { return vec![] };
        spec.args
            .iter()
            .map(|a| {
                let mut s = a.clone();
                for (k, v) in vars {
                    s = s.replace(&format!("{{{k}}}"), v);
                }
                s
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"
[cli]
id = "claude"
display_name = "Claude Code"
command = "claude"
version_args = ["--version"]
extra_paths = ["~/.local/bin"]

[mcp]
style = "inline_flag"
flag = "--mcp-config"
pointer = "/mcpServers/nexmesh"
env_key = "env"
entry = { command = "{mcpd}", args = [] }

[modes.headless]
args = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--mcp-config", "{mcp_config}"]
prompt_on_stdin = true
resume_args = ["--resume", "{resume}"]
attachable = false

[modes.interactive]
args = ["--mcp-config", "{mcp_config}"]
attachable = true

[driver]
kind = "stream_json"
replay_user_messages = true
"#;

    #[test]
    fn parses_a_real_descriptor() {
        let d = CliDescriptor::parse(CLAUDE).unwrap();
        assert_eq!(d.cli.id, "claude");
        assert_eq!(d.driver, DriverSpec::StreamJson { replay_user_messages: true });
        assert!(d.attachable("interactive"));
        assert!(!d.attachable("headless"));
    }

    #[test]
    fn unknown_cli_defaults_to_the_universal_floor() {
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "somenewcli"
display_name = "Some New CLI"
command = "somenewcli"
[modes.interactive]
args = ["chat"]
attachable = true
"#,
        )
        .unwrap();
        assert_eq!(d.driver, DriverSpec::McpOnly);
        assert!(matches!(d.mcp, McpRegistration::None));
    }

    #[test]
    fn substitutes_only_known_placeholders() {
        let d = CliDescriptor::parse(CLAUDE).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("mcp_config", "/tmp/x.json".to_string());
        let args = d.render_args("headless", &vars);
        assert!(args.contains(&"/tmp/x.json".to_string()));
        assert!(!args.iter().any(|a| a.contains('{')));
    }

    #[test]
    fn rejects_a_headless_mode_that_cannot_receive_a_prompt() {
        // mcp_only driver, no {prompt}, no stdin -- the agent would start with
        // no task at all. Better to fail here than at spawn time.
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "nopath"
display_name = "No Path"
command = "nopath"
[modes.headless]
args = ["run"]
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoPromptPath(_, _)), "got {err}");
    }

    #[test]
    fn allows_an_argless_interactive_mode() {
        // `codex` and `gemini` with no args open their TUI; that is valid.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "tuionly"
display_name = "TUI Only"
command = "tuionly"
[modes.interactive]
args = []
attachable = true
"#,
        )
        .unwrap();
        assert!(d.attachable("interactive"));
    }

    #[test]
    fn allows_a_headless_mode_whose_driver_carries_the_prompt() {
        // Codex takes no prompt on argv: it arrives over the app-server RPC.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "rpcdriven"
display_name = "RPC Driven"
command = "rpcdriven"
[modes.headless]
args = ["app-server"]
[driver]
kind = "app_server"
"#,
        )
        .unwrap();
        assert_eq!(d.driver, DriverSpec::AppServer);
    }

    #[test]
    fn a_plugin_driver_must_name_its_helper() {
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "hollow"
display_name = "Hollow"
command = "hollow"
[modes.interactive]
args = []
attachable = true
[driver]
kind = "plugin"
command = "  "
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoPluginCommand(_)), "got {err}");
    }

    #[test]
    fn a_plugin_driver_parses_with_its_command_and_args() {
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "quirky"
display_name = "Quirky"
command = "quirky"
[modes.headless]
args = ["--serve"]
[modes.interactive]
args = []
attachable = true
[driver]
kind = "plugin"
command = "quirky-plugin"
args = ["--verbose"]
"#,
        )
        .unwrap();
        assert_eq!(
            d.driver,
            DriverSpec::Plugin {
                command: "quirky-plugin".into(),
                args: vec!["--verbose".into()]
            }
        );
    }

    #[test]
    fn a_plugin_headless_mode_needs_no_prompt_placeholder() {
        // The prompt reaches the CLI through the plugin's hello frame, so
        // requiring {prompt} on argv would reject every plugin descriptor.
        let d = CliDescriptor::parse(
            r#"
[cli]
id = "viaplugin"
display_name = "Via Plugin"
command = "viaplugin"
[modes.headless]
args = []
[driver]
kind = "plugin"
command = "viaplugin-driver"
"#,
        )
        .unwrap();
        assert!(matches!(d.driver, DriverSpec::Plugin { .. }));
    }

    #[test]
    fn rejects_a_descriptor_with_no_modes() {
        let err = CliDescriptor::parse(
            r#"
[cli]
id = "broken"
display_name = "Broken"
command = "broken"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, DescriptorError::NoModes(_)));
    }
}

/// Validates every descriptor shipped in the repo's `clis/` directory, so a
/// typo in a TOML file fails the build rather than a spawn at 3am.
#[cfg(test)]
mod shipped_descriptors {
    use super::*;

    #[test]
    fn every_shipped_descriptor_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("clis/ directory must exist") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let d = CliDescriptor::parse(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                Some(d.cli.id.as_str()),
                path.file_stem().and_then(|s| s.to_str()),
                "{}: cli.id must match the filename",
                path.display()
            );
            assert!(
                d.modes.contains_key("headless") || d.modes.contains_key("interactive"),
                "{}: needs a headless or interactive mode",
                path.display()
            );
            seen += 1;
        }
        assert!(seen >= 5, "expected the shipped descriptors, found {seen}");
    }

    #[test]
    fn at_least_one_mode_per_cli_is_attachable() {
        // The user must be able to reach any agent's CLI from any machine, so
        // every supported CLI needs a mode a human can actually drive.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../clis");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let d = CliDescriptor::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(
                d.modes.values().any(|m| m.attachable),
                "{}: no attachable mode",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod registration {
    use super::*;

    fn desc(name: &str) -> CliDescriptor {
        let path = format!("{}/../../clis/{name}.toml", env!("CARGO_MANIFEST_DIR"));
        CliDescriptor::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn claude_entry_uses_a_string_command_and_env() {
        let d = desc("claude");
        let McpRegistration::InlineFlag { pointer, entry, env_key, flag } = d.mcp() else {
            panic!("claude should use an inline flag so it never touches ~/.claude.json");
        };
        assert_eq!(flag, "--mcp-config");
        assert_eq!(pointer, "/mcpServers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd",
            &[("NEXMESH_AGENT_ID".into(), "ag-1".into())]);
        assert_eq!(j["command"], "/usr/bin/nexmesh-mcpd");
        assert!(j["args"].is_array());
        assert_eq!(j["env"]["NEXMESH_AGENT_ID"], "ag-1");
    }

    #[test]
    fn opencode_entry_uses_an_array_command_and_environment() {
        // The two CLIs disagree on both the key names and the type of
        // `command`. Getting this wrong means the shim silently never loads.
        let d = desc("opencode");
        let McpRegistration::JsonFile { path, pointer, entry, env_key } = d.mcp() else {
            panic!("opencode should merge into a project json file");
        };
        assert_eq!(path, "opencode.json", "relative => project file, not the user's global config");
        assert_eq!(pointer, "/mcp/servers/nexmesh");
        let j = render_entry(entry, env_key.as_deref(), "/usr/bin/nexmesh-mcpd", &[("K".into(), "v".into())]);
        assert_eq!(j["type"], "local");
        assert_eq!(j["command"], serde_json::json!(["/usr/bin/nexmesh-mcpd"]));
        assert_eq!(j["enabled"], true);
        assert_eq!(j["environment"]["K"], "v", "opencode calls it 'environment', not 'env'");
    }

    #[test]
    fn pointer_merge_preserves_unrelated_settings() {
        // We write into config files we do not own. Clobbering a user's other
        // MCP servers or settings would be unforgivable.
        let mut root = serde_json::json!({
            "theme": "dark",
            "mcpServers": { "someone-elses": { "command": "keepme" } }
        });
        set_pointer(&mut root, "/mcpServers/nexmesh", serde_json::json!({ "command": "x" })).unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["mcpServers"]["someone-elses"]["command"], "keepme");
        assert_eq!(root["mcpServers"]["nexmesh"]["command"], "x");
    }

    #[test]
    fn pointer_merge_creates_missing_intermediate_objects() {
        let mut root = serde_json::json!({});
        set_pointer(&mut root, "/mcp/servers/nexmesh", serde_json::json!(1)).unwrap();
        assert_eq!(root["mcp"]["servers"]["nexmesh"], 1);
    }

    #[test]
    fn pointer_merge_replaces_a_non_object_in_the_path() {
        // A config where `mcp` is a string must not make us panic or corrupt.
        let mut root = serde_json::json!({ "mcp": "nonsense" });
        set_pointer(&mut root, "/mcp/servers/nexmesh", serde_json::json!(1)).unwrap();
        assert_eq!(root["mcp"]["servers"]["nexmesh"], 1);
    }

    #[test]
    fn an_empty_pointer_is_an_error_not_a_wipe() {
        let mut root = serde_json::json!({ "keep": 1 });
        assert!(set_pointer(&mut root, "", serde_json::json!(2)).is_err());
        assert_eq!(root["keep"], 1);
    }

    #[test]
    fn escaped_pointer_segments_are_decoded() {
        let mut root = serde_json::json!({});
        set_pointer(&mut root, "/a~1b/c", serde_json::json!(1)).unwrap();
        assert_eq!(root["a/b"]["c"], 1);
    }

    #[test]
    fn every_shipped_descriptor_declares_a_registration() {
        // A CLI with no way to load the shim could never join a team.
        for name in ["claude", "opencode", "qwen", "gemini", "codex"] {
            assert!(
                !matches!(desc(name).mcp(), McpRegistration::None),
                "{name} has no MCP registration"
            );
        }
    }
}
