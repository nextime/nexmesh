#!/usr/bin/env python3
"""Stand up a believable fleet on one machine and leave it running.

For screenshots, for a demonstration, and for anyone who wants to see what the
dashboard actually looks like before deciding whether to install anything.

Everything here is real: a hub, two node daemons, agents holding seats, a code
graph parsed from this repository. Nothing talks to a model and nothing spends
a credit -- the agents are MCP shims driven by this script, which is exactly
how the test suite drives them.

    python3 tests/demo-fleet.py          # then open the URL it prints

Ctrl-C stops it and removes the temporary directory it ran in.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
PORT = int(os.environ.get("DEMO_PORT", "17950"))
HUB = f"http://127.0.0.1:{PORT}"
USER, PASS = "demo", "demo-dashboard"

T = tempfile.mkdtemp(prefix="nexmesh-demo-")
DB = f"{T}/hub.db"
procs = []


def cleanup(*_):
    for p in procs:
        try: p.send_signal(signal.SIGTERM)
        except Exception: pass
    time.sleep(0.4)
    for p in procs:
        try: p.kill()
        except Exception: pass
    print(f"\nstopped. logs kept in {T}")
    sys.exit(0)


def req(path, token=None, method="GET", body=None):
    r = urllib.request.Request(
        HUB + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method)
    r.add_header("content-type", "application/json")
    if token:
        r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=25) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a],
                          capture_output=True, text=True)


def value(out, prefix):
    for line in out.splitlines():
        if line.startswith(prefix):
            return line.split()[-1]
    raise SystemExit(f"could not find {prefix!r} in:\n{out}")


def until(fn, seconds=30.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception:
            pass
        time.sleep(0.3)
    return None


class Agent:
    """An MCP shim holding a seat, driven straight from this script."""

    def __init__(self, sock, sid, name, role, cli, cwd):
        env = dict(os.environ, NEXMESH_SOCK=sock, NEXMESH_SESSION=sid,
                   NEXMESH_NAME=name, NEXMESH_ROLE=role, NEXMESH_CLI=cli,
                   RUST_LOG="error")
        self.name = name
        self.p = subprocess.Popen(
            [f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, env=env, text=True, bufsize=1, cwd=cwd)
        procs.append(self.p)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": name, "version": "1"}})

    def rpc(self, m, p=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": m}
        if p is not None:
            msg["params"] = p
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def tool(self, name, args=None):
        r = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        if "result" not in r:
            return f"ERR {r}"
        return r["result"]["content"][0]["text"]


def main():
    signal.signal(signal.SIGINT, cleanup)
    signal.signal(signal.SIGTERM, cleanup)

    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        raise SystemExit("build first: cargo build --workspace")

    print("minting credentials")
    optok = value(hub_cli("operator-token", "demo").stdout, "token:")
    tok_a = value(hub_cli("enrol", "machine-a").stdout, "token:")
    tok_b = value(hub_cli("enrol", "machine-b").stdout, "token:")
    setup_code = value(hub_cli("dashboard-setup-code").stdout, "setup code:")

    print("starting the hub")
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    if not until(lambda: req("/healthz")[1].get("ok")):
        raise SystemExit(f"the hub did not come up; see {T}/hub.log")

    s, j = req("/v1/auth/setup", method="POST", body={
        "setup_code": setup_code, "username": USER, "password": PASS})
    if s != 200:
        raise SystemExit(f"could not create the dashboard account: {s} {j}")

    print("starting two nodes")
    socks = {}
    for name, tok in (("machine-a", tok_a), ("machine-b", tok_b)):
        sock = f"{T}/{name}.sock"
        socks[name] = sock
        procs.append(subprocess.Popen(
            [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", name,
             "--clis", f"{ROOT}/clis", "--sock", sock, "--mcpd", f"{BIN}/nexmesh-mcpd"],
            stdout=open(f"{T}/{name}.log", "w"), stderr=subprocess.STDOUT,
            env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: len([n for n in req("/v1/nodes", optok)[1].get("nodes", [])
                       if n.get("connected")]) == 2)

    print("copying the source to parse (never the live checkout)")
    # A COPY, and a node_local workspace. Mounting the real checkout as a
    # `repo` workspace let an agent's signal_done publish it -- which committed
    # and pushed four "Max: finished work" commits to the actual remote before
    # anyone noticed. A demo must not be able to write to the repository it is
    # demonstrating.
    src = os.path.join(T, "workspace")
    os.makedirs(src, exist_ok=True)
    for item in ("crates", "tests", "clis", "capabilities", "Cargo.toml", "README.md"):
        s_path = os.path.join(ROOT, item)
        d_path = os.path.join(src, item)
        if os.path.isdir(s_path):
            shutil.copytree(s_path, d_path,
                            ignore=shutil.ignore_patterns("target", ".git", "__pycache__"))
        elif os.path.exists(s_path):
            shutil.copy2(s_path, d_path)

    print("creating a session")
    sid = req("/v1/sessions", optok, "POST", {
        "name": "payments-api",
        "workspaces": [{"name": "nexmesh", "kind": "node_local",
                        "node": "machine-a",
                        "mounts": {"machine-a": src}}],
        "plan": ["design the capability bridge", "implement the manifest loader",
                 "review", "release"],
    })[1]["session_id"]

    print("seating agents")
    nadia = Agent(socks["machine-a"], sid, "Nadia", "architect", "codex", src)
    max_ = Agent(socks["machine-a"], sid, "Max", "engineer", "claude", src)
    iris = Agent(socks["machine-b"], sid, "Iris", "code reviewer", "opencode", src)

    pt = value(hub_cli("participant-token", "bridge", "--session", sid).stdout, "token:")
    bridge = req("/v1/participant/join", pt, "POST", {
        "session": sid, "name": "Bridge", "role": "capability bridge",
        "kind_hint": "coderai"})[1]["agent_id"]

    print("giving them something to have been doing")
    nadia.tool("set_summary", {"summary": "drafting the manifest format"})
    nadia.tool("store_memory", {"key": "manifest-shape",
                                "value": "capabilities are discovered, never assumed: a listing "
                                         "endpoint, the service's own document, or a probe",
                                "category": "decision"})
    nadia.tool("send_message", {"to": "Max", "text":
                                "manifest is settled -- discovery has three kinds of evidence, "
                                "and a probe counts as one"})

    max_.tool("set_summary", {"summary": "implementing the manifest loader"})
    max_.tool("acquire_file", {"path": "crates/coderai/src/manifest.rs", "purpose": "editing"})
    max_.tool("store_memory", {"key": "route-probe",
                               "value": "GET on a POST-only route answering 405 proves it exists; "
                                        "404 proves it does not",
                               "category": "discovery"})
    max_.tool("send_message", {"to": "Iris", "text": "loader is ready for review"})
    max_.tool("signal_done", {"notes": "manifest loader implemented, 16 capabilities discovered"})

    iris.tool("set_summary", {"summary": "reviewing the manifest loader"})

    req("/v1/participant/tool", pt, "POST", {
        "agent_id": bridge, "tool": "send_message",
        "args": {"to": "Max", "text":
                 "offering ocr, transcription, embeddings and rerank on this session"}})
    req(f"/v1/sessions/{sid}/messages", optok, "POST",
        {"text": "team: ship the loader today if review passes"})

    print("parsing the code graph (tree-sitter over this repository, ~20s)")
    # Asked by an agent that sits on the machine holding the workspace. The
    # parse happens there; the hub only stores the result.
    out = max_.tool("query_code_graph", {"what": "overview", "workspace": "nexmesh"})
    print("  " + out.splitlines()[0] if out else "  (no graph)")

    time.sleep(1.0)
    print(f"""
  ready.

    {HUB}/
    username  {USER}
    password  {PASS}

    operator token  {optok}
      e.g.  NEXMESH_HUB={HUB} NEXMESH_TOKEN={optok} nexmesh peers

  Two nodes, four agents, one session awaiting review, and a code graph
  parsed from a copy of {ROOT}.

  Ctrl-C to stop.
""")
    # The external participant has no process of its own, so without a
    # heartbeat the hub rightly marks it lost after a while. A real bridge
    # polls for work; this stands in for that.
    while True:
        time.sleep(7)   # NODE_LEASE_MS is 20s; stay well inside it
        req("/v1/participant/tool", pt, "POST",
            {"agent_id": bridge, "tool": "list_peers", "args": {}})


if __name__ == "__main__":
    main()
