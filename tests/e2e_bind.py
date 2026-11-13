#!/usr/bin/env python3
"""Binding a certificate to the credential sent with it.

Mutual TLS proves the caller holds a certificate this CA issued. On its own
that is not enough: every machine in the fleet holds such a certificate, so a
token stolen from one machine could be presented from another, or an
operator's certificate could carry a node's token. These checks make sure the
two must name the same identity.
"""
import json, os, signal, ssl, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17911; PLAIN = 17912; LOOSE = 17913
HUB = f"https://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-bind-")
DB = f"{T}/h.db"; PKI = f"{T}/pki"
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

def hub_cli(*a, db=None):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", db or DB, *a],
                          capture_output=True, text=True)

def token_of(out):
    return [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]

def certs(name):
    return (f"{T}/{name}/{name}-cert.pem", f"{T}/{name}/{name}-key.pem")

def https(path, token=None, client=None, port=PORT):
    ctx = ssl.create_default_context(cafile=f"{PKI}/ca-cert.pem")
    if client: ctx.load_cert_chain(client[0], client[1])
    r = urllib.request.Request(f"https://127.0.0.1:{port}{path}")
    if token: r.add_header("authorization", f"Bearer {token}")
    with urllib.request.urlopen(r, timeout=20, context=ctx) as x:
        return x.status, json.loads(x.read() or b"{}")

def post(path, body, token=None, client=None, port=PORT):
    ctx = ssl.create_default_context(cafile=f"{PKI}/ca-cert.pem")
    if client: ctx.load_cert_chain(client[0], client[1])
    r = urllib.request.Request(f"https://127.0.0.1:{port}{path}",
                               data=json.dumps(body).encode(),
                               headers={"content-type": "application/json"}, method="POST")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=20, context=ctx) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        body = e.read() or b"{}"
        try: return e.code, json.loads(body)
        except Exception: return e.code, {"raw": body.decode("utf8", "replace")}

def status(path, token=None, client=None, port=PORT):
    """Like https(), but returns the error body instead of raising."""
    try:
        return https(path, token, client, port)
    except urllib.error.HTTPError as e:
        body = e.read() or b"{}"
        try: return e.code, json.loads(body)
        except Exception: return e.code, {"raw": body.decode("utf8", "replace")}

def until(fn, seconds=30.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

def node(name, token, cert_name, port=PORT, log=None, sock=None):
    log = log or f"{T}/node-{name}-as-{cert_name}.log"
    c, k = certs(cert_name)
    p = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{port}",
        "--name", name, "--clis", f"{ROOT}/clis", "--sock", sock or f"{T}/{name}-{cert_name}.sock",
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--ca-cert", f"{PKI}/ca-cert.pem",
        "--client-cert", c, "--client-key", k],
        stdout=open(log, "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=token, RUST_LOG="info", NO_COLOR="1"))
    procs.append(p)
    return p, log

def main():
    print("=== a fleet CA, and a certificate for each party ===")
    hub_cli("pki", "init", "--host", "127.0.0.1", "--host", "localhost", "--dir", PKI)
    for name in ("operator", "intruder", "machine-b", "machine-c"):
        hub_cli("pki", "issue", name, "--dir", PKI, "--out", f"{T}/{name}")
        check(f"issued a certificate to {name}", os.path.exists(certs(name)[0]))

    # Tokens are minted under the same names, which is what pairing does.
    optok = token_of(hub_cli("operator-token", "operator").stdout)
    btok = token_of(hub_cli("enrol", "machine-b").stdout)
    ctok = token_of(hub_cli("enrol", "machine-c").stdout)

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "--tls-cert", f"{PKI}/server-cert.pem",
        "--tls-key", f"{PKI}/server-key.pem", "--client-ca", f"{PKI}/ca-cert.pem", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    up = until(lambda: https("/healthz", client=certs("operator"))[1].get("ok"))
    check("the hub is up and requiring client certificates", bool(up),
          open(f"{T}/hub.log").read()[-400:])

    print("\n=== the matching pair works, as before ===")
    s, j = https("/v1/nodes", optok, certs("operator"))
    check("the operator's own certificate carries the operator's token", s == 200, f"{s} {j}")

    print("\n=== THE POINT: a stolen operator token on someone else's certificate ===")
    s, j = status("/v1/nodes", optok, certs("intruder"))
    check("a valid operator token is refused over another party's certificate", s == 403,
          f"{s} {j}")
    check("and the refusal names both identities",
          "intruder" in json.dumps(j) and "operator" in json.dumps(j), json.dumps(j)[:300])
    check("the hub logs the mismatch",
          "does not match the token" in open(f"{T}/hub.log").read(),
          open(f"{T}/hub.log").read()[-300:])
    s, j = status("/v1/nodes", optok, certs("machine-b"))
    check("nor can a machine's certificate carry the operator's token", s == 403, f"{s} {j}")

    print("\n=== a node's token on another machine's certificate ===")
    p, log = node("machine-b", btok, "machine-c")
    out = until(lambda: ("session ended" in open(log).read()
                         or "issued for" in open(log).read()
                         or "does not match" in open(log).read()) and open(log).read(), seconds=40)
    p.kill()
    check("machine-b's token is refused over machine-c's certificate", bool(out),
          (open(log).read() or "")[-400:])
    check("and the node is told what mismatched",
          bool(out) and ("certificate" in out.lower() and "machine-c" in out), (out or "")[-400:])
    check("the hub never registered it",
          not any(n.get("name") == "machine-b"
                  for n in https("/v1/nodes", optok, certs("operator"))[1].get("nodes", [])))

    print("\n=== each machine on its own certificate still connects ===")
    node("machine-b", btok, "machine-b")
    node("machine-c", ctok, "machine-c")
    both = until(lambda: len([n for n in https("/v1/nodes", optok, certs("operator"))[1]
                              .get("nodes", []) if n.get("connected")]) == 2, seconds=45)
    check("both machines are online", bool(both),
          json.dumps(https("/v1/nodes", optok, certs("operator"))[1])[:400])

    print("\n=== an external participant is bound the same way ===")
    hub_cli("pki", "issue", "hermes", "--dir", PKI, "--out", f"{T}/hermes")
    s, j = post("/v1/sessions", {"name": "bind", "plan": ["prove it"]},
                optok, certs("operator"))
    sid = j.get("session_id")
    check("a session exists to join", bool(sid), f"{s} {j}")
    ptok = token_of(hub_cli("participant-token", "hermes", "--session", sid).stdout)
    s, j = post("/v1/participant/join",
                {"session": sid, "name": "Hermes", "role": "archivist", "kind_hint": "hermes"},
                ptok, certs("hermes"))
    check("hermes joins on its own certificate", s == 200 and "agent_id" in j, f"{s} {j}")
    s, j = post("/v1/participant/join",
                {"session": sid, "name": "Hermes", "role": "archivist", "kind_hint": "hermes"},
                ptok, certs("intruder"))
    check("and a stolen participant token is refused over another certificate", s == 403,
          f"{s} {j}")
    check("naming both identities", "hermes" in json.dumps(j) and "intruder" in json.dumps(j),
          json.dumps(j)[:300])
    s, j = post("/v1/participant/join", {"session": sid, "name": "X"},
                "garbage", certs("hermes"))
    check("an invalid participant token still gets its own clearer error, not 403", s == 401,
          f"{s} {j}")
    s, j = post("/v1/participant/join", {"session": sid, "name": "X"},
                optok, certs("operator"))
    check("and an operator token is still not a participant token", s == 401, f"{s} {j}")
    # The MCP front door is a participant route, so the same guard must cover
    # it -- a route added to that group must not escape the binding.
    init = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26",
                       "clientInfo": {"name": "bindtest", "version": "0"}}}
    s, j = post("/v1/mcp?name=Bound", init, ptok, certs("hermes"))
    check("an MCP client joins on its own certificate", s == 200 and "result" in j, f"{s} {j}")
    s, j = post("/v1/mcp?name=Bound", init, ptok, certs("intruder"))
    check("and the MCP door refuses a stolen token over another certificate", s == 403,
          f"{s} {j}")

    print("\n=== without mutual TLS there is nothing to bind ===")
    db2 = f"{T}/plain.db"
    tok2 = token_of(hub_cli("operator-token", "whatever-label", db=db2).stdout)
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", db2, "--listen",
        f"127.0.0.1:{PLAIN}", "--tls-cert", f"{PKI}/server-cert.pem",
        "--tls-key", f"{PKI}/server-key.pem", "serve"],
        stdout=open(f"{T}/plain.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    ok = until(lambda: https("/healthz", port=PLAIN)[1].get("ok"))
    check("a TLS-only hub starts", bool(ok), open(f"{T}/plain.log").read()[-300:])
    s, j = status("/v1/nodes", tok2, port=PLAIN)
    check("a token whose label matches no certificate still works there", s == 200, f"{s} {j}")

    print("\n=== the escape hatch, for a fleet named inconsistently ===")
    db3 = f"{T}/loose.db"
    tok3 = token_of(hub_cli("operator-token", "operator", db=db3).stdout)
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", db3, "--listen",
        f"127.0.0.1:{LOOSE}", "--tls-cert", f"{PKI}/server-cert.pem",
        "--tls-key", f"{PKI}/server-key.pem", "--client-ca", f"{PKI}/ca-cert.pem",
        "--no-bind-identity", "serve"],
        stdout=open(f"{T}/loose.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    ok = until(lambda: https("/healthz", client=certs("operator"), port=LOOSE)[1].get("ok"))
    check("a hub started with --no-bind-identity comes up", bool(ok),
          open(f"{T}/loose.log").read()[-300:])
    s, j = status("/v1/nodes", tok3, certs("intruder"), port=LOOSE)
    check("there, any valid certificate carries the token again", s == 200, f"{s} {j}")
    check("and it says plainly in the log that the check is off",
          "no-bind-identity" in open(f"{T}/loose.log").read(),
          open(f"{T}/loose.log").read()[-400:])

    print("\n=== the operator CLI, end to end ===")
    env = dict(os.environ, NEXMESH_OPERATOR_TOKEN=optok,
               NEXMESH_CA_CERT=f"{PKI}/ca-cert.pem",
               NEXMESH_CLIENT_CERT=certs("operator")[0],
               NEXMESH_CLIENT_KEY=certs("operator")[1])
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "nodes"],
                       capture_output=True, text=True, env=env, cwd=ROOT)
    check("nexmesh nodes works on the operator's own certificate",
          r.returncode == 0 and "machine-b" in r.stdout, (r.stdout + r.stderr)[-300:])
    env2 = dict(env, NEXMESH_CLIENT_CERT=certs("intruder")[0],
                NEXMESH_CLIENT_KEY=certs("intruder")[1])
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "nodes"],
                       capture_output=True, text=True, env=env2, cwd=ROOT)
    check("and refuses to work on somebody else's", r.returncode != 0,
          (r.stdout + r.stderr)[-300:])
    check("telling the operator which certificate was presented",
          "intruder" in (r.stdout + r.stderr), (r.stdout + r.stderr)[-300:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL IDENTITY-BINDING CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
