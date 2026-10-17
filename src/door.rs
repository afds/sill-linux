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
//!   - "cancel": the window closes; "closed".
//!   - any `v` but 1: "closed", no try counted.

use crate::identity::{Identity, PairedDevice};
use crate::pairing::{self, b64url, b64url_decode, Verdict, Window};
use crate::protocol::{self, kind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
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
        let request = match tokio::time::timeout(PAIRING_READ_TIMEOUT, crate::server::read_message(&mut stream)).await {
            Ok(Ok((h, p))) if h.kind == kind::PAIR_REQUEST && p.len() <= MAX_PAIRING_PAYLOAD => serde_json::from_slice::<PairRequest>(&p).ok(),
            _ => None,
        };
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
        if window.as_ref().is_some_and(|w| w.expired()) {
            *window = None;
            println!("Pairing: the code ran out.");
        }
        match req.method.as_str() {
            "ask" => {
                match window.as_ref() {
                    Some(w) => self.show_code(w, &name, peer, true),
                    None => {
                        let w = Window::open(&ring::rand::SystemRandom::new(), &fp_mac, name.clone());
                        self.show_code(&w, &name, peer, false);
                        *window = Some(w);
                    }
                }
                PairResult::refused("shown")
            }
            "cancel" => {
                if window.take().is_some() {
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
                        if reason == "stopped" {
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
fn clean(s: &str, fallback: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string();
    if t.is_empty() { fallback.to_string() } else { t }
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
}
