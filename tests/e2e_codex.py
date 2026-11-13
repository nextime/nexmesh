#!/usr/bin/env python3
"""The Codex app_server driver, end to end.

Drives a fake `codex app-server` that speaks the real protocol, then (if the
real codex is installed) checks the handshake against the actual binary.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run FROM inside one. Inherited, it would hand every
# shim here the same identity and make them all claim a single seat -- which
# looks like a bug in the hub and is not. The tests choose their own.
import os as _os
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    _os.environ.pop(_v, None)


SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17899; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-codex-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
FAKELOG = f"{T}/fakecodex.log"
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
d = open(f"{SCR}/fake-cli/fakecodex.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/fakecodex.toml", "w").write(d)

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
        with urllib.request.urlopen(r, timeout=25) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, args=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, args)]
    finally: c.close()

def fakelog():
    try: return open(FAKELOG).read()
    except OSError: return ""

def until(fn, seconds=30.0, step=0.3):
    end = time.time() + seconds
    while time.time() < end:
        try: v = fn()
        except Exception: v = None
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
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 FAKECODEX_LOG=FAKELOG)))
    ok = until(lambda: "fakecodex" in (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))
    check("node online with the app-server test agent", bool(ok))

    sid = http("POST", "/v1/sessions", {"name": "codextest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b", "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakecodex",
        "mode": "headless", "display_name": "Cx", "role": "engineer",
        "workspace": "local", "task": "exercise the app-server driver",
        "placement": {"node": "machine-b"}})
    slot = j["slot_id"]
    check("slot created", s == 200, str(j))
    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("app-server agent spawned", s == 200, str(j))

    print("\n=== handshake, in the right order ===")
    lg = until(lambda: "call turn/start" in fakelog(), seconds=30) and fakelog()
    check("the driver got as far as the first turn", bool(lg), fakelog()[:300])
    calls = [l.split(" ", 1)[1] for l in fakelog().splitlines() if l.startswith("call ")]
    check("initialize came first", calls and calls[0] == "initialize", str(calls[:4]))
    check("then thread/start", "thread/start" in calls[:2], str(calls[:4]))
    check("then turn/start", "turn/start" in calls, str(calls[:4]))
    check("it identified itself as nexmesh",
          '"nexmesh"' in fakelog(), [l for l in fakelog().splitlines() if l.startswith("client")])
    check("the thread was started in the slot's workspace",
          f"cwd {WS}" in fakelog(), [l for l in fakelog().splitlines() if l.startswith("cwd")])
    check("approvals were configured for unattended running",
          "approvalPolicy never" in fakelog(), [l for l in fakelog().splitlines() if "approvalPolicy" in l])

    print("\n=== the prompt went through turn/start, not stdin ===")
    check("the first turn carried the agent's brief",
          "exercise the app-server driver" in fakelog(),
          [l for l in fakelog().splitlines() if l.startswith("turn_start_text")])

    print("\n=== thread id captured as the resume handle ===")
    rh = until(lambda: (q("SELECT resume_handle FROM slots WHERE id=?", (slot,)) or [{}])[0].get("resume_handle"))
    check("resume handle stored", bool(rh), str(q("SELECT resume_handle FROM slots WHERE id=?", (slot,))))

    print("\n=== token usage from thread/tokenUsage/updated ===")
    until(lambda: (q("SELECT input_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,)) or [{}])[0].get("input_tokens"))
    row = q("SELECT input_tokens,output_tokens,cache_read_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,))[0]
    check("input tokens recorded", row["input_tokens"] == 1200, str(row))
    check("output tokens recorded", row["output_tokens"] == 340, str(row))
    check("cached input recorded (codex calls it cachedInputTokens)",
          row["cache_read_tokens"] == 900, str(row))

    print("\n=== a blocking approval request was answered ===")
    ans = until(lambda: "approval_answer" in fakelog(), seconds=20)
    check("the driver answered execCommandApproval", bool(ans),
          fakelog()[-300:])
    check("with the decision vocabulary that method expects",
          '"approved"' in fakelog(), [l for l in fakelog().splitlines() if l.startswith("approval_answer")])

    print("\n=== a message is STEERED into the running turn ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Cx", "text": "steer me mid-turn"})
    steered = until(lambda: "steer_text" in fakelog(), seconds=30)
    elapsed = time.time() - t0
    check("turn/steer was used, not a new turn", bool(steered),
          [l for l in fakelog().splitlines() if "steer" in l or "turn_start" in l])
    check(f"it happened promptly ({elapsed:.1f}s)", bool(steered) and elapsed < 15, f"{elapsed:.1f}s")
    check("the steer named the turn that was running",
          any(l.startswith("steer_expected_turn turn_") for l in fakelog().splitlines()),
          [l for l in fakelog().splitlines() if l.startswith("steer_expected_turn")])
    check("the message text reached the agent",
          "steer me mid-turn" in fakelog(),
          [l for l in fakelog().splitlines() if l.startswith("steer_text")])
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%steer me mid-turn%'") or [{}])[0].get("acked_at_ms"))
    check("and it was marked delivered", bool(acked), str(q("SELECT body,acked_at_ms FROM messages")))

    print("\n=== the real binary agrees about the handshake ===")
    if not shutil.which("codex"):
        print("  SKIP  codex is not installed")
    else:
        r = subprocess.run([sys.executable, "-I", "-c", f'''
import json, subprocess, threading, time, os
p = subprocess.Popen(["codex","app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL, text=True, bufsize=1)
out=[]
threading.Thread(target=lambda: [out.append(l) for l in p.stdout], daemon=True).start()
def send(o): p.stdin.write(json.dumps(o)+"\\n"); p.stdin.flush()
send({{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"clientInfo":{{"name":"nexmesh","version":"0.1.0"}}}}}})
time.sleep(4)
send({{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{{"cwd":"{T}","approvalPolicy":"never"}}}})
time.sleep(6); p.kill()
ids, tid = set(), None
for l in out:
    try: d=json.loads(l)
    except Exception: continue
    if "result" in d: ids.add(d.get("id"))
    if d.get("id")==2 and "result" in d: tid=d["result"].get("thread",{{}}).get("id")
print(json.dumps({{"answered": sorted(x for x in ids if x), "thread_id": tid}}))
'''], capture_output=True, text=True, timeout=120)
        try:
            got = json.loads([l for l in r.stdout.splitlines() if l.startswith("{")][-1])
        except Exception:
            got = {}
        check("real codex answers initialize and thread/start",
              got.get("answered") == [1, 2], f"{got} {r.stderr[-200:]}")
        check("real codex returns a thread id at result.thread.id",
              bool(got.get("thread_id")), str(got))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL CODEX DRIVER CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
