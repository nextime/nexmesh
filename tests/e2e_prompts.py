#!/usr/bin/env python3
"""Permission prompts, answered when nobody is attached -- and only then.

A fleet seat is driven rather than watched. When its CLI stops to ask whether
it may run a tool, the agent is finished until something answers: the hub's
next message is not delivered either, because the node refuses to type at a
screen waiting for a choice. So with no viewer attached the node answers the
prompt itself, takes the NARROWEST affirmative, and records what it did.

With a viewer attached the choice is the person's, and the node must keep its
hands off it. Both halves are checked here against a real agent whose dialog
waits to be answered, so the keystroke it received is observable.
"""
import json, os, shutil, signal, socket, subprocess, sys, tempfile, time
import urllib.request, urllib.error
import websocket

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT = free()
HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-prompts-")
DB, SOCK, WS = f"{T}/h.db", f"{T}/n.sock", f"{T}/ws"
os.makedirs(WS)
CLIS = f"{T}/clis"
os.makedirs(CLIS)
LOG = f"{T}/typed.log"
_d = open(f"{SCR}/fake-cli/faketyped.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
# The dialog waits to be answered, and says what it was answered with.
_d += f'\n[cli.env]\nFAKETYPED_DIALOG_WAIT = "1"\nFAKETYPED_LOG = "{LOG}"\n'
open(f"{CLIS}/faketyped.toml", "w").write(_d)

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None, tok=None):
    r = urllib.request.Request(HUB + p, data=json.dumps(b).encode() if b is not None else None,
                               method=m)
    r.add_header("content-type", "application/json")
    if tok:
        r.add_header("authorization", "Bearer " + tok)
    try:
        with urllib.request.urlopen(r, timeout=25) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def fake_log():
    try:
        return open(LOG).read()
    except OSError:
        return ""


def system_notes(sid, tok):
    s, j = http("GET", f"/v1/sessions/{sid}/log?limit=200", tok=tok)
    return [m for m in (j.get("messages") or [])
            if "[nexmesh] answered a prompt" in str(m.get("body", ""))]


def main():
    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        print("FAILED (1): build first: cargo build --workspace")
        return 1
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "operator-token", "me"],
                         capture_output=True, text=True).stdout
    optok = [w for l in out.splitlines() for w in l.split() if len(w) > 20][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        try:
            if http("GET", "/healthz")[1].get("ok"):
                break
        except Exception:
            pass
        time.sleep(0.2)
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", "machine-b",
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes", tok=optok)
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "faketyped" in j["nodes"][0]["clis"]
            break
        time.sleep(0.25)
    check("node online with the dialog-raising test CLI", ok)

    sid = http("POST", "/v1/sessions", {"name": "prompts", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]}, tok=optok)[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketyped", "mode": "interactive", "display_name": "Ida",
        "role": "engineer", "workspace": "local", "task": "raise a dialog",
        "placement": {"node": "machine-b"}}, tok=optok)
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn", tok=optok)
    check("the interactive agent spawned", s == 200, str(j))
    aid = None
    for _ in range(80):
        s, j = http("GET", f"/v1/sessions/{sid}/resolve/Ida", tok=optok)
        if s == 200 and j.get("agent_id"):
            aid = j["agent_id"]
            break
        time.sleep(0.25)
    check("the agent is registered", aid is not None, str(j))
    if not aid:
        return 1
    for _ in range(60):
        s, j = http("GET", f"/v1/agents/{aid}/transcript?tail=4000", tok=optok)
        if "FAKETYPED READY" in (j.get("transcript") or ""):
            break
        time.sleep(0.25)

    # ---- unattended: the node answers it ---------------------------------
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "please dialog now", "to": "Ida"},
         tok=optok)
    answered = ""
    for _ in range(50):
        answered = fake_log()
        if "dialog answered with" in answered:
            break
        time.sleep(0.4)
    check("the dialog was answered with nobody attached",
          "dialog answered with" in answered, answered[-200:])
    check("and it was answered with the narrow affirmative, not 'always'",
          "dialog answered with '1'" in answered, answered[-200:])
    moved = ""
    for _ in range(30):
        s, j = http("GET", f"/v1/agents/{aid}/transcript?tail=8000", tok=optok)
        moved = j.get("transcript") or ""
        if "Edit applied" in moved:
            break
        time.sleep(0.3)
    check("so the agent carried on past it", "Edit applied" in moved, moved[-160:])
    check("the answer is in the agent's own transcript",
          "[nexmesh] answered a prompt" in moved, moved[-300:])
    notes = []
    for _ in range(30):
        notes = system_notes(sid, optok)
        if notes:
            break
        time.sleep(0.4)
    check("and in the session's chat, as a record", len(notes) >= 1,
          str(notes)[:200])
    if notes:
        check("the record says what was chosen and why",
              "1. Yes" in notes[0]["body"] and "nobody was attached" in notes[0]["body"],
              notes[0]["body"][:200])
        check("and it is pre-claimed, so no agent acts on it",
              notes[0].get("kind") == "system", str(notes[0].get("kind")))

    # ---- attached: the choice belongs to the person ------------------------
    before = fake_log().count("dialog answered with")
    url = (f"ws://127.0.0.1:{PORT}/v1/attach/ws?agent={aid}&cols=100&rows=30&replay=1024")
    ws = websocket.create_connection(url, timeout=20,
                                     subprotocols=["nexmesh.token." + optok])
    ws.settimeout(2)
    t0 = time.time()
    while time.time() - t0 < 2:
        try:
            ws.recv_data()
        except Exception:
            break
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "another dialog please", "to": "Ida"},
         tok=optok)
    time.sleep(9)
    check("with a viewer attached the node does not answer it",
          fake_log().count("dialog answered with") == before,
          f"{before} -> {fake_log().count('dialog answered with')}")
    # And once the viewer leaves, it is dealt with again.
    ws.close()
    after = before
    for _ in range(40):
        after = fake_log().count("dialog answered with")
        if after > before:
            break
        time.sleep(0.5)
    check("and answers it once the viewer detaches", after > before, f"{before} -> {after}")
    return 1 if fails else 0


rc = 1
try:
    rc = main()
finally:
    print(f"  (logs: {T})")
    for p in procs:
        try:
            p.send_signal(signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.4)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass

if fails:
    print(f"FAILED ({len(fails)}): " + "; ".join(fails))
    sys.exit(1)
if rc:
    sys.exit(rc)
print("ALL PROMPT CHECKS PASSED")
