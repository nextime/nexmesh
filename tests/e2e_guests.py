#!/usr/bin/env python3
"""A seat that serves more than one team.

A session is the isolation boundary. Some seats are SERVICES rather than
teammates, though -- a capability bridge offering OCR and transcription, a
reviewer every team sends work to -- and one copy per team means one process,
one model and one set of warm context per team for a single job.

What this proves against a real hub and real shims:

  - A service seat invited into another team becomes addressable there BY
    NAME, and a message sent to it there actually arrives.
  - It does not move: its own team, and its home, are unchanged.
  - Its answer lands in the room it was asked in, not in its own.
  - Teams it was NOT invited into still cannot see or address it.
  - A name the invited room already uses is refused, because a name is only
    unique within a team and is how everything addresses a seat.
  - Taking it out again stops delivery and does not leave messages pending
    for ever.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
_T = tempfile.mkdtemp(prefix="nexmesh-guests-")
PORT = 17894
HUB = f"http://127.0.0.1:{PORT}"
DB = f"{_T}/e2e.db"
SOCK = f"{_T}/node.sock"
WS = f"{_T}/workspace"

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

def http(method, path, body=None):
    req = urllib.request.Request(HUB + path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

class Mcpd:
    def __init__(self, **env_extra):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, RUST_LOG="warn", **env_extra)
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, text=True,
            bufsize=1, cwd=WS)
        procs.append(self.p)
        self.n = 0
        self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                 "clientInfo": {"name": "e2e", "version": "1"}})
    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None: msg["params"] = params
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("mcpd closed stdout: " + (self.p.stderr.read() or ""))
        return json.loads(line)
    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        if "error" in r: return f"RPC-ERROR: {r['error']['message']}"
        return r["result"]["content"][0]["text"]

def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            if http("GET", "/healthz")[0] == 200: return True
        except Exception: pass
        time.sleep(0.2)
    return False

def seat(sid, name):
    _, j = http("GET", f"/v1/sessions/{sid}/status")
    for a in j.get("agents", []):
        if a["name"] == name: return a
    return {}

def main():
    os.makedirs(WS, exist_ok=True)
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
         "--clis", f"{ROOT}/clis", "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd",
         "--label", "machine-a"],
        stdout=open(f"{_T}/node.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and j["nodes"] and j["nodes"][0]["connected"]: break
        time.sleep(0.2)

    tools_sid = http("POST", "/v1/sessions", {"name": "tools"})[1]["session_id"]
    app_sid = http("POST", "/v1/sessions", {"name": "app"})[1]["session_id"]
    far_sid = http("POST", "/v1/sessions", {"name": "far"})[1]["session_id"]

    print("\n=== a service seat, and a team that needs it ===")
    cora = Mcpd(NEXMESH_SESSION=tools_sid, NEXMESH_NAME="Cora",
                NEXMESH_ROLE="services", NEXMESH_CLI="claude")
    max_ = Mcpd(NEXMESH_SESSION=app_sid, NEXMESH_NAME="Max",
                NEXMESH_ROLE="engineer", NEXMESH_CLI="claude")
    zoe = Mcpd(NEXMESH_SESSION=far_sid, NEXMESH_NAME="Zoe",
               NEXMESH_ROLE="engineer", NEXMESH_CLI="claude")
    check("the service seat is not a peer of the team that needs it",
          "Cora" not in max_.tool("list_peers"), max_.tool("list_peers")[:200])
    said = max_.tool("send_message", {"to": "Cora", "text": "ocr this please"})
    check("and cannot be addressed from there at all", "no teammate matching" in said, said)

    print("\n=== the operator invites it in ===")
    s, j = http("POST", f"/v1/sessions/{app_sid}/guests", {"agent": "Cora"})
    check("the invitation is accepted", s == 200 and j.get("ok"), str(j))
    check("and it says what that means",
          "answers in app as well as its own team" in j.get("said", ""), str(j))
    check("it did NOT move: its home is still its own team",
          seat(tools_sid, "Cora").get("visiting") is False, str(seat(tools_sid, "Cora")))
    guest = seat(app_sid, "Cora")
    check("it is on the invited team's roster", bool(guest.get("slot_id")), str(guest))
    check("flagged as a visitor, with the team it lives in named",
          guest.get("visiting") is True and guest.get("home") == "tools", str(guest))

    print("\n=== and it answers there ===")
    check("it is now a peer of that team", "Cora" in max_.tool("list_peers"))
    said = max_.tool("send_message", {"to": "Cora", "text": "ocr this please"})
    check("a message can be addressed to it by name", "Sent to" in said, said)
    got = cora.tool("check_messages")
    check("and the message actually arrives at the service seat",
          "ocr this please" in got, got[:300])

    # Its answer belongs in the room it was asked in, not in its own: that
    # team never asked for it, and the asking team would never see it.
    cora.tool("claim_messages")
    said = cora.tool("send_message", {"to": "Max", "text": "here is the text"})
    check("the service seat can answer back", "Sent to" in said, said)
    got = max_.tool("check_messages")
    check("the answer reaches the asker", "here is the text" in got, got[:300])
    _, j = http("GET", f"/v1/sessions/{app_sid}/log?limit=50")
    bodies = [m.get("body", "") for m in j.get("messages", [])]
    check("and it is in the asking team's chat, where it was asked",
          any("here is the text" in b for b in bodies), str(bodies)[:300])
    _, j = http("GET", f"/v1/sessions/{tools_sid}/log?limit=50")
    bodies = [m.get("body", "") for m in j.get("messages", [])]
    check("not in the service seat's own team, which has nothing to do with it",
          not any("here is the text" in b for b in bodies), str(bodies)[:300])

    print("\n=== it hears its OWN team's announcements, not every room's ===")
    # A capability bridge invited into seven rooms would otherwise read seven
    # rooms' traffic to do a job it is asked for by name.
    cora.tool("claim_messages")
    http("POST", f"/v1/sessions/{app_sid}/messages", {"text": "APP STANDUP IN FIVE"})
    time.sleep(1.5)
    got = cora.tool("check_messages")
    check("a visited team's broadcast is not pushed at the guest",
          "APP STANDUP" not in got, got[:300])
    _, j = http("GET", f"/v1/sessions/{app_sid}/log?limit=20")
    check("though it is in that team's chat for its own members",
          any("APP STANDUP" in m.get("body", "") for m in j.get("messages", [])), str(j)[:200])
    http("POST", f"/v1/sessions/{tools_sid}/messages", {"text": "TOOLS STANDUP IN FIVE"})
    heard = ""
    for _ in range(20):
        heard = cora.tool("check_messages")
        if "TOOLS STANDUP" in heard: break
        time.sleep(0.4)
    check("its own team's broadcast still reaches it", "TOOLS STANDUP" in heard, heard[:300])
    # And being addressed in a visited room still works, which is the point.
    cora.tool("claim_messages")
    max_.tool("send_message", {"to": "Cora", "text": "and this one by name"})
    heard = ""
    for _ in range(20):
        heard = cora.tool("check_messages")
        if "by name" in heard: break
        time.sleep(0.4)
    check("but being addressed by name in a visited room does",
          "by name" in heard, heard[:300])
    cora.tool("claim_messages")

    print("\n=== a team it was not invited into still cannot see it ===")
    check("not a peer there", "Cora" not in zoe.tool("list_peers"),
          zoe.tool("list_peers")[:200])
    said = zoe.tool("send_message", {"to": "Cora", "text": "me too"})
    check("and not addressable there", "no teammate matching" in said, said)

    print("\n=== the rules that keep it addressable ===")
    s, j = http("POST", f"/v1/sessions/{app_sid}/guests", {"agent": "Cora"})
    check("inviting it twice is harmless", s == 200 and "already answers" in j.get("said", ""),
          str(j))
    s, j = http("POST", f"/v1/sessions/{tools_sid}/guests", {"agent": "Cora"})
    check("its own team is not something to be invited into", s != 200, str(j))
    # A name is only unique within a team.
    http("POST", f"/v1/sessions/{far_sid}/slots",
         {"cli_id": "claude", "display_name": "Cora", "role": "decoy"})
    s, j = http("POST", f"/v1/sessions/{far_sid}/guests", {"agent": "Cora"})
    check("a name the room already uses is refused", s != 200
          and "already has a seat called" in str(j), str(j))

    print("\n=== taking it out again ===")
    max_.tool("send_message", {"to": "Cora", "text": "one more job"})
    s, j = http("POST", f"/v1/sessions/{app_sid}/guests", {"agent": "Cora", "leave": True})
    check("it can be released", s == 200 and "no longer answers there" in j.get("said", ""),
          str(j))
    check("what it never read does not stay pending for ever",
          "had not read" in j.get("said", ""), str(j))
    check("it is off that roster", not seat(app_sid, "Cora").get("slot_id"),
          str(seat(app_sid, "Cora")))
    check("and not addressable from there any more",
          "no teammate matching" in max_.tool("send_message",
                                              {"to": "Cora", "text": "hello?"}))
    check("but its own team is untouched", bool(seat(tools_sid, "Cora").get("slot_id")),
          str(seat(tools_sid, "Cora")))
    s, j = http("POST", f"/v1/sessions/{tools_sid}/guests", {"agent": "Cora", "leave": True})
    check("and it cannot be released from the team it LIVES in", s != 200
          and "lives in that team" in str(j), str(j))

if __name__ == "__main__":
    try:
        main()
    finally:
        cleanup()
    if fails:
        print(f"\nGUESTS FAILED ({len(fails)}): " + "; ".join(fails))
        sys.exit(1)
    print("\nALL GUEST CHECKS PASSED")
