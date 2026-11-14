#!/usr/bin/env python3
"""A seat with no driver, no live shim push, and a keyboard all the same.

The case this covers is the one an operator actually has: a CLI started by
hand in a tmux pane, which takes a seat through its shim and is then
unreachable by anything but its own next tool call. Adopt that pane and the
hub can type at it -- but only after the node has proved, through /proc, that
the CLI is still that same process and still inside the pane its own view is
looking at.

Both orders are exercised, because both happen: adopting a pane that already
holds a registered CLI (what you do), and a CLI registering into a pane that
was adopted earlier.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17964; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-termlink-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
HANDLOG = f"{T}/hand.log"; HANDLOG2 = f"{T}/hand2.log"
SRC = "nexmesh-termlink-src"; SRC2 = "nexmesh-termlink-src2"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)

procs = []; fails = []

# Our own tmux server: see the note in e2e_adopt.py. A suite that kills
# sessions must not be able to kill somebody's work.
TMUX_SOCKET = f"nexmesh-e2e-{os.getpid()}"

def tmux(*args):
    r = subprocess.run(["tmux", "-L", TMUX_SOCKET, *args], capture_output=True, text=True)
    return r.stdout.strip()

def kill_group(*names):
    """Kill these sessions AND the grouped ones nexmesh joined to them.

    `tmux new-session -t X` makes a second session in X's group, and killing
    X alone leaves that one alive -- holding the group, and its windows, open.
    A later run that creates a session called X then collides with the
    leftover group and `-t X` resolves to the wrong pane, which made this
    suite pass or fail depending on what the run before it left behind.
    """
    want = set(names)
    listing = tmux("list-sessions", "-F", "#{session_name} #{session_group}")
    for line in listing.splitlines():
        parts = line.split()
        if not parts:
            continue
        name = parts[0]
        group = parts[1] if len(parts) > 1 else ""
        if name in want or group in want or name.startswith("nexmesh-sl-"):
            tmux("kill-session", "-t", name)

def cleanup():
    print(f"  (logs: {T})")
    kill_group("Hand", "Hand2", SRC, SRC2)
    # The server is ours alone, so it goes too -- no leftovers for the next
    # run to collide with, and nothing of anybody else's on it to lose.
    tmux("kill-server")
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

def read(path):
    try: return open(path).read()
    except OSError: return ""

def hand_session(name, log, shimlog, session):
    """A CLI started by hand in a tmux pane, with a shim beside it.

    Started from a shell that stays behind when the CLI exits, which is the
    real shape of a terminal somebody left running -- and the case the whole
    design guards: a message typed at that shell would be EXECUTED.
    """
    sh = f"{T}/hand-{name}.sh"
    with open(sh, "w") as f:
        f.write("#!/bin/sh\n")
        for k, v in [("HANDCLI_LOG", log), ("HANDCLI_SHIMLOG", shimlog),
                     ("HANDCLI_MCPD", f"{BIN}/nexmesh-mcpd"), ("HANDCLI_SOCK", SOCK),
                     ("HANDCLI_NAME", name), ("HANDCLI_SESSION", session)]:
            f.write(f"export {k}='{v}'\n")
        f.write(f"python3 -I {SCR}/fake-cli/fakehandcli\n")
        f.write("exec sh -i\n")
    os.chmod(sh, 0o755)
    kill_group(name)
    tmux("new-session", "-d", "-s", name, "-x", "100", "-y", "30", sh)

def main():
    if not shutil.which("tmux"):
        print("  SKIP  tmux is not installed"); return 0
    # Anything a previous run (or a crash) left behind, before anything else:
    # a leftover group is indistinguishable from this run's own.
    kill_group("Hand", "Hand2", SRC, SRC2)

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
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info,nexmesh_node=debug",
                 NO_COLOR="1", NEXMESH_TMUX_SOCKET=TMUX_SOCKET,
                 TMUX="/tmp/fake-tmux-socket,1234,0")))
    check("node online", bool(until(lambda: (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("connected"))))

    print("\n=== 1. a CLI you started by hand, in a pane nexmesh knows nothing about ===")
    # The team exists; what does not exist is any knowledge of this terminal.
    sid = http("POST", "/v1/sessions", {"name": "termlink"})[1]["session_id"]
    hand_session("Hand", HANDLOG, f"{T}/shim.log", sid)
    seat = until(lambda: (q("SELECT s.id, s.session_id, s.terminal_via FROM slots s "
                            "WHERE s.display_name='Hand'") or [None])[0], seconds=40)
    check("it took a seat through its shim", seat is not None, read(f"{T}/shim.log")[-400:])
    if not seat:
        return 1
    slot = seat["id"]
    check("nothing drives it: no terminal link yet", seat["terminal_via"] is None, str(seat))
    agent = q("SELECT id,provenance,push_capable,io_kind FROM agents WHERE slot_id=?", (slot,))[0]
    check("it is an attached agent with no I/O of its own",
          agent["provenance"] == "attached" and agent["push_capable"] == 0
          and agent["io_kind"] is None, str(agent))

    print("\n=== 2. a message to it has nowhere to go ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Hand", "text": "before the adoption"})
    time.sleep(7)
    check("it is not typed anywhere", "before the adoption" not in read(HANDLOG), read(HANDLOG)[-300:])
    check("and it waits in the queue",
          bool(q("SELECT 1 FROM messages WHERE body LIKE '%before the adoption%' AND acked_at_ms IS NULL")))

    print("\n=== 3. adopt the pane it is sitting in ===")
    s, j = http("POST", f"/v1/sessions/{sid}/adopt",
        {"node": "machine-b", "tmux_target": "Hand", "name": "HandTerm"})
    check("adopt accepted", s == 200 and j.get("slot_id"), str(j))
    linked = until(lambda: (q("SELECT terminal_via, terminal_cli_pid FROM slots WHERE id=?",
                              (slot,)) or [{}])[0].get("terminal_via"), seconds=40)
    check("the node noticed the CLI is in that pane, and said so", bool(linked),
          open(f"{T}/node.log").read()[-800:])
    row = q("SELECT terminal_via,terminal_cli_pid,terminal_cli_ticks FROM slots WHERE id=?", (slot,))[0]
    check("it recorded the CLI's identity, not the shim's",
          bool(row["terminal_cli_pid"]) and bool(row["terminal_cli_ticks"]), str(row))
    shimpid = q("SELECT pid FROM agents WHERE slot_id=?", (slot,))[0]["pid"]
    check("the recorded pid is the CLI, which is the shim's parent",
          row["terminal_cli_pid"] != shimpid, f"cli={row['terminal_cli_pid']} shim={shimpid}")

    print("\n=== 3b. adopting the same terminal twice is one terminal ===")
    # Every adopt used to mint a fresh seat, so re-running it after a reboot
    # -- or because you were not sure the first one took -- left a row of
    # `...Term` twins pointing at one pane, all but one of them dead.
    before = len(q("SELECT id FROM slots WHERE session_id=? AND cli_id='tmux'", (sid,)))
    s, j2 = http("POST", f"/v1/sessions/{sid}/adopt",
        {"node": "machine-b", "tmux_target": "Hand", "name": "HandTerm"})
    after = q("SELECT id FROM slots WHERE session_id=? AND cli_id='tmux'", (sid,))
    check("the second adopt says it reused the seat", j2.get("reused") is True, str(j2))
    check("and no twin seat appeared", len(after) == before, str(after))
    check("it is the same seat as the first adopt", j2.get("slot_id") == j.get("slot_id"),
          f"{j2.get('slot_id')} vs {j.get('slot_id')}")

    print("\n=== 4. now the queued message is typed at it ===")
    got = until(lambda: "before the adoption" in read(HANDLOG), seconds=40)
    check("the message that had nowhere to go arrived", bool(got), read(HANDLOG)[-400:])
    check("the CLI read it as one line", 
          len([l for l in read(HANDLOG).splitlines() if "before the adoption" in l]) == 1,
          read(HANDLOG)[-400:])
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%before the adoption%'")
                           or [{}])[0].get("acked_at_ms"))
    check("and the hub stopped holding it in flight", bool(acked), str(q("SELECT * FROM messages")))

    print("\n=== 5. a fresh message goes the same way, in seconds ===")
    t0 = time.time()
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Hand", "text": "second one after linking"})
    got = until(lambda: "second one after linking" in read(HANDLOG), seconds=30)
    check(f"typed without any tool call ({time.time()-t0:.1f}s)", bool(got), read(HANDLOG)[-300:])

    print("\n=== 6. the other order: register into a pane already adopted ===")
    sid2 = http("POST", "/v1/sessions", {"name": "termlink2"})[1]["session_id"]
    hand_session("Hand2", HANDLOG2, f"{T}/shim2.log", sid2)
    seat2 = until(lambda: (q("SELECT id, session_id FROM slots WHERE display_name='Hand2'") or [None])[0],
                  seconds=40)
    check("the second CLI took a seat", seat2 is not None)
    if seat2:
        s, j = http("POST", f"/v1/sessions/{seat2['session_id']}/adopt",
            {"node": "machine-b", "tmux_target": "Hand2", "name": "Hand2Term"})
        link2 = until(lambda: (q("SELECT terminal_via FROM slots WHERE id=?", (seat2["id"],))
                               or [{}])[0].get("terminal_via"), seconds=40)
        check("it links too, whichever happened first", bool(link2),
              open(f"{T}/node.log").read()[-600:])

    print("\n=== 7. the CLI dies: nothing is typed into the shell it leaves behind ===")
    # The CLI exits, leaving the pane at a shell prompt. This is the case the
    # whole design is guarding: a message typed there would be EXECUTED.
    tmux("send-keys", "-t", "Hand2", "quit", "Enter")
    time.sleep(2.0)
    before = read(HANDLOG2)
    http("POST", f"/v1/sessions/{seat2['session_id']}/messages",
         {"to": "Hand2", "text": "rm -rf would run here"})
    time.sleep(9)
    check("nothing more reached the dead CLI", read(HANDLOG2) == before, read(HANDLOG2)[-300:])
    pane = tmux("capture-pane", "-p", "-t", "Hand2")
    check("and nothing was typed into the pane it left",
          "rm -rf would run here" not in pane, pane[-400:])
    nodelog = open(f"{T}/node.log").read()
    check("the node said why it refused",
          "has gone" in nodelog or "not in the pane" in nodelog
          or "not an adopted terminal" in nodelog, nodelog[-600:])
    check("the message is still in the queue, not lost",
          bool(q("SELECT 1 FROM messages WHERE body LIKE '%rm -rf would run here%' "
                 "AND acked_at_ms IS NULL")))

    print("\n=== 7b. the seat's own name opens the terminal it is sitting in ===")
    # "I do not want to use tmux, I want to use nexmesh." A CLI started by
    # hand has no pty of its own -- the pty belongs to the adopted pane
    # around it, which is a seat of its own -- so attaching by the name you
    # address the agent BY was refused, and you had to know the pane's seat
    # name or go and find the tmux session. That is the plumbing showing
    # through.
    s, rj = http("GET", f"/v1/sessions/{sid}/resolve/Hand")
    check("resolving the seat answers with the terminal it is in",
          s == 200 and rj.get("via") == "HandTerm", str(rj)[:300])
    check("and says which agent that is, not the seat's own shim",
          rj.get("agent_id") != rj.get("seat_agent_id")
          and bool(rj.get("seat_agent_id")), str(rj)[:300])
    check("while keeping the name you asked for", rj.get("name") == "Hand", str(rj)[:200])
    check("and reporting a terminal rather than nothing",
          rj.get("io_kind") in ("pty", "tmux"), str(rj)[:200])
    st = http("GET", f"/v1/sessions/{sid}/status")[1]["agents"]
    row = [a for a in st if a["slot_id"] == slot][0]
    check("status carries the same answer, for the console and the dashboard",
          bool(row.get("attach_agent_id")) and row["attach_agent_id"] != row["agent_id"],
          str(row)[:300])

    print("\n=== 8. and it shows up where you would look ===")
    st = http("GET", f"/v1/sessions/{sid}/status")[1]["agents"]
    row = [a for a in st if a["slot_id"] == slot][0]
    check("status says the seat is typed through a terminal",
          row.get("io_kind") == "tmux" and row.get("typed_via_terminal") is True, str(row))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL TERMINAL LINK CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
