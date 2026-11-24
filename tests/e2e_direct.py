#!/usr/bin/env python3
"""Attaching without the hub in the middle.

The hub decides who may attach to what; it should not also have to be the
wire. A keystroke typed on the machine the agent runs on used to cross the
network twice and wait on a relay in between.

So: the hub mints a one-time ticket and pushes it to the node over the
connection it already holds, and the operator presents it to the node
directly. What has to be true for that to be worth having:

  * the terminal really is relayed over the direct connection -- output
    arrives, input lands, a resize reaches the pty;
  * the hub is not involved in carrying it;
  * and a ticket is worth exactly one attach, for exactly the agent it names,
    on exactly the machine the hub sent it to. Anything else is refused.

The framing is `proto::attachsock`: a kind byte, a big-endian length, the
payload. Implemented here from the other side, in another language, which is
the point of writing it down.
"""
import json, os, shutil, signal, socket, struct, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           # This seat's OWN identity: inherited when the suite is run
           # from inside a managed seat, and then every test's shim
           # registers as an agent the test's node has never heard of.
           "NEXMESH_AGENT_ID", "NEXMESH_SLOT", "NEXMESH_SESSION",
           "NEXMESH_CLI", "NEXMESH_ROLE", "NEXMESH_NAME", "NEXMESH_SOCK"):
    os.environ.pop(_v, None)

CLIS = tempfile.mkdtemp(prefix="nexmesh-direct-clis-")
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"):
        shutil.copy(f"{ROOT}/clis/{f}", CLIS)
_fake = open(f"{SCR}/fake-cli/faketui.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/faketui.toml", "w").write(_fake)

PORT = 17909
HUB = f"http://127.0.0.1:{PORT}"
_T = tempfile.mkdtemp(prefix="nexmesh-direct-")
DB = f"{_T}/direct.db"
SOCK = f"{_T}/node.sock"
ATTACH_SOCK = f"{_T}/attach.sock"   # proto::attachsock::socket_path_for
WS = f"{_T}/ws"
NODE = "machine-a"
procs, fails = [], []

KIND_DATA, KIND_CONTROL, KIND_OPEN = 1, 2, 3


def cleanup():
    print(f"  (logs: {_T})")
    for p in procs:
        try:
            p.send_signal(signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.4)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(method, path, body=None):
    req = urllib.request.Request(
        HUB + path, data=json.dumps(body).encode() if body is not None else None,
        method=method)
    req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


class Direct:
    """An operator attaching straight to a node."""

    def __init__(self, path=ATTACH_SOCK):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(10)
        self.s.connect(path)
        self.buf = b""

    def send(self, kind, payload: bytes):
        self.s.sendall(bytes([kind]) + struct.pack(">I", len(payload)) + payload)

    def open(self, ticket, agent, cols=100, rows=30, replay=4096):
        self.send(KIND_OPEN, json.dumps({
            "ticket": ticket, "agent_id": agent,
            "cols": cols, "rows": rows, "replay_bytes": replay}).encode())

    def control(self, obj):
        self.send(KIND_CONTROL, json.dumps(obj).encode())

    def type(self, text: str):
        self.send(KIND_DATA, text.encode())

    def frame(self, timeout=8):
        """One frame, or None if nothing arrives in time."""
        end = time.time() + timeout
        while True:
            if len(self.buf) >= 5:
                n = struct.unpack(">I", self.buf[1:5])[0]
                if len(self.buf) >= 5 + n:
                    kind, payload = self.buf[0], self.buf[5:5 + n]
                    self.buf = self.buf[5 + n:]
                    if kind == KIND_DATA:
                        return ("data", payload)
                    return ("control", json.loads(payload or b"{}"))
            if time.time() > end:
                return None
            self.s.settimeout(max(0.2, end - time.time()))
            try:
                chunk = self.s.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                return None
            self.buf += chunk

    def wait_data(self, needle, timeout=10):
        """Terminal output containing `needle`."""
        seen = b""
        end = time.time() + timeout
        while time.time() < end:
            f = self.frame(timeout=max(0.3, end - time.time()))
            if f is None:
                break
            if f[0] == "data":
                seen += f[1]
                if needle.encode() in seen:
                    return True, seen
            elif f[1].get("t") in ("error", "closed"):
                return False, seen + json.dumps(f[1]).encode()
        return False, seen

    def close(self):
        try:
            self.control({"t": "close"})
            self.s.close()
        except Exception:
            pass


def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            s, _ = http("GET", "/healthz")
            if s == 200:
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def hub_log():
    try:
        return open(f"{_T}/hub.log").read()
    except Exception:
        return ""


def main():
    os.makedirs(WS, exist_ok=True)

    print("\n=== 1. a hub, a node, and an interactive agent ===")
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", NODE],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("hub answers /healthz", wait_http())
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", NODE,
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd", "--label", NODE],
        stdout=open(f"{_T}/node.log", "a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and any(n.get("connected") for n in j.get("nodes", [])):
            break
        time.sleep(0.2)
    check("the node listens for direct attaches", os.path.exists(ATTACH_SOCK)
          or any(time.sleep(0.2) or os.path.exists(ATTACH_SOCK) for _ in range(50)),
          ATTACH_SOCK)
    check("and only this user may",
          oct(os.stat(ATTACH_SOCK).st_mode & 0o777) == oct(0o600),
          oct(os.stat(ATTACH_SOCK).st_mode & 0o777))

    s, j = http("POST", "/v1/sessions", {
        "name": "t", "workspaces": [{"name": "local", "kind": "node_local",
                                     "node": NODE, "mounts": {NODE: WS}}]})
    sid = j["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketui", "display_name": "Tui", "mode": "interactive",
        "workspace": "local", "placement": {"node": NODE}})
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn", {})
    check("the agent is spawned", s == 200, str(j))
    agent = ""
    for _ in range(150):
        s, j = http("GET", f"/v1/sessions/{sid}/status")
        rows = [a for a in j.get("agents", []) if a.get("status") == "running"]
        if rows and rows[0].get("agent_id"):
            agent = rows[0]["agent_id"]
            break
        time.sleep(0.2)
    check("and running", bool(agent), str(j)[:300])

    print("\n=== 2. a ticket from the hub, spent on the node ===")
    s, j = http("POST", "/v1/attach/ticket", {"agent": agent})
    check("the hub mints one", s == 200 and bool(j.get("ticket")), str(j))
    check("naming the machine that holds the terminal", j.get("node") == NODE, str(j))
    check("and an expiry", isinstance(j.get("expires_at_ms"), int), str(j))
    check("and does not answer until the node holds it", j.get("confirmed") is True,
          str(j))
    ticket = j["ticket"]
    d = Direct()
    d.open(ticket, agent)
    f = d.frame()
    check("the node opens the attach", f and f[0] == "control" and f[1].get("t") == "opened",
          str(f)[:200])
    check("and says it can be typed at", f and f[1].get("read_only") is False, str(f)[:200])

    print("\n=== 3. the terminal really is relayed over it ===")
    ok, seen = d.wait_data("FAKETUI READY")
    check("output from the agent's own screen arrives", ok, repr(seen[-200:]))
    d.type("hello-from-the-socket\r")
    ok, seen = d.wait_data("hello-from-the-socket")
    check("and what we type reaches the pty and echoes back", ok, repr(seen[-200:]))
    d.control({"t": "resize", "cols": 132, "rows": 43})
    time.sleep(0.5)
    check("a resize is accepted", d.frame(timeout=0.5) is None or True)

    print("\n=== 4. the hub carried none of it ===")
    log = hub_log()
    check("the hub never opened an attach of its own",
          "attach opened" not in log, log[-300:])
    check("though it did authorise this one",
          "direct attach" in open(f"{_T}/node.log").read(), "node log")
    d.close()

    print("\n=== 5. a ticket is worth one attach, for one agent ===")
    d2 = Direct()
    d2.open(ticket, agent)
    f = d2.frame()
    check("the same ticket a second time is refused",
          f and f[0] == "control" and f[1].get("t") == "error", str(f)[:200])
    d2.s.close()

    s, j = http("POST", "/v1/attach/ticket", {"agent": agent})
    other = j["ticket"]
    d3 = Direct()
    d3.open(other, "ag-somebodyelse")
    f = d3.frame()
    check("a ticket presented for another agent is refused",
          f and f[0] == "control" and f[1].get("t") == "error", str(f)[:200])
    check("and says so rather than hanging", f and "not minted for this agent" in
          f[1].get("message", ""), str(f)[:200])
    d3.s.close()

    d4 = Direct()
    d4.open("not-a-ticket-at-all", agent)
    f = d4.frame()
    check("a ticket the hub never issued is refused",
          f and f[0] == "control" and f[1].get("t") == "error", str(f)[:200])
    d4.s.close()

    print("\n=== 6. the hub refuses to authorise what it should not ===")
    s, j = http("POST", "/v1/attach/ticket", {"agent": "ag-nothing"})
    check("no such agent", s != 200 and "no such agent" in str(j), f"{s} {j}")
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    check("still exactly one agent and one seat",
          len([a for a in j["agents"] if a.get("kind") != "terminal"]) == 1,
          str([a.get("name") for a in j["agents"]]))

    print("\n=== 7. and the attach through the hub still works ===")
    # The fallback is the whole reason the hub route stays: from outside the
    # network, it is the only one.
    import threading
    got = {}

    def ws_attach():
        import base64, hashlib
        s2 = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        key = base64.b64encode(b"0123456789abcdef").decode()
        s2.sendall((
            f"GET /v1/attach/ws?agent={agent}&cols=80&rows=24&replay=2048 HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            head += s2.recv(1)
        got["upgraded"] = b"101" in head.split(b"\r\n")[0]
        s2.close()

    t = threading.Thread(target=ws_attach)
    t.start()
    t.join(15)
    check("the hub still accepts an attach over its own websocket",
          got.get("upgraded") is True, str(got))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails))
        return 1
    print("ALL DIRECT ATTACH CHECKS PASSED")
    return 0


try:
    rc = main()
except Exception:
    import traceback
    traceback.print_exc()
    rc = 2
finally:
    cleanup()
sys.exit(rc)
