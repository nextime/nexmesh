#!/usr/bin/env python3
"""Signing in to the dashboard with a username and a password.

The dashboard used to want an operator token pasted into the browser. These
checks cover what replaced it: an account set up on first use, against a code
the hub prints, because this hub may answer on a public address and whoever
arrives first must not simply be able to claim it.
"""
import http.cookiejar, json, os, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR); BIN = f"{ROOT}/target/debug"
PORT = 17961; HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-webauth-")
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

JAR = http.cookiejar.CookieJar()
OPENER = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(JAR))

def req(m, p, b=None, token=None, use_cookies=True):
    r = urllib.request.Request(HUB+p, data=json.dumps(b).encode() if b is not None else None, method=m)
    r.add_header("content-type", "application/json")
    if token: r.add_header("authorization", f"Bearer {token}")
    opener = OPENER if use_cookies else urllib.request.build_opener()
    try:
        with opener.open(r, timeout=30) as x:
            raw = x.read()
            return x.status, (json.loads(raw) if raw.strip() else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try: return e.code, json.loads(raw)
        except Exception: return e.code, {"raw": raw.decode("utf8", "replace")}

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

def hub_cli(*a):
    return subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, *a], capture_output=True, text=True)

def session_cookie():
    return next((c.value for c in JAR if c.name == "nexmesh_session"), None)

GOOD = "a-long-enough-password"

def main():
    procs.append(subprocess.Popen([f"{BIN}/nexmesh-hub", "--db", DB, "--listen",
        f"127.0.0.1:{PORT}", "serve"], stdout=open(f"{T}/hub.log","w"),
        stderr=subprocess.STDOUT, env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    until(lambda: req("GET", "/healthz")[1].get("ok"))

    print("=== a fresh hub says it has no account ===")
    s, j = req("GET", "/v1/auth/state")
    check("the page can ask, without being signed in", s == 200, f"{s} {j}")
    check("and is told there is no account yet", j.get("configured") is False, str(j))
    check("nor anyone signed in", j.get("user") is None, str(j))

    print("\n=== the first account cannot just be claimed ===")
    s, j = req("POST", "/v1/auth/setup",
               {"setup_code": "guessed", "username": "intruder", "password": GOOD})
    check("without a setup code it is refused", s == 400, f"{s} {j}")
    check("and says where the code comes from", "dashboard-setup-code" in str(j), str(j))
    check("no account was created", not q("SELECT 1 FROM dashboard_users"), "")

    out = hub_cli("dashboard-setup-code").stdout
    code = next((l.split(": ", 1)[1].strip() for l in out.splitlines()
                 if l.startswith("setup code:")), None)
    check("the hub prints a setup code", bool(code), out)
    s, j = req("POST", "/v1/auth/setup",
               {"setup_code": "still-wrong", "username": "intruder", "password": GOOD})
    check("a wrong code is still refused", s == 400, f"{s} {j}")

    print("\n=== setting up the first account ===")
    s, j = req("POST", "/v1/auth/setup",
               {"setup_code": code, "username": "Stefy", "password": "short"})
    check("a short password is refused with the rule",
          s == 400 and "12 characters" in str(j), f"{s} {j}")
    s, j = req("POST", "/v1/auth/setup",
               {"setup_code": code, "username": "Stefy", "password": GOOD})
    check("the account is created", s == 200 and j.get("ok"), f"{s} {j}")
    check("the username is lower-cased", j.get("user") == "stefy", str(j))
    s, j = req("POST", "/v1/auth/setup",
               {"setup_code": code, "username": "second", "password": GOOD})
    check("the code cannot be used twice", s == 400, f"{s} {j}")
    row = q("SELECT password_hash FROM dashboard_users")[0]
    check("the password is stored as argon2id, not as itself",
          row["password_hash"].startswith("$argon2id$") and GOOD not in row["password_hash"],
          row["password_hash"][:40])

    print("\n=== an account closes the open hub ===")
    s, j = req("GET", "/v1/nodes", use_cookies=False)
    check("the operator API now requires authentication", s == 401, f"{s} {j}")
    check("and says how to get in", "sign in" in str(j).lower(), str(j))

    print("\n=== signing in ===")
    s, j = req("POST", "/v1/auth/login", {"username": "stefy", "password": "wrong"})
    wrong_pass = str(j)
    check("a wrong password is refused", s == 401, f"{s} {j}")
    s, j = req("POST", "/v1/auth/login", {"username": "nobody", "password": GOOD})
    check("so is an unknown account", s == 401, f"{s} {j}")
    check("with the SAME message, so the form does not report who exists",
          str(j) == wrong_pass, f"{j} vs {wrong_pass}")
    s, j = req("POST", "/v1/auth/login", {"username": "  STEFY  ", "password": GOOD})
    check("signing in works, case and spacing aside", s == 200, f"{s} {j}")
    check("and sets a session cookie", bool(session_cookie()), str([c.name for c in JAR]))
    cookie = next(c for c in JAR if c.name == "nexmesh_session")
    check("the cookie is HttpOnly, so a script cannot read it",
          cookie.has_nonstandard_attr("HttpOnly") or cookie.has_nonstandard_attr("httponly"),
          str(cookie.__dict__))
    check("only the hash of it is stored, not the value itself",
          q("SELECT id_sha256 FROM dashboard_sessions")[0]["id_sha256"] != cookie.value, "")

    print("\n=== the session is as good as a token ===")
    s, j = req("GET", "/v1/nodes")
    check("the operator API accepts the cookie", s == 200, f"{s} {j}")
    s, j = req("GET", "/v1/dashboard/state")
    check("and so does the dashboard snapshot", s == 200, f"{s} {j}")
    s, j = req("GET", "/v1/auth/state")
    check("the page is told who is signed in", j.get("user") == "stefy", str(j))

    print("\n=== wrong guesses cost something ===")
    locked = None
    for _ in range(10):
        s, j = req("POST", "/v1/auth/login", {"username": "stefy", "password": "nope"})
        if s == 429:
            locked = j
            break
    check("repeated failures lock the account", locked is not None, "a public form invites guessing")
    check("reported as rate limiting, not as a bad password",
          locked and "locked" in str(locked), str(locked))
    s, j = req("POST", "/v1/auth/login", {"username": "stefy", "password": GOOD})
    check("even the right password waits while it is locked", s == 429, f"{s} {j}")
    # An operator can always get back in from the host.
    hub_cli("dashboard-passwd", "stefy", "--password", "another-long-password")
    s, j = req("POST", "/v1/auth/login", {"username": "stefy", "password": "another-long-password"})
    check("resetting the password from the host clears the lock", s == 200, f"{s} {j}")

    print("\n=== a password change ends old sessions ===")
    JAR.clear()
    req("POST", "/v1/auth/login", {"username": "stefy", "password": "another-long-password"})
    old = session_cookie()
    check("signed in again", bool(old))
    hub_cli("dashboard-passwd", "stefy", "--password", "a-third-long-password")
    s, j = req("GET", "/v1/nodes")
    check("the old session stops working at once", s == 401, f"{s} {j}")

    print("\n=== signing out ===")
    JAR.clear()
    req("POST", "/v1/auth/login", {"username": "stefy", "password": "a-third-long-password"})
    check("signed in", req("GET", "/v1/nodes")[0] == 200)
    s, j = req("POST", "/v1/auth/logout", {})
    check("signing out is accepted", s == 200, f"{s} {j}")
    check("the session row is gone", not q("SELECT 1 FROM dashboard_sessions"), "")

    print("\n=== more accounts, only from inside ===")
    s, j = req("POST", "/v1/auth/users", {"username": "helper", "password": GOOD},
               use_cookies=False)
    check("adding an account needs authentication", s == 401, f"{s} {j}")
    JAR.clear()
    req("POST", "/v1/auth/login", {"username": "stefy", "password": "a-third-long-password"})
    s, j = req("POST", "/v1/auth/users", {"username": "helper", "password": GOOD})
    check("a signed-in user can add one", s == 200 and j.get("user") == "helper", f"{s} {j}")
    s, j = req("POST", "/v1/auth/users", {"username": "helper", "password": GOOD})
    check("a duplicate is refused", s == 400 and "already" in str(j), f"{s} {j}")
    JAR.clear()
    s, j = req("POST", "/v1/auth/login", {"username": "helper", "password": GOOD})
    check("the new account can sign in", s == 200, f"{s} {j}")

    print("\n=== the page itself is still served without a login ===")
    s, _ = req("GET", "/healthz", use_cookies=False)
    check("health is open", s == 200)
    r = urllib.request.urlopen(HUB + "/", timeout=20)
    body = r.read().decode()
    check("the dashboard page loads unauthenticated", r.status == 200)
    check("and offers a sign-in form", 'id="loginForm"' in body, body[:200])
    check("and a first-run form for a hub with no account",
          'id="setupForm"' in body, body[:200])
    check("with a graph tab", '"graph"' in body, body[:200])

    print("\n=== an admin command will not mint into a database that is not there ===")
    # The silent version of this cost an hour: a bare `dashboard-setup-code`
    # on a service install created an empty ~/.nexmesh/hub.db, printed a code
    # into it, and the running hub -- serving a different file -- rejected
    # every attempt to use it.
    empty_home = tempfile.mkdtemp(prefix="nexmesh-nohome-")
    r = subprocess.run([f"{BIN}/nexmesh-hub", "dashboard-setup-code"],
                       capture_output=True, text=True,
                       env={k: v for k, v in os.environ.items()
                            if k != "NEXMESH_DB"} | {"HOME": empty_home})
    check("it refuses", r.returncode != 0, r.stdout + r.stderr)
    check("naming the database it looked for",
          "/.nexmesh/hub.db" in (r.stdout + r.stderr), r.stdout + r.stderr)
    check("and saying how to point it at the right one",
          "NEXMESH_DB" in (r.stdout + r.stderr), r.stdout + r.stderr)
    check("and creates nothing",
          not os.path.exists(os.path.join(empty_home, ".nexmesh", "hub.db")))

    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + ", ".join(fails)); return 1
    print("ALL DASHBOARD LOGIN CHECKS PASSED"); return 0

try: rc = main()
except Exception:
    import traceback; traceback.print_exc(); rc = 2
finally: cleanup()
sys.exit(rc)
