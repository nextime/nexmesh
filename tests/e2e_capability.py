#!/usr/bin/env python3
"""A team of unequal agents: models, capabilities, escalation and a budget.

The point of a mixed team is that the seats are not interchangeable. These
checks cover the machinery that makes that workable: knowing which model is
actually in a seat, addressing a teammate by what it can do rather than by
name, handing work to someone better suited, and stopping before the bill
runs away.
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
PORT = 17935; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-cap-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
for name in ("fakestream",):
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

def http(m, p, b=None, token=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
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

def slot(sid, name, role, caps=None, model=None, task="do the work"):
    body = {"cli_id": "fakestream", "mode": "headless", "display_name": name,
            "role": role, "workspace": "local", "task": task,
            "placement": {"node": "machine-b"}}
    if caps is not None: body["capabilities"] = caps
    if model is not None: body["model"] = model
    s, j = http("POST", f"/v1/sessions/{sid}/slots", body)
    assert "slot_id" in j, j
    return j["slot_id"]

PTOK = [None]
def tool(agent_id, name, args):
    """Call a team tool as a participant -- a teammate like any other.

    Team tools belong to whoever holds the seat, so they are exercised from a
    seat rather than from the operator API, which deliberately cannot call
    them.
    """
    return http("POST", "/v1/participant/tool",
                {"agent_id": agent_id, "tool": name, "args": args}, token=PTOK[0])

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
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    check("node online", bool(until(lambda: "fakestream" in
        (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))))

    sid = http("POST", "/v1/sessions", {"name": "captest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]})[1]["session_id"]

    print("=== a seat can be asked for a model, and reports what it got ===")
    strong = slot(sid, "Iris", "code reviewer", ["strong", "vision"], "big-expensive-1")
    cheap = slot(sid, "Max", "engineer", ["cheap"], "small-cheap-1")
    http("POST", f"/v1/slots/{strong}/spawn")
    http("POST", f"/v1/slots/{cheap}/spawn")
    row = until(lambda: (q("SELECT model FROM slots WHERE id=?", (strong,)) or [{}])[0].get("model"))
    check("the requested model is kept on the slot, so a respawn repeats it",
          row == "big-expensive-1", str(row))
    rep = until(lambda: (q("SELECT model FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                           (strong,)) or [{}])[0].get("model"), seconds=30)
    check("the agent reported the model it is running", bool(rep),
          open(f"{T}/node.log").read()[-400:])
    check("and it is the one that was asked for, so the flag really reached the CLI",
          rep == "big-expensive-1", str(rep))
    rep2 = until(lambda: (q("SELECT model FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                            (cheap,)) or [{}])[0].get("model"), seconds=30)
    check("each seat reports its own", rep2 == "small-cheap-1", str(rep2))

    print("\n=== a teammate can be found by what it can do ===")
    # Asked from a seat, because that is who asks in practice. A participant
    # is a teammate like any other, and needs no CLI to drive these.
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "participant-token",
        "hermes", "--session", sid], capture_output=True, text=True).stdout
    PTOK[0] = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    s, j = http("POST", "/v1/participant/join",
        {"session": sid, "name": "Hermes", "role": "archivist", "kind_hint": "test"},
        token=PTOK[0])
    assert "agent_id" in j, j
    ag = j["agent_id"]
    hslot = j["slot_id"]
    s, j = tool(ag, "list_peers", {})
    out = j.get("text", "")
    check("list_peers names the model in each seat", "model=big-expensive-1" in out, out)
    check("and what each one can do", "can=strong,vision" in out, out)
    s, j = tool(ag, "list_peers", {"capability": "VISION"})
    check("filtering by capability ignores case", "Iris" in j.get("text", ""), j.get("text"))
    s, j = tool(ag, "list_peers", {"capability": "nonesuch"})
    check("a capability nobody has says so plainly",
          "No teammate has the capability" in j.get("text", ""), j.get("text"))
    s, j = tool(ag, "send_message", {"to": "vision", "text": "look at this diagram"})
    check("a teammate can be addressed by a capability only one holds",
          j.get("ok") and "Iris" in j.get("text", ""), str(j))

    print("\n=== escalation: handing work to a better-suited seat ===")
    tool(ag, "set_summary", {"summary": "stuck on the grammar", "status": "in_progress"})
    s, j = tool(ag, "escalate", {"to": "strong", "reason": "this needs a stronger model"})
    check("the escalation is accepted", j.get("ok"), str(j))
    check("and names who took it", "Iris" in j.get("text", ""), j.get("text"))
    st = q("SELECT id,task_state FROM slots WHERE id IN (?,?)", (hslot, strong))
    states = {r["id"]: r["task_state"] for r in st}
    check("the escalating seat is marked escalated, so nobody waits on it",
          states.get(hslot) == "escalated", str(states))
    check("and the receiving seat is working", states.get(strong) == "working", str(states))
    msg = q("SELECT kind,body FROM messages WHERE to_slot=?", (strong,))
    check("the handoff carries the reason and the task",
          any("stronger model" in m["body"] and "stuck on the grammar" in m["body"] for m in msg),
          str(msg))
    check("and is a handoff, not an ordinary message",
          any(m["kind"] == "handoff" for m in msg), str(msg))
    s, j = tool(ag, "escalate", {"to": "Hermes", "reason": "loop"})
    check("escalating to yourself is refused", not j.get("ok"), str(j))

    print("\n=== a budget stops the session instead of the bill ===")
    bid = http("POST", "/v1/sessions", {"name": "budgeted", "token_budget": 10,
        "workspaces": [{"name": "local", "kind": "node_local", "node": "machine-b",
                        "mounts": {"machine-b": WS}}]})[1]["session_id"]
    check("a session can carry a budget",
          q("SELECT token_budget FROM sessions WHERE id=?", (bid,))[0]["token_budget"] == 10)
    bslot = slot(bid, "Spender", "engineer", [], None, "spend the budget")
    http("POST", f"/v1/slots/{bslot}/spawn")
    paused = until(lambda: (q("SELECT status,pause_reason FROM sessions WHERE id=?", (bid,))
                            or [{}])[0].get("status") == "paused", seconds=45)
    row = q("SELECT status,pause_reason FROM sessions WHERE id=?", (bid,))[0]
    check("reaching the budget pauses the session", bool(paused), str(row))
    check("and says why, with the numbers",
          row["pause_reason"] and "budget" in row["pause_reason"].lower()
          and "10" in row["pause_reason"], str(row))
    check("the agents are paused, not killed",
          bool(q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL", (bslot,))),
          "destroying work to save tokens is the wrong trade")
    check("it is recorded in the audit trail",
          bool(q("SELECT 1 FROM audit WHERE action='session_paused_on_budget'")),
          str(q("SELECT action FROM audit")))
    hit = q("SELECT budget_hit_at_ms FROM sessions WHERE id=?", (bid,))[0]
    check("the crossing is stamped, so release is not undone on the next token report",
          hit["budget_hit_at_ms"] is not None, str(hit))
    # `release` means "let an agent finish and stop it"; un-pausing is pause=false.
    http("POST", f"/v1/sessions/{bid}/pause", {"paused": False})
    back = until(lambda: (q("SELECT status FROM sessions WHERE id=?", (bid,))
                          or [{}])[0].get("status") == "active", seconds=20)
    check("an operator can resume it", bool(back),
          str(q("SELECT status FROM sessions WHERE id=?", (bid,))))
    check("and it is not re-paused on the next token report, having been overruled",
          q("SELECT budget_hit_at_ms FROM sessions WHERE id=?", (bid,))[0]["budget_hit_at_ms"]
          is not None,
          "the crossing stays stamped so release is not undone")

    print("\n=== raising the budget re-arms the guard ===")
    s, j = http("POST", f"/v1/sessions/{bid}/budget", {"token_budget": 5000})
    check("the budget can be raised", j.get("ok") and j.get("token_budget") == 5000, str(j))
    check("and it reports what has already been spent", j.get("spent", 0) > 0, str(j))
    check("the guard is armed again, not stuck on the first crossing",
          q("SELECT budget_hit_at_ms FROM sessions WHERE id=?", (bid,))[0]["budget_hit_at_ms"]
          is None, "otherwise raising a budget would have no effect")
    s, j = http("POST", f"/v1/sessions/{bid}/budget", {"token_budget": 0})
    check("a nonsense budget is refused", s == 400, f"{s} {j}")
    s, j = http("POST", f"/v1/sessions/{bid}/budget", {})
    check("and the ceiling can be removed altogether",
          j.get("ok") and q("SELECT token_budget FROM sessions WHERE id=?",
                            (bid,))[0]["token_budget"] is None, str(j))

    print("\n=== the unbudgeted session was never touched ===")
    check("no budget means no ceiling",
          q("SELECT status FROM sessions WHERE id=?", (sid,))[0]["status"] == "active",
          str(q("SELECT id,status FROM sessions")))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL CAPABILITY CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
