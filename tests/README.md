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

To exercise a real CLI instead, point a slot at `claude`, `opencode` or `qwen`;
that does cost credits, which is why it is not the default.
