# Named configurations: two logins for one CLI, on one machine, as one user

A node spawns agent CLIs as the user it runs as, so by default every agent on
a machine shares that user's credentials — one login per CLI. To have two
accounts for the same tool, give each its own **descriptor** with its own
environment pointing at its own configuration directory. Same binary, same
user, different login.

Copy one of these into the node's `clis/` directory (`/etc/nexmesh/clis/` in
the deployment here), rename the file to match its `id`, and set the paths.
The id is what `--cli` and `add_agent` name, so pick something you will
recognise: `claude-personal`, `claude-work`.

The variable differs per CLI. These were read from the installed binaries
rather than from documentation:

| CLI | variable | verified |
|---|---|---|
| claude | `CLAUDE_CONFIG_DIR` | yes, present in the binary |
| codex | `CODEX_HOME` | yes, present in the binary |
| opencode | `OPENCODE_CONFIG_DIR` | yes, present in the binary |
| qwen | `QWEN_CONFIG_DIR` (also `QWEN_DIR`) | yes, present in the binary |
| gemini | not checked — it was not installed here | **no** |

`HOME` is the blunt instrument that works for anything, including a CLI whose
own variable you cannot find. It moves caches and git configuration too, so
prefer the specific one where it exists.

## Setting one up

The directory has to be a real, logged-in configuration. nexmesh does not log
anything in — it points a CLI at credentials that already exist:

```bash
mkdir -p ~/.claude-work
CLAUDE_CONFIG_DIR=~/.claude-work claude      # log in once, interactively
```

Then the descriptor below makes that login available as its own CLI id, and a
slot picks it with `--cli claude-work`. Both can run at the same time on the
same machine.

## What a descriptor's environment cannot do

It cannot override the `NEXMESH_*` variables: those are how the MCP shim finds
its node, and an agent that could lose them would be spawned into a team it
cannot join. They are applied after the descriptor's own and win.
