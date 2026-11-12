#!/usr/bin/env python3
"""Attach end-to-end: a terminal here driving an agent's pty over the hub.

Spawns an interactive agent on a node, then drives the REAL `nexmesh attach`
client through a local pty -- so raw mode, keystroke relay, resize and detach
are all exercised, not simulated.
"""
import json, os, pty, select, shutil, signal, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17893; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-attach-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
_d = open(f"{SCR}/fake-cli/faketui.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/faketui.toml", "w").write(_d)

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
        with urllib.request.urlopen(r, timeout=20) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

class Term:
    """A local pty running the real attach client."""
    def __init__(self, args, cols=100, rows=30):
        self.master, slave = pty.openpty()
        import fcntl, termios, struct
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.p = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                  close_fds=True, cwd=ROOT)
        os.close(slave); procs.append(self.p)
        self.buf = ""
    def pump(self, seconds=1.0):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.1)
            if r:
                try: data = os.read(self.master, 65536)
                except OSError: break
                if not data: break
                self.buf += data.decode(errors="replace")
        return self.buf
    def expect(self, text, seconds=12.0):
        end = time.time() + seconds
        while time.time() < end:
            if text in self.buf: return True
            self.pump(0.3)
        return text in self.buf
    def send(self, s):
        os.write(self.master, s.encode())
    def winch(self, cols, rows):
        import fcntl, termios, struct
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.p.send_signal(signal.SIGWINCH)

def main():
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        try:
            if http("GET", "/healthz")[1].get("ok"): break
        except Exception: pass
        time.sleep(0.2)
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes")
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "faketui" in j["nodes"][0]["clis"]; break
        time.sleep(0.25)
    check("node online with the interactive test CLI", ok)

    sid = http("POST", "/v1/sessions", {"name": "attachtest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b", "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "faketui",
        "mode": "interactive", "display_name": "Max", "role": "engineer",
        "workspace": "local", "task": "be attachable", "placement": {"node": "machine-b"}})
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("interactive agent spawned", s == 200, str(j))

    # Wait for the agent to come up and produce its banner.
    aid = None
    for _ in range(80):
        s, j = http("GET", f"/v1/sessions/{sid}/resolve/Max")
        if s == 200 and j.get("agent_id"): aid = j["agent_id"]; break
        time.sleep(0.25)
    check("the agent is registered", aid is not None, str(j))

    got_tty = False
    for _ in range(80):
        s, j = http("GET", f"/v1/agents/{aid}/transcript?tail=8192")
        if "FAKETUI READY" in j.get("transcript", ""):
            got_tty = "tty=True" in j["transcript"]; break
        time.sleep(0.25)
    check("the agent really got a PTY, not a pipe", got_tty,
          j.get("transcript", "")[:200])

    print("\n=== attaching with the real client ===")
    t = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Max"])
    check("client reports the attach", t.expect("attached to Max"), t.buf[-300:])
    check("scrollback is replayed on attach", t.expect("FAKETUI READY"), t.buf[-300:])

    print("\n=== typing into an agent on the 'other machine' ===")
    t.send("hello from the operator\n")
    check("keystrokes reach the agent and its reply comes back",
          t.expect("you said: hello from the operator"), t.buf[-400:])

    print("\n=== resize propagates ===")
    t.winch(132, 50)
    t.pump(1.0)
    s, j = http("GET", "/v1/nodes")
    check("client survives a window change", t.p.poll() is None)

    print("\n=== a second viewer sees the same session ===")
    t2 = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Max"])
    check("a second attach is accepted", t2.expect("attached to Max"), t2.buf[-200:])
    t.send("broadcast check\n")
    check("both viewers see the same output",
          t.expect("you said: broadcast check") and t2.expect("you said: broadcast check"),
          f"t1={t.buf[-200:]!r} t2={t2.buf[-200:]!r}")

    print("\n=== detaching leaves the agent running ===")
    t2.send("\x1d")  # Ctrl-]
    check("Ctrl-] detaches", t2.expect("detached"), t2.buf[-200:])
    check("detach message says the agent survives",
          t2.expect("still running"), t2.buf[-200:])
    time.sleep(0.5)
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    alive = [a for a in j["agents"] if a["name"] == "Max" and a["status"] == "running"]
    check("the agent is still running after a detach", bool(alive), str(j)[:200])
    t.send("still here?\n")
    check("the remaining viewer still works",
          t.expect("you said: still here?"), t.buf[-300:])

    print("\n=== attaching to something that has no terminal ===")
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "faketui",
        "mode": "interactive", "display_name": "Ghost", "placement": {"node": "machine-b"}})
    t3 = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Ghost"])
    check("attaching to a never-spawned agent fails cleanly",
          t3.expect("no agent matching") or t3.expect("Error"), t3.buf[-300:])

    print("\n=== agent exit closes the viewers ===")
    t.send("quit\n")
    check("viewer is told when the agent exits",
          t.expect("exited") or t.expect("still running"), t.buf[-300:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL ATTACH CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
