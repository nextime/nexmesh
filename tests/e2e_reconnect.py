#!/usr/bin/env python3
"""A node restart must not strand the CLIs attached to it.

The node dials the hub and reconnects with backoff. The shim between a CLI
and that node did not: a node restart -- an upgrade, a reboot -- left every
attached CLI talking to a dead socket for the rest of its conversation. The
seat read `disconnected` for ever and the only cure was restarting the CLI,
which is the one thing a long conversation cannot afford.

So: drive a shim through a tool call, kill the node under it, start a new
one on the same socket, and call another tool. It must answer, from the
same seat, without a second seat appearing.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
_T = tempfile.mkdtemp(prefix="nexmesh-reconnect-")
PORT = 17899
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

class Mcpd:
    def __init__(self, env_extra, cwd=None):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="info", **env_extra)
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=open(f"{_T}/mcpd.log", "w"), env=env,
            text=True, bufsize=1, cwd=cwd or WS_DIR)
        procs.append(self.p)
        self.n = 0
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

def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            s, _ = http("GET", "/healthz")
            if s == 200: return True
        except Exception: pass
        time.sleep(0.2)
    return False

def start_node(tok):
    p = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-a", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--label", "machine-a"],
        stdout=open(f"{_T}/node.log", "a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1"))
    procs.append(p)
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and any(n.get("connected") for n in j.get("nodes", [])):
            return p
        time.sleep(0.2)
    return p

def seats(sid):
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    return j.get("agents", [])

def main():
    os.makedirs(WS_DIR, exist_ok=True)
    print("\n=== 1. hub, node, one attached CLI ===")
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    hub = subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1"))
    procs.append(hub)
    check("hub answers /healthz", wait_http())
    node = start_node(tok)

    s, j = http("POST", "/v1/sessions", {"name": "reconnect",
        "workspaces": [{"name": "local", "kind": "node_local", "node": "machine-a",
                        "mounts": {"machine-a": WS_DIR}}]})
    sid = j.get("session_id")
    check("session created", s == 200 and bool(sid), str(j))

    cli = Mcpd({"NEXMESH_SESSION": sid, "NEXMESH_NAME": "Survivor",
                "NEXMESH_ROLE": "engineer", "NEXMESH_CLI": "claude",
                "NEXMESH_CLI_SESSION": "conversation-that-outlives-a-node"})
    r = cli.call("initialize", {"protocolVersion": "2024-11-05",
        "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
    check("the shim completes the MCP handshake",
          r.get("result", {}).get("serverInfo", {}).get("name") == "nexmesh", str(r)[:200])
    t = cli.tool("set_summary", {"summary": "before the restart"})
    check("a tool call works before the restart", "updated" in t.lower(), t)
    before = seats(sid)
    check("the CLI has exactly one seat", len(before) == 1, str(before)[:200])
    seat_id = before[0]["slot_id"]

    print("\n=== 2. the node dies and comes back ===")
    node.kill(); node.wait(timeout=5)
    time.sleep(0.5)
    start_node(tok)
    check("a replacement node is online", any(
        n.get("connected") for n in http("GET", "/v1/nodes")[1].get("nodes", [])))

    print("\n=== 3. the conversation carries on ===")
    t = cli.tool("set_summary", {"summary": "after the restart"})
    check("a tool call works after the node restart",
          "updated" in t.lower(), t)
    t = cli.tool("check_team_status")
    check("and keeps working", not t.startswith("RPC-ERROR"), t[:200])

    after = seats(sid)
    check("it is still one seat, not two", len(after) == 1, str(after)[:300])
    if after:
        now = after[0]["slot_id"]
        check("and it is the same seat", now == seat_id, f"{seat_id} -> {now}")
        check("the summary written after the restart reached the hub",
              after[0].get("summary") == "after the restart", str(after[0])[:300])

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        import traceback; traceback.print_exc(); fails.append(f"exception: {e}")
    finally:
        cleanup()
    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); sys.exit(1)
    print("ALL RECONNECT CHECKS PASSED")
