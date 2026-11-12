#!/usr/bin/env python3
"""Mutual TLS: a stolen token must no longer be enough."""
import json, os, signal, ssl, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17903; HUB = f"https://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-mtls-")
DB = f"{T}/h.db"; PKI = f"{T}/pki"; SOCK = f"{T}/n.sock"
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

def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a], capture_output=True, text=True)

def https(path, token=None, client=None):
    ctx = ssl.create_default_context(cafile=f"{PKI}/ca-cert.pem")
    if client: ctx.load_cert_chain(client[0], client[1])
    r = urllib.request.Request(HUB + path)
    if token: r.add_header("authorization", f"Bearer {token}")
    with urllib.request.urlopen(r, timeout=20, context=ctx) as x:
        return x.status, json.loads(x.read() or b"{}")

def until(fn, seconds=30.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

def main():
    print("=== 1. the hub becomes a CA ===")
    r = hub_cli("pki", "init", "--host", "127.0.0.1", "--host", "localhost",
                "--host", "machine-b", "--dir", PKI)
    for f in ("ca-cert.pem", "ca-key.pem", "server-cert.pem", "server-key.pem"):
        check(f"pki init wrote {f}", os.path.exists(f"{PKI}/{f}"), r.stdout + r.stderr)
    check("the CA key is not world-readable",
          oct(os.stat(f"{PKI}/ca-key.pem").st_mode)[-3:] == "600")
    check("it explains how to issue node certificates", "pki issue" in r.stdout, r.stdout[-200:])

    print("\n=== 2. issue a client certificate per node ===")
    r = hub_cli("pki", "issue", "machine-b", "--dir", PKI, "--out", f"{T}/machine-b")
    check("a node certificate was issued",
          os.path.exists(f"{T}/machine-b/machine-b-cert.pem") and os.path.exists(f"{T}/machine-b/machine-b-key.pem"),
          r.stdout + r.stderr)
    hub_cli("pki", "issue", "operator", "--dir", PKI, "--out", f"{T}/op")
    check("and one for the operator", os.path.exists(f"{T}/op/operator-cert.pem"))
    r = hub_cli("pki", "issue", "../escape", "--dir", PKI, "--out", f"{T}/bad")
    check("a path-escaping name is refused", r.returncode != 0, r.stdout + r.stderr)

    print("\n=== 3. mutual TLS without TLS is refused ===")
    r = subprocess.run([f"{BIN}/nexmesh-hub", "--db", f"{T}/x.db",
                        "--client-ca", f"{PKI}/ca-cert.pem", "serve"],
                       capture_output=True, text=True, timeout=30)
    check("--client-ca alone is a startup error",
          r.returncode != 0 and "without TLS" in (r.stdout + r.stderr), (r.stdout + r.stderr)[-200:])

    print("\n=== 4. start the hub requiring client certificates ===")
    optok = [l.split()[1] for l in hub_cli("operator-token", "operator").stdout.splitlines()
             if l.startswith("token:")][0]
    nodetok = [l.split()[1] for l in hub_cli("enrol", "machine-b").stdout.splitlines()
               if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "--tls-cert", f"{PKI}/server-cert.pem",
        "--tls-key", f"{PKI}/server-key.pem", "--client-ca", f"{PKI}/ca-cert.pem", "serve"],
        stdout=open(f"{T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    up = until(lambda: https("/healthz", client=(f"{T}/op/operator-cert.pem", f"{T}/op/operator-key.pem"))[1].get("ok"))
    check("the hub serves HTTPS to a client holding a certificate", bool(up),
          open(f"{T}/hub.log").read()[-300:])
    check("it logs that mutual TLS is on",
          "mutual_tls=true" in open(f"{T}/hub.log").read(), open(f"{T}/hub.log").read()[-200:])

    print("\n=== 5. THE POINT: a valid token without a certificate is refused ===")
    refused = False
    try:
        https("/v1/nodes", token=optok)   # correct operator token, no client cert
    except Exception as e:
        refused = True
        reason = str(e)
    check("a correct operator token alone cannot even reach the hub", refused,
          "the connection succeeded without a client certificate!")
    check("it fails at the TLS layer, before any token is considered",
          refused and ("SSL" in reason or "certificate" in reason.lower() or "EOF" in reason),
          reason if refused else "")
    s, j = https("/v1/nodes", token=optok,
                 client=(f"{T}/op/operator-cert.pem", f"{T}/op/operator-key.pem"))
    check("with both certificate and token it works", s == 200, f"{s} {j}")
    bad = False
    try:
        https("/v1/nodes", token="wrong-token",
              client=(f"{T}/op/operator-cert.pem", f"{T}/op/operator-key.pem"))
    except urllib.error.HTTPError as e:
        bad = e.code == 401
    check("a certificate alone is not enough either: the token still counts", bad)

    print("\n=== 6. a node connects over mutual TLS ===")
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", SOCK,
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--ca-cert", f"{PKI}/ca-cert.pem",
        "--client-cert", f"{T}/machine-b/machine-b-cert.pem", "--client-key", f"{T}/machine-b/machine-b-key.pem"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=nodetok, RUST_LOG="info", NO_COLOR="1")))
    online = until(lambda: (https("/v1/nodes", optok,
        (f"{T}/op/operator-cert.pem", f"{T}/op/operator-key.pem"))[1].get("nodes") or [{}])[0].get("connected"),
        seconds=40)
    check("the node enrolled with its certificate", bool(online),
          open(f"{T}/node.log").read()[-400:])

    print("\n=== 7. a node with the right token but no certificate is locked out ===")
    logf = f"{T}/nocert.log"
    p2 = subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", f"{ROOT}/clis", "--sock", f"{T}/n2.sock",
        "--mcpd", f"{BIN}/nexmesh-mcpd", "--ca-cert", f"{PKI}/ca-cert.pem"],
        stdout=open(logf, "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=nodetok, RUST_LOG="info", NO_COLOR="1"))
    out = ""
    for _ in range(60):
        out = open(logf).read()
        if "session ended" in out: break
        time.sleep(0.25)
    p2.kill()
    check("it cannot connect even with a valid enrolment token", "session ended" in out, out[-300:])
    check("and it is told why", "certificate" in out.lower() or "tls" in out.lower()
          or "handshake" in out.lower() or "close_notify" in out.lower(), out[-400:])

    print("\n=== 8. the operator CLI works the same way ===")
    env = dict(os.environ, NEXMESH_OPERATOR_TOKEN=optok,
               NEXMESH_CA_CERT=f"{PKI}/ca-cert.pem",
               NEXMESH_CLIENT_CERT=f"{T}/op/operator-cert.pem",
               NEXMESH_CLIENT_KEY=f"{T}/op/operator-key.pem")
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "nodes"],
                       capture_output=True, text=True, env=env, cwd=ROOT)
    check("nexmesh nodes works with a client certificate",
          r.returncode == 0 and "machine-b" in r.stdout, r.stdout + r.stderr)
    env2 = dict(env); env2.pop("NEXMESH_CLIENT_CERT"); env2.pop("NEXMESH_CLIENT_KEY")
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "nodes"],
                       capture_output=True, text=True, env=env2, cwd=ROOT)
    check("and fails without one, despite a valid token", r.returncode != 0,
          (r.stdout + r.stderr)[-200:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL MUTUAL-TLS CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
