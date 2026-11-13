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
            "What is waiting on you. Reading does NOT clear it: a message stays here until \
             you claim_messages, so nothing is lost by looking, and anyone watching the team \
             can tell what has actually been dealt with from what has merely been seen.",
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
        tool("claim_messages",
            "Say you have dealt with the messages waiting on you. This is the ONLY thing that \
             clears them: check_messages shows them and leaves them, because reading is not \
             answering, and a message glanced at used to be a message gone. Claim them once \
             you have acted, replied, or decided they need nothing. Omit 'ids' to claim \
             everything waiting on you.",
            json!({
                "ids": json!({ "type": "array", "items": { "type": "integer" },
                               "description": "Message ids from check_messages. Omitted means all" })
            }), &[]),
        tool("notify_operator",
            "Raise something with whoever is DRIVING this team -- not a teammate. Use it when \
             you are blocked on something no agent here can settle: a decision that is not \
             yours, a machine or a session you cannot see, work that belongs to another team. \
             It goes to this session's own orchestrator if there is one, and to the \
             supervisors who can see the whole fleet if there is not, or if yours has stopped \
             answering. The reply comes back with check_messages. This is not for progress \
             reports: set_summary and signal_done are what the people watching already read.",
            json!({
                "text": s("What you need, and what you already tried"),
                "escalate": json!({ "type": "boolean", "description":
                    "Go straight to the supervisors, past this session's orchestrator. \
                     For when the orchestrator itself is the problem" })
            }), &["text"]),
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
            "Record a decision, convention, discovery, blocker or context for this session. \
             Shorthand for store_memory at session scope.",
            json!({ "key": s("Short stable key"), "value": s("The content"),
                    "category": json!({ "type": "string",
                        "enum": ["decision", "convention", "discovery", "blocker", "context"] }) }),
            &["key", "value"]),
        tool("query_knowledge",
            "Read shared knowledge for this session. Call this when you start. \
             Shorthand for recall.",
            json!({ "key": s("Exact key"), "category": s("Filter by category") }), &[]),
        tool("store_memory",
            "Remember something, choosing who should be able to see it. 'session' (the \
             default) is this team; 'agent' is private to you; 'group' is a named subset you \
             belong to; 'global' outlives every session and needs the 'memory-writer' \
             capability. A nearer scope shadows a wider one for the same key, so a group can \
             override a global default without editing it.",
            json!({
                "key": s("Short stable key"),
                "value": s("The content"),
                "category": json!({ "type": "string",
                    "enum": ["decision", "convention", "discovery", "blocker", "context"] }),
                "scope": json!({ "type": "string",
                    "enum": ["agent", "group", "session", "global"],
                    "description": "Who may see it. Defaults to session." }),
                "group": s("Group name, required when scope is 'group'"),
                "ttl_seconds": json!({ "type": "integer",
                    "description": "Forget it after this long. Omit for permanent." })
            }), &["key", "value"]),
        tool("recall",
            "Search everything you are allowed to see -- your own notes, your groups, this \
             session and global memory -- by key, by words anywhere in a key or value, or by \
             category. Prefer this over guessing a key. Call it when you start.",
            json!({
                "key": s("Exact key, if you know it"),
                "search": s("Words to look for anywhere in a key or value"),
                "category": s("Filter by category"),
                "scope": json!({ "type": "string",
                    "enum": ["agent", "group", "session", "global"],
                    "description": "Look only in this scope. Omitted means all you can see." }),
                "limit": json!({ "type": "integer", "description": "Most entries to return" })
            }), &[]),
        tool("forget",
            "Remove something you remembered. Needs the same right as writing that scope.",
            json!({ "key": s("The key to remove"),
                    "scope": json!({ "type": "string",
                        "enum": ["agent", "group", "session", "global"] }),
                    "group": s("Group name, when scope is 'group'") }), &["key"]),
        tool("save_context",
            "Write a short summary of what you are doing and what you have learned, kept \
             against your identity rather than this session. If you are restarted -- even \
             days later, even on a CLI that cannot resume -- this is what you will be told \
             you were doing. Keep it current and keep it brief.",
            json!({ "summary": s("A few sentences: where you are, what matters, what is next") }),
            &["summary"]),
        tool("memory_history",
            "What a key used to say, and who changed it. Two teammates writing one key is \
             normal; this is how you see it happened.",
            json!({ "key": s("The key"),
                    "scope": json!({ "type": "string",
                        "enum": ["agent", "group", "session", "global"] }),
                    "group": s("Group name, when scope is 'group'") }), &["key"]),
        tool("ask_capability",
            "Ask a teammate that can do something you cannot -- OCR a page, transcribe audio, \
             embed text, generate an image -- and WAIT for the result. Use list_peers with a \
             capability filter to see what is on offer and which models each holder has. This \
             blocks until the answer comes back, so expect it to take as long as the work does.",
            json!({
                "capability": s("What you need, e.g. 'ocr', 'transcribe', 'embed'"),
                "request": json!({ "type": "object",
                    "description": "What to do, in the shape that capability documents. For \
                                    binary input, pass base64 or a path the holder can read.",
                    "additionalProperties": true }),
                "model": s("Which of the holder's models to use, when it offers several"),
                "timeout_seconds": json!({ "type": "integer",
                    "description": "How long to wait. Default 180." })
            }), &["capability", "request"]),
        tool("pending_asks",
            "Requests other teammates have sent you for a capability you offer. Answer each \
             with answer_ask. Only for an agent that advertises a capability.",
            json!({}), &[]),
        tool("answer_ask",
            "Return the result of a request from pending_asks. Say plainly if it failed -- the \
             asker is blocked until you answer, and a failure it can read beats a timeout it \
             cannot explain.",
            json!({ "ask_id": s("The id from pending_asks"),
                    "result": s("The answer, or why it could not be done"),
                    "failed": json!({ "type": "boolean",
                        "description": "True if the work could not be done" }) }),
            &["ask_id", "result"]),
        tool("query_code_graph",
            "Ask about this workspace's code without reading it: what calls a function, what a \
             file imports, where a symbol is defined, which subsystem something belongs to. \
             Parsed from the real syntax tree, so it is exact and costs you no context. Start \
             with what='overview'. Call edges are resolved by name, so they are a strong hint \
             rather than proof.",
            json!({
                "what": json!({ "type": "string",
                    "enum": ["overview", "find", "callers", "calls", "imports", "importers",
                             "defines", "subsystem", "subsystems", "path"],
                    "description": "The question to ask. 'overview' first if unsure." }),
                "target": s("The symbol or file to ask about, e.g. 'parse_header' or 'src/a.rs'"),
                "to": s("The other end, for what='path'"),
                "rebuild": json!({ "type": "boolean",
                    "description": "Re-parse the workspace first. Needed after large changes; \
                                    it is built automatically the first time." })
            }), &["what"]),
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
