#!/usr/bin/env python3
"""Handing a file to the team, and getting it back to look at.

An upload is written on the machine that holds the workspace, because that is
where an agent can read it. The hub keeps no copy. So showing the picture in
the chat afterwards means asking that machine for the bytes again, and this
suite drives the whole round trip: write it on the node, record what it was,
announce it, fetch it back through the hub, and serve it as something a
browser will render -- or, for anything else, as something it will not.
"""
import base64, json, os, signal, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run FROM inside one. The tests choose their own.
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION",
           # This seat's OWN identity: inherited when the suite is run
           # from inside a managed seat, and then every test's shim
           # registers as an agent the test's node has never heard of.
           "NEXMESH_AGENT_ID", "NEXMESH_SLOT", "NEXMESH_SESSION",
           "NEXMESH_CLI", "NEXMESH_ROLE", "NEXMESH_NAME", "NEXMESH_SOCK"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17972; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-uploads-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
procs = []; fails = []

# A 1x1 PNG, by hand: small enough to read in a diff, real enough that the
# node sniffs it as an image from its own first bytes rather than its name.
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmM"
    "IQAAAABJRU5ErkJggg==")


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
    r = urllib.request.Request(HUB + p, data=json.dumps(b).encode() if b is not None else None,
                               method=m)
    r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=120) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def raw(path):
    """A GET that keeps the headers and the bytes, which is what a browser does."""
    r = urllib.request.Request(HUB + path, method="GET")
    try:
        with urllib.request.urlopen(r, timeout=120) as x:
            return x.status, dict(x.headers), x.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def until(fn, seconds=40.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None


def main():
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-a"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log", "w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-a", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd"], stdout=open(f"{T}/node.log", "w"),
        stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    check("the machine is connected", bool(until(lambda: [
        n for n in (http("GET", "/v1/nodes")[1].get("nodes") or []) if n.get("connected")])))

    s, j = http("POST", "/v1/sessions", {"name": "pics", "workspaces": [
        {"name": "app", "kind": "node_local", "node": "machine-a",
         "mounts": {"machine-a": WS}}]})
    sid = j["session_id"]
    check("a session with a workspace on that machine", s == 200, str(j))

    print("\n=== a picture goes to the machine, not to the hub ===")
    s, j = http("POST", f"/v1/sessions/{sid}/upload", {
        "name": "shot.png", "data_base64": base64.b64encode(PNG).decode(),
        "say": "the screen when it broke"})
    check("the hub accepted it", s == 200 and j.get("ok"), str(j)[:300])
    up = j.get("upload_id") or ""
    check("and gave it an id to ask for later", up.startswith("up-"), str(j)[:200])
    landed = f"{WS}/uploads/shot.png"
    check("the bytes were written on the machine", os.path.isfile(landed), landed)
    check("byte for byte what was sent",
          os.path.isfile(landed) and open(landed, "rb").read() == PNG)
    check("the hub kept no copy of its own",
          not any("shot.png" in f for f in os.listdir(T)), str(os.listdir(T)))

    print("\n=== the chat line knows there is a picture in it ===")
    s, log = http("GET", f"/v1/sessions/{sid}/log?limit=20")
    said = [m for m in log.get("messages", []) if "the screen when it broke" in m.get("body", "")]
    check("the team was told", bool(said), str([m.get("body", "")[:50] for m in log.get("messages", [])]))
    f = (said[0].get("file") or {}) if said else {}
    check("the message carries the file", f.get("id") == up, str(f))
    check("with the type a browser needs to decide", f.get("media_type") == "image/png", str(f))
    check("and where to fetch it", f.get("url") == f"/v1/uploads/{up}", str(f))
    check("and which machine is holding it", f.get("node") == "machine-a", str(f))

    print("\n=== fetching it back goes and asks that machine ===")
    code, headers, body = raw(f"/v1/uploads/{up}")
    check("the hub served it", code == 200, f"{code} {body[:120]}")
    check("the same bytes came back", body == PNG, f"{len(body)} bytes")
    check("typed from the bytes themselves, not the name",
          headers.get("content-type", "").startswith("image/png"),
          headers.get("content-type", ""))
    check("shown rather than downloaded",
          headers.get("content-disposition", "").startswith("inline"),
          headers.get("content-disposition", ""))
    check("and never sniffed into something else",
          headers.get("x-content-type-options") == "nosniff", str(headers))

    print("\n=== what a browser must NOT be handed ===")
    # An uploaded page served as a page would run on the hub's own origin,
    # where the dashboard's session lives. Anything but a known-renderable
    # type downloads instead, whatever it calls itself.
    s, j = http("POST", f"/v1/sessions/{sid}/upload", {
        "name": "notes.html", "data_base64": base64.b64encode(
            b"<script>fetch('/v1/sessions')</script>").decode()})
    evil = j.get("upload_id") or ""
    check("it was accepted like any other file", s == 200 and bool(evil), str(j)[:200])
    code, headers, body = raw(f"/v1/uploads/{evil}")
    check("but it is not served as a page", code == 200
          and not headers.get("content-type", "").startswith("text/html"),
          headers.get("content-type", ""))
    check("it downloads instead",
          headers.get("content-disposition", "").startswith("attachment"),
          headers.get("content-disposition", ""))

    print("\n=== asking for one that is not there ===")
    code, headers, body = raw("/v1/uploads/up-nothing")
    check("says so, rather than failing as an error", code in (400, 404),
          f"{code} {body[:120]}")
    check("and says what was wrong", b"no such upload" in body, body[:160])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL UPLOAD CHECKS PASSED"); return 0


try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
