#!/usr/bin/env python3
"""The room's own controls, in a real browser.

The 3D view has things no other test can see: a menu that stands in the room
and can be moved, channel signs that pick which team is shown, and screens you
can take hold of. None of that is reachable from the hub's API, so this drives
the actual page in headless chromium with software GL and asks it what it did.
It needs no hub: the page has a demo fleet of two teams for exactly this, which
is also what the README's pictures are made from.
"""
import importlib.util, json, os, shutil, signal, socket, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
spec = importlib.util.spec_from_file_location("xrshots", os.path.join(ROOT, "tests", "xr-shots.py"))
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


def main():
    chrome = shutil.which("chromium") or shutil.which("google-chrome")
    if not chrome:
        print("SKIP no chromium")
        return 0
    tmp = tempfile.mkdtemp(prefix="nexmesh-xrroom-")
    web, dbg = free(), free()
    procs, fails = [], []
    try:
        procs.append(subprocess.Popen(
            [sys.executable, "-m", "http.server", str(web), "--bind", "127.0.0.1",
             "-d", os.path.join(ROOT, "crates", "hub", "static")],
            stdout=open(f"{tmp}/web.log", "w"), stderr=subprocess.STDOUT))
        procs.append(subprocess.Popen(
            [chrome, "--headless=new", f"--remote-debugging-port={dbg}",
             f"--user-data-dir={tmp}/prof", "--no-first-run", "--no-sandbox",
             "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
             "--window-size=1200,900", f"http://127.0.0.1:{web}/xr.html?demo=1&bare=1"],
            stdout=open(f"{tmp}/chrome.log", "w"), stderr=subprocess.STDOUT))
        cdp = CDP(dbg)
        cdp.connect()
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        for _ in range(80):
            if cdp.js(f"({APP} && {APP}.scene && {APP}.scene.nodes.length) ? 1 : 0") == 1:
                break
            time.sleep(0.5)
        else:
            print("FAILED (1): the room never drew anything")
            return 1

        def report():
            return json.loads(cdp.js("""(() => {
              const a = %s, m = a.panels.find((p) => p.kind === 'menu');
              return JSON.stringify({
                seats: a.scene.nodes.filter((n) => n.kind === 'agent' && !n.orch).length,
                drivers: a.scene.nodes.filter((n) => n.orch).map((n) => n.data.name),
                sessions: a.scene.sessions, only: a.scene.only || null,
                menu: !!m, labels: m ? m.buttons.map((b) => b.label) : [],
                signs: a.scene.signs.map((s) => ({ name: s.data.session, only: !!s.only,
                                                   dimmed: !!s.dimmed, pick: !!s.bounds })),
              });
            })()""" % APP))

        def pick(text):
            cdp.js("""(() => {
              const m = %s.panels.find((p) => p.kind === 'menu');
              const b = m.buttons.find((x) => x.label.includes(%s));
              if (!b) throw new Error('no row ' + %s);
              b.act(); return 1;
            })()""" % (APP, json.dumps(text), json.dumps(text)))
            time.sleep(0.3)

        def pressSign(name):
            cdp.js("""(() => {
              const a = %s;
              const s = a.scene.signs.find((x) => x.data.session === %s);
              if (!s) throw new Error('no sign for ' + %s);
              a.press({ hit: { object: s, t: 3 }, panelHit: null });
              return 1;
            })()""" % (APP, json.dumps(name), json.dumps(name)))
            time.sleep(0.3)

        # The menu is a screen in the room -- not strapped to a hand -- and it
        # carries everything the flat views can do.
        cdp.js(f"(() => {{ {APP}.openMenu(); return 1; }})()")
        a = report()
        if not a["menu"]:
            fails.append("there is no menu panel")
        if a["seats"] != 8:
            fails.append(f"the demo fleet should seat 8, saw {a['seats']}")
        if a["sessions"] != ["docs-site", "payments"]:
            fails.append(f"the teams are wrong: {a['sessions']}")
        for want in ("the chat", "the code map", "the keyboard", "say something",
                     "the machines", "the plan", "close the screens"):
            if not any(want in l for l in a["labels"]):
                fails.append(f"the menu has no {want!r} row")
        # The teams are picked in the room, so they are NOT a list in here.
        if any(l.strip().startswith("#") for l in a["labels"]):
            fails.append("the menu still lists the teams instead of leaving them to the signs")
        if cdp.js("(() => %s.panels.some((p) => p.wrist) ? 1 : 0)()" % APP):
            fails.append("something is still mounted on a wrist")

        # Whoever drives the fleet is in the room, and belongs to no team.
        if a["drivers"] != ["nextime"]:
            fails.append(f"the orchestrator is not in the room: {a['drivers']}")

        # A channel sign for every team, pointable, whether shown or not.
        if sorted(s["name"] for s in a["signs"]) != ["docs-site", "payments"]:
            fails.append(f"the channel signs are wrong: {a['signs']}")
        if not all(s["pick"] for s in a["signs"]):
            fails.append("a channel sign cannot be pointed at")

        # Pressing one shows that team on its own; pressing it again restores.
        pressSign("docs-site")
        b = report()
        if b["only"] != "docs-site":
            fails.append(f"pressing the sign set only={b['only']}")
        if b["seats"] != 3:
            fails.append(f"#docs-site is 3 seats, the room kept {b['seats']}")
        if b["drivers"] != ["nextime"]:
            fails.append("choosing one team removed the orchestrator from the room")
        marked = {s["name"]: (s["only"], s["dimmed"]) for s in b["signs"]}
        if marked.get("docs-site") != (True, False):
            fails.append(f"the chosen channel is not lit: {marked}")
        if marked.get("payments") != (False, True):
            fails.append(f"the other channel should be dimmed but still there: {marked}")
        if len(b["signs"]) != 2:
            fails.append("choosing a team removed the other team's sign, so there is no way back")
        pressSign("docs-site")
        c = report()
        if c["only"] or c["seats"] != 8:
            fails.append(f"pressing it again did not restore every team: {c['only']}, {c['seats']}")
        pressSign("payments")
        pressSign("docs-site")
        d = report()
        if d["only"] != "docs-site" or d["seats"] != 3:
            fails.append("switching straight from one channel to another did not work")
        pressSign("docs-site")

        # Closing the screens leaves the menu; the menu's own close button
        # closes it, and it can be asked for again.
        cdp.js(f"(() => {{ {APP}.openChat(); return 1; }})()")
        time.sleep(0.3)
        pick("close the screens")
        left = json.loads(cdp.js(
            "(() => JSON.stringify(%s.panels.map((p) => p.kind)))()" % APP))
        if left != ["menu"]:
            fails.append(f"closing the screens left {left}")
        back = json.loads(cdp.js("""(() => {
          const a = %s, m = a.panels.find((p) => p.kind === 'menu');
          a.press({ panelHit: { panel: m, row: -1, col: -1, zone: 'close' }, hit: null });
          const gone = !a.panels.some((p) => p.kind === 'menu');
          a.openMenu();
          return JSON.stringify({ gone, backAgain: a.panels.some((p) => p.kind === 'menu') });
        })()""" % APP))
        if not back["gone"]:
            fails.append("the menu's close button did not close it")
        if not back["backAgain"]:
            fails.append("the menu could not be opened again")

        # Taking hold of a screen moves it, in a headset...
        moved = json.loads(cdp.js("""(() => {
          const a = %s;
          a.openChat();
          const q = a.panels.find((p) => p.kind === 'chat');
          const before = q.pos.slice();
          a.headAt = [0, 1.5, 0];
          a.pointers = [{ src: 'L', handed: 'left', squeeze: 1, sOrigin: [0, 1.4, -1],
                          origin: [0, 1.4, -1], panelHit: { panel: q, row: -1 }, hit: null }];
          a.applyGrab();                       // squeeze: take hold
          a.pointers[0].sOrigin = [0.5, 1.6, -1];
          a.applyGrab();                       // and move the hand
          const after = q.pos.slice();
          a.pointers[0].squeeze = 0;
          a.applyGrab();                       // let go
          return JSON.stringify({ before, after, held: a.grab.size,
                                  faces: Math.abs(q.yaw - Math.atan2(-q.pos[0], -q.pos[2])) < 0.3 });
        })()""" % APP))
        d = [round(moved["after"][i] - moved["before"][i], 3) for i in range(3)]
        if d != [0.5, 0.2, 0.0]:
            fails.append(f"a held screen moved by {d}, not by the hand's [0.5, 0.2, 0.0]")
        if not moved["faces"]:
            fails.append("a moved screen stopped facing the person who moved it")
        if moved["held"] != 0:
            fails.append("letting go did not let go")

        # ...and with the mouse on a flat screen, where it must not also
        # turn the room.
        drag = json.loads(cdp.js("""(() => {
          const a = %s;
          const q = a.panels.find((p) => p.kind === 'chat');
          const before = q.pos.slice(), yaw = a.cam.yaw;
          a.movePanel(q, 120, -60);
          return JSON.stringify({ moved: V3.len(V3.sub(q.pos, before)) > 0.05,
                                  turned: Math.abs(a.cam.yaw - yaw) > 1e-6 });
        })()""" % APP))
        if not drag["moved"]:
            fails.append("dragging a screen with the mouse did not move it")
        if drag["turned"]:
            fails.append("dragging a screen also turned the room")

        errs = json.loads(cdp.js("JSON.stringify(window.__errs || [])"))
        if errs:
            fails.append(f"the page reported errors: {errs[:3]}")
    finally:
        for p in procs:
            try:
                p.send_signal(signal.SIGTERM)
            except Exception:
                pass
        time.sleep(0.3)
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass
        shutil.rmtree(tmp, ignore_errors=True)

    if fails:
        print(f"FAILED ({len(fails)}): " + "; ".join(fails))
        return 1
    print("ALL XR ROOM CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
