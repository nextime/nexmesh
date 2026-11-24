#!/usr/bin/env python3
"""The editable skill and template library, over its HTTP surface.

The per-seat selection is covered by e2e_seatprompt; this is the part that is
new: a store the operator can WRITE -- add a skill, edit a built-in, reset it,
override a template -- and the three-scope enable/disable dial, where turning a
skill on for a whole team re-renders the stored prompt of every seat in it that
was still following its defaults, and leaves a seat with a selection of its own
alone.

It also exercises `nexmesh-hub seed-prompts`: the store is seeded from the
built-ins the way the package installer seeds it, and the hub is pointed at it
with NEXMESH_PROMPTS_DIR, the way the generated hub.env points at it.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17931; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-skills-")
DB = f"{T}/h.db"; SOCK = f"{T}/n.sock"
CLIS = f"{T}/clis"; os.makedirs(CLIS)
STORE = f"{T}/prompts"
os.environ.setdefault("NEXMESH_IDENTITY", "/nonexistent/nexmesh-identity")
procs = []; fails = []
TOK = SID = None

def cleanup():
    print(f"  (logs: {T})")
    for p in procs:
        try: p.send_signal(signal.SIGKILL)
        except Exception: pass

def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond: fails.append(label)

def http(m, p, b=None):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=25) as x: return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e: return e.code, json.loads(e.read() or b"{}")

def until(fn, seconds=30.0, step=0.25):
    end = time.time() + seconds
    while time.time() < end:
        try:
            if fn(): return True
        except Exception: pass
        time.sleep(step)
    return False

def skill_named(name):
    for s in http("GET", "/v1/skills")[1].get("skills", []):
        if s["name"] == name: return s
    return None

def role_named(rid):
    for r in http("GET", "/v1/roles")[1].get("roles", []):
        if r["id"] == rid: return r
    return None

def seat_skills(slot):
    return http("GET", f"/v1/slots/{slot}/prompt")[1].get("skills", [])

def make_slot(name):
    return http("POST", f"/v1/sessions/{SID}/slots", {
        "cli_id": "faker", "mode": "interactive", "display_name": name,
        "placement": {"node": "machine-b"}})[1]["slot_id"]

def main():
    global TOK, SID

    with open(f"{CLIS}/faker.toml", "w") as f:
        f.write('[cli]\nid = "faker"\ndisplay_name = "Faker"\ncommand = "true"\n'
                'version_args = ["--version"]\n[modes.interactive]\nargs = []\nattachable = true\n')

    # SEED THE STORE the way the installer does, then confirm it is idempotent.
    r1 = subprocess.run([f"{BIN}/nexmesh-hub", "seed-prompts", STORE],
                        capture_output=True, text=True)
    check("seed-prompts wrote the built-ins", "seeded" in r1.stdout and r1.returncode == 0, r1.stdout + r1.stderr)
    nskills = len([x for x in os.listdir(f"{STORE}/skills") if x.endswith(".md")])
    nroles = len([x for x in os.listdir(f"{STORE}/roles") if x.endswith(".toml")])
    check("every built-in skill and role is now a file", nskills >= 9 and nroles >= 13,
          f"{nskills} skills, {nroles} roles")
    # Edit a seeded file, re-seed, and it must be left alone.
    with open(f"{STORE}/skills/handover.md", "w") as f: f.write("MY EDIT\n")
    subprocess.run([f"{BIN}/nexmesh-hub", "seed-prompts", STORE], capture_output=True, text=True)
    check("re-seeding leaves an edited file alone",
          open(f"{STORE}/skills/handover.md").read().strip() == "MY EDIT")
    # Put it back so the rest of the suite starts from the built-in text.
    os.remove(f"{STORE}/skills/handover.md")

    TOK = [l.split()[1] for l in subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
        capture_output=True, text=True).stdout.splitlines() if l.startswith("token:")][0]
    # The hub pointed at the store the way the generated hub.env points at it.
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1", NEXMESH_PROMPTS_DIR=STORE)))
    check("hub is up", until(lambda: http("GET", "/healthz")[1].get("ok")))

    procs.append(subprocess.Popen([f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}",
        "--name", "machine-b", "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log","a"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=TOK, RUST_LOG="info", NO_COLOR="1")))
    check("node is online", until(lambda: (http("GET", "/v1/nodes")[1].get("nodes") or [{}])[0]
                                          .get("connected"), 40))
    SID = http("POST", "/v1/sessions", {"name": "skilltest"})[1]["session_id"]

    print("\n=== 1. the library reads back, with sources ===")
    sk = http("GET", "/v1/skills")[1]
    check("it is editable (a store is configured)", sk.get("editable") is True, json.dumps(sk)[:200])
    check("at least the nine built-in skills are listed", len(sk.get("skills", [])) >= 9)
    check("a built-in reads as a built-in", (skill_named("handover") or {}).get("source") == "builtin")
    check("the reviewer template carries its skills",
          "review-craft" in (role_named("reviewer") or {}).get("skills", []))

    print("\n=== 2. add, edit and reset a skill ===")
    s, j = http("PUT", "/v1/skills/house-style", {"body": "Two spaces, never tabs."})
    check("a new local skill is written", s == 200 and j.get("source") == "local", f"{s} {j}")
    check("and it appears in the library as local", (skill_named("house-style") or {}).get("source") == "local")
    s, j = http("PUT", "/v1/skills/handover", {"body": "---\nname: handover\n---\n\nOur handover.\n"})
    check("editing a built-in makes it an override", s == 200 and j.get("source") == "override", f"{s} {j}")
    one = http("GET", "/v1/skills/handover")[1]
    check("and the built-in is still available underneath", bool(one.get("builtin")), json.dumps(one)[:120])
    s, j = http("DELETE", "/v1/skills/handover")
    check("resetting it reverts to the built-in", "reverted" in j.get("outcome", ""), f"{s} {j}")
    check("handover reads as a built-in again", (skill_named("handover") or {}).get("source") == "builtin")
    s, j = http("DELETE", "/v1/skills/house-style")
    check("removing a local-only skill removes it", j.get("outcome") == "removed", f"{s} {j}")
    check("a bad name is refused", http("PUT", "/v1/skills/../evil", {"body": "x"})[0] != 200)

    print("\n=== 3. override and reset a template ===")
    s, j = http("PUT", "/v1/roles/reviewer", {"title": "Reviewer", "summary": "Ours.",
        "skills": ["review-craft"], "prompt": "You review the work and say what is wrong with it, nothing more."})
    check("a template override is written", s == 200 and j.get("source") == "override", f"{s} {j}")
    check("the listing shows it overridden and with the new skills",
          (role_named("reviewer") or {}).get("skills") == ["review-craft"])
    check("a thin brief is refused", http("PUT", "/v1/roles/reviewer",
          {"title": "x", "summary": "y", "skills": [], "prompt": "too short"})[0] != 200)
    http("DELETE", "/v1/roles/reviewer")
    check("resetting the template brings the built-in skills back",
          "independent-evidence" in (role_named("reviewer") or {}).get("skills", []))

    print("\n=== 4. the three-scope dial re-renders the seats that followed defaults ===")
    plain = make_slot("Plain")
    custom = make_slot("Custom")
    http("POST", f"/v1/slots/{plain}/prompt", {"template": "reviewer"})
    http("POST", f"/v1/slots/{custom}/prompt", {"template": "reviewer"})
    # The custom seat drops one skill of its own -- it has now exercised seat scope.
    http("POST", f"/v1/slots/{custom}/prompt", {"skills_off": ["independent-evidence"]})
    check("both seats start on the reviewer's skills",
          "review-craft" in seat_skills(plain) and "review-craft" in seat_skills(custom))
    # Turn handover on for the whole team.
    s, j = http("POST", "/v1/skills/prefs",
                {"scope": "session", "reference": SID, "skill": "handover", "enabled": True})
    check("the session dial reports it re-rendered a seat", s == 200 and j.get("seats_rerendered", 0) >= 1, f"{s} {j}")
    check("the default-following seat now carries handover", "handover" in seat_skills(plain), seat_skills(plain))
    check("the seat with its own selection is left alone",
          "handover" not in seat_skills(custom) and "independent-evidence" not in seat_skills(custom),
          seat_skills(custom))
    # Global dial, and clearing it.
    http("POST", "/v1/skills/prefs", {"scope": "global", "skill": "review-craft", "enabled": False})
    check("a global disable reaches the default-following seat",
          "review-craft" not in seat_skills(plain), seat_skills(plain))
    http("POST", "/v1/skills/prefs", {"scope": "global", "skill": "review-craft", "enabled": None})
    check("clearing the global dial brings it back", "review-craft" in seat_skills(plain), seat_skills(plain))
    check("the seat dial is refused here, pointing at the seat's own endpoint",
          http("POST", "/v1/skills/prefs",
               {"scope": "seat", "reference": plain, "skill": "handover", "enabled": True})[0] != 200)

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + "; ".join(fails)); cleanup(); sys.exit(1)
    print("ALL SKILL LIBRARY CHECKS PASSED"); cleanup()

if __name__ == "__main__":
    try: main()
    except Exception as e:
        import traceback; traceback.print_exc(); cleanup(); sys.exit(1)
