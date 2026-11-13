#!/usr/bin/env python3
"""Shared-repo workspaces: work crossing between machines.

Runs two nodes with separate checkouts of one repository and checks that what
one agent publishes, the other can pull -- and that a conflict is reported
without destroying anybody's work.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time
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
PORT = 17908; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-ws-")
DB = f"{T}/h.db"
ORIGIN = f"{T}/origin.git"
SOCK_A, SOCK_B = f"{T}/a.sock", f"{T}/b.sock"
WS_A, WS_B = f"{T}/checkout-a", f"{T}/checkout-b"
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
        with urllib.request.urlopen(r, timeout=120) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def until(fn, seconds=40.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

class Agent:
    """An agent joining through a particular node, working in that node's checkout."""
    def __init__(self, sock, cwd, sid, name, role):
        env = dict(os.environ, NEXMESH_SOCK=sock, NEXMESH_SESSION=sid, NEXMESH_NAME=name,
                   NEXMESH_ROLE=role, NEXMESH_CLI="claude", RUST_LOG="error")
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True,
            bufsize=1, cwd=cwd)
        procs.append(self.p); self.n = 0; self.cwd = cwd
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": name, "version": "1"}})
    def rpc(self, m, p=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": m}
        if p is not None: msg["params"] = p
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())
    def tool(self, name, args=None):
        r = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r: return f"RPC-ERROR {r['error']['message']}"
        return r["result"]["content"][0]["text"]
    def write(self, rel, text):
        os.makedirs(os.path.dirname(f"{self.cwd}/{rel}") or self.cwd, exist_ok=True)
        open(f"{self.cwd}/{rel}", "w").write(text)
    def read(self, rel):
        try: return open(f"{self.cwd}/{rel}").read()
        except OSError: return None

def main():
    subprocess.run(["git", "init", "--bare", "--initial-branch", "main", ORIGIN],
                   capture_output=True, check=True)

    toks = {}
    for name in ("machine-a", "machine-b"):
        out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", name],
                             capture_output=True, text=True).stdout
        toks[name] = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    for name, sock in (("machine-a", SOCK_A), ("machine-b", SOCK_B)):
        procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
            "--name", name, "--clis", f"{ROOT}/clis", "--sock", sock,
            "--mcpd", f"{BIN}/nexmesh-mcpd"],
            stdout=open(f"{T}/node-{name}.log","w"), stderr=subprocess.STDOUT,
            env=dict(os.environ, NEXMESH_TOKEN=toks[name], RUST_LOG="info", NO_COLOR="1")))
    both = until(lambda: len([n for n in (http("GET","/v1/nodes")[1].get("nodes") or [])
                              if n.get("connected")]) == 2)
    check("two machines are connected", bool(both), str(http("GET","/v1/nodes")[1]))

    print("\n=== a workspace that lives on both machines ===")
    s, j = http("POST", "/v1/sessions", {"name": "shared", "workspaces": [
        {"name": "app", "kind": "repo", "repo_url": ORIGIN, "repo_branch": "main",
         "mounts": {"machine-a": WS_A, "machine-b": WS_B}}]})
    sid = j["session_id"]
    check("session with a repo workspace created", s == 200, str(j))

    s, j = http("POST", f"/v1/sessions/{sid}/sync", {"op": "ensure"})
    check("both checkouts were prepared", s == 200
          and all(r.get("ok") for r in j["workspaces"][0]["results"]), str(j)[:400])
    check("each machine got its own checkout",
          os.path.isdir(f"{WS_A}/.git") and os.path.isdir(f"{WS_B}/.git"))

    print("\n=== work crosses from one machine to the other ===")
    max_ = Agent(SOCK_A, WS_A, sid, "Max", "engineer")
    iris = Agent(SOCK_B, WS_B, sid, "Iris", "code reviewer")

    max_.write("src/auth.rs", "fn login() {}\n")
    out = max_.tool("publish_work", {"summary": "add the login stub"})
    check("Max published", "published" in out, out[:250])
    check("Iris cannot see it yet", iris.read("src/auth.rs") is None,
          "the file appeared without a sync")
    out = iris.tool("sync_workspace")
    # A checkout with no commits of its own adopts the branch rather than
    # rebasing onto it; both are a successful pull.
    check("Iris pulled", any(w in out for w in ("updated", "file(s) changed",
                                                "took the current state")), out[:250])
    check("and now has Max's file", iris.read("src/auth.rs") == "fn login() {}\n",
          str(iris.read("src/auth.rs")))

    print("\n=== the team is told, so nobody has to guess ===")
    s, log = http("GET", f"/v1/sessions/{sid}/log?limit=20")
    check("publishing announced itself to the team",
          any("published work" in m["body"] for m in log.get("messages", [])),
          str([m["body"][:60] for m in log.get("messages", [])]))

    print("\n=== finishing publishes automatically ===")
    iris.write("tests/auth_test.rs", "#[test] fn works() {}\n")
    out = iris.tool("signal_done", {"notes": "review done"})
    check("signal_done also shared the work", "shared with the team" in out, out[:300])
    out = max_.tool("sync_workspace")
    check("Max pulled it without being told twice",
          max_.read("tests/auth_test.rs") == "#[test] fn works() {}\n", out[:200])

    print("\n=== a conflict is reported, and nothing is lost ===")
    max_.write("src/auth.rs", "fn login() { /* Max */ }\n")
    iris.write("src/auth.rs", "fn login() { /* Iris */ }\n")
    out = max_.tool("publish_work", {"summary": "Max edits login"})
    check("the first one through succeeds", "published" in out, out[:200])
    out = iris.tool("publish_work", {"summary": "Iris edits login"})
    check("the second is told it conflicts", "CONFLICT" in out, out[:300])
    check("it names the file", "auth.rs" in out, out[:300])
    check("and Iris's work is still on disk, unmangled",
          iris.read("src/auth.rs") == "fn login() { /* Iris */ }\n",
          str(iris.read("src/auth.rs")))

    print("\n=== status reports without changing anything ===")
    out = iris.tool("workspace_status")
    check("status mentions uncommitted work", "uncommitted" in out, out[:200])
    check("still uncommitted after asking",
          iris.read("src/auth.rs") == "fn login() { /* Iris */ }\n")

    print("\n=== a node-local workspace says so plainly ===")
    s, j = http("POST", "/v1/sessions", {"name": "localonly", "workspaces": [
        {"name": "here", "kind": "node_local", "node": "machine-a", "mounts": {"machine-a": f"{T}/local"}}]})
    os.makedirs(f"{T}/local", exist_ok=True)
    lsid = j["session_id"]
    solo = Agent(SOCK_A, f"{T}/local", lsid, "Solo", "engineer")
    out = solo.tool("publish_work")
    check("an agent not in a shared workspace is told why",
          "not in a shared workspace" in out, out[:250])

    print("\n=== the operator can drive it across both machines ===")
    s, j = http("POST", f"/v1/sessions/{sid}/sync", {"op": "status"})
    check("operator status covers both machines",
          s == 200 and len(j["workspaces"][0]["results"]) == 2, str(j)[:300])
    names = sorted(r["node"] for r in j["workspaces"][0]["results"])
    check("naming each one", names == ["machine-a", "machine-b"], str(names))
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "sync", sid, "--op", "status"],
                       capture_output=True, text=True, cwd=ROOT)
    check("`nexmesh sync` works too", r.returncode == 0 and "machine-a" in r.stdout,
          r.stdout + r.stderr)

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL WORKSPACE SYNC CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
