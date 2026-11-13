# Descriptors for this fleet

`clis/` in the repository root holds the generic, shipped descriptors, and
`clis/profiles/` holds worked examples. These are the ones this deployment
actually uses, with real account names.

Two claude accounts, as the same user on each machine:

| id | configuration directory | account |
|---|---|---|
| `claude-nextime` | the default (no variable set) | the default login |
| `claude-thinkdifferent` | `~/.claude-thinkdifferent` | the second login |

Plain `claude` is deliberately **not** installed alongside them. With both
accounts present, an agent asking for "claude" would get whichever the file
happened to provide, and an agent should not pick an account by accident.

Each directory must already hold a logged-in configuration — nexmesh points a
CLI at credentials, it never creates them — and the login is per machine, so
it is done once on each:

```bash
mkdir -p ~/.claude-thinkdifferent
CLAUDE_CONFIG_DIR=~/.claude-thinkdifferent claude     # log in interactively
```

## Why `claude-nextime` sets no variable

Naming the default explicitly would have been wrong. With `CLAUDE_CONFIG_DIR`
set, claude keeps `.claude.json` *inside* that directory; with it unset, this
install keeps it at `~/.claude.json`. Those are different files, and on this
machine the one inside `~/.claude/` is months out of date — so
`CLAUDE_CONFIG_DIR=~/.claude` would have pointed agents at a stale session
while looking entirely correct.

So: `claude-nextime` sets nothing and inherits whatever a terminal `claude`
uses, which is exactly the claim it makes. Only the *second* account names a
directory, and because that directory is genuinely separate it gets its own
`.claude.json` inside it, as intended.
