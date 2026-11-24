#!/usr/bin/env python3
"""Role templates, a team's standing rules, observers, and flood protection.

Four things an operator sets, all of which are about what a seat is TOLD or
ALLOWED, and none of which need a node to prove:

  * ROLE TEMPLATES -- a ready-made brief per role, so the fourth reviewer on
    a fleet gets the same brief as the first. A fleet can keep its own, and
    its own must win over the built-in of the same name, or a local change
    disappears at the next upgrade.

  * A TEAM'S RULES -- what everybody in one session is told, over and above
    their own brief. Only the operator and an orchestrating seat may write
    them: rules an agent can rewrite are not rules.

  * AN OBSERVER -- a seat that reads and reports and cannot steer. Enforced
    in the hub on every call, because a brief saying "do not give
    instructions" is a request, and a model that believes it is helping will
    route around a request.

  * FLOOD PROTECTION -- a seat sending too much, and two seats talking only
    to each other. Off until somebody sets it, and when it fires the agent is
    told what to do instead rather than having its message swallowed.
"""
import json, os, shutil, signal, socket, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
DOOR = "/v1/mcp"


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT = free()
HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-roles-")
DB = f"{T}/h.db"
PROMPTS = f"{T}/prompts"
os.makedirs(f"{PROMPTS}/roles")
os.makedirs(f"{PROMPTS}/skills")

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None, token=None, headers=None):
    r = urllib.request.Request(HUB + p, data=json.dumps(b).encode() if b is not None else None,
                               method=m)
    r.add_header("content-type", "application/json")
    if token:
        r.add_header("authorization", f"Bearer {token}")
    for k, v in (headers or {}).items():
        r.add_header(k, v)
    try:
        with urllib.request.urlopen(r, timeout=20) as x:
            raw = x.read()
            return x.status, (json.loads(raw) if raw.strip() else {}), dict(x.headers)
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            body = json.loads(raw) if raw.strip() else {}
        except Exception:
            body = {"raw": raw.decode("utf8", "replace")}
        return e.code, body, dict(e.headers)


def until(fn, seconds=20.0, step=0.2):
    end = time.time() + seconds
    while time.time() < end:
        try:
            v = fn()
            if v:
                return v
        except Exception:
            pass
        time.sleep(step)
    return None


def q(sql, args=()):
    c = sqlite3.connect(DB)
    c.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in c.execute(sql, args)]
    finally:
        c.close()


RPC = [0]


def rpc(method, params=None, token=None, session=None):
    body = {"jsonrpc": "2.0", "method": method}
    RPC[0] += 1
    body["id"] = RPC[0]
    if params is not None:
        body["params"] = params
    headers = {"accept": "application/json, text/event-stream"}
    if session:
        headers["mcp-session-id"] = session
    return http("POST", DOOR, body, token=token, headers=headers)


def mint(label, session, observer=False):
    argv = [f"{BIN}/nexmesh-hub", "--db", DB, "participant-token", label,
            "--session", session]
    if observer:
        argv.append("--observer")
    out = subprocess.run(argv, capture_output=True, text=True)
    lines = [l for l in out.stdout.splitlines() if l.startswith("token:")]
    if not lines:
        return None, out.stdout + out.stderr
    return lines[0].split()[1], out.stdout


def door_seat(token, name, role=None):
    """Take a seat through the MCP front door, as any outside client would."""
    url = f"{DOOR}?name={name}" + (f"&role={role}" if role else "")
    body = {"jsonrpc": "2.0", "id": 999, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26",
                       "clientInfo": {"name": name, "version": "1"}}}
    s, j, h = http("POST", url, body, token=token,
                   headers={"accept": "application/json, text/event-stream"})
    return s, j, (h.get("Mcp-Session-Id") or h.get("mcp-session-id"))


def call(token, session, tool, args):
    s, j, _ = rpc("tools/call", {"name": tool, "arguments": args},
                  token=token, session=session)
    content = ""
    try:
        content = j["result"]["content"][0]["text"]
    except Exception:
        content = json.dumps(j)
    is_error = bool(j.get("result", {}).get("isError")) or "error" in j
    return content, is_error


def main():
    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        print("FAILED (1): build first: cargo build --workspace")
        return 1
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}",
         "--prompts-dir", PROMPTS, "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    if not until(lambda: http("GET", "/healthz")[1].get("ok")):
        print("FAILED (1): hub did not start")
        return 1
    sid = http("POST", "/v1/sessions", {"name": "roletest"})[1]["session_id"]

    # ======================================================================
    print("=== 1. the role templates this hub has ===")
    s, j, _ = http("GET", "/v1/roles")
    roles = {r["id"]: r for r in (j.get("roles") or [])}
    check("the roles endpoint answers", s == 200 and bool(roles), json.dumps(j)[:200])
    # Named one by one: these are the roles this fleet works in, and a
    # rename should fail here rather than when somebody makes that seat.
    for want in ["orchestrator", "supervisor", "coder", "engineer", "hacker",
                 "reviewer", "planner", "coordinator", "designer", "sysadmin",
                 "reverser", "webmaster", "assistant", "observer"]:
        check(f"there is a {want} template", want in roles, str(sorted(roles)))
    check("each one says what it is for in a line",
          all(r.get("summary") for r in roles.values()))
    check("and carries skills by name, not by copying them",
          any(r.get("skills") for r in roles.values()))

    s, j, _ = http("GET", "/v1/roles/reviewer")
    check("one can be read in full before it is applied",
          s == 200 and "YOUR ROLE: REVIEWER" in (j.get("prompt") or ""),
          json.dumps(j)[:200])
    check("and the skill it carries is rendered into it",
          "review-craft" in (j.get("prompt") or ""))
    check("without its front matter, which is for a CLI and not for a model",
          "description:" not in (j.get("prompt") or ""))

    # ======================================================================
    print("\n=== 2. a template becomes a seat's own prompt ===")
    slot = http("POST", f"/v1/sessions/{sid}/slots",
                {"cli_id": "claude", "display_name": "Rev", "role": "reviewer"})[1]["slot_id"]
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"template": "reviewer"})
    check("applying a template succeeds", s == 200, json.dumps(j)[:200])
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("and it is STORED on the seat, not referenced",
          "YOUR ROLE: REVIEWER" in (j.get("system_prompt") or ""),
          json.dumps(j)[:200])
    check("so it can be read back and edited afterwards",
          "review-craft" in (j.get("system_prompt") or ""))

    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt",
                   {"template": "sysadmin", "skills": ["live-systems"]})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("the skills can be chosen instead of the template's own",
          "live-systems" in (j.get("system_prompt") or "")
          and "handover" not in (j.get("system_prompt") or ""),
          json.dumps(j)[:300])

    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"template": "no-such-role"})
    check("a template that does not exist is refused, and says what there is",
          s >= 400 and "reviewer" in json.dumps(j), json.dumps(j)[:200])
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt",
                   {"template": "coder", "system_prompt": "and also this"})
    check("a template AND a hand-written prompt is refused rather than guessed",
          s >= 400, json.dumps(j)[:200])

    # ======================================================================
    print("\n=== 2b. the skills are selected and deselected per seat ===")
    # A seat remembers what it was built from, so one skill can be turned on
    # or off without restating the template and the whole list -- which is
    # what a checkbox needs, and what storing only the rendered text made
    # impossible.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"template": "coder"})
    check("applying a template records the choice", s == 200, json.dumps(j)[:200])
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("the seat says which template it is built from", j.get("template") == "coder",
          json.dumps(j.get("template")))
    check("and which skills it carries", j.get("skills") == j.get("template_skills"),
          f'{j.get("skills")} vs {j.get("template_skills")}')
    check("and offers every skill this hub has, to tick",
          "live-systems" in (j.get("skills_available") or [])
          and len(j.get("skills_available") or []) >= 9,
          str(j.get("skills_available")))

    # Deselect one. Nothing else moves, and the brief loses that section.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"skills_off": ["handover"]})
    check("one can be deselected without naming the template again", s == 200,
          json.dumps(j)[:200])
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("it is gone from the selection", "handover" not in (j.get("skills") or []),
          str(j.get("skills")))
    check("the others are untouched", "small-steps" in (j.get("skills") or []),
          str(j.get("skills")))
    check("and the brief no longer carries that section",
          "HOW TO DO THIS WELL: handover" not in (j.get("system_prompt") or ""))
    check("while the role's own brief stays",
          "YOUR ROLE: CODER" in (j.get("system_prompt") or ""))

    # Select one the template does not suggest.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"skills_on": ["live-systems"]})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("one the template does not suggest can be selected",
          "live-systems" in (j.get("skills") or []), str(j.get("skills")))
    check("and the brief carries it",
          "HOW TO DO THIS WELL: live-systems" in (j.get("system_prompt") or ""))
    check("the template still says what it suggests, separately",
          "live-systems" not in (j.get("template_skills") or []),
          str(j.get("template_skills")))

    # Toggling the same one twice is not an error, and does not duplicate it.
    http("POST", f"/v1/slots/{slot}/prompt", {"skills_on": ["live-systems"]})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("selecting it again changes nothing",
          (j.get("skills") or []).count("live-systems") == 1, str(j.get("skills")))

    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"skills_on": ["no-such-skill"]})
    check("a skill this hub does not have is refused, and says what it has",
          s >= 400 and "handover" in json.dumps(j), json.dumps(j)[:200])

    # All of them off leaves the role's own brief alone.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"skills": []})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("deselecting everything keeps the role brief", j.get("skills") == []
          and "YOUR ROLE: CODER" in (j.get("system_prompt") or ""),
          json.dumps(j.get("skills")))
    check("and carries no sections at all",
          "HOW TO DO THIS WELL" not in (j.get("system_prompt") or ""))

    # Writing the prompt by hand takes it back, and the skills stop being
    # editable -- because re-rendering would throw the operator's words away.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt",
                   {"system_prompt": "You are ours, and we wrote this ourselves."})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("writing it by hand drops the template link", j.get("template") is None,
          json.dumps(j.get("template")))
    check("and the words stand",
          j.get("system_prompt") == "You are ours, and we wrote this ourselves.",
          json.dumps(j.get("system_prompt"))[:120])
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt", {"skills_on": ["handover"]})
    check("a skill cannot then be toggled, and the refusal says what to do",
          s >= 400 and "template" in json.dumps(j), json.dumps(j)[:250])
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("and the hand-written prompt was not touched by the attempt",
          j.get("system_prompt") == "You are ours, and we wrote this ourselves.")

    # A template AND a deselection in one go: start from the template's own
    # defaults, minus that one.
    s, j, _ = http("POST", f"/v1/slots/{slot}/prompt",
                   {"template": "sysadmin", "skills_off": ["handover"]})
    s, j, _ = http("GET", f"/v1/slots/{slot}/prompt")
    check("a template can be applied without one of its own skills",
          j.get("template") == "sysadmin"
          and "live-systems" in (j.get("skills") or [])
          and "handover" not in (j.get("skills") or []),
          json.dumps(j.get("skills")))

    # ======================================================================
    print("\n=== 3. a fleet's own template wins over the built-in ===")
    # What makes a local change survive an upgrade.
    open(f"{PROMPTS}/roles/coder.toml", "w").write(
        'id = "coder"\ntitle = "Our Coder"\nsummary = "ours"\n'
        'skills = ["house-style"]\nprompt = "Do it the way we do it here."\n')
    open(f"{PROMPTS}/skills/house-style.md", "w").write(
        "---\nname: house-style\n---\n\nTabs, and no clever bits.\n")
    # A file that will not parse must not take the others away.
    open(f"{PROMPTS}/roles/broken.toml", "w").write("this is not toml = = =")
    procs[0].send_signal(signal.SIGTERM)
    procs[0].wait(timeout=20)
    procs[0] = subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}",
         "--prompts-dir", PROMPTS, "serve"],
        stdout=open(f"{T}/hub2.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1"))
    until(lambda: http("GET", "/healthz")[1].get("ok"))
    s, j, _ = http("GET", "/v1/roles/coder")
    check("the local one is used", j.get("title") == "Our Coder", json.dumps(j)[:200])
    check("including a skill only this fleet has",
          "Tabs, and no clever bits." in (j.get("prompt") or ""), json.dumps(j)[:300])
    s, j, _ = http("GET", "/v1/roles")
    ids = [r["id"] for r in (j.get("roles") or [])]
    check("the other built-in templates are all still there",
          "reviewer" in ids and len(ids) >= 14, str(ids))
    check("and a template that will not parse is ignored, not fatal",
          "broken" not in ids and http("GET", "/healthz")[1].get("ok"))
    check("the hub says which file it would not read",
          "broken.toml" in open(f"{T}/hub2.log").read())

    # ======================================================================
    print("\n=== 4. a team's standing rules ===")
    s, j, _ = http("GET", f"/v1/sessions/{sid}")
    check("a team starts with none", s == 200 and not j.get("instructions"),
          json.dumps(j)[:200])
    s, j, _ = http("POST", f"/v1/sessions/{sid}/instructions",
                   {"instructions": "Never deploy on a Friday."})
    check("the operator can set them", s == 200 and j.get("changed") is True,
          json.dumps(j)[:200])
    s, j, _ = http("GET", f"/v1/sessions/{sid}")
    check("and they are the team's from then on",
          j.get("instructions") == "Never deploy on a Friday.", json.dumps(j)[:200])
    # Everybody already running is TOLD: the rules only reach a seat's prompt
    # when its next process starts, which may be days.
    s, j, _ = http("GET", f"/v1/sessions/{sid}/log?limit=50")
    said = json.dumps(j.get("messages") or [])
    check("everybody already in the team is told in the chat",
          "Never deploy on a Friday." in said, said[:300])
    s, j, _ = http("POST", f"/v1/sessions/{sid}/instructions",
                   {"instructions": "Never deploy on a Friday."})
    check("setting the same rules again says nothing to anybody",
          j.get("changed") is False, json.dumps(j)[:200])
    s, j, _ = http("POST", f"/v1/sessions/{sid}/instructions",
                   {"instructions": "x" * 9000})
    check("rules nobody could hold in their head are refused, with the limit",
          s >= 400 and "limit" in json.dumps(j), json.dumps(j)[:200])

    # ---- and from the chat, which is where the dashboard types ----------
    s, j, _ = http("POST", f"/v1/sessions/{sid}/messages", {"text": "/rules"})
    check("the chat can show them", s == 200 and "Friday" in str(j.get("said", "")),
          json.dumps(j)[:200])
    s, j, _ = http("POST", f"/v1/sessions/{sid}/messages",
                   {"text": "/rules Never deploy on a Friday.  Two spaces kept."})
    check("and set them, keeping the line as it was written",
          s == 200 and j.get("command") is True, json.dumps(j)[:200])
    s, j, _ = http("GET", f"/v1/sessions/{sid}")
    check("a sentence is not reflowed on its way through the chat",
          j.get("instructions") == "Never deploy on a Friday.  Two spaces kept.",
          json.dumps(j)[:200])
    s, j, _ = http("POST", f"/v1/sessions/{sid}/messages",
                   {"text": "/rules Never deploy on a Friday."})

    # ---- and from inside: an orchestrating seat may, an agent may not -----
    tok, out = mint("watcher-setup", sid)
    check("a participant token can be minted", bool(tok), out)
    s, j, msid = door_seat(tok, "Bob", "engineer")
    check("an outside client can take a seat", s == 200 and bool(msid), json.dumps(j)[:200])
    text, is_err = call(tok, msid, "team_rules", {})
    check("any seat can READ the team's rules",
          "Never deploy on a Friday." in text, text[:200])
    text, is_err = call(tok, msid, "set_team_rules", {"text": "Deploy whenever."})
    check("but an ordinary agent cannot write them", is_err or "orchestrator" in text,
          text[:300])
    s, j, _ = http("GET", f"/v1/sessions/{sid}")
    check("and the rules are unchanged by the attempt",
          j.get("instructions") == "Never deploy on a Friday.", json.dumps(j)[:200])

    bob = q("SELECT id FROM slots WHERE display_name = 'Bob'")[0]["id"]
    s, j, _ = http("POST", f"/v1/slots/{bob}/authority", {"authority": "orchestrator"})
    check("the operator can make that seat an orchestrator", s == 200, json.dumps(j)[:200])
    text, is_err = call(tok, msid, "set_team_rules",
                        {"text": "Never deploy on a Friday.\nAsk before touching the DB."})
    check("an orchestrating seat CAN set them", not is_err, text[:300])
    s, j, _ = http("GET", f"/v1/sessions/{sid}")
    check("and they take", "Ask before touching the DB." in (j.get("instructions") or ""),
          json.dumps(j)[:300])
    s, j, _ = http("GET", f"/v1/sessions/{sid}/log?limit=80")
    check("with the team told that they changed",
          "Ask before touching the DB." in json.dumps(j.get("messages") or []))

    # ======================================================================
    print("\n=== 5. an observer watches and cannot steer ===")
    otok, out = mint("browser", sid, observer=True)
    check("an observer token can be minted", bool(otok), out)
    check("and it says where to point an MCP client", "/v1/mcp" in out, out[:300])
    bad_tok = subprocess.run(
        [f"{BIN}/nexmesh-hub", "--db", DB, "participant-token", "loose", "--observer"],
        capture_output=True, text=True)
    check("an observer token with no team named is refused",
          bad_tok.returncode != 0, bad_tok.stdout + bad_tok.stderr)

    s, j, osid = door_seat(otok, "Watcher", "observer")
    check("the observer takes its seat", s == 200 and bool(osid), json.dumps(j)[:200])
    seat = q("SELECT authority FROM slots WHERE display_name = 'Watcher'")
    check("and the hub records it as an observer, from the TOKEN",
          seat and seat[0]["authority"] == "observer", str(seat))

    s, j, _ = rpc("tools/list", {}, token=otok, session=osid)
    names = [t["name"] for t in (j.get("result", {}).get("tools") or [])]
    check("it is shown the reading tools",
          all(n in names for n in ["check_team_status", "get_plan", "query_knowledge",
                                   "recall", "reply", "team_rules"]),
          str(sorted(names)))
    check("and never told that send_message or broadcast exist",
          "send_message" not in names and "broadcast" not in names, str(sorted(names)))
    check("nor any of the fleet tools",
          not any(n in names for n in ["list_seats", "elect", "add_agent"]),
          str(sorted(names)))
    check("nor anything that writes",
          not any(n in names for n in ["store_knowledge", "update_plan", "acquire_file",
                                       "set_team_rules", "fold_context"]),
          str(sorted(names)))
    check("though it may read what the team put away, which is most of the point",
          all(n in names for n in ["search_context", "unfold_context", "context_map"]),
          str(sorted(names)))
    check("it is given ask_progress instead, to ask one seat how it is getting on",
          "ask_progress" in names, str(sorted(names)))

    # Calling one anyway: a model that has seen a tool will try it from
    # memory, and the refusal has to stand on its own.
    text, is_err = call(otok, osid, "send_message", {"to": "Bob", "text": "do this instead"})
    check("calling send_message anyway is REFUSED, not merely unlisted",
          is_err or "cannot" in text.lower() or "observes" in text.lower(), text[:300])
    check("and the refusal explains what the seat is for",
          "observe" in text.lower() or "reads" in text.lower(), text[:300])
    text, is_err = call(otok, osid, "store_knowledge",
                        {"key": "k", "value": "v", "category": "c"})
    check("so is writing to the team's memory", is_err or "observes" in text.lower(),
          text[:200])
    text, is_err = call(otok, osid, "list_seats", {})
    check("so is reaching for a fleet tool", is_err or "not" in text.lower(), text[:200])

    text, is_err = call(otok, osid, "check_team_status", {})
    check("but reading the team's state works", not is_err, text[:200])
    text, is_err = call(otok, osid, "team_rules", {})
    check("and reading its rules", "Friday" in text, text[:200])
    text, is_err = call(otok, osid, "query_knowledge", {"query": "anything"})
    check("and the team's memory, which is most of the point", not is_err, text[:200])

    text, is_err = call(otok, osid, "ask_progress", {"agent": "Bob", "want": "eta"})
    check("it can ask one seat for an ETA", not is_err, text[:300])
    msgs = q("SELECT body FROM messages WHERE to_slot = ? ORDER BY id DESC LIMIT 1", (bob,))
    body = msgs[0]["body"] if msgs else ""
    check("the question that arrives is the HUB's words, not the observer's",
          "watching this team" in body and "expect this to be done" in body, body[:300])
    check("and it tells the seat the observer cannot give it instructions",
          "cannot give you instructions" in body, body[:300])
    text, is_err = call(otok, osid, "ask_progress", {"agent": "Bob", "want": "nonsense"})
    check("asking for something that is not one of the three is refused",
          is_err or "not something to ask" in text, text[:200])

    # ======================================================================
    print("\n=== 6. flood protection ===")
    s, j, _ = http("POST", f"/v1/slots/{bob}/authority", {"authority": "agent"})
    s, j, _ = http("POST", f"/v1/sessions/{sid}/messages", {"text": "/flood Bob 3 -"})
    check("a rate can be set on a seat", s == 200 and j.get("command") is True,
          json.dumps(j)[:200])
    check("and it says what will happen", "3 a minute" in str(j.get("said", "")),
          json.dumps(j)[:200])
    # Bob is a door seat and can send; the first three go, the fourth does not.
    sent = 0
    refused = ""
    for i in range(5):
        text, is_err = call(tok, msid, "broadcast", {"text": f"message {i}"})
        if is_err or "Not sent" in text:
            refused = text
            break
        sent += 1
    check("the messages under the limit are sent", sent == 3, f"sent {sent}")
    check("and the one over it is refused", bool(refused), refused[:200])
    check("the refusal tells the agent what to do instead",
          "store_knowledge" in refused or "notify_operator" in refused, refused[:300])
    told = q("SELECT body FROM operator_messages WHERE body LIKE '%refused messages%'")
    check("the operator is told", bool(told), str(told)[:200])
    for _ in range(3):
        call(tok, msid, "broadcast", {"text": "again"})
    told2 = q("SELECT COUNT(*) AS n FROM operator_messages WHERE body LIKE '%refused%'")
    check("once, not once per refused message", told2[0]["n"] == 1, str(told2))
    audited = q("SELECT COUNT(*) AS n FROM audit WHERE action = 'message_flood'")
    check("though every refusal is recorded", audited[0]["n"] >= 2, str(audited))

    s, j, _ = http("POST", f"/v1/sessions/{sid}/messages", {"text": "/flood Bob off"})
    check("and it can be turned off again", s == 200, json.dumps(j)[:200])
    text, is_err = call(tok, msid, "broadcast", {"text": "after the limit was lifted"})
    check("after which messages go through", not is_err and "Not sent" not in text,
          text[:200])

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails))
        return 1
    print("ALL ROLE AND OBSERVER CHECKS PASSED")
    return 0


if __name__ == "__main__":
    code = 1
    try:
        code = main()
    finally:
        for p in procs:
            try:
                p.send_signal(signal.SIGTERM)
                p.wait(timeout=10)
            except Exception:
                try:
                    p.kill()
                except Exception:
                    pass
        if code == 0:
            shutil.rmtree(T, ignore_errors=True)
        else:
            print(f"  (logs: {T})")
    sys.exit(code)
