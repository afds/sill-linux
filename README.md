# sill-linux

An experimental Linux (Wayland) host for the [Sill](https://getsill.app) iPhone and iPad app:
stream a Linux desktop, and later single windows, to the existing Sill app over your network.

**Status: phases 1 and 3 of [docs/feasibility.md](docs/feasibility.md).** It streams the whole
desktop to the **App Store build** of the Sill app over your network (TLS 1.3 with pinned keys,
and a one-time pairing with a code this terminal shows, as Sill for Mac does), and takes the
app's input: taps and clicks, scrolling, hardware keys and typed text. No single windows yet.

This is a separate project. It is not part of Sill and isn't supported by Sill's author. Sill's
own host is the free Mac app; this one speaks the same wire protocol (pinned in
[SILL_REFERENCE.md](SILL_REFERENCE.md)), so the Sill app needs no changes.

## Running it

Needs a Wayland desktop with the screen-cast portal (tested on GNOME 50), PipeWire, GStreamer
with an HEVC encoder (NVENC `nvh265enc`, VA-API `vah265enc`, or `x265enc`), and Avahi's
`avahi-publish` for discovery. To build:

```
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev   # Debian, Ubuntu
cargo run --release -- [--port 47460] [--name NAME] [--open | --plain] [--no-advertise] [--view-only]
```

Run it in a terminal you can see: when a device asks to pair, the code is printed there (and a
QR code, with `qrencode` installed). Pairing is once per device; `--forget-devices` removes them
all. `--open` lets any device in without pairing (still TLS); `--plain` is the old unencrypted
door, for a Debug build of the app.

The host's key, recognition key and paired devices live in `~/.local/state/sill-linux/` (0600).
Deleting `host-key.pk8` makes a new host as far as devices are concerned: each pairs again.

The first run asks which monitor to share and to allow remote control (the RemoteDesktop portal);
the answer is kept
(`~/.local/state/sill-linux/remote-desktop-token`, delete it to be asked again; with
`--view-only`, no remote control, `restore-token`). The device's Command key is Ctrl here
(`--command-key super` to change it), so ⌘C copies; the app's Spotlight key (⌘Space) taps Super,
which opens GNOME's Activities. With a device
streaming, it prints a `[1s]` line: frames captured, encoded, sent and dropped, then the device's
own `client …` line every two seconds; `in N` is the device's input events (frame age and round trip, median/worst).

Quality, resolution and frame rate come from the app's Settings panel (the gear), as with Sill
for Mac, and apply at once without restarting the stream; they are kept in
`~/.local/state/sill-linux/settings.json`. Resolution: Standard is the monitor's logical size
(1920×1080 for a 4K monitor at 200 %), this host's default; Retina is every pixel. Prioritize Speed
stays on (NVENC's lowest-latency preset); there is no virtual display on Linux.

A firewall must let the device in: TCP on the door's port and mDNS (UDP 5353), e.g.
`sudo ufw allow 47460/tcp` and `sudo ufw allow 5353/udp`.

**On the iPad or iPhone:** the Sill app from the App Store (0.5.1 or later), on the same network.
This machine shows up as a row, "Not paired"; tap it, then type the code from this terminal (Enter
Code Instead). From then on a tap connects. With `--plain`, only a Debug build of the app (Xcode)
connects, by its row or with the launch argument `-SillConnect <IP>:47460`.

**Without a device:** Sill's own `Scripts/sillclient.py` pairs and opens a session like the app
(`--pair-ask --then-code=FILE --identity=DIR`, then `--tls --identity=DIR`; the host writes each
code to `SILL_LINUX_TEST_CODE_FILE`). `tools/receive.py HOST PORT [SECONDS] [--join-late]`, on a
`--plain` door, checks every message's layout and decodes the stream with ffmpeg.

Testing switches: `SILL_LINUX_ENCODER=nv|va|x265` picks the encoder; `SILL_LINUX_DMABUF=1`
tries zero copy for NVENC (broken on NVIDIA here, see the doc); `SILL_LINUX_INPUT_LOG=1` prints
every input action sent to the portal.

## Dependency policy

Unlike Sill (Swift, Apple frameworks only), a Linux host can't avoid outside libraries. The rule
here: system libraries every desktop distribution ships (PipeWire, libei, D-Bus, Avahi, FFmpeg's
libavcodec) and well-maintained Rust crates for them (zbus/ashpd, pipewire-rs, reis, rustls,
ffmpeg-next). No services, no telemetry, no network access beyond the devices you pair.

## License

Apache-2.0, as Sill.
