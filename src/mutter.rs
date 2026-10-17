//! GNOME's own screen-cast and remote-desktop D-Bus API (`org.gnome.Mutter.ScreenCast`,
//! `org.gnome.Mutter.RemoteDesktop`), as gnome-remote-desktop uses it: one remote-desktop session
//! with a screen-cast session linked to it. `RecordMonitor` gives the Desktop and `RecordWindow`
//! one window (captured itself, even when covered); input goes to the same session, with absolute
//! positions in a stream's own coordinates.
//!
//! Over the portal: no consent dialog (GNOME's sharing indicator still shows), single windows, and
//! no 1920×1080 box on absolute positions at scale 2. GNOME only; the portal is the fallback
//! (`--portal`). The sessions live as long as this process's D-Bus connection.

use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus::{self, zvariant::{OwnedObjectPath, OwnedValue, Value}};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const SC: &str = "org.gnome.Mutter.ScreenCast";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";

/// Mutter's axis flags.
pub const AXIS_FINISH: u32 = 1 << 0;
pub const AXIS_SOURCE_FINGER: u32 = 1 << 2;

/// Whether Mutter's API is here (a GNOME session).
pub async fn available(conn: &zbus::Connection) -> bool {
    let dbus = zbus::fdo::DBusProxy::new(conn).await;
    match dbus {
        Ok(p) => {
            let name = |n: &'static str| zbus::names::BusName::try_from(n).unwrap();
            p.name_has_owner(name(SC).try_into().unwrap()).await.unwrap_or(false)
                && p.name_has_owner(name(RD).try_into().unwrap()).await.unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// The connector of the monitor to record: `wanted` if given, else the primary one.
pub async fn monitor_connector(conn: &zbus::Connection, wanted: Option<&str>) -> Result<String> {
    type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
    type Monitor = ((String, String, String, String), Vec<Mode>, HashMap<String, OwnedValue>);
    type Logical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, HashMap<String, OwnedValue>);
    let reply = conn
        .call_method(Some("org.gnome.Mutter.DisplayConfig"), "/org/gnome/Mutter/DisplayConfig", Some("org.gnome.Mutter.DisplayConfig"), "GetCurrentState", &())
        .await
        .context("DisplayConfig.GetCurrentState")?;
    let (_serial, monitors, logical, _props): (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>) =
        reply.body().deserialize().context("reading the monitor layout")?;
    let connectors: Vec<String> = monitors.iter().map(|m| m.0 .0.clone()).collect();
    if let Some(w) = wanted {
        return connectors.iter().find(|c| *c == w).cloned().ok_or_else(|| anyhow!("no monitor {w} (there are: {})", connectors.join(", ")));
    }
    logical
        .iter()
        .find(|l| l.4)
        .and_then(|l| l.5.first().map(|m| m.0.clone()))
        .or_else(|| connectors.first().cloned())
        .ok_or_else(|| anyhow!("no monitor"))
}

/// One linked remote-desktop and screen-cast session.
pub struct Session {
    pub conn: zbus::Connection,
    pub rd: OwnedObjectPath,
    pub sc: OwnedObjectPath,
    started: std::sync::atomic::AtomicBool,
}

/// A recorded source: its stream object (absolute motion names it), PipeWire node and size in
/// compositor (logical) coordinates.
#[derive(Debug, Clone)]
pub struct Stream {
    pub path: OwnedObjectPath,
    pub node: u32,
    pub size: Option<(i32, i32)>,
}

impl Session {
    pub async fn new(conn: zbus::Connection) -> Result<Session> {
        let rd: OwnedObjectPath = conn
            .call_method(Some(RD), "/org/gnome/Mutter/RemoteDesktop", Some(RD), "CreateSession", &())
            .await
            .context("RemoteDesktop.CreateSession")?
            .body()
            .deserialize()?;
        let rd_proxy = zbus::Proxy::new(&conn, RD, rd.clone(), RD_SESSION).await?;
        let id: String = rd_proxy.get_property("SessionId").await.context("RemoteDesktop SessionId")?;
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("remote-desktop-session-id", Value::from(id));
        let sc: OwnedObjectPath = conn
            .call_method(Some(SC), "/org/gnome/Mutter/ScreenCast", Some(SC), "CreateSession", &(props,))
            .await
            .context("ScreenCast.CreateSession")?
            .body()
            .deserialize()?;
        Ok(Session { conn, rd, sc, started: std::sync::atomic::AtomicBool::new(false) })
    }

    /// Records the monitor or a window and waits for its PipeWire node. The first one starts the
    /// session.
    pub async fn record(&self, source: Source<'_>, cursor: u32) -> Result<Stream> {
        // Listened for before the call: on a session already started, Mutter announces the new
        // stream's node while it answers the call, before its object could be named.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SC_STREAM)?
            .member("PipeWireStreamAdded")?
            .build();
        let mut added = zbus::MessageStream::for_match_rule(rule, &self.conn, Some(16)).await?;
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("cursor-mode", Value::from(cursor));
        let reply = match source {
            Source::Monitor(connector) => {
                self.conn.call_method(Some(SC), self.sc.as_ref(), Some(SC_SESSION), "RecordMonitor", &(connector, props)).await
            }
            Source::Window(id) => {
                props.insert("window-id", Value::from(id));
                self.conn.call_method(Some(SC), self.sc.as_ref(), Some(SC_SESSION), "RecordWindow", &(props,)).await
            }
        }
        .context("ScreenCast record")?;
        let path: OwnedObjectPath = reply.body().deserialize()?;
        if !self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.conn
                .call_method(Some(RD), self.rd.as_ref(), Some(RD_SESSION), "Start", &())
                .await
                .context("RemoteDesktop.Start")?;
        }
        let node = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(msg) = added.next().await {
                let msg = msg?;
                if msg.header().path().map(|p| p.as_str() == path.as_str()).unwrap_or(false) {
                    let (node,): (u32,) = msg.body().deserialize()?;
                    return Ok::<u32, anyhow::Error>(node);
                }
            }
            bail!("the stream ended")
        })
        .await
        .map_err(|_| anyhow!("no PipeWire stream within 5 s"))??;
        let stream = zbus::Proxy::new(&self.conn, SC, path.clone(), SC_STREAM).await?;
        let params: HashMap<String, OwnedValue> = stream.get_property("Parameters").await.unwrap_or_default();
        let size = params.get("size").and_then(|v| <(i32, i32)>::try_from(v.clone()).ok());
        Ok(Stream { path, node, size })
    }

    /// Stops one recorded stream (a window that is no longer streamed).
    pub async fn stop_stream(&self, stream: &Stream) {
        let _ = self.conn.call_method(Some(SC), stream.path.as_ref(), Some(SC_STREAM), "Stop", &()).await;
    }

    async fn rd_call<B: serde::Serialize + zbus::zvariant::DynamicType>(&self, method: &str, body: &B) -> Result<()> {
        self.conn.call_method(Some(RD), self.rd.as_ref(), Some(RD_SESSION), method, body).await?;
        Ok(())
    }

    pub async fn motion_absolute(&self, stream: &Stream, x: f64, y: f64) -> Result<()> {
        self.rd_call("NotifyPointerMotionAbsolute", &(stream.path.as_str(), x, y)).await
    }
    pub async fn motion_relative(&self, dx: f64, dy: f64) -> Result<()> {
        self.rd_call("NotifyPointerMotionRelative", &(dx, dy)).await
    }
    pub async fn button(&self, button: i32, pressed: bool) -> Result<()> {
        self.rd_call("NotifyPointerButton", &(button, pressed)).await
    }
    pub async fn axis(&self, dx: f64, dy: f64, flags: u32) -> Result<()> {
        self.rd_call("NotifyPointerAxis", &(dx, dy, flags)).await
    }
    pub async fn keycode(&self, code: u32, pressed: bool) -> Result<()> {
        self.rd_call("NotifyKeyboardKeycode", &(code, pressed)).await
    }
    pub async fn keysym(&self, sym: u32, pressed: bool) -> Result<()> {
        self.rd_call("NotifyKeyboardKeysym", &(sym, pressed)).await
    }

    pub async fn stop(&self) {
        let _ = self.rd_call("Stop", &()).await;
    }
}

pub enum Source<'a> {
    Monitor(&'a str),
    Window(u64),
}

/// For `bail!` users that want a plain error.
pub fn unavailable() -> Result<()> {
    bail!("GNOME's screen-cast API (org.gnome.Mutter.ScreenCast) is not here")
}
