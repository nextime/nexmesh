#!/usr/bin/env python3
"""The room's own controls, in a real browser.

The 3D view has things no other test can see: a menu that must exist on your
wrist in a headset, a way to look at one team out of several, and screens you
can take hold of and move. None of that is reachable from the hub's API, so
this drives the actual page in headless chromium with software GL and asks it
what it did. It needs no hub: the page has a demo fleet of two teams for
exactly this, which is also what the README's pictures are made from.
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
              const a = %s, m = a.panels.find((p) => p.wrist);
              return JSON.stringify({
                seats: a.scene.nodes.filter((n) => n.kind === 'agent').length,
                sessions: a.scene.sessions, only: a.scene.only || null,
                menu: !!m, labels: m ? m.buttons.map((b) => b.label) : [],
              });
            })()""" % APP))

        def pick(text):
            cdp.js("""(() => {
              const m = %s.panels.find((p) => p.wrist);
              const b = m.buttons.find((x) => x.label.includes(%s));
              if (!b) throw new Error('no row ' + %s);
              b.act(); return 1;
            })()""" % (APP, json.dumps(text), json.dumps(text)))
            time.sleep(0.3)

        # The menu exists, and everything the flat views can do is in it.
        cdp.js(f"(() => {{ {APP}.openMenu(); return 1; }})()")
        a = report()
        if not a["menu"]:
            fails.append("the menu is not marked as wrist-mounted")
        if a["seats"] != 8:
            fails.append(f"the demo fleet should seat 8, saw {a['seats']}")
        if a["sessions"] != ["docs-site", "payments"]:
            fails.append(f"the teams are wrong: {a['sessions']}")
        for want in ("the chat", "the code map", "say something", "every team",
                     "#docs-site", "#payments", "close the screens"):
            if not any(want in l for l in a["labels"]):
                fails.append(f"the menu has no {want!r} row")

        # One team on its own, which is the dropdown the flat views have.
        pick("#docs-site")
        b = report()
        if b["only"] != "docs-site":
            fails.append(f"picking a team set only={b['only']}")
        if b["seats"] != 3:
            fails.append(f"#docs-site is 3 seats, the room kept {b['seats']}")
        if not any(l.startswith("▸ #docs-site") for l in b["labels"]):
            fails.append("the team being shown is not ticked in the menu")
        if b["sessions"] != ["docs-site", "payments"]:
            fails.append("the other team fell out of the menu and cannot be got back")
        pick("every team")
        c = report()
        if c["seats"] != 8 or c["only"]:
            fails.append("there is no way back to every team")

        # Closing the screens must not close the menu: that would leave a
        # headset with no controls at all and no way to get them back.
        cdp.js(f"(() => {{ {APP}.openChat(); return 1; }})()")
        time.sleep(0.3)
        pick("close the screens")
        left = json.loads(cdp.js(
            "(() => JSON.stringify(%s.panels.map((p) => p.kind + (p.wrist ? '/wrist' : ''))))()"
            % APP))
        if left != ["menu/wrist"]:
            fails.append(f"closing the screens left {left}")

        # Taking hold of a screen moves it, in a headset...
        moved = json.loads(cdp.js("""(() => {
          const a = %s;
          a.openChat();
          const q = a.panels.find((p) => !p.wrist);
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
          const q = a.panels.find((p) => !p.wrist);
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
