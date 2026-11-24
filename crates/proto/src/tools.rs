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
/// What a CLI is told about the team at the MCP handshake.
///
/// Two transports carry this -- the shim beside an agent, and the hub's own
/// MCP front door -- and a self-seated CLI never sees the brief the fleet
/// gives an agent it spawned. That makes this the ONLY thing some teammates
/// are ever told, which is why it covers the chat rather than just the tools:
/// an agent that does not know a person is in the room answers the operator
/// by messaging a teammate, and one did exactly that.
///
/// `who` is the name the team addresses it by; empty when it has none yet.
pub fn team_instructions(who: &str, slot: &str) -> String {
    let name = if who.trim().is_empty() {
        String::new()
    } else {
        format!(" as '{}'", who.trim())
    };
    format!(
        "You are part of a nexmesh team{name} (slot {slot}). Teammates may run on other \
         machines, under other CLIs, or not be CLIs at all.\n\n\
         WORKING WITH THEM: call query_knowledge and check_team_status before planning. \
         set_summary whenever what you are doing changes -- peers and the operator read \
         it. store_knowledge as you decide things, not at the end: context drift between \
         agents is the main way teams like this fail. list_peers says who exists, and \
         ask_capability reaches a teammate that can do what you cannot (OCR, \
         transcription, speech, images).\n\n\
         WHOSE WORK IS WHOSE: list_peers says what each teammate OWNS as well as who they \
         are -- a team sharing one checkout divides it, a container or a directory each. \
         Before editing anything you did not write, ask who_owns <path>: if it belongs to \
         somebody, send_message them what needs doing rather than doing it for them. Two \
         agents editing one area undo each other.\n\n\
         WHAT THE TEAM KNOWS: the memory and the code graph are the team's. Read before \
         deciding (query_knowledge, query_code_graph) and write back what you learn \
         (store_knowledge), as you learn it rather than at the end. Contradicting a \
         teammate's memory is a conversation, not an overwrite. Everything you remember \
         has a SCOPE, and it decides who is affected when you are wrong: 'agent' is \
         yours alone and the place for anything provisional, 'session' is this team and \
         is where most of it belongs, 'global' is the whole fleet for ever and is for \
         what is true of the MACHINES rather than this job -- which host has the GPU, \
         what a service is managed by. The test is whether the next team on a different \
         task would still need it. A nearer scope shadows a wider one for the same key, \
         so a team can override a fleet-wide default without touching it. The code graph \
         has scopes for a different reason: it is parsed per workspace, so 'session' or \
         'global' is how you see past your own checkout -- neither parses anything, they \
         answer from what is already there.\n\n\
         THE CHAT: teammate and operator messages arrive prefixed [PENDING MESSAGES] on \
         any tool result, each with its own number. ANSWER ONE WITH `reply(to: <number>, \
         text: ...)`: it goes back to whoever sent it, wherever they are. A message whose \
         sender is the OPERATOR is from the PERSON running the fleet, reading the room on \
         a dashboard, in a terminal console or on a phone, and `reply` puts your answer \
         in front of them without sending it to your teammates. send_message starts \
         something with one teammate; broadcast goes to the whole room and is for \
         something they all need -- using it to answer one person is how six models come \
         to read a note meant for one, and is how a budget goes. Reading a message does \
         not deal with it: check_messages shows what is waiting, claim_messages is what \
         says you have handled it, and anything unclaimed stays on the board for whoever \
         is watching the fleet. If you are stuck on something the team cannot settle -- a \
         credential, a decision that is not yours -- call notify_operator.\n\n\
         YOUR CONTEXT IS NOT WHERE WORK IS KEPT: fold_context writes down what a stretch \
         of this job established -- the decision and the reason for it, the paths with \
         their line numbers, the exact error -- and hands the raw material over as \
         `detail`, which nexmesh keeps OUT of your context and gives back in full \
         whenever you ask. Do it when the step you are on has finished with that stretch, \
         and before your CLI compacts you: a compaction replaces your context with a \
         summary of itself, and the next one summarises the summary. A fold survives the \
         compaction, your restart, and even a move to another CLI. search_context finds \
         one by its words, unfold_context gives the detail back, context_map says how \
         full you are and what you have put away, and fold_context with `covers` puts one \
         summary above a group of earlier folds -- which is what keeps a month of work \
         down to a handful of them. If you are handed a list of folds when you start, \
         that is what you did before: read the summaries, and unfold only what the work \
         in front of you actually needs.\n\n\
         A STALE GRAPH: query_code_graph is DERIVED from the files and can lag them. If \
         what it says disagrees with what you read on disk, the files win -- and say so \
         in the chat, because a graph nobody trusts is worse than no graph. What you \
         CONCLUDE about the code goes in store_knowledge, which is attributed and does \
         not get regenerated away.\n\n\
         PERMISSION: if your CLI stops to ask permission for something and nobody is \
         watching your terminal, that question goes to the operator with the choices you \
         were offered, and you wait for them. That is normal and not a failure. Say what \
         you are waiting on if you get the chance, and never work around it.\n\n\
         SERVICES: some seats are services rather than teammates -- OCR, transcription, \
         images, embeddings, review -- and may be visiting from another team, which \
         list_peers says. Ask them rather than hand-rolling what they do, and rather than \
         installing a tool to do it yourself."
    )
}

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
            "Send a message to a teammate by name or role. Delivered wherever they run.              To ANSWER something you were sent, use `reply` instead: it goes to whoever              sent it, including the operator, without you having to work out who that was.",
            json!({ "to": s("Teammate name or role"), "text": s("Message body") }),
            &["to", "text"]),
        tool("reply",
            "Answer one message, by its number. It goes back to whoever sent it: a              teammate, the operator (as a line in the chat they are reading, which your              teammates are NOT sent), or -- for something forwarded to you from the              operator channel -- the seat that raised it, in its own session. This is the              tool for answering; `broadcast` is not.",
            json!({ "to": json!({ "type": "integer",
                                  "description": "The message's number, as check_messages                                                   and the pending-message block show it" }),
                    "text": s("Your answer") }),
            &["to", "text"]),
        tool("broadcast",
            "Send a message to EVERY teammate at once. For something the whole team needs:              a decision, a change they must all know about. Answering one person with this              is how six agents come to read a note meant for one -- use `reply` for that.",
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
            "Record a decision, convention, discovery, blocker or context. Defaults to this \
             team; 'global' is for what holds across every team on this fleet and 'agent' for \
             a note only you keep. Shorthand for store_memory.",
            json!({ "key": s("Short stable key"), "value": s("The content"),
                    "category": json!({ "type": "string",
                        "enum": ["decision", "convention", "discovery", "blocker", "context"] }),
                    "scope": json!({ "type": "string",
                        "enum": ["agent", "group", "session", "global"],
                        "description": "Who may see it. Defaults to this session (the team). \
                                        'global' holds for every team and needs the \
                                        'memory-writer' capability." }),
                    "group": s("Group name, required when scope is 'group'") }),
            &["key", "value"]),
        tool("query_knowledge",
            "Read what is known: your own notes, your groups, this team, and what holds \
             fleet-wide, nearest first. Call this when you start. Name a scope to read only \
             that one. Shorthand for recall.",
            json!({ "key": s("Exact key"),
                    "search": s("Words to look for anywhere in a key or value"),
                    "category": s("Filter by category"),
                    "scope": json!({ "type": "string",
                        "enum": ["agent", "group", "session", "global"],
                        "description": "Read only this scope. Omitted means everything you \
                                        can see." }) }), &[]),
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
        // A long-lived agent spends its context twice: once working, and
        // then again carrying what it did. These four are the second cost:
        // what the work established is written down once, kept out of
        // context, and read back only where a question needs it.
        tool("fold_context",
            "Fold a stretch of your own context into something that outlives it. Write what              that stretch ESTABLISHED, and hand over the raw material as `detail` -- nexmesh              keeps the detail out of your context and gives it back, in full, whenever you or              a teammate asks. Do this BEFORE you are compacted: a compaction summarises your              summary, and the fourth generation of that remembers the shape of the work and              none of its facts.              

When to fold, and what to keep: fold a stretch when, and only when, the step              you are on no longer needs it. KEEP, verbatim, in the summary: paths with their              line numbers, signatures and the lines that carry the logic, exact error text,              decisions together with the reason they were taken, exact values and names, and              how the goal has changed. DROP: the output of a command once you have taken the              result out of it, a file you read twice, the route you took to a conclusion, and              a dead end -- but keep what the dead end taught.              

Folds fold into folds: pass `covers` with the ids of earlier folds to replace              them with one summary above them. They stay underneath it, in full.",
            json!({
                "topic": s("One line saying what this stretch of work was"),
                "summary": s("What it established -- the part you will carry from now on"),
                "detail": s("The raw material, verbatim: kept out of your context, never lost"),
                "covers": json!({ "type": "array", "items": { "type": "string" },
                    "description": "Fold ids this one replaces, to build a tier above them" }),
                "scope": json!({ "type": "string",
                    "enum": ["agent", "group", "session", "global"],
                    "description": "Who may see it. Defaults to 'agent' -- your own working                                     context. Use 'session' for what the team should know." }),
                "group": s("Group name, when scope is 'group'"),
                "tokens_before": json!({ "type": "integer",
                    "description": "The real token count of what you folded, if you know                                     it. Otherwise it is estimated from the detail, which                                     only affects the arithmetic you are shown." })
            }), &["topic", "summary"]),
        tool("unfold_context",
            "Read back what a fold put away: the detail, verbatim, exactly as it was handed              over. Call this the moment you need an exact value again rather than working              from the summary -- that is what folding is for, and re-deriving what you              already know is the cost it exists to avoid. A fold that covers other folds              answers with those, so a question walks down the tree only as far as it has to.",
            json!({ "id": s("The fold id, as context_map or search_context gave it") }),
            &["id"]),
        tool("search_context",
            "Find a fold by words, in its topic, its summary OR the detail it put away. This              is how something you are no longer carrying is found again -- search first,              unfold second, and only then go and read the files.",
            json!({
                "search": s("Words to look for"),
                "scope": json!({ "type": "string",
                    "enum": ["agent", "group", "session", "global"],
                    "description": "Look only here. Omitted means everything you can see." }),
                "limit": json!({ "type": "integer", "description": "Most folds to return" })
            }), &["search"]),
        tool("context_map",
            "How full your context is, what you have folded so far, and what is worth folding              next. Read it when you are told you are filling up, and whenever you are picking              up work you do not remember starting.",
            json!({}), &[]),
        tool("memory_history",
            "What a key used to say, and who changed it. Two teammates writing one key is \
             normal; this is how you see it happened.",
            json!({ "key": s("The key"),
                    "scope": json!({ "type": "string",
                        "enum": ["agent", "group", "session", "global"] }),
                    "group": s("Group name, when scope is 'group'") }), &["key"]),
        tool("who_owns",
            "Who on this team is responsible for a path -- a file, a directory, a container. \
             Ask BEFORE editing anything you did not write: a team that has divided a \
             repository between its agents expects work inside somebody's area to be handed \
             to them, not done for them. Answers with the teammate, the zone that covers it, \
             and what to do about it.",
            json!({ "path": s("Path in the workspace, e.g. 'verti/src/main.rs' or 'ombia'") }),
            &["path"]),
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
                                    it is built automatically the first time. One workspace \
                                    only, so not with a wider scope." }),
                "scope": json!({ "type": "string",
                    "enum": ["workspace", "session", "global"],
                    "description": "How far to look. 'workspace' (the default) is your own \
                                    directory, and the only scope that parses on demand. \
                                    'session' is every workspace this team has already \
                                    parsed -- use it when teammates have their own checkouts \
                                    of the same code. 'global' is every parsed workspace on \
                                    the fleet, for finding where something lives at all." })
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
        tool("team_rules",
            "Read this team's standing rules: what the operator or your orchestrator has \
             set for everybody working on this session, over and above your own brief. \
             You were given them in your first prompt; read them again if you have been \
             running a long time, after a compaction, or whenever you are told they have \
             changed. They outrank your own preferences about how to work.",
            json!({}), &[]),
        tool("get_history",
            "Query the session's message history.",
            json!({ "limit": json!({ "type": "integer", "description": "Most recent N messages" }) }), &[]),
    ]
}

/// The fleet-management surface, shown only to a seat that holds the
/// authority to use it.
///
/// Advertised conditionally rather than merely refused on call. A tool a
/// model can see is a tool it will try, and an agent refused by `elect` will
/// reword the arguments and try again -- several times -- before it believes
/// the answer. Not listing it is the honest signal.
///
/// These used to live in a SEPARATE MCP SERVER holding an operator token,
/// which meant authority travelled with whichever MCP config named that
/// server: install it user-scoped and every CLI that user started was a
/// fleet operator. They are here now, decided per caller by
/// `slots.authority`, because the hub already knows which seat is calling.
pub fn orchestrator_tools() -> Vec<ToolDef> {
    vec![
        tool("elect",
            "Change what a seat is ALLOWED TO DO: 'agent' (no fleet tools), \
             'orchestrator' (runs one team -- its own session), or 'super' \
             (runs the fleet). This is authority, not the seat's job -- for the job \
             description, which confers nothing, there is nothing to elect. An \
             orchestrator can elect other orchestrators inside its own session only, \
             and can never make a superorchestrator.",
            json!({ "agent": s("The seat's name, or its slot id"),
                    "authority": s("'agent', 'orchestrator' or 'super'") }),
            &["agent", "authority"]),
        tool("set_team_rules",
            "Set this team's standing rules: the conventions, the things not to touch, \
             the way this team works -- what is true of the WORK rather than of one \
             seat. Every seat gets them in its first prompt, and everybody already \
             running is told in the chat that they have changed.\n\n\
             This REPLACES whatever is there, so read `team_rules` first and send the \
             whole text back with your change in it. Short and specific: rules nobody \
             can hold in their head are rules nobody follows. Send an empty string to \
             clear them.",
            json!({ "text": s("The rules, in full. Empty clears them.") }),
            &["text"]),
        tool("add_agent",
            "Add a seat to a team. It starts empty and disconnected; `spawn` or the \
             operator fills it. An orchestrator adds to its own session only.",
            json!({ "name": s("What teammates will call it"),
                    "role": s("Its job, e.g. 'Dev' -- confers no authority"),
                    "cli": s("Which CLI should fill it, e.g. 'claude', 'codex', 'opencode'"),
                    "model": s("The model to ask for (optional)"),
                    "session": s("Which team, by name or id. Defaults to your own.") }),
            &["name"]),
        tool("remove_agent",
            "Take a seat out of a team for good, stopping whatever fills it. Its \
             history stays in the session log. An orchestrator removes from its own \
             session only.",
            json!({ "agent": s("The seat's name, or its slot id") }),
            &["agent"]),
        tool("rename_agent",
            "Rename a seat. Nothing else changes: same seat, same agent, same history, \
             same inbox. An orchestrator renames within its own session only.",
            json!({ "agent": s("The seat's current name, or its slot id"),
                    "name": s("The new name, 64 characters or fewer, unique in its session") }),
            &["agent", "name"]),
        tool("move_agent",
            "Move a seat to another team. Needs reach over BOTH the team it is in and \
             the one it is going to, so in practice this is a superorchestrator's tool: \
             an orchestrator reaches one session, and moving a seat from it to itself is \
             not a move.",
            json!({ "agent": s("The seat's name, or its slot id"),
                    "session": s("The team to move it to, by name or id") }),
            &["agent", "session"]),
        tool("invite_agent",
            "Have a seat from ANOTHER team also answer in one of yours, without moving it \
             or giving that team a copy of its own. For a service every team needs -- a \
             capability bridge, a reviewer -- so that one process, one model and one set \
             of warm context serves all of them. Needs reach over both teams, so this is \
             a superorchestrator's tool. Pass leave=true to take it out again; a seat \
             cannot be removed from the team it LIVES in this way.",
            json!({ "agent": s("The seat's name, or its slot id"),
                    "session": s("The team it should also answer in, by name or id"),
                    "leave": json!({ "type": "boolean",
                                     "description": "Take it out of that team instead" }) }),
            &["agent", "session"]),
        tool("list_seats",
            "Every seat you have reach over -- its team, name, job, authority, what is \
             filling it and where. An orchestrator sees its own session; a \
             superorchestrator sees the fleet.",
            json!({}), &[]),
    ]
}

/// What this caller is shown, which is what this caller may call.
/// What an OBSERVER may call: read the state, answer whoever asked, and ask
/// a seat how it is getting on. Nothing else.
///
/// A subset of `team_tools`, by name, rather than its own list of
/// definitions: one description per tool, so the observer is told the same
/// thing about `recall` as everybody else, and a tool whose wording improves
/// improves here too.
///
/// `ask_progress` is the exception and is an observer's only way to put
/// anything in front of a teammate. It takes NO free text on purpose --
/// see its description.
pub fn observer_tools() -> Vec<ToolDef> {
    let allowed = OBSERVER_READS;
    let mut v: Vec<ToolDef> =
        team_tools().into_iter().filter(|t| allowed.contains(&t.name.as_str())).collect();
    v.push(tool("ask_progress",
        "Ask one teammate how it is getting on. This is the ONLY thing an observer can          put in front of a teammate, and it carries no words of yours: the question is          fixed, so that watching a team cannot turn into steering it.\n\n         Ask for `progress` (what state the work is in), `tests` (the result of the last          build or test run) or `eta` (when they expect to be done). Once per thing per          seat: a seat answering an observer is a seat not doing its work.",
        json!({ "agent": s("The teammate's name, role or seat id"),
                "want": s("'progress', 'tests' or 'eta'") }),
        &["agent"]));
    v
}

/// The tools an observer may READ with. Everything here is a question about
/// state, or an answer to somebody who asked the observer something.
const OBSERVER_READS: &[&str] = &[
    // Who is here and what they are doing.
    "list_peers",
    "check_team_status",
    "get_plan",
    "get_history",
    "workspace_status",
    "view_file_locks",
    "who_owns",
    // What the team knows. Reading the memory is most of the point: somebody
    // outside wants the team's own account of the work, not a summary of a
    // summary.
    "query_knowledge",
    "recall",
    "memory_history",
    // The folds, and the detail underneath them. An observer that can read
    // the team's memory but not what it put away would be reading the
    // summaries of summaries it was created to see past.
    "search_context",
    "unfold_context",
    "context_map",
    "query_code_graph",
    "team_rules",
    "pending_asks",
    // Its own mail, and answering it. `reply` goes back to whoever asked --
    // which for an observer is usually the operator, and that conversation
    // is the reason the seat exists.
    "check_messages",
    "claim_messages",
    "reply",
    "notify_operator",
    // Its own status line, so the fleet can see what it is watching.
    "set_summary",
];

pub fn team_tools_for(authority: crate::authority::Authority) -> Vec<ToolDef> {
    if authority.is_read_only() {
        return observer_tools();
    }
    let mut v = team_tools();
    if authority.is_orchestrating() {
        v.extend(orchestrator_tools());
    }
    v
}

/// May a seat at this level call this tool?
///
/// THE GATE, checked on every call in `hub::tools::dispatch`. Advertising is
/// not authorising: a model that has seen a tool once will call it again from
/// memory after being demoted, and a model told in a prompt not to do
/// something will route around that the moment it believes it is helping.
pub fn allowed_for(authority: crate::authority::Authority, name: &str) -> bool {
    if authority.is_read_only() {
        return observer_tools().iter().any(|t| t.name == name);
    }
    if needs_authority(name) {
        return authority.is_orchestrating();
    }
    team_tools().iter().any(|t| t.name == name)
}

pub fn tool_names() -> Vec<String> {
    team_tools().into_iter().map(|t| t.name).collect()
}

pub fn is_known_tool(name: &str) -> bool {
    team_tools().iter().any(|t| t.name == name)
        || orchestrator_tools().iter().any(|t| t.name == name)
        // An observer's list is a subset of the team's plus one tool of its
        // own, and "is this a tool at all" must know about that one too --
        // otherwise a shim refuses it before the hub ever sees it.
        || observer_tools().iter().any(|t| t.name == name)
}

/// Is this one of the tools that authority gates?
pub fn needs_authority(name: &str) -> bool {
    orchestrator_tools().iter().any(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::authority::Authority;

    #[test]
    fn an_observer_cannot_say_anything_of_its_own_to_a_teammate() {
        // The whole point of the level. `send_message` and `broadcast` carry
        // the caller's own words, which is how watching becomes steering.
        for banned in ["send_message", "broadcast", "escalate"] {
            assert!(
                !allowed_for(Authority::Observer, banned),
                "an observer must not be able to call {banned}"
            );
            assert!(
                !observer_tools().iter().any(|t| t.name == banned),
                "{banned} must not even be advertised to an observer"
            );
        }
    }

    #[test]
    fn an_observer_cannot_change_anything() {
        for banned in [
            "store_knowledge", "store_memory", "forget", "save_context", "update_plan",
            "fold_context",
            "signal_done", "acquire_file", "release_file", "approve", "answer_ask",
            "publish_work", "sync_workspace", "submit_feedback", "set_team_rules",
            "elect", "add_agent", "remove_agent", "rename_agent", "move_agent",
            "invite_agent", "list_seats",
        ] {
            assert!(!allowed_for(Authority::Observer, banned), "observer can call {banned}");
        }
    }

    #[test]
    fn an_observer_can_read_the_state_and_the_teams_memory() {
        // Reading the memory is most of why the seat exists: somebody
        // outside wants the team's own account of the work.
        for ok in [
            "list_peers", "check_team_status", "get_plan", "query_knowledge", "recall",
            "memory_history", "get_history", "team_rules", "check_messages", "reply",
            "search_context", "unfold_context", "context_map",
            "notify_operator", "ask_progress", "set_summary",
        ] {
            assert!(allowed_for(Authority::Observer, ok), "observer cannot call {ok}");
        }
    }

    #[test]
    fn asking_for_progress_carries_no_words_of_the_observers_own() {
        // If it took a `text`, it would be `send_message` with extra steps.
        let t = observer_tools().into_iter().find(|t| t.name == "ask_progress").unwrap();
        let props = t.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("agent") && props.contains_key("want"));
        assert_eq!(props.len(), 2, "no free-text field: {props:?}");
    }

    #[test]
    fn an_ordinary_agent_is_unaffected_by_the_observer_level_existing() {
        for ok in ["send_message", "broadcast", "store_knowledge", "signal_done"] {
            assert!(allowed_for(Authority::Agent, ok));
        }
        assert!(!allowed_for(Authority::Agent, "list_seats"));
        assert!(!allowed_for(Authority::Agent, "ask_progress"),
                "ask_progress is an observer's substitute for send_message, not an extra");
    }

    #[test]
    fn an_orchestrator_keeps_everything_it_had() {
        for ok in ["send_message", "list_seats", "elect", "set_team_rules"] {
            assert!(allowed_for(Authority::Orchestrator, ok), "{ok}");
            assert!(allowed_for(Authority::Super, ok), "{ok}");
        }
    }

    #[test]
    fn an_observers_list_is_strictly_smaller_than_an_agents() {
        let obs = team_tools_for(Authority::Observer).len();
        let agent = team_tools_for(Authority::Agent).len();
        assert!(obs < agent, "{obs} vs {agent}");
    }

    #[test]
    fn an_observers_own_tool_is_not_given_to_anybody_else() {
        // It is the restricted substitute for `send_message`, not an extra,
        // and a seat that has `send_message` has no business with it. The
        // hub refuses it for the same reason it refuses the other direction:
        // a gate with a hole is one somebody adds a tool behind.
        for a in [Authority::Agent, Authority::Orchestrator, Authority::Super] {
            assert!(!allowed_for(a, "ask_progress"), "{a:?}");
            assert!(!team_tools_for(a).iter().any(|t| t.name == "ask_progress"));
        }
        assert!(allowed_for(Authority::Observer, "ask_progress"));
    }

    #[test]
    fn an_unknown_tool_is_allowed_to_nobody() {
        for a in [Authority::Observer, Authority::Agent, Authority::Orchestrator,
                  Authority::Super] {
            assert!(!allowed_for(a, "rm_minus_rf"));
            assert!(!allowed_for(a, ""));
        }
    }

    #[test]
    fn every_tool_an_observer_is_shown_is_one_it_may_call() {
        // Otherwise the seat is advertised something that refuses, which is
        // the exact failure the authority work was done to stop.
        for t in observer_tools() {
            assert!(allowed_for(Authority::Observer, &t.name), "{}", t.name);
        }
    }

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
    fn the_handshake_text_covers_the_chat_not_only_the_tools() {
        let t = team_instructions("Max", "sl-1");
        assert!(t.contains("as 'Max'"), "{t}");
        assert!(t.contains("sl-1"));
        for word in ["operator", "claim_messages", "broadcast", "notify_operator",
                     "[PENDING MESSAGES]", "query_knowledge", "ask_capability",
                     "who_owns", "query_code_graph", "store_knowledge"] {
            assert!(t.contains(word), "{word} missing from the handshake text");
        }
        // A seat with no name yet reads as a sentence, not as "as ''".
        let anon = team_instructions("", "sl-2");
        assert!(!anon.contains("as ''"), "{anon}");
        assert!(anon.contains("team (slot sl-2)"), "{anon}");
    }

    #[test]
    fn known_tool_lookup_works() {
        assert!(is_known_tool("send_message"));
        assert!(!is_known_tool("rm_minus_rf"));
    }
}
