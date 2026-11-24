#!/usr/bin/env python3
"""A data directory per SEAT, so a CLI that names no conversation can resume.

claude takes `--session-id`, so nexmesh names its conversation and resumes
that name. codex and opencode do not: neither exports a conversation id to
its MCP children, neither names one at launch, and both record every seat's
conversations in ONE store with no column identifying the process. Two
opencode seats spawned together were 610ms apart with the same cwd, so
nothing outside the CLI can tell which conversation belongs to which seat --
and resuming the WRONG one is worse than starting fresh.

Both do offer "continue the last one" (codex `resume --last`, opencode
`--continue`). That is only safe when the seat has a store of its OWN, which
is what this builds and what this suite checks:

  * the store is made per seat and per CLI,
  * entries that must be shared -- credentials, config -- are symlinked out
    of the real store rather than copied, so they cannot drift,
  * the CLI is pointed at it,
  * the FIRST spawn does not try to continue anything, because there is
    nothing of this seat's to continue,
  * a later spawn does, once that store holds a conversation.
"""
import json, os, signal, socket, subprocess, sys, tempfile, time
import urllib.request, urllib.error

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
T = tempfile.mkdtemp(prefix="nexmesh-seatstore-")
DB, SOCK, WS = f"{T}/h.db", f"{T}/n.sock", f"{T}/ws"
CLIS, LOG = f"{T}/clis", f"{T}/typed.log"
# Where the per-seat stores go, and the "real" shared store they branch from.
STATE, SHARED = f"{T}/cli-state", f"{T}/shared"
for d in (WS, CLIS, SHARED):
    os.makedirs(d)
open(f"{SHARED}/creds.json", "w").write('{"token":"shared-login"}')
open(f"{SHARED}/config.toml", "w").write("shared = true\n")

_d = open(f"{SCR}/fake-cli/faketyped.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
_d += f'\n[cli.env]\nFAKETYPED_LOG = "{LOG}"\n'
# `--continue-last` stands in for `resume --last` / `--continue`. The fake
# records its argv, so what the node decided is observable. Added INTO the
# existing interactive mode -- a second [modes.interactive] table is a
# duplicate key and the node rejects the whole descriptor.
assert "[modes.interactive]" in _d, "the fake CLI's descriptor changed shape"
_d = _d.replace("[modes.interactive]\n",
                '[modes.interactive]\nresume_latest_args = ["--continue-last"]\n', 1)
# And model a CLI that CANNOT name a conversation, which is the whole reason
# this mechanism exists. The fake normally takes `--session-id`, and naming a
# conversation is strictly better than continuing "the last" one -- so with
# those left in, the node rightly resumes by id and never reaches
# `resume_latest_args`. codex and opencode have no such flag.
_d = "\n".join(l for l in _d.splitlines()
                if not l.startswith(("session_id_args", "resume_args"))) + "\n"
_d += (f'\n[session_store]\nenv = "FAKE_STORE_HOME"\nroot = "{SHARED}"\n'
       'share = ["creds.json", "config.toml"]\n')
open(f"{CLIS}/faketyped.toml", "w").write(_d)

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None):
    r = urllib.request.Request(HUB + p,
                               data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=25) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def until(fn, seconds=30.0, step=0.3):
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


def cleanup():
    print(f"  (logs: {T})")
    for p in procs:
        try: p.send_signal(signal.SIGTERM)
        except Exception: pass
    time.sleep(0.4)
    for p in procs:
        try: p.kill()
        except Exception: pass


def main():
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    if not until(lambda: http("GET", "/healthz")[0] == 200):
        check("hub comes up", False)
        return 1
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", "machine-a",
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log", "w"), stderr=subprocess.STDOUT,
        # The stores go under the test's own tree, not somebody's home.
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1",
                 NEXMESH_CLI_STATE=STATE)))
    ok = until(lambda: "faketyped" in (http("GET", "/v1/nodes")[1].get("nodes") or [{}])[0]
               .get("clis", []))
    check("node online with the test CLI", bool(ok))
    if not ok:
        return 1

    sid = http("POST", "/v1/sessions", {"name": "store", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-a",
         "mounts": {"machine-a": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketyped", "mode": "interactive", "display_name": "Ida",
        "role": "engineer", "workspace": "local", "task": "keep a conversation",
        "placement": {"node": "machine-a"}})
    slot = j["slot_id"]

    print("\n=== first spawn: its own store, and nothing to continue ===")
    http("POST", f"/v1/slots/{slot}/spawn")
    told = until(lambda: "STORE=" in fake_log() and fake_log())
    check("the agent started", bool(told), fake_log()[-300:])
    if not told:
        return 1
    store = [l.split("STORE=", 1)[1].strip()
             for l in fake_log().splitlines() if "STORE=" in l][-1]
    check("the CLI was pointed at a store of its own",
          store != "<unset>" and slot in store, f"STORE={store!r}")
    check("which is under the configured root, not somebody's home",
          store.startswith(STATE), f"STORE={store!r}")
    check("and it is per CLI as well as per seat", "/faketyped/" in store, store)
    check("the directory exists", os.path.isdir(store), store)
    for entry in ("creds.json", "config.toml"):
        link = os.path.join(store, entry)
        check(f"{entry} is SHARED, by symlink rather than a copy",
              os.path.islink(link) and os.path.realpath(link) == os.path.realpath(
                  os.path.join(SHARED, entry)),
              f"{link} -> {os.path.realpath(link) if os.path.exists(link) else 'missing'}")
    check("the shared login is readable through it",
          open(os.path.join(store, "creds.json")).read() == '{"token":"shared-login"}')
    argv = [l for l in fake_log().splitlines() if "argv:" in l][-1]
    check("the FIRST spawn does not try to continue anything",
          "--continue-last" not in argv, argv)

    print("\n=== once that store holds a conversation, it continues it ===")
    # Stand in for the CLI having written a conversation of its own. Anything
    # not shared counts, which is deliberately generic: the node does not
    # need to understand a CLI's storage format to tell it has written some.
    open(os.path.join(store, "conversations.db"), "w").write("a conversation")
    pid = until(lambda: (http("GET", f"/v1/sessions/{sid}/status")[1]["agents"] or [{}])[0].get("pid"))
    check("the seat has a live process to replace", bool(pid), str(pid))
    before = len([l for l in fake_log().splitlines() if "argv:" in l])
    if pid:
        try:
            os.kill(pid, signal.SIGKILL)
        except Exception as e:
            check("could stop it", False, str(e))
    again = until(lambda: len([l for l in fake_log().splitlines() if "argv:" in l]) > before, 60)
    check("the hub brought the seat back", bool(again), fake_log()[-300:])
    argv2 = [l for l in fake_log().splitlines() if "argv:" in l][-1]
    check("and this time it continues the seat's own last conversation",
          "--continue-last" in argv2, argv2)

    print("\n=== a second seat gets a store of its own, not that one ===")
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketyped", "mode": "interactive", "display_name": "Bo",
        "role": "engineer", "workspace": "local", "task": "a different conversation",
        "placement": {"node": "machine-a"}})
    slot2 = j["slot_id"]
    http("POST", f"/v1/slots/{slot2}/spawn")
    second = until(lambda: any(slot2 in l for l in fake_log().splitlines() if "STORE=" in l))
    check("it was pointed somewhere else", bool(second),
          str([l for l in fake_log().splitlines() if "STORE=" in l]))
    store2 = [l.split("STORE=", 1)[1].strip()
              for l in fake_log().splitlines() if "STORE=" in l and slot2 in l][-1]
    check("the two seats do not share a store", store2 != store, f"{store} vs {store2}")
    # THE POINT. Without isolation this seat would be offered the other
    # seat's conversation, because "the last one" would be whichever seat
    # wrote most recently.
    argv3 = [l for l in fake_log().splitlines() if "argv:" in l][-1]
    check("and a fresh seat is NOT offered the other seat's conversation",
          "--continue-last" not in argv3, argv3)
    return 1 if fails else 0


rc = 1
try:
    rc = main()
finally:
    cleanup()
if fails:
    print(f"\nSEAT STORE FAILED ({len(fails)}): " + "; ".join(fails))
else:
    # A verdict, out loud. run-all.sh treats a suite that prints nothing as a
    # FAILURE, and it is right to: this one passed every check and said so
    # nowhere, which is indistinguishable from dying quietly.
    print("\nALL SEAT STORE CHECKS PASSED")
sys.exit(rc)
