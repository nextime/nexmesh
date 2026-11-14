#!/usr/bin/env python3
"""Milestone 2 end-to-end: hub <-> node <-> mcpd, plus an external participant.

Drives mcpd with raw MCP JSON-RPC instead of a real agent CLI, so the whole
message path is exercised without spending API credits.
"""
import shutil, json, os, signal, subprocess, sys, time, urllib.request, urllib.error

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
# A CLI exports its conversation id to its MCP children -- and this suite is
# very often run FROM inside such a CLI, which would hand every shim the same
# identity and make them all share one seat. The tests choose their own.
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
    """An MCP client speaking to nexmesh-mcpd over stdio, as a CLI would."""
    def __init__(self, env_extra, cwd=None):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="warn", **env_extra)
        # A human runs their CLI inside the workspace, so mcpd's cwd is how
        # the hub infers which workspace this agent is in.
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
            bufsize=1, cwd=cwd or WS_DIR)
        procs.append(self.p)
        self.n = 0
    def close(self):
        try: self.p.terminate(); self.p.wait(timeout=3)
        except Exception: pass

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
    # A node advertises only the CLIs whose binaries it can actually resolve,
    # so what it reports depends on the machine it runs on. Assert the
    # mechanism always, and the real names only where those binaries exist: a
    # CI runner has none of them, and a test that needs third-party tools
    # installed is a test that fails for the wrong reason.
    installed = [c for c in ("claude", "opencode", "codex", "qwen", "gemini")
                 if shutil.which(c)]
    if installed:
        check("node advertises its installed CLIs",
              all(c in clis for c in installed), f"want {installed}, got {clis}")
    else:
        check("node reports an inventory, empty on a machine with no agent CLI",
              isinstance(clis, list), str(clis))
        print("  SKIP  no agent CLI installed here, so there is nothing to advertise")

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

    print("\n=== 10b. a team message is not delivered because ONE seat took it ===")
    # The bug this pins cost an afternoon on the real fleet. "Delivered" was
    # one flag on the message, set by the FIRST ack -- and a capability
    # bridge, handed team chat it cannot answer, acks everything at once. So
    # the operator's question to a team of three read as delivered the moment
    # the bridge took it, while the only seats that could answer had never
    # been handed it. The fleet looked like it was ignoring her.
    s, j = http("POST", f"/v1/sessions/{sid}/messages", {"text": "who is awake in here?"})
    check("the send says which seats it is for and how it reaches them",
          isinstance(j.get("seats"), list) and len(j["seats"]) >= 2
          and all("route" in x for x in j["seats"]), str(j)[:300])

    def team_msg():
        s, j = http("GET", f"/v1/sessions/{sid}/log?limit=100")
        rows = [m for m in j.get("messages", []) if m["body"] == "who is awake in here?"]
        return rows[-1] if rows else {}

    m = team_msg()
    check("it is addressed to the whole team and counts an audience",
          m.get("to") == "(team)" and m.get("audience", 0) >= 2, str(m)[:300])
    check("nobody has seen it yet", m.get("seen") == 0 and m.get("delivered") is False,
          str(m)[:300])

    # The external participant reads and acks it -- the bridge's behaviour.
    s, pj = http("GET", f"/v1/participant/messages?agent_id={hermes}", token=ptok)
    ids = [x["id"] for x in pj.get("messages", []) if x["body"] == "who is awake in here?"]
    http("POST", "/v1/participant/ack", {"agent_id": hermes, "message_ids": ids}, token=ptok)
    m = team_msg()
    check("one seat acking does not make it delivered to the team",
          m.get("seen") == 1 and m.get("audience", 0) >= 2 and m.get("delivered") is False,
          str(m)[:300])

    # And when the CLI seat reads it too, the team really has heard it.
    t = max_.tool("check_team_status")
    check("the CLI seat still gets its own copy", "who is awake in here?" in t, t[:300])
    # And it is told who asked, and how to answer them. An agent told only to
    # "respond" answered a sibling seat instead, and the operator's question
    # read as ignored.
    check("the operator is named as the operator, not as a teammate",
          "from the OPERATOR" in t, t[:400])
    check("and the way back to them is spelled out", "broadcast" in t, t[:400])
    m = team_msg()
    check("once every seat has seen it, it is delivered",
          m.get("seen") == m.get("audience") and m.get("delivered") is True, str(m)[:300])

    print("\n=== 11. node death returns in-flight messages to the queue ===")
    s, j = http("POST", f"/v1/sessions/{sid}/messages", {"to": "Max", "text": "operator ping"})
    check("operator can message a slot", j.get("sent"), str(j))
    # Kill mcpd before it can ack, then confirm the message is not lost.
    max_.p.kill(); time.sleep(0.5)
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    check("session survives an agent dying", s == 200, str(s))

    print("\n=== an attached agent that comes back reuses its seat ===")
    # Run by hand, so run repeatedly: restarted, or probed by its own
    # tooling. A fresh seat each time fills a session with teammates that
    # never answer.
    before = len(http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", []))
    env = dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_CLI="handmade",
               NEXMESH_NAME="Handmade", NEXMESH_ROLE="tester",
               NEXMESH_SESSION=sid)
    def probe():
        p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             text=True, env=env, bufsize=1)
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                  "params": {"protocolVersion": "2024-11-05",
                                             "capabilities": {},
                                             "clientInfo": {"name": "probe", "version": "1"}}}) + "\n")
        p.stdin.flush()
        p.stdout.readline()
        p.stdin.close(); p.wait(timeout=20)
    probe()
    after_first = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("the first run takes a seat", len(after_first) == before + 1,
          f"{before} -> {len(after_first)}")
    # Its process is gone now, so the next run is the same agent returning.
    probe(); probe()
    seats = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("running it again reuses that seat rather than taking more",
          len(seats) == before + 1, f"expected {before + 1}, got {len(seats)}")
    check("and there is exactly one of it",
          sum(1 for a in seats if a.get("name") == "Handmade") == 1,
          str([a.get("name") for a in seats]))

    print("\n=== a seat follows the CLI's conversation, not the directory ===")
    # claude and opencode both keep several conversations per project, and
    # resuming one starts a NEW process with a new pid. So a seat cannot be
    # identified by directory, by pid, or by name alone. The CLI publishes
    # the conversation id -- CLAUDE_CODE_SESSION_ID, CODEX_THREAD_ID -- and
    # that is what a seat follows.
    def talk(conversation, name="Window"):
        e = dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_CLI="handmade",
                 NEXMESH_NAME=name, NEXMESH_ROLE="tester", NEXMESH_SESSION=sid,
                 NEXMESH_CLI_SESSION=conversation)
        p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             text=True, env=e, bufsize=1, cwd=WS_DIR)
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                  "params": {"protocolVersion": "2024-11-05",
                                             "capabilities": {},
                                             "clientInfo": {"name": name, "version": "1"}}}) + "\n")
        p.stdin.flush()
        p.stdout.readline()
        p.stdin.close(); p.wait(timeout=20)

    base = len(http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", []))
    talk("conv-aaa")
    one = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("a conversation takes a seat", len(one) == base + 1, f"{base} -> {len(one)}")

    # Same folder, same name, DIFFERENT conversation: a second window.
    talk("conv-bbb")
    two = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("a second conversation in the same directory gets its own seat",
          len(two) == base + 2, f"expected {base + 2}, got {len(two)}")

    # Resuming the first: same conversation id, new process.
    talk("conv-aaa"); talk("conv-aaa")
    three = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("resuming a conversation returns to its own seat, however often",
          len(three) == base + 2, f"expected {base + 2}, got {len(three)}")

    # And it can move: the same identity pointed at another session takes a
    # seat there, and stops occupying the one it left.
    s2, j2 = http("POST", "/v1/sessions", {"name": "elsewhere"})
    other = j2.get("session_id")
    check("a second session exists to move to", s2 == 200 and bool(other), str(j2))
    if other:
        e = dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_CLI="handmade",
                 NEXMESH_NAME="Window", NEXMESH_ROLE="tester",
                 NEXMESH_SESSION=other, NEXMESH_CLI_SESSION="conv-aaa")
        p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             text=True, env=e, bufsize=1, cwd=WS_DIR)
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                  "params": {"protocolVersion": "2024-11-05",
                                             "capabilities": {},
                                             "clientInfo": {"name": "Window", "version": "1"}}}) + "\n")
        p.stdin.flush(); p.stdout.readline(); p.stdin.close(); p.wait(timeout=20)
        moved = http("GET", f"/v1/sessions/{other}/status")[1].get("agents", [])
        check("the identity takes a seat in the session it moved to",
              any(a.get("name") == "Window" for a in moved), str(moved))
        left = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
        stale = [a for a in left
                 if a.get("name") == "Window" and a.get("state") not in (None, "exited")
                 and a.get("kind") not in (None, "-")]
        check("and the seat it left stops showing a live teammate",
              not stale, str(stale))

    # Every command takes a session NAME -- that is what the dashboard shows
    # and what a person types -- so the endpoints behind them have to accept
    # one. They used to match an id only, so `nexmesh status <name>` reported
    # "no agents in this session" for a session full of them: a wrong answer
    # that reads as news about the fleet rather than a misunderstood question.
    by_id = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    by_name = http("GET", "/v1/sessions/e2e/status")[1].get("agents", [])
    check("a session's status can be asked for by name, not just by id",
          by_name and len(by_name) == len(by_id),
          f"by id: {len(by_id)}, by name: {len(by_name)}")
    by_name_log = http("GET", "/v1/sessions/e2e/log")[1].get("messages", [])
    check("and so can its log",
          isinstance(by_name_log, list) and len(by_name_log) > 0, str(by_name_log)[:160])
    nosuch = http("GET", "/v1/sessions/no-such-session-at-all/status")[1].get("agents", [])
    check("while a name that matches nothing still returns nothing",
          nosuch == [], str(nosuch)[:160])

    print("\n=== a new conversation retires the dead seats it replaces ===")
    # `claude --continue` resumes a conversation but issues a NEW id, so the
    # hub cannot match it to the seat its predecessor held -- four
    # conversations came back from a restart as eight seats. The seat is NOT
    # handed over: from here a resumed conversation and a brand new one in
    # the same directory are indistinguishable, and guessing let a second
    # window steal the first's seat, name and inbox. Instead the dead ones
    # are retired, and anything still waiting for them is carried across,
    # because a message nobody claimed is work somebody still has to do.
    import os as _os
    ghost_dir = _os.path.join(WS_DIR, "ghost-town")
    _os.makedirs(ghost_dir, exist_ok=True)
    g1 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_NAME": "Ghosty", "NEXMESH_CLI_SESSION": "conv-ghost-1"}, cwd=ghost_dir)
    g1.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                           "clientInfo": {"name": "Ghosty", "version": "1"}})
    seats_before = [a for a in http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
                    if a.get("name") == "Ghosty"]
    check("the first conversation has a seat", len(seats_before) == 1, str(seats_before)[:200])
    first_ghost = seats_before[0]["slot_id"] if seats_before else None
    # Something is said to it that nobody claims.
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "for whoever works here", "to": "Ghosty"})
    g1.close() if hasattr(g1, "close") else g1.p.terminate()
    time.sleep(1.5)
    # It comes back with a different id, as a resumed conversation does.
    g2 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_NAME": "Ghosty-again", "NEXMESH_CLI_SESSION": "conv-ghost-2"}, cwd=ghost_dir)
    g2.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                           "clientInfo": {"name": "Ghosty", "version": "1"}})
    after = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    ghosts = [a for a in after if a.get("name", "").startswith("Ghost")]
    # BOTH seats exist: a seat outlives its process on purpose, which is what
    # crash recovery stands on, and the hub must not guess which dead seat
    # was a restart and which is a conversation that may yet come back.
    check("the new conversation gets its own seat, and the old one remains",
          len(ghosts) == 2, str([(a["name"], a["status"]) for a in ghosts]))
    check("and the new conversation kept its own identity",
          any(g["name"] == "Ghosty-again" and g["slot_id"] != first_ghost for g in ghosts),
          str(ghosts)[:200])
    # Retiring the empty one is the operator's call, and only takes seats a
    # live one has replaced in the same place.
    s_t, j_t = http("POST", f"/v1/sessions/{sid}/seats/tidy", {})
    check("tidying retires the superseded seat", s_t == 200 and j_t.get("retired") == 1,
          f"{s_t} {j_t}")
    left = [a for a in http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
            if a.get("name", "").startswith("Ghost")]
    check("leaving the live one", len(left) == 1 and left[0]["name"] == "Ghosty-again",
          str([(a["name"], a["status"]) for a in left]))
    s_t2, j_t2 = http("POST", f"/v1/sessions/{sid}/seats/tidy", {})
    check("and tidying again takes nothing", s_t2 == 200 and j_t2.get("retired") == 0,
          f"{s_t2} {j_t2}")
    # The unclaimed message followed the work, not the corpse.
    msgs = http("GET", f"/v1/sessions/{sid}/log")[1].get("messages", [])
    carried = [m for m in msgs if "for whoever works here" in (m.get("body") or "")]
    check("and what was waiting was carried over, not deleted", bool(carried), str(msgs)[:200])

    print("\n=== a nameless CLI comes back to the seat the last one finished in ===")
    # The morning-after case: you close `claude` and start another in the same
    # folder. It has no name to give and a brand-new conversation id, so
    # without this it took a fresh seat and left yesterday's -- the one every
    # command and every queued message addresses -- standing empty.
    quiet_dir = _os.path.join(WS_DIR, "quiet-corner")
    _os.makedirs(quiet_dir, exist_ok=True)
    q1 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_CLI_SESSION": "conv-quiet-1"}, cwd=quiet_dir)
    def seat_of(r):
        """The seat the hub just sat this shim in, from what it was told."""
        import re as _re
        text = json.dumps(r)
        m = _re.search(r"slot (sl-[0-9a-f]+)", text)
        return m.group(1) if m else None

    r1 = q1.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "q", "version": "1"}})
    first_seat = seat_of(r1)
    check("the first conversation took a seat", bool(first_seat), str(r1)[:200])
    rows = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    first_name = next((a["name"] for a in rows if a["slot_id"] == first_seat), None)

    # It ENDS -- the shim closes, which is an exit somebody reported.
    q1.close() if hasattr(q1, "close") else q1.p.terminate()
    time.sleep(2.0)
    q2 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_CLI_SESSION": "conv-quiet-2"}, cwd=quiet_dir)
    r2 = q2.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "q", "version": "1"}})
    check("the new conversation took the SAME seat, not a twin",
          seat_of(r2) == first_seat, f"{seat_of(r2)} vs {first_seat}")
    rows = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    mine = [a for a in rows if a["slot_id"] == first_seat]
    check("and kept the name the team addresses",
          len(mine) == 1 and mine[0]["name"] == first_name,
          f"{[(a['name'], a['slot_id']) for a in mine]} wanted {first_name}")
    check("and it is the only seat in that directory",
          len([a for a in rows if a.get("cli") == "handmade"
               and a["slot_id"] in (first_seat, seat_of(r2))]) == 1, str(mine)[:200])

    # But a NAMED conversation still gets its own seat: a name says which
    # seat it wants, and two named ones are deliberately two teammates.
    q3 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_NAME": "Deliberate", "NEXMESH_CLI_SESSION": "conv-quiet-3"},
              cwd=quiet_dir)
    r3 = q3.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "q", "version": "1"}})
    named = [a for a in http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
             if a.get("name") == "Deliberate"]
    check("a named conversation is still its own teammate",
          len(named) == 1 and named[0]["slot_id"] != first_seat, str(named)[:200])
    check("and it was told so", seat_of(r3) != first_seat, f"{seat_of(r3)} vs {first_seat}")

    # The case a deploy actually produces: a NODE restart empties the seat.
    # The agent keeps the word `disconnected` -- nobody reported an ending --
    # but gets an exit time, which is the hub saying there is no process left
    # to come back to it. Requiring the word `exited` left that seat standing
    # empty while the CLI that meant to be in it took a sibling, which is
    # what twenty agents in a fleet view are made of.
    import sqlite3 as _sq
    q4seat = seat_of(r2)
    q2.close() if hasattr(q2, "close") else q2.p.terminate()
    time.sleep(1.5)
    _c = _sq.connect(DB)
    _c.execute("UPDATE agents SET status = 'disconnected' WHERE slot_id = ? "
               "AND exited_at_ms IS NOT NULL", (q4seat,))
    _c.commit(); _c.close()
    q5 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_CLI_SESSION": "conv-quiet-5"}, cwd=quiet_dir)
    r5 = q5.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "q", "version": "1"}})
    check("a seat emptied by a node restart is reclaimed too",
          seat_of(r5) == q4seat, f"{seat_of(r5)} vs {q4seat}")

    # And a conversation that is merely out of touch keeps its seat: taking
    # it would hand one terminal's conversation to another.
    q4 = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_SESSION": sid,
               "NEXMESH_CLI_SESSION": "conv-quiet-4"}, cwd=quiet_dir)
    r4 = q4.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "q", "version": "1"}})
    check("a second LIVE conversation in the same folder gets its own seat",
          seat_of(r4) not in (None, q4seat), f"{seat_of(r4)} vs {q4seat}")

    print("\n=== an external participant comes back to its own seat ===")
    # A bridge or a hermes agent has no conversation id and no process the
    # hub can see, so its NAME is its identity. Restarting one used to mint a
    # fresh seat and leave a derelict one behind under the same name --
    # exactly the duplication that fills a session with teammates who never
    # answer, and it was fixed everywhere except here.
    s1, j1 = http("POST", "/v1/participant/join",
                  {"session": sid, "name": "Bridgey", "role": "bridge",
                   "capabilities": ["ocr"]}, token=ptok)
    first_slot = j1.get("slot_id")
    check("an external participant joins", s1 == 200 and bool(first_slot), f"{s1} {j1}")

    s2, j2 = http("POST", "/v1/participant/join",
                  {"session": sid, "name": "Bridgey", "role": "bridge",
                   "capabilities": ["ocr"]}, token=ptok)
    # This used to assert that a second LIVE instance got its own seat, on
    # the reasoning that a restart would find the old agent gone. That test
    # can never work: a service restarts in a second or two, and the lease
    # deciding whether the old agent is live runs for twenty, so a restart
    # always looked concurrent and every one left a derelict seat behind --
    # three CoderAI seats in one afternoon, each cleared by hand. Two
    # instances under one name are indistinguishable from a restart anyway.
    check("joining again under the same name takes the same seat",
          s2 == 200 and j2.get("slot_id") == first_slot,
          f"{j2.get('slot_id')} vs {first_slot}")
    seats_now = [a for a in http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
                 if a.get("name") == "Bridgey"]
    check("so one name is one seat, however often it restarts",
          len(seats_now) == 1, str([(a["name"], a["status"]) for a in seats_now]))
    s2b, j2b = http("POST", "/v1/participant/join",
                    {"session": sid, "name": "Bridgey-two", "role": "bridge",
                     "capabilities": ["ocr"]}, token=ptok)
    check("and an instance that wants its own seat asks for its own name",
          s2b == 200 and j2b.get("slot_id") != first_slot, f"{s2b} {j2b}")

    # Let both leases lapse, as they would when a bridge is stopped, then
    # rejoin: that must return to a seat rather than making a third.
    gone = False
    for _ in range(90):
        time.sleep(0.5)
        agents = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
        live = [a for a in agents
                if a.get("name") == "Bridgey" and a.get("status") == "running"]
        if not live:
            gone = True
            break
    check("their leases lapse once nothing renews them", gone, "still live after 45s")
    s3, j3 = http("POST", "/v1/participant/join",
                  {"session": sid, "name": "Bridgey", "role": "bridge",
                   "capabilities": ["ocr", "transcribe"]}, token=ptok)
    check("a restart returns to the seat it already had",
          s3 == 200 and j3.get("slot_id") == first_slot,
          f"{j3.get('slot_id')} vs {first_slot}")
    seats = [a for a in http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
             if a.get("name") == "Bridgey"]
    check("and no second seat is left behind", len(seats) == 1, str(len(seats)))

    print("\n=== a live agent can be moved between sessions without restarting ===")
    # Its session is not something the agent carries: a tool call resolves it
    # through the agent's slot. So moving the seat moves whoever is in it,
    # mid-conversation, with no restart.
    mover = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_NAME": "Mover",
                  "NEXMESH_ROLE": "tester", "NEXMESH_SESSION": sid,
                  "NEXMESH_CLI_SESSION": "conv-mover"})
    mover.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "Mover", "version": "1"}})
    mover.call("tools/call", {"name": "acquire_file",
                              "arguments": {"path": "moved.rs", "purpose": "editing"}})
    here = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
    check("it starts where it joined", any(a.get("name") == "Mover" for a in here),
          str([a.get("name") for a in here]))
    slot = next((a.get("slot_id") or a.get("slot") for a in here
                 if a.get("name") == "Mover"), None)
    check("its seat is identifiable", bool(slot), str(here))

    if slot and other:
        s3, j3 = http("POST", f"/v1/slots/{slot}/move", {"session": "elsewhere"})
        check("the move is accepted", s3 == 200, f"{s3} {j3}")
        check("and it reports the lock it had to let go of",
              j3.get("locks_released", 0) >= 1, str(j3))
        now_there = http("GET", f"/v1/sessions/{other}/status")[1].get("agents", [])
        check("the agent is in the new session",
              any(a.get("name") == "Mover" for a in now_there),
              str([a.get("name") for a in now_there]))
        gone = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
        check("and no longer in the old one",
              not any(a.get("name") == "Mover" for a in gone),
              str([a.get("name") for a in gone]))
        # The process never restarted: it should still be able to work, and
        # its work should land in the session it was moved to.
        r = mover.call("tools/call", {"name": "set_summary",
                                      "arguments": {"summary": "still here, elsewhere"}})
        check("the same process keeps working after the move", "result" in r, str(r)[:200])
        after = http("GET", f"/v1/sessions/{other}/status")[1].get("agents", [])
        check("and its work lands in the new session",
              any(a.get("name") == "Mover" and a.get("summary") == "still here, elsewhere"
                  for a in after),
              str(after)[:300])
        s4, j4 = http("POST", f"/v1/slots/{slot}/move", {"session": "nowhere-at-all"})
        check("moving to a session that does not exist is refused", s4 == 400, f"{s4} {j4}")

        # A move that does not survive a restart is not a move. With no
        # NEXMESH_SESSION to go on, the conversation must come back to the
        # seat it holds -- in the session it was moved INTO -- rather than
        # being resolved all over again from its directory or from "the only
        # active session". Before this, a restart quietly undid every move
        # and left the seat it had been moved into empty.
        again = Mcpd({"NEXMESH_CLI": "handmade", "NEXMESH_ROLE": "tester",
                      "NEXMESH_CLI_SESSION": "conv-mover"})
        again.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                  "clientInfo": {"name": "Mover", "version": "1"}})
        back = http("GET", f"/v1/sessions/{other}/status")[1].get("agents", [])
        check("a moved conversation comes back to the session it was moved into",
              any((a.get("slot_id") or a.get("slot")) == slot for a in back),
              str(back)[:300])
        not_here = http("GET", f"/v1/sessions/{sid}/status")[1].get("agents", [])
        check("and does not take a fresh seat in the session it left",
              not any(a.get("name") == "Mover" for a in not_here),
              str(not_here)[:300])

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
