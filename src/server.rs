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
/// Sill's `Door`: connections not yet admitted, in all and from one address.
const MAX_PENDING: usize = 8;
const MAX_PENDING_PER_SOURCE: usize = 2;

/// A connection not yet admitted, counted against the caps until dropped.
struct Pending {
    _permit: tokio::sync::OwnedSemaphorePermit,
    ip: std::net::IpAddr,
    sources: Arc<Mutex<HashMap<std::net::IpAddr, usize>>>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut m = self.sources.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

/// Seconds on a monotonic clock, for the pointer's rules.
fn clock() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Kind 4: bundleID length (u16 BE), the id, the PNG.
fn icon_message(app: &str, png: &[u8]) -> Vec<u8> {
    let mut p = (app.len() as u16).to_be_bytes().to_vec();
    p.extend_from_slice(app.as_bytes());
    p.extend_from_slice(png);
    protocol::message(kind::APP_ICON, protocol::now(), false, &p)
}

/// Kind 5: `[AppInfo]`, `{"name", "bundleID"}`.
fn apps_message(apps: &[(String, String)]) -> Vec<u8> {
    let list: Vec<serde_json::Value> = apps.iter().map(|(id, name)| serde_json::json!({"name": name, "bundleID": id})).collect();
    protocol::message(kind::APP_LIST, protocol::now(), false, &serde_json::to_vec(&list).unwrap())
}

/// What the server asks of the capture side.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// Start encoding this source (a device picked it): the Desktop, or a window by Mutter's id.
    Start(Source),
    /// Nobody watches any more: stop encoding.
    Stop,
    /// A client needs a keyframe (it joined, or it lost a delta).
    Keyframe,
    /// What the encoder should run at now.
    Apply(Apply),
    /// A window's light from the device's bar: "close", "minimize" or "fullScreen".
    WindowCommand(u64, String),
    /// Start an installed app (its .desktop id) from the device's All Apps; its window is picked.
    Launch(String),
    /// A trackpad gesture (one of Sill's six names) for GNOME to act on.
    Gesture(String),
    /// Aa: size the streamed window's frame to this many logical pixels (the device's panel over
    /// its text scale).
    Fit(u64, i32, i32),
}

/// What is streamed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Desktop,
    /// Mutter's window id.
    Window(u64),
}

/// A window this host can stream, from the window list (the GNOME Shell extension).
#[derive(Debug, Clone, PartialEq)]
pub struct HostWindow {
    pub mutter_id: u64,
    pub title: String,
    pub app_name: String,
    pub app_id: String,
    pub width: i64,
    pub height: i64,
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
    /// Kind 8 messages read from it (kind 26's `seen`), and the last kind 26 sent to it.
    inputs_read: u64,
    last_pointer: Option<Vec<u8>>,
    /// Sent kind 32: this machine's copies go to it. With the times of its last second's asks.
    clip_subscribed: bool,
    clip_arrivals: Vec<f64>,
    gesture_arrivals: Vec<f64>,
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
    /// The windows the device's bar lists, with the 32-bit ids the app uses for them.
    windows: Vec<protocol::WindowInfo>,
    wire_ids: HashMap<u64, u32>,
    mutter_ids: HashMap<u32, u64>,
    next_wire_id: u32,
    /// Installed apps (.desktop id, name) and their icons, sent to every device as it connects.
    apps: Vec<(String, String)>,
    icons: Vec<(String, Vec<u8>)>,
    /// The pointer's current image (kind 14's payload), for devices as they connect.
    cursor: Option<Vec<u8>>,
    /// Who moves this machine's pointer (kind 26).
    pointer: crate::pointer::PointerControl,
    /// The last viewport any device sent (one stream: the last to report wins, as Sill for Mac).
    viewport: Option<Viewport>,
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
    /// Where a device's text goes to become this machine's clipboard; None: not shared.
    clipboard: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    /// Trackpad gestures are taken (GNOME, with the extension).
    gestures: Arc<std::sync::atomic::AtomicBool>,
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
                windows: Vec::new(),
                wire_ids: HashMap::new(),
                mutter_ids: HashMap::new(),
                next_wire_id: 1,
                apps: Vec::new(),
                icons: Vec::new(),
                viewport: None,
                pointer: Default::default(),
                cursor: None,
            })),
            control,
            host_name: Arc::new(host_name),
            launch_id: Arc::new(uuid::Uuid::new_v4().to_string()),
            door,
            input: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(None)),
            gestures: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            if let Some(path) = st.settings_path.clone()
                && let Err(e) = next.save(&path) {
                    eprintln!("Settings: could not save {}: {e}", path.display());
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
        // A window Aa sized goes back.
        let _ = self.control.send(Control::Stop);
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
                let permits = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));
                let sources = Arc::new(Mutex::new(HashMap::new()));
                let mut last_refusal: Option<Instant> = None;
                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            let ip = peer.ip().to_canonical();
                            if !crate::door::origin_allowed(ip, &crate::door::local_subnets()) {
                                if last_refusal.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                                    println!("Refused a connection from {ip}: not this machine's networks (at most one such line a minute).");
                                    last_refusal = Some(Instant::now());
                                }
                                continue;
                            }
                            let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                            {
                                let mut m = sources.lock().unwrap();
                                let n = m.entry(ip).or_insert(0);
                                if *n >= MAX_PENDING_PER_SOURCE {
                                    continue;
                                }
                                *n += 1;
                            }
                            let pending = Pending { _permit: permit, ip, sources: sources.clone() };
                            let s = server.clone();
                            tokio::spawn(async move { s.handle(stream, peer, pending).await });
                        }
                        Err(e) => {
                            // Out of file descriptors, say: never spin.
                            eprintln!("Accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            })
        })?;
        let ticker = self.clone();
        thread::Builder::new().name("ticker".into()).spawn(move || ticker.tick_loop())?;
        Ok(local)
    }

    async fn handle(self, stream: TcpStream, peer: SocketAddr, pending: Pending) {
        let _ = stream.set_nodelay(true);
        let peer_text = peer.to_string();
        let door = match &self.door {
            Door::Plain => {
                drop(pending);
                return self.session(stream, peer_text).await;
            }
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
            Admission::Session(label) => {
                drop(pending);
                self.session(tls, format!("{label} at {peer_text}")).await
            }
            Admission::Refuse(goodbye) => {
                let mut tls = tls;
                if let Some(g) = goodbye {
                    let _ = tls.write_all(&protocol::message(kind::GOODBYE, protocol::now(), false, g.as_bytes())).await;
                }
                let _ = tls.shutdown().await;
            }
        }
    }

    fn window_list(&self, st: &State) -> Vec<u8> {
        let active = st.active;
        let list = WindowList {
            mac_name: (*self.host_name).clone(),
            windows: st.windows.clone(),
            active,
            launch_id: Some((*self.launch_id).clone()),
            host_version: None,
            protocol: Some(protocol::PROTOCOL),
            gestures: self.gestures.load(Ordering::Relaxed).then_some(1),
            clipboard: self.clipboard.lock().unwrap().is_some().then_some(crate::clipboard::GENERATION),
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
        // shortcut: only pongs are bounded for a device that stops reading; window lists and cursor
        // shapes it never takes still queue (at the host's own pace) until it leaves or is evicted.
        let pongs_queued = Arc::new(AtomicUsize::new(0));
        let pongs_written = pongs_queued.clone();

        let id = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_id;
            st.next_id += 1;
            let _ = tx.send(self.window_list(&st));
            let _ = tx.send(self.settings_message(&st, None));
            for (app, png) in &st.icons {
                let _ = tx.send(icon_message(app, png));
            }
            let _ = tx.send(apps_message(&st.apps));
            if let Some(c) = &st.cursor {
                let _ = tx.send(protocol::message(kind::CURSOR_SHAPE, protocol::now(), false, c));
            }
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
                    inputs_read: 0,
                    last_pointer: None,
                    clip_subscribed: false,
                    clip_arrivals: Vec::new(),
                    gesture_arrivals: Vec::new(),
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
                        if data[0] == kind::PONG {
                            pongs_written.fetch_sub(1, Ordering::SeqCst);
                        }
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
                // The app's ping carries 8 or 16 bytes; at most 32 pongs wait for a device that stops reading.
                kind::PING if payload.len() <= 64 && pongs_queued.load(Ordering::SeqCst) < 32 => {
                    pongs_queued.fetch_add(1, Ordering::SeqCst);
                    self.send_to(id, protocol::message(kind::PONG, protocol::now(), false, &payload))
                }
                kind::PING => {}
                kind::SELECT_SOURCE => match serde_json::from_slice::<StreamSource>(&payload) {
                    Ok(source) => self.select(id, source),
                    Err(e) => eprintln!("Bad source from client {id}: {e}"),
                },
                kind::HELLO => {
                    if let Ok(hello) = serde_json::from_slice::<Hello>(&payload) {
                        let label = crate::door::clean(hello.device.as_deref().unwrap_or(""), "a device");
                        println!(
                            "Client hello: {label}, Sill {} ({}), protocol {}",
                            crate::door::clean(hello.app_version.as_deref().unwrap_or(""), "?"),
                            crate::door::clean(hello.build.as_deref().unwrap_or(""), "?"),
                            hello.protocol.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                        );
                    }
                }
                kind::WINDOW_COMMAND => self.window_command(id, &payload),
                kind::LAUNCH_APP => self.launch(id, &payload),
                kind::CLIPBOARD => self.clipboard_from(id, &payload),
                kind::GESTURE => self.gesture(id, &payload),
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
                        st.viewport = Some(v.clone());
                        if let Some(fit) = Self::fit_control(&st) {
                            let _ = self.control.send(fit);
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
                    if stats_seen.is_multiple_of(2)
                        && let Ok(s) = serde_json::from_slice::<ClientStats>(&payload) {
                            println!(
                                "client {}: {} fps, frame age {}, rtt {}",
                                crate::door::clean(&s.device, "a device"),
                                s.fps,
                                median_and_worst(s.frame_age_ms, s.frame_age_max_ms),
                                median_and_worst(s.rtt_ms, s.rtt_max_ms)
                            );
                        }
                }
                kind::INPUT => {
                    let input = self.input.lock().unwrap().clone();
                    let event = InputEvent::parse(&payload);
                    {
                        let mut st = self.state.lock().unwrap();
                        if let Some(c) = st.clients.get_mut(&id) {
                            c.inputs_read += 1;
                            c.last_pointer = None; // it drives: the host's next move reaches it again
                        }
                        let moves = event.as_ref().is_some_and(crate::pointer::moves_pointer);
                        st.pointer.input_arrived(id, moves, clock());
                    }
                    match (input, event) {
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
            StreamSource::Window(w) if !st.mutter_ids.contains_key(&w) => {
                println!("Client {id} picked window {w}, which this host does not list; it keeps what it streams.");
                if let Some(c) = st.clients.get(&id) {
                    let _ = c.tx.send(self.window_list(&st));
                }
                return;
            }
            other => other,
        };
        if chosen == st.active {
            if let Some(c) = st.clients.get(&id) {
                let _ = c.tx.send(self.window_list(&st));
            }
            return;
        }
        st.active = chosen;
        let control = match chosen {
            StreamSource::None => {
                println!("Streaming: nothing");
                Control::Stop
            }
            StreamSource::Desktop => {
                println!("Streaming: the Desktop");
                Control::Start(Source::Desktop)
            }
            StreamSource::Window(w) => {
                let m = st.mutter_ids[&w];
                let title = st.windows.iter().find(|x| x.id == w).map(|x| format!("{} — {}", x.app_name, x.title)).unwrap_or_default();
                println!("Streaming: window {title}");
                Control::Start(Source::Window(m))
            }
        };
        let list = self.window_list(&st);
        for c in st.clients.values_mut() {
            c.needs_keyframe = true;
            let _ = c.tx.send(list.clone());
        }
        let _ = self.control.send(control);
        if let Some(fit) = Self::fit_control(&st) {
            let _ = self.control.send(fit);
        }
    }

    /// Aa while a window streams: its frame sized to the panel over the text scale (nothing
    /// before the device first sets Aa, as Sill for Mac).
    fn fit_control(st: &State) -> Option<Control> {
        let StreamSource::Window(w) = st.active else { return None };
        let v = st.viewport.as_ref()?;
        let scale = v.scale.filter(|s| *s > 0.0)?;
        (v.width > 0.0 && v.height > 0.0).then_some(())?;
        Some(Control::Fit(*st.mutter_ids.get(&w)?, (v.width / scale).round() as i32, (v.height / scale).round() as i32))
    }

    /// Picks a window for every device, as a device's pick does (an app launched from a device).
    pub fn pick_window(&self, mutter_id: u64) {
        let wire = self.state.lock().unwrap().wire_ids.get(&mutter_id).copied();
        if let Some(w) = wire {
            self.select(0, StreamSource::Window(w));
        }
    }

    /// The installed apps, for the device's All Apps.
    pub fn set_apps(&self, apps: Vec<(String, String)>) {
        let mut st = self.state.lock().unwrap();
        if st.apps == apps {
            return;
        }
        st.apps = apps;
        let msg = apps_message(&st.apps);
        for c in st.clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// An app's icon (PNG), kept for devices that connect later.
    pub fn add_icon(&self, app: &str, png: Vec<u8>) {
        let mut st = self.state.lock().unwrap();
        let msg = icon_message(app, &png);
        match st.icons.iter_mut().find(|(a, _)| a == app) {
            Some(e) => e.1 = png,
            None => st.icons.push((app.to_string(), png)),
        }
        for c in st.clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    fn launch(&self, id: u64, payload: &[u8]) {
        #[derive(serde::Deserialize)]
        struct LaunchApp {
            #[serde(rename = "bundleID")]
            bundle_id: String,
        }
        let Ok(req) = serde_json::from_slice::<LaunchApp>(payload) else { return };
        let st = self.state.lock().unwrap();
        let Some((app, name)) = st.apps.iter().find(|(a, _)| *a == req.bundle_id).cloned() else {
            println!("Client {id} asked to launch {:?}, which this host does not list.", req.bundle_id);
            return;
        };
        println!("Launching {name} for client {id}");
        let _ = self.control.send(Control::Launch(app));
    }

    /// The window list (from the GNOME Shell extension): sent to the devices when it changed. A
    /// streamed window that is gone stops the stream (the app asks for the Desktop then).
    pub fn set_windows(&self, list: Vec<HostWindow>) {
        let mut st = self.state.lock().unwrap();
        let mut windows = Vec::with_capacity(list.len());
        for w in &list {
            let wire = match st.wire_ids.get(&w.mutter_id) {
                Some(&id) => id,
                None => {
                    let id = st.next_wire_id;
                    st.next_wire_id += 1;
                    st.wire_ids.insert(w.mutter_id, id);
                    st.mutter_ids.insert(id, w.mutter_id);
                    id
                }
            };
            windows.push(protocol::WindowInfo {
                id: wire,
                title: w.title.clone(),
                app_name: w.app_name.clone(),
                bundle_id: w.app_id.clone(),
                width: w.width,
                height: w.height,
            });
        }
        if windows == st.windows {
            return;
        }
        st.windows = windows;
        if let StreamSource::Window(w) = st.active
            && !st.windows.iter().any(|x| x.id == w) {
                println!("Streaming: nothing (the window closed)");
                st.active = StreamSource::None;
                let _ = self.control.send(Control::Stop);
            }
        let msg = self.window_list(&st);
        for c in st.clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// A window's thumbnail (any image the app's UIImage reads; PNG here) to every device.
    pub fn broadcast_thumbnail(&self, mutter_id: u64, image: &[u8]) {
        let st = self.state.lock().unwrap();
        let Some(&wire) = st.wire_ids.get(&mutter_id) else { return };
        let mut payload = wire.to_be_bytes().to_vec();
        payload.extend_from_slice(image);
        let msg = protocol::message(kind::THUMBNAIL, protocol::now(), false, &payload);
        for c in st.clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// The capture side went back to the Desktop by itself (a window could not be streamed, or its
    /// stream ended): the devices are told.
    pub fn showing_desktop(&self) {
        let mut st = self.state.lock().unwrap();
        if st.active == StreamSource::Desktop || st.active == StreamSource::None {
            return;
        }
        st.active = StreamSource::Desktop;
        println!("Streaming: the Desktop");
        let list = self.window_list(&st);
        for c in st.clients.values_mut() {
            c.needs_keyframe = true;
            let _ = c.tx.send(list.clone());
        }
    }

    /// One read of this machine's pointer (global logical pixels) against the streamed source's
    /// rectangle (None: nothing to judge it against, so off the stream). Every device but the one
    /// driving gets kind 26 when its payload changed. Whether the pointer is moving.
    pub fn pointer_sample(&self, p: (f64, f64), rect: Option<(f64, f64, f64, f64)>) -> bool {
        let mut st = self.state.lock().unwrap();
        let now = clock();
        st.pointer.read(p, now);
        let at = rect.map(|r| crate::pointer::fraction(p, r)).filter(|f| f.2).map(|f| (f.0, f.1));
        let driver = st.pointer.controller;
        for (id, c) in st.clients.iter_mut() {
            if driver == crate::pointer::Controller::Client(*id) {
                continue;
            }
            let payload = crate::pointer::payload(at, c.inputs_read);
            if c.last_pointer.as_deref() != Some(&payload[..]) {
                let _ = c.tx.send(protocol::message(kind::MAC_POINTER, protocol::now(), false, &payload));
                c.last_pointer = Some(payload);
            }
        }
        st.pointer.is_moving(now)
    }

    /// Shares the clipboard from now on: a device's text goes to `tx` to be written here.
    pub fn set_clipboard(&self, tx: mpsc::UnboundedSender<String>) {
        *self.clipboard.lock().unwrap() = Some(tx);
    }

    /// How many devices take this machine's copies.
    pub fn clipboard_subscribers(&self) -> usize {
        self.state.lock().unwrap().clients.values().filter(|c| c.clip_subscribed).count()
    }

    /// A copy made here, to every subscriber.
    pub fn broadcast_clipboard(&self, text: &str) {
        let Some(p) = crate::clipboard::payload(text) else { return };
        let msg = protocol::message(kind::CLIPBOARD, protocol::now(), false, &p);
        let st = self.state.lock().unwrap();
        for c in st.clients.values().filter(|c| c.clip_subscribed) {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// A device's kind 32: subscribe, stop, or its text becomes this machine's clipboard (and goes to
    /// the other subscribers). One line per subscription or stop, none per text, never contents.
    fn clipboard_from(&self, id: u64, payload: &[u8]) {
        use crate::clipboard::FromDevice;
        let Some(writer) = self.clipboard.lock().unwrap().clone() else { return };
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.get_mut(&id) else { return };
        let ask = crate::clipboard::host_receives(payload, &mut c.clip_arrivals, clock());
        let label = c.label.clone();
        match ask {
            FromDevice::Subscribe | FromDevice::Set(_) if !c.clip_subscribed => {
                c.clip_subscribed = true;
                println!("Clipboard: {label} shares the clipboard.");
            }
            FromDevice::Stop if c.clip_subscribed => {
                c.clip_subscribed = false;
                println!("Clipboard: {label} stopped sharing the clipboard.");
            }
            FromDevice::Refuse(why) => println!("Clipboard: refused a message from {label} ({why})."),
            _ => {}
        }
        if let FromDevice::Set(text) = ask {
            if let Some(p) = crate::clipboard::payload(&text) {
                let msg = protocol::message(kind::CLIPBOARD, protocol::now(), false, &p);
                for (other, c) in st.clients.iter().filter(|(o, c)| **o != id && c.clip_subscribed) {
                    let _ = other;
                    let _ = c.tx.send(msg.clone());
                }
            }
            let _ = writer.send(text);
        }
    }

    /// Takes trackpad gestures from now on (the window list says so).
    pub fn set_gestures(&self) {
        self.gestures.store(true, Ordering::Relaxed);
    }

    /// A device's three- or four-finger gesture (kind 28): one of Sill's six names, at most four a
    /// second per connection, as Sill for Mac.
    fn gesture(&self, id: u64, payload: &[u8]) {
        #[derive(serde::Deserialize)]
        struct TrackpadGesture {
            gesture: String,
        }
        const NAMES: [&str; 6] = ["swipeUp", "swipeDown", "swipeLeft", "swipeRight", "pinch", "spread"];
        if !self.gestures.load(Ordering::Relaxed) {
            return;
        }
        let Ok(g) = serde_json::from_slice::<TrackpadGesture>(payload) else { return };
        if !NAMES.contains(&g.gesture.as_str()) {
            return;
        }
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.get_mut(&id) else { return };
        let now = clock();
        c.gesture_arrivals.retain(|t| now - t < 1.0);
        if c.gesture_arrivals.len() >= 4 {
            return;
        }
        c.gesture_arrivals.push(now);
        let label = c.label.clone();
        drop(st);
        println!("Gesture from {label}: {}", g.gesture);
        let _ = self.control.send(Control::Gesture(g.gesture));
    }

    /// The pointer's image changed: hotspot and size in points, and a PNG. Sent to every device
    /// when it differs from the last.
    pub fn set_cursor_shape(&self, (hx, hy, w, h): (f64, f64, f64, f64), png: &[u8]) {
        let mut p = Vec::with_capacity(16 + png.len());
        for v in [hx, hy, w, h] {
            p.extend_from_slice(&(v as f32).to_be_bytes());
        }
        p.extend_from_slice(png);
        let mut st = self.state.lock().unwrap();
        if st.cursor.as_ref() == Some(&p) {
            return;
        }
        let msg = protocol::message(kind::CURSOR_SHAPE, protocol::now(), false, &p);
        st.cursor = Some(p);
        for c in st.clients.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// Whether a source streams (the pointer is watched only then).
    pub fn streaming(&self) -> bool {
        self.state.lock().unwrap().active != StreamSource::None
    }

    /// The streamed window's Mutter id, if a window streams.
    pub fn active_window(&self) -> Option<u64> {
        let st = self.state.lock().unwrap();
        match st.active {
            StreamSource::Window(w) => st.mutter_ids.get(&w).copied(),
            _ => None,
        }
    }

    /// How many devices are connected (thumbnails are made only then, and all again for a new one).
    pub fn client_count(&self) -> usize {
        self.state.lock().unwrap().clients.len()
    }

    fn window_command(&self, id: u64, payload: &[u8]) {
        #[derive(serde::Deserialize)]
        struct WindowCommand {
            id: u32,
            action: String,
        }
        let Ok(cmd) = serde_json::from_slice::<WindowCommand>(payload) else { return };
        let st = self.state.lock().unwrap();
        let Some(&m) = st.mutter_ids.get(&cmd.id) else { return };
        println!("Window command from client {id}: {:?} window {}", cmd.action, cmd.id);
        let _ = self.control.send(Control::WindowCommand(m, cmd.action));
    }

    fn remove(&self, id: u64, reason: &str) {
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clients.remove(&id) else { return };
        st.pointer.client_left(id);
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

    /// Sharing was stopped on this machine (GNOME's indicator): every device is told, in words the
    /// app shows as they are, and does not reconnect by itself (kind 22 with a reason it does not
    /// know, `reconnect` false); a tap on its row connects again. One that has not closed in 2 s is
    /// closed.
    pub fn stopped_here(&self) {
        let goodbye = serde_json::json!({
            "reason": "stopped",
            "message": format!("Sharing was stopped on {}. Tap it to connect again.", self.host_name),
            "reconnect": false,
        });
        let msg = protocol::message(kind::GOODBYE, protocol::now(), false, goodbye.to_string().as_bytes());
        let ids: Vec<u64> = {
            let st = self.state.lock().unwrap();
            for c in st.clients.values() {
                let _ = c.tx.send(msg.clone());
            }
            st.clients.keys().copied().collect()
        };
        let server = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            for id in ids {
                server.remove(id, "sharing stopped here");
            }
        });
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
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Start(Source::Desktop));
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
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Start(Source::Desktop));
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

    #[test]
    fn apps_icons_and_aa() {
        let icon = icon_message("org.gnome.Calculator.desktop", b"PNG");
        assert_eq!(icon[0], kind::APP_ICON);
        assert_eq!(&icon[14..16], &28u16.to_be_bytes());
        assert_eq!(&icon[16..44], b"org.gnome.Calculator.desktop");
        assert_eq!(&icon[44..], b"PNG");
        let apps = apps_message(&[("org.gnome.Calculator.desktop".into(), "Calculator".into())]);
        let v: serde_json::Value = serde_json::from_slice(&apps[14..]).unwrap();
        assert_eq!(v, serde_json::json!([{"name": "Calculator", "bundleID": "org.gnome.Calculator.desktop"}]));

        let (tx, _rx) = std::sync::mpsc::channel();
        let server = Server::new("test".into(), tx, Door::Plain, None);
        let mut st = server.state.lock().unwrap();
        st.wire_ids.insert(77, 1);
        st.mutter_ids.insert(1, 77);
        st.viewport = Some(Viewport { width: 1180.0, height: 760.0, scale: Some(1.5), fps: None });
        assert_eq!(Server::fit_control(&st), None, "the Desktop is never resized");
        st.active = StreamSource::Window(1);
        assert_eq!(Server::fit_control(&st), Some(Control::Fit(77, 787, 507)));
        st.viewport.as_mut().unwrap().scale = None;
        assert_eq!(Server::fit_control(&st), None, "no Aa yet: the window is left alone");
    }
}