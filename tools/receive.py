#!/usr/bin/env python3
"""Acts as the Sill app on the plain door: hello, viewport, the Desktop, pings; then checks what
came back the way the app's decoder would take it, and decodes the stream with ffmpeg.

usage: tools/receive.py HOST PORT [SECONDS] [--out FILE.h265] [--join-late]

Checks (exit 1 if any fails):
- the first message is a window list, and one after the pick says the Desktop;
- the first frame is a keyframe, preceded by kind 0 (VPS, SPS, PPS; nalLen 4);
- every frame is 4-byte length-prefixed NAL units with no start codes, no AUD, no parameter sets;
- ffmpeg decodes the reassembled Annex-B stream with no error.
--join-late connects a second client halfway through and checks it gets a keyframe within 1.5 s.
"""
import json, socket, struct, subprocess, sys, threading, time

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


def read_msg(s):
    kind, ts, key, ln = HDR.unpack(read_exact(s, 14))
    return kind, struct.unpack(">d", struct.pack(">Q", ts))[0], key == 1, read_exact(s, ln)


def session(host, port, seconds, label):
    s = socket.create_connection((host, port))
    s.settimeout(5)
    s.sendall(msg(23, json.dumps({"appVersion": "0.5.1", "build": "4", "protocol": 1, "device": label}).encode()))
    s.sendall(msg(9, json.dumps({"width": 1180, "height": 760, "fps": 60}).encode()))
    r = {"lists": [], "params": None, "frames": [], "errors": [], "ages": [], "first_key_at": None,
         "pongs": 0, "ticks": 0, "start": time.time()}
    end = time.time() + seconds
    last_ping = 0
    picked = False
    while time.time() < end:
        if time.time() - last_ping > 0.25:
            s.sendall(msg(10, struct.pack(">d", time.monotonic())))
            last_ping = time.time()
        try:
            kind, ts, key, p = read_msg(s)
        except socket.timeout:
            r["errors"].append("no message for 5 s")
            break
        if kind == 2:
            lst = json.loads(p)
            r["lists"].append(lst)
            if not picked and lst["active"] == {"none": {}}:
                s.sendall(msg(6, b'{"desktop":{}}'))
                picked = True
        elif kind == 0:
            nal_len, count = p[0], p[1]
            sets, off = [], 2
            for _ in range(count):
                ln = struct.unpack(">H", p[off:off + 2])[0]
                sets.append(p[off + 2:off + 2 + ln])
                off += 2 + ln
            types = [(x[0] >> 1) & 63 for x in sets]
            if nal_len != 4 or types != [32, 33, 34] or not key:
                r["errors"].append(f"bad parameter sets: nalLen {nal_len}, types {types}, key {key}")
            r["params"] = sets
        elif kind == 1:
            if not r["frames"]:
                if not key:
                    r["errors"].append("first frame is not a keyframe")
                if r["params"] is None:
                    r["errors"].append("keyframe before any parameter sets")
            if key and r["first_key_at"] is None:
                r["first_key_at"] = time.time() - r["start"]
            off, nals = 0, []
            while off < len(p):
                ln = struct.unpack(">I", p[off:off + 4])[0]
                nal = p[off + 4:off + 4 + ln]
                if ln == 0 or off + 4 + ln > len(p):
                    r["errors"].append("bad length prefix")
                    break
                if nal[:3] == b"\0\0\1" or nal[:4] == b"\0\0\0\1":
                    r["errors"].append("start code inside a frame")
                t = (nal[0] >> 1) & 63
                if t in (32, 33, 34, 35):
                    r["errors"].append(f"NAL type {t} inside a frame")
                nals.append(nal)
                off += 4 + ln
            r["frames"].append((key, nals, r["params"] if key else None))
            r["ages"].append((time.time() - ts) * 1000)
        elif kind == 11:
            r["pongs"] += 1
        elif kind == 13:
            r["ticks"] += 1
    s.close()
    return r


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    host, port = args[0], int(args[1])
    seconds = float(args[2]) if len(args) > 2 else 6
    out = sys.argv[sys.argv.index("--out") + 1] if "--out" in sys.argv else "/tmp/sill-linux-receive.h265"
    late = {}
    if "--join-late" in sys.argv:
        def late_join():
            time.sleep(seconds / 2)
            late["r"] = session(host, port, 2.5, "late client")
        t = threading.Thread(target=late_join)
        t.start()
    r = session(host, port, seconds, "receive.py")
    if "--join-late" in sys.argv:
        t.join()

    errors = list(r["errors"])
    if not r["lists"] or r["lists"][0]["active"] != {"none": {}}:
        errors.append("first window list does not say nothing streams")
    if not any(l["active"] == {"desktop": {}} for l in r["lists"]):
        errors.append("no window list says the Desktop")
    with open(out, "wb") as f:
        for key, nals, params in r["frames"]:
            for n in (params or []) + nals:
                f.write(b"\0\0\0\1" + n)
    n = len(r["frames"])
    keys = sum(1 for k, _, _ in r["frames"] if k)
    ages = sorted(r["ages"])
    p50 = ages[len(ages) // 2] if ages else float("nan")
    print(f"host name {r['lists'][0]['macName'] if r['lists'] else '?'}; {n} frames ({keys} key) in {seconds:.0f} s "
          f"= {n / seconds:.1f} fps; first keyframe {r['first_key_at']}; frame age p50 {p50:.1f} ms; "
          f"pongs {r['pongs']}; ticks {r['ticks']}")
    if n == 0:
        errors.append("no frames (a still screen sends only the keep-alive frame every 0.5 s: move something)")
    else:
        d = subprocess.run(["ffmpeg", "-v", "error", "-i", out, "-f", "null", "-"], capture_output=True, text=True)
        probe = subprocess.run(["ffprobe", "-v", "error", "-count_frames", "-show_entries",
                                "stream=width,height,profile,pix_fmt,nb_read_frames", "-of", "compact", out],
                               capture_output=True, text=True)
        print("ffprobe:", probe.stdout.strip())
        if d.returncode != 0 or d.stderr.strip():
            errors.append("ffmpeg decode errors: " + d.stderr.strip()[:500])
    if "r" in late:
        lr = late["r"]
        print(f"late client: first keyframe after {lr['first_key_at']} s, {len(lr['frames'])} frames, errors {lr['errors']}")
        if lr["first_key_at"] is None or lr["first_key_at"] > 1.5:
            errors.append("late client got no keyframe within 1.5 s")
        errors += ["late: " + e for e in lr["errors"]]
    for e in errors:
        print("FAIL:", e)
    print("OK" if not errors else f"{len(errors)} failure(s)")
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
