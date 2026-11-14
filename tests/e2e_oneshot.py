#!/usr/bin/env python3
"""A CLI that is a new process every turn still has one seat, and that seat
is not dead between turns.

Most CLIs are one long-lived process. Some are not: a cron-driven agent loop
runs the CLI once per turn -- it registers, takes its seat, says its piece and
exits, and a minute later another one does the same. The hub recorded every
one of those endings correctly and then reported the seat as `exited` for the
thirty seconds in between, so a teammate that was working perfectly well read
as a corpse on the dashboard, once a minute, for ever.

So: run a pinned shim, let it exit, run it again. The seat must be the same
seat both times, an unanswered message must survive the gap and reach the
second turn, and once the occupant has shown that it comes back the seat must
say `waiting` rather than `exited` -- while the agent row itself still says
`exited`, because the process really did end.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
_T = tempfile.mkdtemp(prefix="nexmesh-oneshot-")
PORT = 17903
HUB_HTTP = f"http://127.0.0.1:{PORT}"
DB = f"{_T}/e2e.db"
SOCK = f"{_T}/node.sock"
WS_DIR = f"{_T}/workspace"
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

procs = []
fails = []

def cleanup():
    print(f"  (logs: {_T})")
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

def http(method, path, body=None, token=None):
    url = HUB_HTTP + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("content-type", "application/json")
    if token: req.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

class Turn:
    """One turn: a process that registers, works, and goes."""
    def __init__(self, sid, cwd=None):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="info",
                   NEXMESH_SESSION=sid, NEXMESH_CLI="hermes",
                   NEXMESH_NAME="", NEXMESH_ROLE="archivist",
                   NEXMESH_CLI_SESSION="one-conversation-many-processes")
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=open(f"{_T}/mcpd.log", "a"), env=env,
            text=True, bufsize=1, cwd=cwd or WS_DIR)
        procs.append(self.p)
        self.n = 0
        self.call("initialize", {"protocolVersion": "2024-11-05",
            "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None: msg["params"] = params
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("mcpd closed stdout")
        return json.loads(line)
    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r: return f"RPC-ERROR: {r['error']['message']}"
        return r["result"]["content"][0]["text"]
    def end(self):
        """The turn is over: the process goes, and so does its shim."""
        try:
            self.p.stdin.close()
        except Exception: pass
        try:
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()

def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            s, _ = http("GET", "/healthz")
            if s == 200: return True
        except Exception: pass
        time.sleep(0.2)
    return False

def seats(sid):
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    return j.get("agents", [])

def wait_word(sid, want, timeout=8):
    """Wait for the seat's word, which travels CLI -> node -> hub."""
    for _ in range(int(timeout * 5)):
        rows = seats(sid)
        if rows and rows[0].get("status") == want:
            return rows[0]
        time.sleep(0.2)
    return (seats(sid) or [{}])[0]

def main():
    os.makedirs(WS_DIR, exist_ok=True)
    print("\n=== 1. a hub, a node, and a session for a one-shot CLI ===")
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    hub = subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1"))
    procs.append(hub)
    check("hub answers /healthz", wait_http())
    node = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-a", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--label", "machine-a"],
        stdout=open(f"{_T}/node.log", "a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1"))
    procs.append(node)
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and any(n.get("connected") for n in j.get("nodes", [])):
            break
        time.sleep(0.2)

    s, j = http("POST", "/v1/sessions", {"name": "loop",
        "workspaces": [{"name": "local", "kind": "node_local", "node": "machine-a",
                        "mounts": {"machine-a": WS_DIR}}]})
    sid = j.get("session_id")
    check("session created", s == 200 and bool(sid), str(j))

    print("\n=== 2. the first turn, and its ending ===")
    first = Turn(sid)
    t = first.tool("set_summary", {"summary": "turn one"})
    check("the first turn can call a tool", "updated" in t.lower(), t)
    rows = seats(sid)
    check("it took exactly one seat", len(rows) == 1, str(rows)[:200])
    seat = rows[0]["slot_id"]
    first.end()
    row = wait_word(sid, "exited")
    # Nothing yet says this occupant recurs: one process that ended is a
    # terminal somebody closed until proven otherwise.
    check("one turn and no second reads `exited`", row.get("status") == "exited",
          str(row)[:300])

    print("\n=== 3. something to say to it while nobody is there ===")
    s, j = http("POST", f"/v1/sessions/{sid}/messages",
                {"to": seat, "text": "while you were away"})
    check("the hub takes a message for an empty seat", s == 200, str(j))

    print("\n=== 4. the next turn: same seat, and the message is there ===")
    time.sleep(1.2)  # turns are apart in time; so are their rows
    second = Turn(sid)
    rows = seats(sid)
    check("still one seat, not a twin beside it", len(rows) == 1, str(rows)[:300])
    check("and it is the same seat", rows and rows[0]["slot_id"] == seat,
          f"{rows[0]['slot_id'] if rows else '-'} vs {seat}")
    got = second.tool("check_messages")
    check("the message waited for this turn", "while you were away" in got, got[:300])
    second.end()

    print("\n=== 5. between turns, the seat is waiting -- not dead ===")
    row = wait_word(sid, "waiting")
    check("an occupant that comes back leaves the seat `waiting`",
          row.get("status") == "waiting", str(row)[:300])
    check("the process itself is still recorded as exited",
          row.get("agent_status") == "exited", str(row)[:300])
    check("and no process is claimed for it", row.get("exited") is True, str(row)[:300])

    s, j = http("GET", f"/v1/dashboard/state?session={sid}")
    agents = (j.get("detail") or {}).get("agents") or []
    check("the dashboard says the same thing",
          len(agents) == 1 and agents[0].get("status") == "waiting", str(agents)[:300])

    print("\n=== 6. a third turn still comes home ===")
    time.sleep(1.2)
    third = Turn(sid)
    rows = seats(sid)
    check("three turns, one seat", len(rows) == 1 and rows[0]["slot_id"] == seat,
          str(rows)[:300])
    check("and it is running while the turn runs", rows[0].get("status") == "running",
          str(rows)[:300])
    third.end()

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL ONE-SHOT SEAT CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
