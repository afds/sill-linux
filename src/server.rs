//! The door and the sessions behind it.
//!
//! The door is Sill's home door in one of three forms (`Door`): TLS with pairing required (TXT
//! `p=1`, what the App Store build pairs with), TLS open to any device (`p=0`), or plain TCP (no
//! `p`: a Debug build of the app only). Connections run on a tokio runtime of their own (a reader
//! task and a writer task each); the ticker is a thread, and the capture side calls
//! `broadcast_access_unit` from its own. One lock guards the shared state; nothing waits under it.
//!
//! Rules carried over from Sill's `StreamServer` (the home branch of `broadcast`):
//! - a client that joins waits for a keyframe, and gets the parameter sets just before it;
//! - a client with more than two frames unsent loses the next delta and waits for a keyframe
//!   (latency beats quality), and the encoder is asked for one;
//! - an empty tick every 30 ms while streaming keeps the device's Wi-Fi out of power save;
//! - a client that drains no frame for 4 s is closed (not in its first 8 s).

use crate::door::{PairingDoor, Admission};
use crate::input::InputEvent;
use crate::protocol::{self, kind, ClientStats, Hello, StreamSource, Viewport, WindowList};
use crate::settings::{self, HostSettingsChange, RunningStream, Settings};
use std::path::PathBuf;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

const TICK_INTERVAL: Duration = Duration::from_millis(30);
const EVICT_AFTER: Duration = Duration::from_secs(4);
const EVICT_GRACE: Duration = Duration::from_secs(8);
const MAX_FRAMES_IN_FLIGHT: usize = 2;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the server asks of the capture side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Control {
    /// Start encoding the Desktop (a device picked it).
    Start,
    /// Nobody watches any more: stop encoding.
    Stop,
    /// A client needs a keyframe (it joined, or it lost a delta).
    Keyframe,
    /// What the encoder should run at now.
    Apply(Apply),
}

/// The encoder's settings, from the saved settings and the devices' rates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Apply {
    pub bitrate: i64,
    pub fps: i64,
    pub retina: bool,
}

impl Apply {
    pub fn kbps(&self) -> u32 {
        (self.bitrate / 1000) as u32
    }
}

/// What the server hands the input side: one device's event, or that it left (its keys and
/// buttons go up).
#[derive(Debug)]
pub enum InputCmd {
    Event(u64, InputEvent),
    Left(u64),
}

/// How the door speaks.
#[derive(Clone)]
pub enum Door {
    /// Plain TCP, no `p` in the TXT record: only a Debug build of the app dials it.
    Plain,
    /// TLS 1.3 with pinned keys; pairing required or open (`PairingDoor::require_pairing`).
    Tls(Arc<PairingDoor>),
}

struct Client {
    label: String,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    close: watch::Sender<bool>,
    frames_in_flight: Arc<AtomicUsize>,
    last_drain: Arc<Mutex<Instant>>,
    connected_at: Instant,
    needs_keyframe: bool,
    /// The rate its viewport asks for (nil: 60).
    fps: Option<i64>,
}

struct State {
    /// Frames sent and withheld since the last stats line, summed over clients.
    sent: u64,
    dropped: u64,
    clients: HashMap<u64, Client>,
    next_id: u64,
    active: StreamSource,
    settings: Settings,
    settings_path: Option<PathBuf>,
    software_encoder: bool,
    /// The encoded picture's size, for the panel's readout.
    encoded: Option<(i64, i64)>,
}

#[derive(Clone)]
pub struct Server {
    state: Arc<Mutex<State>>,
    control: Sender<Control>,
    host_name: Arc<String>,
    launch_id: Arc<String>,
    door: Door,
    /// The input side once the portal allows it; None: input is ignored (view only).
    input: Arc<Mutex<Option<mpsc::UnboundedSender<InputCmd>>>>,
}

impl Server {
    /// `settings_path`: where devices' changes are saved (None: kept until this host quits).
    pub fn new(host_name: String, control: Sender<Control>, door: Door, settings_path: Option<PathBuf>) -> Server {
        let settings = settings_path.as_deref().map(Settings::load).unwrap_or_default();
        Server {
            state: Arc::new(Mutex::new(State {
                sent: 0,
                dropped: 0,
                clients: HashMap::new(),
                next_id: 1,
                active: StreamSource::None,
                settings,
                settings_path,
                software_encoder: false,
                encoded: None,
            })),
            control,
            host_name: Arc::new(host_name),
            launch_id: Arc::new(uuid::Uuid::new_v4().to_string()),
            door,
            input: Arc::new(Mutex::new(None)),
        }
    }

    /// What the encoder should run at, from the settings and the connected devices.
    pub fn current_apply(&self) -> Apply {
        Self::apply_of(&self.state.lock().unwrap())
    }

    fn apply_of(st: &State) -> Apply {
        let fastest = st.clients.values().filter_map(|c| c.fps).max();
        let fps = st.settings.fps(fastest.or(if st.clients.is_empty() { None } else { Some(60) }));
        Apply { bitrate: st.settings.bitrate_at(fps), fps, retina: st.settings.retina() }
    }

    pub fn set_software_encoder(&self, software: bool) {
        self.state.lock().unwrap().software_encoder = software;
    }

    /// From the capture side with each frame: a new size is told to the devices (the panel's
    /// readout).
    pub fn set_encoded_size(&self, width: i64, height: i64) {
        let mut st = self.state.lock().unwrap();
        if st.encoded != Some((width, height)) {
            st.encoded = Some((width, height));
            self.broadcast_settings(&st, None);
        }
    }

    fn settings_message(&self, st: &State, answering: Option<i64>) -> Vec<u8> {
        let apply = Self::apply_of(st);
        let stream = match (st.active, st.encoded) {
            (StreamSource::None, _) | (_, None) => None,
            (_, Some((w, h))) => Some(RunningStream {
                width: w,
                height: h,
                fps: apply.fps,
                mbps: (apply.bitrate as f64 / 1e6).round() as i64,
                on_virtual_display: false,
            }),
        };
        let state = settings::state(&st.settings, st.software_encoder, stream, answering);
        protocol::message(kind::HOST_SETTINGS, protocol::now(), false, &serde_json::to_vec(&state).unwrap())
    }

    /// Kind 16 to every device; with `answering`, that device's token goes to it alone.
    fn broadcast_settings(&self, st: &State, answering: Option<(u64, i64)>) {
        let plain = self.settings_message(st, None);
        for (id, c) in &st.clients {
            match answering {
                Some((aid, token)) if aid == *id => {
                    let _ = c.tx.send(self.settings_message(st, Some(token)));
                }
                _ => {
                    let _ = c.tx.send(plain.clone());
                }
            }
        }
    }

    /// Re-derives the encoder's settings after the devices or the settings changed.
    fn reapply(&self, st: &State) {
        let _ = self.control.send(Control::Apply(Self::apply_of(st)));
    }

    fn change_settings(&self, id: u64, change: HostSettingsChange) {
        let mut st = self.state.lock().unwrap();
        let label = st.clients.get(&id).map(|c| c.label.clone()).unwrap_or_default();
        let (next, refused) = settings::accept(st.settings, &change);
        if next != st.settings {
            println!(
                "Settings from {label}: {}",
                describe_change(&st.settings, &next)
            );
            st.settings = next;
            if let Some(path) = st.settings_path.clone() {
                if let Err(e) = next.save(&path) {
                    eprintln!("Settings: could not save {}: {e}", path.display());
                }
            }
            self.reapply(&st);
        }
        if !refused.is_empty() {
            println!("Settings from {label} refused: {}", refused.join(", "));
        }
        self.broadcast_settings(&st, change.token.map(|t| (id, t)));
    }

    /// This host is quitting: every device is told (kind 22 "quit": the app says "‹name› quit
    /// Sill." and reconnects once this host is back) and its held keys and buttons go up.
    pub fn quit(&self) {
        let goodbye = protocol::message(kind::GOODBYE, protocol::now(), false, br#"{"reason":"quit"}"#);
        let st = self.state.lock().unwrap();
        let input = self.input.lock().unwrap().clone();
        for (id, c) in &st.clients {
            let _ = c.tx.send(goodbye.clone());
            if let Some(tx) = &input {
                let _ = tx.send(InputCmd::Left(*id));
            }
        }
        if !st.clients.is_empty() {
            println!("Quitting: told {} device{}.", st.clients.len(), if st.clients.len() == 1 { "" } else { "s" });
        }
    }

    /// Where devices' input goes from now on.
    pub fn set_input(&self, tx: mpsc::UnboundedSender<InputCmd>) {
        *self.input.lock().unwrap() = Some(tx);
    }

    /// Binds and serves on threads of its own; returns the bound address.
    pub fn listen(&self, addr: SocketAddr) -> std::io::Result<SocketAddr> {
        let std_listener = std::net::TcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let local = std_listener.local_addr()?;
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("net").enable_all().build()?;
        let server = self.clone();
        thread::Builder::new().name("accept".into()).spawn(move || {
            runtime.block_on(async move {
                let listener = match TcpListener::from_std(std_listener) {
                    Ok(l) => l,
                    Err(e) => return eprintln!("Listener failed: {e}"),
                };
                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            let s = server.clone();
                            tokio::spawn(async move { s.handle(stream, peer).await });
                        }
                        Err(e) => eprintln!("Accept failed: {e}"),
                    }
                }
            })
        })?;
        let ticker = self.clone();
        thread::Builder::new().name("ticker".into()).spawn(move || ticker.tick_loop())?;
        Ok(local)
    }

    async fn handle(self, stream: TcpStream, peer: SocketAddr) {
        let _ = stream.set_nodelay(true);
        let peer_text = peer.to_string();
        let door = match &self.door {
            Door::Plain => return self.session(stream, peer_text).await,
            Door::Tls(d) => d.clone(),
        };
        let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, door.acceptor().accept(stream)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return println!("TLS handshake with {peer_text} failed: {e}"),
            Err(_) => return println!("TLS handshake with {peer_text} timed out"),
        };
        let (alpn, fingerprint) = {
            let (_, conn) = tls.get_ref();
            let fp = conn.peer_certificates().and_then(|c| c.first()).and_then(|c| crate::tls::spki_fingerprint(c));
            (conn.alpn_protocol().map(|a| a.to_vec()), fp)
        };
        let Some(fingerprint) = fingerprint else { return println!("TLS from {peer_text} without a P-256 key: closed") };
        match door.admit(alpn.as_deref(), &fingerprint) {
            Admission::Pairing => door.serve_pairing(tls, fingerprint, peer_text).await,
            Admission::Session(label) => self.session(tls, format!("{label} at {peer_text}")).await,
            Admission::Refuse(goodbye) => {
                let mut tls = tls;
                if let Some(g) = goodbye {
                    let _ = tls.write_all(&protocol::message(kind::GOODBYE, protocol::now(), false, g.as_bytes())).await;
                }
                let _ = tls.shutdown().await;
            }
        }
    }

    fn window_list(&self, active: StreamSource) -> Vec<u8> {
        let list = WindowList {
            mac_name: (*self.host_name).clone(),
            windows: vec![],
            active,
            launch_id: Some((*self.launch_id).clone()),
            host_version: None,
            protocol: Some(protocol::PROTOCOL),
        };
        protocol::message(kind::WINDOW_LIST, protocol::now(), false, &serde_json::to_vec(&list).unwrap())
    }

    /// One session, on any byte stream (plain TCP or TLS): the window list, then frames,
    /// ticks and pongs out; the device's messages in.
    async fn session<S: AsyncRead + AsyncWrite + Send + 'static>(&self, stream: S, peer: String) {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (close_tx, mut close_rx) = watch::channel(false);
        let mut close_rx_writer = close_tx.subscribe();
        let frames_in_flight = Arc::new(AtomicUsize::new(0));
        let last_drain = Arc::new(Mutex::new(Instant::now()));

        let id = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_id;
            st.next_id += 1;
            let _ = tx.send(self.window_list(st.active));
            let _ = tx.send(self.settings_message(&st, None));
            st.clients.insert(
                id,
                Client {
                    label: peer.clone(),
                    tx,
                    close: close_tx,
                    frames_in_flight: frames_in_flight.clone(),
                    last_drain: last_drain.clone(),
                    connected_at: Instant::now(),
                    needs_keyframe: true,
                    fps: None,
                },
            );
            if st.active != StreamSource::None {
                let _ = self.control.send(Control::Keyframe);
            }
            id
        };
        println!("Client connected: {peer}");

        let writer = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    m = rx.recv() => {
                        let Some(data) = m else { break };
                        let is_frame = data[0] == kind::FRAME;
                        if wr.write_all(&data).await.is_err() {
                            break;
                        }
                        if is_frame {
                            frames_in_flight.fetch_sub(1, Ordering::SeqCst);
                            *last_drain.lock().unwrap() = Instant::now();
                        }
                    }
                    _ = close_rx_writer.changed() => break,
                }
            }
            let _ = wr.shutdown().await;
            writer.remove(id, "write failed");
        });

        let mut stats_seen = 0u64;
        let mut input_noted = false;
        let reason = loop {
            let msg = tokio::select! {
                m = read_message(&mut rd) => m,
                _ = close_rx.changed() => break "closed by the host",
            };
            let (h, payload) = match msg {
                Ok(m) => m,
                Err(e) => break if e.kind() == std::io::ErrorKind::InvalidData { "message too large" } else { "closed" },
            };
            match h.kind {
                kind::PING => self.send_to(id, protocol::message(kind::PONG, protocol::now(), false, &payload)),
                kind::SELECT_SOURCE => match serde_json::from_slice::<StreamSource>(&payload) {
                    Ok(source) => self.select(id, source),
                    Err(e) => eprintln!("Bad source from client {id}: {e}"),
                },
                kind::HELLO => {
                    if let Ok(hello) = serde_json::from_slice::<Hello>(&payload) {
                        let label = hello.device.clone().unwrap_or_else(|| "a device".into());
                        println!(
                            "Client hello: {label}, Sill {} ({}), protocol {}",
                            hello.app_version.as_deref().unwrap_or("?"),
                            hello.build.as_deref().unwrap_or("?"),
                            hello.protocol.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                        );
                    }
                }
                kind::CHANGE_SETTINGS => match serde_json::from_slice::<HostSettingsChange>(&payload) {
                    Ok(change) => self.change_settings(id, change),
                    Err(e) => eprintln!("Bad settings change from client {id}: {e}"),
                },
                kind::VIEWPORT => {
                    if let Ok(v) = serde_json::from_slice::<Viewport>(&payload) {
                        let mut st = self.state.lock().unwrap();
                        let changed = st.clients.get_mut(&id).map(|c| {
                            let was = c.fps;
                            c.fps = v.fps;
                            was != v.fps
                        });
                        if changed == Some(true) {
                            self.reapply(&st);
                        }
                        drop(st);
                        println!(
                            "Viewport from client {id}: {}×{} pt, scale {}, {} fps",
                            v.width,
                            v.height,
                            v.scale.map(|s| s.to_string()).unwrap_or_else(|| "none".into()),
                            v.fps.unwrap_or(60)
                        );
                    }
                }
                kind::CLIENT_STATS => {
                    stats_seen += 1;
                    if stats_seen % 2 == 0 {
                        if let Ok(s) = serde_json::from_slice::<ClientStats>(&payload) {
                            println!(
                                "client {}: {} fps, frame age {}, rtt {}",
                                s.device,
                                s.fps,
                                median_and_worst(s.frame_age_ms, s.frame_age_max_ms),
                                median_and_worst(s.rtt_ms, s.rtt_max_ms)
                            );
                        }
                    }
                }
                kind::INPUT => {
                    let input = self.input.lock().unwrap().clone();
                    match (input, InputEvent::parse(&payload)) {
                        (Some(tx), Some(event)) => {
                            let _ = tx.send(InputCmd::Event(id, event));
                        }
                        (None, _) if !input_noted => {
                            input_noted = true;
                            println!("Input from client {id} is ignored (view only).");
                        }
                        _ => {}
                    }
                }
                _ => {} // kinds this host does not serve yet are skipped, as Sill's readers skip unknown kinds
            }
        };
        self.remove(id, reason);
    }

    fn send_to(&self, id: u64, data: Vec<u8>) {
        if let Some(c) = self.state.lock().unwrap().clients.get(&id) {
            let _ = c.tx.send(data);
        }
    }

    fn select(&self, id: u64, source: StreamSource) {
        let mut st = self.state.lock().unwrap();
        let chosen = match source {
            StreamSource::Window(w) => {
                println!("Client {id} picked window {w}; this host lists no windows yet, so it keeps the Desktop.");
                StreamSource::Desktop
            }
            other => other,
        };
        if chosen == st.active {
            if let Some(c) = st.clients.get(&id) {
                let _ = c.tx.send(self.window_list(st.active));
            }
            return;
        }
        st.active = chosen;
        println!("Streaming: {}", if chosen == StreamSource::Desktop { "the Desktop" } else { "nothing" });
        let list = self.window_list(chosen);
        for c in st.clients.values_mut() {
            c.needs_keyframe = true;
            let _ = c.tx.send(list.clone());
        }
        let _ = self.control.send(if chosen == StreamSource::None { Control::Stop } else { Control::Start });
    }

    fn remove(&self, id: u64, reason: &str) {
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.remove(&id) else { return };
        let _ = c.close.send(true);
        if let Some(tx) = self.input.lock().unwrap().as_ref() {
            let _ = tx.send(InputCmd::Left(id));
        }
        println!("Client left: {} ({reason})", c.label);
        if st.clients.is_empty() && st.active != StreamSource::None {
            st.active = StreamSource::None;
            println!("Streaming: nothing (no devices)");
            let _ = self.control.send(Control::Stop);
        }
        self.reapply(&st);
    }

    /// Called from the capture thread with each encoded access unit.
    pub fn broadcast_access_unit(&self, au: &protocol::AccessUnit) {
        if au.frame.is_empty() {
            return;
        }
        let ts = protocol::now();
        let frame = protocol::message(kind::FRAME, ts, au.keyframe, &au.frame);
        let params = (au.keyframe && !au.parameter_sets.is_empty()).then(|| {
            protocol::message(kind::PARAMETER_SETS, ts, true, &protocol::encode_parameter_sets(&au.parameter_sets))
        });
        let mut want_keyframe = false;
        let (mut sent, mut dropped) = (0u64, 0u64);
        let mut st = self.state.lock().unwrap();
        if st.active == StreamSource::None {
            return;
        }
        for c in st.clients.values_mut() {
            if c.needs_keyframe {
                let Some(params) = params.as_ref() else { continue };
                let _ = c.tx.send(params.clone());
                c.needs_keyframe = false;
            } else if !au.keyframe && c.frames_in_flight.load(Ordering::SeqCst) > MAX_FRAMES_IN_FLIGHT {
                dropped += 1;
                c.needs_keyframe = true;
                want_keyframe = true;
                continue;
            } else if let Some(params) = params.as_ref() {
                let _ = c.tx.send(params.clone());
            }
            c.frames_in_flight.fetch_add(1, Ordering::SeqCst);
            let _ = c.tx.send(frame.clone());
            sent += 1;
        }
        st.sent += sent;
        st.dropped += dropped;
        if want_keyframe {
            let _ = self.control.send(Control::Keyframe);
        }
    }

    /// "net.sent N net.dropped M" since the last call, for the capture side's stats line.
    pub fn take_stats(&self) -> String {
        let mut st = self.state.lock().unwrap();
        let s = format!("net.sent {} net.dropped {}", st.sent, st.dropped);
        st.sent = 0;
        st.dropped = 0;
        s
    }

    fn tick_loop(&self) {
        loop {
            thread::sleep(TICK_INTERVAL);
            let tick = protocol::message(kind::TICK, protocol::now(), false, &[]);
            let mut evict = Vec::new();
            {
                let st = self.state.lock().unwrap();
                let streaming = st.active != StreamSource::None;
                for (id, c) in &st.clients {
                    if streaming {
                        let _ = c.tx.send(tick.clone());
                    }
                    let stuck = c.frames_in_flight.load(Ordering::SeqCst) > 0
                        && c.last_drain.lock().unwrap().elapsed() > EVICT_AFTER
                        && c.connected_at.elapsed() > EVICT_GRACE;
                    if stuck {
                        evict.push(*id);
                    }
                }
            }
            for id in evict {
                self.remove(id, "no frame drained for 4 s");
            }
        }
    }
}

/// "bitrate 15 → 40 Mbps per 60 fps, resolution Standard → Retina".
fn describe_change(a: &Settings, b: &Settings) -> String {
    let mut parts = Vec::new();
    if a.bitrate != b.bitrate {
        parts.push(format!("bitrate {} → {} Mbps per 60 fps", a.bitrate / 1_000_000, b.bitrate / 1_000_000));
    }
    if a.capture_scale != b.capture_scale {
        let name = |s: &Settings| if s.retina() { "Retina" } else { "Standard" };
        parts.push(format!("resolution {} → {}", name(a), name(b)));
    }
    if a.max_fps != b.max_fps {
        parts.push(format!("frame rate limit {} → {}", a.max_fps, b.max_fps));
    }
    parts.join(", ")
}

/// One message: the header, then its payload (refused past Sill's cap for client messages).
pub async fn read_message<R: AsyncRead + Unpin>(rd: &mut R) -> std::io::Result<(protocol::Header, Vec<u8>)> {
    let mut header = [0u8; protocol::HEADER_LEN];
    rd.read_exact(&mut header).await?;
    let h = protocol::Header::parse(&header);
    if h.len > protocol::MAX_CLIENT_PAYLOAD {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
    }
    let mut payload = vec![0u8; h.len];
    rd.read_exact(&mut payload).await?;
    Ok((h, payload))
}

/// Sill's client stats format: "7/80 ms", "–" for a second without a sample.
fn median_and_worst(median: i64, worst: Option<i64>) -> String {
    match worst {
        None => format!("{median} ms"),
        Some(w) if median < 0 => if w < 0 { "–".into() } else { format!("–/{w} ms") },
        Some(w) => format!("{median}/{} ms", w.max(median)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddrV4, TcpStream as StdStream};
    use std::sync::mpsc::{self as smpsc, Receiver};

    fn read_message(s: &mut StdStream) -> (protocol::Header, Vec<u8>) {
        let mut h = [0u8; protocol::HEADER_LEN];
        s.read_exact(&mut h).unwrap();
        let h = protocol::Header::parse(&h);
        let mut p = vec![0u8; h.len];
        s.read_exact(&mut p).unwrap();
        (h, p)
    }

    /// Reads until a message of `want` arrives, skipping ticks and others.
    fn read_until(s: &mut StdStream, want: u8) -> (protocol::Header, Vec<u8>) {
        loop {
            let m = read_message(s);
            if m.0.kind == want {
                return m;
            }
        }
    }

    fn start() -> (Server, Receiver<Control>, SocketAddr) {
        let (ctx, crx) = smpsc::channel();
        let server = Server::new("test host".into(), ctx, Door::Plain, None);
        let addr = server.listen(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))).unwrap();
        (server, crx, addr)
    }

    fn select_desktop(s: &mut StdStream) {
        let m = protocol::message(kind::SELECT_SOURCE, 0.0, false, br#"{"desktop":{}}"#);
        s.write_all(&m).unwrap();
    }

    fn au(keyframe: bool) -> protocol::AccessUnit {
        protocol::AccessUnit {
            parameter_sets: if keyframe { vec![vec![0x40, 1], vec![0x42, 1], vec![0x44, 1]] } else { vec![] },
            frame: vec![0, 0, 0, 2, if keyframe { 0x26 } else { 0x02 }, 1],
            keyframe,
        }
    }

    #[test]
    fn session_like_the_app() {
        let (server, crx, addr) = start();
        let mut s = StdStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // First message: the window list, nothing streaming.
        let (h, p) = read_message(&mut s);
        assert_eq!(h.kind, kind::WINDOW_LIST);
        let list: WindowList = serde_json::from_slice(&p).unwrap();
        assert_eq!(list.active, StreamSource::None);
        assert_eq!(list.mac_name, "test host");

        // Ping is echoed as pong with the same payload.
        s.write_all(&protocol::message(kind::PING, 0.0, false, &1.5f64.to_be_bytes())).unwrap();
        let (_, p) = read_until(&mut s, kind::PONG);
        assert_eq!(p, 1.5f64.to_be_bytes());

        // The device asks for the Desktop: the capture side is told to start, the list says so.
        select_desktop(&mut s);
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Start);
        let (_, p) = read_until(&mut s, kind::WINDOW_LIST);
        assert_eq!(serde_json::from_slice::<WindowList>(&p).unwrap().active, StreamSource::Desktop);

        // A delta before any keyframe is withheld; the keyframe comes with its parameter sets.
        server.broadcast_access_unit(&au(false));
        server.broadcast_access_unit(&au(true));
        let (h, p) = read_until(&mut s, kind::PARAMETER_SETS);
        assert!(h.keyframe);
        assert_eq!(p, protocol::encode_parameter_sets(&au(true).parameter_sets));
        let (h, p) = read_until(&mut s, kind::FRAME);
        assert!(h.keyframe);
        assert_eq!(p, au(true).frame);
        server.broadcast_access_unit(&au(false));
        let (h, _) = read_until(&mut s, kind::FRAME);
        assert!(!h.keyframe);

        // Ticks flow while streaming.
        read_until(&mut s, kind::TICK);

        // The last client leaving stops the capture.
        drop(s);
        let mut got_stop = false;
        while let Ok(c) = crx.recv_timeout(Duration::from_secs(2)) {
            if c == Control::Stop {
                got_stop = true;
                break;
            }
        }
        assert!(got_stop);
    }

    #[test]
    fn joining_client_asks_for_keyframe() {
        let (_server, crx, addr) = start();
        let mut a = StdStream::connect(addr).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        read_message(&mut a);
        select_desktop(&mut a);
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Start);
        let mut b = StdStream::connect(addr).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (_, p) = read_message(&mut b);
        assert_eq!(serde_json::from_slice::<WindowList>(&p).unwrap().active, StreamSource::Desktop);
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Keyframe);
    }

    #[test]
    fn stats_format() {
        assert_eq!(median_and_worst(7, Some(80)), "7/80 ms");
        assert_eq!(median_and_worst(-1, Some(-1)), "–");
        assert_eq!(median_and_worst(-1, Some(9)), "–/9 ms");
        assert_eq!(median_and_worst(7, None), "7 ms");
    }
}
