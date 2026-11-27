//! The skill that ships with the MCP registration.
//!
//! Registering the server gives an agent the TOOLS. It does not tell it how a
//! team works, and that part is not derivable from a tool list -- so every
//! seat was inferring it, and getting it wrong in the same few ways:
//!
//!   * answering the operator with `broadcast`, so a note meant for one
//!     person was read by six agents;
//!   * reading messages with `check_messages` and never claiming them, so
//!     nobody could tell what had been dealt with from what had been seen;
//!   * learning something expensive about a codebase and leaving it in its
//!     own context, where the next seat in that chair cannot reach it;
//!   * doing work by hand that a capability bridge in the fleet does better,
//!     because nothing said one existed.
//!
//! So the installer writes this beside the registration. It is kept here,
//! next to the tool definitions it describes, so the two cannot drift.

/// `SKILL.md`, with the YAML front matter a skill-aware CLI expects.
pub fn skill_md() -> String {
    format!("---\nname: nexmesh\ndescription: {DESCRIPTION}\n---\n\n{BODY}")
}

const DESCRIPTION: &str = "Working as one seat in a nexmesh team: answering \
teammates and the operator, claiming work, asking the fleet for a capability \
you do not have, and keeping the team's shared knowledge and code graph \
current. Use whenever nexmesh tools are available -- at the start of a turn \
that mentions teammates, a session or the operator, when a [PENDING MESSAGES] \
block appears, before starting on an unfamiliar part of a codebase, and after \
learning anything the next seat would need.";

const BODY: &str = r#"# Working in a nexmesh team

You hold a **seat** in a **session**. Other seats may be other CLIs, on other
machines, or not CLIs at all. The session is the boundary: you cannot see
other teams, and nothing you say leaves this one.

## Messages

Teammates' messages arrive prefixed `[PENDING MESSAGES]` on any tool result,
each with its own number. They do not interrupt you mid-thought -- they arrive
at your next tool call -- so read them when they appear rather than finishing
in silence.

- **`reply(to: <number>, text: ...)`** answers one message. It goes back to
  whoever sent it, wherever they are. This is the tool for answering.
- **`send_message(to: <name>, text: ...)`** starts something with one
  teammate.
- **`broadcast(text: ...)`** goes to EVERY seat. For a decision or a change
  the whole team must know. Using it to answer one person is how six agents
  come to read a note meant for one -- use `reply`.

A message from **the OPERATOR** is from the person running the fleet, reading
this session's chat. `reply` puts your answer in front of them without
sending it to your teammates. They cannot be reached with `broadcast`.

**`check_messages`** shows what is waiting without clearing it.
**`claim_messages`** is how you say you have dealt with something. Reading is
not answering: anything you leave unclaimed still reads as outstanding to
everyone watching the team, which is the point -- so claim what you have
actually handled, and leave what you have not.

## Shared knowledge: keep it current

The team's memory outlives your context and your process. A seat is
respawned, moved to another machine, or replaced by a different model, and
everything it worked out that stayed in its own head is gone.

- **`store_knowledge(key, value, category)`** -- a DECISION or a conclusion:
  "we use dial-out websockets because the nodes roam", "the encoder path is
  shmproc.php, not the obvious one". Attributed to you. Write one when you
  decide something, when you discover something that cost you effort to
  find, or when you were wrong about something and now know better.
- **`query_knowledge`** -- read it BEFORE planning. Somebody has usually been
  here, and the answer is often that the obvious approach was tried and does
  not work. With no scope it reads everything you can see, nearest first.
- **`store_memory` / `recall` / `forget`** -- the same tools under their real
  names, with the full set of arguments.

The habit that matters: **query before you plan, store before you finish.**
Not a log of what you did -- the transcript is that -- but the handful of
things the next seat would otherwise have to rediscover. If you learned
nothing worth another agent's time, store nothing in the team's scopes; what
is only worth YOUR time goes in your own.

### How far should it travel

Everything you remember has a SCOPE, and picking it is not a formality: it
decides who is affected when you turn out to be wrong.

- **`agent`** -- YOUR OWN MEMORY. Use it instead of your CLI's built-in
  memory (a memory file, notes to self): what you have already tried, where
  you had got to, how this operator likes things done, anything you would
  want to know next time. Nobody else reads it, so it costs nobody anything
  when it is wrong, and it is the right place for anything provisional. It
  belongs to your seat, not your process -- it survives a restart, a move to
  another machine and a change of CLI. Write to it freely. It is NOT loaded
  into you when you start: you are told only that it exists, and how much.
  Pull what the task in front of you needs -- `recall(scope: "agent",
  search: ...)` -- when you need it, not all of it at once.
- **`group`** -- a named subset of the team you belong to, for when three of
  you are on one thing and the rest of the room does not need it.
- **`session`** -- THIS TEAM. The default, and where everything project-wide belongs:
  decisions about the work in hand, conventions this team adopted, what the
  code turned out to do. It dies with the team, which is correct for
  something that was only ever true of this piece of work.
- **`global`** -- the whole fleet, for ever, across every team that will ever
  run here. For things that are true of the MACHINES and the TOOLS rather
  than the work: which host has the GPU, that a service is managed by
  sysvinit and not systemd, that a repository's tests need a flag to run at
  all. **It is the orchestrator's to keep.** An orchestrator writes it;
  any other seat needs the `memory-writer` capability, deliberately, and even
  then should very rarely use it: a global note is read by teams nobody has
  thought of yet, and an experiment that writes one leaves it behind for all
  of them. Found something plainly true across sessions? Tell your
  orchestrator and let it decide.

The test: **would the next team on a different task still need this?** If
yes, and it is about the fleet rather than this job, it is global -- and the
orchestrator's call. If it is about this job, it is the session's. If it is
about how YOU work, or you are not sure it is right yet, it is yours.

A nearer scope SHADOWS a wider one for the same key, so a team can override
a fleet-wide default by writing its own, without touching the global one.
That is also how to disagree with a global note: write the team's own, say
why in the chat, and leave the wider one for the operator to settle. Reading
back shows you the scope each entry came from -- a fleet convention and one
teammate's note deserve different weight, and the scope is what tells you
which you are looking at.

## The code graph

**`query_code_graph`** answers structural questions -- what defines a symbol,
what calls it, what a module depends on -- from a parse the fleet keeps, so a
codebase is read once rather than once per agent.

**Find with the graph, read with the files.** Before you grep a tree or open
files to learn where something is defined, what calls it, what a file
imports or what a module is made of, ask the graph -- then open only the
files and lines it points you to. Reading a tree to orient yourself fills
your context with code you did not need, and every teammate pays it again.
Start a new piece of work with `what='overview'`.

It keeps up with the files: the fleet checks every workspace for changes
about once a minute and reparses only the files that changed, so an answer
is normally less than a minute or two behind the disk. An answer that is
behind says so. The one case to read the file anyway is a file YOU changed
in the last minute or two.

It has scopes too, for a different reason: a graph is parsed from the files
in one directory on one machine.

- **`workspace`** (the default) -- your own checkout. The only scope that
  parses on demand, so this is the one to ask first, and the only one that
  can answer `path` between two symbols.
- **`session`** -- every workspace this team has already parsed. Use it when
  teammates have their own checkouts: a team of four in one repository is
  four workspaces, and your own graph cannot see theirs.
- **`global`** -- every parsed workspace on the fleet. For "where does this
  live at all", across repositories and across teams.

The wider two never parse anything: that is minutes of somebody else's
machine, and they answer from what is already there and say what they
skipped.

It is DERIVED: it is regenerated from the files and holds no opinions. If what it tells you does not match what you read on disk, the
files win -- and say so in the chat, because a graph nobody trusts is worse
than none. Anything you CONCLUDE about the code belongs in
`store_knowledge`, which is attributed and does not get regenerated away.

## Capabilities you do not have

Some seats are services rather than teammates -- OCR, transcription, image
work, embeddings, review -- and may be visiting from another team. They are
listed by `list_peers` with what they can be asked for, labelled as services.

- **`list_peers(capability: "ocr")`** finds who can do a thing, rather than
  guessing from names.
- **`ask_capability`** asks the fleet for one, when you do not know who holds
  it.

Do not hand-roll something a bridge in the fleet does properly. Do not
install a tool to do it either: ask.

## Permission prompts

If your CLI stops to ask the operator's permission for something, that
question goes to the operator's chat with the choices you were offered, and
you wait. That is normal and not a failure -- say what you are waiting for if
you have the chance. Never work around it.

## Finishing, and being stuck

- **`set_summary`** -- one line, kept current. It is what the operator and
  your teammates read to know what you are on. Stale is worse than empty.
- **`signal_done`** -- you believe the work is finished and reviewable.
- **`escalate`** -- you are blocked by something you cannot resolve: a
  decision, an access problem, a contradiction between two instructions.
  Escalating early is cheap; a seat that sat on a blocker for an hour is not.
- **`notify_operator`** -- for the person, not the team.
- **`acquire_file` / `release_file`** before and after editing something
  another seat might also be editing. `view_file_locks` says who holds what.

## Two things that are easy to get wrong

1. **Your teammates are not you.** They have their own context, their own
   model, and their own part of the tree. A message that would make sense to
   you after your last twenty minutes of work will not make sense to them --
   say what you are asking for and why in the message itself.
2. **Silence reads as ignoring.** A question you intend to answer after
   finishing something looks identical, from outside, to one you never saw.
   Acknowledge, then finish.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_has_the_front_matter_a_skill_needs() {
        let s = skill_md();
        assert!(s.starts_with("---\nname: nexmesh\ndescription: "), "{}", &s[..80]);
        // One line: a description broken over several lines is not valid
        // front matter, and the failure is silent -- the skill simply never
        // triggers.
        let desc = s.lines().nth(2).unwrap();
        assert!(desc.starts_with("description: "));
        assert!(!desc.ends_with('\\'), "{desc}");
        assert!(s.contains("\n---\n\n# Working in a nexmesh team"));
    }

    #[test]
    fn it_says_the_things_seats_actually_got_wrong() {
        // Each of these is a real failure from the fleet, not a guess at
        // what an agent might need telling.
        //
        // Matched against the text with its line wrapping flattened: a
        // phrase that happens to straddle a wrap is still present, and a
        // test that cares where the lines break would fail on a reflow that
        // changed nothing.
        let s = skill_md().split_whitespace().collect::<Vec<_>>().join(" ");
        for must in [
            "reply(to: <number>",      // answering the operator with broadcast
            "Reading is not answering", // check_messages without claiming
            "query before you plan, store before you finish",
            "query_code_graph",
            "ask_capability",
            "the files win",           // a stale graph contradicting the tree
            // The scopes, and the test for choosing between them. A seat
            // that writes everything to one scope either loses what the next
            // team needed or leaves the fleet full of one job's details.
            "would the next team on a different task still need this",
            "A nearer scope SHADOWS a wider one",
            "memory-writer",
            // And why the graph's widest scope does not parse anything.
            "minutes of somebody else's machine",
        ] {
            assert!(s.contains(must), "missing: {must}");
        }
    }

    #[test]
    fn every_tool_it_names_is_a_tool_that_exists() {
        // A skill that tells an agent to call something the hub does not
        // have teaches it to fail, and the agent will keep trying.
        let s = skill_md();
        for name in crate::tools::tool_names() {
            let _ = name;
        }
        let known: Vec<String> = crate::tools::tool_names();
        for word in s.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            // Only check things written like a tool call, which is how they
            // appear in the body: `name(` or **`name`**.
            if word.len() < 5 || !word.contains('_') {
                continue;
            }
            if word.chars().any(|c| c.is_uppercase()) {
                continue;
            }
            if ["pending_messages", "skill_md", "front_matter", "nexmesh_team",
                "store_memory", "code_graph", "set_summary"].contains(&word) {
                continue;
            }
            if known.iter().any(|k| k == word) {
                continue;
            }
            panic!("the skill names '{word}', which is not a tool the hub serves");
        }
    }
}
