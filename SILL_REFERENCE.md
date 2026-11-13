# Sill reference

sill-linux speaks the wire protocol of [Sill](https://github.com/Saffsanity/sill). Sill's Swift
sources are the specification, and its test client and checks are the oracle. This project
compares against one pinned Sill commit and moves the pin on purpose.

- Repository: https://github.com/Saffsanity/sill
- Commit: `bf69067fe60348bfcf3d617086a8589fbdf8dbb6` (main, 2026-11-12, Sill 0.6.2, PR #364);
  moved from `5c323e6` (2026-10-17): docs/upstream-0.6.2-plan.md
- Protocol: `SillProtocol.current` 1, ALPNs `sill/1` and `sill-pair/1`, kinds 0–28, 32, 43
  (sound), 44 (keyframe request) and 47 (the Desktop's next display); this host serves 44

## What to read there

| What | Where in Sill |
|---|---|
| Header, kinds, parameter sets | `Sources/StreamProtocol/StreamMessage.swift` |
| Window list, sources, image blobs | `Sources/StreamProtocol/Switcher.swift` |
| Input events | `Sources/StreamProtocol/Input.swift` |
| Viewport, cursor shape | `Sources/StreamProtocol/Viewport.swift` |
| Host settings (kinds 16, 17) | `Sources/StreamProtocol/HostSettings.swift` |
| Hello, versions | `Sources/StreamProtocol/Compatibility.swift` |
| Signed info, pairing messages, goodbye | `Sources/StreamProtocol/Remote.swift` |
| TLS options | `Sources/StreamProtocol/RemoteTLS.swift` |
| Fingerprints, Mac ID, certificate | `Sources/StreamProtocol/RemoteIdentity.swift` |
| Pairing code, proofs, tag, link, TXT `p` | `Sources/StreamProtocol/Pairing.swift` |
| Encoder behaviour | `Sources/SillHost/HEVCEncoder.swift` |
| Fan-out, keyframes on join, ticks | `Sources/SillHost/StreamServer.swift` |
| Key mapping and modifier handling | `Sources/SillHost/InputInjector.swift`, `KeyStrokes.swift` |
| Test client (Python, OpenSSL) | `Scripts/sillclient.py` |
| Byte-level checks | `Tests/checks/protocol` (and its `crosscheck.py`) |

## Moving the pin

1. Read Sill's `git log <old>..<new> -- Sources/StreamProtocol` and its CLAUDE.md
   "Compatibility floor".
2. Update this file and any vectors copied from `Tests/checks`.
3. Re-run this project's golden tests and the sillclient.py check.
