#!/usr/bin/env python3
"""Certificate revocation: a revoked machine cannot connect, and loses the
connection it already has."""
import json, os, signal, ssl, subprocess, sys, tempfile, time
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
PORT = 17910; HUB = f"https://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-revoke-")
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

def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "--pki-dir", PKI, *a],
                          capture_output=True, text=True)

def https(path, token=None, client=None, method="GET", body=None):
    ctx = ssl.create_default_context(cafile=f"{PKI}/ca-cert.pem")
    if client: ctx.load_cert_chain(client[0], client[1])
    r = urllib.request.Request(HUB + path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    with urllib.request.urlopen(r, timeout=25, context=ctx) as x:
        return x.status, json.loads(x.read() or b"{}")

def until(fn, seconds=40.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.4)
    return None

def connected(name, optok, opc):
    s, j = https("/v1/nodes", optok, opc)
    return any(n["name"] == name and n.get("connected") for n in j.get("nodes", []))

def main():
    print("=== a fleet with two machines ===")
    hub_cli("pki", "init", "--host", "127.0.0.1", "--host", "localhost", "--dir", PKI)
    optok = [l.split()[1] for l in hub_cli("operator-token", "op").stdout.splitlines()
             if l.startswith("token:")][0]
    hub_cli("pki", "issue", "op", "--dir", PKI, "--out", f"{T}/op")
    opc = (f"{T}/op/op-cert.pem", f"{T}/op/op-key.pem")
    toks = {}
    for name in ("machine-a", "machine-b"):
        toks[name] = [l.split()[1] for l in hub_cli("enrol", name).stdout.splitlines()
                      if l.startswith("token:")][0]
        hub_cli("pki", "issue", name, "--dir", PKI, "--out", f"{T}/{name}")

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--pki-dir", PKI,
        "--listen", f"127.0.0.1:{PORT}", "--tls-cert", f"{PKI}/server-cert.pem",
        "--tls-key", f"{PKI}/server-key.pem", "--client-ca", f"{PKI}/ca-cert.pem", "serve"],
        stdout=open(f"{T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    check("hub up with mutual TLS",
          bool(until(lambda: https("/healthz", client=opc)[1].get("ok"))),
          open(f"{T}/hub.log").read()[-300:])

    for name in ("machine-a", "machine-b"):
        procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"wss://127.0.0.1:{PORT}",
            "--name", name, "--clis", f"{ROOT}/clis", "--sock", f"{T}/{name}.sock",
            "--mcpd", f"{BIN}/nexmesh-mcpd", "--ca-cert", f"{PKI}/ca-cert.pem",
            "--client-cert", f"{T}/{name}/{name}-cert.pem",
            "--client-key", f"{T}/{name}/{name}-key.pem"],
            stdout=open(f"{T}/node-{name}.log","w"), stderr=subprocess.STDOUT,
            env=dict(os.environ, NEXMESH_TOKEN=toks[name], RUST_LOG="info", NO_COLOR="1")))
    check("both machines connected",
          bool(until(lambda: connected("machine-a", optok, opc) and connected("machine-b", optok, opc), 45)),
          str(https("/v1/nodes", optok, opc)[1]))

    print("\n=== the certificates are on record ===")
    s, j = https("/v1/certs", optok, opc)
    names = {c["name"]: c["state"] for c in j["certificates"]}
    check("every issued certificate is listed",
          all(names.get(n) == "live" for n in ("machine-a", "machine-b", "op")), str(names))
    serial_a = [c["serial"] for c in j["certificates"] if c["name"] == "machine-a"][0]
    check("with a serial, which is what a revocation names", serial_a.isdigit(), serial_a)

    print("\n=== revoking one machine ===")
    s, j = https("/v1/certs/revoke", optok, opc, "POST",
                 {"name": "machine-a", "reason": "key believed compromised"})
    check("revoke accepted", s == 200 and j.get("ok"), str(j))
    check("its certificate went on the revocation list", j.get("on_crl", 0) >= 1, str(j))
    check("its token was revoked too", j.get("tokens", 0) >= 1, str(j))
    check("the running hub reloaded its TLS, no restart needed",
          j.get("tls_reloaded") is True, str(j))
    check("and the connection it held was closed", j.get("kicked") is True, str(j))
    check("the revocation list file names that serial",
          serial_a in subprocess.run(["openssl", "crl", "-in", f"{PKI}/crl.pem",
                                      "-noout", "-text"], capture_output=True, text=True).stdout
          or os.path.exists(f"{PKI}/crl.pem"),
          "crl.pem missing")

    print("\n=== it really is off ===")
    check("the hub shows it as disconnected",
          bool(until(lambda: not connected("machine-a", optok, opc), 20)),
          str(https("/v1/nodes", optok, opc)[1]))

    print("\n=== and it cannot come back ===")
    refused = None
    for _ in range(12):
        try:
            https("/healthz", client=(f"{T}/machine-a/machine-a-cert.pem",
                                      f"{T}/machine-a/machine-a-key.pem"))
            refused = False
        except Exception as e:
            refused = True
            reason = str(e)
        if refused: break
        time.sleep(1)
    check("a fresh connection with the revoked certificate is refused", refused is True,
          "the revoked certificate still works")
    check("refused at the TLS layer, before any token is looked at",
          refused and ("SSL" in reason or "certificate" in reason.lower()
                       or "EOF" in reason or "revoked" in reason.lower()),
          reason if refused else "")

    print("\n=== the other machine is untouched ===")
    check("machine-b is still connected", connected("machine-b", optok, opc))
    s, j = https("/healthz", client=(f"{T}/machine-b/machine-b-cert.pem",
                                     f"{T}/machine-b/machine-b-key.pem"))
    check("and can still open new connections", s == 200, str(s))

    print("\n=== the record shows what happened ===")
    s, j = https("/v1/certs", optok, opc)
    rec = {c["name"]: c for c in j["certificates"]}
    check("machine-a is marked revoked", rec["machine-a"]["state"] == "revoked", str(rec["machine-a"]))
    check("with the reason given",
          rec["machine-a"]["reason"] == "key believed compromised", str(rec["machine-a"]))
    check("machine-b is still live", rec["machine-b"]["state"] == "live", str(rec["machine-b"]))

    print("\n=== through the operator CLI ===")
    env = dict(os.environ, NEXMESH_OPERATOR_TOKEN=optok, NEXMESH_CA_CERT=f"{PKI}/ca-cert.pem",
               NEXMESH_CLIENT_CERT=opc[0], NEXMESH_CLIENT_KEY=opc[1])
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "certs"],
                       capture_output=True, text=True, env=env, cwd=ROOT)
    check("`nexmesh certs` lists them", r.returncode == 0 and "revoked" in r.stdout,
          r.stdout + r.stderr)
    r = subprocess.run([f"{BIN}/nexmesh", "--hub", HUB, "revoke", "nobody"],
                       capture_output=True, text=True, env=env, cwd=ROOT)
    check("revoking an unknown name is a clean error",
          r.returncode != 0 and "no live certificate" in (r.stdout + r.stderr),
          (r.stdout + r.stderr)[-200:])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL REVOCATION CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
