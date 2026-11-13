#!/usr/bin/env python3
"""TLS + operator authentication, end to end.

Runs the hub with a self-signed certificate it generated itself, a node that
trusts it over wss://, and the real operator CLI with a token -- then checks
that every way of getting it wrong is refused.
"""
import json, os, shutil, signal, socket, ssl, subprocess, sys, tempfile, time
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
PORT = 17895
T = tempfile.mkdtemp(prefix="nexmesh-secure-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"; WS = f"{T}/ws"; os.makedirs(WS)
HUB = f"https://127.0.0.1:{PORT}"
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

def hub_cli(*args):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *args],
                          capture_output=True, text=True)

CTX = None
def https(method, path, body=None, token=None):
    r = urllib.request.Request(HUB+path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=20, context=CTX) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

def nexmesh(*args, token=None, ca=None):
    env = dict(os.environ)
    if token: env["NEXMESH_OPERATOR_TOKEN"] = token
    else: env.pop("NEXMESH_OPERATOR_TOKEN", None)
    if ca: env["NEXMESH_CA_CERT"] = ca
    return subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, *args],
                          capture_output=True, text=True, env=env, cwd=ROOT)

def main():
    global CTX
    print("=== 1. the hub generates its own certificate ===")
    r = hub_cli("gen-cert", "--host", "127.0.0.1", "--host", "localhost",
                "--host", "machine-b", "--out", T)
    cert, key = f"{T}/cert.pem", f"{T}/key.pem"
    check("gen-cert wrote a cert and key", os.path.exists(cert) and os.path.exists(key), r.stdout + r.stderr)
    check("the private key is not world-readable",
          oct(os.stat(key).st_mode)[-3:] == "600", oct(os.stat(key).st_mode))
    check("the cert is a real PEM certificate",
          open(cert).read().startswith("-----BEGIN CERTIFICATE-----"), open(cert).read()[:40])

    print("\n=== 2. an unauthenticated hub refuses to serve off-box ===")
    r = subprocess.run([f"{BIN}/nexmesh-hub", "--db", f"{T}/other.db",
                        "--listen", "0.0.0.0:17896", "serve"],
                       capture_output=True, text=True, timeout=30,
                       env=dict(os.environ, NO_COLOR="1"))
    check("binding 0.0.0.0 with no operator token is refused",
          r.returncode != 0 and "UNAUTHENTICATED" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-300:])
    check("the refusal explains how to fix it",
          "operator-token" in (r.stdout + r.stderr), (r.stdout + r.stderr)[-200:])

    print("\n=== 3. mint an operator token and start with TLS ===")
    out = hub_cli("operator-token", "machine-b").stdout
    optok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    check("operator token minted", len(optok) == 48, out)
    check("the token itself is never stored",
          optok not in open(DB, "rb").read().decode("latin-1"), "found the raw token in the db!")

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB,
        "--listen", f"127.0.0.1:{PORT}", "--tls-cert", cert, "--tls-key", key, "serve"],
        stdout=open(f"{T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    CTX = ssl.create_default_context(cafile=cert)
    up = False
    for _ in range(100):
        try:
            if https("GET", "/healthz")[1].get("ok"): up = True; break
        except Exception: pass
        time.sleep(0.2)
    check("hub serves HTTPS with its own certificate", up)

    # Plain HTTP against a TLS port must not accidentally work.
    try:
        urllib.request.urlopen(f"http://127.0.0.1:{PORT}/healthz", timeout=5)
        plain_ok = True
    except Exception:
        plain_ok = False
    check("plain HTTP to the TLS port fails", not plain_ok)

    print("\n=== 4. the operator API rejects the wrong credentials ===")
    s, j = https("GET", "/v1/nodes")
    check("no token is 401", s == 401, f"{s} {j}")
    check("the 401 says what to do", "operator-token" in json.dumps(j), str(j))
    s, j = https("GET", "/v1/nodes", token="not-the-right-token")
    check("a wrong token is 401", s == 401, f"{s} {j}")
    s, j = https("GET", "/v1/nodes", token=optok)
    check("the right token is accepted", s == 200, f"{s} {j}")

    print("\n=== 5. health and node/participant paths stay reachable ===")
    s, _ = https("GET", "/healthz")
    check("/healthz needs no token", s == 200, str(s))
    s, j = https("POST", "/v1/participant/join", {"name": "X"})
    check("participant join is gated by its OWN token, not the operator's",
          s == 401 and "participant" in json.dumps(j).lower(), f"{s} {j}")
    s, j = https("POST", "/v1/participant/join", {"name": "X"}, token=optok)
    check("an operator token is NOT accepted as a participant token",
          s == 401, f"{s} {j}")

    print("\n=== 6. a node enrols over wss:// trusting that certificate ===")
    tok = hub_cli("enrol", "machine-b").stdout
    tok = [l.split()[1] for l in tok.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--ca-cert", cert],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    online = False
    for _ in range(140):
        s, j = https("GET", "/v1/nodes", token=optok)
        if s == 200 and j.get("nodes") and j["nodes"][0].get("connected"):
            online = True; break
        time.sleep(0.25)
    check("node connected over TLS", online, open(f"{T}/node.log").read()[-400:])

    print("\n=== 7. a node that does not trust the cert is refused ===")
    # A node reconnects forever on purpose, so watch its first attempt rather
    # than waiting for it to give up.
    logf = f"{T}/untrusted.log"
    p2 = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", f"{T}/n2.sock",
        "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(logf, "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1"))
    out = ""
    for _ in range(60):
        out = open(logf).read()
        if "session ended" in out or "error" in out.lower(): break
        time.sleep(0.25)
    p2.kill()
    check("an untrusted certificate stops the connection",
          "session ended" in out, out[-300:])
    check("and the node says WHY, not just that it failed",
          "certificate" in out.lower() or "unknown" in out.lower() or "tls" in out.lower(),
          out[-400:])

    print("\n=== 8. the real CLI, with and without the token ===")
    r = nexmesh("nodes", token=optok, ca=cert)
    check("nexmesh nodes works with a token over TLS",
          r.returncode == 0 and "machine-b" in r.stdout, r.stdout + r.stderr)
    r = nexmesh("nodes", ca=cert)
    check("nexmesh nodes fails cleanly without a token",
          r.returncode != 0 and "authentication required" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-200:])
    r = nexmesh("nodes", token="wrong", ca=cert)
    check("nexmesh nodes fails cleanly with a bad token",
          r.returncode != 0 and "invalid or revoked" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-200:])
    r = nexmesh("nodes", token=optok)
    check("nexmesh without the CA cert refuses to trust the hub",
          r.returncode != 0, (r.stdout + r.stderr)[-200:])

    print("\n=== 9. an orchestrator scoped to one session cannot reach past it ===")
    # Two sessions, a token confined to the first, and every way out tried.
    sa = https("POST", "/v1/sessions", {"name": "alpha"}, token=optok)[1]["session_id"]
    sb = https("POST", "/v1/sessions", {"name": "beta"}, token=optok)[1]["session_id"]
    scoped_out = hub_cli("operator-token", "alpha-lead", "--session", "alpha").stdout
    scoped = next((l.split()[-1] for l in scoped_out.splitlines()
                   if l.startswith("token:")), None)
    check("a scoped token can be minted", bool(scoped), scoped_out[-200:])
    check("and it says what its reach is",
          "this session only" in scoped_out, scoped_out[-300:])
    r = hub_cli("operator-token", "nowhere-lead", "--session", "no-such-session")
    check("scoping to a session that does not exist is refused, not minted",
          r.returncode != 0 and "no session" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-200:])

    s1, j1 = https("GET", f"/v1/sessions/{sa}/status", token=scoped)
    check("it can read its own session", s1 == 200, f"{s1} {j1}")
    s2, j2 = https("GET", f"/v1/sessions/{sb}/status", token=scoped)
    check("but not another session's", s2 == 403, f"{s2} {j2}")
    s3, j3 = https("GET", "/v1/sessions", token=scoped)
    names = [x.get("name") for x in j3.get("sessions", [])]
    check("and the fleet's other sessions are not even listed to it",
          names == ["alpha"], str(names))
    s4, j4 = https("GET", "/v1/nodes", token=scoped)
    check("machines are a fleet matter, so it is refused", s4 == 403, f"{s4} {j4}")
    s5, j5 = https("POST", "/v1/sessions", {"name": "gamma"}, token=scoped)
    check("creating a session needs a super", s5 == 403, f"{s5} {j5}")
    s6, j6 = https("GET", "/v1/certs", token=scoped)
    check("certificates are a fleet matter too", s6 == 403, f"{s6} {j6}")

    # A seat belongs to a session, so the seat decides -- a scoped token must
    # not reach one through the /v1/slots path either.
    slot_b = https("POST", f"/v1/sessions/{sb}/slots",
                   {"cli_id": "handmade", "display_name": "Theirs", "role": "tester",
                    "mode": "interactive"}, token=optok)[1].get("slot_id")
    if slot_b:
        s7, j7 = https("POST", f"/v1/slots/{slot_b}/name", {"name": "Mine"}, token=scoped)
        check("a seat in another session cannot be touched by its id",
              s7 == 403, f"{s7} {j7}")
    slot_a = https("POST", f"/v1/sessions/{sa}/slots",
                   {"cli_id": "handmade", "display_name": "Ours", "role": "tester",
                    "mode": "interactive"}, token=scoped)[1].get("slot_id")
    check("while a seat in its own session can be added", bool(slot_a), str(slot_a))
    if slot_a:
        s8, j8 = https("POST", f"/v1/slots/{slot_a}/name", {"name": "Renamed"}, token=scoped)
        check("and renamed", s8 == 200, f"{s8} {j8}")
        s9, j9 = https("POST", f"/v1/slots/{slot_a}/move", {"session": "beta"}, token=scoped)
        check("but moving a seat between sessions needs a super",
              s9 == 403, f"{s9} {j9}")
    # Naming a session that does not exist must not reveal whether some other
    # session owns that name: it gets the ordinary answer, not "out of scope".
    s10, j10 = https("GET", "/v1/sessions/not-a-session-here/status", token=scoped)
    check("an unknown session name leaks nothing about scope",
          s10 == 200 and j10.get("agents") == [], f"{s10} {j10}")

    print("\n=== 10. a scoped orchestrator can reach the supervisors ===")
    s, j = https("POST", "/v1/operator/messages",
                 {"text": "alpha needs a GPU machine it cannot see"}, token=scoped)
    check("it can send a note to the supers", s == 200 and j.get("to") == "supers", f"{s} {j}")
    s, j = https("POST", "/v1/operator/messages",
                 {"text": "to beta please", "to": "beta"}, token=scoped)
    check("but it cannot address another session directly", s == 400, f"{s} {j}")
    s, j = https("GET", "/v1/operator/messages", token=optok)
    texts = [m.get("text") for m in j.get("messages", [])]
    check("a super reads what was sent to the supers",
          any("GPU machine" in t for t in texts), str(texts)[:200])
    check("and sees which session it came from",
          any(m.get("from_session") == sa for m in j.get("messages", [])), str(j)[:300])
    s, j = https("GET", "/v1/operator/messages", token=optok)
    check("reading advances that operator's cursor, so it is not served twice",
          j.get("messages") == [], str(j)[:200])
    s, j = https("POST", "/v1/operator/messages",
                 {"text": "no GPU today, use zeiss tomorrow", "to": "alpha"}, token=optok)
    check("a super can answer one session's orchestrator", s == 200, f"{s} {j}")
    s, j = https("GET", "/v1/operator/messages", token=scoped)
    texts = [m.get("text") for m in j.get("messages", [])]
    check("and the scoped orchestrator reads the answer",
          any("zeiss tomorrow" in t for t in texts), str(texts)[:200])
    check("while never being shown notes addressed elsewhere",
          all(m.get("to_session") == sa for m in j.get("messages", [])), str(j)[:300])

    print("\n=== 11. revocation takes effect ===")
    hub_cli("revoke-operator", "machine-b")
    s, j = https("GET", "/v1/nodes", token=optok)
    check("a revoked operator token stops working", s == 401, f"{s} {j}")
    r = hub_cli("operators")
    check("operators lists it as revoked", "revoked" in r.stdout, r.stdout)
    check("operators never prints the token", optok not in r.stdout, r.stdout)

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL SECURITY CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
