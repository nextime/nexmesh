---
name: handover
description: Writing down what you learned so the next seat does not pay for it again — what belongs in the team's memory, and what a handoff to another agent has to carry.
sources:
  - https://danielgordon-ml.github.io/blog/context-is-the-bottleneck.html
  - https://arxiv.org/pdf/2606.08878
  - https://dev.to/crabtalk/why-multi-agent-workflows-fail-in-production-5d2a
  - https://www.splunk.com/en_us/blog/artificial-intelligence/multi-agent-system-failures.html
---

# Leaving it better documented than you found it

Everything you work out stays in your own context and dies with your seat
unless you write it somewhere the team can read. That is the main way teams
of agents waste money: three seats discovering the same thing.

**Write it as you learn it, not at the end.** A seat that is compacted,
restarted or moved loses whatever it was saving up.

`store_knowledge` is for:

- a constraint that is not visible in the code (this must run before that; a
  version is pinned on purpose; this box has no AVX)
- a reason behind something that looks wrong
- a decision, with what was rejected and why
- a trap somebody else will fall into

It is **not** for: what the code already says, what the git history already
says, a narration of what you did today, or anything that will be false next
week.

**Contradicting a teammate's memory is a conversation, not an overwrite.**
`send_message` them, settle it, then record the answer once.

**Say where things are.** A finding that names the file, the host and the
command is actionable; the same finding in general terms is a rumour.

## Handing work to somebody else

Teams of agents fail at the handoff more than anywhere else. The analyses
agree on this and on why: what one agent knows is not what the next one is
told, nothing crashes when that happens, and a better model does not fix it.

A handoff carries three things, or it is not a handoff:

- **what has been done**, concretely, and what was verified to say so;
- **what constraints apply** -- the decisions already made that this work has
  to live inside, including the ones that look arbitrary;
- **what is still open**, and which of it is blocking.

And it goes to **one named owner**. Work handed to a room is work nobody has.
If you are handing back something you could not finish, say which part, and
say what you would try next -- that sentence is usually worth more than
everything you did finish.
