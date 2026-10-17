# sill-linux

An experimental Linux (Wayland) host for the [Sill](https://getsill.app) iPhone and iPad app:
stream a Linux desktop, and later single windows, to the existing Sill app over your network.

**Status: phase 1 of [docs/feasibility.md](docs/feasibility.md).** It streams the whole desktop
to a **Debug build** of the Sill app over your network, unencrypted. No input yet, no windows,
and the App Store build won't connect (it needs the TLS door with pairing, phase 3).

This is a separate project. It is not part of Sill and isn't supported by Sill's author. Sill's
own host is the free Mac app; this one speaks the same wire protocol (pinned in
[SILL_REFERENCE.md](SILL_REFERENCE.md)), so the Sill app needs no changes.

## Running it

Needs a Wayland desktop with the screen-cast portal (tested on GNOME 50), PipeWire, GStreamer
with an HEVC encoder (NVENC `nvh265enc`, VA-API `vah265enc`, or `x265enc`), and Avahi's
`avahi-publish` for discovery. To build:

```
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev   # Debian, Ubuntu
cargo run --release -- [--port 47460] [--bitrate 15] [--name NAME] [--no-advertise]
```

The first run asks which monitor to share; the answer is kept
(`~/.local/state/sill-linux/restore-token`, delete it to be asked again). With a device
streaming, it prints a `[1s]` line: frames captured, encoded, sent and dropped, then the device's
own `client …` line every two seconds (frame age and round trip, median/worst).

A firewall must let the device in: TCP on the door's port and mDNS (UDP 5353), e.g.
`sudo ufw allow 47460/tcp` and `sudo ufw allow 5353/udp`.

**On the iPad or iPhone:** build Sill's iOS app (`iOSClient/Sill.xcodeproj` in
[Sill](https://github.com/Saffsanity/sill)) in the **Debug** configuration from Xcode on a Mac,
on the same network. This machine shows up as a row; tap it. Or add the launch argument
`-SillConnect <this machine's IP>:47460` to the scheme, and `-SillHUD 1` for the stream's
numbers on the device.

**Without a device:** `tools/receive.py HOST PORT [SECONDS] [--join-late]` acts as the app,
checks every message's layout, and decodes the stream with ffmpeg.

Testing switches: `SILL_LINUX_ENCODER=nv|va|x265` picks the encoder; `SILL_LINUX_DMABUF=1`
tries zero copy for NVENC (broken on NVIDIA here, see the doc).

## Dependency policy

Unlike Sill (Swift, Apple frameworks only), a Linux host can't avoid outside libraries. The rule
here: system libraries every desktop distribution ships (PipeWire, libei, D-Bus, Avahi, FFmpeg's
libavcodec) and well-maintained Rust crates for them (zbus/ashpd, pipewire-rs, reis, rustls,
ffmpeg-next). No services, no telemetry, no network access beyond the devices you pair.

## License

Apache-2.0, as Sill.
