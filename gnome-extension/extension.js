// Sill for Linux: the window list for the sill-linux host, on the session bus.
//
// org.sill_linux.Shell at /org/sill_linux/Shell:
//   ListWindows() -> a(tsssii)   normal windows: Mutter's window id (what
//                                 org.gnome.Mutter.ScreenCast RecordWindow takes), title,
//                                 app name, app id (.desktop id, or WM class), width, height
//                                 (frame, logical pixels), in stacking order, bottom first;
//                                 minimized windows are left out, as Sill for Mac lists only
//                                 windows on screen
//   Geometry(t id) -> (iiiiii)    where the frame sits in the window's buffer (what a window's
//                                 screen cast shows): left, top, right and bottom insets (the
//                                 shadow), and the buffer's width and height, logical pixels;
//                                 all 0 for no such window
//   ClipboardTypes() -> as        the clipboard's MIME types (read without its contents)
//   ClipboardText() -> s          its text ("" for none)
//   SetClipboard(s text) -> b     the clipboard's text, from a device
//   ClipboardChanged              the clipboard has new contents
//   CursorShape() -> (dddds)      the pointer's current image: hotspot x, y and size w, h in
//                                 logical pixels, and a PNG of it (path; "" when there is none)
//   CursorChanged                 the pointer's image changed
//   Gesture(s name) -> s          a trackpad gesture from a device, as GNOME's own: swipeUp
//                                 Activities then the app grid, swipeDown back, swipeLeft and
//                                 swipeRight the next and previous workspace, pinch the app grid,
//                                 spread the desktop shown (pinch or spread again: the windows
//                                 back); answers what it did, "" for nothing
//   WindowsChanged                a window came, went, was retitled, resized, minimized or
//                                 unminimized (coalesced, 250 ms)
//   Thumbnail(t id, u maxSize) -> s   a PNG of the window, at most maxSize on its longer side,
//                                 in $XDG_RUNTIME_DIR/sill-linux/ (0700); "" when it has none
//   Activate(t id) -> b           unminimize, raise and focus
//   ListApps() -> a(ss)           installed apps shown in the app grid: .desktop id, name
//   AppIcon(s app, u size) -> s   a PNG of the app's icon, size×size, beside the thumbnails; ""
//                                 when it has none
//   Launch(s app) -> b            start the app (a running one is raised)
//   Fit(t id, i w, i h) -> (iiiib)  resize the window's frame to w×h logical pixels where it is
//                                 (unmaximized first); answers its frame before (x, y, w, h) and
//                                 whether it was maximized; w 0: not touched (full screen, or gone)
//   Restore(t id, i x, i y, i w, i h, b maximized) -> b   put a fitted window back
//   PointerOver(t id) -> (iiiiiib) the pointer (x, y, logical pixels) and that window's frame
//                                 (x, y, w, h), and whether the window is on screen (false for id
//                                 0, a minimized window or none)
//   Close(t id) -> b, Minimize(t id) -> b, ToggleFullscreen(t id) -> b
//
// Mutter's own screen-cast API already lets any unsandboxed process of this user record a window;
// this adds titles and a window list, which GNOME keeps from ordinary apps. Sandboxed (Flatpak)
// apps cannot reach this name unless allowed to.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Mtk from 'gi://Mtk';
import Shell from 'gi://Shell';
import Cairo from 'cairo';
import GdkPixbuf from 'gi://GdkPixbuf';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const BUS_NAME = 'org.sill_linux.Shell';
const PATH = '/org/sill_linux/Shell';
const XML = `
<node>
  <interface name="org.sill_linux.Shell">
    <method name="ListWindows"><arg type="a(tsssii)" direction="out" name="windows"/></method>
    <method name="Thumbnail">
      <arg type="t" direction="in" name="id"/><arg type="u" direction="in" name="maxSize"/>
      <arg type="s" direction="out" name="path"/>
    </method>
    <method name="Geometry">
      <arg type="t" direction="in" name="id"/><arg type="(iiiiii)" direction="out" name="geometry"/>
    </method>
    <method name="ListApps"><arg type="a(ss)" direction="out" name="apps"/></method>
    <method name="AppIcon">
      <arg type="s" direction="in" name="app"/><arg type="u" direction="in" name="size"/>
      <arg type="s" direction="out" name="path"/>
    </method>
    <method name="Launch"><arg type="s" direction="in" name="app"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Fit">
      <arg type="t" direction="in" name="id"/><arg type="i" direction="in" name="width"/>
      <arg type="i" direction="in" name="height"/><arg type="(iiiib)" direction="out" name="before"/>
    </method>
    <method name="Restore">
      <arg type="t" direction="in" name="id"/><arg type="i" direction="in" name="x"/>
      <arg type="i" direction="in" name="y"/><arg type="i" direction="in" name="width"/>
      <arg type="i" direction="in" name="height"/><arg type="b" direction="in" name="maximized"/>
      <arg type="b" direction="out" name="ok"/>
    </method>
    <method name="PointerOver">
      <arg type="t" direction="in" name="id"/><arg type="(iiiiiib)" direction="out" name="pointer"/>
    </method>
    <method name="Activate"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Close"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Minimize"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="ToggleFullscreen"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="ClipboardTypes"><arg type="as" direction="out" name="types"/></method>
    <method name="ClipboardText"><arg type="s" direction="out" name="text"/></method>
    <method name="SetClipboard"><arg type="s" direction="in" name="text"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Gesture"><arg type="s" direction="in" name="name"/><arg type="s" direction="out" name="did"/></method>
    <method name="CursorShape"><arg type="(dddds)" direction="out" name="shape"/></method>
    <signal name="CursorChanged"/>
    <signal name="ClipboardChanged"/>
    <signal name="WindowsChanged"/>
  </interface>
</node>`;

function listed(w) {
    return w.get_window_type() === Meta.WindowType.NORMAL && !w.is_skip_taskbar() && !w.minimized;
}

function windows() {
    const all = global.display.list_all_windows().filter(listed);
    return global.display.sort_windows_by_stacking(all);
}

function byId(id) {
    return global.display.list_all_windows().find(w => w.get_id() === id) ?? null;
}

class Service {
    constructor() {
        this._tracker = Shell.WindowTracker.get_default();
        this._dir = GLib.build_filenamev([GLib.get_user_runtime_dir(), 'sill-linux']);
        GLib.mkdir_with_parents(this._dir, 0o700);
        this._pending = 0;
        this._signals = [];
        this._windowSignals = new Map();
    }

    ListWindows() {
        return windows().map(w => {
            const app = this._tracker.get_window_app(w);
            const r = w.get_frame_rect();
            return [
                w.get_id(),
                w.get_title() ?? '',
                app?.get_name() ?? w.get_wm_class() ?? '',
                app?.get_id() ?? w.get_wm_class() ?? '',
                r.width,
                r.height,
            ];
        });
    }

    Geometry(id) {
        const w = byId(id);
        if (!w)
            return [0, 0, 0, 0, 0, 0];
        const f = w.get_frame_rect(), b = w.get_buffer_rect();
        return [f.x - b.x, f.y - b.y, b.x + b.width - (f.x + f.width), b.y + b.height - (f.y + f.height), b.width, b.height];
    }

    Thumbnail(id, maxSize) {
        const w = byId(id);
        const actor = w?.get_compositor_private();
        if (!actor)
            return '';
        try {
            // The frame alone, without the shadow around it.
            const f = w.get_frame_rect(), b = w.get_buffer_rect();
            const clip = new Mtk.Rectangle({x: f.x - b.x, y: f.y - b.y, width: f.width, height: f.height});
            const src = actor.get_image(clip) ?? actor.get_image(null);
            if (!src)
                return '';
            const sw = src.getWidth(), sh = src.getHeight();
            const s = Math.min(1, maxSize / Math.max(sw, sh, 1));
            const dw = Math.max(1, Math.round(sw * s)), dh = Math.max(1, Math.round(sh * s));
            const dst = new Cairo.ImageSurface(Cairo.Format.ARGB32, dw, dh);
            const cr = new Cairo.Context(dst);
            cr.scale(dw / sw, dh / sh);
            cr.setSourceSurface(src, 0, 0);
            cr.paint();
            cr.$dispose();
            const path = GLib.build_filenamev([this._dir, `thumb-${id}.png`]);
            dst.writeToPNG(path);
            return path;
        } catch (e) {
            logError(e, 'sill-linux: thumbnail');
            return '';
        }
    }

    ListApps() {
        return Shell.AppSystem.get_default().get_installed()
            .filter(a => a.should_show())
            .map(a => [a.get_id() ?? '', a.get_name() ?? ''])
            .filter(([id, name]) => id && name)
            .sort((a, b) => a[1].localeCompare(b[1]));
    }

    AppIcon(appId, size) {
        try {
            const app = Shell.AppSystem.get_default().lookup_app(appId);
            const icon = app?.get_app_info()?.get_icon();
            if (!icon)
                return '';
            const info = new St.IconTheme().lookup_by_gicon(icon, size, St.IconLookupFlags.FORCE_SIZE);
            const file = info?.get_filename();
            if (!file)
                return '';
            const pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_size(file, size, size);
            const path = GLib.build_filenamev([this._dir, `icon-${GLib.compute_checksum_for_string(GLib.ChecksumType.MD5, appId, -1)}.png`]);
            pixbuf.savev(path, 'png', [], []);
            return path;
        } catch (e) {
            logError(e, 'sill-linux: app icon');
            return '';
        }
    }

    Launch(appId) {
        const app = Shell.AppSystem.get_default().lookup_app(appId);
        if (!app)
            return false;
        if (app.get_n_windows() > 0)
            app.activate();
        else
            app.launch(0, -1, Shell.AppLaunchGpu.APP_PREF);
        return true;
    }

    Fit(id, width, height) {
        const w = byId(id);
        if (!w || w.is_fullscreen() || width < 1 || height < 1)
            return [0, 0, 0, 0, false];
        const f = w.get_frame_rect();
        const maximized = w.is_maximized();
        if (maximized)
            w.unmaximize();
        w.move_resize_frame(false, f.x, f.y, width, height);
        return [f.x, f.y, f.width, f.height, maximized];
    }

    Restore(id, x, y, width, height, maximized) {
        const w = byId(id);
        if (!w || w.is_fullscreen())
            return false;
        w.move_resize_frame(false, x, y, width, height);
        if (maximized)
            w.maximize();
        return true;
    }

    PointerOver(id) {
        const [x, y] = global.get_pointer();
        const w = id ? byId(id) : null;
        if (!w || w.minimized)
            return [x, y, 0, 0, 0, 0, false];
        const f = w.get_frame_rect();
        return [x, y, f.x, f.y, f.width, f.height, true];
    }

    Gesture(name) {
        const overview = Main.overview;
        const apps = overview.visible && overview.dash?.showAppsButton?.checked;
        const time = global.get_current_time();
        const workspace = dir => {
            const ws = global.workspace_manager.get_active_workspace().get_neighbor(dir);
            if (ws === global.workspace_manager.get_active_workspace())
                return '';
            ws.activate(time);
            return `workspace ${ws.index() + 1}`;
        };
        if (this._shownDesktop && (name === 'pinch' || name === 'spread')) {
            for (const w of this._shownDesktop)
                w.unminimize();
            this._shownDesktop = null;
            return 'the windows back';
        }
        switch (name) {
        case 'swipeUp':
            if (apps)
                return '';
            if (overview.visible) {
                // showApps() does not leave the window picker once Activities is open (GNOME 50);
                // the Show Apps button, which the overview follows, does.
                overview.dash.showAppsButton.checked = true;
                return 'the app grid';
            }
            overview.show();
            return 'Activities';
        case 'swipeDown':
            if (apps) {
                overview.dash.showAppsButton.checked = false;
                return 'Activities';
            }
            if (overview.visible) {
                overview.hide();
                return 'Activities closed';
            }
            return '';
        case 'swipeLeft':
            return workspace(Meta.MotionDirection.RIGHT);
        case 'swipeRight':
            return workspace(Meta.MotionDirection.LEFT);
        case 'pinch':
            if (apps) {
                overview.hide();
                return 'the app grid closed';
            }
            if (overview.visible)
                overview.dash.showAppsButton.checked = true;
            else
                overview.showApps();
            return 'the app grid';
        case 'spread': {
            if (overview.visible) {
                overview.hide();
                return 'Activities closed';
            }
            const active = global.workspace_manager.get_active_workspace();
            const shown = windows().filter(w => w.located_on_workspace(active) && w.can_minimize());
            if (!shown.length)
                return '';
            for (const w of shown)
                w.minimize();
            this._shownDesktop = shown;
            return 'the desktop';
        }
        }
        return '';
    }

    CursorShapeAsync(_params, invocation) {
        const none = () => invocation.return_value(new GLib.Variant('((dddds))', [[0, 0, 0, 0, '']]));
        try {
            const tracker = global.backend.get_cursor_tracker();
            const sprite = tracker.get_sprite();
            if (!sprite)
                return none();
            const w = sprite.get_width(), h = sprite.get_height();
            const [hx, hy] = tracker.get_hot();
            // The sprite is drawn for the monitor's scale (a size-24 cursor is 48 px at 200 %); its
            // size in points is the cursor size the person set, as a Mac's cursor is in points.
            // shortcut: the scale as the sprite's width over that size, rounded; a theme whose images
            // are not square at the nominal size would read slightly off.
            const nominal = new Gio.Settings({schema_id: 'org.gnome.desktop.interface'}).get_int('cursor-size') || 24;
            const scale = Math.max(1, Math.round(Math.max(w, h) / nominal));
            const path = GLib.build_filenamev([this._dir, 'cursor.png']);
            const stream = Gio.File.new_for_path(path).replace(null, false, Gio.FileCreateFlags.NONE, null);
            // The sprite's pixels as GNOME's screenshots write them (Cogl's get_data gives JS nothing).
            Shell.Screenshot.composite_to_stream(sprite, 0, 0, w, h, 1, null, 0, 0, 1, stream, (_o, res) => {
                try {
                    Shell.Screenshot.composite_to_stream_finish(res);
                    stream.close(null);
                    invocation.return_value(new GLib.Variant('((dddds))', [[hx / scale, hy / scale, w / scale, h / scale, path]]));
                } catch (e) {
                    logError(e, 'sill-linux: cursor shape');
                    none();
                }
            });
        } catch (e) {
            logError(e, 'sill-linux: cursor shape');
            none();
        }
    }

    ClipboardTypes() {
        return St.Clipboard.get_default().get_mimetypes(St.ClipboardType.CLIPBOARD);
    }

    ClipboardTextAsync(_params, invocation) {
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_c, text) => {
            invocation.return_value(new GLib.Variant('(s)', [text ?? '']));
        });
    }

    SetClipboard(text) {
        St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
        return true;
    }

    Activate(id) {
        const w = byId(id);
        if (!w)
            return false;
        if (w.minimized)
            w.unminimize();
        w.activate(global.get_current_time());
        return true;
    }

    Close(id) {
        const w = byId(id);
        if (!w)
            return false;
        w.delete(global.get_current_time());
        return true;
    }

    Minimize(id) {
        const w = byId(id);
        if (!w || !w.can_minimize())
            return false;
        w.minimize();
        return true;
    }

    ToggleFullscreen(id) {
        const w = byId(id);
        if (!w)
            return false;
        if (w.is_fullscreen())
            w.unmake_fullscreen();
        else
            w.make_fullscreen();
        return true;
    }

    // WindowsChanged, at most every 250 ms.
    changed() {
        if (this._pending)
            return;
        this._pending = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 250, () => {
            this._pending = 0;
            this._dbus?.emit_signal('WindowsChanged', null);
            return GLib.SOURCE_REMOVE;
        });
    }

    _watch(w) {
        if (this._windowSignals.has(w))
            return;
        this._windowSignals.set(w, [
            w.connect('notify::title', () => this.changed()),
            w.connect('size-changed', () => this.changed()),
            w.connect('notify::minimized', () => this.changed()),
            w.connect('unmanaged', () => {
                this._unwatch(w);
                this.changed();
            }),
        ]);
    }

    _unwatch(w) {
        const ids = this._windowSignals.get(w);
        if (ids) {
            for (const i of ids)
                w.disconnect(i);
            this._windowSignals.delete(w);
        }
    }

    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._dbus.export(Gio.DBus.session, PATH);
        this._name = Gio.bus_own_name_on_connection(Gio.DBus.session, BUS_NAME, Gio.BusNameOwnerFlags.NONE, null, null);
        for (const w of global.display.list_all_windows())
            this._watch(w);
        this._cursorTracker = global.backend.get_cursor_tracker();
        this._cursorSignal = this._cursorTracker.connect('cursor-changed', () => this._dbus?.emit_signal('CursorChanged', null));
        const selection = global.display.get_selection();
        this._selection = selection;
        this._selectionSignal = selection.connect('owner-changed', (_s, type) => {
            if (type === Meta.SelectionType.SELECTION_CLIPBOARD)
                this._dbus?.emit_signal('ClipboardChanged', null);
        });
        this._signals.push(global.display.connect('window-created', (_d, w) => {
            this._watch(w);
            this.changed();
        }));
    }

    disable() {
        for (const id of this._signals)
            global.display.disconnect(id);
        if (this._cursorSignal) {
            this._cursorTracker.disconnect(this._cursorSignal);
            this._cursorSignal = 0;
        }
        if (this._selectionSignal) {
            this._selection.disconnect(this._selectionSignal);
            this._selectionSignal = 0;
        }
        this._signals = [];
        for (const w of [...this._windowSignals.keys()])
            this._unwatch(w);
        if (this._pending) {
            GLib.source_remove(this._pending);
            this._pending = 0;
        }
        if (this._name) {
            Gio.bus_unown_name(this._name);
            this._name = 0;
        }
        this._dbus?.unexport();
        this._dbus = null;
    }
}

export default class SillLinuxExtension extends Extension {
    enable() {
        this._service = new Service();
        this._service.enable();
    }

    disable() {
        this._service?.disable();
        this._service = null;
    }
}
