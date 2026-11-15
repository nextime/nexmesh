#!/usr/bin/env python3
"""Photograph and film the immersive room, without a headset.

The scene in `/xr` renders to an ordinary canvas when there is no headset,
which is what makes this possible: a browser draws it, this drives the camera
through a scripted tour, and ffmpeg turns the frames into the animation in the
README.

    tests/xr-shots.py                 # stills + a gif into docs/images/
    tests/xr-shots.py --frames 120    # a longer film

It starts its own hub with a demo fleet, so nothing here photographs anybody's
real machines -- the page's own `?demo=1` data is `machine-a`/`machine-b`, like
every other example in this repository.

Needs chromium (or chrome) and ffmpeg. Software GL is fine and is what CI
would use; the picture is identical, only slower to draw.
"""
import argparse, base64, json, os, shutil, signal, socket, subprocess, sys, tempfile, time
import urllib.request, urllib.error

SCR = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(SCR)
OUT = os.path.join(ROOT, "docs", "images")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def browser():
    for name in ("chromium", "google-chrome", "chromium-browser", "chrome"):
        found = shutil.which(name)
        if found:
            return found
    sys.exit("no chromium or chrome on PATH")


class CDP:
    """Just enough Chrome DevTools Protocol to drive a page and take pictures."""

    def __init__(self, port):
        self.port = port
        self.id = 0
        self.ws = None

    def connect(self, timeout=25):
        import struct
        end = time.time() + timeout
        target = None
        while time.time() < end:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/json", timeout=2) as r:
                    tabs = json.loads(r.read())
                page = [t for t in tabs if t.get("type") == "page"]
                if page:
                    target = page[0]["webSocketDebuggerUrl"]
                    break
            except Exception:
                pass
            time.sleep(0.3)
        if not target:
            sys.exit("the browser never offered a page to drive")
        # A minimal websocket client: the handshake, then text frames. Pulling
        # in a dependency for forty lines would be the bigger cost.
        import base64 as b64, hashlib, os as _os
        host, rest = target[len("ws://"):].split("/", 1)
        h, p = host.split(":")
        self.sock = socket.create_connection((h, int(p)), timeout=10)
        key = b64.b64encode(_os.urandom(16)).decode()
        self.sock.sendall((
            f"GET /{rest} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n").encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        self.rest = buf.split(b"\r\n\r\n", 1)[1]

    def _send(self, payload):
        import struct, os as _os
        data = payload.encode()
        head = bytearray([0x81])
        n = len(data)
        if n < 126:
            head.append(0x80 | n)
        elif n < 65536:
            head.append(0x80 | 126)
            head += struct.pack(">H", n)
        else:
            head.append(0x80 | 127)
            head += struct.pack(">Q", n)
        mask = _os.urandom(4)
        head += mask
        self.sock.sendall(bytes(head) + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def _recv(self):
        import struct

        def take(n):
            while len(self.rest) < n:
                chunk = self.sock.recv(65536)
                if not chunk:
                    raise RuntimeError("the browser closed the connection")
                self.rest += chunk
            out, self.rest = self.rest[:n], self.rest[n:]
            return out

        while True:
            h = take(2)
            op = h[0] & 0x0F
            ln = h[1] & 0x7F
            if ln == 126:
                ln = struct.unpack(">H", take(2))[0]
            elif ln == 127:
                ln = struct.unpack(">Q", take(8))[0]
            body = take(ln)
            if op == 1:
                return json.loads(body)
            if op == 8:
                raise RuntimeError("the browser hung up")

    def call(self, method, params=None, timeout=60):
        self.id += 1
        mine = self.id
        self._send(json.dumps({"id": mine, "method": method, "params": params or {}}))
        end = time.time() + timeout
        while time.time() < end:
            msg = self._recv()
            if msg.get("id") == mine:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
        raise RuntimeError(f"{method}: no answer")

    def js(self, expr):
        r = self.call("Runtime.evaluate", {
            "expression": expr, "returnByValue": True, "awaitPromise": True})
        if r.get("exceptionDetails"):
            raise RuntimeError(str(r["exceptionDetails"])[:300])
        return r.get("result", {}).get("value")

    def shot(self, path):
        r = self.call("Page.captureScreenshot", {"format": "png", "fromSurface": True})
        with open(path, "wb") as f:
            f.write(base64.b64decode(r["data"]))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--frames", type=int, default=90, help="frames in the animation")
    ap.add_argument("--fps", type=int, default=12, help="frames a second in the gif")
    ap.add_argument("--width", type=int, default=1100)
    ap.add_argument("--height", type=int, default=560)
    ap.add_argument("--keep", action="store_true", help="keep the frame pngs")
    args = ap.parse_args()

    os.makedirs(OUT, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="nexmesh-xr-shots-")
    port = free_port()
    dbg = free_port()
    procs = []

    hub = os.path.join(ROOT, "target", "debug", "nexmesh-hub")
    if not os.path.exists(hub):
        sys.exit("build first: cargo build -p nexmesh-hub")
    db = os.path.join(tmp, "h.db")
    procs.append(subprocess.Popen(
        [hub, "--db", db, "--listen", f"127.0.0.1:{port}", "serve"],
        stdout=open(os.path.join(tmp, "hub.log"), "w"), stderr=subprocess.STDOUT))
    for _ in range(100):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/healthz", timeout=1) as r:
                if json.loads(r.read()).get("ok"):
                    break
        except Exception:
            time.sleep(0.2)

    chrome = browser()
    prof = os.path.join(tmp, "profile")
    # No `tour=1` in the URL: the tour is started by hand AFTER the stills,
    # because a camera that is already walking overrides every pose the
    # stills ask for -- which is how three photographs of the same room got
    # taken from the same angle.
    url = (f"http://127.0.0.1:{port}/xr?demo=1&bare=1")
    cmd = [
        chrome, "--headless=new", f"--remote-debugging-port={dbg}",
        f"--user-data-dir={prof}", "--no-first-run", "--no-default-browser-check",
        f"--window-size={args.width},{args.height}",
        "--hide-scrollbars", "--force-device-scale-factor=1",
        # Software GL, so this works on a machine with no display and in CI.
        "--use-gl=angle", "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
        "--disable-gpu-sandbox", "--no-sandbox", url,
    ]
    procs.append(subprocess.Popen(
        cmd, stdout=open(os.path.join(tmp, "chrome.log"), "w"), stderr=subprocess.STDOUT))

    cdp = CDP(dbg)
    try:
        cdp.connect()
        cdp.call("Page.enable")
        cdp.call("Runtime.enable")
        # Wait for the scene to exist rather than for a number of seconds.
        ok = False
        for _ in range(120):
            try:
                n = cdp.js("window.nexmeshXR ? window.nexmeshXR.scene.nodes.length : 0")
                if n:
                    ok = True
                    break
            except Exception:
                pass
            time.sleep(0.25)
        if not ok:
            print(open(os.path.join(tmp, "chrome.log")).read()[-2000:], file=sys.stderr)
            sys.exit("the scene never came up (see the log above)")

        frames = os.path.join(tmp, "frames")
        os.makedirs(frames, exist_ok=True)

        print("stills")
        shots = [
            ("xr-team.png", "team", -0.55, 0.0, 1),
            ("xr-team-selected.png", "team", 0.35, -0.04, 2),
            ("xr-graph.png", "graph", 0.5, 0.05, 4),
        ]
        for name, mode, yaw, pitch, pick in shots:
            cdp.js(f"""(() => {{
              const a = window.nexmeshXR;
              a.setMode("{mode}");
              a.cam.yaw = {yaw}; a.cam.pitch = {pitch}; a.cam.dist = 0.2;
              const list = a.scene.active().filter((o) => !o.flat);
              a.select(list[Math.min({pick}, list.length - 1)]);
              a.scene.hovered = null;
              return true;
            }})()""")
            time.sleep(0.8)
            cdp.shot(os.path.join(OUT, name))
            print("  ", name)

        print(f"filming {args.frames} frames")
        # The page's own tour drives the camera; this only takes pictures of
        # it, so the motion is the same every time.
        cdp.js("window.nexmeshXR.tour()")
        for i in range(args.frames):
            cdp.shot(os.path.join(frames, f"f{i:04d}.png"))
            time.sleep(1.0 / max(args.fps, 1))

        gif = os.path.join(OUT, "xr-tour.gif")
        pal = os.path.join(tmp, "palette.png")
        # Two passes, because a gif has 256 colours and letting ffmpeg choose
        # them from the actual frames is the difference between this looking
        # like a render and looking like a fax.
        # 640 wide and a 96-colour palette: a README is read on a phone as
        # often as not, and a three-megabyte animation is one nobody waits
        # for. The scene is mostly flat colour, so the loss is invisible.
        vf = f"fps={args.fps},scale=640:-1:flags=lanczos"
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(args.fps),
                        "-i", os.path.join(frames, "f%04d.png"),
                        "-vf", f"{vf},palettegen=max_colors=96:stats_mode=diff", pal],
                       check=True)
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(args.fps),
                        "-i", os.path.join(frames, "f%04d.png"), "-i", pal,
                        "-lavfi", f"{vf} [x]; [x][1:v] paletteuse=dither=bayer:bayer_scale=3",
                        "-loop", "0", gif], check=True)
        mp4 = os.path.join(OUT, "xr-tour.mp4")
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(args.fps),
                        "-i", os.path.join(frames, "f%04d.png"),
                        "-c:v", "libx264", "-pix_fmt", "yuv420p",
                        "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", mp4], check=True)
        size = os.path.getsize(gif) / 1024
        print(f"  xr-tour.gif  {size:.0f} KB")
        print(f"  xr-tour.mp4  {os.path.getsize(mp4)/1024:.0f} KB")
        if size > 3000:
            print("  (over 3MB: rerun with --frames 60 --fps 10 for a lighter one)")
        if args.keep:
            print("  frames in", frames)
    finally:
        for p in procs:
            try:
                p.send_signal(signal.SIGTERM)
            except Exception:
                pass
        time.sleep(0.5)
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print("logs in", tmp)


if __name__ == "__main__":
    main()
