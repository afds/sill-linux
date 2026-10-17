# sill-linux

A Linux host for the [Sill](https://getsill.app) iPhone and iPad app: stream your Linux desktop, or
a single window, to the Sill app from the App Store, and control it from the device.

**This is an unofficial, independent project.** Sill's author did not make or endorse it and does
not support it. Please don't send Sill's support questions about it; open an issue here instead.
Sill's own host is the free [Sill for Mac](https://getsill.app). This host speaks the same wire
protocol (pinned in [SILL_REFERENCE.md](SILL_REFERENCE.md)), so the app needs no changes.

## What works

- **The desktop**, HEVC, at the app's chosen quality, resolution and frame rate (the gear in the
  app sets them; kept in `~/.local/state/sill-linux/settings.json`).
- **Input:** taps and clicks, scrolling, the trackpad, hardware keys, and typed text. The device's
  Command key is Ctrl here, so ⌘C copies (`--command-key super` changes that). The app's Spotlight
  key opens GNOME's Activities.
- **Single windows (GNOME, with the extension):**
  - each window in the app's bar with a thumbnail; a tap streams it alone, raised and focused;
  - close, minimize and full screen from a held thumbnail;
  - All Apps, with icons, to start an app;
  - Aa sizes the window to the device's panel, and it goes back when it stops streaming.
- **More than one monitor:** each monitor but the main one is an entry with the windows ("Display —
  ‹its name›", with a thumbnail); a tap streams that monitor, and input goes there. Desktop is the
  main monitor (`--monitor` picks another).
- **This machine's pointer** shown on the device while the local mouse moves it, in its current
  shape.
- **Three-finger gestures** on the app's trackpad work as GNOME's own: swipe up for Activities and
  the app grid, left and right for workspaces, pinch for the app grid, spread for the desktop.
- **The clipboard both ways**, text only. It skips copies a password manager marks; turn it off
  with `--no-clipboard`. The app's 0.5.1 lacks this; it needs a later app version.

- **Away from home** through your own VPN (WireGuard into your home network, Tailscale): connect
  once at home, and the app keeps this machine as a "Remote" row it dials from anywhere the VPN
  reaches, on the same port and with the same encryption. `--no-remote` turns it off.

Not supported: audio, a virtual display, and connections from the open internet (port forwarding).

## Requirements

- **GNOME 48 to 51 on Wayland** for everything above. On other Wayland desktops only the whole
  desktop streams, through the screen-cast and remote-desktop portals.
- PipeWire.
- GStreamer with an HEVC encoder: NVENC (`nvh265enc`), VA-API (`vah265enc`), or `x265enc`.
- Avahi's `avahi-publish`, for discovery.
- The Sill app 0.5.1 or later, on the same network.

## Build and install

```
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev   # Debian, Ubuntu
cargo build --release
./target/release/sill-linux --install-extension   # GNOME: windows, pointer, gestures, clipboard
./target/release/sill-linux --install-service     # starts with your session
```

After `--install-extension`, log out and in once: GNOME loads an extension's code only at login.
The extension is a thin layer that reports GNOME's state and carries out plain actions; the
decisions live in sill-linux. So after updating sill-linux, `systemctl --user restart sill-linux`
is enough. Log out again only when sill-linux says the running extension is older.

GNOME's orange sharing indicator shows only while a device streams. Its stop button ends the
session: the device says sharing was stopped and waits for a tap to connect again.

The service's output: `journalctl --user -u sill-linux -f`. Remove it with
`--uninstall-service`. To run in a terminal instead: `./target/release/sill-linux`. Ctrl-C tells
devices the host quit, and they reconnect by themselves when it is back.

A firewall must let the device in: TCP on the port (47460 by default) and mDNS, e.g.
`sudo ufw allow 47460/tcp` and `sudo ufw allow 5353/udp`.

## Pairing

In the app, this machine shows up as a row, "Not paired". Tap it: on GNOME with the extension, a
pairing window comes up in the middle of this screen with a QR code to scan with the app's camera
and the code for Enter Code Instead. It closes by itself once the device pairs; its Cancel keeps
that device from asking again for 10 minutes. Without the extension the code comes as a desktop
notification, and in a terminal it is printed too (with a QR code if `qrencode` is installed). Each device pairs once; after that a tap connects.
`--forget-devices` removes every paired device.

## Security

- Every connection is TLS 1.3, and both ends' keys are pinned, as with Sill for Mac.
- A device must pair with a one-time code shown on this machine.
- `--open` lets any device in without pairing (still TLS). `--plain` is unencrypted, only for a
  Debug build of the app.
- The host's keys and paired devices are in `~/.local/state/sill-linux/` (0600). Delete
  `host-key.pk8` and every device must pair again.
- No servers, no telemetry, and no network use beyond the devices you pair. Report a vulnerability
  privately through GitHub's security advisories, not in a public issue.

## Options

```
sill-linux [--port N] [--name NAME] [--open | --plain] [--no-advertise] [--view-only]
           [--show-cursor] [--keep-all-taps] [--choose-screen] [--portal] [--monitor CONNECTOR]
           [--no-notifications] [--no-clipboard] [--no-remote] [--command-key ctrl|super]
           [--forget-devices]
```

- `--monitor DP-3` picks another monitor on GNOME (the primary by default).
- `--portal` uses the desktop portal even on GNOME (the desktop only).
- `--show-cursor` draws the pointer into the picture.
- `--keep-all-taps` clicks every tap. By default, a slow nudge followed by a lift is not clicked,
  since the app's trackpad can report it as a tap.
- `--view-only` takes no input.
- `--no-remote` keeps this machine's addresses from paired devices, so there is no Remote row.

With a device streaming, the host prints a `[1s]` line: frames captured, encoded, sent and
dropped, and the device's own frame age and round trip.

## For developers

[docs/feasibility.md](docs/feasibility.md) has the design and each phase's findings.

- `tools/receive.py` checks a `--plain` door's messages and decodes the stream with ffmpeg.
- `tools/windows.py` walks every window.
- Sill's own `Scripts/sillclient.py` pairs and opens a session like the app.

Test switches:

- `SILL_LINUX_ENCODER=nv|va|x265` picks the encoder.
- `SILL_LINUX_SHM=1` forces shared memory instead of DMA-BUF.
- `SILL_LINUX_INPUT_LOG=1` prints every input action.

Dependencies are system libraries every distribution ships (PipeWire, GStreamer, D-Bus, Avahi) and
well-maintained Rust crates for them.

## License

Apache-2.0, as Sill. See [NOTICE](NOTICE) for what comes from Sill.
