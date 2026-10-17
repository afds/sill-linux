#!/usr/bin/env python3
"""Moves the host's pointer from a plain-door session and keeps the stream, so a decoded frame
shows where the pointer went. Moves only: no clicks, no keys (they would land in whatever has
focus on the host).

usage: tools/pointer-check.py HOST PORT X Y [--out FILE.h265]
  X, Y: the pointer's place as fractions of the screen (0..1).
Then: ffmpeg -i FILE.h265 -vf "select=eq(n\\,N)" -frames:v 1 frame.png
"""
import json, socket, struct, sys, time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from receive import msg, read_msg  # noqa: E402


def main():
    host, port, x, y = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
    out = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else "/tmp/sill-linux-pointer.h265"
    s = socket.create_connection((host, port))
    s.settimeout(5)
    s.sendall(msg(23, json.dumps({"appVersion": "0.5.1", "device": "pointer-check"}).encode()))
    params, frames, picked, moved = None, [], False, None
    start = time.time()
    while time.time() - start < 4:
        kind, ts, key, p = read_msg(s)
        if kind == 2 and not picked and json.loads(p)["active"] == {"none": {}}:
            s.sendall(msg(6, b'{"desktop":{}}'))
            picked = True
        elif kind == 0:
            off, params = 2, []
            for _ in range(p[1]):
                ln = struct.unpack(">H", p[off:off + 2])[0]
                params.append(p[off + 2:off + 2 + ln])
                off += 2 + ln
        elif kind == 1:
            nals, off = [], 0
            while off < len(p):
                ln = struct.unpack(">I", p[off:off + 4])[0]
                nals.append(p[off + 4:off + 4 + ln])
                off += 4 + ln
            frames.append((params if key else None, nals))
            if moved is None and time.time() - start > 1:
                # A little path ending at (x, y), so the compositor repaints the cursor.
                for i in range(1, 11):
                    f = i / 10
                    ev = {"pointer": {"_0": "move", "x": 0.5 + (x - 0.5) * f, "y": 0.5 + (y - 0.5) * f}}
                    s.sendall(msg(8, json.dumps(ev).encode()))
                    time.sleep(0.02)
                moved = len(frames)
    s.close()
    with open(out, "wb") as f:
        for ps, nals in frames:
            for n in (ps or []) + nals:
                f.write(b"\0\0\0\1" + n)
    print(f"{len(frames)} frames, moved after frame {moved}; last frame index {len(frames) - 1}; wrote {out}")


if __name__ == "__main__":
    main()
