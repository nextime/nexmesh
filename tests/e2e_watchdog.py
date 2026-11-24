#!/usr/bin/env python3
"""Looking after a seat that is filling up, or going round in circles.

Both are measured out of the CLI's own conversation file, so the fake agent
here writes real records: a test can fill a context to a known number of
tokens, or make an agent repeat one turn, and watch what the fleet does.

What has to be true:

  * BOTH ARE OFF until somebody turns them on. A hub that compacts seats, or
    pauses them, because it was upgraded is not something anybody chose.
  * A reading is taken and shown even with everything off, because "how full
    is that seat?" is worth answering whether or not anything acts on it.
  * A seat's own setting beats its team's, in both directions -- including a
    seat switched OFF inside a team that has it on, which is the first thing
    an operator reaches for.
  * Over the threshold, the CLI's OWN compaction command is typed at it.
  * An agent repeating itself is interrupted, PAUSED, and reported to the
    operator, with its conversation left alone.
  * Different arguments are different work. An agent reading forty files
    must never be mistaken for one in a loop.
"""
import json, os, shutil, signal, socket, sqlite3, subprocess, sys, tempfile, time
import uuid as _uuid
import urllib.request, urllib.error

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           "NEXMESH_AGENT_ID", "NEXMESH_SLOT", "NEXMESH_SESSION",
           "NEXMESH_CLI", "NEXMESH_ROLE", "NEXMESH_NAME", "NEXMESH_SOCK"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT = free()
HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-watchdog-")
DB, SOCK, WS = f"{T}/h.db", f"{T}/n.sock", f"{T}/ws"
PROJECTS = f"{T}/projects"
os.makedirs(WS)
os.makedirs(PROJECTS)
CLIS = f"{T}/clis"
os.makedirs(CLIS)
LOG = f"{T}/typed.log"

# The descriptor, with this run's temporary directories baked in: a suite
# must not read or write the real ~/.claude of whoever runs it.
_d = open(f"{SCR}/fake-cli/fakectx.toml").read()
_d = _d.replace('extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
_d = _d.replace('session_path = "/tmp/{cwd_slug}/{id}.jsonl"',
                f'session_path = "{PROJECTS}/{{cwd_slug}}/{{id}}.jsonl"')
_d += f'\n[cli.env]\nFAKECTX_LOG = "{LOG}"\nFAKECTX_PROJECTS = "{PROJECTS}"\n'
open(f"{CLIS}/fakectx.toml", "w").write(_d)

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None, tok=None):
    r = urllib.request.Request(HUB + p, data=json.dumps(b).encode() if b is not None else None,
                               method=m)
    r.add_header("content-type", "application/json")
    if tok:
        r.add_header("authorization", "Bearer " + tok)
    try:
        with urllib.request.urlopen(r, timeout=25) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def q(sql, args=()):
    """Read the hub's own record. Some of what this suite checks -- a seat's
    resume handle -- is not on any status surface, because nothing but a
    respawn reads it."""
    c = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    try:
        return [tuple(r) for r in c.execute(sql, args)]
    finally:
        c.close()


def until(fn, seconds=20.0, step=0.2):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception:
            pass
        time.sleep(step)
    return None


def fake_log():
    try:
        return open(LOG).read()
    except OSError:
        return ""


OPTOK = None
SID = None


def seat_row(name):
    s, j = http("GET", f"/v1/sessions/{SID}/status", tok=OPTOK)
    for a in j.get("agents") or []:
        if a.get("name") == name:
            return a
    return {}


def say(text):
    """Type a line at a seat, as the operator would."""
    s, j = http("POST", f"/v1/sessions/{SID}/messages", {"text": text}, tok=OPTOK)
    return s, j


def make_seat(name):
    s, j = http("POST", f"/v1/sessions/{SID}/slots", {
        "cli_id": "fakectx", "mode": "interactive", "display_name": name,
        "role": "engineer", "workspace": "local", "task": "fill up",
        "placement": {"node": "machine-b"}}, tok=OPTOK)
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn", tok=OPTOK)
    aid = until(lambda: http("GET", f"/v1/sessions/{SID}/resolve/{name}",
                             tok=OPTOK)[1].get("agent_id"))
    until(lambda: "FAKECTX READY" in (http(
        "GET", f"/v1/agents/{aid}/transcript?tail=4000", tok=OPTOK)[1].get("transcript") or ""))
    path = until(lambda: conv_of_last_spawn())
    assert path and path not in CONV.values(), f"no new conversation for {name}: {path}"
    CONV[name] = path
    return slot, aid


# Which file each seat's conversation is in, learnt when the seat starts.
CONV = {}


def conv_path(name):
    """The conversation file of the seat called `name`.

    Taken from what the CLI itself said at startup, not from the hub: the hub
    does not record a conversation until the file EXISTS -- deliberately, so
    a start that dies before writing anything cannot displace a good handle
    -- and nothing writes this file until this test does. Asking the hub
    first is a deadlock, and the first version of this test resolved it by
    writing to a file called `None.jsonl` and passing anyway, because with
    one seat in the directory the node's "newest conversation here" fallback
    picked that file up.
    """
    return CONV[name]


def conv_of_last_spawn():
    """The conversation the most recently started fake CLI reported."""
    lines = [l for l in fake_log().splitlines() if "conversation: " in l]
    return lines[-1].split("conversation: ", 1)[1].strip() if lines else None


def append(name, record):
    with open(conv_path(name), "a") as f:
        f.write(json.dumps(record) + "\n")
        f.flush()


def turn(content, usage=None):
    rec = {"type": "assistant", "isSidechain": False,
           "message": {"role": "assistant", "model": "claude-opus-5", "content": content}}
    if usage is not None:
        rec["message"]["usage"] = usage
    return rec


def fill(name, tokens):
    """Write a turn whose prompt was that big, as the CLI would.

    The records are written by the TEST rather than by making the fake agent
    do it. Driving the fake would mean going through typed delivery, which
    moves on its own cadence -- forty seconds a message, measured -- and
    what is being tested here is what the node reads off the file and what
    the hub does about it, not how a message reaches a terminal. That path
    has its own suite.
    """
    append(name, turn(
        [{"type": "text", "text": f"turn {time.time():.4f} of a growing conversation"}],
        {"input_tokens": 10, "cache_creation_input_tokens": 0,
         "cache_read_input_tokens": max(int(tokens) - 10, 0), "output_tokens": 0}))


def loop_turns(name, n):
    """n IDENTICAL turns: the same tool, the same arguments."""
    for _ in range(n):
        append(name, turn([{"type": "tool_use", "name": "Bash",
                            "input": {"command": "make", "timeout": 30}}]))


def work_turns(name, n):
    """n turns of real progress: the same tool, different arguments."""
    for i in range(n):
        append(name, turn([{"type": "tool_use", "name": "Read",
                            "input": {"path": f"src/file{i}.rs"}}]))


def main():
    global OPTOK, SID
    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        print("FAILED (1): build first: cargo build --workspace")
        return 1
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "operator-token", "me"],
                         capture_output=True, text=True).stdout
    OPTOK = [w for l in out.splitlines() for w in l.split() if len(w) > 20][0]
    # NO fleet-wide thresholds passed, deliberately: the default is off, and
    # the first section proves it.
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        try:
            if http("GET", "/healthz")[1].get("ok"):
                break
        except Exception:
            pass
        time.sleep(0.2)
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", "machine-b",
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes", tok=OPTOK)
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "fakectx" in j["nodes"][0]["clis"]
            break
        time.sleep(0.25)
    check("node online with the transcript-writing test CLI", ok)
    if not ok:
        return 1

    SID = http("POST", "/v1/sessions", {"name": "watch", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]}, tok=OPTOK)[1]["session_id"]

    # ======================================================================
    print("\n=== 1. a reading is taken, and nothing is done about it ===")
    # The state every fleet is in the moment this hub is deployed: the
    # numbers are measured and shown, and no seat is touched.
    slot, aid = make_seat("Ida")
    check("the seat spawned and its agent registered", bool(aid))
    fill("Ida", 150000)
    row = until(lambda: seat_row("Ida").get("context_used_tokens") and seat_row("Ida"))
    check("the hub learns how full that context is", bool(row),
          json.dumps(seat_row("Ida"))[:300])
    if row:
        check("and to the token, read from the conversation file",
              row["context_used_tokens"] == 150000, str(row.get("context_used_tokens")))
        check("against the window the CLI's descriptor declares",
              row["context_limit_tokens"] == 200000, str(row.get("context_limit_tokens")))
        check("nothing is set on the seat, so it inherits",
              row.get("compact_at_pct") is None and row.get("loop_guard_repeats") is None,
              str((row.get("compact_at_pct"), row.get("loop_guard_repeats"))))
    # 75% of its window, which would be over most thresholds anybody would
    # set -- and nothing is typed at it, because nobody set one.
    time.sleep(3)
    check("NOTHING was typed at it: this is off until it is turned on",
          "/squash-now" not in fake_log(), fake_log()[-200:])

    # ======================================================================
    print("\n=== 2. over the threshold, its own command is typed at it ===")
    s, j = say("/compact-at Ida 60")
    check("the operator can set a seat's threshold", s == 200 and j.get("command") is True,
          str(j))
    check("and is told what it will now do", "60%" in str(j.get("said", "")), str(j))
    # It is already at 75%, so the next reading is over.
    fill("Ida", 150001)

    # BUT THE FOLD COMES FIRST. A compaction replaces a context with a
    # summary of itself, and the next one summarises the summary; a fold is
    # written somewhere that outlives the context. So the seat is asked to
    # fold, and nothing is typed at its terminal yet.
    # `q` here hands back TUPLES: (to_slot, body).
    asked = until(lambda: [r for r in q(
        "SELECT to_slot, body FROM messages ORDER BY id DESC LIMIT 6")
        if "fold_context" in (r[1] or "")], 20)
    check("the seat is asked to FOLD before anything is compacted", bool(asked),
          json.dumps(q("SELECT to_slot, body FROM messages ORDER BY id DESC LIMIT 3"))[:400])
    if asked:
        check("addressed to that seat, not said to the whole room",
              asked[0][0] == slot, f"{asked[0][0]} vs {slot}")
        body = asked[0][1]
        check("told plainly that a compaction is coming, so it knows why now",
              "about to be asked to compact" in body, body[:200])
        check("and told what to keep, or it keeps the story and loses the facts",
              "line numbers" in body and "no longer needs it" in body, body[:400])
    check("and NOTHING is typed at it while that fold is outstanding",
          "/squash-now" not in fake_log(), fake_log()[-200:])
    # It folds, as an agent reading that message would. Writing the row is
    # standing in for the tool call: what is being checked here is that the
    # compaction waits for a fold and then goes ahead, not how the tool is
    # dispatched -- `tests/e2e_folds.py` covers that end of it.
    c = sqlite3.connect(DB)
    c.execute("INSERT INTO folds (id, scope, scope_ref, tier, topic, summary, detail, "
              "covers, author_slot, tokens_before, tokens_after, active, created_at_ms) "
              "VALUES ('fd-e2e-1','agent',?,1,'filling up','what it established',"
              "'the detail','[]',?,4000,40,1,?)",
              (slot, slot, int(time.time() * 1000)))
    c.commit()
    c.close()
    fill("Ida", 150002)
    # Longer than the usual wait on purpose. On a seat that is TYPED at, the
    # fold ask is itself typed into the terminal, at the pace a person types
    # -- so the compaction keystroke queues behind a message of several
    # hundred characters. Measured at around forty seconds on this harness.
    typed = until(lambda: "/squash-now" in fake_log(), 90)
    check("the CLI's OWN compaction command was typed at it", bool(typed),
          fake_log()[-300:])
    check("the seat was told why, in its own scrollback",
          until(lambda: "compact" in (http(
              "GET", f"/v1/agents/{aid}/transcript?tail=6000",
              tok=OPTOK)[1].get("transcript") or "").lower()))
    check("and it says it was nexmesh and not a person at a keyboard",
          until(lambda: "nexmesh (context" in (http(
              "GET", f"/v1/agents/{aid}/transcript?tail=6000",
              tok=OPTOK)[1].get("transcript") or "")),
          "")
    # Asked once, not once per reading: compaction takes a while and every
    # reading during it is still over.
    typed_count = fake_log().count("/squash-now")
    fill("Ida", 150003)
    fill("Ida", 150004)
    time.sleep(3)
    check("and asked ONCE, not once per reading",
          fake_log().count("/squash-now") == typed_count,
          f"{fake_log().count('/squash-now')} vs {typed_count}")

    # ======================================================================
    print("\n=== 3. a compacted seat reads as empty again ===")
    # What makes the number occupancy rather than spending: it goes DOWN.
    fill("Ida", 9000)
    check("occupancy falls when the conversation does",
          until(lambda: seat_row("Ida").get("context_used_tokens") == 9000),
          str(seat_row("Ida").get("context_used_tokens")))

    # ======================================================================
    print("\n=== 4. a seat can opt out of a setting its team has on ===")
    slot2, aid2 = make_seat("Bob")
    s, j = say("/compact-at session 50")
    check("a threshold can be set for the whole team", s == 200, str(j))
    s, j = say("/compact-at Bob off")
    check("and one seat can be switched off inside it", s == 200, str(j))
    check("which it says plainly", "never" in str(j.get("said", "")), str(j))
    before = fake_log().count("/squash-now")
    fill("Bob", 190000)
    check("the reading is still taken for that seat",
          until(lambda: seat_row("Bob").get("context_used_tokens") == 190000),
          str(seat_row("Bob").get("context_used_tokens")))
    time.sleep(2)
    check("but nothing is typed at it, 95% full and its team set to 50%",
          fake_log().count("/squash-now") == before,
          f"{fake_log().count('/squash-now')} vs {before}")

    # ======================================================================
    print("\n=== 5. an agent going round in circles is stopped ===")
    slot3, aid3 = make_seat("Cal")
    s, j = say("/loop-guard Cal 4")
    check("the loop guard can be set on a seat", s == 200 and j.get("command") is True,
          str(j))
    esc_before = fake_log().count("ESC")
    loop_turns("Cal", 4)
    check("it was interrupted -- a keystroke at a busy CLI, not a line",
          until(lambda: fake_log().count("ESC") > esc_before),
          fake_log()[-300:])
    check("its seat is paused, so nothing starts it again",
          until(lambda: seat_row("Cal").get("paused") is True),
          str(seat_row("Cal").get("paused")))
    check("the cycle is recorded: one turn, four times round",
          until(lambda: seat_row("Cal").get("loop_period") == 1
                and (seat_row("Cal").get("loop_repeats") or 0) >= 4),
          str((seat_row("Cal").get("loop_period"), seat_row("Cal").get("loop_repeats"))))
    # And the operator is told, because a paused seat nobody knows about is
    # a team member who has silently left.
    # The operator inbox calls the body `text`, not `body`: this is the
    # channel a person reads, not the team's chat.
    told = until(lambda: [
        m for m in (http("GET", f"/v1/operator/messages?session={SID}",
                         tok=OPTOK)[1].get("messages") or [])
        if "Cal" in str(m.get("text", ""))
    ])
    check("and the operator is told, with what to do about it", bool(told),
          json.dumps(http("GET", f"/v1/operator/messages?session={SID}",
                          tok=OPTOK)[1])[:300])
    if told:
        check("naming the setting that stopped it, so it can be changed",
              "loop guard" in told[0]["text"], told[0]["text"][:200])
    check("its conversation is left intact",
          until(lambda: any(f.endswith(".jsonl")
                            for d, _, fs in os.walk(PROJECTS) for f in fs)))

    # ======================================================================
    print("\n=== 6. work that merely repeats a tool is not a loop ===")
    slot4, aid4 = make_seat("Dee")
    s, j = say("/loop-guard Dee 3")
    check("the guard is on for this seat too", s == 200, str(j))
    esc_before = fake_log().count("ESC")
    work_turns("Dee", 9)
    time.sleep(4)
    check("nine reads of nine different files is not a loop",
          seat_row("Dee").get("paused") is not True
          and fake_log().count("ESC") == esc_before,
          str(seat_row("Dee")))

    # ======================================================================
    print("\n=== 7. a seat with the guard off is left to repeat itself ===")
    slot5, aid5 = make_seat("Eve")
    s, j = say("/loop-guard session 3")
    check("the guard can be set for the whole team", s == 200, str(j))
    s, j = say("/loop-guard Eve off")
    check("and one seat can be left out of it", s == 200, str(j))
    esc_before = fake_log().count("ESC")
    loop_turns("Eve", 8)
    time.sleep(4)
    check("a seat that is MEANT to repeat itself is not stopped",
          seat_row("Eve").get("paused") is not True
          and fake_log().count("ESC") == esc_before,
          str(seat_row("Eve")))
    check("though its repeating is still recorded, for somebody to look at",
          until(lambda: (seat_row("Eve").get("loop_repeats") or 0) >= 3, 20),
          str(seat_row("Eve").get("loop_repeats")))

    # ======================================================================
    print("\n=== 8. the settings survive a hub restart ===")
    # They are a property of the seat, not of a running process.
    procs[0].send_signal(signal.SIGTERM)
    procs[0].wait(timeout=20)
    procs[0] = subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub2.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1"))
    check("the hub is back", until(lambda: http("GET", "/healthz")[1].get("ok")))
    row = seat_row("Bob")
    check("a seat switched off is still off", row.get("compact_at_pct") == 0, str(row))
    check("and the team's setting is still there",
          until(lambda: http("GET", f"/v1/sessions/{SID}", tok=OPTOK)[1].get("compact_at_pct")
                == 50, 10)
          or http("GET", f"/v1/sessions/{SID}", tok=OPTOK)[1].get("compact_at_pct") == 50,
          str(http("GET", f"/v1/sessions/{SID}", tok=OPTOK)[1].get("compact_at_pct")))

    # ======================================================================
    print("\n=== 9. a CLI that took a seat by itself is measured too ===")
    # Several seats on a real fleet are hand-run CLIs that joined a team
    # rather than processes the node started. Such a seat was unmeasurable:
    # the node recorded no CLI id for it, an empty id matches no descriptor,
    # and nothing then knew where this CLI keeps its conversations. On the
    # live fleet that was the one claude seat with no reading.
    # After the hub restart above, the node reconnects on a backoff. A shim
    # that registers in that window is refused -- "not connected to the hub"
    # -- which is correct and not what this section is about.
    check("the node is back with the hub",
          bool(until(lambda: (http("GET", "/v1/nodes", tok=OPTOK)[1].get("nodes") or [{}])[0]
                     .get("connected"), 30)))
    conv = str(_uuid.uuid4())
    shim = subprocess.Popen(
        [f"{BIN}/nexmesh-mcpd"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=open(f"{T}/handshim.log", "a"), cwd=WS, text=True, bufsize=1,
        # No NEXMESH_AGENT_ID: nobody spawned this, so it asks the hub for a
        # seat of its own -- exactly what a CLI started by hand does.
        env=dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_CLI="fakectx",
                 NEXMESH_SESSION=SID, NEXMESH_NAME="Handy",
                 CLAUDE_CODE_SESSION_ID=conv))
    procs.append(shim)
    check("the hand-run CLI took a seat", bool(until(lambda: seat_row("Handy"))),
          "no seat called Handy")
    CONV["Handy"] = f"{PROJECTS}/{WS.replace('/', '-')}/{conv}.jsonl"
    fill("Handy", 120000)
    check("and its context is read, against the window of the CLI it says it is",
          until(lambda: seat_row("Handy").get("context_used_tokens") == 120000),
          str(seat_row("Handy").get("context_used_tokens")))
    check("with the window from that CLI's descriptor",
          seat_row("Handy").get("context_limit_tokens") == 200000,
          str(seat_row("Handy").get("context_limit_tokens")))

    # ======================================================================
    print("\n=== 10. a seat's recorded conversation is corrected from the file ===")
    # The other half of conversation drift. On the live fleet five seats had
    # the hub believing a conversation they were not in: one that had not
    # been written since the previous evening, and one that was never written
    # at all. Measuring was fixed by reading the right file; this is the hub's
    # RECORD, which is what a respawn resumes -- so a drifted seat came back
    # on an old conversation, or on none.
    slot6, aid6 = make_seat("Fred")
    # The seat has NO recorded conversation, and that is the interesting
    # precondition rather than a gap in the test: the node names one at spawn
    # but will not record it until the file exists, precisely so a start that
    # dies cannot displace a good handle. This fake never writes the file it
    # was named -- which is exactly the live case, where a seat's recorded id
    # pointed at a conversation nobody had ever written.
    recorded = q("SELECT resume_handle FROM slots WHERE id = ?", (slot6,))[0][0]
    check("the seat has no usable conversation recorded yet", recorded is None,
          str(recorded))
    # What it is really in is a conversation carrying its own handshake,
    # under an id the hub has never heard of.
    real = str(_uuid.uuid4())
    really_in = f"{PROJECTS}/{WS.replace('/', '-')}/{real}.jsonl"
    with open(really_in, "w") as f:
        f.write(json.dumps({
            "type": "system",
            "content": f"You are part of a nexmesh team (slot {slot6}). "
                       "Teammates may run on other machines.",
        }) + "\n")
    CONV["Fred"] = really_in
    fill("Fred", 70000)
    check("the hub is told which conversation it is REALLY in",
          until(lambda: q("SELECT resume_handle FROM slots WHERE id = ?", (slot6,))[0][0]
                == real, 45),
          str(q("SELECT resume_handle FROM slots WHERE id = ?", (slot6,))))
    check("and it is measured from that same file",
          until(lambda: seat_row("Fred").get("context_used_tokens") == 70000, 45),
          str(seat_row("Fred").get("context_used_tokens")))
    # Bound to the PLACE as well, which is what a respawn on this machine
    # actually reads.
    bound = q("SELECT handle FROM slot_conversations WHERE slot_id = ?", (slot6,))
    check("and bound to the machine and directory it lives in",
          bool(bound) and bound[0][0] == real, str(bound))
    # A seat whose own record is RIGHT and whose hub record is stale is told
    # about too. This is not the same question, and treating it as one left
    # a hand-run seat on the live fleet with a hub handle two days old that
    # nothing ever corrected -- the node saw no disagreement to report.
    slot7, aid7 = make_seat("Gus")
    g_real = str(_uuid.uuid4())
    g_path = f"{PROJECTS}/{WS.replace('/', '-')}/{g_real}.jsonl"
    with open(g_path, "w") as f:
        f.write(json.dumps({
            "type": "system",
            "content": f"You are part of a nexmesh team (slot {slot7}).",
        }) + "\n")
    CONV["Gus"] = g_path
    fill("Gus", 30000)
    check("a seat is told to the hub once per connection, not only on a change",
          # 45s, not the usual 20: this can only happen on the node's own
          # twenty-second pass, so a twenty-second wait is a coin toss on
          # where in that cycle the test lands.
          until(lambda: q("SELECT resume_handle FROM slots WHERE id = ?", (slot7,))[0][0]
                == g_real, 45),
          str(q("SELECT resume_handle FROM slots WHERE id = ?", (slot7,))))

    # Said once, not once every twenty seconds: the node remembers what it
    # has told the hub for each seat.
    said = open(f"{T}/node.log").read().count("telling the hub which conversation")
    time.sleep(2)
    check("corrected once, not on every pass",
          open(f"{T}/node.log").read().count("telling the hub which conversation") == said
          and said >= 2,
          f"{said} then "
          + str(open(f"{T}/node.log").read().count("telling the hub which conversation")))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails))
        return 1
    print("ALL WATCHDOG CHECKS PASSED")
    return 0


if __name__ == "__main__":
    code = 1
    try:
        code = main()
    finally:
        for p in procs:
            try:
                p.send_signal(signal.SIGTERM)
                p.wait(timeout=10)
            except Exception:
                try:
                    p.kill()
                except Exception:
                    pass
        if code == 0:
            shutil.rmtree(T, ignore_errors=True)
        else:
            print(f"  (logs: {T})")
    sys.exit(code)
