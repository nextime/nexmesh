#!/usr/bin/env python3
"""Folds: context that outlives the window it was learned in.

A long-lived agent spends its context twice -- once working, and then again
carrying what it did. A CLI's own compaction answers the second cost by
summarising its context into its context, where the next compaction
summarises the summary; four generations of that remember the shape of the
work and none of its facts, and none of it survives a restart on a CLI that
cannot resume.

A fold is the alternative: the agent writes down what a stretch of work
established, nexmesh keeps the raw material out of its context, and hands it
back in full whenever anything asks. These checks are about the bargain
holding -- that what was put away is findable, readable, private to the seat
that wrote it unless it said otherwise, and that a tier of folds over folds
keeps a long job down to a handful of summaries.

The design is billion-context's, reimplemented rather than imported; its own
form is a proxy in front of the model API, and nexmesh needs none because it
is already the MCP server inside every seat.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run from inside one. Inherited, every shim here would
# claim one seat.
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           "NEXMESH_AGENT_ID", "NEXMESH_SLOT", "NEXMESH_SESSION",
           "NEXMESH_CLI", "NEXMESH_ROLE", "NEXMESH_NAME", "NEXMESH_SOCK"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
T = tempfile.mkdtemp(prefix="nexmesh-folds-")
DB = f"{T}/h.db"


def free():
    import socket
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT = free()
HUB = f"http://127.0.0.1:{PORT}"

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}"
          + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None, token=None):
    r = urllib.request.Request(
        HUB + p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token:
        r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=20) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def until(fn, seconds=20.0, step=0.3):
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


def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a],
                          capture_output=True, text=True)


def ptoken(session, label):
    out = hub_cli("participant-token", label, "--session", session).stdout
    return [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]


def join(session, token, name):
    s, j = http("POST", "/v1/participant/join",
                {"session": session, "name": name, "role": "engineer",
                 "kind_hint": "test"}, token=token)
    assert "agent_id" in j, j
    return j["agent_id"], j["slot_id"]


def tool(agent, token, name, args):
    s, j = http("POST", "/v1/participant/tool",
                {"agent_id": agent, "tool": name, "args": args}, token=token)
    return j


def text(j):
    return j.get("text", "") or str(j)


def fold_id(j):
    """The id out of 'Folded as fd-... (tier 1, agent memory).'"""
    t = text(j)
    assert "Folded as " in t, t
    return t.split("Folded as ", 1)[1].split()[0]


def main():
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    if not until(lambda: http("GET", "/healthz")[1].get("ok")):
        print("FAILED: the hub did not come up")
        return 1

    sid = http("POST", "/v1/sessions", {"name": "foldtest"})[1]["session_id"]
    a_tok = ptoken(sid, "worker")
    b_tok = ptoken(sid, "other")
    worker, worker_slot = join(sid, a_tok, "Worker")
    other, _other_slot = join(sid, b_tok, "Other")

    print("=== what is folded is out of context, not out of reach ===")
    detail = ("psql output, batch after batch:\n"
              + "BEGIN; UPDATE claims SET tenant_id = tenants.id; COMMIT;\n" * 300
              + "the column is tenant_id, added in migrations/0041.sql:12")
    j = tool(worker, a_tok, "fold_context", {
        "topic": "the tenant migration",
        "summary": "tenant_id added in migrations/0041.sql:12; the backfill is idempotent "
                   "and runs in batches of 5000",
        "detail": detail})
    check("a stretch of work can be folded", "Folded as fd-" in text(j), text(j))
    fid = fold_id(j)
    check("and it says what that bought", "tokens of detail now cost you" in text(j), text(j))
    j = tool(worker, a_tok, "unfold_context", {"id": fid})
    body = text(j)
    check("the detail comes back", "psql output, batch after batch" in body, body[:200])
    check("all of it, not a sample", body.count("BEGIN; UPDATE claims") == 300,
          str(body.count("BEGIN; UPDATE claims")))
    check("and the summary with it", "migrations/0041.sql:12" in body, body[:200])

    print("\n=== found by its words, not by remembering an id ===")
    j = tool(worker, a_tok, "search_context", {"search": "idx_nothing_like_this"})
    check("a miss says so plainly", "Nothing folded mentions" in text(j), text(j))
    j = tool(worker, a_tok, "search_context", {"search": "migrations/0041.sql"})
    check("a word from the SUMMARY finds it", fid in text(j), text(j))
    j = tool(worker, a_tok, "search_context", {"search": "UPDATE claims SET tenant_id"})
    check("and a word from the DETAIL finds it too -- which is the point",
          fid in text(j), text(j))

    print("\n=== a fold belongs to the seat that wrote it ===")
    j = tool(other, b_tok, "unfold_context", {"id": fid})
    check("a teammate cannot read another seat's working context",
          "private context" in str(j), str(j)[:200])
    check("and it is refused rather than answered empty", not j.get("ok", True), str(j)[:160])
    j = tool(worker, a_tok, "fold_context", {
        "topic": "the api we settled on",
        "summary": "POST /v1/claims takes a batch of up to 500",
        "detail": "the whole argument, with the two options we rejected",
        "scope": "session"})
    tid = fold_id(j)
    check("a fold meant for the team says so", "session memory" in text(j), text(j))
    j = tool(other, b_tok, "unfold_context", {"id": tid})
    check("and every teammate can read that one",
          "two options we rejected" in text(j), text(j)[:200])

    print("\n=== folds fold into folds ===")
    ids = []
    for day in ("monday", "tuesday", "wednesday"):
        j = tool(worker, a_tok, "fold_context", {
            "topic": f"{day}'s work",
            "summary": f"what {day} established",
            "detail": f"the long version of {day}, " + "x" * 3000})
        ids.append(fold_id(j))
    j = tool(worker, a_tok, "fold_context", {
        "topic": "the week",
        "summary": "three days of migration work, in one line",
        "covers": ids})
    check("a tier can be built over them", "tier 2" in text(j), text(j))
    check("and it says what moved underneath it",
          "3 earlier fold(s) now sit underneath it" in text(j), text(j))
    top = fold_id(j)
    j = tool(worker, a_tok, "context_map", {})
    m = text(j)
    check("the map counts the parent, not the three children",
          "tier 2:" in m and "the week" in m, m[:400])
    check("and the children are not listed twice",
          m.count("monday's work") == 0, m[:400])
    j = tool(worker, a_tok, "unfold_context", {"id": top})
    down = text(j)
    check("unfolding the parent offers the way down",
          "covers 3 earlier fold(s)" in down, down[:300])
    check("naming each child, so a question descends only as far as it needs",
          all(i in down for i in ids), down[:300])
    j = tool(worker, a_tok, "unfold_context", {"id": ids[0]})
    check("and the detail is still there at the bottom",
          "the long version of monday" in text(j), text(j)[:200])

    print("\n=== a fold that cannot be made is refused, not half-made ===")
    j = tool(worker, a_tok, "fold_context", {"topic": "", "summary": "s"})
    check("a fold with no topic is refused", not j.get("ok", True), str(j)[:160])
    j = tool(worker, a_tok, "fold_context",
             {"topic": "t", "summary": "s", "covers": [ids[0]]})
    check("and a stretch of work is never put under two parents",
          "already been folded" in str(j), str(j)[:200])
    j = tool(worker, a_tok, "unfold_context", {"id": "fd-nope"})
    check("an id that does not exist says so, and says how to find a real one",
          "no fold 'fd-nope'" in str(j) and "search_context" in str(j), str(j)[:200])

    print("\n=== the rule travels with the map ===")
    # An agent reading how full it is has to know what to keep, or it writes
    # the narrative and loses the facts -- which is what a CLI's own
    # compaction does and the reason this exists.
    j = tool(worker, a_tok, "context_map", {})
    m = text(j)
    check("the map says when to fold", "no longer needs it" in m, m[-500:])
    check("and what to keep verbatim",
          "line numbers" in m and "exact error text" in m, m[-500:])
    check("and what to drop", "dead end" in m, m[-500:])
    check("a participant with no readable window is told so rather than a number",
          "cannot be read for this CLI" in m, m[:200])

    print("\n=== folding something tiny is pointed out rather than praised ===")
    j = tool(worker, a_tok, "fold_context", {
        "topic": "one line", "summary": "a summary longer than the thing it folded",
        "detail": "tiny"})
    check("a fold that costs more than it saved says so",
          "gains nothing" in text(j), text(j))

    print("\n=== an observer may read what the team put away ===")
    o_tok = hub_cli("participant-token", "watcher", "--session", sid, "--observer").stdout
    o_tok = [l.split()[1] for l in o_tok.splitlines() if l.startswith("token:")]
    if o_tok:
        watcher, _ = join(sid, o_tok[0], "Watcher")
        j = tool(watcher, o_tok[0], "unfold_context", {"id": tid})
        check("an observer can unfold the team's folds",
              "two options we rejected" in text(j), text(j)[:200])
        j = tool(watcher, o_tok[0], "fold_context", {"topic": "t", "summary": "s"})
        check("but never write one", not j.get("ok", True), str(j)[:200])
    else:
        print("  SKIP  this hub has no observer authority to test with")

    print("\n=== the operator can ask for a fold on any CLI ===")
    # A message rather than a keystroke, which is what makes this work on
    # every seat on a fleet -- including the ones whose conversation nexmesh
    # cannot read and whose fullness it therefore cannot measure.
    s_, j = http("POST", f"/v1/sessions/{sid}/messages", {"text": "/fold Worker"})
    check("the command is accepted", s_ == 200 and j.get("command") is True, str(j)[:200])
    check("and says what it asked for", "fold what it no longer needs" in str(j.get("said", "")),
          str(j)[:200])
    j = tool(worker, a_tok, "check_messages", {})
    got = text(j)
    check("the seat is actually asked, in words it can act on",
          "fold_context" in got and "no longer needs it" in got, got[:400])

    print("\n=== it survives the hub ===")
    for p in procs:
        p.send_signal(signal.SIGTERM)
    time.sleep(1.0)
    procs.clear()
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub2.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("the hub is back", bool(until(lambda: http("GET", "/healthz")[1].get("ok"))))
    j = tool(worker, a_tok, "unfold_context", {"id": fid})
    check("and the detail is still whole after a restart",
          text(j).count("BEGIN; UPDATE claims") == 300, str(text(j).count("BEGIN")))
    return 1 if fails else 0


rc = 1
try:
    rc = main()
finally:
    print(f"  (logs: {T})")
    for p in procs:
        try:
            p.send_signal(signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.3)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass

if fails:
    print(f"FAILED ({len(fails)}): " + "; ".join(fails))
    sys.exit(1)
if rc:
    sys.exit(rc)
print("ALL FOLD CHECKS PASSED")
