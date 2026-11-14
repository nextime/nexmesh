#!/usr/bin/env python3
"""Delivery to an INTERACTIVE seat: typed at its terminal.

A seat with a terminal rather than a structured driver used to have exactly
one delivery route -- the agent's own next tool call. For an interactive CLI
sitting at its prompt that can be minutes away, or never, so a task handed to
one simply waited. A pty is a keyboard: this suite proves the hub now types
the message at the CLI's own prompt, that the CLI reads it, and the two things
that must never happen -- answering a dialog on the operator's behalf, and
typing at a terminal nothing vouches for.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run FROM inside one. Inherited, it would hand every
# shim here the same identity and make them all claim a single seat.
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17963; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-ptyinject-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
TYPED = f"{T}/typed.log"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
for name in ("faketyped", "faketui"):
    d = open(f"{SCR}/fake-cli/{name}.toml").read().replace(
        'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
    open(f"{CLIS}/{name}.toml", "w").write(d)

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

def q(sql, args=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, args)]
    finally: c.close()

def until(fn, seconds=25.0, step=0.3):
    end = time.time() + seconds
    while time.time() < end:
        try: v = fn()
        except Exception: v = None
        if v: return v
        time.sleep(step)
    return None

def typed():
    try: return open(TYPED).read()
    except OSError: return ""

def main():
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        # The node explains a held injection at debug level: it happens on
        # every tick while a dialog is up, and an operator does not need to
        # be told three times a second.
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info,nexmesh_node=debug",
                 NO_COLOR="1", FAKETYPED_LOG=TYPED, FAKETYPED_DIALOG_SECONDS="8")))
    ok = until(lambda: "faketyped" in (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))
    check("node online with the typeable test agent", bool(ok))

    sid = http("POST", "/v1/sessions", {"name": "ptytest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "faketyped",
        "mode": "interactive", "display_name": "Tess", "role": "engineer",
        "workspace": "local", "task": "sit at a prompt", "placement": {"node": "machine-b"}})
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("interactive agent spawned", s == 200, str(j))

    print("\n=== a terminal is a delivery route, and the hub knows it ===")
    agent = until(lambda: (q("SELECT id,push_capable,io_kind,pid FROM agents "
                             "WHERE slot_id=? AND exited_at_ms IS NULL", (slot,)) or [None])[0])
    check("the agent is running", agent is not None)
    check("an interactive seat with a driver is push-capable",
          agent and agent["push_capable"] == 1, str(agent))
    check("its I/O is recorded as a pty", agent and agent["io_kind"] == "pty", str(agent))
    st = http("GET", f"/v1/sessions/{sid}/status")[1]["agents"]
    row = [a for a in st if a["slot_id"] == slot][0]
    check("status reports the pid behind the seat", bool(row.get("pid")), str(row))
    check("status reports the terminal", row.get("io_kind") == "pty", str(row))
    check("status reports the agent as running, not just the work as idle",
          row.get("status") == "running", str(row))

    print("\n=== the message is typed at the prompt, and the CLI reads it ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Tess", "text": "ping at the terminal"})
    got = until(lambda: "ping at the terminal" in typed(), seconds=30)
    elapsed = time.time() - t0
    check("the CLI itself received the message", bool(got), typed()[-300:])
    check(f"it arrived without any tool call ({elapsed:.1f}s)", bool(got) and elapsed < 20,
          f"{elapsed:.1f}s")
    check("it was typed as ONE line, so the TUI got one submit",
          len([l for l in typed().splitlines() if "ping at the terminal" in l]) == 1,
          typed()[-400:])
    check("the marker a tool result would have carried is there too",
          "[PENDING MESSAGES]" in typed(), typed()[-300:])
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages "
                             "WHERE body LIKE '%ping at the terminal%'") or [{}])[0].get("acked_at_ms"))
    check("the hub stops holding it in flight once typed", bool(acked),
          str(q("SELECT inflight_at_ms,acked_at_ms FROM messages")))

    print("\n=== typed once, not once per tick ===")
    before = len([l for l in typed().splitlines() if "ping at the terminal" in l])
    time.sleep(8)
    check("no duplicate after further ticks",
          len([l for l in typed().splitlines() if "ping at the terminal" in l]) == before,
          typed()[-400:])

    print("\n=== nothing is typed over a dialog ===")
    # The first message puts a permission prompt on the agent's screen. The
    # second must wait for it: typing there delivers nothing, and the Enter
    # after it would ANSWER the question -- on a real CLI, approving a tool
    # call nobody read.
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Tess", "text": "open the dialog please"})
    check("the dialog-opening message was typed",
          bool(until(lambda: "open the dialog please" in typed(), seconds=30)), typed()[-300:])
    time.sleep(1.0)
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Tess", "text": "second message waits"})
    time.sleep(5.0)   # two push ticks, with the dialog still up
    held = "second message waits" not in typed()
    check("it was held while the prompt was waiting for an answer", held, typed()[-500:])
    check("and the hub put it back in the queue rather than losing it",
          bool(q("SELECT 1 FROM messages WHERE body LIKE '%second message waits%' "
                 "AND acked_at_ms IS NULL")),
          str(q("SELECT body,inflight_at_ms,acked_at_ms FROM messages")))
    after = until(lambda: "second message waits" in typed(), seconds=40)
    check("and it was typed once the dialog cleared", bool(after), typed()[-500:])
    nodelog = open(f"{T}/node.log").read()
    check("the node said why it waited",
          "waiting for an answer" in nodelog, nodelog[-600:])

    print("\n=== a terminal nothing vouches for is never typed at ===")
    # An adopted pane has a pty too, and whatever is sitting in it may be a
    # shell prompt, where a teammate's message plus a newline is EXECUTED.
    # `faketui` has no driver, which is what adoption looks like.
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "faketui",
        "mode": "interactive", "display_name": "Anon", "role": "watcher",
        "workspace": "local", "task": "just sit there", "placement": {"node": "machine-b"}})
    bare = j["slot_id"]
    http("POST", f"/v1/slots/{bare}/spawn")
    bare_agent = until(lambda: (q("SELECT id,push_capable,io_kind FROM agents "
                                  "WHERE slot_id=? AND exited_at_ms IS NULL", (bare,)) or [None])[0])
    check("it is running with a terminal of its own",
          bare_agent and bare_agent["io_kind"] == "pty", str(bare_agent))
    check("but it is NOT push-capable: no driver, no proof of what is in there",
          bare_agent and bare_agent["push_capable"] == 0, str(bare_agent))
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Anon", "text": "rm -rf would be typed here"})
    time.sleep(8)
    tr = ""
    if bare_agent:
        tr = http("GET", f"/v1/agents/{bare_agent['id']}/transcript?tail=65536")[1].get("transcript") or ""
    check("nothing was typed into it", "rm -rf would be typed here" not in tr, tr[-300:])
    check("its message waits for the MCP floor instead",
          bool(q("SELECT 1 FROM messages WHERE body LIKE '%rm -rf would be typed%' "
                 "AND acked_at_ms IS NULL")),
          str(q("SELECT body,acked_at_ms FROM messages WHERE body LIKE '%rm -rf%'")))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL PTY DELIVERY CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
