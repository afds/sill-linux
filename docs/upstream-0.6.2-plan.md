# Catching up with Sill 0.6.2

Written 2026-11-13. Sill moved from the pinned `5c323e6` (2026-10-17) to `bf69067f` (0.6.2, 832
commits). The wire is still `sill/1`; every addition is an optional field or a kind an older host
skips (Sill's CLAUDE.md, "Compatibility floor"), so a 0.6.2 device works with sill-linux 0.1.2
as it is.

## Nothing to do here

- Device only: Picture in Picture, full screen, pinch to zoom, Reverse Scrolling, the Duo's
  keyboard, the version label, saved Macs' VPN addresses, the device's auto-lock.
- Mac only: the menu bar, the firewall check, nightly builds, away through a relay, the
  off-console view (another user on the Mac's screen).
- Left out on purpose: the locked list (`locked`, `offConsole`): this host only keeps the session
  awake; a lock still ends sharing with the "locked" goodbye.
- Already here: capture scale.

## Step 1: small and useful

1. Kind 44, a device's keyframe request after its picture was interrupted (PiP, the app away):
   a keyframe for every device, at most one a second per connection.
2. Viewport bounds (Sill's security finding 4): a kind 9 with a side outside 1 to 16384 points or
   a scale outside 0.25 to 4 is refused whole. Aa's Off (a scale, then none) puts the window
   back, as when it stops streaming.
3. Key repeat (`keyRepeat` 1 in the window list): the device then sends a hardware keyboard's
   keys as keys, and the host repeats a held one. GNOME's clients repeat a held key themselves.
4. The door against Sill's findings 3, 9 and 10 (an open door's reconnect pin, the waiting
   connections' limits, a removed device's input): read, fix what applies.
5. The pin moves to `bf69067f` (SILL_REFERENCE.md).

## Step 2: the Desktop's displays (done 2026-11-13)

`desktopDisplays`, `desktopDisplay`, `desktopDisplayNames` in the window list and kind 47 (the
Desktop button steps to the next display): the main monitor first, then left to right, wrapping.
A fresh RecordMonitor per step, as a switch already is; input and the pointer follow. Tested with
two virtual monitors in the nested shell.

## Step 3: Send Audio (kind 43), a spike first (done 2026-11-13)

- Capture: PipeWire, the default sink's monitor for the Desktop, the app's own stream (by PID)
  for a window.
- Codec: a device's hello lists only `aac-eld`. GStreamer's `fdkaacenc` has lc, he-aac-v1/v2 and
  ld, not ELD, so libfdk-aac itself (Ubuntu's multiverse: a packaging and licence question).
- Then `Audio.swift`'s packets, the sound against the picture's clock, kind 16's `sendAudio` and
  `audioNote`.
- The spike: AAC-ELD heard on the iPad, before the rest.
