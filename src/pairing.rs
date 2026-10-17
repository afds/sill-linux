//! Sill's pairing crypto (`Pairing.swift`, `RemoteIdentity.swift`) and the host's pairing window
//! (`PairingWindow.swift`), checked against the vectors in Sill's `Tests/checks/protocol`.
//!
//! - Fingerprint: SHA-256 of a P-256 key's SubjectPublicKeyInfo DER; the Mac ID is Crockford
//!   base32 of its first 10 bytes.
//! - Typed code: 11 random digits and a Damm check digit. K = PBKDF2-HMAC-SHA256(code,
//!   "sill-pair-v1" ‖ fp_mac, 600 000 rounds, 32 bytes); the QR path's K is the link's secret.
//! - proof_D = HMAC(K, "sill-pair-v1 device" ‖ 0 ‖ fp_device ‖ fp_mac); proof_M swaps the label
//!   to "mac" and the fingerprints. Both fingerprints come from the TLS session.
//! - Recognition tag (TXT `r`): base64url(n6 ‖ HMAC(recognitionKey, "sill-tag-v1" ‖ n6)[..6]).

use ring::{digest, hmac, pbkdf2, rand::SecureRandom};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

const LABEL: &str = "sill-pair-v1";
pub const ROUNDS: u32 = 600_000;

/// base64url without padding (RFC 4648 §5).
pub fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Plain base64 with padding (RFC 4648 §4), as Foundation's `base64EncodedString`.
pub fn b64(data: &[u8]) -> String {
    let mut out: String = b64url(data).chars().map(|c| match c { '-' => '+', '_' => '/', c => c }).collect();
    while out.len() % 4 != 0 {
        out.push('=');
    }
    out
}

/// Decodes canonical unpadded base64url; None for anything else.
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    };
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    (b64url(&out) == text).then_some(out)
}

/// The SubjectPublicKeyInfo DER of a 65-byte uncompressed P-256 point, as `SPKI.der(point:)`.
pub fn spki_der(point: &[u8]) -> Vec<u8> {
    let mut d = vec![0x30, 0x59, 0x30, 0x13];
    d.extend_from_slice(&[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]); // ecPublicKey
    d.extend_from_slice(&[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]); // prime256v1
    d.extend_from_slice(&[0x03, 0x42, 0x00]);
    d.extend_from_slice(point);
    d
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, data).as_ref().try_into().unwrap()
}

/// The fingerprint every pin compares: SHA-256 of the key's SPKI.
pub fn fingerprint(point: &[u8]) -> [u8; 32] {
    sha256(&spki_der(point))
}

/// The Mac ID: Crockford base32 of the fingerprint's first 10 bytes, 16 characters.
pub fn mac_id(fingerprint: &[u8]) -> String {
    const A: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let (mut buffer, mut bits, mut out) = (0u32, 0u32, String::new());
    for &b in &fingerprint[..10] {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((buffer >> bits) & 31) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    out
}

/// Damm's quasigroup (`PairingCode.table`).
const DAMM: [[u8; 10]; 10] = [
    [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
    [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
    [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
    [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
    [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
    [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
    [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
    [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
    [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
    [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
];

pub fn check_digit(digits: &[u8]) -> u8 {
    digits.iter().fold(0, |i, &d| DAMM[i as usize][d as usize])
}

/// A fresh 12-digit code: 11 uniform random digits (rejection sampling) and the check digit.
pub fn generate_code(rng: &dyn SecureRandom) -> String {
    let mut digits = Vec::with_capacity(12);
    while digits.len() < 11 {
        let mut b = [0u8; 16];
        rng.fill(&mut b).expect("random source");
        digits.extend(b.iter().filter(|&&x| x < 250).map(|x| x % 10).take(11 - digits.len()));
    }
    digits.push(check_digit(&digits));
    digits.iter().map(|d| char::from(b'0' + d)).collect()
}

/// "4829 1355 7208".
pub fn grouped(code: &str) -> String {
    code.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" ")
}

/// The typed path's key.
pub fn code_key(code: &str, mac_fingerprint: &[u8]) -> [u8; 32] {
    let mut salt = LABEL.as_bytes().to_vec();
    salt.extend_from_slice(mac_fingerprint);
    let mut out = [0u8; 32];
    pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, NonZeroU32::new(ROUNDS).unwrap(), &salt, code.as_bytes(), &mut out);
    out
}

fn proof(key: &[u8], role: &str, first: &[u8], second: &[u8]) -> [u8; 32] {
    let mut m = format!("{LABEL} {role}").into_bytes();
    m.push(0);
    m.extend_from_slice(first);
    m.extend_from_slice(second);
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), &m).as_ref().try_into().unwrap()
}

pub fn device_proof(key: &[u8], fp_device: &[u8], fp_mac: &[u8]) -> [u8; 32] {
    proof(key, "device", fp_device, fp_mac)
}

pub fn mac_proof(key: &[u8], fp_mac: &[u8], fp_device: &[u8]) -> [u8; 32] {
    proof(key, "mac", fp_mac, fp_device)
}

/// The Mac's check of proof_D, in constant time.
pub fn is_valid_device_proof(given: &[u8], key: &[u8], fp_device: &[u8], fp_mac: &[u8]) -> bool {
    let mut m = format!("{LABEL} device").into_bytes();
    m.push(0);
    m.extend_from_slice(fp_device);
    m.extend_from_slice(fp_mac);
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), &m, given).is_ok()
}

/// The TXT record's `r` for `recognition_key`, with a fresh nonce (or the given one, for tests).
pub fn recognition_tag(recognition_key: &[u8], nonce: [u8; 6]) -> String {
    let mut m = b"sill-tag-v1".to_vec();
    m.extend_from_slice(&nonce);
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, recognition_key), &m);
    let mut out = nonce.to_vec();
    out.extend_from_slice(&tag.as_ref()[..6]);
    b64url(&out)
}

// MARK: - The pairing window

pub const WINDOW_LIFETIME: Duration = Duration::from_secs(300);
pub const MAX_FAILURES: u32 = 5;
const SPACING: [u64; 4] = [1, 2, 4, 8];

/// One open window: the code and the QR secret, until it is used, runs out, or five wrong codes.
pub struct Window {
    pub code: String,
    pub secret: [u8; 16],
    pub code_key: [u8; 32],
    pub opened: Instant,
    pub requested_by: String,
    failures: u32,
    next_allowed: Instant,
}

/// What a proof gets (PairingWindow's verdicts).
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Accept { proof_m: [u8; 32] },
    Reject { tries_left: u32 },
    Busy { retry_after: f64 },
    Closed(&'static str), // "closed", "expired" or "stopped"
}

impl Window {
    pub fn open(rng: &dyn SecureRandom, mac_fingerprint: &[u8], requested_by: String) -> Window {
        let code = generate_code(rng);
        let mut secret = [0u8; 16];
        rng.fill(&mut secret).expect("random source");
        let now = Instant::now();
        Window {
            code_key: code_key(&code, mac_fingerprint),
            code,
            secret,
            opened: now,
            requested_by,
            failures: 0,
            next_allowed: now,
        }
    }

    pub fn expired(&self) -> bool {
        self.opened.elapsed() >= WINDOW_LIFETIME
    }

    /// Judges one proof; the caller closes the window on Accept, on "expired" and on "stopped".
    pub fn try_proof(&mut self, method: &str, given: &[u8], fp_device: &[u8], fp_mac: &[u8]) -> Verdict {
        if self.expired() {
            return Verdict::Closed("expired");
        }
        let now = Instant::now();
        if now < self.next_allowed {
            return Verdict::Busy { retry_after: (self.next_allowed - now).as_secs_f64() };
        }
        let key: Option<&[u8]> = match method {
            "qr" => Some(&self.secret),
            "code" => Some(&self.code_key),
            _ => None, // an unknown method never matches: a wrong proof
        };
        if let Some(key) = key
            && is_valid_device_proof(given, key, fp_device, fp_mac) {
                return Verdict::Accept { proof_m: mac_proof(key, fp_mac, fp_device) };
            }
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            return Verdict::Closed("stopped");
        }
        self.next_allowed = now + Duration::from_secs(SPACING[(self.failures as usize - 1).min(3)]);
        Verdict::Reject { tries_left: MAX_FAILURES - self.failures }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // The vectors of Sill's Tests/checks/protocol/main.swift ("§3.6 vectors, computed in Python").
    #[test]
    fn sill_vectors() {
        let fp_mac = sha256(b"mac");
        let fp_dev = sha256(b"device");
        assert_eq!(hex(&fp_mac), "348a629f5ceed032c3e8706ec47d9bfafb00fb4250b018dd965435ca50cb836e");
        let k = code_key("482913557208", &fp_mac);
        assert_eq!(hex(&k), "e9baf879a1cc4ac8b21554df480b6c1621671319107c91e7cb05d154f649d31d");
        assert_eq!(b64url(&device_proof(&k, &fp_dev, &fp_mac)), "dk7PZj4vHsAxj--1u9Vg7ehSn8a1tWc0RCG46YpFVO8");
        assert_eq!(b64url(&mac_proof(&k, &fp_mac, &fp_dev)), "6BmGWC9XT8J8OY8phhq9bASgoSnoWpRxBeqZtvRrWmk");
        let qr = [0x22u8; 16];
        assert_eq!(b64url(&device_proof(&qr, &fp_dev, &fp_mac)), "AyZAJqNTU5nSKEaHx6RZOf0K5dOeygR_8JSrCV_gNOo");
        assert_eq!(b64url(&mac_proof(&qr, &fp_mac, &fp_dev)), "XAX-uZPsq_Ax8fvI_sV7Ex0a-ROi19rI-FW-ykPh9TE");
        assert!(is_valid_device_proof(&device_proof(&qr, &fp_dev, &fp_mac), &qr, &fp_dev, &fp_mac));
        assert!(!is_valid_device_proof(&mac_proof(&qr, &fp_mac, &fp_dev), &qr, &fp_dev, &fp_mac));

        assert_eq!(mac_id(&sha256(b"example")), "A3C5HR4RBV67YR21");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(mac_id(&[0u8; 32]), "0000000000000000");
        assert_eq!(mac_id(&[0xffu8; 32]), "ZZZZZZZZZZZZZZZZ");

        let tag = recognition_tag(&[0x11u8; 32], [0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6]);
        assert_eq!(tag, "oaKjpKWmL4V47Ctm");
    }

    #[test]
    fn codes() {
        assert_eq!(check_digit(&[4, 8, 2, 9, 1, 3, 5, 5, 7, 2, 0, 8]), 0);
        assert_ne!(check_digit(&[4, 8, 2, 9, 1, 3, 5, 5, 7, 2, 0, 9]), 0);
        let rng = ring::rand::SystemRandom::new();
        for _ in 0..200 {
            let c = generate_code(&rng);
            assert_eq!(c.len(), 12);
            let d: Vec<u8> = c.bytes().map(|b| b - b'0').collect();
            assert_eq!(check_digit(&d), 0);
        }
        assert_eq!(grouped("482913557208"), "4829 1355 7208");
    }

    #[test]
    fn base64url() {
        assert_eq!(b64url(&[0u8; 32]).len(), 43);
        assert_eq!(b64url(&[0u8; 16]).len(), 22);
        for n in 0..40u8 {
            let v: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            assert_eq!(b64url_decode(&b64url(&v)).unwrap(), v);
        }
        assert!(b64url_decode("AAAA=").is_none());
        assert!(b64url_decode("ab+c").is_none());
        assert!(b64url_decode("abcde").is_none());
        assert!(b64url_decode("AB").is_none()); // non-canonical trailing bits
    }

    #[test]
    fn window_rules() {
        let rng = ring::rand::SystemRandom::new();
        let (fp_mac, fp_dev) = (sha256(b"mac"), sha256(b"device"));
        let mut w = Window::open(&rng, &fp_mac, "iPad".into());
        assert_eq!(w.try_proof("code", &[0u8; 32], &fp_dev, &fp_mac), Verdict::Reject { tries_left: 4 });
        assert!(matches!(w.try_proof("code", &[0u8; 32], &fp_dev, &fp_mac), Verdict::Busy { .. }));
        w.next_allowed = Instant::now();
        let good = device_proof(&w.code_key, &fp_dev, &fp_mac);
        match w.try_proof("code", &good, &fp_dev, &fp_mac) {
            Verdict::Accept { proof_m } => assert_eq!(proof_m, mac_proof(&w.code_key, &fp_mac, &fp_dev)),
            other => panic!("{other:?}"),
        }
        let qr = device_proof(&w.secret, &fp_dev, &fp_mac);
        assert!(matches!(w.try_proof("qr", &qr, &fp_dev, &fp_mac), Verdict::Accept { .. }));
        let mut s = Window::open(&rng, &fp_mac, "x".into());
        for left in (1..5).rev() {
            s.next_allowed = Instant::now();
            assert_eq!(s.try_proof("code", &[1u8; 32], &fp_dev, &fp_mac), Verdict::Reject { tries_left: left });
        }
        s.next_allowed = Instant::now();
        assert_eq!(s.try_proof("code", &[1u8; 32], &fp_dev, &fp_mac), Verdict::Closed("stopped"));
    }
}
