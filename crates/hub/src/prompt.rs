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

WHOSE WORK IS WHOSE
- `list_peers` says what each teammate OWNS as well as who they are. A team \
  with one checkout and several agents divides it: a container, a directory, \
  a subsystem each.
- Before editing anything you did not write, ask `who_owns <path>`. If it \
  belongs to somebody, the work is theirs: `send_message` them what needs \
  doing and why. Do not do it for them -- two agents editing one area undo \
  each other, and the one who owns it is the one who knows why it is like \
  that.
- Work in nobody's zone is yours to take; take a lock first if the workspace \
  is shared.
- If you believe a zone is wrong, say so to the operator with \
  `notify_operator` rather than editing across it.

WHAT THE TEAM KNOWS, AND KEEPING IT THAT WAY
- The memory and the code graph belong to the TEAM, not to you. Read before \
  you decide -- `query_knowledge` for what has been settled, \
  `query_code_graph` for what the code actually does -- and write what you \
  learn back, as you learn it.
- `store_knowledge` / `store_memory` is for a decision, a convention, a \
  discovery or a dead end that would otherwise be rediscovered by somebody \
  else tomorrow. One key per thing, so it can be updated rather than \
  duplicated; `recall` and `memory_history` show what is there and how it \
  changed.
- When your work changes something the graph describes -- a module moved, an \
  interface renamed -- say so, so the next agent asking about it is not \
  reading last week's shape.
- Contradicting a teammate's memory is a conversation, not an overwrite: \
  `send_message` them, settle it, then one of you writes the answer.

THE CHAT, AND WHO IS IN IT
- Every message is in the team's chat, and a person is reading it. The operator \
  sees the whole room on a dashboard, in a terminal console and on a phone, and \
  can say something to the team or to you by name at any time.
- A message whose sender is 'operator' IS that person, not a teammate. Answer \
  it with `send_message` addressed to 'operator' -- replying to a teammate \
  instead sends your answer to somebody who never asked.
- `send_message` is for one teammate; `broadcast` is for the room. Prefer one \
  teammate: a room full of agents reading a message meant for one is how a \
  team of models burns a budget.
- Reading a message does not deal with it. `check_messages` shows what is \
  waiting; `claim_messages` is what says you have handled it. Anything \
  unclaimed is still on the board for whoever reads the fleet.
- If you are stuck on something nobody on the team can settle -- a credential, \
  a decision that is not yours, a machine that is not answering -- call \
  `notify_operator`. It reaches your team's orchestrator, and the people \
  running the fleet when there is none.

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
    fn the_prompt_says_whose_work_is_whose_and_what_the_team_knows() {
        // A team that divides a repository gains nothing if its agents do
        // not know they have to ask -- and the memory is only shared if
        // somebody writes to it.
        let p = first_prompt(&brief(true));
        assert!(p.contains("who_owns"), "{p}");
        assert!(p.contains("says what each teammate OWNS"), "{p}");
        assert!(p.contains("query_code_graph"));
        assert!(p.contains("store_knowledge"));
        assert!(p.contains("belong to the TEAM"));
    }

    #[test]
    fn the_prompt_explains_the_chat_and_who_reads_it() {
        // An agent that does not know a person is in the room answers the
        // operator by messaging a teammate -- which is exactly what one did.
        let p = first_prompt(&brief(true));
        assert!(p.contains("operator"), "{p}");
        assert!(p.contains("claim_messages"));
        assert!(p.contains("notify_operator"));
        assert!(p.contains("broadcast"));
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
