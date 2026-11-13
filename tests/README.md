# Integration tests

`cargo test` covers the units. These two drive real processes end to end:

```
cargo build
python3 -I tests/e2e_team.py     # attached CLI agent + external (hermes-style) agent
python3 -I tests/e2e_spawn.py    # hub places a slot, node spawns the CLI, shim registers
python3 -I tests/e2e_attach.py   # pty takeover of an agent through the hub
python3 -I tests/e2e_secure.py   # TLS + operator auth, and every way of getting it wrong
python3 -I tests/e2e_driver.py   # mid-turn injection, acks, tokens, resume, flap detection
python3 -I tests/e2e_adopt.py    # adopting a tmux session that was already running
python3 -I tests/e2e_codex.py    # the Codex app-server driver, incl. a real-binary handshake
python3 -I tests/e2e_dashboard.py     # the dashboard page and its snapshot
python3 -I tests/e2e_orchestrator.py  # spinning up a team through the MCP orchestrator
python3 -I tests/e2e_mtls.py          # mutual TLS: a valid token alone must be refused
python3 -I tests/e2e_pairing.py       # joining with a one-time code; the key never leaves
python3 -I tests/e2e_opencode.py      # the http_server driver, and a TUI opened on demand
python3 -I tests/e2e_workspace.py     # two machines sharing one repo; conflicts preserved
python3 -I tests/e2e_control.py       # pause really withholds work; release really lets go
python3 -I tests/e2e_revoke.py        # a revoked certificate fails at the handshake
python3 -I tests/e2e_bind.py          # a certificate must name the identity its token claims
python3 -I tests/e2e_plugin.py        # a CLI nexmesh knows nothing about, driven by an external plugin
python3 -I tests/e2e_mcpdoor.py       # the MCP front door: a hand-written MCP client joins a team
python3 -I tests/e2e_capability.py    # models, capability labels, escalation and a token budget
python3 -I tests/e2e_memory.py        # scoped memory, groups, search, and resuming days later
```

Neither spends API credits. `e2e_team.py` drives `nexmesh-mcpd` with raw MCP
JSON-RPC, standing in for a CLI. `e2e_spawn.py` spawns `tests/fake-cli/fakecli`,
a minimal but faithful MCP *host*: it reads the `--mcp-config` the node wrote,
launches the shim with the env from that config, and speaks MCP to it — exactly
the sequence a real CLI follows. Both write their state under the system temp
directory and clean up their processes.

`e2e_attach.py` spawns `tests/fake-cli/faketui` (an interactive agent that
reports whether it is on a tty) and then drives the **real `nexmesh attach`
client** through a local pty, so raw mode, keystroke relay, `SIGWINCH` and the
Ctrl-] detach are exercised rather than simulated.

`e2e_driver.py` uses `tests/fake-cli/fakestream`, which speaks Claude Code's
real stream-json protocol -- the event shapes were copied from an actual
`claude -p --output-format stream-json --verbose --replay-user-messages` run,
so the driver is tested against the protocol it will meet. `fakecrash` always
dies, for flap detection.

`e2e_adopt.py` needs `tmux`; it skips cleanly without it.

`e2e_codex.py` drives `tests/fake-cli/fakecodex`, which implements the real
app-server protocol (shapes from `codex app-server generate-json-schema`),
including notifications without a `jsonrpc` field and a blocking approval
request. Its last two checks run the **real** `codex app-server` and confirm it
still answers `initialize` and `thread/start` the way the driver expects; they
skip if codex is not installed, and need no API credits because no turn is
started.

`e2e_orchestrator.py` drives `nexmesh-orch` with raw MCP JSON-RPC, exactly as
any MCP-capable CLI would, and checks that `create_team` really does place and
start agents on a node.

To exercise a real CLI instead, point a slot at `claude`, `opencode` or `qwen`;
that does cost credits, which is why it is not the default.
