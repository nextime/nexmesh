//! The team tool set, defined once and shared.
//!
//! The hub dispatches these; every front door (mcpd over stdio, the participant
//! API over HTTP) advertises exactly this list. Defining them in one place
//! means a new tool cannot be offered by one transport and rejected by another.
//!
//! Nothing here is CLI-specific. A hermes agent calling `send_message` over
//! HTTP and a `claude` process calling it over MCP stdio hit the same code.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, as MCP's `inputSchema`.
    pub input_schema: Value,
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

/// Every tool available to every participant.
pub fn team_tools() -> Vec<ToolDef> {
    vec![
        tool("list_peers",
            "List your teammates: name, role, capabilities, model, machine, task state and \
             current summary. Use the capability filter to find someone able to do a \
             particular kind of work rather than guessing from names.",
            json!({
                "role": s("Only peers with this role"),
                "capability": s("Only peers with this capability label, e.g. 'strong', \
                                 'cheap', 'vision', 'ocr'")
            }), &[]),
        tool("send_message",
            "Send a message to a teammate by name or role. Delivered wherever they run.",
            json!({ "to": s("Teammate name or role"), "text": s("Message body") }),
            &["to", "text"]),
        tool("broadcast",
            "Send a message to every teammate at once.",
            json!({ "text": s("Message body") }), &["text"]),
        tool("check_messages",
            "Fetch messages addressed to you that you have not yet seen.",
            json!({}), &[]),
        tool("set_summary",
            "Update your one-line status, visible to peers and on the dashboard.",
            json!({ "summary": s("What you are doing right now") }), &["summary"]),
        tool("check_team_status",
            "See every teammate's role, task state, machine and summary.",
            json!({}), &[]),
        tool("get_plan",
            "Read the team plan and each item's progress.", json!({}), &[]),
        tool("update_plan",
            "Mark a plan item's status.",
            json!({ "item": s("Plan item label or index"),
                    "status": json!({ "type": "string", "enum": ["pending", "in_progress", "done", "blocked"] }) }),
            &["item", "status"]),
        tool("signal_done",
            "Declare your task complete. Triggers review unless you are a reviewer.",
            json!({ "notes": s("What you did, for the reviewer") }), &[]),
        tool("escalate",
            "Hand your task to a teammate better suited to it -- more capable, or with a \
             capability you lack. Say what you tried and why you are stuck: they get that \
             as the handoff, not just the task. Your own state becomes 'escalated' and \
             theirs 'working', so nobody waits on you. Use list_peers with a capability \
             filter to find who to ask.",
            json!({
                "to": s("Teammate name, role, or a capability label like 'strong'"),
                "reason": s("Why you are escalating: what you tried and where it failed"),
                "task": s("What they should do. Omitted means your own current task")
            }), &["to", "reason"]),
        tool("submit_feedback",
            "Give review feedback on a teammate's work.",
            json!({ "to": s("Teammate name or role"), "text": s("The feedback"),
                    "actionable": json!({ "type": "boolean", "description": "True if they must act on it before approval" }) }),
            &["to", "text", "actionable"]),
        tool("approve",
            "Approve a teammate's completed work.",
            json!({ "to": s("Teammate name or role"), "notes": s("Optional remarks") }), &["to"]),
        tool("acquire_file",
            "Take an exclusive lock on a path before editing a shared file.",
            json!({ "path": s("Path, relative to the workspace"), "purpose": s("Why you need it") }),
            &["path"]),
        tool("release_file",
            "Release a lock you hold.", json!({ "path": s("The locked path") }), &["path"]),
        tool("view_file_locks",
            "See active locks and ownership zones for your workspace.", json!({}), &[]),
        tool("store_knowledge",
            "Record a decision, convention, discovery, blocker or context for the whole team.",
            json!({ "key": s("Short stable key"), "value": s("The content"),
                    "category": json!({ "type": "string",
                        "enum": ["decision", "convention", "discovery", "blocker", "context"] }) }),
            &["key", "value"]),
        tool("query_knowledge",
            "Read shared knowledge. Call this when you start.",
            json!({ "key": s("Exact key"), "category": s("Filter by category") }), &[]),
        tool("publish_work",
            "Share what you have done with the rest of the team. Commits your changes in the \
             shared workspace and pushes them, then everyone else can pull them. Call this when \
             you finish a piece of work -- teammates on other machines cannot see your files \
             until you do.",
            json!({ "summary": s("One line describing what you are sharing") }), &[]),
        tool("sync_workspace",
            "Pull in what your teammates have published. Your uncommitted work is kept. Call \
             this before starting something that depends on someone else's changes.",
            json!({}), &[]),
        tool("workspace_status",
            "What is in your workspace that has not been shared, and what others have shared \
             that you have not taken. Changes nothing.",
            json!({}), &[]),
        tool("get_history",
            "Query the session's message history.",
            json!({ "limit": json!({ "type": "integer", "description": "Most recent N messages" }) }), &[]),
    ]
}

pub fn tool_names() -> Vec<String> {
    team_tools().into_iter().map(|t| t.name).collect()
}

pub fn is_known_tool(name: &str) -> bool {
    team_tools().iter().any(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique() {
        let mut names = tool_names();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate tool name");
    }

    #[test]
    fn every_tool_has_a_valid_object_schema() {
        for t in team_tools() {
            assert!(!t.description.is_empty(), "{} has no description", t.name);
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            let props = t.input_schema["properties"].as_object().expect(&t.name);
            // Every required argument must actually be declared.
            for req in t.input_schema["required"].as_array().unwrap() {
                let key = req.as_str().unwrap();
                assert!(props.contains_key(key), "{}: required '{}' not in properties", t.name, key);
            }
        }
    }

    #[test]
    fn known_tool_lookup_works() {
        assert!(is_known_tool("send_message"));
        assert!(!is_known_tool("rm_minus_rf"));
    }
}
