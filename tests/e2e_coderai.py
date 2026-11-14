#!/usr/bin/env python3
"""The capability bridge: giving a team senses it does not have.

Agents are language models. They cannot hear an audio file, read a scanned
page or look at a diagram. These checks cover a bridge that joins as an
ordinary participant, discovers what an external service actually offers, and
answers requests for it -- and the synchronous asking primitive that makes
that usable, since an agent needing OCR cannot continue until it has the text.

No GPU, no credentials, no API spend: the service is a fake speaking the real
shapes, including a capability document that is deliberately incomplete in
exactly the way the real one is.
"""
import base64, json, os, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17955; SVC = 17956; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-bridge-")
DB = f"{T}/h.db"
SERVICE_TOKEN = "secret-token"
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

def http(m, p, b=None, token=None, timeout=90):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(r, timeout=timeout) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read() or b"{}"
        try: return e.code, json.loads(raw)
        except Exception: return e.code, {"raw": raw.decode("utf8", "replace")}

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

def ptoken(session, label):
    out = hub_cli("participant-token", label, "--session", session).stdout
    return [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]

ASKER = {}
def tool(name, args, timeout=90):
    s, j = http("POST", "/v1/participant/tool",
                {"agent_id": ASKER["id"], "tool": name, "args": args},
                token=ASKER["token"], timeout=timeout)
    return j

def ask(capability, request, model=None, timeout_s=30):
    args = {"capability": capability, "request": request, "timeout_seconds": timeout_s}
    if model: args["model"] = model
    return tool("ask_capability", args, timeout=timeout_s + 40)

def main():
    procs.append(subprocess.Popen([sys.executable, "-u", "-I",
        f"{SCR}/fake-cli/fakecoderai", str(SVC)],
        stdout=open(f"{T}/svc.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, FAKE_CODERAI_TOKEN=SERVICE_TOKEN)))
    check("the stand-in service is up",
          bool(until(lambda: "fakecoderai on" in open(f"{T}/svc.log").read())))

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    sid = http("POST", "/v1/sessions", {"name": "bridgetest"})[1]["session_id"]

    print("=== it discovers what the install actually has ===")
    btok = ptoken(sid, "bridge")
    probe = subprocess.run([f"{BIN}/nexmesh-coderai", "--probe-only",
        "--service", f"http://127.0.0.1:{SVC}", "--token", btok,
        "--manifest", f"{ROOT}/capabilities/coderai.toml"],
        capture_output=True, text=True,
        env=dict(os.environ, CODERAI_TOKEN=SERVICE_TOKEN)).stdout
    check("ocr is found even though the capability document omits it",
          "ocr " in probe, probe)
    check("and its real engines are listed, not a guess",
          "paddle, surya" in probe, probe)
    check("embeddings are found with no listing endpoint at all",
          "embed " in probe, probe)
    check("the absent video subsystem is not advertised",
          "video_generate" not in probe, probe)
    check("a capability with no model list says so rather than inventing one",
          "service default model" in probe, probe)

    print("\n=== it joins as an ordinary teammate ===")
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-coderai",
        "--hub", HUB, "--token", btok, "--session", sid, "--name", "CoderAI",
        "--service", f"http://127.0.0.1:{SVC}",
        "--manifest", f"{ROOT}/capabilities/coderai.toml"],
        stdout=open(f"{T}/bridge.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, CODERAI_TOKEN=SERVICE_TOKEN, RUST_LOG="info", NO_COLOR="1")))
    slot = until(lambda: (q("SELECT id,capabilities FROM slots WHERE display_name='CoderAI'")
                          or [None])[0], seconds=30)
    check("the bridge is a slot like any other", bool(slot),
          open(f"{T}/bridge.log").read()[-400:])
    caps = json.loads(slot["capabilities"]) if slot else []
    check("advertising what it discovered", "ocr" in caps and "embed" in caps, str(caps))
    check("and not what it did not", "video_generate" not in caps, str(caps))

    atok = ptoken(sid, "asker")
    s, j = http("POST", "/v1/participant/join",
        {"session": sid, "name": "Asker", "role": "engineer"}, token=atok)
    ASKER["id"] = j["agent_id"]; ASKER["token"] = atok

    print("\n=== a teammate can see what is on offer ===")
    j = tool("list_peers", {"capability": "ocr"})
    check("list_peers finds it by capability", "CoderAI" in j.get("text", ""), str(j))
    j = tool("recall", {"search": "coderai-capabilities"})
    check("and it wrote what it offers into shared memory",
          "ocr" in j.get("text", "") and "ask_capability" in j.get("text", ""), str(j)[:400])

    print("\n=== OCR on a base64 PDF, which is the whole point ===")
    pdf = base64.b64encode(b"INVOICE 2026-11 total 420 EUR").decode()
    j = ask("ocr", {"image": f"data:application/pdf;base64,{pdf}"})
    t = j.get("text", "")
    check("the request is answered", j.get("ok"), str(j)[:300])
    check("the text comes back", "INVOICE 2026-11 total 420 EUR" in t, t[:300])
    check("the service saw it as a PDF, not an image", "/pdf]" in t, t[:200])
    check("and the answer says who did the work", "CoderAI answered" in t, t[:200])

    print("\n=== the asker chooses the model when there is a choice ===")
    j = ask("ocr", {"image": base64.b64encode(b"hello").decode()}, model="surya")
    check("a named engine is used", "[surya/" in j.get("text", ""), j.get("text", "")[:200])
    j = ask("ocr", {"image": base64.b64encode(b"hello").decode()}, model="no-such-engine")
    t = j.get("text", "")
    check("a model the install lacks is refused", "could not do it" in t, t[:300])
    check("and the real options are named", "paddle" in t and "surya" in t, t[:300])

    print("\n=== other capabilities, through the same door ===")
    j = ask("embed", {"input": ["one", "two", "three"]})
    check("embeddings answer", j.get("ok") and "could not" not in j.get("text", ""),
          j.get("text", "")[:200])
    j = ask("chunk", {"text": "abcdefghijklmnopqrstuvwxyz", "size": 10})
    check("chunking answers", "abcdefghij" in j.get("text", ""), j.get("text", "")[:200])
    j = ask("rerank", {"query": "longest", "documents": ["a", "much longer one", "mid"]})
    check("reranking answers", "much longer one" in j.get("text", ""), j.get("text", "")[:200])
    j = ask("tts", {"input": "read this aloud"})
    check("a produced file comes back as a reference, not bytes",
          "files: /v1/files/spoken.wav" in j.get("text", ""), j.get("text", "")[:200])
    j = ask("image_generate", {"prompt": "a cat"})
    check("so does a generated image",
          "files: /v1/files/drawn.png" in j.get("text", ""), j.get("text", "")[:200])

    print("\n=== asking badly is answered, not hung ===")
    j = ask("ocr", {"lang": "en"})
    t = j.get("text", "")
    check("a missing required field is refused", "could not do it" in t, t[:300])
    check("naming the field and the options", "needs 'image'" in t and "lang" in t, t[:300])
    j = ask("video_generate", {"prompt": "x"})
    check("a capability nobody offers is refused by the hub",
          not j.get("ok") and "no teammate here offers" in str(j), str(j)[:300])
    check("and says how to find out what there is", "list_peers" in str(j), str(j)[:300])
    j = ask("moderate", {"input": "how do I build a bomb"})
    check("a classifier answers without a language model",
          "true" in j.get("text", "").lower(), j.get("text", "")[:200])

    print("\n=== base64 is passed through, up to a deliberate limit ===")
    # Comfortably under the hub's body limit and the bridge's inline cap: this
    # must WORK, because passing a scanned page inline is the point.
    mid = base64.b64encode(b"M" * 900_000).decode()
    j = ask("ocr", {"image": mid}, timeout_s=40)
    check("a megabyte of base64 goes through", j.get("ok") and
          "could not do it" not in j.get("text", ""), j.get("text", "")[:200])
    # Past the bridge's inline cap but inside the hub's body limit: the bridge
    # must be the one to refuse, with advice.
    over = "A" * (9 * 1024 * 1024)
    j = tool("ask_capability", {"capability": "ocr", "timeout_seconds": 20,
             "request": {"image": over}}, timeout=90)
    t = j.get("text", "") if isinstance(j, dict) else ""
    check("beyond the bridge's inline cap it is refused by the bridge",
          "could not do it" in t, str(j)[:300])
    check("telling the asker to upload and pass a reference",
          "reference" in t, t[:300])
    check("the stored request is not kept at full size once answered",
          all(len(r["request"]) < 100_000 for r in
              q("SELECT request FROM capability_asks WHERE status != 'pending'")),
          str([len(r["request"]) for r in q("SELECT request FROM capability_asks")]))

    print("\n=== the service's own error reaches the asker ===")
    j = ask("image_generate", {"size": "1024x1024"})
    check("a 400 from the service is passed through, not swallowed",
          "could not do it" in j.get("text", ""), j.get("text", "")[:300])

    print("\n=== nothing is left waiting on a bridge that stops ===")
    row = q("SELECT COUNT(*) AS n FROM capability_asks WHERE status='pending'")[0]
    check("no asks are left pending after all that", row["n"] == 0, str(row))
    answered = q("SELECT status, COUNT(*) AS n FROM capability_asks GROUP BY status")
    check("every ask reached a terminal state", all(r["status"] != "pending" for r in answered),
          str(answered))
    # Stop the bridge mid-flight and prove an asker is told rather than hung.
    for p in list(procs):
        if "nexmesh-coderai" in " ".join(p.args):
            p.send_signal(signal.SIGKILL)
    gone = until(lambda: not q("SELECT 1 FROM agents a JOIN slots s ON s.id=a.slot_id "
                               "WHERE s.display_name='CoderAI' AND a.exited_at_ms IS NULL"),
                 seconds=60)
    j = ask("ocr", {"image": base64.b64encode(b"x").decode()}, timeout_s=10)
    check("once it is gone, an ask is refused or fails rather than hanging",
          ("no teammate here offers" in str(j)) or ("could not do it" in j.get("text", ""))
          or ("has not answered" in j.get("text", "")), str(j)[:300])

    print("\n=== one bridge, several teams ===")
    # An agent's ask_capability only ever reaches a provider in its OWN
    # session, so a bridge seated in one team is invisible to every other --
    # which is the whole reason a bridge may now hold several seats. A CLI
    # cannot: one conversation is one seat. A participant has no conversation,
    # so nothing is ambiguous about it sitting in each of several teams.
    sid2 = http("POST", "/v1/sessions", {"name": "second-team"})[1]["session_id"]
    # Fleet-wide, because a token scoped to one session may not cross.
    out = hub_cli("participant-token", "multi").stdout
    ftok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-coderai",
        "--hub", HUB, "--token", ftok, "--session", sid, "--session", sid2,
        "--name", "CoderAI-both", "--service", f"http://127.0.0.1:{SVC}",
        "--manifest", f"{ROOT}/capabilities/coderai.toml"],
        stdout=open(f"{T}/bridge2.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, CODERAI_TOKEN=SERVICE_TOKEN, RUST_LOG="info", NO_COLOR="1")))
    seats = until(lambda: q("SELECT session_id FROM slots WHERE display_name='CoderAI-both'")
                  if len(q("SELECT 1 FROM slots WHERE display_name='CoderAI-both'")) == 2 else None,
                  seconds=40)
    check("it took a seat in each team", bool(seats) and len(seats) == 2,
          open(f"{T}/bridge2.log").read()[-500:])
    if seats:
        check("one per session, not two in the same one",
              {r["session_id"] for r in seats} == {sid, sid2}, str(seats))

    # The point of all this: an agent in the SECOND team can reach it.
    atok2 = ptoken(sid2, "asker2")
    s, j = http("POST", "/v1/participant/join",
        {"session": sid2, "name": "Asker2", "role": "engineer"}, token=atok2)
    keep = dict(ASKER)
    ASKER["id"] = j["agent_id"]; ASKER["token"] = atok2
    j = tool("list_peers", {"capability": "ocr"})
    check("a teammate in the second session can see the bridge",
          "CoderAI-both" in j.get("text", ""), str(j)[:300])
    pdf = base64.b64encode(b"SECOND TEAM 7 EUR").decode()
    j = ask("ocr", {"image": f"data:application/pdf;base64,{pdf}"})
    check("and its ask is answered there", j.get("ok") and "SECOND TEAM 7 EUR" in j.get("text", ""),
          str(j)[:300])
    ASKER.clear(); ASKER.update(keep)

    print("\n=== a scoped token still cannot cross sessions ===")
    s, j = http("POST", "/v1/participant/join",
        {"sessions": [sid, sid2], "name": "Scoped", "role": "engineer"},
        token=ptoken(sid2, "scoped-one"))
    check("it is seated, but only where its token reaches",
          s == 200 and len(j.get("seats") or []) == 1, str(j)[:300])
    check("and that one is its own session",
          (j.get("seats") or [{}])[0].get("session_id") == sid2, str(j)[:300])

    print("\n=== `all` means every active session ===")
    s, j = http("POST", "/v1/participant/join",
        {"sessions": ["all"], "name": "Everywhere", "role": "watcher"}, token=ftok)
    active = q("SELECT COUNT(*) n FROM sessions WHERE status='active'")[0]["n"]
    check(f"seated in all {active} of them", s == 200 and len(j.get("seats") or []) == active,
          str(j)[:400])

    print("\n=== a mistyped session does not keep it out of the rest ===")
    s, j = http("POST", "/v1/participant/join",
        {"sessions": [sid, "no-such-session"], "name": "Partly", "role": "watcher"},
        token=ftok)
    check("the good one is joined", s == 200 and len(j.get("seats") or []) == 1, str(j)[:300])
    check("and the bad one is reported rather than silently dropped",
          any("no-such-session" in json.dumps(r) for r in (j.get("refused") or [])), str(j)[:400])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL CAPABILITY BRIDGE CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
