#!/usr/bin/env python3
"""A CLI that survives its node.

An interactive agent normally runs on a pty the NODE owns, as the node's own
child. That makes the node's lifetime the agent's: stop the node and the
master closes, the slave sends SIGHUP, and the CLI dies with whatever it was
doing. So deploying a node binary ends every conversation on that machine --
which is a thing nobody should have to weigh before a deploy, and it cost
real work on the live fleet: a seat dropped at a node restart got spawned a
second time, and two CLIs ended up appending to one 123MB conversation.

`[supervise] kind = "tmux"` puts the CLI in a tmux session instead. The tmux
server owns the real pty and outlives the node; the node attaches a SECOND,
grouped client under a pty of its own. This suite kills the node outright --
SIGKILL, no chance to tidy up, the way a crash or a hard restart would -- and
checks that the SAME process is still there afterwards and still being
watched.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time, urllib.request, urllib.error
import fcntl, pty, select, struct, termios

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17911; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-supervise-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
os.environ.setdefault("NEXMESH_IDENTITY", "/nonexistent/nexmesh-identity")
TMUX_SOCKET = f"nexmesh-sup-{os.getpid()}"
SHIMLOG = f"{T}/shim.log"   # what the CLI's own shim did, line by line
procs = []; fails = []

def cleanup():
    print(f"  (logs: {T})")
    subprocess.run(["tmux", "-L", TMUX_SOCKET, "kill-server"], capture_output=True)
    for p in procs:
        try: p.send_signal(signal.SIGKILL)
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

def tmux(*a):
    return subprocess.run(["tmux", "-L", TMUX_SOCKET, *a],
                          capture_output=True, text=True).stdout.strip()

def until(fn, seconds=30.0, step=0.25):
    end = time.time() + seconds
    while time.time() < end:
        try:
            if fn(): return True
        except Exception: pass
        time.sleep(step)
    return False

class Term:
    """A real terminal of a known size, for driving the ctl as a person would."""
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
    def send(self, s): os.write(self.master, s.encode())


def start_node():
    p = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=TOK, RUST_LOG="info", NO_COLOR="1",
                 NEXMESH_TMUX_SOCKET=TMUX_SOCKET))
    procs.append(p)
    return p

def node_online():
    j = http("GET", "/v1/nodes")[1]
    return bool(j.get("nodes")) and j["nodes"][0].get("connected")

def agent():
    for a in http("GET", f"/v1/sessions/{SID}/status")[1].get("agents", []):
        if a["slot_id"] == SLOT: return a
    return None

def main():
    global TOK, SID, SLOT
    if not shutil.which("tmux"):
        print("  SKIP  tmux is not installed"); return 0

    # A CLI that is held in tmux rather than owned by the node.
    shutil.copy(f"{SCR}/fake-cli/faketui.toml", f"{CLIS}/faketui.toml") \
        if os.path.exists(f"{SCR}/fake-cli/faketui.toml") else None
    with open(f"{CLIS}/heldtui.toml", "w") as f:
        f.write(f'''
# The same fake TUI the attach tests use, but HELD IN TMUX rather than owned
# by the node -- which is the whole subject of this suite.
[cli]
id = "heldtui"
display_name = "Held TUI"
command = "faketui"
version_args = ["--version"]
extra_paths = ["{SCR}/fake-cli"]

[supervise]
kind = "tmux"

[modes.interactive]
args = []
attachable = true
''')

    # And one that runs a REAL MCP shim as its child, held in tmux too: the
    # shim is what carries an agent id, and the id it carries is the one it
    # was given when the CLI started.
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
# The conversation the CLI starts in. A real claude puts this into the
# environment of every MCP server it starts, which is how the fleet learns
# which conversation a seat is actually in.
CLAUDE_CODE_SESSION_ID = "conv-one"

[supervise]
kind = "tmux"

[modes.interactive]
args = []
attachable = true
typed_delivery = true
''')

    TOK = [l.split()[1] for l in subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
        capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("hub is up", until(lambda: http("GET", "/healthz")[1].get("ok")))

    node = start_node()
    check("node is online", until(node_online, 40))

    print("\n=== 1. a seat held in tmux ===")
    SID = http("POST", "/v1/sessions", {"name": "suptest"})[1]["session_id"]
    SLOT = http("POST", f"/v1/sessions/{SID}/slots", {"cli_id": "heldtui",
        "mode": "interactive", "display_name": "Held",
        "placement": {"node": "machine-b"}})[1]["slot_id"]
    s, j = http("POST", f"/v1/slots/{SLOT}/spawn", {})
    check("it spawned", s == 200, f"{s} {j}")
    check("the agent is running", until(lambda: agent() and agent().get("pid")))

    held = f"nexmesh-cli-{SLOT}"
    check("the CLI is in a tmux session of its own",
          until(lambda: held in tmux("list-sessions", "-F", "#{session_name}")),
          tmux("list-sessions", "-F", "#{session_name}"))
    # The node attaches a SECOND, grouped client -- that is what makes the
    # ordinary attach path work unchanged.
    check("the node has a grouped client onto it",
          f"nexmesh-{SLOT}" in tmux("list-sessions", "-F", "#{session_name}"),
          tmux("list-sessions", "-F", "#{session_name}"))

    # The agent's identity must be the CLI IN THE PANE, not the node's tmux
    # client: every check that asks "is this still the same CLI", and the
    # guard that asks "is the CLI inside the pane this view looks at" before
    # typing a teammate's message into it, depend on that.
    pane_pid = tmux("display-message", "-t", held, "-p", "#{pane_pid}")
    before = agent()["pid"]
    check("the agent's pid is the process in the pane, not the tmux client",
          str(before) == pane_pid, f"agent={before} pane={pane_pid}")

    print("\n=== 1b. the attach is one agent, not a way onto the machine ===")
    # The node's client onto a held session had tmux's whole keyboard:
    # Ctrl-B % in an attach split the pane and gave whoever was attached a
    # SHELL on this host, Ctrl-B c another, Ctrl-B : a tmux command prompt.
    view = f"nexmesh-{SLOT}"
    # ASK FOR THE VALUE, NOT THE LISTING. `show-options -t <s> <opt>` prints
    # a "<name> <value>" line on tmux 3.5a but prints NOTHING for an option
    # whose value is the key `None` on 3.6 -- so a test written against the
    # listing passes on one host in the fleet and fails on the next while
    # the lock is working on both. `-v` prints the value alone, on either.
    check("the view the node attaches has no prefix key at all",
          until(lambda: tmux("show-options", "-v", "-t", view, "prefix") == "None"),
          tmux("show-options", "-v", "-t", view, "prefix"))
    check("nor a second one",
          tmux("show-options", "-v", "-t", view, "prefix2") == "None",
          tmux("show-options", "-v", "-t", view, "prefix2"))
    # And the operator's own way in is untouched: that IS how you get a
    # terminal on a machine, and the lock is on the door nexmesh opens.
    check("the held session keeps tmux's own keys, which is the way in by hand",
          tmux("show-options", "-v", "-t", held, "prefix") != "None",
          tmux("show-options", "-v", "-t", held, "prefix"))
    # At WINDOW scope, which is where the option lives. Asked at session
    # scope this came back empty, and an empty answer was accepted as "off"
    # -- so a set that tmux had quietly ignored read as a set that worked.
    check("and a CLI that exits takes its session with it rather than going dead",
          until(lambda: tmux("show-options", "-wv", "-t", held, "remain-on-exit")
                        in ("off", "0")),
          tmux("show-options", "-wv", "-t", held, "remain-on-exit"))

    print("\n=== 2. kill the node outright ===")
    # SIGKILL: no chance to tidy up, the way a crash or a hard restart goes.
    node.send_signal(signal.SIGKILL)
    node.wait()
    check("the node is gone", until(lambda: not node_online(), 40))
    # THE WHOLE POINT.
    check("the CLI is still in tmux after its node was killed",
          held in tmux("list-sessions", "-F", "#{session_name}"),
          tmux("list-sessions", "-F", "#{session_name}"))
    still = tmux("display-message", "-t", held, "-p", "#{pane_pid}")
    check("and it is the SAME process, not a new one", still == pane_pid,
          f"was {pane_pid}, now {still}")
    # A managed agent is disconnected, NOT exited: its process is right
    # there, and saying it exited would invite recovery to start a second one
    # in the same seat on top of it.
    check("the hub calls it disconnected rather than exited",
          until(lambda: agent() and agent()["status"] == "disconnected" and
                        not agent().get("exited_at_ms"), 40),
          json.dumps(agent()))

    print("\n=== 3. the node comes back ===")
    # A SEAT HELD BY AN OLDER NODE, OR ON A HOST WHOSE OWN tmux SAYS ON.
    #
    # `hold` runs only for a session that does not exist yet, so a seat that
    # outlives its node -- the whole point of holding one -- is never told
    # again. Turn the option back on by hand with nothing running, the way
    # an operator's config or a pre-fix binary would have left it, and the
    # returning node has to put it back.
    tmux("set-option", "-w", "-t", held, "remain-on-exit", "on")
    check("remain-on-exit is on again, as a seat from before the fix would be",
          tmux("show-options", "-wv", "-t", held, "remain-on-exit") == "on",
          tmux("show-options", "-wv", "-t", held, "remain-on-exit"))
    start_node()
    check("node is online again", until(node_online, 40))
    s, j = http("POST", f"/v1/slots/{SLOT}/spawn", {})
    # Spawning into an occupied seat is refused; recovery or an attach is how
    # it comes back. Either way what must NOT happen is a second CLI.
    print(f"  (spawn while held: {s} {str(j)[:90]})")
    check("there is still exactly ONE held session",
          tmux("list-sessions", "-F", "#{session_name}").count(held) == 1,
          tmux("list-sessions", "-F", "#{session_name}"))
    after = tmux("display-message", "-t", held, "-p", "#{pane_pid}")
    check("and still the same CLI process as before the restart",
          after == pane_pid, f"was {pane_pid}, now {after}")

    print("\n=== 4. repeated starts while this node is watching the seat ===")
    # Invariants under repeated starts: one view session, one held session,
    # and a seat that still has its process afterwards.
    #
    # NOT a reproduction of the loop that cost an evening on the live fleet.
    # That one is hub-side -- a reattached process registering under a new
    # agent id hit the UNIQUE index on (node_id, pid, proc_start_ticks) --
    # and `nodes::reregister_tests` is where it is reproduced, having been
    # checked to fail without the fix. These checks pass either way, because
    # here the hub still has the seat's agent and refuses a second start
    # before it ever reaches the node. Kept for the invariants, not as a
    # regression test.
    view = f"nexmesh-{SLOT}"
    check("the node has its view onto the seat",
          until(lambda: view in tmux("list-sessions", "-F", "#{session_name}")),
          tmux("list-sessions", "-F", "#{session_name}"))
    # Re-attaching is also where the seat gets told again: a node that only
    # set this at creation would have carried the sabotage above for the
    # rest of the seat's life, and a dead pane would then stand in for a
    # running agent on exactly the seats that live longest.
    check("and re-attaching put remain-on-exit back to off",
          until(lambda: tmux("show-options", "-wv", "-t", held, "remain-on-exit")
                        in ("off", "0"), 20),
          tmux("show-options", "-wv", "-t", held, "remain-on-exit"))
    for round_n in (1, 2, 3):
        http("POST", f"/v1/slots/{SLOT}/spawn", {})
        time.sleep(1.5)
        check(f"round {round_n}: still exactly one view session",
              tmux("list-sessions", "-F", "#{session_name}").split().count(view) == 1,
              tmux("list-sessions", "-F", "#{session_name}"))
    # What the loop destroyed: the hub's knowledge that anything is in the
    # seat. Checked last because it is the symptom the operator sees -- an
    # attach that says "no process" about a CLI that is right there.
    check("the seat still has its process, so attach has something to open",
          until(lambda: agent() and agent().get("pid"), 20), json.dumps(agent()))
    check("and it is the SAME CLI, not a second one",
          tmux("display-message", "-t", held, "-p", "#{pane_pid}") == pane_pid,
          f"was {pane_pid}, now {tmux('display-message', '-t', held, '-p', '#{pane_pid}')}")
    check("with one held session still, not several",
          tmux("list-sessions", "-F", "#{session_name}").split().count(held) == 1,
          tmux("list-sessions", "-F", "#{session_name}"))

    print("\n=== 5. a reattached CLI keeps its own tools ===")
    # THE FAILURE THIS SECTION EXISTS FOR, found on the live fleet.
    #
    # A CLI's MCP shim is told its agent id once, in the config written at
    # the spawn that started the CLI, and that cannot be rewritten while the
    # process lives. Supervision then made the CLI outlive the node: it is
    # reattached, the hub mints a NEW agent id for it, and the shim is still
    # saying the old one. It was refused -- so the seat could be TYPED AT
    # while every tool the agent had was dead: it could not reply to the
    # operator, store what it learned, or set its own summary. Four seats
    # went quiet that way and the only symptom was one failed set_summary.
    SSLOT = http("POST", f"/v1/sessions/{SID}/slots", {"cli_id": "shimtui",
        "mode": "interactive", "display_name": "Shim",
        "placement": {"node": "machine-b"}})[1]["slot_id"]
    s, j = http("POST", f"/v1/slots/{SSLOT}/spawn", {})
    check("the shim CLI spawned", s == 200, f"{s} {j}")

    def shimlog():
        return open(SHIMLOG).read() if os.path.exists(SHIMLOG) else ""
    check("its shim registered and its tools work",
          until(lambda: "tool set_summary: ok" in shimlog(), 40), shimlog()[-400:])
    before_agent = None
    for a in http("GET", f"/v1/sessions/{SID}/status")[1].get("agents", []):
        if a["slot_id"] == SSLOT: before_agent = a.get("agent_id")
    held2 = f"nexmesh-cli-{SSLOT}"
    pane2 = tmux("display-message", "-t", held2, "-p", "#{pane_pid}")
    check("and it is held in tmux", bool(pane2), tmux("list-sessions", "-F", "#{session_name}"))

    # Kill the node under it, exactly as a deploy does.
    for p in list(procs):
        if p.poll() is None and "nexmesh-node" in " ".join(p.args):
            p.send_signal(signal.SIGKILL); p.wait()
    check("the node is gone again", until(lambda: not node_online(), 40))
    start_node()
    check("and back", until(node_online, 40))
    check("the CLI is the same process after the restart",
          tmux("display-message", "-t", held2, "-p", "#{pane_pid}") == pane2,
          f"was {pane2}")

    # The hub gives the reattached seat a new agent id...
    after_agent = None
    def reattached():
        found = None
        for a in http("GET", f"/v1/sessions/{SID}/status")[1].get("agents", []):
            if a["slot_id"] == SSLOT and a.get("status") == "running":
                found = a.get("agent_id")
        return found
    until(lambda: reattached() is not None, 40)
    after_agent = reattached()
    print(f"  (agent id was {before_agent}, is now {after_agent})")

    # ...and the shim, still saying the OLD one, must be accepted anyway.
    mark = len(shimlog())
    # Typing at the seat makes the CLI call a tool, which is the question
    # "do your tools still work" asked at a moment of our choosing.
    http("POST", f"/v1/sessions/{SID}/messages",
         {"text": "after-the-restart", "to": "Shim"})
    def worked_after():
        return "tool set_summary: ok" in shimlog()[mark:]
    check("ITS TOOLS STILL WORK after the reattach", until(worked_after, 60),
          shimlog()[mark:][-500:] or "(nothing new in the shim log)")
    node_log = open(f"{T}/node.log").read()
    check("the node did not refuse the shim as an unknown agent",
          "is not known to this node" not in node_log,
          [l for l in node_log.splitlines() if "not known" in l][-2:])
    check("it says instead that it matched the shim to the seat it reattached",
          "registering it under the seat" in node_log,
          [l for l in node_log.splitlines() if "shim" in l][-3:])

    print("\n=== 5b. the window is the size the viewer can show ===")
    # A viewer reserves its bottom row for the key bar, so the agent gets one
    # row less than the terminal -- and the node resizes the pty to match.
    # For a seat held in tmux that pty is a tmux CLIENT, and tmux hears about
    # a resize only as SIGWINCH. A signal that arrives before tmux installs
    # its handler is gone, because the default disposition of SIGWINCH is to
    # ignore it, and tmux then keeps the size it read at startup for ever.
    #
    # It sizes the window one row taller than the viewer can show, the CLI
    # draws a row nobody sees, and since a TUI repaints only what changed the
    # view never recovers: a refresh half-fixes it and the next keystroke
    # spoils it again. Measured on the live fleet, every tmux client on one
    # machine was exactly one row out, and one seat's screen was unreadable.
    #
    # THIS CHECK DOES NOT REPRODUCE THAT RACE, and was tried with the fix
    # removed to be sure: a single client started on a quiet machine gets its
    # SIGWINCH and is correct either way. What it does hold is the invariant
    # the race broke -- the window is what the viewer can show -- so a change
    # that stops the size reaching tmux at all is caught here.
    # A real viewer, in a terminal of a known size, so the number under test
    # is the one the ctl computes rather than one the test asserts.
    TERM_ROWS, TERM_COLS = 24, 100
    view = Term([f"{BIN}/nexmesh", "--hub", f"http://127.0.0.1:{PORT}",
                 "attach", "suptest", "Shim"], cols=TERM_COLS, rows=TERM_ROWS)
    view.pump(2.0)
    def window():
        return tmux("display-message", "-p", "-t", f"nexmesh-cli-{SSLOT}",
                    "#{window_width}x#{window_height}").strip()
    # One row is the key bar's, so the agent gets one less than the terminal.
    want = f"{TERM_COLS}x{TERM_ROWS - 1}"
    check("tmux sizes the window to what the viewer can show, not one row more",
          until(lambda: window() == want, 30), f"wanted {want}, tmux says {window()}")
    view.send("\x1d")   # Ctrl-] detaches
    view.pump(1.0)

    print("\n=== 6. /clear moves the seat to a new conversation ===")
    # A managed seat is started on a named conversation and the hub records
    # it, so the seat can be put back as it was. Then the operator types
    # `/clear` or `/new`: the CLI keeps running and starts a FRESH
    # conversation. If nothing tells the hub, the next respawn resumes the
    # one that was deliberately left behind, and the real one is orphaned
    # with nothing pointing at it.
    #
    # The CLI is the only thing that knows, and it says so by restarting its
    # MCP servers with the new id in their environment.
    def recovery():
        c = sqlite3.connect(DB)
        try:
            row = c.execute("SELECT resume_handle FROM slots WHERE id=?", (SSLOT,)).fetchone()
            bind = c.execute("SELECT handle FROM slot_conversations WHERE slot_id=?",
                             (SSLOT,)).fetchone()
            return (row[0] if row else None, bind[0] if bind else None)
        finally:
            c.close()
    check("the hub recorded the conversation the seat started in",
          until(lambda: recovery() == ("conv-one", "conv-one"), 30), str(recovery()))

    mark2 = len(shimlog())
    tmux("send-keys", "-t", f"nexmesh-cli-{SSLOT}", "newconv conv-two", "Enter")
    check("the CLI started a new conversation",
          until(lambda: "cleared to conv-two: ok" in shimlog()[mark2:], 60),
          shimlog()[mark2:][-300:])
    # THE POINT: recovery must now name the conversation that is actually in
    # the seat, not the one it was started on.
    check("and the hub follows it to the new one, handle and binding both",
          until(lambda: recovery() == ("conv-two", "conv-two"), 40), str(recovery()))

    print("\n=== 6b. only a shim really INSIDE the seat may say so ===")
    # The claimed agent id is text in an environment variable; the ancestry
    # is not. A shim run by hand on a borrowed id would otherwise rewrite the
    # seat's conversation to whatever ITS environment carried -- and the next
    # respawn of that seat reads the handle back and tries to resume it. That
    # happened for real while this was being debugged: a diagnostic shim
    # repointed a live seat at an unrelated thread, and only the seat being
    # held in tmux stopped a second CLI starting on somebody else's work.
    agent_now = None
    c = sqlite3.connect(DB)
    try:
        row = c.execute("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                        (SSLOT,)).fetchone()
        agent_now = row[0] if row else None
    finally:
        c.close()
    check("the seat has a live agent to borrow the id of", bool(agent_now), str(agent_now))
    imposter = subprocess.run(
        [f"{BIN}/nexmesh-mcpd"], input=json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "imposter", "version": "0"}}}) + "\n",
        capture_output=True, text=True, timeout=30,
        env=dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_AGENT_ID=agent_now or "ag-none",
                 NEXMESH_SLOT=SSLOT, NEXMESH_CLI="shimtui",
                 # What a hand-run shim carries: ITS shell's conversation.
                 CLAUDE_CODE_SESSION_ID="conv-imposter"))
    check("a shim on a borrowed id still registers, so a live seat is never silenced",
          '"tools"' in imposter.stdout or "result" in imposter.stdout,
          (imposter.stdout + imposter.stderr)[-300:])
    time.sleep(3)
    check("but the seat's conversation is UNCHANGED by it",
          recovery() == ("conv-two", "conv-two"), str(recovery()))
    check("and the node said it would not take its word",
          "not running inside its CLI" in open(f"{T}/node.log").read(),
          open(f"{T}/node.log").read()[-400:])

    print("\n=== 7. the CLI crashes: the seat is dead, and said to be ===")
    # A seat held in tmux survives the NODE dying, which is the point of the
    # whole arrangement -- and that made the opposite case worth a check of
    # its own: when the CLI itself goes, the session has to go with it, or
    # the hub keeps a seat it believes is running and a viewer sits looking
    # at a dead screen with nothing to tell it otherwise.
    slot7 = http("POST", f"/v1/sessions/{SID}/slots", {"cli_id": "heldtui",
        "mode": "interactive", "display_name": "Doomed",
        "placement": {"node": "machine-b"}})[1]["slot_id"]
    http("POST", f"/v1/slots/{slot7}/spawn", {})
    def seat7():
        for a in http("GET", f"/v1/sessions/{SID}/status")[1].get("agents", []):
            if a["slot_id"] == slot7: return a
        return None
    check("the doomed seat started", until(lambda: seat7() and seat7().get("pid")),
          str(seat7()))
    held7 = f"nexmesh-cli-{slot7}"
    pane7 = tmux("display-message", "-t", held7, "-p", "#{pane_pid}")
    check("its CLI is the process in the pane", pane7.isdigit(), pane7)
    os.kill(int(pane7), signal.SIGKILL)
    check("the tmux session goes with the CLI rather than holding a dead pane",
          until(lambda: held7 not in tmux("list-sessions", "-F", "#{session_name}"), seconds=20),
          tmux("list-sessions", "-F", "#{session_name}"))
    check("and the hub is told the agent exited, so every viewer is detached",
          until(lambda: (seat7() or {}).get("pid") is None
                        or (seat7() or {}).get("status") not in ("running", "starting"),
                seconds=25),
          str(seat7()))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL SUPERVISE CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
