#!/usr/bin/env python3
"""Spin up a team through the orchestrator MCP server.

Drives `nexmesh-orch` with raw MCP JSON-RPC exactly as any MCP-capable CLI
would, so the thing being tested is the same path Claude Code, opencode, qwen
or Codex would take.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17902; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-orch-")
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

def http(m, p, b=None, tok=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if tok: r.add_header("authorization", f"Bearer {tok}")
    try:
        with urllib.request.urlopen(r, timeout=25) as x: return x.status, json.loads(x.read() or b"{}")
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

class Orch:
    """An MCP client talking to nexmesh-orch, as any CLI would."""
    def __init__(self, token=None):
        env = dict(os.environ, NEXMESH_HUB_HTTP=HUB, RUST_LOG="warn")
        env.pop("NEXMESH_OPERATOR_TOKEN", None)
        if token: env["NEXMESH_OPERATOR_TOKEN"] = token
        self.p = subprocess.Popen([f"{BIN}/nexmesh-orch"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True, bufsize=1)
        procs.append(self.p); self.n = 0
    def rpc(self, m, p=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": m}
        if p is not None: msg["params"] = p
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line: raise RuntimeError("nexmesh-orch closed stdout")
        return json.loads(line)
    def tool(self, name, args=None):
        r = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r: return f"RPC-ERROR {r['error']['message']}"
        return r["result"]["content"][0]["text"]

def main():
    optok = [l.split()[1] for l in subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "operator-token", "orch"],
        capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    tok = [l.split()[1] for l in subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
        capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: (http("GET","/v1/nodes",tok=optok)[1].get("nodes") or [{}])[0].get("connected"))

    print("=== it is a plain MCP server, so any CLI can load it ===")
    o = Orch(optok)
    r = o.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                             "clientInfo": {"name": "any-cli", "version": "1"}})
    check("MCP handshake",
          r.get("result", {}).get("serverInfo", {}).get("name") == "nexmesh-orchestrator", str(r)[:200])
    check("it explains itself to the model",
          "list_nodes" in r["result"].get("instructions", ""), str(r["result"].get("instructions"))[:160])
    names = [t["name"] for t in o.rpc("tools/list")["result"]["tools"]]
    for t in ("list_nodes", "create_team", "team_status", "read_log", "message_team",
              "adopt_terminal", "read_transcript"):
        check(f"tools/list offers {t}", t in names, str(names))

    print("\n=== it can see the fleet ===")
    out = o.tool("list_nodes")
    check("list_nodes finds the node and its CLIs",
          "machine-b" in out and "fakestream" in out, out[:300])

    print("\n=== one call spins up a whole team ===")
    out = o.tool("create_team", {
        "name": "login feature",
        "workspace": {"name": "app", "kind": "node_local", "node": "machine-b", "path": WS},
        "plan": ["design", "implement", "review"],
        "agents": [
            {"name": "Max", "role": "engineer", "cli": "fakestream",
             "task": "Implement the login flow.", "node": "machine-b", "owns": ["src/**"]},
            {"name": "Iris", "role": "code reviewer", "cli": "fakestream",
             "task": "Review Max's work.", "node": "machine-b"},
        ]})
    check("create_team reports a session", "Created session" in out, out[:300])
    check("both agents were started", out.count("Started on machine-b") == 2, out[:400])
    sid = q("SELECT id FROM sessions")[0]["id"]
    slots = q("SELECT display_name, role FROM slots WHERE session_id=?", (sid,))
    check("two slots exist with the right names",
          sorted(s["display_name"] for s in slots) == ["Iris", "Max"], str(slots))
    check("the plan was recorded", len(q("SELECT 1 FROM plan_items WHERE session_id=?", (sid,))) == 3)
    check("the workspace was created with its mount",
          bool(q("SELECT 1 FROM workspace_mounts")), str(q("SELECT * FROM workspaces")))
    check("ownership zones were applied", bool(q("SELECT 1 FROM ownership")), "owns was ignored")
    live = until(lambda: len(q("SELECT 1 FROM agents WHERE exited_at_ms IS NULL")) == 2)
    check("both agents are actually running", bool(live),
          str(q("SELECT id,status FROM agents")))

    print("\n=== and it can watch them ===")
    out = o.tool("team_status")
    check("team_status lists both, with no session id needed",
          "Max" in out and "Iris" in out, out[:300])
    check("it says where each one runs", out.count("on machine-b") == 2, out[:300])

    o.tool("message_team", {"to": "Max", "text": "focus on the refresh path"})
    out = until(lambda: ("focus on the refresh path" in o.tool("read_log")) or None, seconds=20)
    check("message_team reaches the log", bool(out), o.tool("read_log")[:300])
    check("the log marks delivery state", "!" in o.tool("read_log") or "~" in o.tool("read_log")
          or "( ! =" in o.tool("read_log"), o.tool("read_log")[:200])
    delivered = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%refresh path%'")
                               or [{}])[0].get("acked_at_ms"), seconds=25)
    check("and the driver actually delivered it", bool(delivered),
          str(q("SELECT body,acked_at_ms FROM messages")))

    out = o.tool("read_transcript", {"who": "Max"})
    check("read_transcript returns the agent's own output",
          "fakestream" in out or "session=" in out, out[:200])

    print("\n=== errors come back as errors, not silence ===")
    out = o.tool("create_team", {"name": "bad", "agents": [
        {"name": "Ghost", "cli": "nosuchcli", "task": "nothing"}]})
    check("an unknown CLI is reported per-agent",
          "nosuchcli" in out or "FAILED" in out or "NOT started" in out, out[:300])
    out = o.tool("read_transcript", {"who": "Nobody"})
    check("an unknown agent is a clean error", "ERROR" in out, out[:200])
    out = o.tool("not_a_tool")
    check("an unknown tool is rejected", "unknown tool" in out.lower() or "ERROR" in out, out[:200])

    print("\n=== without an operator token it says so plainly ===")
    o2 = Orch(None)
    o2.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                          "clientInfo": {"name": "x", "version": "1"}})
    out = o2.tool("list_nodes")
    check("an unauthenticated orchestrator explains the problem",
          "no operator token" in out.lower(), out[:260])
    check("and gives advice the model can act on, not an HTTP header",
          "NEXMESH_OPERATOR_TOKEN" in out and "Authorization" not in out, out[:260])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL ORCHESTRATOR CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
