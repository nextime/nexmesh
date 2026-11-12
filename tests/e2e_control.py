#!/usr/bin/env python3
"""Pausing and releasing agents."""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17909; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-ctrl-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
d = open(f"{SCR}/fake-cli/fakestream.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/fakestream.toml", "w").write(d)
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
        with urllib.request.urlopen(r, timeout=30) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, a=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, a)]
    finally: c.close()

def until(fn, seconds=30.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

def acked(body):
    rows = q("SELECT acked_at_ms FROM messages WHERE body LIKE ?", (f"%{body}%",))
    return bool(rows) and rows[0]["acked_at_ms"] is not None

def main():
    tok = [l.split()[1] for l in subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
           capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-a", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("connected"))

    sid = http("POST", "/v1/sessions", {"name": "ctrl", "workspaces": [
        {"name": "w", "kind": "node_local", "node": "machine-a", "mounts": {"machine-a": WS}}]})[1]["session_id"]
    slots = {}
    for name in ("Ann", "Bo"):
        s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakestream",
            "mode": "headless", "display_name": name, "role": "engineer",
            "workspace": "w", "task": f"{name} works", "placement": {"node": "machine-a"}})
        slots[name] = j["slot_id"]
        http("POST", f"/v1/slots/{j['slot_id']}/spawn")
    running = until(lambda: len(q("SELECT 1 FROM agents WHERE exited_at_ms IS NULL")) == 2, 40)
    check("two driven agents are running", bool(running), str(q("SELECT id,status FROM agents")))

    print("\n=== baseline: messages get through ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Ann", "text": "baseline one"})
    check("a message is delivered normally", bool(until(lambda: acked("baseline one"), 25)),
          str(q("SELECT body,acked_at_ms FROM messages")))

    print("\n=== pausing one agent stops work reaching it ===")
    s, j = http("POST", f"/v1/sessions/{sid}/pause",
                {"paused": True, "who": "Ann", "reason": "operator is looking at something"})
    check("pause accepted", s == 200, str(j))
    check("the slot is marked paused",
          q("SELECT paused FROM slots WHERE id=?", (slots["Ann"],))[0]["paused"] == 1)
    check("the agent is told, and why",
          until(lambda: any("PAUSED by the operator" in m["body"] and "looking at something" in m["body"]
                            for m in q("SELECT body FROM messages"))),
          str([m["body"][:50] for m in q("SELECT body FROM messages")]))

    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Ann", "text": "while paused"})
    time.sleep(9)   # several delivery ticks
    check("work sent while paused is NOT delivered", not acked("while paused"),
          "it was delivered despite the pause")
    check("but it is kept, not dropped",
          bool(q("SELECT 1 FROM messages WHERE body LIKE '%while paused%'")))

    print("\n=== the other agent is unaffected ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Bo", "text": "bo still works"})
    check("Bo still receives work", bool(until(lambda: acked("bo still works"), 25)),
          "pausing one agent stopped another")

    print("\n=== resuming delivers what was held ===")
    s, j = http("POST", f"/v1/sessions/{sid}/pause", {"paused": False, "who": "Ann"})
    check("resume accepted", s == 200, str(j))
    check("the held message now arrives", bool(until(lambda: acked("while paused"), 25)),
          str(q("SELECT body,acked_at_ms FROM messages")))
    check("and the agent is told it may carry on",
          any("RESUMED" in m["body"] for m in q("SELECT body FROM messages")))

    print("\n=== pausing the whole team ===")
    s, j = http("POST", f"/v1/sessions/{sid}/pause", {"paused": True, "reason": "stand down"})
    check("team pause accepted", s == 200, str(j))
    check("the session itself is marked paused",
          q("SELECT status FROM sessions WHERE id=?", (sid,))[0]["status"] == "paused")
    check("every slot is paused",
          all(r["paused"] == 1 for r in q("SELECT paused FROM slots WHERE session_id=?", (sid,))))
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Bo", "text": "during team pause"})
    time.sleep(9)
    check("nobody receives work during a team pause", not acked("during team pause"))
    http("POST", f"/v1/sessions/{sid}/pause", {"paused": False})
    check("resuming the team delivers it", bool(until(lambda: acked("during team pause"), 25)))
    check("the session is active again",
          q("SELECT status FROM sessions WHERE id=?", (sid,))[0]["status"] == "active")

    print("\n=== releasing an agent lets it stop ===")
    before = q("SELECT restarts FROM slots WHERE id=?", (slots["Ann"],))[0]["restarts"]
    s, j = http("POST", f"/v1/sessions/{sid}/release",
                {"who": "Ann", "note": "nice work", "grace_ms": 3000})
    check("release accepted", s == 200, str(j))
    check("the slot reaches the end of the state machine",
          q("SELECT task_state FROM slots WHERE id=?", (slots["Ann"],))[0]["task_state"] == "released")
    check("the agent is told its work is accepted",
          any("RELEASED by the operator" in m["body"] and "nice work" in m["body"]
              for m in q("SELECT body FROM messages")),
          str([m["body"][:60] for m in q("SELECT body FROM messages")][-4:]))
    gone = until(lambda: q("SELECT exited_at_ms FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC",
                           (slots["Ann"],))[0]["exited_at_ms"] is not None, 30)
    check("its process actually stopped", bool(gone),
          str(q("SELECT id,status,exited_at_ms FROM agents WHERE slot_id=?", (slots["Ann"],))))

    print("\n=== and it is NOT brought back ===")
    time.sleep(10)   # several recovery ticks
    check("a released agent is not respawned",
          q("SELECT restarts FROM slots WHERE id=?", (slots["Ann"],))[0]["restarts"] == before,
          str(q("SELECT restarts,task_state FROM slots WHERE id=?", (slots["Ann"],))))
    check("no new agent appeared for that slot",
          not q("SELECT 1 FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (slots["Ann"],)))
    check("Bo is still going", bool(q("SELECT 1 FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                                      (slots["Bo"],))), "releasing one stopped the other")

    print("\n=== releasing the team ===")
    s, j = http("POST", f"/v1/sessions/{sid}/release", {"note": "we are done"})
    check("release-all accepted", s == 200, str(j))
    check("everyone is released",
          all(r["task_state"] == "released" for r in
              q("SELECT task_state FROM slots WHERE session_id=?", (sid,))),
          str(q("SELECT display_name,task_state FROM slots WHERE session_id=?", (sid,))))

    print("\n=== through the operator CLI too ===")
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "status", sid],
                       capture_output=True, text=True, cwd=ROOT)
    check("status shows the released state", r.returncode == 0 and "released" in r.stdout,
          r.stdout + r.stderr)
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "pause", sid, "--reason", "via cli"],
                       capture_output=True, text=True, cwd=ROOT)
    check("`nexmesh pause` works", r.returncode == 0, r.stdout + r.stderr)
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "release", sid, "Nobody"],
                       capture_output=True, text=True, cwd=ROOT)
    check("releasing an unknown agent is a clean error",
          r.returncode != 0 and "no agent matching" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-200:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL CONTROL CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
