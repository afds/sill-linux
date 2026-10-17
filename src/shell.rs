//! The window list from the sill-linux GNOME Shell extension (`gnome-extension/`), on the
//! session bus as `org.sill_linux.Shell`: windows with Mutter's window ids (what `RecordWindow`
//! takes), thumbnails, and raise, close, minimize or fullscreen on a device's request.

use anyhow::{Context, Result};
use ashpd::zbus;

const NAME: &str = "org.sill_linux.Shell";
const PATH: &str = "/org/sill_linux/Shell";

/// One window, as the extension lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellWindow {
    pub id: u64,
    pub title: String,
    pub app_name: String,
    pub app_id: String,
    pub width: i32,
    pub height: i32,
}

/// A window's frame on the screen, logical pixels, and whether it was maximized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

/// A window's frame inside its buffer (`Geometry`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Insets {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub width: i32,
    pub height: i32,
}

impl Insets {
    /// The frame in the buffer's logical coordinates (what a window stream's absolute motion
    /// takes): x, y, width, height.
    pub fn frame_logical(&self) -> (f64, f64, f64, f64) {
        let (l, t) = (self.left.max(0) as f64, self.top.max(0) as f64);
        let w = (self.width - self.left.max(0) - self.right.max(0)).max(1) as f64;
        let h = (self.height - self.top.max(0) - self.bottom.max(0)).max(1) as f64;
        (l, t, w, h)
    }

    /// The frame's part of a crop of the buffer (`crop` in stream pixels), sizes even.
    pub fn frame_in(&self, (x, y, w, h): (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let kx = w as f64 / self.width as f64;
        let ky = h as f64 / self.height as f64;
        let l = (self.left.max(0) as f64 * kx).round() as u32;
        let t = (self.top.max(0) as f64 * ky).round() as u32;
        let r = (self.right.max(0) as f64 * kx).round() as u32;
        let b = (self.bottom.max(0) as f64 * ky).round() as u32;
        if l + r + 2 > w || t + b + 2 > h {
            return (x, y, w & !1, h & !1);
        }
        (x + l, y + t, (w - l - r) & !1, (h - t - b) & !1)
    }
}

/// A cursor sprite's hotspot and size in points: the sprite is drawn for the monitor's scale (a
/// size-24 cursor is 48 px at 200 %), so its scale is its size over the cursor size set in GNOME,
/// rounded, at least 1. The app draws it at that many of its own points, as a Mac's cursor.
/// shortcut: a theme whose images are not square at the nominal size reads slightly off.
pub fn cursor_points((hx, hy, w, h): (i32, i32, i32, i32), nominal: i32) -> (f64, f64, f64, f64) {
    let nominal = if nominal > 0 { nominal } else { 24 } as f64;
    let scale = (w.max(h) as f64 / nominal).round().max(1.0);
    (hx as f64 / scale, hy as f64 / scale, w as f64 / scale, h as f64 / scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_in_points() {
        assert_eq!(cursor_points((4, 4, 24, 24), 24), (4.0, 4.0, 24.0, 24.0));
        assert_eq!(cursor_points((8, 8, 48, 48), 24), (4.0, 4.0, 24.0, 24.0), "200 %");
        assert_eq!(cursor_points((8, 8, 48, 48), 48), (8.0, 8.0, 48.0, 48.0), "a larger cursor set");
        assert_eq!(cursor_points((11, 12, 24, 24), 0), (11.0, 12.0, 24.0, 24.0));
    }

    #[test]
    fn frame_inside_the_crop() {
        // Calculator in the nested test: 360×616 frame, 25 px of shadow, buffer 410×666.
        let i = Insets { left: 25, top: 25, right: 25, bottom: 25, width: 410, height: 666 };
        assert_eq!(i.frame_in((0, 0, 410, 666)), (25, 25, 360, 616));
        // At scale 2 the crop is twice the logical buffer.
        assert_eq!(i.frame_in((0, 0, 820, 1332)), (50, 50, 720, 1232));
        // No shadow (maximized).
        let m = Insets { left: 0, top: 0, right: 0, bottom: 0, width: 1600, height: 1000 };
        assert_eq!(m.frame_in((0, 0, 1600, 1000)), (0, 0, 1600, 1000));
        // Input: logical frame whatever the crop's scale; fraction 0 and 1 hit its corners.
        let (x, y, w, h) = i.frame_logical();
        assert_eq!((x, y, w, h), (25.0, 25.0, 360.0, 616.0));
        assert_eq!((x + 1.0 * w, y + 1.0 * h), (385.0, 641.0));
        // Insets bigger than the crop: the crop as it is.
        let odd = Insets { left: 300, top: 0, right: 300, bottom: 0, width: 410, height: 666 };
        assert_eq!(odd.frame_in((0, 0, 411, 666)), (0, 0, 410, 666));
    }
}

#[derive(Clone)]
pub struct Client {
    proxy: zbus::Proxy<'static>,
}

impl Client {
    /// The extension's service, if it runs (installed, enabled, and the session logged into
    /// again since it was installed).
    pub async fn connect(conn: &zbus::Connection) -> Option<Client> {
        let dbus = zbus::fdo::DBusProxy::new(conn).await.ok()?;
        let name = zbus::names::BusName::try_from(NAME).ok()?;
        if !dbus.name_has_owner(name).await.unwrap_or(false) {
            return None;
        }
        let proxy = zbus::Proxy::new(conn, NAME, PATH, NAME).await.ok()?;
        Some(Client { proxy })
    }

    /// The extension's interface version (None: one from before versions, 1).
    pub async fn version(&self) -> Option<u32> {
        self.proxy.call_method("Version", &()).await.ok()?.body().deserialize().ok()
    }

    /// The windows the device's bar lists: normal ones, not minimized (Sill for Mac lists only
    /// windows on screen).
    pub async fn list(&self) -> Result<Vec<ShellWindow>> {
        let reply = self.proxy.call_method("ListWindows", &()).await.context("ListWindows")?;
        let rows: Vec<(u64, String, String, String, i32, i32, bool, bool)> = reply.body().deserialize()?;
        Ok(rows
            .into_iter()
            .filter(|r| r.7 && !r.6)
            .map(|(id, title, app_name, app_id, width, height, _, _)| ShellWindow { id, title, app_name, app_id, width, height })
            .collect())
    }

    /// A PNG of the window, at most `max` on its longer side.
    pub async fn thumbnail(&self, id: u64, max: u32) -> Option<Vec<u8>> {
        let reply = self.proxy.call_method("Thumbnail", &(id, max)).await.ok()?;
        let path: String = reply.body().deserialize().ok()?;
        if path.is_empty() {
            return None;
        }
        let png = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        png
    }

    /// Where the frame sits in the window's buffer: left, top, right, bottom insets and the
    /// buffer's width and height, logical pixels (None: no such window, or an older extension).
    pub async fn geometry(&self, id: u64) -> Option<Insets> {
        let reply = self.proxy.call_method("Geometry", &(id,)).await.ok()?;
        let ((left, top, right, bottom, width, height),): ((i32, i32, i32, i32, i32, i32),) = reply.body().deserialize().ok()?;
        (width > 0 && height > 0).then_some(Insets { left, top, right, bottom, width, height })
    }

    /// Installed apps: (.desktop id, name), by name.
    pub async fn apps(&self) -> Result<Vec<(String, String)>> {
        let reply = self.proxy.call_method("ListApps", &()).await.context("ListApps")?;
        Ok(reply.body().deserialize()?)
    }

    /// A PNG of an app's icon, `size`×`size`.
    pub async fn app_icon(&self, app: &str, size: u32) -> Option<Vec<u8>> {
        let reply = self.proxy.call_method("AppIcon", &(app, size)).await.ok()?;
        let path: String = reply.body().deserialize().ok()?;
        if path.is_empty() {
            return None;
        }
        let png = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        png
    }

    pub async fn app_running(&self, app: &str) -> bool {
        self.flag("AppRunning", &(app,)).await
    }
    pub async fn activate_app(&self, app: &str) -> bool {
        self.flag("ActivateApp", &(app,)).await
    }
    pub async fn launch_app(&self, app: &str) -> bool {
        self.flag("LaunchApp", &(app,)).await
    }

    /// The window's frame, whether it is maximized and full screen; None: no such window.
    pub async fn frame_state(&self, id: u64) -> Option<(Frame, bool)> {
        let reply = self.proxy.call_method("FrameState", &(id,)).await.ok()?;
        let ((x, y, width, height, maximized, fullscreen, exists),): ((i32, i32, i32, i32, bool, bool, bool),) = reply.body().deserialize().ok()?;
        exists.then_some((Frame { x, y, width, height, maximized }, fullscreen))
    }
    pub async fn move_resize(&self, id: u64, x: i32, y: i32, w: i32, h: i32) -> bool {
        self.flag("MoveResize", &(id, x, y, w, h)).await
    }
    pub async fn set_maximized(&self, id: u64, on: bool) -> bool {
        self.flag("SetMaximized", &(id, on)).await
    }

    /// Activities' state: "hidden", "windows" or "apps".
    pub async fn overview_state(&self) -> Option<String> {
        self.proxy.call_method("OverviewState", &()).await.ok()?.body().deserialize().ok()
    }
    pub async fn set_overview(&self, state: &str) -> bool {
        self.flag("SetOverview", &(state,)).await
    }
    /// The workspace `delta` to the right; its index, None when there is none.
    pub async fn switch_workspace(&self, delta: i32) -> Option<i32> {
        let i: i32 = self.proxy.call_method("SwitchWorkspace", &(delta,)).await.ok()?.body().deserialize().ok()?;
        (i >= 0).then_some(i)
    }
    pub async fn minimize_workspace(&self) -> Vec<u64> {
        match self.proxy.call_method("MinimizeWorkspace", &()).await {
            Ok(r) => r.body().deserialize().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
    pub async fn unminimize(&self, ids: &[u64]) -> bool {
        self.flag("Unminimize", &(ids,)).await
    }

    async fn flag<B: serde::Serialize + zbus::zvariant::DynamicType>(&self, method: &str, body: &B) -> bool {
        match self.proxy.call_method(method, body).await {
            Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
            Err(_) => false,
        }
    }

    /// The pointer and, for a window id, that window's frame if it is on screen.
    pub async fn pointer_over(&self, id: u64) -> Option<((f64, f64), Option<(f64, f64, f64, f64)>)> {
        let reply = self.proxy.call_method("PointerOver", &(id,)).await.ok()?;
        let ((x, y, fx, fy, fw, fh, on),): ((i32, i32, i32, i32, i32, i32, bool),) = reply.body().deserialize().ok()?;
        Some(((x as f64, y as f64), on.then_some((fx as f64, fy as f64, fw as f64, fh as f64))))
    }

    pub async fn clipboard_types(&self) -> Option<Vec<String>> {
        self.proxy.call_method("ClipboardTypes", &()).await.ok()?.body().deserialize().ok()
    }

    pub async fn clipboard_text(&self) -> Option<String> {
        self.proxy.call_method("ClipboardText", &()).await.ok()?.body().deserialize().ok()
    }

    pub async fn set_clipboard(&self, text: &str) -> bool {
        match self.proxy.call_method("SetClipboard", &(text,)).await {
            Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Fires on every `ClipboardChanged`.
    pub async fn clipboard_changes(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.receive_signal("ClipboardChanged").await?)
    }

    /// The pointer's image: hotspot and size in logical pixels, and its PNG (Some(None): no image
    /// now; None: the extension has no such call, an older one).
    pub async fn cursor_shape(&self) -> Option<Option<((f64, f64, f64, f64), Vec<u8>)>> {
        let reply = self.proxy.call_method("CursorShape", &()).await.ok()?;
        let ((hx, hy, w, h, path, nominal),): ((i32, i32, i32, i32, String, i32),) = reply.body().deserialize().ok()?;
        if path.is_empty() {
            return Some(None);
        }
        let png = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        Some(png.map(|p| (cursor_points((hx, hy, w, h), nominal), p)))
    }

    /// Fires on every `CursorChanged`.
    pub async fn cursor_changes(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.receive_signal("CursorChanged").await?)
    }

    pub async fn command(&self, method: &str, id: u64) -> bool {
        match self.proxy.call_method(method, &(id,)).await {
            Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Fires on every `WindowsChanged` (coalesced by the extension to one per 250 ms).
    /// The pairing window (extension version 3): `modules` is the QR code's width×width modules,
    /// '1' dark, row by row.
    pub async fn show_pairing(&self, title: &str, code: &str, modules: &str, width: u32, seconds: u32) -> bool {
        self.flag("ShowPairing", &(title, code, modules, width, seconds)).await
    }
    pub async fn close_pairing(&self) {
        let _ = self.proxy.call_method("ClosePairing", &()).await;
    }
    pub async fn pairing_cancels(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.receive_signal("PairingCancelled").await?)
    }

    pub async fn changes(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.receive_signal("WindowsChanged").await?)
    }
}
