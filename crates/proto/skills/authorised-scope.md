---
name: authorised-scope
description: Staying inside an authorised scope in security and reverse-engineering work, and what to do when something out of scope turns up.
sources:
  - https://blog.gitguardian.com/agentic-ai-security-blast-radius/
  - https://thenewstack.io/ai-agent-context-boundary
---

# Scope, evidence, and stopping

In this kind of work, scope discipline is not paperwork. It is the difference
between a finding and an incident.

- **Only what you were given.** Only the hosts, accounts, binaries and
  samples the operator named. A neighbouring target that looks interesting is
  a question for the operator, never a decision to make alone — and it is
  the one mistake an apology does not undo.
- **Least force that proves it.** A read proves access; a deletion proves
  nothing and causes damage. Never degrade a service, never destroy or alter
  data, and stop the moment the proof exists.
- **Nothing persistent.** No backdoors, no added accounts, no scheduled
  tasks, no clearing of logs. Everything you did should still be visible
  afterwards, including to you.
- **Keep evidence as you go.** The target, the exact steps, what you
  observed, the impact, your confidence, and which sample or request it came
  from. Work reconstructed from memory afterwards is not evidence.
- **Mark guesses as guesses.** "This field is a length, proven across three
  samples" and "this one looks like a flag" are different claims. A
  confident wrong claim spreads into everything built on top of it.
- **Stop and report** if you find live personal data, somebody else's
  credentials, or signs that a real attacker has been there. Those are not
  things to keep testing around.
- **A credential you find is not a credential you may use.** Not even to
  prove it works, not even to get past something in scope. An agent that
  found an unrelated token with broad permissions and used it has already
  deleted somebody's production database that way. Report it and stop; see
  `secrets-hygiene`.
