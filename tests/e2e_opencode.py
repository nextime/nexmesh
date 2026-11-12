#!/usr/bin/env python3
"""The http_server driver: an agent that runs its own server.

Drives a fake opencode server implementing the real API, so the whole path is
exercised -- native steering, token accounting, permissions, and a terminal
opened on demand -- without any model calls.
"""
import fcntl, json, os, pty, select, shutil, signal, sqlite3, struct, subprocess
import sys, tempfile, termios, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17907; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-oc-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
FLOG = f"{T}/fakeopencode.log"
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
d = open(f"{SCR}/fake-cli/fakeopencode.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/fakeopencode.toml", "w").write(d)

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
        with urllib.request.urlopen(r, timeout=25) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, a=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, a)]
    finally: c.close()

def flog():
    try: return open(FLOG).read()
    except OSError: return ""

def until(fn, seconds=35.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

class Term:
    def __init__(self, args, cols=100, rows=30):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.p = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                  close_fds=True, cwd=ROOT)
        os.close(slave); procs.append(self.p); self.buf = ""
    def pump(self, s=0.5):
        end = time.time() + s
        while time.time() < end:
            r, _, _ = select.select([self.master], [], [], 0.1)
            if r:
                try: data = os.read(self.master, 65536)
                except OSError: break
                if not data: break
                self.buf += data.decode(errors="replace")
        return self.buf
    def expect(self, text, s=18.0):
        end = time.time() + s
        while time.time() < end:
            if text in self.buf: return True
            self.pump(0.3)
        return text in self.buf
    def send(self, s): os.write(self.master, s.encode())

def main():
    tok = [l.split()[1] for l in subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
           capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-a", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 FAKEOPENCODE_LOG=FLOG)))
    ok = until(lambda: "fakeopencode" in (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))
    check("node online with the server-backed test CLI", bool(ok))

    sid = http("POST", "/v1/sessions", {"name": "octest", "workspaces": [
        {"name": "app", "kind": "node_local", "node": "machine-a", "mounts": {"machine-a": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakeopencode",
        "mode": "headless", "display_name": "Ovi", "role": "engineer",
        "workspace": "app", "task": "FIRST exercise the http driver",
        "placement": {"node": "machine-a"}})
    slot = j["slot_id"]
    check("slot created", s == 200, str(j))
    check("spawn accepted", http("POST", f"/v1/slots/{slot}/spawn")[0] == 200)

    print("\n=== the node finds the server the CLI printed ===")
    listening = until(lambda: "listening http://" in flog(), seconds=40)
    check("the server started and announced itself", bool(listening), flog()[:300])
    check("the password warning did not get mistaken for the URL",
          "agent server is up" in open(f"{T}/node.log").read(),
          open(f"{T}/node.log").read()[-400:])

    print("\n=== it is reached with a per-agent password ===")
    check("the server was given a password", "unauthorised" not in flog(), flog()[:300])
    pw_used = until(lambda: "session_created" in flog())
    check("and the node authenticated successfully", bool(pw_used), flog()[:400])

    print("\n=== session, prompt and workspace ===")
    check("a session was created in the slot's workspace",
          f'"{WS}"' in flog(), [l for l in flog().splitlines() if l.startswith("cwd")])
    check("the first prompt was QUEUED, not steered",
          any(l.startswith("prompt queue ::") for l in flog().splitlines()),
          [l for l in flog().splitlines() if l.startswith("prompt")])
    check("it carried the agent's brief",
          "exercise the http driver" in flog(),
          [l for l in flog().splitlines() if l.startswith("prompt")][:1])
    rh = until(lambda: (q("SELECT resume_handle FROM slots WHERE id=?", (slot,)) or [{}])[0].get("resume_handle"))
    check("the session id became the resume handle", bool(rh and rh.startswith("ses_")), str(rh))

    print("\n=== permissions are answered, or the agent stalls ===")
    replied = until(lambda: "permission_reply" in flog(), seconds=30)
    check("the driver answered a permission request", bool(replied), flog()[-300:])
    check("with a reply the API accepts", '"always"' in flog(),
          [l for l in flog().splitlines() if l.startswith("permission_reply")])

    print("\n=== token accounting from the session object ===")
    until(lambda: (q("SELECT input_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,)) or [{}])[0].get("input_tokens"))
    row = q("SELECT input_tokens,output_tokens,cache_read_tokens FROM agents WHERE slot_id=? ORDER BY started_at_ms DESC", (slot,))[0]
    check("input tokens recorded", row["input_tokens"] >= 1500, str(row))
    check("output tokens recorded", row["output_tokens"] >= 250, str(row))
    check("cache reads recorded (opencode nests them under cache.read)",
          row["cache_read_tokens"] >= 800, str(row))

    print("\n=== a message is STEERED, using opencode's own word for it ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Ovi", "text": "steer this in"})
    steered = until(lambda: any(l.startswith("prompt steer ::") for l in flog().splitlines()), seconds=30)
    elapsed = time.time() - t0
    check("delivery was 'steer'", bool(steered),
          [l for l in flog().splitlines() if l.startswith("prompt")])
    check(f"and it was prompt ({elapsed:.1f}s)", bool(steered) and elapsed < 15, f"{elapsed:.1f}s")
    check("the message text got through", "steer this in" in flog(), flog()[-300:])
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%steer this in%'") or [{}])[0].get("acked_at_ms"))
    check("and it was marked delivered", bool(acked), str(q("SELECT body,acked_at_ms FROM messages")))

    print("\n=== a headless agent you can still open a terminal onto ===")
    t = Term([f"{BIN}/nexmesh", "--hub", HUB, "attach", sid, "Ovi"])
    check("attach succeeded", t.expect("attached to Ovi"), t.buf[-300:])
    check("a terminal client was started on demand",
          t.expect("OPENCODE TUI attached"), t.buf[-400:])
    check("it was pointed at the agent's own server",
          any(l.startswith("attach_url http://127.0.0.1:") for l in flog().splitlines()),
          [l for l in flog().splitlines() if l.startswith("attach_url")])
    check("and it is a real tty, so it can be driven", t.expect("tty=True"), t.buf[-300:])
    t.send("hello from the operator\n")
    check("keystrokes reach it", t.expect("you typed: hello from the operator"), t.buf[-400:])

    print("\n=== closing the window leaves the agent running ===")
    t.send("\x1d")
    check("detach works", t.expect("detached"), t.buf[-200:])
    time.sleep(1.5)
    s, j = http("GET", f"/v1/sessions/{sid}/status")
    alive = [a for a in j["agents"] if a["name"] == "Ovi" and a["status"] == "running"]
    check("the agent survived the terminal closing", bool(alive), str(j)[:250])
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Ovi", "text": "still steering"})
    check("and can still be steered afterwards",
          until(lambda: "still steering" in flog(), seconds=25), flog()[-250:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL OPENCODE DRIVER CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
