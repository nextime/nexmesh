//! The orchestrator tool set, defined once.
//!
//! These are the operator's verbs, not an agent's: creating teams, placing
//! them on machines, and watching what they do. An agent gets the team tools
//! from `nexmesh-mcpd`; whoever is *running* the fleet gets these.

use serde_json::{json, Value};

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "list_nodes",
            description: "List the machines in the fleet, whether each is connected, and which \
                          agent CLIs it can run. Call this before creating a team so you place \
                          agents on machines that actually have the CLI.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        Tool {
            name: "create_team",
            description: "Create a session and spin up a team of agents across machines, in one \
                          call. Each agent is placed on a node, given a role and a task, and \
                          started. Returns the session id and what was launched.",
            schema: json!({
                "type": "object",
                "properties": {
                    "name": s("Short name for this piece of work, e.g. 'login feature'"),
                    "workspace": {
                        "type": "object",
                        "description": "Where the work happens. Omit for a session with no files.",
                        "properties": {
                            "name": s("Name for the workspace, e.g. 'app'"),
                            "kind": { "type": "string", "enum": ["node_local", "repo"],
                                "description": "node_local: files on ONE machine. repo: the same \
                                                tree checked out on several, so locks arbitrate \
                                                between them." },
                            "node": s("Node holding it (required for node_local)"),
                            "path": s("Absolute path on that node"),
                            "repo": s("Git remote, for a repo workspace"),
                            "mounts": { "type": "object", "description":
                                "For a repo workspace: node name -> absolute path on that node",
                                "additionalProperties": { "type": "string" } }
                        },
                        "required": ["name", "kind"]
                    },
                    "plan": { "type": "array", "items": { "type": "string" },
                        "description": "Ordered steps the team works through" },
                    "token_budget": { "type": "integer",
                        "description": "Pause the session once it has spent this many tokens \
                                        in total across every agent. Omitted means no ceiling." },
                    "agents": {
                        "type": "array",
                        "description": "The team. At least one.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": s("What teammates call this agent, e.g. 'Max'"),
                                "role": s("Role, e.g. 'engineer' or 'code reviewer'. A role \
                                           containing review/qa/test approves its own work."),
                                "cli": s("Agent CLI id from list_nodes, e.g. claude, opencode, codex"),
                                "model": s("Model for this seat, if its CLI can be told, e.g. \
                                            'claude-opus-4' or 'anthropic/claude-opus-4' for \
                                            opencode. Omitted means the CLI's default."),
                                "can": { "type": "array", "items": { "type": "string" },
                                    "description": "Capability labels teammates can address \
                                                    instead of guessing from names, e.g. \
                                                    ['strong','vision']" },
                                "task": s("What this agent should do"),
                                "node": s("Node to run it on; omitted means any node with that CLI"),
                                "mode": { "type": "string", "enum": ["headless", "interactive"],
                                    "description": "headless: best message delivery and token \
                                                    accounting. interactive: a real terminal you \
                                                    can attach to and take over." },
                                "owns": { "type": "array", "items": { "type": "string" },
                                    "description": "Globs this agent owns, e.g. ['src/**']" }
                            },
                            "required": ["name", "cli", "task"]
                        }
                    }
                },
                "required": ["name", "agents"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "team_status",
            description: "Every agent in a session: role, machine, task state, token use and \
                          current summary.",
            schema: json!({ "type": "object",
                "properties": { "session": s("Session id; omitted means the most recent") },
                "additionalProperties": false }),
        },
        Tool {
            name: "read_log",
            description: "The conversation between the agents, including whether each message has \
                          actually reached its recipient. An unseen message is usually why a team \
                          looks stuck.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "limit": { "type": "integer", "description": "How many recent messages (default 40)" }
            }, "additionalProperties": false }),
        },
        Tool {
            name: "message_team",
            description: "Send a message to the whole team or one agent, as the operator.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "to": s("Agent name or role; omitted means everyone"),
                "text": s("What to say")
            }, "required": ["text"], "additionalProperties": false }),
        },
        Tool {
            name: "add_agent",
            description: "Add one more agent to a running session and start it.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "name": s("Agent name"), "role": s("Role"),
                "cli": s("Agent CLI id"), "task": s("What it should do"),
                "model": s("Model for this seat, if its CLI can be told"),
                "can": { "type": "array", "items": { "type": "string" },
                    "description": "Capability labels, e.g. ['strong','vision']" },
                "node": s("Node to run it on"),
                "mode": { "type": "string", "enum": ["headless", "interactive"] },
                "workspace": s("Workspace name to join"),
                "owns": { "type": "array", "items": { "type": "string" } }
            }, "required": ["name", "cli", "task"], "additionalProperties": false }),
        },
        Tool {
            name: "adopt_terminal",
            description: "Adopt a tmux session already running on a machine, so it can be watched \
                          and driven from anywhere. It becomes attachable; it does NOT become a \
                          team member unless the CLI inside it loads the nexmesh shim.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "node": s("Node whose tmux server holds it"),
                "tmux_target": s("tmux session, window or pane, e.g. 'work' or 'work:1.0'"),
                "name": s("What to call it"), "role": s("Role")
            }, "required": ["node", "tmux_target", "name"], "additionalProperties": false }),
        },
        Tool {
            name: "pause_agents",
            description: "Stop sending work to one agent, or to the whole team. Reversible. A \
                          turn already running finishes -- this means 'take on nothing further', \
                          not 'stop this instant'.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "who": s("Agent name or role; omitted means everyone"),
                "reason": s("Why, so the agents can be told")
            }, "additionalProperties": false }),
        },
        Tool {
            name: "resume_agents",
            description: "Start sending work again, to one agent or the whole team. Anything \
                          queued while they were paused is delivered.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "who": s("Agent name or role; omitted means everyone")
            }, "additionalProperties": false }),
        },
        Tool {
            name: "release_agents",
            description: "Accept an agent's work and let it stop. Agents cannot disconnect on \
                          their own -- that is what stops one wandering off before review -- so \
                          this is how that hold is lifted. Omit the name to release the team and \
                          end the session's work.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "who": s("Agent name or role; omitted means everyone"),
                "note": s("A word to the agent about why")
            }, "additionalProperties": false }),
        },
        Tool {
            name: "read_transcript",
            description: "What one agent's process actually printed. Use this when an agent is \
                          behaving oddly or died: it outlives the process.",
            schema: json!({ "type": "object", "properties": {
                "session": s("Session id; omitted means the most recent"),
                "who": s("Agent name, role or id"),
                "tail": { "type": "integer", "description": "Bytes from the end (default 8000)" }
            }, "required": ["who"], "additionalProperties": false }),
        },
        Tool {
            name: "list_sessions",
            description: "All sessions this hub knows about.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
    ]
}

pub fn names() -> Vec<&'static str> {
    tools().into_iter().map(|t| t.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        let mut n = names();
        let before = n.len();
        n.sort();
        n.dedup();
        assert_eq!(before, n.len());
    }

    #[test]
    fn a_team_can_be_composed_of_unequal_agents() {
        // The orchestrator is where a team is actually built, so if a model or
        // a capability cannot be stated here, a mixed team cannot be made at
        // all through the MCP surface -- only through the HTTP API.
        let t = tools().into_iter().find(|t| t.name == "create_team").unwrap();
        let agent = &t.schema["properties"]["agents"]["items"]["properties"];
        assert!(agent.get("model").is_some(), "a seat must be able to name its model");
        assert!(agent.get("can").is_some(), "a seat must be able to declare capabilities");
        assert_eq!(agent["can"]["type"], "array");
        assert!(
            t.schema["properties"].get("token_budget").is_some(),
            "a team must be able to carry a spending ceiling"
        );
        // And the same on the tool that adds one later, or a team could only
        // be mixed at creation time.
        let t = tools().into_iter().find(|t| t.name == "add_agent").unwrap();
        let p = t.schema["properties"].as_object().unwrap();
        assert!(p.contains_key("model") && p.contains_key("can"), "{:?}", p.keys());
    }

    #[test]
    fn every_tool_declares_a_usable_schema() {
        for t in tools() {
            assert!(!t.description.is_empty(), "{}", t.name);
            assert_eq!(t.schema["type"], "object", "{}", t.name);
            let props = t.schema["properties"].as_object().unwrap();
            for r in t.schema["required"].as_array().unwrap_or(&vec![]) {
                assert!(props.contains_key(r.as_str().unwrap()),
                        "{}: required '{}' is not declared", t.name, r);
            }
        }
    }

    #[test]
    fn create_team_requires_a_name_and_at_least_the_agent_essentials() {
        let t = tools().into_iter().find(|t| t.name == "create_team").unwrap();
        let req: Vec<&str> = t.schema["required"].as_array().unwrap()
            .iter().map(|v| v.as_str().unwrap()).collect();
        assert!(req.contains(&"name") && req.contains(&"agents"), "{req:?}");
        let agent_req = &t.schema["properties"]["agents"]["items"]["required"];
        let ar: Vec<&str> = agent_req.as_array().unwrap()
            .iter().map(|v| v.as_str().unwrap()).collect();
        // Without a cli we cannot place it, and without a task it has nothing to do.
        assert!(ar.contains(&"name") && ar.contains(&"cli") && ar.contains(&"task"), "{ar:?}");
    }
}
