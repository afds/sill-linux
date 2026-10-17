//! The TLS door's admission and pairing, as Sill's `Door` and `RemoteAccess` judge them at home.
//!
//! - `sill/1` (a session): with pairing required only a paired key, else any key. An unpaired key
//!   gets the goodbye `pairingRequired` (the app: "‹Mac› now asks devices to pair. Tap it to pair
//!   this ‹iPad›.") and the door closes.
//! - `sill-pair/1` (pairing): one kind 19 in, one kind 20 out, then close.
//!   - "ask": opens the pairing window (or keeps the open one), prints its code and QR code on
//!     this terminal, answers "shown"; the app then shows its scanner and "Enter Code Instead".
//!   - "code" / "qr": the proof against the window (five wrong codes stop it, 1, 2, 4, 8 s
//!     between tries); ok carries proof_M, the Mac ID, the name and the recognition key.
//!   - "cancel": the window closes, only for the key whose ask opened it; "closed".
//!   - asks are limited as Sill's `AskLimits` (below): past them, "openOnMac" and no code.
//!   - any `v` but 1: "closed", no try counted.

use crate::identity::{Identity, PairedDevice};
use crate::pairing::{self, b64url, b64url_decode, Verdict, Window};
use crate::protocol::{self, kind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

const PAIRING_READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PAIRING_PAYLOAD: usize = 4096;

/// Kind 19.
#[derive(Debug, Deserialize)]
pub struct PairRequest {
    pub v: i64,
    pub method: String,
    #[serde(default)]
    pub proof: String,
    #[serde(default)]
    pub name: String,
    pub model: Option<String>,
}

/// Kind 20; nil fields left out, as Swift's encoder does.
#[derive(Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
    #[serde(rename = "macID", skip_serializing_if = "Option::is_none")]
    pub mac_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recognition_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tries_left: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<f64>,
}

impl PairResult {
    fn refused(reason: &str) -> PairResult {
        PairResult { reason: Some(reason.into()), ..Default::default() }
    }
}

pub enum Admission {
    Pairing,
    /// A session; the label names the device for the log.
    Session(String),
    /// Closed, with this goodbye's JSON first if any.
    Refuse(Option<String>),
}

pub struct PairingDoor {
    acceptor: TlsAcceptor,
    pub identity: Identity,
    pub require_pairing: bool,
    host_name: String,
    port: u16,
    paired: Mutex<Vec<PairedDevice>>,
    window: Mutex<Option<Window>>,
    /// Locked only under `window`.
    asks: Mutex<AskLimits>,
    started: Instant,
    /// The pairing code's desktop notification.
    note: crate::notify::Note,
    /// TEST ONLY: SILL_LINUX_TEST_CODE_FILE, where each window's code is written (for
    /// Sill's sillclient.py `--then-code`).
    code_file: Option<PathBuf>,
}

impl PairingDoor {
    pub fn new(identity: Identity, require_pairing: bool, host_name: String, port: u16) -> anyhow::Result<Arc<PairingDoor>> {
        let config = crate::tls::server_config(&identity)?;
        let paired = identity.load_paired();
        Ok(Arc::new(PairingDoor {
            acceptor: TlsAcceptor::from(config),
            paired: Mutex::new(paired),
            identity,
            require_pairing,
            host_name,
            port,
            window: Mutex::new(None),
            asks: Mutex::new(AskLimits::default()),
            started: Instant::now(),
            note: crate::notify::Note::default(),
            code_file: std::env::var_os("SILL_LINUX_TEST_CODE_FILE").map(PathBuf::from),
        }))
    }

    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }

    pub fn paired_count(&self) -> usize {
        self.paired.lock().unwrap().len()
    }

    fn paired_device(&self, fingerprint: &[u8]) -> Option<PairedDevice> {
        let fp = b64url(fingerprint);
        self.paired.lock().unwrap().iter().find(|d| d.fingerprint == fp).cloned()
    }

    pub fn admit(&self, alpn: Option<&[u8]>, fingerprint: &[u8; 32]) -> Admission {
        match alpn {
            Some(a) if a == crate::tls::PAIRING_ALPN => Admission::Pairing,
            Some(a) if a == crate::tls::SESSION_ALPN => match self.paired_device(fingerprint) {
                Some(d) => Admission::Session(d.name),
                None if !self.require_pairing => Admission::Session(format!("an unpaired device (key {}…)", &b64url(fingerprint)[..8])),
                None => {
                    println!("Refused a session from an unpaired key ({}…): pairing is required.", &b64url(fingerprint)[..8]);
                    Admission::Refuse(Some(r#"{"reason":"pairingRequired"}"#.into()))
                }
            },
            _ => Admission::Refuse(None),
        }
    }

    pub async fn serve_pairing<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        self: Arc<Self>,
        mut stream: S,
        fp_device: [u8; 32],
        peer: String,
    ) {
        // The header first: nothing is allocated for a message that is not a small kind 19.
        let read = async {
            let mut header = [0u8; protocol::HEADER_LEN];
            stream.read_exact(&mut header).await.ok()?;
            let h = protocol::Header::parse(&header);
            if h.kind != kind::PAIR_REQUEST || h.len > MAX_PAIRING_PAYLOAD {
                return None;
            }
            let mut p = vec![0u8; h.len];
            stream.read_exact(&mut p).await.ok()?;
            serde_json::from_slice::<PairRequest>(&p).ok()
        };
        let request = tokio::time::timeout(PAIRING_READ_TIMEOUT, read).await.ok().flatten();
        let result = match request {
            None => PairResult::refused("closed"),
            Some(req) => {
                // PBKDF2 (opening a window) takes a few hundred milliseconds: off the network threads.
                let door = self.clone();
                tokio::task::spawn_blocking(move || door.judge(req, &fp_device, &peer)).await.unwrap_or_else(|_| PairResult::refused("closed"))
            }
        };
        let payload = serde_json::to_vec(&result).unwrap();
        let _ = stream.write_all(&protocol::message(kind::PAIR_RESULT, protocol::now(), false, &payload)).await;
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    }

    fn judge(&self, req: PairRequest, fp_device: &[u8; 32], peer: &str) -> PairResult {
        if req.v != 1 {
            return PairResult::refused("closed");
        }
        let name = clean(&req.name, "a device");
        let fp_mac = self.identity.fingerprint;
        let mut window = self.window.lock().unwrap();
        let mut asks = self.asks.lock().unwrap();
        let now = self.started.elapsed().as_secs_f64();
        let source = peer.rsplit_once(':').map_or(peer, |(ip, _)| ip);
        if window.is_none() {
            asks.asker = None;
        }
        if window.as_ref().is_some_and(|w| w.expired()) {
            *window = None;
            asks.asker = None;
            self.note.close();
            println!("Pairing: the code ran out.");
        }
        match req.method.as_str() {
            "ask" => {
                match window.as_ref() {
                    // Another key's ask while a window is up: the code already shows, no new notice.
                    Some(w) => {
                        if asks.asked_by(fp_device) {
                            self.show_code(w, &name, peer, true)
                        }
                    }
                    None if asks.refuses(fp_device, source, now) => {
                        println!("Pairing: {name} at {peer} asked; not shown (too many asks, or too many wrong codes, in 10 minutes).");
                        return PairResult::refused("openOnMac");
                    }
                    None => {
                        let w = Window::open(&ring::rand::SystemRandom::new(), &fp_mac, name.clone());
                        self.show_code(&w, &name, peer, false);
                        *window = Some(w);
                        asks.opened(fp_device, source, now);
                    }
                }
                PairResult::refused("shown")
            }
            "cancel" => {
                if window.is_some() && asks.asked_by(fp_device) {
                    *window = None;
                    asks.asker = None;
                    self.note.close();
                    println!("Pairing: {name} at {peer} no longer needs its code; the window closed.");
                }
                PairResult::refused("closed")
            }
            method => {
                let Some(w) = window.as_mut() else { return PairResult::refused("closed") };
                let given = b64url_decode(&req.proof).unwrap_or_default();
                match w.try_proof(method, &given, fp_device, &fp_mac) {
                    Verdict::Accept { proof_m } => {
                        *window = None;
                        asks.asker = None;
                        self.note.close();
                        let device = PairedDevice {
                            fingerprint: b64url(fp_device),
                            name: name.clone(),
                            model: req.model.as_deref().map(|m| clean(m, "")).filter(|m| !m.is_empty()),
                            paired_at: protocol::now(),
                            method: method.to_string(),
                        };
                        let mut paired = self.paired.lock().unwrap();
                        paired.retain(|d| d.fingerprint != device.fingerprint);
                        paired.push(device);
                        if let Err(e) = self.identity.save_paired(&paired) {
                            eprintln!("Pairing: could not save the paired devices: {e}");
                        }
                        println!(
                            "Paired {name} (key {}…) from {peer}, with {}.",
                            &b64url(fp_device)[..8],
                            if method == "qr" { "the QR code" } else { "the code" }
                        );
                        PairResult {
                            ok: true,
                            proof: Some(b64url(&proof_m)),
                            mac_id: Some(self.identity.mac_id.clone()),
                            name: Some(self.host_name.clone()),
                            recognition_key: Some(b64url(&self.identity.recognition_key)),
                            ..Default::default()
                        }
                    }
                    Verdict::Reject { tries_left } => {
                        println!("Pairing: a wrong code from {peer} ({tries_left} tr{} left).", if tries_left == 1 { "y" } else { "ies" });
                        PairResult { reason: Some("code".into()), tries_left: Some(tries_left), ..Default::default() }
                    }
                    Verdict::Busy { retry_after } => {
                        PairResult { reason: Some("busy".into()), retry_after: Some((retry_after * 10.0).ceil() / 10.0), ..Default::default() }
                    }
                    Verdict::Closed(reason) => {
                        *window = None;
                        asks.asker = None;
                        self.note.close();
                        if reason == "stopped" {
                            asks.stopped(fp_device, source, now);
                            println!("Pairing stopped after {} wrong codes.", pairing::MAX_FAILURES);
                        }
                        PairResult::refused(reason)
                    }
                }
            }
        }
    }

    /// The code (and the QR code, when `qrencode` is installed) on this terminal.
    fn show_code(&self, w: &Window, name: &str, peer: &str, again: bool) {
        let left = pairing::WINDOW_LIFETIME.saturating_sub(w.opened.elapsed()).as_secs();
        println!();
        println!(
            "Pairing: {name} at {peer} {}. On it, enter this code ({}:{:02} left):",
            if again { "asks again" } else { "asks to pair" },
            left / 60,
            left % 60
        );
        println!();
        println!("        {}", pairing::grouped(&w.code));
        println!();
        let link = self.pair_link(&w.secret);
        if let Ok(out) = std::process::Command::new("qrencode").args(["-t", "ANSIUTF8", "-m", "1", &link]).output() {
            if out.status.success() {
                println!("or scan this with the Sill app's camera:");
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
        }
        if let Some(path) = &self.code_file {
            let _ = std::fs::write(path, &w.code);
        }
        self.note.show(
            &format!("{name} wants to pair with {}", self.host_name),
            &format!("Enter this code on it: {}  ({}:{:02} left)", pairing::grouped(&w.code), left / 60, left % 60),
        );
    }

    /// `sill://pair?v=1&m=…&k=…&s=…&n=…&p=…&a=…`, as `PairLink.url`.
    fn pair_link(&self, secret: &[u8]) -> String {
        let mut items = vec![
            ("v", "1".to_string()),
            ("m", self.identity.mac_id.clone()),
            ("k", self.identity.fingerprint_b64()),
            ("s", b64url(secret)),
            ("n", self.host_name.clone()),
            ("p", self.port.to_string()),
        ];
        for ip in local_ipv4s().into_iter().take(5) {
            items.push(("a", format!("{ip}:{}", self.port)));
        }
        let query: Vec<String> = items.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
        format!("sill://pair?{}", query.join("&"))
    }
}

/// Sill's `AskLimits`: at most 3 windows opened by devices' asks in 10 minutes, and none for 10
/// minutes to the key or the address that sent the fifth wrong code (Sill quiets the window's asker;
/// here the guesser, so a stranger's wrong codes never silence the owner's device). Pure: `now` is
/// passed in, in seconds of a monotonic clock.
#[derive(Default)]
struct AskLimits {
    opened: Vec<f64>,
    quiet: Vec<([u8; 32], String, f64)>,
    /// The key and address whose ask opened the window that is up.
    asker: Option<([u8; 32], String)>,
}

impl AskLimits {
    const SPAN: f64 = 600.0;
    const MAX_WINDOWS: usize = 3;

    fn refuses(&mut self, key: &[u8; 32], source: &str, now: f64) -> bool {
        self.opened.retain(|t| now - t < Self::SPAN);
        self.quiet.retain(|q| now - q.2 < Self::SPAN);
        self.opened.len() >= Self::MAX_WINDOWS || self.quiet.iter().any(|(k, s, _)| k == key || s == source)
    }

    fn opened(&mut self, key: &[u8; 32], source: &str, now: f64) {
        self.opened.push(now);
        self.asker = Some((*key, source.into()));
    }

    fn asked_by(&self, key: &[u8; 32]) -> bool {
        self.asker.as_ref().is_some_and(|(k, _)| k == key)
    }

    fn stopped(&mut self, key: &[u8; 32], source: &str, now: f64) {
        self.quiet.push((*key, source.into(), now));
    }
}

/// Sill's `PairLink.escape`: everything but RFC 3986's unreserved characters and ":".
fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~:".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A device's name for the log and the trust list: control characters out, at most 64 characters.
pub fn clean(s: &str, fallback: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string();
    if t.is_empty() { fallback.to_string() } else { t }
}

/// Whether a connection from `ip` may reach the door at all, as Sill's `OriginPolicy` keeps its home
/// door to the Mac's own networks: loopback, link-local, private and CGNAT addresses (a home LAN,
/// Tailscale's 100.64/10 and fd7a:…), IPv6 ULA, or any address in a subnet of this machine's own
/// interfaces. Anything else (the internet, through a forwarded port or a global IPv6 address) is
/// closed before TLS.
pub fn origin_allowed(ip: std::net::IpAddr, subnets: &[(std::net::IpAddr, u32)]) -> bool {
    use std::net::IpAddr;
    let ip = ip.to_canonical();
    let near = match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_link_local() || v.is_private() || (v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64),
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xFFC0) == 0xFE80 || (v.segments()[0] & 0xFE00) == 0xFC00,
    };
    near || subnets.iter().any(|&(net, prefix)| match (ip, net.to_canonical()) {
        (IpAddr::V4(a), IpAddr::V4(n)) => (1..=32).contains(&prefix) && (u32::from(a) ^ u32::from(n)) >> (32 - prefix) == 0,
        (IpAddr::V6(a), IpAddr::V6(n)) => (1..=128).contains(&prefix) && (u128::from(a) ^ u128::from(n)) >> (128 - prefix) == 0,
        _ => false,
    })
}

/// This machine's interface addresses with their prefix lengths.
pub fn local_subnets() -> Vec<(std::net::IpAddr, u32)> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() {
                match (*ifa.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                        out.push((std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into(), m.sin_addr.s_addr.count_ones()));
                    }
                    libc::AF_INET6 => {
                        let a = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in6);
                        let ones: u32 = m.sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum();
                        out.push((std::net::Ipv6Addr::from(a.sin6_addr.s6_addr).into(), ones));
                    }
                    _ => {}
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// This machine's IPv4 addresses other than loopback, for the pairing link.
pub fn local_ipv4s() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_result_json() {
        assert_eq!(serde_json::to_string(&PairResult::refused("shown")).unwrap(), r#"{"ok":false,"reason":"shown"}"#);
        let r = PairResult { reason: Some("code".into()), tries_left: Some(4), ..Default::default() };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"ok":false,"reason":"code","triesLeft":4}"#);
        let ok = PairResult { ok: true, proof: Some("p".into()), mac_id: Some("M".into()), name: Some("n".into()), recognition_key: Some("r".into()), ..Default::default() };
        assert_eq!(serde_json::to_string(&ok).unwrap(), r#"{"ok":true,"proof":"p","macID":"M","name":"n","recognitionKey":"r"}"#);
    }

    #[test]
    fn link_escaping() {
        assert_eq!(escape("Tom & Jerry"), "Tom%20%26%20Jerry");
        assert_eq!(escape("10.0.0.2:47460"), "10.0.0.2:47460");
    }

    #[test]
    fn ask_limits() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut l = AskLimits::default();
        for i in 0..3 {
            assert!(!l.refuses(&a, "10.0.0.2", i as f64));
            l.opened(&a, "10.0.0.2", i as f64);
        }
        assert!(l.asked_by(&a) && !l.asked_by(&b), "only the asker may cancel");
        assert!(l.refuses(&b, "10.0.0.3", 10.0), "three windows in 10 minutes");
        assert!(!l.refuses(&b, "10.0.0.3", 600.5));
        l.stopped(&b, "10.0.0.3", 601.0);
        assert!(l.refuses(&b, "10.0.0.9", 602.0), "the key stays quiet");
        assert!(l.refuses(&[3u8; 32], "10.0.0.3", 602.0), "and so does the address");
        assert!(!l.refuses(&a, "10.0.0.2", 602.0), "the owner does not");
        assert!(!l.refuses(&b, "10.0.0.3", 1201.5));
    }

    #[test]
    fn origins() {
        let ok = |a: &str, nets: &[(&str, u32)]| {
            let nets: Vec<_> = nets.iter().map(|(n, p)| (n.parse().unwrap(), *p)).collect();
            origin_allowed(a.parse().unwrap(), &nets)
        };
        for a in ["127.0.0.1", "::1", "192.168.1.20", "10.1.2.3", "172.16.0.9", "169.254.3.4", "100.101.102.103",
                  "fe80::1", "fd7a:115c:a1e0::5", "::ffff:192.168.1.20"] {
            assert!(ok(a, &[]), "{a}");
        }
        for a in ["8.8.8.8", "100.128.0.1", "2001:db8::5", "::ffff:8.8.8.8"] {
            assert!(!ok(a, &[]), "{a}");
        }
        assert!(ok("203.0.113.7", &[("203.0.113.1", 24)]), "a public LAN subnet of this machine");
        assert!(!ok("203.0.114.7", &[("203.0.113.1", 24)]));
        assert!(ok("2001:db8:1:2::9", &[("2001:db8:1:2::1", 64)]));
        assert!(!ok("2001:db8:1:3::9", &[("2001:db8:1:2::1", 64)]));
        assert!(!ok("8.8.8.8", &[("0.0.0.0", 0)]), "a /0 never admits everything");
    }
}
