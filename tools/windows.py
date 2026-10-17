#!/usr/bin/env python3
"""Acts as the Sill app on the plain door and walks the windows: picks each listed window in turn,
then the Desktop, and checks what each pick streams the way the app would see it.

usage: tools/windows.py HOST PORT [SECONDS_PER_PICK] [--command ACTION] [--tap X,Y [--tap-app NAME]]

For each source: its first frame is a keyframe after kind 0, ffmpeg decodes the segment, and its
picture's size is printed (a window streams at its own size, not the monitor's). Thumbnails
(kind 3) are counted per window and their PNG sizes checked (at most 208 on the longer side).
--tap X,Y taps there (fractions of the picture) a second into the turn of the first window (or of
the first whose app name is NAME; "desktop" taps the Desktop), and saves that window's last picture as
/tmp/sill-linux-window-tap.png.
--type TEXT types TEXT (as the app's keyboard does) right after that tap.
--scale S sends the viewport with Aa at S (the host sizes each picked window to 1180×760 ÷ S).
--launch APP asks the host to launch APP (a .desktop id) at the end and prints what streams then.
Apps (kind 5) and icons (kind 4) are counted.
--command close|minimize|fullScreen sends that window command (kind 15) to the last window at the
end and prints the window list that follows.
Exit 1 if any check fails.
"""
import json, socket, struct, subprocess, sys, time

HDR = struct.Struct(">BQBI")


def msg(kind, payload=b""):
    return HDR.pack(kind, struct.unpack(">Q", struct.pack(">d", time.time()))[0], 0, len(payload)) + payload


def read_exact(s, n):
    b = bytearray()
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return bytes(b)


def png_size(p):
    if p[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    return struct.unpack(">II", p[16:24])


def probe(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height",
                          "-of", "csv=p=0", path], capture_output=True, text=True)
    dec = subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-f", "null", "-"], capture_output=True, text=True)
    return out.stdout.strip(), dec.stderr.strip()


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    host, port = args[0], int(args[1])
    per = float(args[2]) if len(args) > 2 else 3
    command = sys.argv[sys.argv.index("--command") + 1] if "--command" in sys.argv else None
    tap = tuple(float(v) for v in sys.argv[sys.argv.index("--tap") + 1].split(",")) if "--tap" in sys.argv else None
    s = socket.create_connection((host, port))
    s.settimeout(0.2)
    s.sendall(msg(23, json.dumps({"appVersion": "0.5.1", "build": "4", "protocol": 1, "device": "windows.py"}).encode()))
    scale = float(sys.argv[sys.argv.index("--scale") + 1]) if "--scale" in sys.argv else None
    launch = sys.argv[sys.argv.index("--launch") + 1] if "--launch" in sys.argv else None
    s.sendall(msg(9, json.dumps({"width": 1180, "height": 760, "fps": 60, "scale": scale}).encode()))
    buf = b""
    lists, thumbs, segments = [], {}, {}
    apps, icons = [], {}
    params, current = None, None
    errors = []

    def pump(until):
        nonlocal buf, params
        while time.time() < until:
            try:
                c = s.recv(1 << 20)
                if not c:
                    raise EOFError
                buf += c
            except socket.timeout:
                pass
            while len(buf) >= 14:
                kind, _, key, ln = HDR.unpack(buf[:14])
                if len(buf) < 14 + ln:
                    break
                p, buf = buf[14:14 + ln], buf[14 + ln:]
                if kind == 2:
                    lists.append(json.loads(p))
                elif kind == 5:
                    apps[:] = json.loads(p)
                elif kind == 4:
                    n = struct.unpack(">H", p[:2])[0]
                    icons[p[2:2 + n].decode()] = png_size(p[2 + n:])
                elif kind == 3:
                    wid = struct.unpack(">I", p[:4])[0]
                    size = png_size(p[4:])
                    thumbs.setdefault(wid, []).append(size)
                    if size is None or max(size) > 208:
                        errors.append(f"thumbnail of window {wid}: {size}")
                elif kind == 0:
                    sets, off = [], 2
                    for _ in range(p[1]):
                        n = struct.unpack(">H", p[off:off + 2])[0]
                        sets.append(p[off + 2:off + 2 + n])
                        off += 2 + n
                    params = sets
                elif kind == 1 and current is not None:
                    seg = segments.setdefault(current, [])
                    if not seg and not key:
                        continue  # a frame of the old source still on its way
                    off, nals = 0, []
                    while off < len(p):
                        n = struct.unpack(">I", p[off:off + 4])[0]
                        nals.append(p[off + 4:off + 4 + n])
                        off += 4 + n
                    seg.append((params if key else []) + nals)

    pump(time.time() + 1.5)
    if not lists:
        print("no window list")
        return 1
    windows = lists[-1]["windows"]
    typed = sys.argv[sys.argv.index("--type") + 1] if "--type" in sys.argv else None
    tap_app = sys.argv[sys.argv.index("--tap-app") + 1] if "--tap-app" in sys.argv else None
    tapped = next((w["id"] for w in windows if tap_app is None or w["appName"] == tap_app), None)
    if tap_app == "desktop":
        tapped = "desktop"
    print(f"{len(windows)} windows:")
    for w in windows:
        print(f"  {w['id']}: {w['appName']} — {w['title']} ({w['width']}×{w['height']}, {w['bundleID']})")
    order = [("window", w["id"]) for w in windows] + [("desktop", None)]
    for what, wid in order:
        current = f"window {wid}" if wid is not None else "desktop"
        s.sendall(msg(6, json.dumps({"window": {"_0": wid}} if wid is not None else {"desktop": {}}).encode()))
        if tap and (wid if wid is not None else "desktop") == tapped:
            pump(time.time() + 1)
            for action in ("move", "leftDown", "leftUp"):
                s.sendall(msg(8, json.dumps({"pointer": {"_0": action, "x": tap[0], "y": tap[1]}}).encode()))
                pump(time.time() + 0.05)
            if typed:
                s.sendall(msg(8, json.dumps({"text": {"_0": typed}}).encode()))
                pump(time.time() + 0.05)
            pump(time.time() + max(0.5, per - 1.15))
        else:
            pump(time.time() + per)
    if launch:
        s.sendall(msg(7, json.dumps({"bundleID": launch}).encode()))
        pump(time.time() + 4)
        active = lists[-1]["active"]
        names = {w["id"]: w["appName"] for w in lists[-1]["windows"]}
        wid = active.get("window", {}).get("_0")
        print(f"after launching {launch}: streaming {names.get(wid, active)}")
    if command and windows:
        wid = windows[-1]["id"]
        s.sendall(msg(15, json.dumps({"id": wid, "action": command}).encode()))
        n = len(lists)
        pump(time.time() + 2)
        after = lists[-1]["windows"] if len(lists) > n else None
        print(f"after {command} on {wid}: " + ("no new list" if after is None else f"{len(after)} windows {[w['id'] for w in after]}"))
    s.close()

    for what, wid in order:
        name = f"window {wid}" if wid is not None else "desktop"
        seg = segments.get(name, [])
        path = f"/tmp/sill-linux-{name.replace(' ', '-')}.h265"
        with open(path, "wb") as f:
            for nals in seg:
                for n in nals:
                    f.write(b"\0\0\0\1" + n)
        if not seg:
            errors.append(f"{name}: no frames")
            print(f"{name}: no frames")
            continue
        size, err = probe(path)
        if tap and (wid if wid is not None else "desktop") == tapped:
            subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", path, "-update", "1", "/tmp/sill-linux-window-tap.png"])
        print(f"{name}: {len(seg)} frames, picture {size}" + (f", decode errors: {err[:200]}" if err else ""))
        if err:
            errors.append(f"{name}: {err[:200]}")
    print(f"apps: {len(apps)}, icons: {len(icons)}" + (f", e.g. {apps[0]['name']} ({apps[0]['bundleID']}) icon {icons.get(apps[0]['bundleID'])}" if apps else ""))
    for wid, sizes in sorted(thumbs.items()):
        print(f"thumbnails of window {wid}: {len(sizes)}, {sizes[-1]}")
    for e in errors:
        print("FAIL:", e)
    return 1 if errors else 0


sys.exit(main())
