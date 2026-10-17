# sill-linux

An experimental Linux (Wayland) host for the [Sill](https://getsill.app) iPhone and iPad app:
stream a Linux desktop, and later single windows, to the existing Sill app over your network.

**Status: feasibility and planning only. Nothing works yet.** Read
[docs/feasibility.md](docs/feasibility.md).

This is a separate project. It is not part of Sill and isn't supported by Sill's author. Sill's
own host is the free Mac app; this one speaks the same wire protocol (pinned in
[SILL_REFERENCE.md](SILL_REFERENCE.md)), so the Sill app needs no changes.

## Dependency policy

Unlike Sill (Swift, Apple frameworks only), a Linux host can't avoid outside libraries. The rule
here: system libraries every desktop distribution ships (PipeWire, libei, D-Bus, Avahi, FFmpeg's
libavcodec) and well-maintained Rust crates for them (zbus/ashpd, pipewire-rs, reis, rustls,
ffmpeg-next). No services, no telemetry, no network access beyond the devices you pair.

## License

Apache-2.0, as Sill.
