---
name: secrets-hygiene
description: Working near credentials as an agent — why a secret that reaches your context is already exposed, and what to do with one you did not come for.
sources:
  - https://thenewstack.io/ai-agent-context-boundary
  - https://blog.gitguardian.com/agentic-ai-security-blast-radius/
  - https://dev.to/th3chris/how-ai-agents-leak-your-credentials-and-how-to-stop-it-124k
  - https://thehackernews.com/2026/09/secrets-sprawl-is-identity-problem-that.html
---

# Credentials, and why an agent is the dangerous part

Two things make this different from a person doing the same work. Everything
you read enters a transcript that is stored, shipped to a model and often
read later by somebody else — so a secret you merely *look at* has left the
machine. And you can act on what you find in milliseconds, without the pause
in which a person would think better of it.

Measured, not theoretical: commits identified as AI-assisted leak secrets at
roughly twice the rate of human-written ones, and thousands of live
credentials have been found sitting in published agent configuration files.
In one case an agent hit a credential mismatch on a staging task, found an
unrelated token with broad permissions, used it, and deleted a production
database.

- **Never print a secret.** Not to check it, not in a log line, not in a
  "here is what I found". `echo $TOKEN`, `env`, `cat .env` and a careless
  `grep -r` all put it in the transcript. Test a credential by *using* it in
  a child process and reading the exit status.
- **A credential you did not come for is not yours to use.** Finding a token
  that would unblock you is not permission to use it. That is the mistake
  above, exactly. Say what you found, where, and stop.
- **Say WHERE, not WHAT.** "There is an AWS key in `deploy/old.env:12`" is a
  useful report. The key itself is a second leak, in a place with a longer
  memory.
- **Never commit one, and look before you commit.** A secret in history is a
  secret to be rotated, not deleted — the commit survives in clones and
  forks.
- **Ask for the narrowest credential that does the job**, scoped to the one
  service and the one environment. Broad permissions are what turned the
  incident above from an error into an outage.
- **A secret you did handle is a secret to rotate.** If one reached your
  context, say so plainly so it can be replaced. That is cheap; a quiet
  exposure is not.
