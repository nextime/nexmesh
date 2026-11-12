#!/usr/bin/env python3
"""The plugin driver: a CLI nexmesh knows nothing about, driven from outside.

`fakequirk` speaks none of the three protocols nexmesh handles natively --
sigil-prefixed lines, an opaque correlation tag, its own token format. Every
driver guarantee has to come from `quirkplugin`, an ordinary Python script, so
these checks are really asking: can someone add support for a new CLI without
touching nexmesh?
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17915; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-plugin-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
for name in ("fakequirk", "quirkdies", "quirkmissing"):
    d = open(f"{SCR}/fake-cli/{name}.toml").read().replace(
        'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
    open(f"{CLIS}/{name}.toml", "w").write(d)

procs = []; fails = []
def cleanup():
    print(f"  (logs: {T})")
    for p in procs:
        try: p.send_signal(signal.SIGTERM)
        except Exception: pass
    time.sleep(0.4)
    for p in procs:
        try: p.kill()
        except Exception: pass

def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond: fails.append(label)

def http(m, p, b=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=20) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, args=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, args)]
    finally: c.close()

def until(fn, seconds=25.0, step=0.3):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(step)
    return None

def nodelog():
    return open(f"{T}/node.log").read()

def make_slot(sid, cli_id, name, task="index the archive"):
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": cli_id,
        "mode": "headless", "display_name": name, "role": "engineer",
        "workspace": "local", "task": task, "placement": {"node": "machine-b"}})
    assert "slot_id" in j, j
    return j["slot_id"]

def transcript(sid, who):
    s, j = http("GET", f"/v1/sessions/{sid}/resolve/{who}")
    if "agent_id" not in j: return ""
    return http("GET", f"/v1/agents/{j['agent_id']}/transcript?tail=65536")[1].get("transcript", "")

def main():
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 QUIRKDIES_WAIT_FOR_DELIVERY="1")))
    ok = until(lambda: "fakequirk" in (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))
    check("the node advertises a CLI supported only by a plugin", bool(ok), nodelog()[-400:])

    sid = http("POST", "/v1/sessions", {"name": "plugintest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b", "mounts": {"machine-b": WS}}]})[1]["session_id"]
    slot = make_slot(sid, "fakequirk", "Max")
    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("a plugin-driven agent spawns", s == 200, str(j))

    print("\n=== the node owns the process; the plugin only translates ===")
    agent = until(lambda: (q("SELECT id,pid,push_capable FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (slot,)) or [None])[0])
    check("the agent is running", agent is not None, nodelog()[-500:])
    check("the hub recorded the CLI's own pid, not the plugin's",
          agent and agent["pid"] > 0, str(agent))
    check("and knows it can be pushed to mid-turn",
          agent and agent["push_capable"] == 1, str(agent))
    check("the node logged which plugin it started",
          "driver plugin started" in nodelog(), nodelog()[-400:])

    print("\n=== the PLUGIN delivered the first prompt, not the command line ===")
    # The descriptor has no {prompt} and no prompt_on_stdin: if the task reached
    # the CLI at all, it got there through the plugin.
    tr = until(lambda: "index the archive" in transcript(sid, "Max") and transcript(sid, "Max"))
    check("the task reached the CLI through the plugin", bool(tr), transcript(sid, "Max")[-400:])
    check("and the CLI echoed it back in its own format",
          bool(tr) and ">> index the archive" in tr, (tr or "")[-400:])

    print("\n=== a resume handle, in a format only this CLI uses ===")
    rh = until(lambda: (q("SELECT resume_handle FROM slots WHERE id=?", (slot,)) or [{}])[0].get("resume_handle"))
    check("the plugin reported a resume handle", bool(rh), str(q("SELECT resume_handle FROM slots WHERE id=?", (slot,))))
    check("it is this CLI's own session id", bool(rh) and rh.startswith("qk-"), str(rh))

    print("\n=== token accounting, parsed out of a bespoke line ===")
    until(lambda: (q("SELECT input_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,)) or [{}])[0].get("input_tokens"))
    row = q("SELECT input_tokens,output_tokens,cache_read_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,))[0]
    check("input tokens metered", row["input_tokens"] > 0, str(row))
    check("output tokens metered", row["output_tokens"] > 0, str(row))
    check("cache reads metered", row["cache_read_tokens"] > 0, str(row))

    print("\n=== a teammate's message arrives MID-TURN and is confirmed ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Max", "text": "ping through the plugin"})
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%ping through the plugin%'") or [{}])[0].get("acked_at_ms"), seconds=30)
    elapsed = time.time() - t0
    check("the message was delivered and acknowledged", bool(acked),
          str(q("SELECT id,body,acked_at_ms,inflight_agent FROM messages")))
    check(f"without waiting for a tool call ({elapsed:.1f}s)", bool(acked) and elapsed < 15, f"{elapsed:.1f}s")
    tr = transcript(sid, "Max")
    check("the agent actually saw the message",
          "PENDING MESSAGES" in tr and "ping through the plugin" in tr, tr[-500:])
    check("the ack named exact message ids, not a guess",
          "plugin confirmed delivery" in nodelog(), nodelog()[-400:])

    print("\n=== the plugin's own log reaches the node's log ===")
    check("a log frame is logged", "driving fakequirk" in nodelog(), nodelog()[-400:])
    check("and so is its stderr", "quirkplugin" in nodelog(), nodelog()[-400:])

    print("\n=== a plugin that misbehaves does not take the agent down ===")
    dslot = make_slot(sid, "quirkdies", "Dora", task="survive a bad plugin")
    http("POST", f"/v1/slots/{dslot}/spawn")
    dagent = until(lambda: (q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (dslot,)) or [None])[0])
    check("the agent starts even though its plugin prints junk", dagent is not None, nodelog()[-500:])
    check("the junk line was reported, not obeyed",
          "unparseable plugin frame" in nodelog(), nodelog()[-600:])
    dtr = until(lambda: "survive a bad plugin" in transcript(sid, "Dora") and transcript(sid, "Dora"))
    check("the prompt it did send still got through", bool(dtr), transcript(sid, "Dora")[-300:])

    print("\n=== when the plugin dies, messages go back to the queue ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Dora", "text": "sent into a dying plugin"})
    gone = until(lambda: "the driver plugin exited" in nodelog(), seconds=30)
    check("the node noticed the plugin exit", bool(gone), nodelog()[-600:])
    msg = until(lambda: (q("SELECT id,acked_at_ms,inflight_agent FROM messages WHERE body LIKE '%dying plugin%'") or [None])[0])
    released = until(lambda: (q("SELECT inflight_agent FROM messages WHERE body LIKE '%dying plugin%'")
                              or [{}])[0].get("inflight_agent") is None, seconds=30)
    check("the message is not left marked in flight", bool(released), str(msg))
    check("and it is not acknowledged either: nothing was silently dropped",
          msg and msg["acked_at_ms"] is None, str(msg))
    check("the agent itself is still alive",
          bool(q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (dslot,))),
          "a dead plugin must not kill the agent")
    check("the node says the agent falls back to MCP delivery",
          "MCP delivery" in nodelog(), nodelog()[-600:])

    print("\n=== a plugin that is not installed fails loudly, before the CLI starts ===")
    mslot = make_slot(sid, "quirkmissing", "Mo", task="never runs")
    http("POST", f"/v1/slots/{mslot}/spawn")
    said = until(lambda: "definitely-not-an-installed-plugin" in nodelog(), seconds=20)
    check("the node names the missing plugin", bool(said), nodelog()[-600:])
    check("and says how to fix it", "correct the descriptor" in nodelog(), nodelog()[-600:])
    check("no agent was left running for it",
          not q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (mslot,)),
          str(q("SELECT id,exited_at_ms FROM agents WHERE slot_id=?", (mslot,))))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL PLUGIN DRIVER CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
