//! The team protocol text injected into every agent's first prompt.
//!
//! Written once and transport-neutral, so a `claude` process and a hermes
//! agent are told the same rules.

pub struct AgentBrief<'a> {
    pub display_name: &'a str,
    pub role: &'a str,
    pub role_description: &'a str,
    pub session_name: &'a str,
    pub node_name: &'a str,
    pub workspace_path: &'a str,
    pub shared_workspace: bool,
    pub task: &'a str,
}

pub fn first_prompt(b: &AgentBrief) -> String {
    let coordination = if b.shared_workspace {
        "Your workspace is SHARED with teammates on other machines. Before editing any \
         file outside your ownership zone, call `acquire_file`; release it when done. \
         Never assume a file you did not lock is unchanged."
    } else {
        "Your workspace is LOCAL to this machine. Teammates work on their own machines, \
         so you will not collide with them on files -- but they cannot see your files \
         either. Report findings through `send_message` and `store_knowledge`, not by \
         pointing at a path."
    };

    format!(
        "You are {name}, the {role} on team \"{session}\".

{role_desc}

YOUR TASK
{task}

YOU ARE NOT ALONE. You are one agent on a team spread across machines. You are \
running on the machine \"{node}\", in {cwd}.

HOW TO WORK WITH THE TEAM
- Call `query_knowledge` FIRST, before planning anything. A teammate may already \
  have decided something you are about to contradict.
- Call `set_summary` whenever what you are doing changes. Peers and the operator \
  read it; a stale summary makes you look stuck.
- Use `list_peers` and `check_team_status` to see who exists and what they are on.
- `send_message` reaches a teammate wherever they run -- another machine, another \
  CLI, or a program that is not a CLI at all. Address them by name or role.
- Record decisions with `store_knowledge` as you make them, not at the end. \
  Context drift between agents is the main way teams like this fail.
- {coordination}

FINISHING
- When your task is done, call `signal_done`. Do not stop before that: your work \
  needs review, and you cannot be released until the loop completes.
- If a reviewer sends actionable feedback, address it and call `signal_done` again.
- Teammate messages may arrive attached to any tool result, prefixed \
  [PENDING MESSAGES]. Read them and respond -- they are from real peers waiting on you.
",
        name = b.display_name,
        role = b.role,
        session = b.session_name,
        role_desc = b.role_description,
        task = b.task,
        node = b.node_name,
        cwd = b.workspace_path,
        coordination = coordination,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(shared: bool) -> AgentBrief<'static> {
        AgentBrief {
            display_name: "Max",
            role: "engineer",
            role_description: "You write the implementation.",
            session_name: "auth",
            node_name: "machine-a",
            workspace_path: "/srv/app",
            shared_workspace: shared,
            task: "Build the login flow.",
        }
    }

    #[test]
    fn the_prompt_names_the_agent_its_machine_and_its_task() {
        let p = first_prompt(&brief(true));
        assert!(p.contains("You are Max, the engineer"));
        assert!(p.contains("machine-a"));
        assert!(p.contains("/srv/app"));
        assert!(p.contains("Build the login flow."));
        assert!(p.contains("signal_done"));
    }

    #[test]
    fn file_coordination_advice_matches_the_workspace_model() {
        // Telling an agent on a node-local workspace to take locks would be
        // actively misleading: nobody else can touch its files.
        assert!(first_prompt(&brief(true)).contains("SHARED with teammates"));
        assert!(first_prompt(&brief(false)).contains("LOCAL to this machine"));
        assert!(!first_prompt(&brief(false)).contains("SHARED with teammates"));
    }
}
