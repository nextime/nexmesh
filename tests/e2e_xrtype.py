#!/usr/bin/env python3
"""Typing into an agent's terminal from the room, end to end.

The immersive view attaches to an agent over the same websocket the dashboard
and `nexmesh attach` use, and the keyboard drawn in the room sends keys to it.
This drives the real page in headless chromium against a real hub, a real node
and a real interactive agent, presses keys on that keyboard the way a pointer
does, and then checks the agent's own output came back onto the panel. Every
link in the chain is the real one: keyboard, websocket, hub, node, pty, and the
VT emulator reading the screen back out.
"""
import importlib.util, json, os, shutil, signal, socket, subprocess, sys, tempfile, time
import urllib.request, urllib.error

for _v in ("CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "OPENCODE_SESSION_ID",
           "QWEN_SESSION_ID", "GEMINI_SESSION_ID", "NEXMESH_CLI_SESSION"):
    os.environ.pop(_v, None)

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
BIN = f"{ROOT}/target/debug"
spec = importlib.util.spec_from_file_location("xrshots", os.path.join(SCR, "xr-shots.py"))
xrshots = importlib.util.module_from_spec(spec)
spec.loader.exec_module(xrshots)
CDP = xrshots.CDP
APP = "window.nexmeshXR"


def free():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT, DBG = free(), free()
HUB = f"http://127.0.0.1:{PORT}"
T = tempfile.mkdtemp(prefix="nexmesh-xrtype-")
DB, SOCK = f"{T}/h.db", f"{T}/n.sock"
WS = f"{T}/ws"
os.makedirs(WS)
CLIS = f"{T}/clis"
os.makedirs(CLIS)
for f in os.listdir(f"{ROOT}/clis"):
    if f.endswith(".toml"):
        shutil.copy(f"{ROOT}/clis/{f}", CLIS)
_d = open(f"{SCR}/fake-cli/faketui.toml").read().replace(
    'extra_paths = ["tests/fake-cli"]', f'extra_paths = ["{SCR}/fake-cli"]')
open(f"{CLIS}/faketui.toml", "w").write(_d)

procs, fails = [], []


def check(label, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {label}" + (f"  -- {detail}" if detail and not cond else ""))
    if not cond:
        fails.append(label)


def http(m, p, b=None, tok=None):
    r = urllib.request.Request(HUB + p, data=json.dumps(b).encode() if b is not None else None,
                               method=m)
    r.add_header("content-type", "application/json")
    if tok:
        r.add_header("authorization", "Bearer " + tok)
    try:
        with urllib.request.urlopen(r, timeout=20) as x:
            return x.status, json.loads(x.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def main():
    chrome = shutil.which("chromium") or shutil.which("google-chrome")
    if not chrome:
        print("SKIP no chromium")
        return 0
    if not os.path.exists(f"{BIN}/nexmesh-hub"):
        print("FAILED (1): build first: cargo build --workspace")
        return 1

    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "enrol", "machine-b"],
                         capture_output=True, text=True).stdout
    tok = [l.split()[1] for l in out.splitlines() if l.startswith("token:")][0]
    out = subprocess.run([f"{BIN}/nexmesh-hub", "--db", DB, "operator-token", "me"],
                         capture_output=True, text=True).stdout
    optok = [w for l in out.splitlines() for w in l.split() if len(w) > 20][0]
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-hub", "--db", DB, "--listen", f"127.0.0.1:{PORT}", "serve"],
        stdout=open(f"{T}/hub.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, RUST_LOG="info", NO_COLOR="1")))
    for _ in range(100):
        try:
            if http("GET", "/healthz")[1].get("ok"):
                break
        except Exception:
            pass
        time.sleep(0.2)
    procs.append(subprocess.Popen(
        [f"{BIN}/nexmesh-node", "--hub", f"ws://127.0.0.1:{PORT}", "--name", "machine-b",
         "--clis", CLIS, "--sock", SOCK, "--mcpd", f"{BIN}/nexmesh-mcpd"],
        stdout=open(f"{T}/node.log", "w"), stderr=subprocess.STDOUT,
        env=dict(os.environ, NEXMESH_TOKEN=tok, RUST_LOG="info", NO_COLOR="1")))
    ok = False
    for _ in range(120):
        s, j = http("GET", "/v1/nodes", tok=optok)
        if j.get("nodes") and j["nodes"][0].get("connected"):
            ok = "faketui" in j["nodes"][0]["clis"]
            break
        time.sleep(0.25)
    check("node online with the interactive test CLI", ok)

    sid = http("POST", "/v1/sessions", {"name": "xrtype", "workspaces": [
        {"name": "local", "kind": "node_local", "node": "machine-b",
         "mounts": {"machine-b": WS}}]}, tok=optok)[1]["session_id"]
    s, j = http("POST", f"/v1/sessions/{sid}/slots", {
        "cli_id": "faketui", "mode": "interactive", "display_name": "Max",
        "role": "engineer", "workspace": "local", "task": "be typed at",
        "placement": {"node": "machine-b"}}, tok=optok)
    slot = j["slot_id"]
    s, j = http("POST", f"/v1/slots/{slot}/spawn", tok=optok)
    check("interactive agent spawned", s == 200, str(j))
    aid = None
    for _ in range(80):
        s, j = http("GET", f"/v1/sessions/{sid}/resolve/Max", tok=optok)
        if s == 200 and j.get("agent_id"):
            aid = j["agent_id"]
            break
        time.sleep(0.25)
    check("the agent is registered", aid is not None, str(j))
    if not aid:
        return 1

    # The page, served by the hub itself, holding the operator token the way a
    # browser would: the attach websocket cannot carry a header, so the token
    # goes over as the subprotocol.
    procs.append(subprocess.Popen(
        [chrome, "--headless=new", f"--remote-debugging-port={DBG}",
         f"--user-data-dir={T}/prof", "--no-first-run", "--no-sandbox",
         "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
         "--window-size=1200,900", "about:blank"],
        stdout=open(f"{T}/chrome.log", "w"), stderr=subprocess.STDOUT))
    cdp = CDP(DBG)
    cdp.connect()
    cdp.call("Page.enable")
    cdp.call("Runtime.enable")
    cdp.call("Page.addScriptToEvaluateOnNewDocument",
             {"source": "try { localStorage.setItem('nexmesh-token', %s); } catch (e) {}"
                        % json.dumps(optok)})
    cdp.call("Page.navigate", {"url": f"{HUB}/xr?bare=1"})
    seats = 0
    for _ in range(80):
        seats = cdp.js(f"({APP} && {APP}.scene) ? {APP}.scene.nodes.filter"
                       "((n) => n.kind === 'agent' && !n.orch).length : 0") or 0
        if seats:
            break
        time.sleep(0.5)
    check("the room drew the fleet's one seat", seats == 1, f"saw {seats}")

    # And whoever is driving the fleet, which the hub reports from its own
    # operator tokens rather than from any seat.
    drivers = json.loads(cdp.js(
        "JSON.stringify(%s.scene.nodes.filter((n) => n.orch)"
        ".map((n) => [n.data.name, n.data.role]))" % APP))
    check("the orchestrator is in the room, from the hub's own token list",
          any(d[0] == "me" for d in drivers), str(drivers))
    check("and it is marked as driving the whole fleet, not a team",
          any("whole fleet" in (d[1] or "") for d in drivers), str(drivers))

    # Open that teammate's screen, the way clicking them does.
    cdp.js("""(() => {
      const a = %s;
      const who = a.scene.nodes.find((n) => n.kind === 'agent');
      a.select(who);
      return 1;
    })()""" % APP)
    live = {}
    for _ in range(60):
        live = json.loads(cdp.js("""(() => {
          const p = %s.panels.find((q) => q.kind === 'terminal');
          return JSON.stringify(p ? { state: p.live ? p.live.state : "no attach",
                                      ro: p.live ? p.live.readOnly : null,
                                      focused: !!p.focused,
                                      text: (p.lines || []).join("\\n") } : {});
        })()""" % APP))
        if live.get("state") == "attached":
            break
        time.sleep(0.5)
    check("the screen attached to the agent", live.get("state") == "attached", str(live)[:200])
    check("it is writable, not watch-only", live.get("ro") is False, str(live.get("ro")))
    check("it took the focus, so the keyboard knows where to type", live.get("focused") is True)
    # The panel is painted by the render loop, a frame after the attach opens,
    # so the banner is waited for separately rather than read in the same breath.
    banner = ""
    for _ in range(40):
        banner = cdp.js("""(() => {
          const p = %s.panels.find((q) => q.kind === 'terminal');
          return (p.lines || []).join("\\n");
        })()""" % APP) or ""
        if "FAKETUI READY" in banner:
            break
        time.sleep(0.25)
    check("the agent's own banner is on the panel", "FAKETUI READY" in banner, banner[:120])

    # The keyboard in the room, pressed key by key -- through the same grid a
    # pointer resolves, not by calling the terminal directly.
    cdp.js(f"(() => {{ {APP}.openKeys(); return 1; }})()")
    grid = json.loads(cdp.js("""(() => {
      const k = %s.panels.find((q) => q.kind === 'keys');
      return JSON.stringify(k ? k.grid.map((r) => r.map((x) => x.label)) : null);
    })()""" % APP))
    check("the keyboard has a key grid", bool(grid) and len(grid) == 5, str(grid)[:120])
    typed = cdp.js("""(() => {
      const a = %s, k = a.panels.find((q) => q.kind === 'keys');
      const press = (label) => {
        for (const row of k.grid) {
          const key = row.find((x) => x.label === label);
          if (key) { key.act(); return true; }
        }
        return false;
      };
      const missing = [];
      for (const ch of ['h', 'e', 'l', 'l', 'o']) if (!press(ch)) missing.push(ch);
      if (!press('enter')) missing.push('enter');
      return missing.join(',');
    })()""" % APP)
    check("every key pressed existed on the keyboard", typed == "", f"missing {typed}")

    seen = ""
    for _ in range(60):
        seen = cdp.js("""(() => {
          const p = %s.panels.find((q) => q.kind === 'terminal');
          return (p.lines || []).join("\\n");
        })()""" % APP)
        if "you said: hello" in seen:
            break
        time.sleep(0.5)
    check("the agent received what the keyboard sent and answered",
          "you said: hello" in seen, seen[-200:])

    # Shift and ctrl are one-shot, and ctrl sends a control code rather than
    # a letter: proven by what the emulator was handed, not by the label.
    sent = json.loads(cdp.js("""(() => {
      const a = %s, out = [];
      const p = a.panels.find((q) => q.kind === 'terminal');
      const real = p.live.type.bind(p.live);
      p.live.type = (b) => out.push(b);
      a.kb.shift = true; a.sendChar('a');
      const shiftAfter = a.kb.shift;
      a.kb.ctrl = true; a.sendChar('c');
      const ctrlAfter = a.kb.ctrl;
      a.sendChar('a');
      p.live.type = real;
      return JSON.stringify({ out, shiftAfter, ctrlAfter });
    })()""" % APP))
    check("shift makes a capital and is then spent",
          sent["out"][0] == "A" and sent["shiftAfter"] is False, str(sent))
    check("ctrl sends a control code, not a letter",
          sent["out"][1] == "\u0003" and sent["ctrlAfter"] is False, repr(sent["out"][1:2]))
    check("the next key is plain again", sent["out"][2] == "a", str(sent["out"]))

    # Resizing: a corner in each hand, and a corner under the mouse. A
    # terminal resized this way is a WINDOW resize -- the character grid
    # changes and the process is told -- not a magnified picture.
    zones = json.loads(cdp.js("""(() => {
      const p = %s.panels.find((q) => q.kind === 'terminal');
      return JSON.stringify({
        close: p.zoneAt(1 - 10 / p.cw, 10 / p.ch),
        br: p.zoneAt(1 - 10 / p.cw, 1 - 10 / p.ch),
        bl: p.zoneAt(10 / p.cw, 1 - 10 / p.ch),
        tl: p.zoneAt(10 / p.cw, 10 / p.ch),
        middle: p.zoneAt(0.5, 0.5),
        titlebar: p.zoneAt(0.5, 12 / p.ch),
      });
    })()""" % APP))
    check("the close button is in the corner where it is drawn",
          zones["close"] == "close", str(zones))
    check("each corner is a resize handle",
          [zones["br"], zones["bl"], zones["tl"]] == ["corner:br", "corner:bl", "corner:tl"],
          str(zones))
    check("the middle of a screen is not a handle",
          zones["middle"] == "body" and zones["titlebar"] == "body", str(zones))

    pulled = json.loads(cdp.js("""(() => {
      const a = %s;
      const p = a.panels.find((q) => q.kind === 'terminal');
      const before = { w: p.w, h: p.h, cols: p.cols, rows: p.rows,
                       tcols: p.live.t.cols, trows: p.live.t.rows };
      // Where the corner that must NOT move is, in the room.
      const corner = () => {
        const r = M4.dir(RY(p.yaw), [1, 0, 0]);
        return V3.add(p.pos, V3.add(V3.mul(r, -p.w / 2), [0, p.h / 2, 0]));
      };
      const fixedBefore = corner();
      a.grab.clear();
      a.pointers = [{ src: 'R', handed: 'right', squeeze: 1, sOrigin: [0, 1.4, -1],
                      origin: [0, 1.4, -1], hit: null,
                      panelHit: { panel: p, row: -1, zone: 'corner:br' } }];
      a.applyGrab();                                  // take the corner
      a.pointers[0].sOrigin = [0.5, 1.0, -1];         // out and down
      a.applyGrab();
      const after = { w: p.w, h: p.h, cols: p.cols, rows: p.rows,
                      tcols: p.live.t.cols, trows: p.live.t.rows };
      const fixedAfter = corner();
      a.pointers[0].squeeze = 0;
      a.applyGrab();
      return JSON.stringify({ before, after, want: p.live.want || null,
                              drift: V3.len(V3.sub(fixedAfter, fixedBefore)),
                              grabs: a.grab.size });
    })()""" % APP))
    check("dragging a corner made the screen wider and taller",
          pulled["after"]["w"] > pulled["before"]["w"] + 0.3
          and pulled["after"]["h"] > pulled["before"]["h"] + 0.2, str(pulled))
    check("it resized the character grid rather than magnifying the text",
          pulled["after"]["cols"] > pulled["before"]["cols"]
          and pulled["after"]["rows"] > pulled["before"]["rows"], str(pulled))
    check("the emulator took the new grid",
          pulled["after"]["tcols"] == pulled["after"]["cols"]
          and pulled["after"]["trows"] == pulled["after"]["rows"], str(pulled["after"]))
    check("the process was told the new size",
          pulled["want"] == [pulled["after"]["cols"], pulled["after"]["rows"]],
          str(pulled["want"]))
    check("the opposite corner stayed where it was",
          pulled["drift"] < 0.02, f"it moved {pulled['drift']:.3f}m")
    check("letting go of the corner released it", pulled["grabs"] == 0)

    mouse = json.loads(cdp.js("""(() => {
      const a = %s;
      const p = a.panels.find((q) => q.kind === 'terminal');
      const w0 = p.w, cols0 = p.cols, dist0 = a.cam.dist;
      a.lastRay = { panelHit: { panel: p, row: -1, zone: 'corner:br' }, hit: null };
      const ev = (t, x, y) => a.canvas.dispatchEvent(
        new PointerEvent(t, { clientX: x, clientY: y, pointerId: 1, bubbles: true }));
      ev('pointerdown', 600, 400);
      ev('pointermove', 760, 520);
      ev('pointerup', 760, 520);
      const dragged = { w: p.w, cols: p.cols };
      // And the wheel is the camera again, wherever it is pointed.
      a.lastRay = { panelHit: { panel: p, row: -1, zone: 'body' }, hit: null };
      a.canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: -120, cancelable: true }));
      return JSON.stringify({ w0, cols0, dragged, wNow: p.w,
                              camMoved: a.cam.dist !== dist0 });
    })()""" % APP))
    check("the mouse on a corner resizes it too",
          mouse["dragged"]["w"] > mouse["w0"] and mouse["dragged"]["cols"] > mouse["cols0"],
          str(mouse))
    check("the wheel over a screen no longer resizes it",
          abs(mouse["wNow"] - mouse["dragged"]["w"]) < 1e-9, str(mouse))
    check("the wheel still moves the camera", mouse["camMoved"] is True, str(mouse))

    # A refused attach. The hub refuses a seat whose CLI was started by hand
    # in a tmux session whose view has exited -- the process is alive but
    # there is no pty to attach to -- and the room has to say so and offer
    # the remedy rather than sitting on "attaching" for ever.
    refused = json.loads(cdp.js("""(() => {
      const a = %s;
      const who = a.scene.nodes.find((n) => n.kind === 'agent' && !n.orch);
      a.openTerminal(who);
      const p = a.panels.find((q) => q.kind === 'terminal');
      p.agent = Object.assign({}, p.agent,
                              { terminal_slot: 'sl-view1', terminal_live: false });
      const why = 'no terminal here for ag-x: its CLI was started by hand, '
                + 'so only its own shim reaches it';
      a.attachRefused(p, why);
      const rows = p.buttons.map((b) => b.label);
      const live = Object.assign({}, p.agent, { terminal_live: true });
      a.attachRefused(Object.assign(p, { agent: live }), why);
      const rowsLive = p.buttons.map((b) => b.label);
      return JSON.stringify({ rows, rowsLive, said: a.status,
                              wrapped: rows.filter((l) => l.trim().length > 0
                                                       && l.length <= p.cols).length });
    })()""" % APP))
    body = " ".join(refused["rows"])
    check("a refused attach says so on the panel",
          "no live terminal" in body, str(refused["rows"])[:160])
    check("it quotes the hub's own reason",
          "started by hand" in body and "only its own shim" in body, body[:200])
    check("the reason is wrapped to the panel, not cut off",
          refused["wrapped"] >= 3, str(refused["rows"]))
    check("it offers to start the terminal",
          any("start its terminal" in l for l in refused["rows"]), str(refused["rows"]))
    check("and offers the transcript and a way out",
          any("transcript" in l for l in refused["rows"])
          and any("close" in l for l in refused["rows"]), str(refused["rows"]))
    check("a seat whose view IS alive is not offered a restart",
          not any("start its terminal" in l for l in refused["rowsLive"]),
          str(refused["rowsLive"]))

    # Put the live screen back: a refusal closes the attach, and the checks
    # below are about a screen that has one.
    cdp.js("""(() => {
      const a = %s;
      const who = a.scene.nodes.find((n) => n.kind === 'agent' && !n.orch);
      const p = a.panels.find((q) => q.kind === 'terminal');
      if (p) { p.buttons = []; }
      a.openTerminal(who);
      return 1;
    })()""" % APP)
    for _ in range(40):
        st = cdp.js("""(() => {
          const p = %s.panels.find((q) => q.kind === 'terminal');
          return p && p.live ? p.live.state : "none";
        })()""" % APP)
        if st == "attached":
            break
        time.sleep(0.3)
    check("the screen attaches again after a refusal was shown", st == "attached", str(st))

    # The tabs: the dashboard's own views, as screens in the room.
    views = json.loads(cdp.js("""(() => {
      const a = %s, seen = {};
      for (const v of ['machines', 'plan', 'knowledge', 'files']) {
        a.openView(v);
        const p = a.panels.find((q) => q.kind === 'view');
        seen[v] = { title: p.title, lines: (p.lines || []).join(" | ").slice(0, 160) };
      }
      return JSON.stringify(seen);
    })()""" % APP))
    check("the machines view lists this fleet's machine",
          "machine-b" in views["machines"]["lines"], str(views["machines"]))
    for v in ("plan", "knowledge", "files"):
        check(f"the {v} view says something rather than nothing",
              len(views[v]["lines"]) > 4, str(views[v]))

    # The close button detaches: the panel goes, and so does the websocket.
    closed = json.loads(cdp.js("""(() => {
      const a = %s;
      const p = a.panels.find((q) => q.kind === 'terminal');
      const ws = p.live.ws;
      a.press({ panelHit: { panel: p, row: -1, col: -1, zone: 'close' }, hit: null });
      return JSON.stringify({ gone: !a.panels.some((q) => q === p),
                              said: a.status, ws: ws.readyState, focus: !!a.focus });
    })()""" % APP))
    check("the close button removed the screen", closed["gone"] is True, str(closed))
    check("and said it detached", "detached" in (closed["said"] or ""), str(closed["said"]))
    check("the attach was closed with it", closed["ws"] >= 2, f"readyState {closed['ws']}")
    check("nothing is left holding the keyboard's focus", closed["focus"] is False, str(closed))

    errs = json.loads(cdp.js("JSON.stringify(window.__errs || [])"))
    check("the page reported no errors", not errs, str(errs[:3]))
    return 1 if fails else 0


rc = 1
try:
    rc = main()
finally:
    print(f"  (logs: {T})")
    for p in procs:
        try:
            p.send_signal(signal.SIGTERM)
        except Exception:
            pass
    time.sleep(0.4)
    for p in procs:
        try:
            p.kill()
        except Exception:
            pass

if fails:
    print(f"FAILED ({len(fails)}): " + "; ".join(fails))
    sys.exit(1)
if rc:
    sys.exit(rc)
print("ALL XR TYPING CHECKS PASSED")
