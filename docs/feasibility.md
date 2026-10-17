# A Linux (Wayland) host for Sill: feasibility

Written 2026-10-17 against Sill at the commit in [SILL_REFERENCE.md](../SILL_REFERENCE.md).
Nothing here has been built or run yet: the findings come from reading Sill's code and probing one
Linux machine read-only.

## Summary

A Linux host can serve the **existing** Sill app without changing it. The Desktop works the same
way on every Wayland desktop, through the screen-cast portal. Picking one window from the device's
thumbnail bar, Sill's core feature, needs separate code for each desktop family, and GNOME also
needs a GNOME Shell extension, because GNOME refuses to list windows to an ordinary process.

The host is written in Rust. Sill's protocol files that matter most (TLS, pairing, signatures) use
Apple's Security, Network and CommonCrypto, so sharing Sill's Swift code would save little, and
Rust has the better Linux libraries.

## The machine probed

GNOME Shell 50.1 on Wayland, PipeWire 1.6.2, an RTX 3060. FFmpeg has `hevc_nvenc`, `hevc_vaapi`,
`hevc_vulkan` and `libx265`. libei 1.5, Avahi, GStreamer 1.28, Rust 1.96; no Swift.

- Portal ScreenCast version 5, `AvailableSourceTypes` 7 (monitor, window, virtual);
  RemoteDesktop version 2; InputCapture; Clipboard.
- Mutter's own ScreenCast API version 4, RemoteDesktop version 1.
- `org.gnome.Shell.Introspect.GetWindows` → `AccessDenied: GetWindows is not allowed`.

## 1. What the Sill app needs from a host

Sill's app is the fixed point. Nothing in it is Mac-bound beyond wording ("Mac", "Retina") and
optional features. File references are to Sill.

### Framing and video

- **Header** (`StreamMessage.swift:115`): 14 bytes, big-endian: kind (1), timestamp (8, a Double's
  bits, Unix seconds; used only for frame age), isKeyframe (1), length (4). Unknown kinds are
  skipped. Caps: frames 32 MiB, other host messages 4 MiB, client messages 1 MiB.
- **Codec** (`HEVCEncoder.swift:162`): HEVC Main, real time, no B-frames, a keyframe at least every
  `fps × 4` frames.
- **Kind 1, frame**: one access unit as **4-byte length-prefixed NAL units** (hvcC style, not
  Annex-B). Linux encoders give Annex-B, so the host rewrites start codes as lengths.
- **Kind 0, parameter sets** (`StreamMessage.swift:138`): `nalLen(1) count(1)`, then for each of
  VPS, SPS, PPS `len(2 BE) + NAL` without start codes. Sent before every keyframe, with
  isKeyframe 1, and cached for clients that join (`StreamServer.swift:1456`). Lift the three out of
  the IDR, and strip them from kind 1 to be safe.
- **Decoder** (`iOSClient/HEVCDisplayView.swift`): drops everything until a flagged keyframe; a
  changed kind 0 flushes and waits for the next keyframe; presentation timestamps are ignored
  (display immediately). The client never asks for a keyframe, so the host sends an IDR when a
  client joins and after it drops a frame for that client.

### Session

- **Minimum host**: kind 2 (window list), 0 and 1 (video), 11 (pong: echo the ping's 8 bytes),
  13 (an empty tick every 30 ms while streaming, which keeps the device's Wi-Fi out of power save).
- **Kind 2 is required**: over TLS the app counts a session as connected only at the first window
  list (`StreamClient.swift:1559`), and then asks for the Desktop itself when `active` is none.
- **Optional**, hidden by the app when absent: 14 cursor shape (`hotX hotY w h` as Float32 BE in
  points, then PNG), 16 host settings, 18 signed host info (away from home), 24/25/27 menus (the
  app's request just times out), 26 pointer, 28 gestures (only with `WindowList.gestures`),
  32 clipboard (only with `WindowList.clipboard`).
- **Kind 23 hello**: the device's first message of every session. Can be logged and ignored.

### Discovery, TLS and pairing (what an App Store build dials)

A Release build of the app dials only TLS doors; a Debug build still dials a plain TCP door.

- **Bonjour** `_sill._tcp` with TXT `p` (`1` pairing required, `0` open; no `p` makes the app show
  "Update Sill") and `r`, the recognition tag: base64url of `n6 ‖ HMAC-SHA256(recognitionKey,
  "sill-tag-v1" ‖ n6)[0..<6]`, n6 fresh random bytes per registration (`Pairing.swift:143`).
- **TLS 1.3 only** (`RemoteTLS.swift:53`): no resumption; the server requires a client
  certificate and offers ALPNs `sill/1` and `sill-pair/1`; clients send SNI "sill". Trust is the
  SHA-256 of the peer's P-256 SPKI; no chain, name or date checks. Any self-signed P-256
  certificate works for the host; the host must accept the device's hand-built one unvalidated.
- **Pairing** (`sill-pair/1`): one kind 19 from the device (≤ 4096 bytes, within 10 s), one
  kind 20 back, then close. Typed code: 12 digits, the last a Damm check digit; key =
  PBKDF2-HMAC-SHA256(code, `"sill-pair-v1" ‖ fp_mac`, 600 000 rounds, 32 bytes). QR: a 16-byte
  secret in the link. Proofs are HMACs over both TLS fingerprints (`Pairing.swift:106`). An ok must
  carry `proof`, `macID`, `name` and `recognitionKey`. The home door also takes `ask` and
  `cancel`.
- **Kind 18** (optional): base64 MacInfo JSON, the X9.63 public key, a DER ECDSA-P256-SHA256
  signature; `macID` is Crockford base32 of the fingerprint's first 10 bytes; signed by the TLS key.
- `Scripts/sillclient.py` already does the device's side of all this with Python and OpenSSL, so a
  non-Apple TLS stack interoperates.

### Input (`Input.swift`)

- Positions are 0…1 fractions of the streamed frame. Pointer: move, left and right down and up;
  modified clicks come as modifier keys held around the click.
- Scroll: deltas as frame fractions (natural sign), with gesture phases began, ended,
  momentumBegan, momentumEnded; the app makes its own momentum. On Wayland: smooth axis with
  finger source and an axis stop; momentum dropped or merged.
- Text: Unicode, typed whatever the keyboard layout; `\n`, `\t` and `\u{8}` mean Return, Tab and
  Backspace.
- Keys: USB HID usages (0xE0–0xE7 the modifiers) with modifier bits shift 1<<17, control 1<<18,
  option 1<<19, command 1<<20. Needs a HID → evdev table and a choice for Command.

### JSON shape

Sill's JSON is Swift's synthesized Codable. Enums with payloads look like
`{"pointer":{"_0":"leftDown","x":0.5,"y":0.5}}`, `{"key":{"hidUsage":4,"down":true,"modifiers":0}}`,
`{"text":{"_0":"a"}}`, and sources `{"window":{"_0":123}}`, `{"desktop":{}}`, `{"none":{}}`. Serde
must reproduce these exactly; golden tests against Sill's own encodings are the guard.

### Catalog (`Switcher.swift`)

- WindowList: `macName`, `windows`, `active`, optional `launchID`, `hostVersion`, `protocol`,
  `gestures`, `clipboard`.
- WindowInfo: `id` (u32, any stable id), `title`, `appName`, `bundleID`, `width`, `height` in
  points. On Linux `bundleID` can be the `.desktop` file id, which also keys icons and `launchApp`.
- Thumbnail: `windowID(4 BE) + JPEG`, every 1–2 s per window. Icon: `idLen(2 BE) + id + PNG`, once
  per app.

## 2. Capture, per desktop family

| | Desktop | One window, picked on the device | Window list |
|---|---|---|---|
| wlroots, COSMIC, niri, Hyprland | `ext-image-copy-capture-v1` on an output, or the portal | `ext-image-copy-capture-v1` with `ext-foreign-toplevel-image-capture-source-v1` (COSMIC, Jay, niri in progress); Hyprland: its toplevel export | `ext-foreign-toplevel-list-v1` |
| KDE Plasma | the portal, or `zkde_screencast_unstable_v1.stream_output` | `zkde_screencast_unstable_v1.stream_window(uuid)`, a PipeWire stream | `org_kde_plasma_window_management`; both granted only through `X-KDE-Wayland-Interfaces` in the host's `.desktop` file |
| GNOME | the portal, or Mutter's `RecordMonitor` | Mutter's `RecordWindow(window-id)`, but no ordinary process can learn window ids; the portal's window source makes the user pick on the Linux screen | only through a GNOME Shell extension exposing windows and ids over D-Bus |

- The Desktop works everywhere through the portal: one consent dialog, then `persist_mode` 2 and a
  restore token (for screen cast plus input, persistence goes through RemoteDesktop's
  `SelectDevices`, not ScreenCast's `SelectSources`).
- The portal's *virtual* source is the analog of Sill's virtual display: a headless monitor that
  could be sized to the device's panel.
- KWin and Mutter do not implement the `ext-image-copy-capture` protocols; they use PipeWire.

## 3. Input injection

- GNOME and KDE: portal RemoteDesktop, `ConnectToEIS`, then libei: absolute and relative pointer,
  buttons, smooth and discrete scroll, keycodes.
- wlroots family: `zwlr_virtual_pointer_v1` and `zwp_virtual_keyboard_v1`, or libei where the
  compositor has it.
- Unicode text has no "type this string" call as on macOS: either upload a keymap with the needed
  keysyms (virtual keyboard), or look up each character in the current keymap and fall back.

## 4. Encoding

HEVC without B-frames at low latency: NVENC on NVIDIA, VA-API on Intel and AMD, x265
(`ultrafast`, `zerolatency`) as the software fallback, through libavcodec (`ffmpeg-next`). PipeWire
DMA-BUF straight into the encoder where possible; shared memory first in the spike.

## 5. Discovery and doors

Avahi for `_sill._tcp` with `p` and `r`; rustls for TLS 1.3 with a client certificate and the two
ALPNs; P-256 keys; the pairing code and proofs; kind 18. Every byte must match Sill:
golden tests take their vectors from Sill's `Tests/checks/protocol`, and `sillclient.py` drives
the host end to end.

## 6. Plan

Each phase is usable on its own.

0. **This repo and this doc.** Done: the crate, the license, the dependency policy (README), the
   pinned Sill commit (SILL_REFERENCE.md).
1. **The Desktop to the app, on GNOME.** Built; results in "Phase 1 results" below. Portal ScreenCast (monitor, cursor as metadata) →
   PipeWire shared-memory frames → libavcodec `hevc_nvenc` (zero latency, no B-frames, IDR on
   demand) → Annex-B rewritten as length prefixes, kind 0 split out → a plain TCP door. Sends a
   window list with no windows and `active` desktop, pongs and ticks. Tested first with
   `python3 <sill>/Scripts/sillclient.py PORT 10 desktop --stats`, then with a Debug build of the
   app by address (`-SillConnect host:port`, `-SillHUD 1`). That build needs a Mac with Xcode;
   without one, phase 3 comes before phase 2 so the App Store build can connect.
   Success: frame age and fps in the app's stats comparable to the Mac host's.
2. **Input.** RemoteDesktop and libei: tap and click, scroll with phases, HID keys, text. Built
   with the portal's D-Bus calls, not libei (results below).
3. **TLS door, pairing and Avahi**, so an App Store build finds, pairs with and reconnects to the
   host. Built, ahead of phase 2 (results below). Golden tests reproduce Sill's fingerprints, tags, PBKDF2 proofs and signed info.
4. **Windows.** A backend trait (`list`, `thumbnail`, `capture`, `raise`): the wlroots family and
   COSMIC first (no prompts, real per-window capture), then KDE, then GNOME's extension.

Out of scope for now: the virtual display analog, menus (24, 25, 27), gestures (28), clipboard
(32), away-from-home quality, update notices, packaging.

## 7. Open questions

For Sill's author (Noah):

1. Is a separate host for the Sill app welcome, and how should it be named and presented
   (support load; the App Store copy says "Mac")?
2. The app says "Mac" everywhere. Leave it, or add an optional `hostKind` to the window list
   later (a change in Sill)?
3. Can third-party hosts rely on the protocol staying stable? Sill's compatibility floor already
   promises additive-only changes, which covers most of this.

For this project:

4. Command → Ctrl (app shortcuts work as on the Mac) or Super (desktop shortcuts)?
5. GNOME windows: ship a Shell extension, or give GNOME the Desktop and portal-picked windows only?
6. Packaging: a native package (KDE's and wlroots' privileged protocols work) or Flatpak (portals
   only, so no window list on KDE and wlroots either)?

## Sources

- [ext-image-copy-capture-v1](https://wayland.app/protocols/ext-image-copy-capture-v1),
  [ext-image-capture-source-v1](https://wayland.app/protocols/ext-image-capture-source-v1)
- [niri: ext-image-copy for toplevels](https://github.com/niri-wm/niri/pull/4599)
- [Portal ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html),
  [Portal RemoteDesktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
- [Mutter ScreenCast D-Bus API (RecordWindow)](https://github.com/jadahl/gnome-remote-desktop/blob/master/src/org.gnome.Mutter.ScreenCast.xml)
- [KDE screencast protocol](https://wayland.app/protocols/kde-zkde-screencast-unstable-v1)

## Phase 1 results (2026-10-17)

Built as planned, on this machine (GNOME 50.1, RTX 3060, a 3840×2160 monitor at scale 2):
portal → pipewiresrc (shared memory) → nvh265enc → h265parse → appsink → the plain door, with
Avahi's `avahi-publish` for `_sill._tcp`.

Verified without a device:
- `cargo test`: 10 tests (header, parameter sets, Annex-B conversion, Swift's JSON shapes, a
  session as the app runs it, a joining client's keyframe request).
- Sill's own `Scripts/sillclient.py` against the door: window list, the Desktop pick, pongs,
  34 ticks a second (Sill's rate).
- `tools/receive.py` on loopback and over the LAN address: 3840×2160 HEVC Main, 4:2:0, no
  B-frames, decoded by ffmpeg with no error; the first keyframe 0.04–0.2 s after the pick; a
  device joining mid-stream got its keyframe in 0.02–0.46 s; nothing dropped; frame age under
  1 ms on loopback. A decoded frame showed the desktop as it was.
- CPU about 4–15 % of one core while streaming; the restore token skips the dialog.

Found on the way:
- **pipewiresrc's clock.** Its keep-alive resend waits on the pipeline clock, which was
  PipeWire's own; with the screen still it never fired and the stream stalled for seconds (the
  streaming thread sat in `gst_clock_id_wait`). The pipeline now runs on the system clock.
- **The valve and renegotiation.** A valve in its default mode drops caps too, so opening it
  made the encoder renegotiate and pipewiresrc reconnect its PipeWire stream; with
  `drop-mode=forward-sticky-events` opening it changes nothing upstream.
- **DMA-BUF on NVIDIA.** The zero-copy path (DMA-BUF → glupload → nvh265enc) negotiated and
  encoded, but the picture was garbage (NVIDIA's tiled layouts read as linear). Shared memory is
  the default; `SILL_LINUX_DMABUF=1` keeps the experiment. Next to try: `cudaupload`/CUDA import
  with explicit modifiers, or asking the portal for linear buffers.
- GNOME sends a frame only when something on the screen changes, so a still screen streams the
  0.5 s keep-alive and nothing else, as the Mac host's still windows do.

Not measured yet: the frame rate of a busy full screen (every test window opened behind the
terminal), the shared-memory copy's ceiling at 4K, and anything on a real device: frame age,
rtt and the picture on the iPad, which is the next step.

## Phase 3 results (2026-10-17)

Brought ahead of input because the App Store build shows "Update Sill" for a door without `p`,
and a Debug build needs Xcode. Built: rustls (ring) for TLS 1.3 with a required client
certificate, no tickets or resumption, ALPNs `sill/1` and `sill-pair/1`; any P-256 client key
passes the handshake and the door decides by fingerprint and ALPN. The host key (P-256, PKCS#8),
the recognition key and the paired devices are kept in `~/.local/state/sill-linux/` (0600); the
certificate is made fresh from the key at each launch, as Sill's. Avahi's record carries `p=1`
(or `p=0` with `--open`) and `r`. Pairing follows Sill's host: an ask opens a 5-minute window and
answers `shown`; five wrong codes stop it, 1, 2, 4, 8 s apart; `cancel` closes it; any `v` but 1
is `closed`; an unpaired key on `sill/1` gets the goodbye `pairingRequired`.

Verified:
- Sill's vectors from `Tests/checks/protocol` in Rust: the PBKDF2 code key, proof_D and proof_M
  on both paths, the Mac ID and the recognition tag (`cargo test`, 17 tests in all).
- Sill's own `Scripts/sillclient.py` (Python and OpenSSL, independent of this code) against the
  door: an unpaired session refused with kind 22 first; an ask answered `shown`; the code paired
  with "proof_M checked, pin saved"; a pinned `sill/1` session with hello, the Desktop pick, pongs
  and 34 ticks a second; a cancel answered `closed`.
- `avahi-browse` shows the record with `p=1` and a recognition tag.

On a device (2026-10-17): the App Store app, Sill 0.5.1 (4) on an iPad (iPad16,1), paired by the
typed code over IPv6 link-local and streamed the Desktop, "the picture looks smooth". Over about a
minute of a mostly still screen: 2–5 frames a second (what changed, plus the 0.5 s keep-alive),
nothing dropped, frame age 0–18 ms (median 0–7), rtt 3–6 ms; the app sent input (ignored).
The app's panel is 728×535 pt, so a 3840×2160 stream is far more than it shows.

Not yet tried in this phase: Not built: the QR path is printed only with `qrencode`
installed and is untested; kind 18 (no "away from home" for this host); Remote Access.

## Phase 2 results (2026-10-17)

Input through the RemoteDesktop portal's D-Bus calls (`NotifyPointerMotionAbsolute`, buttons,
`NotifyPointerAxis` with `finish`, keycodes, keysyms); libei is not needed for this. The screen
cast is now part of the RemoteDesktop session (one dialog, its own restore token);
`--view-only` keeps the screen-cast-only session. `input.rs` translates Sill's events, pure and
tested (10 tests): positions in the stream's pixels, natural scrolling negated for Wayland with
the axis stop after the device's own coast (or 0.12 s after a lift with none), HID usages through
Linux's `usb_kbd_keycode` table, Command as Ctrl (or Super) and ⌘Space as a Super tap, modifier
bits pressed around a key whose modifier key is not down, text as keysyms with held keys released
first, and every key and button released when a device leaves.

Verified here: the portal grants "pointer and keyboard"; ten pointer moves from a plain-door
client, no portal error; the decoded stream shows the pointer at exactly the asked place (20 %,
30 %). Found on the way: GNOME takes absolute motion in the stream's own pixels (3840×2160 at
scale 2), not the logical size the portal reports (1920×1080); the first try put the pointer at
half the distance. Then, on the iPad, the trackpad stopped at the screen's middle: GNOME's portal
refuses absolute positions outside the size it reports ("Invalid position" past 1920×1080) while
the compositor divides them by the scale, so absolute motion reaches only the top-left quarter at
scale 2. A place outside that box is now reached by its nearest point inside it and then relative
motion in logical pixels (unaccelerated: the pointer landed within 2 px of 80 %, 75 % in the
decoded stream). Then, on the host's own screen, the pointer flickered to the middle and back
during moves in the right or bottom half (the compositor drew between the jump and the relative
move): now only the first move of a stroke, after a second without one, jumps; the rest are
relative from where the translator put the pointer last (logged live: one jump, then nine relative
moves summing to exactly 80 % of the width; on the iPad, "much better"). libei's absolute device, with the compositor's own regions, would avoid the
two-step move. On the iPad (2026-10-17) after that fix: "the controls work fine". Not tried here, on purpose (they would act on whatever has focus): clicks,
scrolls, keys and text; those are for the device test.

## Settings and resolution (2026-10-17)

The app's Settings panel works (kinds 16 and 17, Sill's `HostSettings.swift` and
`DeviceSettings.accepted`): Quality (Sill's seven presets, per 60 fps), Resolution (Retina, the
stream's pixels; Standard, the monitor's logical size, scaled on the GPU with
glupload → glcolorconvert → glcolorscale into nvh265enc's GL input), Frame Rate limit (60/120,
the stream at the lowest of it and the fastest device's viewport). Prioritize Speed is reported on
and "off" refused (NVENC already runs at its lowest-latency preset); the virtual display is
reported unavailable with a note; no Direct Wireless row. Changes apply live (the encoder's
bitrate, the size filter's caps, a frame-rate limiter before the encoder), with a keyframe, and are
saved in `settings.json`. Standard is this host's default, Sill.app's is Retina: the app's panel is
far smaller than a 4K picture.

Verified: `cargo test` 31; Sill's `sillclient.py --set=…` against the door: kind 16 after the window
list; Retina and Pro answered with its token, saved, and the live stream went from 1920×1080 to
3840×2160 in 0.25 s with a keyframe (both decoded by ffmpeg); 200 Mbps refused with the setting
kept. Input still lands in the captured stream's pixels whatever the encoded size (the motion
actions logged in Standard mode: 80 % of the width exactly).

Found on the iPad: Retina → Standard ended the capture ("no more input formats", then, with the
reconfigure stopped at the valve, "not-negotiated": glcolorscale would not renegotiate from
3840×2160 to 1920×1080 mid-stream). Changing the size live is gone: a resolution change now replaces
the half after the valve (scaler, encoder, parser, appsink) while pipewiresrc runs on, the valve's
sticky caps going to the new half with its first buffer, and upstream reconfigure events stopped at
the valve so pipewiresrc never renegotiates. Five switches back and forth through Sill's test
client: each a keyframe 0.5–0.9 s later, the capture never ended. Bitrate and frame rate still
change live.

The pointer is no longer in the picture by default (`--show-cursor` puts it back): as Sill for
Mac, the app draws its own where it needs one, and the compositor no longer sends a frame per
pointer move. Moves of this machine's own mouse are then not seen on the device (Sill for Mac's
kind 26 shows them; not built here).

Not done: the pointer drawn on the device (kinds 14 and 26: needs PipeWire's cursor metadata,
which pipewiresrc does not pass on, so pipewire-rs directly); a busy screen's frame rate on the
iPad is still to be measured.

## Stray clicks from the trackpad (2026-10-17)

Reported on the iPad: "sometimes when I use the trackpad and lift my finger after moving the
cursor, it clicks". The timed input log (`SILL_LINUX_INPUT_LOG=1`) showed the clicks are the app's
own: a press and its release 1 ms apart, from iOS's tap recognizer on the trackpad, which allows a
little movement and a long touch, so a slow, small nudge followed by a lift is a tap (650 ms of
moves, the click 69 ms after the last). It would do the same against Sill for Mac.

Worked around here (`input::Translator`, on by default, `--keep-all-taps` off): a tap within
120 ms of a run of moves at least 150 ms long is dropped; a real tap (a twitch of tens of
milliseconds, or a pause first) and a drag (0.45 s of stillness first) are kept, and a press whose
release does not come within 40 ms (the Pencil after hovering) goes through that much later.
`examples/replay-input.rs` replays a log: on the reported session it kept the two real taps and
dropped only the stray one.

For Sill's app (Noah): the trackpad's tap recognizer could require that the touch moved the
pointer less than a few points, or fail it once the finger tracker has moved the cursor.

## Fullscreen video and DMA-BUF (2026-10-17)

Reported on the iPad: a YouTube video put fullscreen stopped updating. The log: 2 frames a
second (the keep-alive alone) while fullscreen, 18–33 outside it. Known: mutter #3074 (PipeWire
capture broken on unredirected windows, Wayland, NVIDIA): a fullscreen window shown by direct
scanout gives a shared-memory screen cast no new frames.

DMA-BUF is now the default for NVENC, shared memory the automatic fallback (when the pipeline
fails before its first frame) and `SILL_LINUX_SHM=1`. The garbage picture of phase 1 came from
glupload's indirect DMA-BUF import, which read NVIDIA's tiled buffers as linear (GNOME 50 offers
only NVIDIA block-linear modifiers, `AR24:0x03…`, no linear); asking glupload for an
`external-oes` texture selects its direct import, which hands EGL the modifier, and the picture is
right. The upload and colour conversion moved into the half that stays (a new glupload would not
take the running DMA-BUF format, "not-negotiated"), so a resolution change replaces only scaler,
encoder, parser and appsink, linked without the up-front caps check. Verified: four Retina/Standard
switches on each path, a keyframe each, decoded by ffmpeg; a crop of the dock correct.

On the iPad (2026-10-17): fullscreen YouTube keeps updating with DMA-BUF, "it works great".
Kept for anyone where it does not: a fullscreen YouTube video should keep updating with DMA-BUF (if not, the system
switch `MUTTER_DEBUG_PAINT=disable-direct-scanout` in `~/.config/environment.d/`, then a new
login, makes GNOME always composite).

## Sound (parked, 2026-10-17)

The App Store app plays no sound yet. Sill pull request #94 (Code-Tomato, "The Mac's sound on the
device: Send Audio, kind 43, AAC-ELD", open and conflicting with main on 2026-10-17) adds it:
kind 43 `audio`, host to device, only to a device whose hello lists `aac-eld`. Its payload is
either `0x01` + JSON `AudioFormat` (`codec`, `sampleRate`, `channels`, `framesPerPacket`,
`primingFrames`, `bitrate`, `epoch`, `cookie` (base64), `source`, `app`) or `0x02` + binary
`AudioPackets`: `epoch` u16, `seq` u32, `segmentStart` u8, count u8, then each packet as length u16 +
bytes (at most 255 packets, 16 KiB a message). Kinds 16 and 17 gain `sendAudio`, kind 16
`audioNote`, ClientStats `audioBehindMs` and `audioLate`. 48 kHz stereo, 128 kbps.

For this host once #94 ships: capture the default output's monitor from PipeWire (the Desktop's
sound; one app's node once single windows exist), encode AAC-ELD with libfdk-aac directly
(GStreamer's fdkaacenc offers lc, he-aac-v1/v2 and ld, not ELD; the library itself has AOT 39),
and frame kind 43 as above. To settle first: the `cookie` the device's AudioToolbox decoder
accepts (an AudioSpecificConfig, or an ESDS as Apple's encoder gives it); the PR's
`AudioDecoder` and `docs/audio-plan.md` say. Decided (2026-10-17): wait until #94 ships rather than
test against a Debug build of its branch.

## Everyday use (2026-10-17)

- `--install-service`: a systemd user unit (`~/.config/systemd/user/sill-linux.service`,
  `WantedBy=graphical-session.target`, `Restart=on-failure`), with the options given beside it;
  `--uninstall-service`. The session's user environment has `WAYLAND_DISPLAY` and the session bus,
  so the portal works from the service.
- The pairing code as a desktop notification (`org.freedesktop.Notifications` through `gdbus`,
  critical urgency so it stays), closed when the device pairs, cancels, the code runs out or five
  wrong codes stop it; `--no-notifications`.
- Quitting (SIGINT, SIGTERM, SIGHUP): kind 22 `{"reason":"quit"}` to every device and their held
  keys and buttons released, then `_exit` after 0.3 s (plain `exit` hung in the libraries' exit
  handlers with their threads running). Verified with Sill's test client: the goodbye at once, exit
  status 0, no process left.
- `--choose-screen`: the saved portal choice is ignored once; the new one is saved.
- Seen once on the DMA-BUF path, mid-stream, nothing broken: GStreamer-Video-CRITICAL
  `gst_video_frame_map_id: assertion 'info->finfo->format == meta->format' failed`. To watch.

## Phase 4a: GNOME's own API (2026-10-17)

Decided with the user: on GNOME, capture and input through Mutter's D-Bus API
(`org.gnome.Mutter.ScreenCast`, `org.gnome.Mutter.RemoteDesktop`), as gnome-remote-desktop does,
and a GNOME Shell extension for the window list (phase 4b). An ordinary process may create these
sessions (verified); they live as long as its D-Bus connection.

`mutter.rs`: a remote-desktop session, a screen-cast session linked to it
(`remote-desktop-session-id`), `RecordMonitor(connector)` (the primary monitor from
`DisplayConfig.GetCurrentState`, or `--monitor`), `PipeWireStreamAdded` for the node, and input
through `NotifyPointerMotionAbsolute(stream, x, y)` and the rest. pipewiresrc connects by node to
this process's own PipeWire connection. No consent dialog (GNOME's sharing indicator shows);
`--portal` keeps the portal, which is also the fallback away from GNOME.

Verified: the capture over DMA-BUF with no dialog; absolute positions in the stream's pixels across
the whole screen in one step: a move to 80 %, 75 % (3072, 1620 of 3840×2160) put the pointer exactly
there, nothing refused. So on GNOME's API the translator has no box (`absolute_everywhere`): no
relative moves and no flash at a stroke's start.

## Phase 4b: single windows on GNOME (2026-10-17)

The app's bar lists this machine's windows and a pick streams that window alone, as with Sill for
Mac. Three parts:

- **The window list** comes from a GNOME Shell extension (`gnome-extension/`, uuid
  `sill-linux@sill-linux`, GNOME 48–51), since GNOME keeps the list from ordinary processes
  (`Introspect.GetWindows`: AccessDenied). It owns `org.sill_linux.Shell` on the session bus:
  `ListWindows` (Mutter's window id, title, app name, `.desktop` id, frame size; normal windows,
  not minimized, as Sill for Mac lists only windows on screen), `Geometry` (the frame's insets in
  the window's buffer: the shadow), `Thumbnail` (a PNG at most 208 px on its longer side, the
  frame alone), `Activate`, `Close`, `Minimize`, `ToggleFullscreen`, and `WindowsChanged`
  (created, closed, retitled, resized, minimized; at most every 250 ms). `sill-linux
  --install-extension` writes it (built into the binary) and enables it; GNOME on Wayland loads a
  new extension only at the next login. Without it the host says so and streams the Desktop alone.
- **The catalog** (`server.rs`): kind 2 lists the windows under 32-bit ids of the host's own
  (Mutter's are 64-bit), kept for the run; kind 3 thumbnails, as Sill for Mac paces them (every
  2 s for a window that changed or streams, else every 6 s, all again for a device that joins,
  none while no device is connected); kind 15 (a window's lights) closes, minimizes or toggles
  full screen through the extension. A streamed window that closes stops the stream, and the app
  asks for the Desktop.
- **The capture** (`capture.rs`): a pick records the window with `RecordWindow(window-id)` after
  `Activate` (raised and focused, so keys go to it). Found while building, in a headless nested
  GNOME Shell (its own D-Bus session, settings in a keyfile):
  - Mutter starts no stream added to a session already running (`RecordWindow` answers, no
    `PipeWireStreamAdded` follows), so every switch makes a new linked remote-desktop and
    screen-cast pair (no dialog either way) and stops the old one once the new one runs; input
    follows. The Desktop is recorded anew too: a second consumer of a running monitor stream got
    no first frame while the screen stood still.
  - On a session already started, `PipeWireStreamAdded` can come while Mutter answers the call,
    so the signal is listened for (a match rule) before the call.
  - A window's buffers are the stream's whole size (the monitor's here) with the window in one
    corner and a crop rectangle in each buffer's metadata (`GstVideoCropMeta` from pipewiresrc),
    which glupload ignores. The valve's probe reads the crop; the tail crops on the GPU with a
    small `glshader` (or `videocrop` on shared memory) to the frame inside it (the extension's
    `Geometry` takes the shadow off), then scales for Standard (the window's logical size). A
    window streams only once its crop is known (500 ms at most); a crop that changes (a resize,
    maximize) rebuilds the tail once it has held for 200 ms.
  - Input in a window: a device's position is a fraction of the cropped picture, mapped into the
    window's region of the buffer, then given to `NotifyPointerMotionAbsolute` on the window's
    stream (Mutter maps it onto the window). A window stream takes positions in the window's
    logical buffer coordinates, unlike a monitor stream (its own pixels): at scale 2, positions in
    the crop's pixels put the pointer about the shadow's width inside the frame at the top and
    left, and past the frame at the right and bottom (the iPad, 2026-10-17). So input gets the
    frame from `Geometry` in logical pixels, and Standard is the frame's logical size.

Verified in the nested shell (Text Editor and Calculator), `tools/windows.py` on a `--plain`
door: the list with both windows; Desktop → each window → Desktop, a keyframe each time; pictures
at each window's frame size (700×520, 360×616), decoded by ffmpeg with no error; thumbnails
208×155 and 122×208; minimize from the device takes the window off the list; typing into the
streamed Text Editor shows in the stream at once ("ello sill": the first letter of a fresh
headless session's first text was lost; not seen on the real desktop before). Not settled there:
clicks (in that headless shell a click activated no GTK button even on the Desktop, where hover
worked), so taps inside a window wait for the iPad. The user's real settings were compared before
and after every run.

On the iPad (2026-10-17): typing into a streamed window works, and since the logical-coordinates
fix the trackpad stays inside the window.

## Phase 4c: apps and Aa (2026-10-17)

- **All Apps and icons.** The extension's `ListApps` (the app grid's apps: .desktop id and name)
  and `AppIcon` (the icon theme's file at 64 px, through GdkPixbuf, so SVG icons become PNG) give
  kind 5 (`[{name, bundleID}]`) and kind 4 (id length, id, PNG), sent to every device as it
  connects, as Sill for Mac does. The .desktop id is the bundleID throughout, so a window's
  thumbnail gets its app's icon. The list is read once at start.
- **Launch** (kind 7 `{bundleID}`, only an app the host listed): `Launch` starts the app, or raises
  a running one; its topmost window listed within 20 s is then picked for every device.
- **Aa** (the viewport's `scale`): while a window streams, its frame is resized to the panel over
  the scale in logical pixels where it is (unmaximized first; never one in full screen), at a pick
  and at each new viewport; nothing before the device first sets Aa. The crop then follows the new
  size. A window Aa sized goes back to its whole frame before (maximized again if it was) once it
  stops streaming: another pick, the Desktop, the last device leaving, this host quitting (issue
  #57's rule on the Mac; here without its exceptions for a window resized or moved meanwhile).

Verified in the nested shell (`tools/windows.py --scale 1 --launch org.gnome.Calculator.desktop`):
172 apps and 172 icons of 64×64; both windows streamed at 1180×760 with Aa at 1× (from 700×520 and
360×616) and each put back at its old frame and place when the next source took over; launching
the running Calculator streamed its window. Not tried there: launching an app that is not running.

## This machine's pointer on the device (2026-10-17)

Kind 26 (`MacPointer`): while this machine's own mouse moves the pointer (or another device does),
every device but the one driving is sent where it is over the streamed source, and the app draws
its arrow there. Its portrait trackpad then carries on from that point. `pointer.rs` ports Sill for
Mac's `PointerControl`:
- any kind 8 hands its device the pointer, and a pointer or scroll input opens a settle of 0.25 s;
- after the settle, a read 0.5 px or more from the last position that counted is this machine's
  mouse, and a read more than 0.25 s after the one before only resets that position;
- fractions are rounded to 4 places, the far edges count as outside, and `seen` is the count of
  kind 8 messages read on that connection.

Each device is sent a report only when its own report changed, and one that drives is sent its next
one again. The extension's `PointerOver(window)` reads the pointer and the streamed window's frame.
`watch_pointer` polls it at the stream's rate while the pointer moves and every 30 ms otherwise,
only while a device watches a stream and never with `--show-cursor` (the pointer is then in the
picture). The Desktop's rectangle is the monitor's logical origin (`DisplayConfig`) and the stream's
size, read once. The cursor's shape follows (below).

Verified in the nested shell: with a second test client moving the pointer to 0.25, 0.5, 0.999 and
0, the first got exactly those fractions on the Desktop and over a window (`inside:false` while the
pointer was off the window). The driving client got none. The host-mouse rule has unit tests only:
the nested shell has no mouse of its own.

## The clipboard both ways (2026-10-17)

Kind 32 as Sill's main has it since PR #78 (`Clipboard {text, image, stop}`; `WindowList.clipboard`
1 for text). The App Store build 0.5.1 predates it and sends no kind 32, so this waits for the next
Sill release to be tried on a device. `clipboard.rs` ports Sill's `ClipboardPolicy`:
- any kind 32 subscribes and `{"stop":true}` ends it;
- text (at most 256 KB, the JSON at most 1 MiB) becomes this machine's clipboard and goes to every
  other subscriber;
- a subscription or text is taken at most 4 times a second per connection, a stop always;
- a copy whose MIME types carry a password manager's marker (KDE's `x-kde-passwordManagerHint`,
  which KeePassXC sets, and the nspasteboard.org markers) is never sent, and its text is never read;
- this host's own write of a device's text is not sent back out;
- only copies made while a device shares the clipboard are sent.

Through the extension: `ClipboardChanged` (Meta.Selection's `owner-changed` for the clipboard),
`ClipboardTypes`, `ClipboardText` and `SetClipboard` (St.Clipboard). GNOME on Wayland gives an
ordinary process no clipboard of its own. It is on by default, as in Sill.app; `--no-clipboard`
turns it off. One line per subscription or stop, never contents.

Verified in the nested shell (its own clipboard, never this machine's) with Sill's
`Scripts/sillclient.py --clipboard [--copy=TEXT@T]`, two clients:
- B's text reached A once and never came back to B;
- a copy made in that session reached both;
- the window lists said `clipboard=1`.

The marker rule has unit tests only (St.Clipboard writes one type).

## Trackpad gestures (2026-10-17)

Kind 28 (`TrackpadGesture`, Sill's six names), taken only once the window list says `gestures` 1
(GNOME, with the extension), at most four a second per connection, as Sill for Mac. Sill for Mac
turns each gesture into the Mac's own shortcut and guesses what is open; GNOME's state can be read,
so the extension's `Gesture(name)` acts on it the way GNOME's own touchpad gestures do:
- swipe up: Activities, then the app grid;
- swipe down: back the same way;
- swipe left and right: the next and previous workspace (natural direction, as on the Mac);
- pinch: the app grid, or closes it;
- spread: closes Activities, else shows the desktop (the active workspace's windows minimized);
- a pinch or a spread after that brings those windows back.

Found in the nested shell: once Activities is open, `Main.overview.showApps()` leaves it in the
window picker (GNOME 50), while setting the Show Apps button's `checked` switches to the app grid.

Verified there with a test client sending each gesture: every step answered as listed (Activities,
the app grid, nothing at the top; back down; workspace 2 and 1; the desktop and the windows back).

## Cursor shapes (2026-10-17)

Kind 14 (`CursorShapeBlob`: hotspot x, y and size w, h as big-endian Float32 points, then a PNG),
so the app's arrow takes the pointer's shape here (arrow, I-beam, hand, resize). GNOME names no
cursor; Mutter's `CursorTracker` holds the current sprite as a Cogl texture, and the extension's
`CursorShape` writes it with `Shell.Screenshot.composite_to_stream`, as GNOME's screenshots do.
Found in the nested shell: Cogl's `get_data` fills nothing a GJS caller can read (every pixel
transparent). `CursorChanged` follows `cursor-changed`; the host reads the shape at each change
(the last of a burst, 30 ms on) while a device is connected, keeps the last for devices that
connect, and sends it only when it differs. The app draws it at the size given, in its own points,
as Sill for Mac sends the Mac's cursor in points. So the sprite's pixels are divided by its scale,
taken as the sprite's size over the cursor size set in GNOME (`cursor-size`, 24), rounded: on the
iPad mini the first build drew a 200 % cursor at 48 points, twice a Mac's.

Verified in the nested shell with a test client moving the pointer from a corner onto the Text
Editor and back: an arrow (hotspot 4, 4), an I-beam (11, 12), the arrow again, each 24×24 points,
each a PNG. A headless shell has no sprite until the pointer first moves.

## A thin extension (2026-10-17)

GNOME on Wayland loads an extension's code once per login, so an extension change costs a log-out,
while sill-linux restarts in a second. The extension (interface version 2, `Version`) now only
reads GNOME's state and does plain actions, and sill-linux decides:
- which windows the bar lists (`ListWindows` reports every window, with `minimized` and
  `listed`);
- a cursor's size in points (`CursorShape` gives the sprite's pixels and `cursor-size`;
  `shell::cursor_points`);
- what a gesture does (`OverviewState`, `SetOverview`, `SwitchWorkspace`, `MinimizeWorkspace` and
  `Unminimize`; `gestures.rs`);
- Aa and its way back (`FrameState`, `MoveResize`, `SetMaximized`);
- whether a launch raises or starts an app (`AppRunning`, `ActivateApp`, `LaunchApp`).

At start the host says once when the running extension is older than its own. A log-out is needed
only when the extension's interface changes (a new kind of read or action), not for a change in
behaviour.

Verified in the nested shell, every earlier check again with the same results: windows, Aa and
its way back, launching, minimize, every gesture step, the pointer fractions, and the cursor
shapes in points.

## GNOME's sharing indicator only while a device streams (2026-10-17)

GNOME shows its orange sharing indicator while any screen-cast or remote-desktop session exists.
sill-linux used to hold one from start to end, so the indicator never went, and its stop button
ended the stream with an error: sill-linux exited, systemd started it again, and the indicator came
back.

Now GNOME's session exists only while something streams. At start the session that checks the
monitor is closed at once. A device picking a source opens a fresh one (as every switch already
did), and 3 s with nothing streaming close it, so the app's quick reconnects (another network
path, back from another app) keep it. Pressing the indicator's stop closes GNOME's session.
sill-linux sees it gone (it asks once a second, and whenever the stream fails: a stopped stream
can simply go quiet) and tells every device with a goodbye the app shows word for word and does not
reconnect after: reason "stopped", "Sharing was stopped on ‹name›. Tap it to connect again.",
`reconnect` false (the App Store 0.5.1 shows any reason it does not know this way). A device still
connected after 2 s is closed. The host keeps running; a tap on the row starts a new session.

Tested in the nested shell with a test-only extension that calls `stop()` on every remote-access
handle, as the indicator does. Handles: 0 at idle, 2 while streaming, 0 again 3 s after the device
left. Stop pressed mid-stream: the goodbye arrived 0.5 s later, the host ran on, and a new
connection streamed. Windows, launching an app, and Aa resizing and restoring ran as before.
The portal path (other desktops, `--portal`) is unchanged: its session lives for the whole run.

## Two Aa fixes (2026-10-17)

- **A window goes back by Sill for Mac's rule** (`restore.rs`, its `WindowRestore`). Unmoved: its
  whole frame from before. Moved here while it streamed: its own size where it is. Resized here:
  left as it is. Full screen: left. The frame Sill left a window at is read back once the app and
  GNOME have taken it (the first change, then 100 ms; 1 s at most). GNOME Calculator takes a size
  a few hundred milliseconds late, and GNOME moves a window that would run off the screen, so a
  read after 250 ms took GNOME's own move for the user's.
- **A launched app's window is picked once it has a size** (its first frame drawn), and Aa waits
  up to a second for one. Before, Aa could size a window still at 0×0, and its way back had
  nothing to go to.

Seen in the nested headless shell, with the build from before this change too: recording Calculator
right after it opened, then sizing it, sometimes crashed GNOME Shell 50 ("Failed to blit window
content to framebuffer", then `meta_multi_texture_get_n_planes: assertion failed`, then a
segfault). After these two fixes it did not happen in six runs, but the cause is GNOME's and
timing-dependent, so it may not be gone. On a real session a GNOME Shell crash logs the user out.

## Away from home (2026-10-17)

The app has no separate protocol for away from home. A host sends kind 18 (Sill's
`SignedMacInfo`) in every session; a device that paired with it saves the addresses (only from a
TLS session pinned to that key, only when `remoteAccess` is true and the list is not empty). Off
the host's network, the saved host shows as a "Remote" row, and a tap dials those addresses with
the same TLS 1.3 handshake, ALPN `sill/1`, at `address.port ?? remotePort`. So one port serves
both, and sill-linux puts its own port in `remotePort` (`remote.rs`, `PairingDoor::mac_info_message`).

- Signed with the TLS key (DER ECDSA P-256/SHA-256 over the exact JSON bytes; plain padded base64
  for `info`, `key` (the 65-byte point) and `sig`). Sent after the window list and the settings,
  as Sill for Mac does, and again to every session when the addresses change (looked at every 30 s).
- Addresses, at most 12: Tailscale's IPv4, other tunnels' (WireGuard, tun, point-to-point)
  IPv4, tunnels' IPv6, then this network's IPv4; never loopback, link-local or container bridges.
  WireGuard into home needs no tunnel on this machine: the device dials the home-network address
  through its own tunnel. The door already admits private, CGNAT (Tailscale) and ULA sources.
- `--no-remote`: `remoteAccess` false and no addresses.
- Not here: Sill for Mac's MagicDNS name, the internet switch (router, port forwards, global IPv6),
  pairing from away (the pairing link carries no addresses), and its 8-session cap and 8 s eviction
  for remote clients (this door's eviction applies to every client).

Checked with Sill's own `sillclient.py` in the nested shell: kind 18 arrives third (kinds 2, 16,
18), and its check passes: openssl verifies the signature, the Mac ID matches the key and the
key matches the pin. With `--no-remote`: `remoteAccess` 0 and no addresses. Not checked on a
device: the Remote row, the dial through WireGuard, and the app moving a remote session home by
itself.
