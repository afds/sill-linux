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
2. **Input.** RemoteDesktop and libei: tap and click, scroll with phases, HID keys, text.
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
