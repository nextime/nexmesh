#!/usr/bin/env python3
"""Pairing: joining a fleet with a one-time code, no manual certificate work."""
import json, os, signal, ssl, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17905; PAIR = 17906
HUB = f"https://127.0.0.1:{PORT}"; PAIRURL = f"https://127.0.0.1:{PAIR}"
T = tempfile.mkdtemp(prefix="nexmesh-pair-")
DB = f"{T}/h.db"; PKI = f"{T}/pki"
NODEID = f"{T}/node-identity"; OPID = f"{T}/op-identity"
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

def https(url, path, token=None, client=None, method="GET", body=None):
    ctx = ssl.create_default_context(cafile=f"{PKI}/ca-cert.pem")
    if client: ctx.load_cert_chain(client[0], client[1])
    r = urllib.request.Request(url + path,
        data=json.dumps(body).encode() if body is not None else None, method=method)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=25, context=ctx) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")

def until(fn, seconds=35.0):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v: return v
        except Exception: pass
        time.sleep(0.3)
    return None

OPCERT = None
def opc():
    return OPCERT

def main():
    global OPCERT
    print("=== 1. a hub with a CA and pairing switched on ===")
    hub_cli("pki", "init", "--host", "127.0.0.1", "--host", "localhost", "--dir", PKI)
    optok = [l.split()[1] for l in hub_cli("operator-token", "boot").stdout.splitlines()
             if l.startswith("token:")][0]
    # Bootstrap operator cert so the test can reach the mTLS port before any pairing.
    hub_cli("pki", "issue", "boot", "--dir", PKI, "--out", f"{T}/boot")
    OPCERT = (f"{T}/boot/boot-cert.pem", f"{T}/boot/boot-key.pem")
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB,
        "--listen", f"127.0.0.1:{PORT}", "--pair-listen", f"127.0.0.1:{PAIR}",
        "--tls-cert", f"{PKI}/server-cert.pem", "--tls-key", f"{PKI}/server-key.pem",
        "--client-ca", f"{PKI}/ca-cert.pem", "--pki-dir", PKI, "serve"],
        stdout=open(f"{T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    up = until(lambda: https(HUB, "/healthz", client=opc())[1].get("ok"))
    check("the hub is up with mutual TLS", bool(up), open(f"{T}/hub.log").read()[-300:])
    pup = until(lambda: https(PAIRURL, "/healthz")[1].get("ok"))
    check("the pairing listener answers WITHOUT a client certificate", bool(pup),
          open(f"{T}/hub.log").read()[-300:])
    s, j = https(PAIRURL, "/v1/nodes", token=optok)
    check("but it serves nothing else", s in (404, 405), f"{s} {j}")

    print("\n=== 2. the dashboard can mint a code ===")
    s, j = https(HUB, "/v1/pair/codes", optok, opc(), "POST", {"kind": "node", "name": "machine-a"})
    check("a code was minted", s == 200 and j.get("code"), f"{s} {j}")
    code = j.get("code", "")
    check("it is readable: grouped, no look-alike characters",
          len(code) == 19 and code.count("-") == 3 and not set("01IO") & set(code), code)
    s, lst = https(HUB, "/v1/dashboard/state", optok, opc())
    check("the dashboard sees it as open",
          any(c["name"] == "machine-a" and c["state"] == "open" for c in lst.get("pairing", [])),
          str(lst.get("pairing")))
    check("but the dashboard never carries the code itself",
          code not in json.dumps(lst), "the code leaked into the snapshot")
    s, j2 = https(HUB, "/v1/pair/codes", optok, opc())
    check("neither does the code list", code not in json.dumps(j2), str(j2)[:200])
    s, j3 = https(HUB, "/v1/pair/codes", optok, opc(), "POST", {"kind": "node", "name": "bad name!"})
    check("a nonsense name is refused", s == 400, f"{s} {j3}")

    print("\n=== 3. one command pairs the machine ===")
    r = subprocess.run([f"{BIN}/nexmesh-node", "pair", "--hub", PAIRURL, "--code", code,
                        "--connect", f"wss://127.0.0.1:{PORT}", "--ca-cert", f"{PKI}/ca-cert.pem"],
                       capture_output=True, text=True, timeout=90,
                       env=dict(os.environ, NEXMESH_IDENTITY=NODEID))
    check("pairing succeeded", r.returncode == 0 and "Paired as node" in r.stdout,
          r.stdout + r.stderr)
    for f in ("identity.json", "ca-cert.pem", "machine-a-cert.pem", "machine-a-key.pem"):
        check(f"it wrote {f}", os.path.exists(f"{NODEID}/{f}"), str(os.listdir(NODEID)))
    check("the private key is not world-readable",
          oct(os.stat(f"{NODEID}/machine-a-key.pem").st_mode)[-3:] == "600")
    check("THE KEY NEVER LEFT: the hub has no copy",
          not any("machine-a-key" in f for f in os.listdir(PKI)), str(os.listdir(PKI)))
    key = open(f"{NODEID}/machine-a-key.pem").read()
    check("and it is nowhere in the hub's database or log",
          key.split("\n")[1][:40] not in open(DB, "rb").read().decode("latin-1")
          and key.split("\n")[1][:40] not in open(f"{T}/hub.log").read())

    print("\n=== 4. the code is single-use ===")
    r2 = subprocess.run([f"{BIN}/nexmesh-node", "pair", "--hub", PAIRURL, "--code", code,
                        "--ca-cert", f"{PKI}/ca-cert.pem"], capture_output=True, text=True,
                        timeout=90, env=dict(os.environ, NEXMESH_IDENTITY=f"{T}/again"))
    check("it cannot be redeemed twice", r2.returncode != 0, r2.stdout + r2.stderr)
    r3 = subprocess.run([f"{BIN}/nexmesh-node", "pair", "--hub", PAIRURL, "--code", "ZZZZ-ZZZZ-ZZZZ-ZZZZ",
                        "--ca-cert", f"{PKI}/ca-cert.pem"], capture_output=True, text=True,
                        timeout=90, env=dict(os.environ, NEXMESH_IDENTITY=f"{T}/never"))
    check("an invented code is refused", r3.returncode != 0, r3.stdout + r3.stderr)
    check("and the two failures are indistinguishable",
          (r2.stdout + r2.stderr).split("Error:")[-1].strip()
          == (r3.stdout + r3.stderr).split("Error:")[-1].strip(),
          f"used: {r2.stderr[-90:]!r} vs unknown: {r3.stderr[-90:]!r}")

    print("\n=== 5. the node then runs with no credentials on the command line ===")
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--clis", f"{ROOT}/clis",
        "--sock", f"{T}/n.sock", "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_IDENTITY=NODEID, RUST_LOG="info", NO_COLOR="1")))
    online = until(lambda: (https(HUB, "/v1/nodes", optok, opc())[1].get("nodes") or [{}])[0].get("connected"),
                   seconds=45)
    check("it connected over mutual TLS using only the paired identity", bool(online),
          open(f"{T}/node.log").read()[-400:])
    nodes = https(HUB, "/v1/nodes", optok, opc())[1]["nodes"]
    check("under the name the code was bound to",
          nodes and nodes[0]["name"] == "machine-a", str(nodes))

    print("\n=== 6. an operator pairs the same way ===")
    s, j = https(HUB, "/v1/pair/codes", optok, opc(), "POST",
                 {"kind": "operator", "name": "machine-b"})
    r = subprocess.run([f"{BIN}/nexmesh", "pair", "--pair-hub", PAIRURL, "--code", j["code"],
                        "--connect", HUB, "--ca-cert", f"{PKI}/ca-cert.pem"],
                       capture_output=True, text=True, timeout=90, cwd=ROOT,
                       env=dict(os.environ, NEXMESH_IDENTITY=OPID))
    check("the operator paired", r.returncode == 0 and "Paired as operator" in r.stdout,
          r.stdout + r.stderr)
    env = dict(os.environ, NEXMESH_IDENTITY=OPID)
    for k in ("NEXMESH_OPERATOR_TOKEN", "NEXMESH_CA_CERT", "NEXMESH_CLIENT_CERT",
              "NEXMESH_CLIENT_KEY", "NEXMESH_HUB_HTTP"):
        env.pop(k, None)
    r = subprocess.run([f"{BIN}/nexmesh", "nodes"], capture_output=True, text=True,
                       env=env, cwd=ROOT)
    check("and `nexmesh nodes` then works with NO flags at all",
          r.returncode == 0 and "machine-a" in r.stdout, r.stdout + r.stderr)

    print("\n=== 7. revoking a code stops it being used ===")
    s, j = https(HUB, "/v1/pair/codes", optok, opc(), "POST", {"kind": "node", "name": "machine-c"})
    s2, _ = https(HUB, f"/v1/pair/codes/{j['id']}/revoke", optok, opc(), "POST", {})
    check("revoke accepted", s2 == 200, str(s2))
    r = subprocess.run([f"{BIN}/nexmesh-node", "pair", "--hub", PAIRURL, "--code", j["code"],
                        "--ca-cert", f"{PKI}/ca-cert.pem"], capture_output=True, text=True,
                       timeout=90, env=dict(os.environ, NEXMESH_IDENTITY=f"{T}/revoked"))
    check("a revoked code cannot be used", r.returncode != 0, r.stdout + r.stderr)

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL PAIRING CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
