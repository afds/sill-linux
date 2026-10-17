//! Who moves this machine's pointer, and where it is in the streamed picture: Sill for Mac's
//! `PointerControl` (kind 26, `MacPointer`), ported. Pure.
//!
//! A device's input (kind 8) hands it the pointer; one that can move it (pointer or scroll) opens a
//! settle of 0.25 s, during which moves are taken for the device's own. After it, a read at least
//! 0.5 px from the last position that counted is this machine's mouse: the host has the pointer,
//! and every device is told where it is.

use crate::input::InputEvent;

const SETTLE: f64 = 0.25;
const MIN_MOVE: f64 = 0.5;
const STILL_AFTER: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Controller {
    Host,
    Client(u64),
}

#[derive(Debug)]
pub struct PointerControl {
    pub controller: Controller,
    host_until: f64,
    last: Option<(f64, f64)>,
    previous: Option<(f64, f64)>,
    previous_at: f64,
    changed_at: f64,
}

impl Default for PointerControl {
    fn default() -> Self {
        PointerControl { controller: Controller::Host, host_until: f64::NEG_INFINITY, last: None, previous: None, previous_at: f64::NEG_INFINITY, changed_at: f64::NEG_INFINITY }
    }
}

impl PointerControl {
    pub fn input_arrived(&mut self, client: u64, moves_pointer: bool, now: f64) {
        self.controller = Controller::Client(client);
        if moves_pointer {
            self.host_until = self.host_until.max(now + SETTLE);
        }
    }

    /// One read of the pointer (global logical pixels); true when the host took the pointer.
    pub fn read(&mut self, p: (f64, f64), now: f64) -> bool {
        let watched = now - self.previous_at <= SETTLE;
        if watched && self.previous.is_some_and(|b| b != p) {
            self.changed_at = now;
        }
        self.previous = Some(p);
        self.previous_at = now;
        let Some(from) = self.last.filter(|_| watched) else {
            self.last = Some(p);
            return false;
        };
        if now < self.host_until {
            self.last = Some(p);
            return false;
        }
        if (p.0 - from.0).hypot(p.1 - from.1) < MIN_MOVE {
            return false;
        }
        self.last = Some(p);
        let handed = self.controller != Controller::Host;
        self.controller = Controller::Host;
        handed
    }

    pub fn client_left(&mut self, client: u64) {
        if self.controller == Controller::Client(client) {
            self.controller = Controller::Host;
        }
    }

    pub fn is_moving(&self, now: f64) -> bool {
        now - self.changed_at <= STILL_AFTER
    }
}

pub fn moves_pointer(e: &InputEvent) -> bool {
    !matches!(e, InputEvent::Text(_) | InputEvent::Key { .. })
}

fn rounded(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// `p` in `rect` (x, y, w, h) as kind 26's fractions, and whether it is inside (far edges out).
pub fn fraction(p: (f64, f64), rect: (f64, f64, f64, f64)) -> (f64, f64, bool) {
    let (rx, ry, w, h) = rect;
    if !(w > 0.0 && h > 0.0 && [rx, ry, w, h, p.0, p.1].iter().all(|v| v.is_finite())) {
        return (0.0, 0.0, false);
    }
    let (x, y) = ((p.0 - rx) / w, (p.1 - ry) / h);
    (rounded(x), rounded(y), (0.0..1.0).contains(&x) && (0.0..1.0).contains(&y))
}

/// The kind 26 payload.
pub fn payload(at: Option<(f64, f64)>, seen: u64) -> Vec<u8> {
    match at {
        Some((x, y)) => serde_json::to_vec(&serde_json::json!({"inside": true, "seen": seen, "x": x, "y": y})),
        None => serde_json::to_vec(&serde_json::json!({"inside": false, "seen": seen})),
    }
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_moves_it() {
        let mut c = PointerControl::default();
        assert!(!c.read((100.0, 100.0), 0.0), "the first read only sets the position");
        c.input_arrived(7, true, 0.01);
        assert_eq!(c.controller, Controller::Client(7));
        assert!(!c.read((150.0, 100.0), 0.03), "inside the settle: the device's move");
        assert!(!c.read((150.0, 100.0), 0.3));
        assert!(c.read((151.0, 100.0), 0.33), "after it, the host's mouse");
        assert_eq!(c.controller, Controller::Host);
        // A slow drag adds up from the last position that counted.
        c.input_arrived(7, false, 0.34);
        assert!(!c.read((151.3, 100.0), 0.36));
        assert!(c.read((151.6, 100.0), 0.39));
        // A gap of more than the settle only resets.
        c.input_arrived(7, false, 0.4);
        assert!(!c.read((300.0, 300.0), 1.0));
        assert_eq!(c.controller, Controller::Client(7));
        c.client_left(7);
        assert_eq!(c.controller, Controller::Host);
        // Keys hand the pointer over but open no settle.
        assert!(!moves_pointer(&InputEvent::Text("a".into())));
    }

    #[test]
    fn fractions() {
        assert_eq!(fraction((960.0, 540.0), (0.0, 0.0, 1920.0, 1080.0)), (0.5, 0.5, true));
        assert_eq!(fraction((1920.0, 10.0), (0.0, 0.0, 1920.0, 1080.0)).2, false, "the far edge is out");
        assert_eq!(fraction((100.0, 100.0), (200.0, 200.0, 400.0, 300.0)), (-0.25, -0.3333, false));
        assert_eq!(fraction((1.0, 1.0), (0.0, 0.0, 0.0, 10.0)).2, false);
        assert_eq!(String::from_utf8(payload(None, 3)).unwrap(), r#"{"inside":false,"seen":3}"#);
    }
}
