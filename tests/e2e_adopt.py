#!/usr/bin/env python3
"""Adopting an already-running tmux session, and driving it from 'elsewhere'.

Starts a tmux session by hand (standing in for one you left running on another
machine), adopts it, then drives it through the real `nexmesh attach` client --
while checking the original session is never disturbed.
"""
import fcntl, json, os, pty, select, shutil, signal, struct, subprocess, sys
import tempfile, termios, time, urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run FROM inside one. Inherited, it would hand every
# shim here the same identity and make them all claim a single seat -- which
# looks like a bug in the hub and is not. The tests choose their own.
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)


SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17898; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-adopt-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
SRC = f"nexmesh-adopt-src-{os.getpid()}"
procs = []; fails = []

def cleanup():
    print(f"  (logs: {T})")
    subprocess.run(["tmux", "kill-session", "-t", SRC],
                   capture_output=True)
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
        with urllib.request.urlopen(r, timeout=25) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def tmux(*args, check_rc=False):
    r = subprocess.run(["tmux", *args], capture_output=True, text=True)
    if check_rc and r.returncode != 0:
        raise RuntimeError(f"tmux {args}: {r.stderr}")
    return r.stdout.strip()

class Term:
    def __init__(self, args, cols=100, rows=30):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.p = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                  close_fds=True, cwd=ROOT)
        os.close(slave); procs.append(self.p); self.buf = ""
    def pump(self, seconds=0.6):
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.1)
            if r:
                try: d = os.read(self.master, 65536)
                except OSError: break
                if not d: break
                self.buf += d.decode(errors="replace")
        return self.buf
    def expect(self, text, seconds=15.0):
        end = time.time() + seconds
        while time.time() < end:
            if text in self.buf: return True
            self.pump(0.3)
        return text in self.buf
    def send(self, s): os.write(self.master, s.encode())

def main():
    if not shutil.which("tmux"):
        print("  SKIP  tmux is not installed"); return 0

    print("=== 1. a session you already had running ===")
    tmux("kill-session", "-t", SRC)
    tmux("new-session", "-d", "-s", SRC, "-x", "90", "-y", "25",
         f"python3 -I {SCR}/fake-cli/faketui", check_rc=True)
    time.sleep(1.0)
    check("the pre-existing session is running",
          SRC in tmux("list-sessions", "-F", "#{session_name}"))
    pane = tmux("capture-pane", "-p", "-t", SRC)
    check("it has produced output before nexmesh ever saw it",
          "FAKETUI READY" in pane, pane[:200])

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
    # Deliberately start the node from inside a TMUX env var, the condition
    # that makes a nested client refuse.
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 TMUX="/tmp/fake-tmux-socket,1234,0")))
    ok = False
    for _ in range(140):
        s, j = http("GET", "/v1/nodes")
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "tmux" in j["nodes"][0]["clis"]; break
        time.sleep(0.25)
    check("node online and offering tmux adoption", ok)

    print("\n=== 2. adopt it ===")
    sid = http("POST", "/v1/sessions", {"name": "adopttest"})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/adopt",
        {"node": "machine-b", "tmux_target": SRC, "name": "Legacy", "role": "pre-existing"})
    check("adopt accepted", s == 200 and j.get("slot_id"), str(j))
    check("the reply is honest about it not being a team member",
          "not a team member" in json.dumps(j).lower(), str(j))

    print("\n=== 3. it is driveable from 'another machine' ===")
    t = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Legacy"])
    check("attach succeeds", t.expect("attached to Legacy"), t.buf[-300:])
    check("we see the screen of the session that was already running",
          t.expect("FAKETUI READY"), t.buf[-500:])
    t.send("hello from machine-b\n")
    check("we can type into the adopted session",
          t.expect("you said: hello from machine-b"), t.buf[-500:])

    print("\n=== 4. the original session is undisturbed ===")
    sessions = tmux("list-sessions", "-F", "#{session_name} #{session_group}")
    check("the original session still exists", SRC in sessions, sessions)
    grouped = [l for l in sessions.splitlines() if l.split()[0] != SRC]
    check("nexmesh joined as a SEPARATE grouped session, not by resizing yours",
          any(SRC in l for l in grouped), sessions)
    pane = tmux("capture-pane", "-p", "-t", SRC)
    check("what we typed landed in the original pane",
          "hello from machine-b" in pane, pane[-300:])

    print("\n=== 5. detaching leaves everything running ===")
    t.send("\x1d")
    check("detach works", t.expect("detached"), t.buf[-200:])
    time.sleep(1.0)
    check("the original session survives the detach",
          SRC in tmux("list-sessions", "-F", "#{session_name}"))
    tmux("send-keys", "-t", SRC, "after detach", "Enter")
    time.sleep(0.8)
    pane = tmux("capture-pane", "-p", "-t", SRC)
    check("and still works", "you said: after detach" in pane, pane[-300:])

    print("\n=== 5b. a viewer that arrives to a still pane still sees it ===")
    # "It attaches but there is only a black terminal with nothing." tmux is a
    # differential renderer: it writes what CHANGED, and it rewrites its
    # status line every second. So on a pane nobody has touched, the recent
    # scrollback is all status ticks and the screen itself has scrolled out of
    # the replay -- the viewer gets an empty screen with a clock in the
    # corner. With no replay AT ALL, the only thing that can paint the screen
    # is the node asking tmux to repaint its client, which is the fix.
    t2 = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Legacy", "--replay", "0"])
    check("attach succeeds with no scrollback replay",
          t2.expect("attached to Legacy"), t2.buf[-300:])
    check("and the screen arrives anyway",
          t2.expect("FAKETUI READY", seconds=15), t2.buf[-600:])
    # Where it came from, not merely that it came: a resize happens to repaint
    # a tmux client too, so seeing the screen does not by itself prove the
    # node asked for it. The node says when it asks, and a viewer that
    # arrives to an unchanged pane has nothing else to go on.
    nodelog = open(f"{T}/node.log").read()
    check("because the node asked tmux to repaint its client",
          "asked tmux to repaint an adopted pane" in nodelog, nodelog[-500:])
    t2.send("\x1d")
    t2.expect("detached")

    print("\n=== 6. a bad target fails cleanly ===")
    s, j = http("POST", f"/v1/sessions/{sid}/adopt",
        {"node": "machine-b", "tmux_target": "", "name": "Bad"})
    check("an empty target is refused", s >= 400, f"{s} {j}")
    before = len(http("GET", f"/v1/sessions/{sid}/status")[1]["agents"])
    s, j = http("POST", f"/v1/sessions/{sid}/adopt",
        {"node": "nosuchnode", "tmux_target": SRC, "name": "Bad2"})
    check("an unknown node is refused", s >= 400, f"{s} {j}")
    after = len(http("GET", f"/v1/sessions/{sid}/status")[1]["agents"])
    check("a failed adoption leaves no orphan slot behind", after == before, f"{before} -> {after}")

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL ADOPT CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
