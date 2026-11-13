# Streaming while the screen is locked: options

Written 2026-10-18. Nothing here is built yet. Read against GNOME Shell 50.1, gdm3 50.1 and
gnome-remote-desktop 50.2 (Ubuntu); Shell's code is quoted from the copy in
`/usr/lib/gnome-shell/libshell-18.so` (`gresource extract … /org/gnome/shell/ui/main.js`).

## The problem

When the screen locks, every device is cut off: GNOME ends the session, sill-linux tells devices
"locked its screen" (`Ended::Locked`, `server.rs`) and waits. A device that taps the Linux machine
while it is locked is refused until someone unlocks it at the desk.

## Why: GNOME Shell inhibits remote access on the lock screen

This is not Mutter on its own; GNOME Shell does it on purpose, in `main.js` `_sessionUpdated()`:

```js
const remoteAccessController = global.backend.get_remote_access_controller();
if (remoteAccessController && !global.backend.is_headless()) {
    if (sessionMode.allowScreencast && _remoteAccessInhibited) {
        remoteAccessController.uninhibit_remote_access();
        _remoteAccessInhibited = false;
    } else if (!sessionMode.allowScreencast && !_remoteAccessInhibited) {
        remoteAccessController.inhibit_remote_access();
        _remoteAccessInhibited = true;
    }
}
```

- The `user` session mode has `allowScreencast: true`. The lock screen's mode, `unlock-dialog`,
  does not set it and inherits `false` from the default mode, `restrictive` (`sessionMode.js`).
- `inhibit_remote_access()` (Mutter's `MetaRemoteAccessController`) stops every running
  `org.gnome.Mutter.ScreenCast` and `RemoteDesktop` session, and refuses new ones ("Session
  creation inhibited") until it is uninhibited. It is a counter.
- The portal goes through the same Mutter sessions, so `--portal` is cut off too.
- Headless sessions are exempt (`!global.backend.is_headless()`); option 4 rests on that.

This machine's settings: `lock-enabled` true, `idle-delay` 720 s (screen blanks after 12 idle
minutes), `lock-delay` 180 s (locks 3 minutes after that).

## Options at a glance

| | Connect while locked | Size | Keeps GNOME's lock screen | Risk |
|---|---|---|---|---|
| 1. Extension skips the inhibit | yes, to the lock screen | ~20 lines of JS plus guards | yes | patches Shell internals |
| 2. Idle inhibit while streaming | no | ~15 lines of Rust | yes | none |
| 3. Turn off locking | — (never locks) | none (a setting) | no | the whole machine |
| 4. Headless session through GDM | yes, to a different desktop | a second architecture | n/a | large |

## 1. The extension skips GNOME's inhibit

sill-linux already ships a GNOME Shell extension (`gnome-extension/`). It can stay enabled on the
lock screen and turn Shell's two calls into no-ops.

### How

- `metadata.json`: `"session-modes": ["user", "unlock-dialog"]`. Without it Shell disables the
  extension on lock and enables it again on unlock (which also drops its D-Bus name while locked).
- In `enable()`, with Shell's `InjectionManager` (`resource:///org/gnome/shell/extensions/extension.js`),
  override on `Meta.RemoteAccessController.prototype`:
  - `inhibit_remote_access`: do nothing, remember that it was skipped.
  - `uninhibit_remote_access`: if a skip is remembered, forget it and do nothing; otherwise call
    the original. Pairing them matters: Mutter's counter must not be decremented below what was
    incremented (Mutter guards it with `g_return_if_fail`, a critical in the journal).
- In `disable()`: restore the methods (`InjectionManager.clear()`), and if a skip is still
  remembered (the extension was turned off while locked) call the real `inhibit_remote_access()`
  so the lock screen ends up as GNOME intended. Shell's own `_remoteAccessInhibited` stays true,
  so its later uninhibit on unlock balances that.
- Shell calls these from JavaScript through the prototype, so the override catches them. The
  extension enabled while already locked does nothing about the inhibit already in place; locking
  again fixes it. (Not handled: it is rare and the user is at the machine to enable it anyway.)

### What the device gets

- Running Desktop streams survive a lock and show the real lock screen.
- A new connection while locked starts the Desktop as usual and shows the lock screen.
- The device can unlock: taps and keys already go through the RemoteDesktop session into Clutter,
  so the password typed on the iPad reaches the unlock dialog. GNOME's own lock screen, its
  password check and its rate limits are untouched.

### Guards that must come with it

Without these, the lock screen keeps out only someone at the desk, not someone holding a paired
device.

- **Window streams.** `RecordWindow` records the window's own surface, even when covered (that is
  why sill-linux uses it). Under the lock screen it would keep showing the app. On lock, a window
  stream must end and the device be moved to the Desktop (which shows the lock screen); while
  locked, `Source::Window` must be refused. sill-linux already asks `org.gnome.ScreenSaver.GetActive`
  (`mutter::screen_locked`); it would also subscribe to its `ActiveChanged` signal.
- **The extension's own methods.** On the lock screen they are reachable for the first time. While
  `Main.sessionMode.isLocked`, these must refuse (return their "none" value): `ListWindows`,
  `Thumbnail`, `MonitorThumbnail` (renders the windows below the lock screen), `Geometry`,
  `FrameState`, `PointerOver`, `ClipboardTypes`, `ClipboardText`, `SetClipboard`, `Activate`,
  `Close`, `Minimize`, `ToggleFullscreen`, `MoveResize`, `SetMaximized`, `SetOverview`,
  `SwitchWorkspace`, `MinimizeWorkspace`, `Unminimize`, `ActivateApp`, `LaunchApp`,
  `ShowPairing` (no pairing from a locked machine). `WindowsChanged` and `ClipboardChanged` stop
  firing. `CursorShape`, `Version`, `ListApps` and `AppIcon` reveal nothing private and may stay.
- **The thumbnail bar.** The host sends device thumbnails of windows; with `ListWindows` empty
  while locked it sends none; on unlock the host must ask again (`ActiveChanged` going false), since
  nothing in the window list changed to fire `WindowsChanged`.
- **The goodbye.** `Ended::Locked` stays for when the extension is missing or older: the host
  checks the extension's `Version()` and only expects to survive a lock with the new one.

### Cons

- It patches GNOME Shell internals. `_sessionUpdated` and the method names have been stable for
  several releases, but nothing promises it.
- extensions.gnome.org would very likely reject it (an `unlock-dialog` session mode needs a review
  justification, and this one undoes a security measure). The extension is installed by sill-linux
  itself, so this matters only if it is ever published there.
- It deliberately undoes a choice GNOME made. Anyone holding a paired device sees the lock screen,
  including its notifications (whatever GNOME shows there per the user's settings), and can try
  passwords. Pairing is already the trust boundary: an unlocked session gives a paired device full
  input, so this widens it only to "also while locked".
- GNOME's sharing indicator is not shown on the lock screen's panel, so the person at the desk does
  not see that a device is watching while locked.
- Blank monitor: after `idle-delay` the monitor powers off. Whether Mutter keeps producing frames
  for a powered-off monitor is untested; input from the device should wake it (virtual input
  devices count as activity for Mutter's idle monitor), also untested.
- Every Shell release needs a check that the lock still cuts off nothing else (e.g. the portal
  checking `allowScreencast` on its own).

## 2. Inhibit idle while a device streams

Ask GNOME's session manager not to go idle while a device is connected:
`org.gnome.SessionManager.Inhibit(app_id, 0, reason, 8)` (flag 8: idle) when streaming starts, and
`Uninhibit(cookie)` (or drop the D-Bus connection) when the last device leaves.

- Stops the screen from blanking and auto-locking under a running stream. This matters most for
  window streams watched without input, where nothing counts as activity.
- The GNOME-sanctioned way (video players do it); `gnome-session-inhibit` shows it from the shell.
- Does nothing for a manual lock (Super+L, closing the lid) or for connecting to a machine that is
  already locked. Not a fix on its own; a complement to 1, or alone when only auto-lock is the
  problem.
- Con: the machine stays awake and unlocked as long as a device is connected, even one forgotten
  on the sofa. A device's session ending (it sleeps, the app closes) ends the inhibit.

## 3. Turn off locking

`gsettings set org.gnome.desktop.screensaver lock-enabled false` (and/or `idle-delay 0`).

- No code; a line in the README for users who want it.
- Weakens the whole machine, not just sill-linux: anyone at the desk gets in. Not something the
  host should set by itself.

## 4. A headless session through GDM (GNOME's Remote Login)

This is how gnome-remote-desktop's system mode (Settings → System → Remote Desktop → Remote Login)
works, and why it is not cut off by the lock: it never touches the locked session.

### How it works

1. A system daemon (`gnome-remote-desktop.service`, user `gnome-remote-desktop`) accepts the
   connection.
2. It asks GDM, through `org.gnome.DisplayManager.RemoteDisplayFactory.CreateRemoteDisplay`, for a
   new **headless** session: a GNOME Shell with no real monitor, showing GDM's login screen.
3. The client logs in on that screen. GDM starts the user's headless GNOME session.
4. The system daemon hands the client's connection over to a per-user daemon inside the new
   session (`org.gnome.RemoteDesktop.Rdp.Handover`, the socket passed as a file descriptor), which
   records a virtual monitor and injects input.

Headless sessions skip the inhibit, so their own lock screen never cuts the stream.

### What sill-linux would need

- A **system daemon** owning the device connection: installed as root, its own system user, a
  systemd unit, Avahi advertising from there.
- **D-Bus policy** to reach GDM's factory: on this machine `gdm.conf` allows
  `RemoteDisplayFactory` only to `root` and group `gdm`, and `org.gnome.RemoteDesktop.conf` adds
  user `gnome-remote-desktop`; everyone else is denied. sill-linux would ship its own policy file.
- **Pairing and TLS identity at system level**, and a way to map a
  paired device to a Linux user.
- A **login flow on the device**: GDM's login screen streamed and typed into, or credentials sent
  by the app (a protocol change in Sill's app, which is otherwise a fixed point).
- A **handover** from the system daemon to a user-level sill-linux inside the headless session.
- **`RecordVirtual`** instead of `RecordMonitor`, with a size chosen for the device, and the
  extension enabled in that session too.

### Cons

- **It is not the user's screen.** It is a second, separate desktop on a virtual monitor. Windows
  open on the physical (locked) session are not in it, so they cannot be streamed — and streaming
  those windows is what sill-linux is for. "Unlock the machine I left" is not what it does.
- **One user, one session.** As far as known, GDM does not log a user in remotely while the same
  user has a local session (gnome-remote-desktop's documentation tells them to log out first).
  Not checked against gdm3 50.1.
- **Size.** It is a second architecture beside the current single user process: system daemon,
  policy, handover, system-level pairing, login. Weeks, not an afternoon.
- **Packaging.** Root install, a system user, a D-Bus policy file. No GDM (another display manager,
  or none) means no feature at all.
- **Conflicts** with GNOME's own Remote Login when that is on: both use the same GDM factory, and
  GNOME's daemon may already hold the user's headless session.
- **Security surface.** A network-facing daemon running outside the user's session, in front of a
  login screen.

### When it would be right

For "use this Linux machine from the iPad with nobody at the desk", where a separate desktop is
fine, a headless session is the clean design. For "see and unlock the desktop I left locked", only
option 1 does it.

## Recommendation

Option 1 with its guards, plus option 2 if auto-lock under a quiet window stream turns out to
matter. Option 3 only as a note in the README; option 4 only if a separate remote desktop becomes a
goal of its own.

## References

- GNOME Shell: `js/ui/main.js` (`_sessionUpdated`), `js/ui/sessionMode.js`,
  `js/extensions/extension.js` (`InjectionManager`).
- Mutter: `src/backends/meta-remote-access-controller.c` (`meta_remote_access_controller_inhibit_remote_access`).
- [GNOME Shell extension review guidelines: session modes](https://gjs.guide/extensions/review-guidelines/review-guidelines.html)
- [gnome-remote-desktop](https://gitlab.gnome.org/GNOME/gnome-remote-desktop) (system mode,
  handover), GDM's `RemoteDisplayFactory`.
- [org.gnome.SessionManager.Inhibit](https://gitlab.gnome.org/GNOME/gnome-session/-/blob/main/gnome-session/org.gnome.SessionManager.xml)
