#!/usr/bin/env python3
"""The web dashboard: the page, the snapshot, and its gating."""
import json, os, re, shutil, signal, subprocess, sys, tempfile, time
import urllib.request, urllib.error

# A CLI exports its conversation id to the MCP servers it starts, and these
# suites are routinely run FROM inside one. Inherited, it would hand every
# shim here the same identity and make them all claim a single seat -- which
# looks like a bug in the hub and is not. The tests choose their own.
import os as _os
for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    _os.environ.pop(_v, None)


SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17901; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-dash-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
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

def req(path, token=None, method="GET", body=None, raw=False):
    r = urllib.request.Request(HUB+path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=25) as x:
            data = x.read()
            return x.status, (data.decode(errors="replace") if raw else json.loads(data or b"{}"))
    except urllib.error.HTTPError as e:
        data = e.read()
        return e.code, (data.decode(errors="replace") if raw else json.loads(data or b"{}"))

def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a], capture_output=True, text=True)

def until(fn, seconds=25.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

class Mcpd:
    def __init__(self, sid, name, role):
        env = dict(os.environ, NEXMESH_SOCK=SOCK, NEXMESH_SESSION=sid, NEXMESH_NAME=name,
                   NEXMESH_ROLE=role, NEXMESH_CLI="claude", RUST_LOG="error")
        self.p = subprocess.Popen([f"{BIN}/nexmesh-mcpd"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True,
            bufsize=1, cwd=WS)
        procs.append(self.p); self.n = 0
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": name, "version": "1"}})
    def rpc(self, m, p=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": m}
        if p is not None: msg["params"] = p
        self.p.stdin.write(json.dumps(msg) + "\n"); self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())
    def tool(self, n, a=None):
        r = self.rpc("tools/call", {"name": n, "arguments": a or {}})
        return r["result"]["content"][0]["text"] if "result" in r else f"ERR {r}"

def main():
    optok = [l.split()[1] for l in hub_cli("operator-token", "dash").stdout.splitlines()
             if l.startswith("token:")][0]
    tok = [l.split()[1] for l in hub_cli("enrol", "machine-b").stdout.splitlines()
           if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: req("/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: (req("/v1/nodes", optok)[1].get("nodes") or [{}])[0].get("connected"))

    print("=== the page is served without a token (it holds no data) ===")
    s, html = req("/", raw=True)
    check("GET / returns the dashboard", s == 200 and "nex<span>mesh</span>" in html, str(s))
    check("it needs no token", s == 200)
    # Look for real resource references, not prose: the page's own comment
    # explains why it avoids a CDN, which a naive substring check trips over.
    refs = re.findall(r'(?:src|href)\s*=\s*["\']([^"\']+)', html)
    external = [u for u in refs if u.startswith("http") or u.startswith("//")]
    check("no external resources are referenced", not external, str(external))
    check("there is no <script src> or <link> at all",
          not re.search(r'<script[^>]+src=', html) and "<link" not in html, str(refs))
    # What matters is that the PAGE is static: it is served without
    # authentication, so anything of a session baked into it would be a leak.
    check("it carries no session data", "sl-" not in html, "no slot ids in the page")
    check("and it explains how to get in",
          "dashboard-setup-code" in html and "operator-token" in html,
          "both ways in should be named")
    s2, _ = req("/dashboard", raw=True)
    check("/dashboard works too", s2 == 200, str(s2))
    s3, _ = req("/favicon.ico", raw=True)
    check("a favicon is served", s3 == 200, str(s3))

    print("\n=== the snapshot IS gated ===")
    s, j = req("/v1/dashboard/state")
    check("no token is refused", s == 401, f"{s} {j}")
    s, j = req("/v1/dashboard/state", "wrong")
    check("a bad token is refused", s == 401, f"{s} {j}")

    print("\n=== build a session with something in it ===")
    sid = req("/v1/sessions", optok, "POST", {"name": "dash-demo", "workspaces": [
        {"name": "app", "kind": "repo", "repo_url": "git@git:app.git",
         "mounts": {"machine-b": WS}}],
        "plan": ["design", "implement", "review"]})[1]["session_id"]
    max_ = Mcpd(sid, "Max", "engineer"); iris = Mcpd(sid, "Iris", "code reviewer")
    pt = [l.split()[1] for l in hub_cli("participant-token", "hermes", "--session", sid)
          .stdout.splitlines() if l.startswith("token:")][0]
    h = req("/v1/participant/join", pt, "POST",
            {"session": sid, "name": "Hermes", "role": "archivist", "kind_hint": "hermes"})[1]["agent_id"]
    max_.tool("set_summary", {"summary": "implementing login"})
    max_.tool("store_knowledge", {"key": "auth", "value": "JWT + rotation", "category": "decision"})
    max_.tool("acquire_file", {"path": "src/auth.rs", "purpose": "editing"})
    max_.tool("send_message", {"to": "Iris", "text": "ready for review"})
    max_.tool("signal_done", {"notes": "done"})
    req("/v1/participant/tool", pt, "POST", {"agent_id": h, "tool": "send_message",
        "args": {"to": "Max", "text": "prior decision stands"}})
    req(f"/v1/sessions/{sid}/messages", optok, "POST", {"text": "team, status please"})
    time.sleep(0.5)

    print("\n=== the snapshot has everything the page draws ===")
    s, d = req(f"/v1/dashboard/state?session={sid}", optok)
    check("snapshot returned", s == 200, str(s))
    for k in ("hub", "nodes", "sessions", "detail"):
        check(f"top-level '{k}'", k in d, str(list(d.keys())))
    det = d.get("detail") or {}
    for k in ("session", "agents", "messages", "plan", "knowledge", "locks",
              "zones", "workspaces", "stats"):
        check(f"detail.{k}", k in det, str(list(det.keys())))

    names = sorted(a["name"] for a in det["agents"])
    check("all three agents appear", names == ["Hermes", "Iris", "Max"], str(names))
    hermes = [a for a in det["agents"] if a["name"] == "Hermes"][0]
    check("the external agent is marked external and hosted nowhere",
          hermes["provenance"] == "external" and hermes["host"] == "external", str(hermes))
    mx = [a for a in det["agents"] if a["name"] == "Max"][0]
    check("summary and state are shown",
          mx["summary"] == "implementing login" and mx["task_state"] == "done_pending_review", str(mx))

    bodies = [m["body"] for m in det["messages"]]
    check("messages include both directions and the operator",
          any("ready for review" in b for b in bodies)
          and any("prior decision stands" in b for b in bodies)
          and any("status please" in b for b in bodies), str(bodies)[:300])
    check("delivery state is exposed per message",
          all("delivered" in m and "in_flight" in m for m in det["messages"]))
    check("the plan is there with 3 items", len(det["plan"]) == 3, str(det["plan"]))
    check("knowledge is there", any(k["key"] == "auth" for k in det["knowledge"]), str(det["knowledge"]))
    check("the lock is there", any(l["path"] == "src/auth.rs" for l in det["locks"]), str(det["locks"]))
    check("the workspace is there with its mount",
          det["workspaces"] and WS in det["workspaces"][0]["mounts"], str(det["workspaces"]))

    st = det["stats"]
    check("stats count the slots", st["slots"] == 3, str(st))
    check("stats count what awaits review", st["awaiting_review"] == 1, str(st))
    check("stats surface unseen messages", st["undelivered"] >= 1, str(st))
    check("stats include the fleet-wide token totals",
          "input_tokens" in st and "output_tokens" in st, str(st))

    print("\n=== it defaults to the most recent session ===")
    s, d2 = req("/v1/dashboard/state", optok)
    check("no session argument still returns a detail",
          (d2.get("detail") or {}).get("session", {}).get("id") == sid, str(d2.get("detail", {}).get("session")))

    print("\n=== all sessions at once ===")
    # A fleet is usually several teams, and the question you arrive with --
    # is anything stuck anywhere? -- is about all of them. Clicking through
    # each session in turn was the only way to answer it.
    s, j = req("/v1/sessions", optok, "POST", {"name": "second-team"})
    sid2 = j.get("session_id")
    req(f"/v1/sessions/{sid2}/slots", optok, "POST",
        {"cli_id": "handmade", "display_name": "Solo", "role": "tester",
         "mode": "interactive"})
    s, da = req("/v1/dashboard/state?session=*", optok)
    det_a = da.get("detail") or {}
    check("the combined view returns a detail", s == 200 and bool(det_a), f"{s} {str(det_a)[:120]}")
    check("and names itself rather than pretending to be a session",
          (det_a.get("session") or {}).get("name") == "all sessions",
          str(det_a.get("session")))
    names = sorted(a["name"] for a in det_a.get("agents", []))
    check("every team's agents are in it",
          names == ["Hermes", "Iris", "Max", "Solo"], str(names))
    check("and each row says which session it is from",
          all(a.get("session") for a in det_a.get("agents", [])),
          str([(a["name"], a.get("session")) for a in det_a.get("agents", [])]))
    sessions_seen = {a.get("session") for a in det_a.get("agents", [])}
    check("rows come from more than one session", len(sessions_seen) == 2, str(sessions_seen))
    st_a = det_a.get("stats") or {}
    check("stats aggregate across the fleet", st_a.get("slots") == 4, str(st_a))
    check("and still surface what is stuck", st_a.get("awaiting_review") == 1, str(st_a))
    msgs = det_a.get("messages", [])
    check("messages carry their session too",
          msgs and all("session" in m for m in msgs), str(msgs[:1])[:200])
    # Narrowing must still narrow.
    s, dn = req(f"/v1/dashboard/state?session={sid2}", optok)
    only = sorted(a["name"] for a in (dn.get("detail") or {}).get("agents", []))
    check("picking one session still shows only that one", only == ["Solo"], str(only))

    print("\n=== an unknown session is not a crash ===")
    s, d3 = req("/v1/dashboard/state?session=se-nope", optok)
    check("unknown session returns 200 with an empty detail",
          s == 200 and (d3.get("detail") or {}).get("session") in (None, {}),
          f"{s} {str(d3.get('detail'))[:160]}")

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL DASHBOARD CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
