#!/usr/bin/env python3
"""Authority lives on the SEAT, in the hub -- not in an MCP config.

What this proves, against a real hub and a real shim:

  - A seat starts as a plain agent, is not OFFERED the fleet tools, and is
    refused them if it calls one anyway from memory.
  - The operator grants authority through the API the dashboard uses.
  - An orchestrator can then elect another orchestrator in its own session,
    add, rename and remove seats there -- and is refused outside it.
  - It can never mint a superorchestrator, which is the escalation that
    would make the whole boundary decorative.

The flaw this replaces: `nexmesh-orch` holds an OPERATOR token, so authority
travelled with whichever MCP config named it. Installed user-scoped, every
CLI that user started was a fleet operator.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
_T = tempfile.mkdtemp(prefix="nexmesh-authority-")
PORT = 17893
HUB = f"http://127.0.0.1:{PORT}"
DB = f"{_T}/e2e.db"
SOCK = f"{_T}/node.sock"
WS = f"{_T}/workspace"
CLIS = f"{_T}/clis"
SHIMLOG = f"{_T}/shim.log"

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           # This seat's OWN identity: inherited when the suite is run
           # from inside a managed seat, and then every test's shim
           # registers as an agent the test's node has never heard of.
           "NEXMESH_AGENT_ID", "NEXMESH_SLOT", "NEXMESH_SESSION",
           "NEXMESH_CLI", "NEXMESH_ROLE", "NEXMESH_NAME", "NEXMESH_SOCK"):
    os.environ.pop(_v, None)

procs, fails = [], []

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
    req = urllib.request.Request(HUB + path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    req.add_header("content-type", "application/json")
    if token: req.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

class Mcpd:
    def __init__(self, env_extra):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="warn", **env_extra)
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
            bufsize=1, cwd=WS)
        procs.append(self.p)
        self.n = 0
    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None: msg["params"] = params
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("mcpd closed stdout: " + (self.p.stderr.read() or ""))
        return json.loads(line)
    def hello(self):
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "1"}})
        return self
    def tools(self):
        return [t["name"] for t in self.call("tools/list")["result"]["tools"]]
    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r: return f"RPC-ERROR: {r['error']['message']}"
        return r["result"]["content"][0]["text"]

class Door:
    """The hub's own MCP door. Re-initialising keeps the seat, which is what
    makes it the right place to watch the ADVERTISED list change."""
    def __init__(self, token, name):
        self.token, self.url, self.sid, self.n = token, f"/v1/mcp?name={name}", None, 0
    def rpc(self, method, params=None):
        self.n += 1
        body = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None: body["params"] = params
        req = urllib.request.Request(HUB + self.url, data=json.dumps(body).encode(),
                                     method="POST")
        req.add_header("content-type", "application/json")
        req.add_header("authorization", f"Bearer {self.token}")
        if self.sid: req.add_header("Mcp-Session-Id", self.sid)
        try:
            with urllib.request.urlopen(req, timeout=15) as r:
                sid = r.headers.get("Mcp-Session-Id") or r.headers.get("mcp-session-id")
                if sid: self.sid = sid
                return json.loads(r.read() or b"{}")
        except urllib.error.HTTPError as e:
            return json.loads(e.read() or b"{}")
    def hello(self):
        self.rpc("initialize", {"protocolVersion": "2025-03-26",
                                "clientInfo": {"name": "e2e", "version": "1"}})
        return self
    def tools(self):
        return [t["name"] for t in self.rpc("tools/list").get("result", {}).get("tools", [])]


def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            if http("GET", "/healthz")[0] == 200: return True
        except Exception: pass
        time.sleep(0.2)
    return False

def slot_of(sid, name):
    _, j = http("GET", f"/v1/sessions/{sid}/status")
    for a in j.get("agents", []):
        if a["name"] == name: return a
    return {}

def shim_tools(when_contains):
    """What a MANAGED shim was actually advertised, from the CLI's own log."""
    try:
        lines = open(SHIMLOG).read().splitlines()
    except OSError:
        return None
    for line in reversed(lines):
        if line.startswith("tools ") and when_contains in line:
            return set(line.split(": ", 1)[1].split(","))
    return None


def main():
    os.makedirs(WS, exist_ok=True)
    os.makedirs(CLIS, exist_ok=True)
    for name in os.listdir(f"{ROOT}/clis"):
        if name.endswith(".toml"):
            with open(f"{ROOT}/clis/{name}") as src, open(f"{CLIS}/{name}", "w") as dst:
                dst.write(src.read())
    # A CLI that runs a REAL MCP shim as its child and writes down the tool
    # list it was offered. That list is the thing under test: a model can
    # only call what its client named, so authority that does not reach the
    # list does not reach the model.
    with open(f"{CLIS}/shimtui.toml", "w") as f:
        f.write(f'''
[cli]
id = "shimtui"
display_name = "Shim TUI"
command = "fakeshim"
version_args = ["--version"]
extra_paths = ["{SCR}/fake-cli"]

[cli.env]
FAKESHIM_LOG = "{SHIMLOG}"
FAKESHIM_MCPD = "{BIN}/nexmesh-mcpd"

[modes.interactive]
args = []
attachable = true
typed_delivery = true
''')
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    if not wait_http():
        check("hub comes up", False); return
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", "machine-a",
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd",
         "--label", "machine-a"],
        stdout=open(f"{_T}/node.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and j["nodes"] and j["nodes"][0]["connected"]: break
        time.sleep(0.2)

    _, j = http("POST", "/v1/sessions", {"name": "alpha"})
    a_sid = j["session_id"]
    _, j = http("POST", "/v1/sessions", {"name": "beta"})
    b_sid = j["session_id"]

    print("\n=== a seat arrives with no authority over anything ===")
    boss = Mcpd({"NEXMESH_SESSION": a_sid, "NEXMESH_NAME": "Boss",
                 "NEXMESH_ROLE": "lead", "NEXMESH_CLI": "claude"}).hello()
    mate = Mcpd({"NEXMESH_SESSION": a_sid, "NEXMESH_NAME": "Mate",
                 "NEXMESH_ROLE": "dev", "NEXMESH_CLI": "claude"}).hello()
    far = Mcpd({"NEXMESH_SESSION": b_sid, "NEXMESH_NAME": "Far",
                "NEXMESH_ROLE": "dev", "NEXMESH_CLI": "claude"}).hello()
    offered = boss.tools()
    check("a plain agent is offered the team tools", "send_message" in offered)
    check("a plain agent is NOT offered the fleet tools",
          not any(t in offered for t in ("elect", "add_agent", "remove_agent")),
          str([t for t in offered if t in ("elect", "add_agent", "remove_agent")]))
    # A model that saw the tool before a demotion will call it from memory,
    # so not advertising it is a courtesy and the refusal is the boundary.
    said = boss.tool("elect", {"agent": "Mate", "authority": "orchestrator"})
    check("and is refused when it calls one anyway", "no authority" in said.lower()
          or "plain agent" in said.lower(), said)
    check("the seat it tried to elect is untouched",
          slot_of(a_sid, "Mate").get("authority") == "agent",
          str(slot_of(a_sid, "Mate")))

    print("\n=== the operator grants authority, as the dashboard does ===")
    bslot = slot_of(a_sid, "Boss")["slot_id"]
    check("every seat reads as a plain agent to start with",
          slot_of(a_sid, "Boss").get("authority") == "agent")
    s, j = http("POST", f"/v1/slots/{bslot}/authority", {"authority": "orchestrator"})
    check("the operator can set it over the API the dashboard uses",
          s == 200 and j.get("authority") == "orchestrator", str(j))
    check("the snapshot carries it for the UI to show",
          slot_of(a_sid, "Boss").get("authority_label") == "orchestrator",
          str(slot_of(a_sid, "Boss")))
    s, j = http("POST", f"/v1/slots/{bslot}/authority", {"authority": "wizard"})
    check("a level the hub does not know is refused, not read as 'agent'",
          s != 200 and slot_of(a_sid, "Boss").get("authority") == "orchestrator", str(j))

    print("\n=== an orchestrator runs its own team ===")
    # Enforcement re-reads the column on EVERY call, so the seat that was
    # just granted authority can use it on the connection it already has --
    # no reconnect, no restart.
    said = boss.tool("elect", {"agent": "Mate", "authority": "orchestrator"})
    check("it elects another orchestrator in its own session, on the same connection",
          slot_of(a_sid, "Mate").get("authority") == "orchestrator", said)

    said = boss.tool("elect", {"agent": "Mate", "authority": "super"})
    check("it can NEVER mint a superorchestrator",
          "cannot make a superorchestrator" in said
          and slot_of(a_sid, "Mate").get("authority") == "orchestrator", said)

    said = boss.tool("elect", {"agent": "Far", "authority": "orchestrator"})
    check("a seat on another team is out of reach, and reads as absent",
          "that you can reach" in said, said)
    check("and really is untouched", slot_of(b_sid, "Far").get("authority") == "agent",
          str(slot_of(b_sid, "Far")))

    said = boss.tool("list_seats")
    check("list_seats shows its own team only",
          "Mate" in said and "Far" not in said, said[:300])

    said = boss.tool("add_agent", {"name": "Nina", "cli": "claude", "role": "dev"})
    check("it adds a seat to its own team", bool(slot_of(a_sid, "Nina").get("slot_id")), said)
    check("and the new seat holds no authority of its own",
          slot_of(a_sid, "Nina").get("authority") == "agent", str(slot_of(a_sid, "Nina")))

    said = boss.tool("rename_agent", {"agent": "Nina", "name": "Nora"})
    check("it renames within its team", bool(slot_of(a_sid, "Nora").get("slot_id")), said)
    said = boss.tool("add_agent", {"name": "Zed", "session": "beta"})
    check("it cannot add to another team", "outside your reach" in said, said)
    said = boss.tool("move_agent", {"agent": "Nora", "session": "beta"})
    check("moving needs reach over both ends, so it is refused",
          "superorchestrator" in said, said)
    said = boss.tool("remove_agent", {"agent": "Nora"})
    check("it removes an empty seat from its own team",
          not slot_of(a_sid, "Nora").get("slot_id"), said)

    print("\n=== a superorchestrator reaches the fleet ===")
    http("POST", f"/v1/slots/{bslot}/authority", {"authority": "super"})
    said = boss.tool("list_seats")
    check("it sees every team", "Far" in said and "Mate" in said, said[:300])
    said = boss.tool("elect", {"agent": "Far", "authority": "orchestrator"})
    check("and elects across them",
          slot_of(b_sid, "Far").get("authority") == "orchestrator", said)
    # Back to an orchestrator, so the demotion path is exercised too.
    http("POST", f"/v1/slots/{bslot}/authority", {"authority": "agent"})
    said = boss.tool("list_seats")
    check("a demotion takes effect immediately, with no reconnect",
          "no authority" in said.lower() or "plain agent" in said.lower(), said)

    print("\n=== what a caller is OFFERED follows its authority ===")
    # Through the hub's own door, where a re-handshake keeps the seat. The
    # shim takes its list once at registration, so there the list lags the
    # grant until the CLI reconnects -- the tools work when CALLED either way,
    # which is what the section above proves.
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "participant-token", "ext",
                          "--session", a_sid], capture_output=True, text=True).stdout
    ptok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    d = Door(ptok, "Ext").hello()
    check("a plain participant is not offered the fleet tools",
          "elect" not in d.tools() and "send_message" in d.tools(), str(d.tools())[:200])
    eslot = slot_of(a_sid, "Ext").get("slot_id")
    http("POST", f"/v1/slots/{eslot}/authority", {"authority": "orchestrator"})
    check("and is offered them once it holds the authority",
          "elect" in Door(ptok, "Ext").hello().tools(),
          str(Door(ptok, "Ext").hello().tools())[:200])

    print("\n=== a MANAGED seat's shim is advertised by its authority ===")
    # The gap this closes. The hub's own MCP door had always built the list
    # per seat; the node hardcoded the plain-agent one for every shim it
    # started -- which is every seat on a fleet. So a superorchestrator was
    # super in the hub and, to its own model, a plain agent: the fleet tools
    # were never in the list its client was given, and a model cannot call a
    # tool it was not shown. It read as "nexmesh cannot send messages".
    plain_slot = http("POST", f"/v1/sessions/{a_sid}/slots", {
        "cli_id": "shimtui", "mode": "interactive", "display_name": "Plainshim",
        "placement": {"node": "machine-a"}})[1]["slot_id"]
    s, j = http("POST", f"/v1/slots/{plain_slot}/spawn", {})
    check("a plain managed seat spawns", s == 200, f"{s} {j}")
    got = None
    for _ in range(100):
        got = shim_tools("at startup")
        if got: break
        time.sleep(0.2)
    check("and its shim is offered the team tools", bool(got) and "send_message" in got,
          str(got)[:200])
    check("but NOT the fleet tools",
          bool(got) and not (got & {"elect", "list_seats", "move_agent"}),
          str(sorted(got & {"elect", "list_seats", "move_agent"}) if got else got))

    os.rename(SHIMLOG, SHIMLOG + ".plain")
    boss_slot = http("POST", f"/v1/sessions/{a_sid}/slots", {
        "cli_id": "shimtui", "mode": "interactive", "display_name": "Bossshim",
        "placement": {"node": "machine-a"}})[1]["slot_id"]
    # Granted BEFORE the spawn, which is the honest order: a client fixes its
    # tool list at startup, so this is the only moment the list can carry it.
    http("POST", f"/v1/slots/{boss_slot}/authority", {"authority": "super"})
    s, j = http("POST", f"/v1/slots/{boss_slot}/spawn", {})
    check("a seat holding authority spawns", s == 200, f"{s} {j}")
    got = None
    for _ in range(100):
        got = shim_tools("at startup")
        if got: break
        time.sleep(0.2)
    check("ITS shim IS offered the fleet tools",
          bool(got) and {"elect", "list_seats", "invite_agent", "move_agent",
                         "add_agent", "remove_agent", "rename_agent"} <= got,
          str(sorted(got)[:14]) if got else str(got))
    check("and keeps the team tools as well",
          bool(got) and {"send_message", "check_messages", "set_summary"} <= got,
          str(got)[:200])

    print("\n=== a superorchestrator can TALK to the fleet, not only reorganise it ===")
    # The other half of the same bug. `list_seats` showed it every team while
    # `send_message` searched its own room only, so the one seat that could
    # move another team's agents could not say a word to any of them.
    http("POST", f"/v1/slots/{bslot}/authority", {"authority": "super"})
    said = boss.tool("send_message", {"to": "Far", "text": "report in"})
    check("it can address a seat on another team", "Far" in said, said[:200])
    said = far.tool("check_messages")
    check("and the message actually ARRIVES there", "report in" in said, said[:300])
    # Resolving a name is not delivering a message: written into the SENDER'S
    # room it would be reported sent and read by nobody, because every
    # delivery query asks for the rooms the RECIPIENT answers in.
    import sqlite3 as _sq
    _c = _sq.connect(DB)
    room = _c.execute("SELECT session_id FROM messages WHERE body = 'report in'").fetchone()
    _c.close()
    check("filed in the recipient's room, not the sender's",
          bool(room) and room[0] == b_sid, f"{room} wanted {b_sid}")
    said = far.tool("reply", {"to": 1, "text": "two files left"})
    got = boss.tool("check_messages")
    check("and the answer comes back across the boundary",
          "two files left" in got, f"{said[:120]} / {got[:200]}")

    print("\n=== nothing else widened ===")
    http("POST", f"/v1/slots/{bslot}/authority", {"authority": "orchestrator"})
    said = boss.tool("send_message", {"to": "Far", "text": "nope"})
    check("an orchestrator still reaches ONE team",
          "no teammate" in said.lower(), said[:200])
    said = boss.tool("list_peers")
    # Past the pending-message banner: the answer from Far is sitting in this
    # seat's inbox and names it, which is not the roster saying it is a peer.
    roster = said.split("[END PENDING MESSAGES]")[-1]
    check("and does not see another team in list_peers",
          "Far" not in roster, roster[:300])

    print("\n=== the change is traceable to whoever made it ===")
    # Never "operator": not being able to tell which caller changed the fleet
    # is precisely what a shared operator token cost.
    import sqlite3
    con = sqlite3.connect(DB)
    con.row_factory = sqlite3.Row
    rows = [dict(r) for r in con.execute(
        "SELECT actor, detail FROM audit WHERE action = 'authority_changed'")]
    con.close()
    by_seat = [r for r in rows if str(r.get("actor", "")).startswith("seat ")]
    check("an election by a seat names THAT SEAT, never 'operator'",
          bool(by_seat), str(rows)[:400])
    check("and the operator's own changes are recorded as the operator",
          any(r.get("actor") == "operator" for r in rows), str(rows)[:400])

    for m in (boss, mate, far):
        try: m.p.terminate()
        except Exception: pass

if __name__ == "__main__":
    try:
        main()
    finally:
        cleanup()
    if fails:
        print(f"\nAUTHORITY FAILED ({len(fails)}): " + "; ".join(fails))
        sys.exit(1)
    print("\nALL AUTHORITY CHECKS PASSED")
