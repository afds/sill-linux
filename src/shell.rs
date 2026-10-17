//! The window list from the Sill for Linux GNOME Shell extension (`gnome-extension/`), on the
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

#[cfg(test)]
mod tests {
    use super::*;

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
        if !dbus.name_has_owner(name.try_into().ok()?).await.unwrap_or(false) {
            return None;
        }
        let proxy = zbus::Proxy::new(conn, NAME, PATH, NAME).await.ok()?;
        Some(Client { proxy })
    }

    pub async fn list(&self) -> Result<Vec<ShellWindow>> {
        let reply = self.proxy.call_method("ListWindows", &()).await.context("ListWindows")?;
        let rows: Vec<(u64, String, String, String, i32, i32)> = reply.body().deserialize()?;
        Ok(rows
            .into_iter()
            .map(|(id, title, app_name, app_id, width, height)| ShellWindow { id, title, app_name, app_id, width, height })
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

    pub async fn launch(&self, app: &str) -> bool {
        match self.proxy.call_method("Launch", &(app,)).await {
            Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Resizes the window's frame to `w`×`h` logical pixels; its frame before and whether it was
    /// maximized (None: not touched).
    pub async fn fit(&self, id: u64, w: i32, h: i32) -> Option<Frame> {
        let reply = self.proxy.call_method("Fit", &(id, w, h)).await.ok()?;
        let ((x, y, width, height, maximized),): ((i32, i32, i32, i32, bool),) = reply.body().deserialize().ok()?;
        (width > 0).then_some(Frame { x, y, width, height, maximized })
    }

    pub async fn restore(&self, id: u64, f: Frame) -> bool {
        match self.proxy.call_method("Restore", &(id, f.x, f.y, f.width, f.height, f.maximized)).await {
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

    pub async fn command(&self, method: &str, id: u64) -> bool {
        match self.proxy.call_method(method, &(id,)).await {
            Ok(r) => r.body().deserialize::<bool>().unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Fires on every `WindowsChanged` (coalesced by the extension to one per 250 ms).
    pub async fn changes(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.receive_signal("WindowsChanged").await?)
    }
}
