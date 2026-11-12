#!/usr/bin/env python3
"""Milestone 4: driver fast path, token accounting, resume and recovery.

Uses a fake agent that speaks Claude Code's real stream-json protocol, so the
driver is exercised against the shapes it will actually meet -- without
spending API credits.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17897; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-driver-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
for name in ("fakestream", "fakecrash"):
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
        v = fn()
        if v: return v
        time.sleep(step)
    return None

def main():
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok") if _try() else None)
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = until(lambda: "fakestream" in (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))
    check("node online with the stream-json test agent", bool(ok))

    sid = http("POST", "/v1/sessions", {"name": "drivertest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b", "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakestream",
        "mode": "headless", "display_name": "Max", "role": "engineer",
        "workspace": "local", "task": "exercise the driver", "placement": {"node": "machine-b"}})
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("driven agent spawned", s == 200, str(j))

    print("\n=== the session stays alive (stdin is not closed) ===")
    agent = until(lambda: (q("SELECT id,push_capable FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (slot,)) or [None])[0])
    check("the agent is running, not one-shot", agent is not None, "it exited immediately")
    check("the hub knows it can be pushed to",
          agent and agent["push_capable"] == 1, str(agent))

    print("\n=== the resume handle was captured from system/init ===")
    rh = until(lambda: (q("SELECT resume_handle FROM slots WHERE id=?", (slot,)) or [{}])[0].get("resume_handle"))
    check("resume handle stored", bool(rh), str(q("SELECT resume_handle FROM slots WHERE id=?", (slot,))))

    print("\n=== token accounting ===")
    tu = until(lambda: (q("SELECT input_tokens,output_tokens,cache_read_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,)) or [{}])[0].get("input_tokens"))
    row = q("SELECT input_tokens,output_tokens,cache_read_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,))[0]
    check("input tokens metered from assistant events", row["input_tokens"] > 0, str(row))
    check("cache reads metered", row["cache_read_tokens"] > 0, str(row))
    check("output tokens taken from the turn result", row["output_tokens"] > 0, str(row))

    print("\n=== a message is PUSHED mid-turn, not left for a tool call ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Max", "text": "ping from the operator"})
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%ping from the operator%'") or [{}])[0].get("acked_at_ms"), seconds=30)
    elapsed = time.time() - t0
    check("the message was delivered and acknowledged", bool(acked), str(q("SELECT * FROM messages")))
    check(f"delivery took under 15s without any tool call ({elapsed:.1f}s)", bool(acked) and elapsed < 15, f"{elapsed:.1f}s")
    s, j = http("GET", f"/v1/sessions/{sid}/resolve/Max")
    tr = http("GET", f"/v1/agents/{j['agent_id']}/transcript?tail=65536")[1].get("transcript", "")
    check("the agent actually saw the PENDING MESSAGES block",
          "PENDING MESSAGES" in tr and "ping from the operator" in tr, tr[-300:])

    print("\n=== an acked message is not pushed again ===")
    before = len([l for l in tr.splitlines() if "ping from the operator" in l])
    time.sleep(7)
    tr2 = http("GET", f"/v1/agents/{j['agent_id']}/transcript?tail=200000")[1].get("transcript", "")
    after = len([l for l in tr2.splitlines() if "ping from the operator" in l])
    check("no duplicate injection across ticks", after == before, f"{before} -> {after}")

    print("\n=== a crash is recovered with a handoff and a resume ===")
    restarts_before = q("SELECT restarts FROM slots WHERE id=?", (slot,))[0]["restarts"]
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Max", "text": "please CRASH now"})
    grew = until(lambda: q("SELECT restarts FROM slots WHERE id=?", (slot,))[0]["restarts"] > restarts_before, seconds=40)
    check("the crash was noticed and the slot restarted", bool(grew),
          str(q("SELECT restarts,status,task_state FROM slots WHERE id=?", (slot,))))
    newagent = until(lambda: (q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (slot,)) or [None])[0], seconds=40)
    check("a fresh agent is running for that slot", newagent is not None)
    nodelog = open(f"{T}/node.log").read()
    check("the replacement was launched with --resume", "--resume" in nodelog, nodelog[-400:])
    check("it resumed the SAME session id, not a fresh one",
          rh in nodelog, f"expected {rh} in the relaunch args")
    if newagent:
        tr3 = until(lambda: http("GET", f"/v1/agents/{newagent['id']}/transcript?tail=65536")[1].get("transcript") or None, seconds=20) or ""
        check("the agent confirms it resumed", "resumed=True" in tr3, tr3[:400])
        check("the handoff prompt told it it was restarted",
              "restarted" in tr3 or "resum" in tr3.lower(), tr3[:400])

    print("\n=== flap detection stops a hopeless slot ===")
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakecrash",
        "mode": "headless", "display_name": "Doomed", "role": "engineer",
        "workspace": "local", "task": "die", "placement": {"node": "machine-b"}})
    doomed = j["slot_id"]
    http("POST", f"/v1/slots/{doomed}/spawn")
    flapped = until(lambda: q("SELECT status,restarts FROM slots WHERE id=?", (doomed,))[0]["status"] == "flapping", seconds=70)
    row = q("SELECT status,restarts FROM slots WHERE id=?", (doomed,))[0]
    check("a repeatedly crashing slot is marked flapping", bool(flapped), str(row))
    check("it gave up after a bounded number of tries", row["restarts"] <= 5, str(row))
    msgs = q("SELECT body FROM messages WHERE kind='system' AND body LIKE '%crashed%'")
    check("the team is told the slot was abandoned", bool(msgs), str(msgs))
    n_before = row["restarts"]
    time.sleep(8)
    check("it is not restarted again after flagging",
          q("SELECT restarts FROM slots WHERE id=?", (doomed,))[0]["restarts"] == n_before,
          str(q("SELECT restarts FROM slots WHERE id=?", (doomed,))))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL DRIVER CHECKS PASSED"); return 0

def _try():
    try: return True
    except Exception: return False

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
