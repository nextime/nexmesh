#!/usr/bin/env python3
"""Managed-spawn end-to-end: hub places a slot, the node spawns the CLI,
the CLI's shim registers as `managed`, and its tool calls reach the hub."""
import json, os, signal, subprocess, sys, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
import tempfile, shutil
# Assemble a descriptor directory: the real CLIs plus the test-only fake one,
# whose extra_paths must point at this checkout's tests/fake-cli.
CLIS = tempfile.mkdtemp(prefix="nexmesh-clis-")
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"): shutil.copy(f"{ROOT}/clis/{f}", CLIS)
_fake = open(f"{SCR}/fake-cli/fakecli.toml").read()
_fake = _fake.replace('extra_paths = ["../tests/fake-cli", "tests/fake-cli"]',
                      f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/fakecli.toml", "w").write(_fake)
PORT = 17881; HUB = f"http://127.0.0.1:{PORT}"
_T = tempfile.mkdtemp(prefix="nexmesh-spawn-")
DB = f"{_T}/spawn.db"; SOCK = f"{_T}/node.sock"; WS = f"{_T}/ws"
procs = []; fails = []

def cleanup():
    print(f"  (logs: {_T})")
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

def http(method, path, body=None):
    req = urllib.request.Request(HUB + path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

def main():
    for f in (DB, DB+"-wal", DB+"-shm", SOCK):
        try: os.remove(f)
        except OSError: pass
    os.makedirs(WS, exist_ok=True)

    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "sissy"],
        capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB,
        "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{_T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    up = False
    for _ in range(100):
        try:
            if http("GET", "/healthz")[0] == 200: up = True; break
        except Exception: pass
        time.sleep(0.2)
    check("hub up", up)

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "sissy", "--clis", CLIS, "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{_T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes")
        if s == 200 and j["nodes"] and j["nodes"][0]["connected"]:
            ok = "fakecli" in j["nodes"][0]["clis"]; break
        time.sleep(0.25)
    check("node online and advertises the test CLI", ok)

    print("\n=== placement ===")
    s, j = http("POST", "/v1/sessions", {"name": "spawntest",
        "workspaces": [{"name": "local", "kind": "node_local", "node": "sissy",
                        "mounts": {"sissy": WS}}]})
    sid = j["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "fakecli",
        "mode": "headless", "display_name": "Max", "role": "engineer",
        "role_description": "You implement.", "workspace": "local",
        "task": "Prove the managed spawn path works.",
        "placement": {"node": "sissy"}, "file_ownership": ["src/**"]})
    check("slot created", s == 200 and "slot_id" in j, str(j))
    slot = j.get("slot_id")

    s, j = http("POST", f"/v1/sessions/{sid}/slots", {"cli_id": "nosuchcli",
        "display_name": "Ghost", "placement": {"node": "sissy"}})
    ghost = j.get("slot_id")
    s2, j2 = http("POST", f"/v1/slots/{ghost}/spawn")
    check("spawning a CLI the node lacks is refused with a clear reason",
          s2 >= 400 and "nosuchcli" in json.dumps(j2), f"{s2} {j2}")

    s, j = http("POST", f"/v1/slots/{slot}/spawn")
    check("spawn accepted and placed on sissy", s == 200 and j.get("node") == "sissy", str(j))

    print("\n=== the spawned agent reaches the hub ===")
    summary = None
    for _ in range(150):
        s, j = http("GET", f"/v1/sessions/{sid}/status")
        for a in j.get("agents", []):
            if a["name"] == "Max" and a["summary"]:
                summary = a; break
        if summary: break
        time.sleep(0.2)
    check("the spawned CLI registered and called a tool",
          summary is not None and "reporting in" in (summary or {}).get("summary", ""), str(summary))
    check("it is recorded as a MANAGED agent on its node",
          summary and summary["provenance"] == "managed" and summary["host"] == "sissy", str(summary))

    # Knowledge written by the spawned agent must be readable afterwards.
    import sqlite3
    for _ in range(50):
        con = sqlite3.connect(DB); con.row_factory = sqlite3.Row
        rows = list(con.execute("SELECT key, value FROM knowledge WHERE session_id=?", (sid,)))
        con.close()
        if rows: break
        time.sleep(0.2)
    check("knowledge written by the spawned agent persisted",
          any(r["key"] == "spawn-proof" for r in rows), str([dict(r) for r in rows]))

    print("\n=== lifecycle bookkeeping ===")
    con = sqlite3.connect(DB); con.row_factory = sqlite3.Row
    ag = list(con.execute("SELECT * FROM agents WHERE slot_id=?", (slot,)))
    check("the agent's identity triple was recorded",
          ag and ag[0]["pid"] and ag[0]["proc_start_ticks"] and ag[0]["node_id"],
          str([dict(a) for a in ag]))
    own = list(con.execute("SELECT pattern FROM ownership WHERE slot_id=?", (slot,)))
    check("ownership zone recorded", [o["pattern"] for o in own] == ["src/**"], str([dict(o) for o in own]))
    aud = [dict(r) for r in con.execute("SELECT action FROM audit")]
    check("the spawn was audited", any(a["action"] == "spawn" for a in aud), str(aud))
    con.close()

    # The fake CLI exits on its own; the node must notice and report it.
    exited = False
    for _ in range(150):
        con = sqlite3.connect(DB); con.row_factory = sqlite3.Row
        r = list(con.execute("SELECT status, exited_at_ms FROM agents WHERE slot_id=?", (slot,)))
        con.close()
        if r and r[0]["status"] == "exited" and r[0]["exited_at_ms"]: exited = True; break
        time.sleep(0.2)
    check("the node noticed the agent exiting and told the hub", exited)

    print("\n=== the generated MCP config ===")
    cfgs = [f for f in os.listdir(f"{WS}/.nexmesh")] if os.path.isdir(f"{WS}/.nexmesh") else []
    check("an inline MCP config was written into the workspace",
          any(f.startswith("mcp-") for f in cfgs), str(cfgs))
    if cfgs:
        cfg = json.load(open(f"{WS}/.nexmesh/{[c for c in cfgs if c.startswith('mcp-')][0]}"))
        env = cfg["mcpServers"]["nexmesh"]["env"]
        check("the config carries the agent identity and socket",
              env.get("NEXMESH_AGENT_ID") and env.get("NEXMESH_SOCK") == SOCK, str(env))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL MANAGED-SPAWN CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
