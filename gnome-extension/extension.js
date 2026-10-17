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
//   WindowsChanged                a window came, went, was retitled, resized, minimized or
//                                 unminimized (coalesced, 250 ms)
//   Thumbnail(t id, u maxSize) -> s   a PNG of the window, at most maxSize on its longer side,
//                                 in $XDG_RUNTIME_DIR/sill-linux/ (0700); "" when it has none
//   Activate(t id) -> b           unminimize, raise and focus
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
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

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
    <method name="Activate"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Close"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="Minimize"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
    <method name="ToggleFullscreen"><arg type="t" direction="in" name="id"/><arg type="b" direction="out" name="ok"/></method>
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
        this._signals.push(global.display.connect('window-created', (_d, w) => {
            this._watch(w);
            this.changed();
        }));
    }

    disable() {
        for (const id of this._signals)
            global.display.disconnect(id);
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
