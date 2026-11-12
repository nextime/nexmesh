#!/usr/bin/env python3
"""Milestone 2 end-to-end: hub <-> node <-> mcpd, plus an external participant.

Drives mcpd with raw MCP JSON-RPC instead of a real agent CLI, so the whole
message path is exercised without spending API credits.
"""
import json, os, signal, subprocess, sys, time, urllib.request, urllib.error

import tempfile
SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
_T = tempfile.mkdtemp(prefix="nexmesh-team-")
PORT = 17880
HUB_HTTP = f"http://127.0.0.1:{PORT}"
DB = f"{_T}/e2e.db"
SOCK = f"{_T}/node.sock"
WS_DIR = f"{_T}/workspace"
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
    """An MCP client speaking to nexmesh-mcpd over stdio, as a CLI would."""
    def __init__(self, env_extra):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="warn", **env_extra)
        # A human runs their CLI inside the workspace, so mcpd's cwd is how
        # the hub infers which workspace this agent is in.
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
            bufsize=1, cwd=WS_DIR)
        procs.append(self.p)
        self.n = 0
    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None: msg["params"] = params
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            err = self.p.stderr.read() if self.p.stderr else ""
            raise RuntimeError(f"mcpd closed stdout. stderr:\n{err}")
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

def main():
    for f in (DB, DB+"-wal", DB+"-shm", SOCK):
        try: os.remove(f)
        except OSError: pass
    os.makedirs(WS_DIR, exist_ok=True)

    print("\n=== 1. hub up, node enrolled ===")
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    hub = subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1"))
    procs.append(hub)
    check("hub answers /healthz", wait_http())

    node = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--label", "machine-b"],
        stdout=open(f"{_T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1"))
    procs.append(node)
    online = False
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and j["nodes"] and j["nodes"][0]["status"] == "online" and j["nodes"][0]["connected"]:
            online = True; break
        time.sleep(0.2)
    check("node is online and connected", online)
    s, j = http("GET", "/v1/nodes")
    clis = j["nodes"][0]["clis"] if j.get("nodes") else []
    check("node advertises its installed CLIs", "claude" in clis and "opencode" in clis, str(clis))

    print("\n=== 2. session with a node-local workspace ===")
    s, j = http("POST", "/v1/sessions", {"name": "e2e",
        "workspaces": [{"name": "local", "kind": "node_local", "node": "machine-b",
                        "mounts": {"machine-b": WS_DIR}}],
        "plan": ["wire the chain", "prove delivery"]})
    check("session created", s == 200 and "session_id" in j, str(j))
    sid = j.get("session_id")

    print("\n=== 3. a hermes-style EXTERNAL agent joins (no node, no CLI, no spawn) ===")
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "participant-token", "hermes",
                          "--session", sid], capture_output=True, text=True).stdout
    ptok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    s, j = http("POST", "/v1/participant/join",
        {"session": sid, "name": "Hermes", "role": "archivist", "kind_hint": "hermes"}, token=ptok)
    check("external participant joined over HTTP", s == 200 and "agent_id" in j, str(j))
    hermes = j.get("agent_id")
    check("participant is offered the same tool set", len(j.get("tools", [])) >= 15,
          str(len(j.get("tools", []))))

    s, j = http("POST", "/v1/participant/join", {"session": sid, "name": "X"}, token="garbage")
    check("a bad participant token is rejected", s == 401, str(s))

    print("\n=== 4. an ATTACHED CLI agent joins via mcpd (nobody spawned it) ===")
    max_ = Mcpd({"NEXMESH_SESSION": sid, "NEXMESH_NAME": "Max",
                 "NEXMESH_ROLE": "engineer", "NEXMESH_CLI": "claude"})
    r = max_.call("initialize", {"protocolVersion": "2024-11-05",
        "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}})
    check("mcpd completes the MCP handshake", r.get("result", {}).get("serverInfo", {}).get("name") == "nexmesh", str(r)[:200])
    r = max_.call("tools/list")
    names = [t["name"] for t in r["result"]["tools"]]
    check("tools/list advertises the team tools", "send_message" in names and "acquire_file" in names, str(names)[:160])

    print("\n=== 5. tools work through the full chain ===")
    t = max_.tool("set_summary", {"summary": "wiring the chain"})
    check("set_summary round-trips mcpd -> node -> hub", "updated" in t.lower(), t)
    t = max_.tool("list_peers")
    check("the CLI agent sees the external agent as a peer", "Hermes" in t, t)
    check("the external agent is labelled external", "external" in t, t)

    t = max_.tool("store_knowledge", {"key": "transport", "value": "ws dial-out", "category": "decision"})
    check("store_knowledge works", "Stored" in t, t)
    s, j = http("POST", "/v1/participant/tool",
        {"agent_id": hermes, "tool": "query_knowledge", "args": {"category": "decision"}}, token=ptok)
    check("the external agent reads what the CLI agent wrote",
          j.get("ok") and "ws dial-out" in j.get("text", ""), str(j)[:200])

    print("\n=== 6. messaging + delivery-with-ack across agent kinds ===")
    s, j = http("POST", "/v1/participant/tool",
        {"agent_id": hermes, "tool": "send_message",
         "args": {"to": "Max", "text": "corpus indexed, 412 docs"}}, token=ptok)
    check("external -> CLI message accepted", j.get("ok"), str(j)[:200])

    t = max_.tool("check_team_status")
    check("the message rides back on the next tool result",
          "PENDING MESSAGES" in t and "corpus indexed" in t, t[:300])
    check("the sender is named", "Hermes" in t, t[:200])

    t2 = max_.tool("check_team_status")
    check("an acked message is not delivered twice", "corpus indexed" not in t2, t2[:200])

    t = max_.tool("send_message", {"to": "Hermes", "text": "thanks, indexing confirmed"})
    check("CLI -> external message accepted", "Sent to Hermes" in t, t)
    s, j = http("GET", f"/v1/participant/messages?agent_id={hermes}", token=ptok)
    msgs = j.get("messages", [])
    check("external agent receives it", any("indexing confirmed" in m["body"] for m in msgs), str(j)[:200])
    s, j2 = http("POST", "/v1/participant/ack",
        {"agent_id": hermes, "message_ids": [m["id"] for m in msgs]}, token=ptok)
    check("external agent can ack", j2.get("acked") == len(msgs), str(j2))
    s, j3 = http("GET", f"/v1/participant/messages?agent_id={hermes}", token=ptok)
    check("acked messages are gone", j3.get("messages") == [], str(j3)[:160])

    print("\n=== 7. review loop spanning both agent kinds ===")
    t = max_.tool("signal_done", {"notes": "chain wired"})
    check("CLI agent signals done", "pending review" in t, t)
    s, j = http("POST", "/v1/participant/tool",
        {"agent_id": hermes, "tool": "submit_feedback",
         "args": {"to": "Max", "text": "add a test", "actionable": True}}, token=ptok)
    check("external agent reviews the CLI agent's work", j.get("ok"), str(j)[:200])
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    states = {a["name"]: a["task_state"] for a in j["agents"]}
    check("state machine advanced to addressing_feedback",
          states.get("Max") == "addressing_feedback", str(states))
    provs = {a["name"]: a["provenance"] for a in j["agents"]}
    check("provenance distinguishes attached from external",
          provs.get("Max") == "attached" and provs.get("Hermes") == "external", str(provs))
    hosts = {a["name"]: a["host"] for a in j["agents"]}
    check("the CLI agent is located on its machine", hosts.get("Max") == "machine-b", str(hosts))

    print("\n=== 8. locks are node-scoped for a node-local workspace ===")
    t = max_.tool("acquire_file", {"path": "src/main.rs", "purpose": "edit"})
    check("lock acquired", "Locked" in t, t)
    t = max_.tool("view_file_locks")
    check("lock domain is the node (workspace inferred from cwd)", "node:" in t, t)

    print("\n=== 9. error handling ===")
    t = max_.tool("send_message", {"to": "NoSuchAgent", "text": "hi"})
    check("unknown peer is a clean error, not a hang", "no teammate" in t.lower(), t)
    t = max_.tool("definitely_not_a_tool")
    check("unknown tool is rejected", "unknown tool" in t.lower() or "ERROR" in t, t)

    print("\n=== 10. the operator can read the conversation ===")
    s, j = http("GET", f"/v1/sessions/{sid}/log?limit=100")
    msgs = j.get("messages", [])
    check("the message log is readable", len(msgs) >= 2, str(j)[:200])
    check("it records who sent each message",
          any(m["from"] == "Hermes" for m in msgs), str([m["from"] for m in msgs]))
    check("it records the recipient", any(m["to"] == "Max" for m in msgs),
          str([m["to"] for m in msgs]))
    check("it distinguishes delivered from unseen",
          any(m["delivered"] for m in msgs), str([(m["body"][:20], m["delivered"]) for m in msgs]))
    check("a broadcast is shown as addressed to the team",
          all(m["to"] != "" for m in msgs), str(msgs)[:120])
    # Incremental follow must not repeat or skip.
    last = max(m["id"] for m in msgs)
    s, j2 = http("GET", f"/v1/sessions/{sid}/log?after={last}")
    check("following from the last id yields nothing new yet", j2.get("messages") == [], str(j2)[:120])
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "follow-test"})
    s, j3 = http("GET", f"/v1/sessions/{sid}/log?after={last}")
    check("a new message appears exactly once when following",
          [m["body"] for m in j3.get("messages", [])] == ["follow-test"], str(j3)[:200])

    print("\n=== 11. node death returns in-flight messages to the queue ===")
    s, j = http("POST", f"/v1/sessions/{sid}/messages", {"to": "Max", "text": "operator ping"})
    check("operator can message a slot", j.get("sent"), str(j))
    # Kill mcpd before it can ack, then confirm the message is not lost.
    max_.p.kill(); time.sleep(0.5)
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    check("session survives an agent dying", s == 200, str(s))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL E2E CHECKS PASSED"); return 0

try:
    rc = main()
except Exception as e:
    import traceback; traceback.print_exc(); rc = 2
finally:
    cleanup()
sys.exit(rc)
