#!/usr/bin/env python3
"""`nexmesh run`: a CLI started where you are standing, on a terminal the
fleet owns.

A CLI somebody starts by hand is a full teammate and cannot be attached to:
the pty behind it belongs to the window it was typed into. `run` makes the
terminal on the other side of the fence instead -- the node starts the CLI on
a pty it owns -- so the thing you are typing at is also attachable from any
machine in the fleet.

What has to be true for that to be worth typing:

  * in a directory nobody owns, it makes the team and mounts the directory,
    rather than asking which session you meant;
  * in a directory a session already owns, it joins that session;
  * the seat is named after the CLI and the machine, so running it again
    tomorrow takes the same seat back rather than leaving a trail of them;
  * a seat somebody is sitting in is never taken from under them;
  * what it started really is attachable, which is the whole point.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

# The fake interactive CLI: a real pty, a prompt, and a shim that registers.
CLIS = tempfile.mkdtemp(prefix="nexmesh-run-clis-")
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"):
        shutil.copy(f"{ROOT}/clis/{f}", CLIS)
_fake = open(f"{SCR}/fake-cli/faketui.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/faketui.toml", "w").write(_fake)

PORT = 17907
HUB = f"http://127.0.0.1:{PORT}"
_T = tempfile.mkdtemp(prefix="nexmesh-run-")
DB = f"{_T}/run.db"
SOCK = f"{_T}/node.sock"
NODE = "machine-a"
# Two directories: one nobody owns, one a session already does.
UNOWNED = f"{_T}/scratch"
OWNED = f"{_T}/payments"
procs, fails = [], []


def cleanup():
    print(f"  (logs: {_T})")
    for p in procs:
        try:
            p.send_signal(signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.5)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(method, path, body=None):
    req = urllib.request.Request(
        HUB + path, data=json.dumps(body).encode() if body is not None else None,
        method=method)
    req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def ctl(*args, cwd=None, timeout=60):
    """`nexmesh ...`, as a person would type it."""
    p = subprocess.run(
        [f"{BIN}/nexmesh", "--hub", HUB, *args],
        capture_output=True, text=True, timeout=timeout, cwd=cwd,
        env=dict(os.environ, NEXMESH_NAME=NODE, NO_COLOR="1"))
    return p.returncode, p.stdout + p.stderr


def wait_http(timeout=20):
    for _ in range(int(timeout * 5)):
        try:
            s, _ = http("GET", "/healthz")
            if s == 200:
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def seats(session):
    s, j = http("GET", f"/v1/sessions/{session}/status")
    return j.get("agents", [])


def wait_for_seat(session, name, want_status=None, timeout=25):
    for _ in range(int(timeout * 5)):
        for a in seats(session):
            if a.get("name") == name:
                if want_status is None or a.get("status") == want_status:
                    return a
        time.sleep(0.2)
    return {}


def main():
    os.makedirs(UNOWNED, exist_ok=True)
    os.makedirs(OWNED, exist_ok=True)

    print("\n=== 1. a fleet of one machine ===")
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", NODE],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("hub answers /healthz", wait_http())
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", NODE,
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd",
         "--label", NODE],
        stdout=open(f"{_T}/node.log", "a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    online = False
    for _ in range(100):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and any(n.get("connected") for n in j.get("nodes", [])):
            online = True
            break
        time.sleep(0.2)
    check("the node is connected", online)

    print("\n=== 2. a dry run says what it would do, and does nothing ===")
    rc, out = ctl("run", "faketui", "--dry-run", cwd=UNOWNED)
    check("dry run succeeds", rc == 0, out)
    check("it would create the team this directory needs",
          "create scratch" in out and UNOWNED in out, out)
    check("and name the seat after the CLI and the machine",
          f"Faketui-{NODE}" in out, out)
    s, j = http("GET", "/v1/sessions")
    check("and it created nothing", j.get("sessions") == [], str(j)[:200])

    print("\n=== 3. in a directory nobody owns: it makes the team ===")
    rc, out = ctl("run", "faketui", "--no-attach", cwd=UNOWNED)
    check("run succeeds", rc == 0, out)
    s, j = http("GET", "/v1/sessions")
    names = [x["name"] for x in j.get("sessions", [])]
    check("a session named after the directory exists", "scratch" in names, str(names))
    s, j = http("GET", f"/v1/session-for?node={NODE}&path={UNOWNED}")
    check("and it owns the directory now", j.get("session") == "scratch", str(j))
    row = wait_for_seat("scratch", f"Faketui-{NODE}", "running")
    check("the CLI is running in it", row.get("status") == "running", str(row)[:300])
    check("on a terminal the node owns, which is what makes it attachable",
          row.get("io_kind") in ("pty", "tmux"), str(row)[:300])
    check("working where we were standing", row.get("cwd") == UNOWNED, str(row)[:300])

    print("\n=== 4. that terminal really can be attached to ===")
    s, j = http("GET", f"/v1/sessions/scratch/resolve/Faketui-{NODE}")
    check("the hub resolves it to a live terminal",
          s == 200 and j.get("exited") is False and j.get("io_kind") is not None,
          str(j)[:300])
    rc, out = ctl("attach", "scratch", f"Faketui-{NODE}", timeout=25)
    # Not a terminal here, so the viewer refuses for THAT reason -- never for
    # want of something to attach to.
    check("attach finds a terminal rather than refusing the seat",
          "no process to attach to" not in out, out[:300])

    print("\n=== 5. a seat somebody is sitting in is not taken from under them ===")
    rc, out = ctl("run", "faketui", "--no-attach", cwd=UNOWNED)
    check("a second run succeeds", rc == 0, out)
    rows = [a["name"] for a in seats("scratch") if a.get("participant_kind") != "terminal"]
    check("it took a seat of its own beside the first",
          f"Faketui-{NODE}" in rows and f"Faketui-{NODE}-2" in rows, str(rows))

    print("\n=== 6. an empty seat of the same name is taken back ===")
    row = wait_for_seat("scratch", f"Faketui-{NODE}-2", "running")
    pid = row.get("pid")
    check("the second one has a process", bool(pid), str(row)[:200])
    os.kill(pid, signal.SIGTERM)
    gone = False
    for _ in range(80):
        r = wait_for_seat("scratch", f"Faketui-{NODE}-2", timeout=1)
        if r.get("exited") is True or r.get("agent_status") in ("exited", "disconnected"):
            gone = True
            break
    check("and it can be made empty", gone, str(wait_for_seat('scratch', f'Faketui-{NODE}-2', timeout=1))[:200])
    rc, out = ctl("run", "faketui", "--name", f"Faketui-{NODE}-2", "--no-attach", cwd=UNOWNED)
    check("run takes that seat back rather than making a third",
          rc == 0 and "taking the seat it left" in out, out)
    rows = [a["name"] for a in seats("scratch")]
    check("so there is no third seat", f"Faketui-{NODE}-3" not in rows, str(rows))

    print("\n=== 7. in a directory a session owns: it joins that session ===")
    s, j = http("POST", "/v1/sessions", {
        "name": "payments",
        "workspaces": [{"name": "local", "kind": "node_local", "node": NODE,
                        "mounts": {NODE: OWNED}}]})
    check("a session owning the directory exists", s == 200, str(j))
    rc, out = ctl("run", "faketui", "--no-attach", cwd=OWNED)
    check("run succeeds there", rc == 0, out)
    check("and says it joined rather than created", "session    payments" in out, out)
    row = wait_for_seat("payments", f"Faketui-{NODE}", "running")
    check("the CLI runs in the session that owns the directory",
          row.get("status") == "running", str(row)[:300])
    s, j = http("GET", "/v1/sessions")
    check("and no second session was invented",
          sorted(x["name"] for x in j["sessions"]) == ["payments", "scratch"],
          str([x["name"] for x in j["sessions"]]))

    print("\n=== 8. a machine that is not here is said so, before anything is made ===")
    rc, out = ctl("run", "faketui", "--node", "machine-z", "--no-attach", cwd=UNOWNED)
    check("it refuses", rc != 0, out)
    check("naming the machine and what is known",
          "machine-z" in out and NODE in out, out)

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails))
        return 1
    print("ALL RUN CHECKS PASSED")
    return 0


try:
    rc = main()
except Exception:
    import traceback
    traceback.print_exc()
    rc = 2
finally:
    cleanup()
sys.exit(rc)
