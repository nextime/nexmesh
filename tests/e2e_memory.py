#!/usr/bin/env python3
"""Memory with a scope, and continuity across sessions.

The first version was one flat table per session: exact-key lookup, silent
last-writer-wins, and everything destroyed with the session that learned it.
These checks are about the three things that fixed: who may see a fact, who
may write one that outlives the team, and whether an agent restarted days
later knows what it was doing.
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17941; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-mem-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
CLIS = f"{T}/clis"; os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
d = open(f"{SCR}/fake-cli/fakestream.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/fakestream.toml", "w").write(d)

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
        with urllib.request.urlopen(r, timeout=20) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def q(sql, args=()):
    c = sqlite3.connect(DB); c.row_factory = sqlite3.Row
    try: return [dict(r) for r in c.execute(sql, args)]
    finally: c.close()

def until(fn, seconds=25.0, step=0.3):
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

def ptoken(session, label):
    out = hub_cli("participant-token", label, "--session", session).stdout
    return [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]

def join(session, token, name, caps=None):
    body = {"session": session, "name": name, "role": "engineer", "kind_hint": "test"}
    s, j = http("POST", "/v1/participant/join", body, token=token)
    assert "agent_id" in j, j
    if caps:
        c = sqlite3.connect(DB)
        c.execute("UPDATE slots SET capabilities = ? WHERE id = ?",
                  (json.dumps(caps), j["slot_id"])); c.commit(); c.close()
    return j["agent_id"], j["slot_id"], token

def tool(agent, token, name, args):
    s, j = http("POST", "/v1/participant/tool",
                {"agent_id": agent, "tool": name, "args": args}, token=token)
    return j

def main():
    tok = hub_cli("enrol", "machine-b").stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))

    sid = http("POST", "/v1/sessions", {"name": "memtest"})[1]["session_id"]
    # One seat trusted with global memory, one not.
    a_tok = ptoken(sid, "scribe"); b_tok = ptoken(sid, "plain")
    scribe, _scribe_slot, _ = join(sid, a_tok, "Scribe", ["memory-writer"])
    plain, _plain_slot, _ = join(sid, b_tok, "Plain")

    print("=== the session scope is the team's ===")
    j = tool(scribe, a_tok, "store_memory", {"key": "db", "value": "sqlite with WAL",
                                             "category": "decision"})
    check("a fact can be stored", j.get("ok"), str(j))
    j = tool(plain, b_tok, "recall", {"key": "db"})
    check("and any teammate reads it", "sqlite with WAL" in j.get("text", ""), str(j))
    check("the scope is shown, so its weight is clear",
          "(session)" in j.get("text", ""), j.get("text"))

    print("\n=== an agent scope is private ===")
    tool(scribe, a_tok, "store_memory", {"key": "scratch", "value": "my own note",
                                         "scope": "agent"})
    j = tool(scribe, a_tok, "recall", {"key": "scratch"})
    check("the owner can read it", "my own note" in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "recall", {"key": "scratch"})
    check("nobody else can", "my own note" not in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "recall", {"key": "scratch", "scope": "agent"})
    check("not even by naming the scope", "my own note" not in j.get("text", ""), str(j))

    print("\n=== global memory is capability-gated ===")
    j = tool(plain, b_tok, "store_memory", {"key": "style", "value": "tabs",
                                            "scope": "global"})
    check("a seat without the capability is refused", not j.get("ok"), str(j))
    check("and told what to do instead",
          "memory-writer" in str(j) and "session" in str(j), str(j))
    j = tool(scribe, a_tok, "store_memory", {"key": "style", "value": "tabs",
                                             "scope": "global", "category": "convention"})
    check("the trusted seat may write it", j.get("ok"), str(j))

    print("\n=== global memory outlives its session ===")
    sid2 = http("POST", "/v1/sessions", {"name": "later"})[1]["session_id"]
    c_tok = ptoken(sid2, "newcomer")
    newcomer, _, _ = join(sid2, c_tok, "Newcomer")
    j = tool(newcomer, c_tok, "recall", {"key": "style"})
    check("a different session reads it", "tabs" in j.get("text", ""), str(j))
    check("and it is labelled global", "(global)" in j.get("text", ""), j.get("text"))
    j = tool(newcomer, c_tok, "recall", {"key": "db"})
    check("but the first session's own memory stays there",
          "sqlite" not in j.get("text", ""), str(j))

    print("\n=== a nearer scope shadows a wider one ===")
    tool(scribe, a_tok, "store_memory", {"key": "style", "value": "spaces here",
                                         "category": "convention"})
    j = tool(plain, b_tok, "recall", {"key": "style"})
    check("the session value wins over the global one",
          "spaces here" in j.get("text", "") and "tabs" not in j.get("text", ""), str(j))
    check("one value is returned, not both",
          j.get("text", "").count("style") == 1, j.get("text"))

    print("\n=== groups are members-only, and membership is the operator's ===")
    s, j = http("POST", f"/v1/sessions/{sid}/groups", {"group": "Reviewers", "who": "Scribe"})
    check("an operator can add a member", j.get("ok") and j.get("group") == "reviewers", str(j))
    j = tool(scribe, a_tok, "store_memory", {"key": "bar", "value": "high",
                                             "scope": "group", "group": "reviewers"})
    check("a member can write it", j.get("ok"), str(j))
    j = tool(scribe, a_tok, "recall", {"key": "bar"})
    check("and read it", "high" in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "recall", {"key": "bar"})
    check("a non-member cannot read it", "high" not in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "store_memory", {"key": "x", "value": "y",
                                            "scope": "group", "group": "reviewers"})
    check("nor write it", not j.get("ok") and "not a member" in str(j), str(j))
    j = tool(plain, b_tok, "store_memory", {"key": "x", "value": "y", "scope": "group"})
    check("a group scope with no group name is refused", not j.get("ok"), str(j))

    print("\n=== an overwrite keeps what was there ===")
    tool(plain, b_tok, "store_memory", {"key": "db", "value": "postgres now"})
    j = tool(scribe, a_tok, "recall", {"key": "db"})
    check("the new value is returned", "postgres now" in j.get("text", ""), str(j))
    check("with its revision", "rev 2" in j.get("text", ""), j.get("text"))
    j = tool(scribe, a_tok, "memory_history", {"key": "db"})
    check("and the old one is recoverable", "sqlite with WAL" in j.get("text", ""), str(j))
    check("naming who wrote the value that was replaced",
          "Scribe" in j.get("text", ""), j.get("text"))
    j = tool(scribe, a_tok, "recall", {"key": "db"})
    check("and recall names who wrote the one you are reading now",
          "by Plain" in j.get("text", ""), j.get("text"))

    print("\n=== memory can be searched, not only looked up ===")
    tool(scribe, a_tok, "store_memory", {"key": "auth-choice",
         "value": "argon2id with a 64MB cost", "category": "decision"})
    j = tool(plain, b_tok, "recall", {"search": "argon2id"})
    check("a value is findable without knowing the key",
          "auth-choice" in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "recall", {"search": "which hashing did we pick?"})
    check("a question in prose is a search, not a syntax error", j.get("ok"), str(j))
    j = tool(plain, b_tok, "recall", {"category": "convention"})
    check("and a category still filters", "style" in j.get("text", ""), str(j))

    print("\n=== forgetting, and expiry ===")
    tool(scribe, a_tok, "store_memory", {"key": "temp", "value": "for a moment",
                                         "ttl_seconds": 1})
    time.sleep(2.2)
    j = tool(scribe, a_tok, "recall", {"key": "temp"})
    check("an expired fact stops being returned", "for a moment" not in j.get("text", ""), str(j))
    j = tool(plain, b_tok, "forget", {"key": "db"})
    check("a teammate can forget a session fact", j.get("ok"), str(j))
    j = tool(plain, b_tok, "forget", {"key": "style", "scope": "global"})
    check("but not a global one without the capability", not j.get("ok"), str(j))

    print("\n=== the original two tools still work ===")
    j = tool(plain, b_tok, "store_knowledge", {"key": "old", "value": "still fine",
                                               "category": "context"})
    check("store_knowledge works", j.get("ok"), str(j))
    j = tool(plain, b_tok, "query_knowledge", {"key": "old"})
    check("query_knowledge reads it back", "still fine" in j.get("text", ""), str(j))
    check("and it landed in the session scope",
          bool(q("SELECT 1 FROM memory WHERE key='old' AND scope='session'")),
          str(q("SELECT key,scope FROM memory")))

    print("\n=== continuity: a seat restarted later knows what it was doing ===")
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    check("node online", bool(until(lambda: "fakestream" in
        (http("GET","/v1/nodes")[1].get("nodes") or [{}])[0].get("clis", []))))
    wsid = http("POST", "/v1/sessions", {"name": "day-one", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{wsid}/slots", {"cli_id": "fakestream",
        "mode": "headless", "display_name": "Max", "role": "engineer",
        "workspace": "local", "task": "day one work", "placement": {"node": "machine-b"}})
    slot1 = j["slot_id"]
    http("POST", f"/v1/slots/{slot1}/spawn")
    ag1 = until(lambda: (q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                           (slot1,)) or [{}])[0].get("id"), seconds=30)
    check("the agent started", bool(ag1), open(f"{T}/node.log").read()[-300:])
    handle = until(lambda: (q("SELECT resume_handle FROM slots WHERE id=?", (slot1,))
                            or [{}])[0].get("resume_handle"), seconds=30)
    check("it reported a resume handle", bool(handle))
    dur = until(lambda: q("SELECT resume_handle,context FROM agent_continuity"), seconds=20)
    check("which is also kept against its identity, not just the slot",
          bool(dur) and dur[0]["resume_handle"] == handle, str(dur))

    # The agent writes down what it was doing, as itself.
    ptok_w = ptoken(wsid, "maxctx")
    s, j = http("GET", f"/v1/sessions/{wsid}/resolve/Max")
    j2 = tool(j["agent_id"], ptok_w, "save_context",
              {"summary": "rewrote the tokeniser; next is the parser"})
    check("the agent can save its own context", "Saved" in str(j2), str(j2))

    # A brand new session, same CLI, same workspace, same name: the same seat.
    wsid2 = http("POST", "/v1/sessions", {"name": "day-two", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]})[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{wsid2}/slots", {"cli_id": "fakestream",
        "mode": "headless", "display_name": "Max", "role": "engineer",
        "workspace": "local", "task": "day two work", "placement": {"node": "machine-b"}})
    slot2 = j["slot_id"]
    check("the new slot starts with no resume handle of its own",
          q("SELECT resume_handle FROM slots WHERE id=?", (slot2,))[0]["resume_handle"] is None)
    http("POST", f"/v1/slots/{slot2}/spawn")
    ag2 = until(lambda: (q("SELECT id FROM agents WHERE slot_id=? AND exited_at_ms IS NULL",
                           (slot2,)) or [{}])[0].get("id"), seconds=30)
    check("the next day's agent started", bool(ag2), open(f"{T}/node.log").read()[-400:])
    nodelog = until(lambda: "--resume" in open(f"{T}/node.log").read() and
                    open(f"{T}/node.log").read(), seconds=20)
    check("it was launched resuming the EARLIER session's conversation",
          bool(nodelog) and f"--resume {handle}" in nodelog,
          (nodelog or "")[-500:])
    tr = until(lambda: "CONTINUING EARLIER WORK" in http(
        "GET", f"/v1/agents/{ag2}/transcript?tail=65536")[1].get("transcript", "") and
        http("GET", f"/v1/agents/{ag2}/transcript?tail=65536")[1].get("transcript", ""),
        seconds=25)
    check("and told, in its first prompt, what it had been doing", bool(tr),
          http("GET", f"/v1/agents/{ag2}/transcript?tail=4096")[1].get("transcript", "")[-400:])
    check("quoting its own words",
          bool(tr) and "next is the parser" in tr, (tr or "")[-300:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL MEMORY CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
