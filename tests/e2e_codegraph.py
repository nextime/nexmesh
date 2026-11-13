#!/usr/bin/env python3
"""The code graph: a workspace parsed once, queried by the whole team.

Real files on a real node, parsed with tree-sitter by the node that holds
them, stored and grouped by the hub, and asked about through a team tool. The
point is that an agent can learn the shape of a codebase without spending its
context reading it -- which matters most for the cheap seats.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
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
PORT = 17947; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-graph-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)

# A small but realistic tree: two clusters that call within themselves, one
# call across, an external import, and junk that must not be walked.
TREE = {
    "src/auth.rs": """
use crate::store::load_user;
pub struct Session { pub id: String }
pub fn sign_in(name: &str) -> Session { let u = load_user(name); make_session(u) }
fn make_session(u: String) -> Session { Session { id: hash_token(u) } }
fn hash_token(s: String) -> String { s }
""",
    "src/store.rs": """
pub fn load_user(name: &str) -> String { read_row(name) }
fn read_row(name: &str) -> String { name.to_string() }
""",
    "src/render.rs": """
pub fn draw_page() -> String { layout(header()) }
fn layout(s: String) -> String { s }
fn header() -> String { String::new() }
""",
    "tools/report.py": """
import requests

class Report:
    def build(self):
        return summarise()

def summarise():
    return "ok"
""",
    "node_modules/dep/index.js": "function must_not_appear() {}",
    "target/debug/leftover.rs": "fn also_must_not_appear() {}",
}
for rel, body in TREE.items():
    path = os.path.join(WS, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(body)

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

def http(m, p, b=None, token=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=60) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, args=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, args)]
    finally: c.close()

def until(fn, seconds=30.0, step=0.3):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(step)
    return None

def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a], capture_output=True, text=True)

PT = [None]
def ask(agent, what, target="", to="", rebuild=False):
    args = {"what": what}
    if target: args["target"] = target
    if to: args["to"] = to
    if rebuild: args["rebuild"] = True
    s, j = http("POST", "/v1/participant/tool",
                {"agent_id": agent, "tool": "query_code_graph", "args": args}, token=PT[0])
    return j

def main():
    tok = hub_cli("enrol", "machine-b").stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    check("node online", bool(until(lambda:
        (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("connected"))))

    # A node-local workspace on purpose: the graph needs files, not a git remote.
    sid = http("POST", "/v1/sessions", {"name": "graphtest", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]})[1]["session_id"]
    out = hub_cli("participant-token", "reader", "--session", sid).stdout
    PT[0] = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    s, j = http("POST", "/v1/participant/join",
        {"session": sid, "name": "Reader", "role": "engineer",
         "workspace": "local", "kind_hint": "test"}, token=PT[0])
    check("a participant joined with that workspace", "agent_id" in j, str(j))
    agent = j["agent_id"]

    print("=== the first question parses the workspace, nobody has to ask ===")
    j = ask(agent, "overview")
    check("the overview answers", j.get("ok"), str(j)[:400])
    text = j.get("text", "")
    check("it parsed the files", "file(s) parsed" in text, text[:400])
    check("and reported what it found", "function(s)" in text, text[:400])
    check("a node-local workspace is parsed, not refused",
          "not in a shared workspace" not in text, text[:300])
    meta = q("SELECT files, communities, truncated FROM code_graph_meta")
    check("the graph was stored by the hub", bool(meta), str(meta))
    check("four source files, not the junk",
          meta and meta[0]["files"] == 4, str(meta))

    print("\n=== dependency and build directories were never walked ===")
    names = [r["name"] for r in q("SELECT name FROM code_graph_nodes")]
    check("node_modules was skipped", "must_not_appear" not in names, str(names))
    check("target/ was skipped", "also_must_not_appear" not in names, str(names))

    print("\n=== symbols are found with their file and line ===")
    j = ask(agent, "find", "make_session")
    check("a function is found", "src/auth.rs#make_session" in j.get("text", ""), str(j))
    check("with its line", "src/auth.rs:" in j.get("text", ""), j.get("text"))
    j = ask(agent, "find", "Session")
    check("and so is a type", "src/auth.rs#Session" in j.get("text", ""), str(j))
    j = ask(agent, "find", "Report")
    check("across languages", "tools/report.py#Report" in j.get("text", ""), str(j))

    print("\n=== who calls what, in both directions ===")
    j = ask(agent, "callers", "make_session")
    check("callers are found", "src/auth.rs#sign_in" in j.get("text", ""), str(j))
    check("and marked as resolved by name, not proven",
          "resolved by name" in j.get("text", ""), j.get("text"))
    j = ask(agent, "calls", "sign_in")
    t = j.get("text", "")
    check("callees are found", "src/auth.rs#make_session" in t, t)
    check("including across files", "src/store.rs#load_user" in t, t)

    print("\n=== imports, inside the workspace and outside it ===")
    j = ask(agent, "imports", "tools/report.py")
    check("an external dependency is recorded",
          "requests" in j.get("text", ""), j.get("text"))
    j = ask(agent, "defines", "src/store.rs")
    t = j.get("text", "")
    check("a file's own symbols are listed", "src/store.rs#load_user" in t and
          "src/store.rs#read_row" in t, t)

    print("\n=== subsystems, found rather than labelled ===")
    j = ask(agent, "subsystems")
    t = j.get("text", "")
    check("subsystems are reported", "subsystem(s)" in t, t)
    check("and named by their members", "sign_in" in t or "draw_page" in t, t)
    j = ask(agent, "subsystem", "draw_page")
    t = j.get("text", "")
    check("a symbol's own subsystem lists its neighbours",
          "layout" in t and "header" in t, t)
    check("and does not drag in the unrelated cluster",
          "sign_in" not in t, "render and auth are not one subsystem: " + t)

    print("\n=== how two symbols are related ===")
    j = ask(agent, "path", "hash_token", "read_row")
    t = j.get("text", "")
    check("a path is found across files", "step(s)" in t, t)
    check("running through the bridge", "load_user" in t, t)
    j = ask(agent, "path", "hash_token", "draw_page")
    check("and unconnected symbols say so",
          "not connected" in j.get("text", ""), j.get("text"))

    print("\n=== asking badly is answered helpfully ===")
    j = ask(agent, "find", "no_such_thing_at_all")
    check("an unknown symbol says so", "Nothing in this workspace" in j.get("text", ""), str(j))
    j = ask(agent, "teleport")
    check("an unknown question lists the real ones",
          not j.get("ok") and "callers" in str(j), str(j))
    j = ask(agent, "path", "hash_token")
    check("a path with one end is refused", not j.get("ok"), str(j))

    print("\n=== rebuilding picks up a change ===")
    open(os.path.join(WS, "src/extra.rs"), "w").write("pub fn brand_new() {}\n")
    j = ask(agent, "find", "brand_new")
    check("a new symbol is absent until a rebuild",
          "Nothing in this workspace" in j.get("text", ""), j.get("text"))
    j = ask(agent, "find", "brand_new", rebuild=True)
    check("and present after one", "src/extra.rs#brand_new" in j.get("text", ""), str(j))
    check("the rebuild replaced rather than accumulated",
          q("SELECT files FROM code_graph_meta")[0]["files"] == 5,
          str(q("SELECT files FROM code_graph_meta")))

    print("\n=== it is the hub's, so every teammate shares it ===")
    s, j2 = http("POST", "/v1/participant/join",
        {"session": sid, "name": "Second", "role": "engineer",
         "workspace": "local", "kind_hint": "test"}, token=PT[0])
    j = ask(j2["agent_id"], "find", "brand_new")
    check("a second agent queries it without parsing again",
          "src/extra.rs#brand_new" in j.get("text", ""), str(j))
    check("and the node parsed only twice in total",
          open(f"{T}/node.log").read().count("parsed a workspace") == 2,
          str(open(f"{T}/node.log").read().count("parsed a workspace")))

    print("\n=== and a person can see it, not only an agent ===")
    # Until the dashboard carried this, an agent could see the shape of a
    # codebase and a human could not -- and the subsystem map is the part a
    # human most wants.
    s, snap = http("GET", f"/v1/dashboard/state?session={sid}")
    # Per-session data sits under "detail"; the envelope holds the fleet.
    g = ((snap.get("detail") or {}).get("graph") or [])
    check("the dashboard snapshot carries the graph", bool(g),
          str((snap.get("detail") or {}).keys()))
    if g:
        w = g[0]
        check("with the workspace it belongs to", w.get("workspace") == "local", str(w)[:200])
        check("how much was parsed", w.get("files") == 5, str(w)[:200])
        check("what kinds of thing are in it",
              any(c["kind"] == "function" for c in w.get("counts", [])), str(w.get("counts")))
        subs = w.get("subsystems") or []
        check("and its subsystems, largest first", len(subs) >= 2, str(subs)[:300])
        check("each with recognisable members",
              subs and all(sub.get("members") for sub in subs), str(subs)[:300])
        check("sorted by size, so the shape reads at a glance",
              all(subs[i]["n"] >= subs[i+1]["n"] for i in range(len(subs)-1)),
              str([x["n"] for x in subs]))

    print("\n=== a person can build the map without an agent asking for it ===")
    # The graph used to appear only as a side effect of query_code_graph, so
    # a fresh fleet showed "nothing parsed yet" with no way to change it.
    s, j = http("POST", "/v1/workspaces/local/graph", {})
    check("parsing can be asked for directly", s == 200 and "summary" in j, f"{s} {str(j)[:200]}")
    s, j = http("POST", "/v1/workspaces/no-such-workspace/graph", {})
    check("and an unknown workspace is a clean error", s == 400, f"{s} {str(j)[:160]}")

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL CODE GRAPH CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)