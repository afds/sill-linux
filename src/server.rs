//! The plain TCP door: Sill's home door as the CLI host serves it without `--pairing`, which a
//! Debug build of the Sill app still dials (phase 1; the TLS door is phase 3).
//!
//! Threads, as Sill's host: one reader and one writer per client, a ticker, and the capture
//! side calling `broadcast_access_unit` from its own thread. One lock guards the shared state;
//! nothing blocks under it (writers own the sockets).
//!
//! Rules carried over from Sill's `StreamServer` (the home branch of `broadcast`):
//! - a client that joins waits for a keyframe, and gets the parameter sets just before it;
//! - a client with more than two frames unsent loses the next delta and waits for a keyframe
//!   (latency beats quality), and the encoder is asked for one;
//! - an empty tick every 30 ms while streaming keeps the device's Wi-Fi out of power save;
//! - a client that drains no frame for 4 s is closed (not in its first 8 s).

use crate::protocol::{self, kind, ClientStats, Hello, StreamSource, Viewport, WindowList};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TICK_INTERVAL: Duration = Duration::from_millis(30);
const EVICT_AFTER: Duration = Duration::from_secs(4);
const EVICT_GRACE: Duration = Duration::from_secs(8);
const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// What the server asks of the capture side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Control {
    /// Start encoding the Desktop (a device picked it).
    Start,
    /// Nobody watches any more: stop encoding.
    Stop,
    /// A client needs a keyframe (it joined, or it lost a delta).
    Keyframe,
}

struct Client {
    label: String,
    tx: Sender<Vec<u8>>,
    socket: TcpStream,
    frames_in_flight: Arc<AtomicUsize>,
    last_drain: Arc<Mutex<Instant>>,
    connected_at: Instant,
    needs_keyframe: bool,
}

struct State {
    clients: HashMap<u64, Client>,
    next_id: u64,
    active: StreamSource,
}

#[derive(Clone)]
pub struct Server {
    state: Arc<Mutex<State>>,
    control: Sender<Control>,
    host_name: Arc<String>,
    launch_id: Arc<String>,
}

impl Server {
    pub fn new(host_name: String, control: Sender<Control>) -> Server {
        Server {
            state: Arc::new(Mutex::new(State { clients: HashMap::new(), next_id: 1, active: StreamSource::None })),
            control,
            host_name: Arc::new(host_name),
            launch_id: Arc::new(uuid::Uuid::new_v4().to_string()),
        }
    }

    /// Binds and serves on its own threads; returns the bound address.
    pub fn listen(&self, addr: SocketAddr) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let server = self.clone();
        thread::Builder::new().name("accept".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => server.accept(s),
                    Err(e) => eprintln!("Accept failed: {e}"),
                }
            }
        })?;
        let ticker = self.clone();
        thread::Builder::new().name("ticker".into()).spawn(move || ticker.tick_loop())?;
        Ok(local)
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

    fn accept(&self, socket: TcpStream) {
        let peer = socket.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
        let _ = socket.set_nodelay(true);
        let (Ok(read_half), Ok(write_half)) = (socket.try_clone(), socket.try_clone()) else { return };
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let frames_in_flight = Arc::new(AtomicUsize::new(0));
        let last_drain = Arc::new(Mutex::new(Instant::now()));

        let id = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_id;
            st.next_id += 1;
            let _ = tx.send(self.window_list(st.active));
            st.clients.insert(
                id,
                Client {
                    label: peer.clone(),
                    tx,
                    socket,
                    frames_in_flight: frames_in_flight.clone(),
                    last_drain: last_drain.clone(),
                    connected_at: Instant::now(),
                    needs_keyframe: true,
                },
            );
            if st.active != StreamSource::None {
                let _ = self.control.send(Control::Keyframe);
            }
            id
        };
        println!("Client connected: {peer}");

        let writer = self.clone();
        let _ = thread::Builder::new()
            .name(format!("write-{id}"))
            .spawn(move || writer.write_loop(id, write_half, rx, frames_in_flight, last_drain));
        let reader = self.clone();
        let _ = thread::Builder::new().name(format!("read-{id}")).spawn(move || reader.read_loop(id, read_half));
    }

    fn write_loop(
        &self,
        id: u64,
        mut socket: TcpStream,
        rx: Receiver<Vec<u8>>,
        frames_in_flight: Arc<AtomicUsize>,
        last_drain: Arc<Mutex<Instant>>,
    ) {
        for data in rx {
            let is_frame = data[0] == kind::FRAME;
            if socket.write_all(&data).is_err() {
                break;
            }
            if is_frame {
                frames_in_flight.fetch_sub(1, Ordering::SeqCst);
                *last_drain.lock().unwrap() = Instant::now();
            }
        }
        self.remove(id, "write failed");
    }

    fn read_loop(&self, id: u64, mut socket: TcpStream) {
        let mut header = [0u8; protocol::HEADER_LEN];
        let mut stats_seen = 0u64;
        let mut input_noted = false;
        let reason = loop {
            if socket.read_exact(&mut header).is_err() {
                break "closed";
            }
            let h = protocol::Header::parse(&header);
            if h.len > protocol::MAX_CLIENT_PAYLOAD {
                break "message too large";
            }
            let mut payload = vec![0u8; h.len];
            if socket.read_exact(&mut payload).is_err() {
                break "closed";
            }
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
                        if let Some(c) = self.state.lock().unwrap().clients.get_mut(&id) {
                            c.label = format!("{label} at {}", c.label);
                        }
                    }
                }
                kind::VIEWPORT => {
                    if let Ok(v) = serde_json::from_slice::<Viewport>(&payload) {
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
                    if !input_noted {
                        input_noted = true;
                        println!("Input from client {id} is ignored for now (phase 2).");
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
        let _ = c.socket.shutdown(Shutdown::Both);
        println!("Client left: {} ({reason})", c.label);
        if st.clients.is_empty() && st.active != StreamSource::None {
            st.active = StreamSource::None;
            println!("Streaming: nothing (no devices)");
            let _ = self.control.send(Control::Stop);
        }
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
                c.needs_keyframe = true;
                want_keyframe = true;
                continue;
            } else if let Some(params) = params.as_ref() {
                let _ = c.tx.send(params.clone());
            }
            c.frames_in_flight.fetch_add(1, Ordering::SeqCst);
            let _ = c.tx.send(frame.clone());
        }
        if want_keyframe {
            let _ = self.control.send(Control::Keyframe);
        }
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
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn read_message(s: &mut TcpStream) -> (protocol::Header, Vec<u8>) {
        let mut h = [0u8; protocol::HEADER_LEN];
        s.read_exact(&mut h).unwrap();
        let h = protocol::Header::parse(&h);
        let mut p = vec![0u8; h.len];
        s.read_exact(&mut p).unwrap();
        (h, p)
    }

    /// Reads until a message of `want` arrives, skipping ticks and others.
    fn read_until(s: &mut TcpStream, want: u8) -> (protocol::Header, Vec<u8>) {
        loop {
            let m = read_message(s);
            if m.0.kind == want {
                return m;
            }
        }
    }

    fn start() -> (Server, Receiver<Control>, SocketAddr) {
        let (ctx, crx) = mpsc::channel();
        let server = Server::new("test host".into(), ctx);
        let addr = server.listen(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))).unwrap();
        (server, crx, addr)
    }

    fn select_desktop(s: &mut TcpStream) {
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
        let mut s = TcpStream::connect(addr).unwrap();
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
        let mut a = TcpStream::connect(addr).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        read_message(&mut a);
        select_desktop(&mut a);
        assert_eq!(crx.recv_timeout(Duration::from_secs(2)).unwrap(), Control::Start);
        let mut b = TcpStream::connect(addr).unwrap();
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
