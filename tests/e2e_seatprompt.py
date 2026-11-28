#!/usr/bin/env python3
"""What a seat tells its agent, over and above whatever its CLI says.

Two separate things, and the difference is the point of this suite:

  * a SYSTEM PROMPT -- who the agent in this seat is. It goes to the CLI as a
    system prompt argument, appended to the CLI's own or replacing it, and it
    lasts: every agent that ever fills the seat gets it, including a respawn
    at 4am that nobody watched.
  * an INSTRUCTION -- what to do on arrival. It is said to each NEW occupant
    as part of the first thing it hears.

So the checks here are not "the hub stored it". They are: the flag and the
text are on the CLI's own command line (read from /proc, not from the hub's
account of itself), the mode decides WHICH flag, clearing it takes the flag
away again, and a CLI that declares no such argument says so in the chat
rather than starting without one and looking like it worked.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17913; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-seatprompt-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
WORK = f"{T}/work"; os.makedirs(WORK)
TYPED = f"{T}/typed.log"      # what the CLI itself read, line by line
os.environ.setdefault("NEXMESH_IDENTITY", "/nonexistent/nexmesh-identity")
procs = []; fails = []
TOK = SID = None

def cleanup():
    print(f"  (logs: {T})")
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

def until(fn, seconds=30.0, step=0.25):
    end = time.time() + seconds
    while time.time() < end:
        try:
            if fn(): return True
        except Exception: pass
        time.sleep(step)
    return False

def seat(slot):
    for a in http("GET", f"/v1/sessions/{SID}/status")[1].get("agents", []):
        if a["slot_id"] == slot: return a
    return None

def cmdline(pid):
    """What the CLI was ACTUALLY started with, from the kernel's copy."""
    with open(f"/proc/{pid}/cmdline", "rb") as f:
        return f.read().decode("utf8", "replace").split("\0")

def make_slot(cli, name):
    return http("POST", f"/v1/sessions/{SID}/slots", {
        "cli_id": cli, "mode": "interactive", "display_name": name,
        "placement": {"node": "machine-b"}, "cwd": WORK})[1]["slot_id"]

def start(slot):
    s, j = http("POST", f"/v1/slots/{slot}/spawn", {})
    assert s == 200, f"spawn: {s} {j}"
    assert until(lambda: seat(slot) and seat(slot).get("pid")), f"no pid for {slot}"
    return seat(slot)["pid"]

def stop(slot):
    """Take the process away so the seat can be started again."""
    pid = (seat(slot) or {}).get("pid")
    if pid:
        try: os.kill(pid, signal.SIGKILL)
        except ProcessLookupError: pass
    until(lambda: not (seat(slot) or {}).get("pid")
                  or (seat(slot) or {}).get("status") in ("exited", "disconnected"), 30)

def main():
    global TOK, SID

    # A CLI that takes both kinds of system prompt argument, under names
    # nothing else uses: the check is that the one the MODE asks for is the
    # one that appears, so they have to be told apart on a command line.
    with open(f"{CLIS}/promptcli.toml", "w") as f:
        f.write(f'''
# A CLI that can be TYPED at, so what it was told on arrival is observable at
# the CLI itself and not only in the hub's account of it.
[cli]
id = "promptcli"
display_name = "Prompt CLI"
command = "faketyped"
version_args = ["--version"]
extra_paths = ["{SCR}/fake-cli"]

[cli.env]
FAKETYPED_LOG = "{TYPED}"

[modes.interactive]
args = []
attachable = true
system_prompt_args = ["--system-sp", "{{prompt}}"]
append_system_prompt_args = ["--append-sp", "{{prompt}}"]

[driver]
kind = "stream_json"
replay_user_messages = true
''')
    # And one that takes neither, which must SAY so rather than start without.
    with open(f"{CLIS}/mutecli.toml", "w") as f:
        f.write(f'''
[cli]
id = "mutecli"
display_name = "Mute CLI"
command = "faketui"
version_args = ["--version"]
extra_paths = ["{SCR}/fake-cli"]

[modes.interactive]
args = []
attachable = true
''')

    TOK = [l.split()[1] for l in subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
        capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}",
        # The fleet's standing habits ride on every seat's system prompt
        # (see section 5b). Off here so the checks below see the seat's OWN
        # text alone; 5b switches them on for one seat.
        "--no-remind-memory", "--no-remind-graph", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("hub is up", until(lambda: http("GET", "/healthz")[1].get("ok")))

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=TOK, RUST_LOG="info", NO_COLOR="1")))
    check("node is online", until(lambda: (http("GET", "/v1/nodes")[1].get("nodes") or [{}])[0]
                                          .get("connected"), 40))

    SID = http("POST", "/v1/sessions", {"name": "prompttest"})[1]["session_id"]

    print("\n=== 1. a seat starts with nothing of its own ===")
    slot = make_slot("promptcli", "Pinned")
    s, j = http("GET", f"/v1/slots/{slot}/prompt")
    check("reading it back works", s == 200, f"{s} {j}")
    check("and there is nothing set", j.get("system_prompt") is None
          and j.get("instructions") is None, json.dumps(j))
    check("the default mode is append", j.get("system_prompt_mode") == "append", json.dumps(j))

    print("\n=== 2. the text and the flag reach the CLI ===")
    SP = "You are the reviewer for this team."
    s, j = http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt": SP})
    check("setting it works", s == 200 and j.get("system_prompt") == SP, f"{s} {j}")
    check("it says the next agent gets it",
          any("next agent started" in x for x in j.get("takes_effect") or []), json.dumps(j))
    argv = cmdline(start(slot))
    check("the CLI was given --append-sp", "--append-sp" in argv, " ".join(argv))
    check("with the seat's own text", SP in argv, " ".join(argv))
    check("and NOT the replacing flag", "--system-sp" not in argv, " ".join(argv))

    print("\n=== 3. replace asks for the other flag, not both ===")
    stop(slot)
    s, j = http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt_mode": "replace"})
    check("the mode changed", j.get("system_prompt_mode") == "replace", f"{s} {j}")
    argv = cmdline(start(slot))
    check("now it is --system-sp", "--system-sp" in argv, " ".join(argv))
    check("and the appending flag is gone", "--append-sp" not in argv, " ".join(argv))
    check("the text is still the seat's", SP in argv, " ".join(argv))

    print("\n=== 4. an unknown mode is refused, not stored ===")
    s, j = http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt_mode": "overwrite"})
    check("it is refused", "not a system prompt mode" in str(j.get("error", "")), f"{s} {j}")
    check("and the seat still says replace",
          http("GET", f"/v1/slots/{slot}/prompt")[1].get("system_prompt_mode") == "replace")

    print("\n=== 5. clearing it takes the flag away again ===")
    stop(slot)
    s, j = http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt": ""})
    check("it is gone", j.get("system_prompt") is None, f"{s} {j}")
    argv = cmdline(start(slot))
    check("and the CLI is started with neither flag",
          "--system-sp" not in argv and "--append-sp" not in argv, " ".join(argv))

    print("\n=== 5b. the team's habits are added after the seat's own text ===")
    stop(slot)
    http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt_mode": "append"})
    s, j = http("POST", f"/v1/slots/{slot}/switch", {"name": "remind_memory", "on": True})
    check("memory reminders switched on for the seat", s == 200, f"{s} {j}")
    argv = cmdline(start(slot))
    check("a seat with no prompt of its own is given the habits by appending",
          "--append-sp" in argv and any("`recall`" in a for a in argv), " ".join(argv))
    stop(slot)
    http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt": SP})
    argv = cmdline(start(slot))
    joined = [a for a in argv if SP in a]
    check("with a prompt of its own, that comes first and the habits after",
          bool(joined) and joined[0].startswith(SP) and "`recall`" in joined[0], " ".join(argv))
    stop(slot)
    http("POST", f"/v1/slots/{slot}/switch", {"name": "remind_memory", "on": False})
    http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt": ""})

    print("\n=== 6. the status says what is set, without the text ===")
    http("POST", f"/v1/slots/{slot}/prompt", {"system_prompt": SP, "instructions": "Read the diff."})
    a = seat(slot)
    check("the system prompt's size is in the status", a.get("system_prompt_bytes") == len(SP),
          json.dumps(a))
    check("so is the instruction's", a.get("instruction_bytes") == len("Read the diff."),
          json.dumps(a))
    # Deliberately NOT the text: a status poll four times a second must not
    # carry a seat's prompt with it.
    check("but the text itself is not", "system_prompt" not in a, json.dumps(a))

    print("\n=== 7. the instruction reaches a NEW occupant ===")
    instr = "TODAY-ONLY-MARKER: start with crates/node."
    fresh = make_slot("promptcli", "Told")
    http("POST", f"/v1/slots/{fresh}/prompt", {"instructions": instr})
    pid = start(fresh)
    def typed():
        if not os.path.exists(TYPED): return False
        t = open(TYPED).read()
        return "TODAY-ONLY-MARKER" in t and "STANDING INSTRUCTION" in t
    _ = pid
    # Read from the CLI's OWN log: that the hub stored an instruction proves
    # nothing about whether the agent was ever told it.
    check("the CLI itself read it", until(typed, 30),
          (open(TYPED).read()[-600:] if os.path.exists(TYPED) else "no log"))
    # It is an instruction to an ARRIVING agent, not a system prompt, so it
    # must not also be on the command line.
    argv = cmdline(pid)
    check("and not on the command line", instr not in argv, " ".join(argv))

    print("\n=== 8. a CLI that takes no such argument says so ===")
    mute = make_slot("mutecli", "Mute")
    http("POST", f"/v1/slots/{mute}/prompt", {"system_prompt": "be brief"})
    start(mute)
    def noted():
        for m in http("GET", f"/v1/sessions/{SID}/log?limit=200")[1].get("messages", []):
            body = m.get("body") or ""
            if "takes no" in body and "mutecli" in body:
                return True
        return False
    check("the chat is told it is running WITHOUT it", until(noted, 30))

    print("\n=== 9. a seat that does not exist ===")
    s, j = http("POST", "/v1/slots/sl-nope/prompt", {"system_prompt": "x"})
    check("is refused by name", "no seat" in str(j.get("error", "")), f"{s} {j}")

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL SEAT-PROMPT CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
