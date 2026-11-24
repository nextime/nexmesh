#!/usr/bin/env python3
"""Permission prompts, answered when nobody is attached -- and only then.

A fleet seat is driven rather than watched. When its CLI stops to ask whether
it may run a tool, the agent is finished until something answers: the hub's
next message is not delivered either, because the node refuses to type at a
screen waiting for a choice. So with no viewer attached the node answers the
prompt itself, takes the NARROWEST affirmative, and records what it did.

With a viewer attached the choice is the person's, and the node must keep its
hands off it. Both halves are checked here against a real agent whose dialog
waits to be answered, so the keystroke it received is observable.
"""
import json, os, shutil, signal, socket, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# The attached-viewer half of this suite needs a websocket client. Without it
# the suite says so and skips, rather than failing on an import: a machine
# missing a test dependency is not a broken fleet.
try:
    import websocket
except ModuleNotFoundError:
    websocket = None

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           # This seat's OWN identity: inherited when the suite is run
           # from inside a managed seat, and then every test's shim
           # registers as an agent the test's node has never heard of.
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
T = tempfile.mkdtemp(prefix="nexmesh-prompts-")
DB, SOCK, WS = f"{T}/h.db", f"{T}/n.sock", f"{T}/ws"
os.makedirs(WS)
CLIS = f"{T}/clis"
os.makedirs(CLIS)
LOG = f"{T}/typed.log"
_d = open(f"{SCR}/fake-cli/faketyped.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
# The dialog waits to be answered, and says what it was answered with.
_d += f'\n[cli.env]\nFAKETYPED_DIALOG_WAIT = "1"\nFAKETYPED_LOG = "{LOG}"\n'
# Long enough for the node to read the first of a chained pair off the screen
# and report it -- which it will not do for the first six seconds after the
# previous answer, because a CLI that has not redrawn yet still has the dialog
# it just answered on the tail of its screen.
_d += '\nFAKETYPED_CHAIN_SECONDS = "12"\n'
# What a person would type at this CLI to compact it. The fake logs every
# line it is given, so what the node typed is observable.
_d = _d.replace("[modes.interactive]\n",
                '[modes.interactive]\ncompact_command = "/squash-now"\n', 1)
open(f"{CLIS}/faketyped.toml", "w").write(_d)

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


def until(fn, seconds=25.0, step=0.3):
    """Wait for something to become true. A check that reads once races the
    thing it is checking, which is how a suite comes to fail only under
    load."""
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


def chat_lines(sid, tok, needle):
    """Lines in the session's chat that say something, matched loosely."""
    s, j = http("GET", f"/v1/sessions/{sid}/log?limit=200", tok=tok)
    return [m for m in (j.get("messages") or []) if needle in str(m.get("body", ""))]


def main():
    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        print("FAILED (1): build first: cargo build --workspace")
        return 1
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "operator-token", "me"],
                         capture_output=True, text=True).stdout
    optok = [w for l in out.splitlines() for w in l.split() if len(w) > 20][0]
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
        # TERM=unknown on purpose: that is what the node inherits when
        # sysvinit starts it, and it used to be passed straight through to
        # every agent -- which turns a TUI monochrome and takes away its
        # mouse and alternate screen.
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 TERM="unknown")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes", tok=optok)
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "faketyped" in j["nodes"][0]["clis"]
            break
        time.sleep(0.25)
    check("node online with the dialog-raising test CLI", ok)

    sid = http("POST", "/v1/sessions", {"name": "prompts", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]}, tok=optok)[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketyped", "mode": "interactive", "display_name": "Ida",
        "role": "engineer", "workspace": "local", "task": "raise a dialog",
        "placement": {"node": "machine-b"}}, tok=optok)
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn", tok=optok)
    check("the interactive agent spawned", s == 200, str(j))
    aid = None
    for _ in range(80):
        s, j = http("GET", f"/v1/sessions/{sid}/resolve/Ida", tok=optok)
        if s == 200 and j.get("agent_id"):
            aid = j["agent_id"]
            break
        time.sleep(0.25)
    check("the agent is registered", aid is not None, str(j))
    if not aid:
        return 1
    for _ in range(60):
        s, j = http("GET", f"/v1/agents/{aid}/transcript?tail=4000", tok=optok)
        if "FAKETYPED READY" in (j.get("transcript") or ""):
            break
        time.sleep(0.25)

    # ---- the pty it was given is a usable terminal -----------------------
    # The agent's TERM describes the pty THIS NODE created, never the
    # terminal that launched the daemon -- which under sysvinit is
    # `TERM=unknown`, and a TUI told that turns off colour, the mouse and the
    # alternate screen.
    term = ""
    for _ in range(40):
        for line in fake_log().splitlines():
            if "TERM=" in line:
                term = line.split("TERM=", 1)[1].strip()
        if term:
            break
        time.sleep(0.25)
    check("the agent is told it has a real terminal, not the daemon's",
          term and term != "unknown" and term != "<unset>", f"TERM={term!r}")
    check("and specifically a colour one", "color" in term or "256" in term,
          f"TERM={term!r}")

    # ---- compaction, when the operator asks for it -----------------------
    # Compaction is a slash command inside the TUI in every CLI checked, so
    # the node types the CLI's OWN command -- which the descriptor names,
    # because the hub does not know the slash commands of every CLI.
    s, j = http("POST", f"/v1/sessions/{sid}/messages", {"text": "/compact Ida"}, tok=optok)
    check("the operator can ask for a compaction", s == 200 and j.get("command") is True,
          str(j))
    typed = until(lambda: "/squash-now" in fake_log(), 30)
    check("the CLI's own compaction command was typed at it", bool(typed),
          fake_log()[-200:])
    check("and it is recorded in the agent's transcript",
          until(lambda: "to compact this context" in (http(
              "GET", f"/v1/agents/{aid}/transcript?tail=4000", tok=optok)[1].get("transcript") or "")),
          "")

    # ---- unattended: the question goes to the OPERATOR -------------------
    # It used to be answered here by the node. That kept the fleet moving and
    # meant nobody had authorised anything, so now it waits for a person.
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "please dialog now", "to": "Ida"},
         tok=optok)
    ap = None
    for _ in range(60):
        s, j = http("GET", "/v1/approvals", tok=optok)
        if j.get("approvals"):
            ap = j["approvals"][0]
            break
        time.sleep(0.4)
    check("the prompt is raised with the operator, not answered", ap is not None,
          str(fake_log()[-200:]))
    if not ap:
        return 1
    check("it names the agent that is stopped", ap.get("agent") == "Ida", str(ap))
    opts = ap.get("options") or []
    check("the operator is shown the choices the CLI itself offered", len(opts) >= 2,
          str(opts))
    check("with each one carrying the keystroke that picks it",
          all(o.get("keys") for o in opts), str(opts))
    check("and the affirmative told apart from the refusal",
          [o.get("affirmative") for o in opts] == [True, False], str(opts))
    check("and nothing has been typed at the agent",
          "dialog answered with" not in fake_log(), fake_log()[-200:])
    asked_lines = chat_lines(sid, optok, "is asking permission")
    check("the question is in the session chat", bool(asked_lines), str(asked_lines)[:300])
    if asked_lines:
        check("as a pre-claimed record, so no agent is handed it to answer",
              asked_lines[0].get("kind") == "approval"
              and asked_lines[0].get("claimed") is not False,
              str(asked_lines[0])[:300])

    # ---- the operator answers it, and the agent carries on ---------------
    s, j = http("POST", f"/v1/approvals/{ap['id']}/decide", {"choice": "allow"}, tok=optok)
    check("the operator can answer it", s == 200 and j.get("ok"), str(j))
    check("and the hub confirms the keystroke reached the terminal", j.get("typed") is True,
          str(j))
    answered = ""
    for _ in range(50):
        answered = fake_log()
        if "dialog answered with" in answered:
            break
        time.sleep(0.4)
    check("the choice was typed at the agent", "dialog answered with" in answered,
          answered[-200:])
    check("and it was the narrow affirmative, never an 'always'",
          "dialog answered with '1'" in answered, answered[-200:])
    moved = ""
    for _ in range(30):
        s, j = http("GET", f"/v1/agents/{aid}/transcript?tail=8000", tok=optok)
        moved = j.get("transcript") or ""
        if "Edit applied" in moved:
            break
        time.sleep(0.3)
    check("so the agent carried on past it", "Edit applied" in moved, moved[-160:])
    check("the decision is in the agent's own transcript",
          "[nexmesh] answered the prompt" in moved, moved[-300:])
    s, j = http("GET", "/v1/approvals", tok=optok)
    check("and nothing is left waiting", not j.get("approvals"), str(j))

    # ---- a standing auto-approval answers without asking -----------------
    http("POST", f"/v1/slots/{slot}/auto-approve", {"on": True}, tok=optok)
    before = fake_log().count("dialog answered with")
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "dialog again please", "to": "Ida"},
         tok=optok)
    after = before
    for _ in range(50):
        after = fake_log().count("dialog answered with")
        if after > before:
            break
        time.sleep(0.4)
    check("an auto-approved seat is answered with nobody asked", after > before,
          f"{before} -> {after}")
    s, j = http("GET", "/v1/approvals", tok=optok)
    check("so nothing waits for the operator", not j.get("approvals"), str(j))
    check("and the chat still says what was decided for them",
          bool(chat_lines(sid, optok, "automatically")),
          str(chat_lines(sid, optok, ""))[-300:])
    http("POST", f"/v1/slots/{slot}/auto-approve", {"on": False}, tok=optok)

    # ---- answered at its own terminal, with the next question behind it ----
    # The operator answers in the agent's own CLI rather than in the
    # dashboard, and the agent stops on something else straight away -- so its
    # screen never passes through a moment with nothing on it to answer. The
    # card for the question she already dealt with used to stay there, with
    # live buttons, until she answered it a SECOND time in the dashboard. On a
    # seat under tmux, whose status line carries a clock, a new card arrived
    # every minute and only the newest could ever be retired: 77 of them for
    # three seats was the state this was found in.
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "chain please", "to": "Ida"},
         tok=optok)
    first = None
    for _ in range(60):
        rows = http("GET", "/v1/approvals", tok=optok)[1].get("approvals") or []
        if rows:
            first = rows[0]
            break
        time.sleep(0.4)
    check("the first question reaches the operator", first is not None, fake_log()[-200:])
    if not first:
        return 1
    check("and it is the one on the screen", "make this edit" in first.get("question", ""),
          str(first.get("question"))[:200])
    # The fake answers that one itself, standing in for her at its keyboard,
    # and puts a different question up.
    second = None
    for _ in range(60):
        rows = http("GET", "/v1/approvals", tok=optok)[1].get("approvals") or []
        if any("run the tests" in (r.get("question") or "") for r in rows):
            second = rows
            break
        time.sleep(0.5)
    check("the question it stopped on next reaches her too", second is not None,
          str(http("GET", "/v1/approvals", tok=optok)[1])[:300])
    if second:
        check("and the one she answered at the terminal is gone from the dashboard",
              len(second) == 1, str([r.get("question", "")[:60] for r in second]))
        check("without her having to answer it a second time",
              first["id"] not in [r["id"] for r in second], str(second)[:200])
        check("and she is told why it went, rather than watching it vanish",
              bool(chat_lines(sid, optok, "different question now")),
              str(chat_lines(sid, optok, ""))[-300:])
        # Answer the live one, so the seat is not left stopped behind the rest
        # of the suite.
        http("POST", f"/v1/approvals/{second[0]['id']}/decide", {"choice": "1"}, tok=optok)
        check("the second one is answerable", until(
            lambda: "second dialog answered with" in fake_log(), 20), fake_log()[-200:])

    # ---- attached: the choice belongs to the person ------------------------
    if websocket is None:
        print("  SKIP  websocket-client is not installed; the attached-viewer half "
              "of this suite cannot run")
        return 1 if fails else 0
    before = fake_log().count("dialog answered with")
    url = (f"ws://127.0.0.1:{PORT}/v1/attach/ws?agent={aid}&cols=100&rows=30&replay=1024")
    ws = websocket.create_connection(url, timeout=20,
                                     subprotocols=["nexmesh.token." + optok])
    ws.settimeout(2)
    t0 = time.time()
    while time.time() - t0 < 2:
        try:
            ws.recv_data()
        except Exception:
            break
    http("POST", f"/v1/sessions/{sid}/messages", {"text": "another dialog please", "to": "Ida"},
         tok=optok)
    time.sleep(9)
    check("with a viewer attached nothing is typed for them",
          fake_log().count("dialog answered with") == before,
          f"{before} -> {fake_log().count('dialog answered with')}")
    check("and the operator is not asked either, because somebody is looking at it",
          not http("GET", "/v1/approvals", tok=optok)[1].get("approvals"),
          str(http("GET", "/v1/approvals", tok=optok)[1]))
    # And once the viewer leaves, it comes to the operator.
    ws.close()
    raised = False
    for _ in range(40):
        if http("GET", "/v1/approvals", tok=optok)[1].get("approvals"):
            raised = True
            break
        time.sleep(0.5)
    check("once the viewer detaches it is raised with the operator", raised)
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
    time.sleep(0.4)
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
print("ALL PROMPT CHECKS PASSED")
