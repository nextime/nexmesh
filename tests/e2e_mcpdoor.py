#!/usr/bin/env python3
"""The MCP front door: any MCP client is a teammate.

An external agent could always join through `/v1/participant/*`, but that
meant writing a client for an API that is ours alone. These checks drive the
hub as a plain Streamable-HTTP MCP client -- hand-written JSON-RPC, no nexmesh
code in sight -- because that is what an agent runtime arriving with only an
MCP implementation will do.

Verified separately against the real `claude` CLI, which reports the endpoint
Connected and negotiates 2025-03-26.
"""
import json, os, signal, sqlite3, subprocess, sys, tempfile, time
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
PORT = 17925; HUB = f"http://127.0.0.1:{PORT}"
DOOR = "/v1/mcp"
T = tempfile.mkdtemp(prefix="nexmesh-mcpdoor-")
DB = f"{T}/h.db"
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

def http(m, p, b=None, token=None, headers=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    for k, v in (headers or {}).items(): r.add_header(k, v)
    try:
        with urllib.request.urlopen(r, timeout=20) as x:
            raw = x.read()
            body = json.loads(raw) if raw.strip() else {}
            return x.status, body, dict(x.headers)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try: body = json.loads(raw) if raw.strip() else {}
        except Exception: body = {"raw": raw.decode("utf8", "replace")}
        return e.code, body, dict(e.headers)

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

RPC = [0]
def rpc(method, params=None, token=None, session=None, url=DOOR, notify=False):
    """One JSON-RPC message to the door, the way any MCP client sends it."""
    body = {"jsonrpc": "2.0", "method": method}
    if not notify:
        RPC[0] += 1
        body["id"] = RPC[0]
    if params is not None:
        body["params"] = params
    headers = {"accept": "application/json, text/event-stream"}
    if session: headers["mcp-session-id"] = session
    return http("POST", url, body, token=token, headers=headers)

def main():
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    sid = http("POST", "/v1/sessions", {"name": "doortest",
        "plan": ["prove the door"]})[1]["session_id"]
    tok = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "participant-token",
        "hermes", "--session", sid], capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    url = f"{DOOR}?name=Hermes&role=archivist"

    print("=== a token is the credential, here as everywhere ===")
    s, j, _ = rpc("initialize", {"protocolVersion": "2025-03-26",
        "clientInfo": {"name": "handwritten", "version": "0"}}, url=url)
    check("no token is refused", s == 401, f"{s} {j}")
    s, j, _ = rpc("initialize", {"protocolVersion": "2025-03-26"}, token="garbage", url=url)
    check("a wrong token is refused", s == 401, f"{s} {j}")

    print("\n=== initialize: the handshake, and a seat at the table ===")
    s, j, h = rpc("initialize", {"protocolVersion": "2025-03-26",
        "clientInfo": {"name": "handwritten", "version": "0"}}, token=tok, url=url)
    check("initialize succeeds", s == 200 and "result" in j, f"{s} {j}")
    r = j.get("result", {})
    check("it agrees a protocol version the client asked for",
          r.get("protocolVersion") == "2025-03-26", str(r.get("protocolVersion")))
    check("it declares the tools capability", "tools" in r.get("capabilities", {}), str(r))
    check("it names itself", r.get("serverInfo", {}).get("name") == "nexmesh", str(r.get("serverInfo")))
    check("it tells the model how to behave in a team",
          "PENDING MESSAGES" in r.get("instructions", ""), str(r.get("instructions"))[:200])
    mcp_session = h.get("Mcp-Session-Id") or h.get("mcp-session-id")
    check("it minted an Mcp-Session-Id header", bool(mcp_session), str(h))
    slot = until(lambda: (q("SELECT id,display_name,participant_kind FROM slots WHERE session_id=?", (sid,)) or [None])[0])
    check("a real slot exists for it", slot is not None, str(q("SELECT * FROM slots")))
    check("named from the URL", slot and slot["display_name"] == "Hermes", str(slot))
    check("and recorded as an external participant, not a CLI",
          slot and slot["participant_kind"] == "external", str(slot))

    print("\n=== an unknown protocol version is answered, not rejected ===")
    s2, j2, h2 = rpc("initialize", {"protocolVersion": "1999-01-01",
        "clientInfo": {"name": "handwritten", "version": "0"}}, token=tok, url=url)
    check("the server offers one it does support",
          s2 == 200 and j2["result"]["protocolVersion"] in ("2025-06-18", "2025-03-26", "2024-11-05"),
          str(j2.get("result", {}).get("protocolVersion")))

    print("\n=== re-initialising keeps the seat, it does not take another ===")
    # `claude mcp list` sends initialize just to check liveness. The first run
    # against the real client left a new teammate behind every time.
    for _ in range(3):
        _, jj, hh = rpc("initialize", {"protocolVersion": "2025-03-26",
            "clientInfo": {"name": "handwritten", "version": "0"}}, token=tok, url=url)
        mcp_session = hh.get("Mcp-Session-Id") or hh.get("mcp-session-id")
    slots = q("SELECT id FROM slots WHERE session_id=?", (sid,))
    check("still exactly one seat after five initializes", len(slots) == 1, str(slots))
    live = q("SELECT id FROM agents WHERE exited_at_ms IS NULL")
    check("and exactly one live participant", len(live) == 1, str(live))
    check("each handshake gets a fresh transport session",
          len(q("SELECT id FROM mcp_sessions")) == 5, str(len(q("SELECT id FROM mcp_sessions"))))
    check("only the newest is open",
          len(q("SELECT id FROM mcp_sessions WHERE closed_at_ms IS NULL")) == 1,
          str(q("SELECT id,closed_at_ms FROM mcp_sessions")))

    print("\n=== the session id is required, and is not a credential on its own ===")
    s, j, _ = rpc("tools/list", token=tok)
    check("a call without the session header is refused", s == 400, f"{s} {j}")
    check("and says what to do", "initialize" in json.dumps(j), json.dumps(j)[:200])
    other = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "participant-token",
        "intruder", "--session", sid], capture_output=True, text=True).stdout
    other = [l.split()[1] for l in other.splitlines() if l.startswith("token:")][0]
    s, j, _ = rpc("tools/list", token=other, session=mcp_session)
    check("a captured session id with another token is refused", s == 404, f"{s} {j}")

    print("\n=== tools/list: the same team tools every CLI agent gets ===")
    s, j, _ = rpc("tools/list", token=tok, session=mcp_session)
    tools = j.get("result", {}).get("tools", [])
    names = [t["name"] for t in tools]
    check("the tools are listed", s == 200 and len(tools) >= 15, f"{s} {len(tools)}")
    check("including the ones a teammate needs",
          all(n in names for n in ("send_message", "check_team_status", "signal_done")), str(names))
    check("each carries an input schema",
          all("inputSchema" in t for t in tools), str(tools[:1]))

    print("\n=== tools/call: a real call through the real dispatcher ===")
    s, j, _ = rpc("tools/call", {"name": "check_team_status", "arguments": {}},
                  token=tok, session=mcp_session)
    res = j.get("result", {})
    check("the call succeeds", s == 200 and "content" in res, f"{s} {j}")
    check("the result is MCP content, not our own shape",
          res.get("content", [{}])[0].get("type") == "text", str(res)[:200])
    check("it is not flagged an error", res.get("isError") is not True, str(res)[:200])
    check("the participant can see itself in the team",
          "Hermes" in res["content"][0]["text"], res["content"][0]["text"][:300])

    print("\n=== a failing tool is an MCP error result, not an HTTP failure ===")
    s, j, _ = rpc("tools/call", {"name": "no_such_tool", "arguments": {}},
                  token=tok, session=mcp_session)
    check("the transport still returns 200", s == 200, f"{s} {j}")
    check("and the result says isError", j.get("result", {}).get("isError") is True, str(j)[:300])

    print("\n=== a teammate's message arrives on a tool result ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Hermes", "text": "mind the archive"})
    got = until(lambda: "mind the archive" in json.dumps(
        rpc("tools/call", {"name": "check_team_status", "arguments": {}},
            token=tok, session=mcp_session)[1]), seconds=20)
    check("the message rode back on a tool result", bool(got))
    s, j, _ = rpc("tools/call", {"name": "check_team_status", "arguments": {}},
                  token=tok, session=mcp_session)
    # It was delivered on an earlier call, so it must NOT come again.
    check("and is not delivered twice", "mind the archive" not in json.dumps(j), str(j)[:300])

    print("\n=== the ack waits for proof the client got it ===")
    http("POST", f"/v1/sessions/{sid}/messages", {"to": "Hermes", "text": "second note"})
    delivered = until(lambda: "second note" in json.dumps(
        rpc("tools/call", {"name": "check_team_status", "arguments": {}},
            token=tok, session=mcp_session)[1]), seconds=20)
    check("the second message was handed over", bool(delivered))
    row = q("SELECT id,acked_at_ms,inflight_agent FROM messages WHERE body LIKE '%second note%'")[0]
    check("it is in flight, not yet acked: the response could still be lost",
          row["acked_at_ms"] is None and row["inflight_agent"] is not None, str(row))
    rpc("ping", token=tok, session=mcp_session)
    acked = until(lambda: (q("SELECT acked_at_ms FROM messages WHERE body LIKE '%second note%'")
                           or [{}])[0].get("acked_at_ms"), seconds=15)
    check("the client coming back is the proof, and acks it", bool(acked),
          str(q("SELECT id,body,acked_at_ms FROM messages WHERE body LIKE '%second note%'")))

    print("\n=== ping, notifications and unknown methods ===")
    s, j, _ = rpc("ping", token=tok, session=mcp_session)
    check("ping is answered", s == 200 and j.get("result") == {}, f"{s} {j}")
    s, j, _ = rpc("notifications/initialized", {}, token=tok, session=mcp_session, notify=True)
    check("a notification is accepted and not answered", s == 202 and not j, f"{s} {j}")
    s, j, _ = rpc("resources/list", token=tok, session=mcp_session)
    check("an unsupported method gets a proper JSON-RPC error",
          s == 200 and j.get("error", {}).get("code") == -32601, f"{s} {j}")
    s, j, _ = http("POST", DOOR, {"not": "json-rpc"}, token=tok,
                   headers={"mcp-session-id": mcp_session})
    check("a body with no method is rejected, not silently accepted",
          s == 400 and j.get("error", {}).get("code") == -32600, f"{s} {j}")

    print("\n=== no server-initiated stream, said plainly ===")
    s, j, h = http("GET", DOOR, None, token=tok, headers={"mcp-session-id": mcp_session})
    check("GET is answered 405, not left hanging", s == 405, f"{s} {j}")
    check("and explains where messages come from instead",
          "tool results" in json.dumps(j), json.dumps(j)[:200])

    print("\n=== a browser on another page cannot drive it ===")
    s, j, _ = http("POST", DOOR, {"jsonrpc": "2.0", "id": 99, "method": "tools/list"},
                   token=tok, headers={"mcp-session-id": mcp_session,
                                       "origin": "https://evil.example"})
    check("a cross-origin POST is refused", s == 403, f"{s} {j}")

    print("\n=== the participant is a real teammate, not a side channel ===")
    s, j = http("GET", f"/v1/sessions/{sid}/status")[:2]
    check("it appears in the session's own status", "Hermes" in json.dumps(j), json.dumps(j)[:400])

    print("\n=== DELETE: the client leaves, and the seat is released ===")
    s, j, _ = http("DELETE", DOOR, None, token=tok, headers={"mcp-session-id": mcp_session})
    check("the session is closed", s == 204, f"{s} {j}")
    check("the participant is marked gone",
          not q("SELECT id FROM agents WHERE exited_at_ms IS NULL"),
          str(q("SELECT id,exited_at_ms FROM agents")))
    s, j, _ = rpc("tools/list", token=tok, session=mcp_session)
    check("the old session id no longer works", s == 404, f"{s} {j}")
    s, j, h = rpc("initialize", {"protocolVersion": "2025-03-26",
        "clientInfo": {"name": "handwritten", "version": "0"}}, token=tok, url=url)
    check("and a client that comes back gets a fresh seat",
          s == 200 and len(q("SELECT id FROM slots WHERE session_id=?", (sid,))) == 2,
          str(q("SELECT id,display_name,status FROM slots")))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL MCP FRONT DOOR CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
