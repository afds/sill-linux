//! The Sill app's input (kind 8, `Input.swift`) as Linux input: what to tell the RemoteDesktop
//! portal, decided here without the portal so it can be tested. Pure; the portal side executes the
//! `Action`s in order (`capture::run_input`).
//!
//! - Positions are 0…1 fractions of the streamed frame, given to `NotifyPointerMotionAbsolute` in
//!   the stream's own pixels (the portal's "stream coordinate space": on GNOME at scale 2 that is
//!   3840×2160 for a monitor it calls 1920×1080; measured, 2026-10-17).
//! - Scroll deltas are fractions of the frame with natural sign (positive dy: the content moves
//!   down, as Sill's Mac host posts them); Wayland's positive vertical axis scrolls down, so both
//!   are negated. Inside a gesture the axis stop comes at the end of the device's own momentum, or
//!   0.12 s after the fingers lift when no momentum follows, so toolkits that coast after a stop
//!   (GTK 4) do not coast on top of the device's coast.
//! - Keys are USB HID usages, sent as evdev keycodes; the Command key is Ctrl by default (so ⌘C
//!   copies, as on the Mac) or Super, and ⌘Space, the Sill app's Spotlight key, taps Super
//!   (GNOME's Activities, KDE's launcher). Modifier bits on a key whose modifier key is not down
//!   (an older device) press that modifier around the key.
//! - Text is typed as keysyms with no modifier held (held keys go up first, as on the Mac);
//!   "\n", "\t" and "\u{8}" are Return, Tab and Backspace.
//! - A device that leaves lets go of every key and button it holds.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const BTN_LEFT: i32 = 0x110;
pub const BTN_RIGHT: i32 = 0x111;
const KEY_LEFTCTRL: i32 = 29;
const KEY_LEFTSHIFT: i32 = 42;
const KEY_LEFTALT: i32 = 56;
const KEY_LEFTMETA: i32 = 125;
const KEY_ENTER: i32 = 28;
const KEY_TAB: i32 = 15;
const KEY_BACKSPACE: i32 = 14;
#[cfg(test)]
const KEY_SPACE: i32 = 57;

const MOD_SHIFT: u64 = 1 << 17;
const MOD_CONTROL: u64 = 1 << 18;
const MOD_OPTION: u64 = 1 << 19;
const MOD_COMMAND: u64 = 1 << 20;

/// No momentum 0.12 s after the fingers lift: the gesture is over.
const STOP_AFTER_LIFT: Duration = Duration::from_millis(120);

/// One `InputEvent` as Swift's Codable writes it.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Pointer { action: PointerAction, x: f64, y: f64 },
    Scroll { x: f64, y: f64, dx: f64, dy: f64 },
    Text(String),
    ScrollGesture { phase: ScrollPhase, x: f64, y: f64 },
    Key { hid_usage: u16, down: bool, modifiers: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PointerAction {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
    RightUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScrollPhase {
    Began,
    Ended,
    MomentumBegan,
    MomentumEnded,
}

impl InputEvent {
    /// `{"pointer":{"_0":"leftDown","x":0.5,"y":0.5}}`, `{"scroll":{"x":…,"y":…,"dx":…,"dy":…}}`,
    /// `{"text":{"_0":"a"}}`, `{"scrollGesture":{"_0":"began","x":…,"y":…}}`,
    /// `{"key":{"hidUsage":4,"down":true,"modifiers":0}}`. None for anything else.
    pub fn parse(json: &[u8]) -> Option<InputEvent> {
        let v: serde_json::Value = serde_json::from_slice(json).ok()?;
        let (case, body) = v.as_object().filter(|o| o.len() == 1)?.iter().next()?;
        let f = |k: &str| body.get(k).and_then(|x| x.as_f64()).filter(|x| x.is_finite());
        let label = || body.get("_0");
        Some(match case.as_str() {
            "pointer" => InputEvent::Pointer { action: serde_json::from_value(label()?.clone()).ok()?, x: f("x")?, y: f("y")? },
            "scroll" => InputEvent::Scroll { x: f("x")?, y: f("y")?, dx: f("dx")?, dy: f("dy")? },
            "text" => InputEvent::Text(label()?.as_str()?.to_string()),
            "scrollGesture" => InputEvent::ScrollGesture { phase: serde_json::from_value(label()?.clone()).ok()?, x: f("x")?, y: f("y")? },
            "key" => InputEvent::Key {
                hid_usage: u16::try_from(body.get("hidUsage")?.as_u64()?).ok()?,
                down: body.get("down")?.as_bool()?,
                modifiers: body.get("modifiers").and_then(|m| m.as_u64()).unwrap_or(0),
            },
            _ => return None,
        })
    }
}

/// What the portal is told, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Absolute position on the stream, in its own pixels, inside the box the portal accepts.
    Motion { x: f64, y: f64 },
    /// Relative motion in logical pixels: the rest of the way to a place outside that box.
    RelMotion { dx: f64, dy: f64 },
    Button { button: i32, pressed: bool },
    /// Smooth scroll in logical pixels, Wayland's sign (positive dy scrolls down).
    Axis { dx: f64, dy: f64 },
    /// The end of a scroll gesture (`finish`).
    AxisStop,
    Key { code: i32, pressed: bool },
    Keysym { sym: i32, pressed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommandKey {
    Ctrl,
    Super,
}

pub struct Translator {
    /// The stream's size in its own pixels (the encoded picture).
    width: f64,
    height: f64,
    /// The size the portal reports for the stream (logical pixels on GNOME): the only absolute
    /// positions it accepts ("Invalid position" past it), though the compositor reads them in the
    /// stream's pixels.
    box_w: f64,
    box_h: f64,
    command: CommandKey,
    last_motion: Option<(f64, f64)>,
    /// evdev keycodes this device holds down, with how many of its keys hold each (both Ctrl and
    /// Command can be Ctrl).
    held: BTreeMap<i32, u32>,
    buttons: Vec<i32>,
    /// HID usages of Command held, for the Spotlight key.
    command_down: u32,
    /// The Spotlight key took the Command keys' Ctrl back up; their ups are then nothing.
    command_spent: bool,
    /// The Spotlight key's Space came down as Super: its up is nothing.
    space_spent: bool,
    /// Modifiers pressed for a key's modifier bits, to let go when that key comes up.
    synthetic: BTreeMap<u16, Vec<i32>>,
    in_gesture: bool,
    stop_at: Option<Instant>,
    /// Drop a tap that ends a pointer move (`keep_all_taps` off).
    filter_taps: bool,
    /// The pointer's last move, and when the moves it ends began (a gap over `STROKE_GAP` starts
    /// a new run).
    last_move: Option<Instant>,
    moves_since: Option<Instant>,
    /// A left press held back to see whether its release comes at once (a tap).
    pending_left: Option<Instant>,
    /// Taps dropped so far, for the log.
    pub dropped_taps: u64,
}

/// The Sill app's trackpad counts a slow, small nudge of the pointer followed by a lift as a tap
/// (iOS's tap recognizer allows a little movement and a long touch), so the lift clicks what the
/// pointer was just moved onto (the iPad, 2026-10-17: 650 ms of moves, the click 69 ms after the
/// last). A tap the app sends is a press and its release within a millisecond or two; one that
/// comes within `TAP_AFTER_MOVE` of moves that ran at least `MOVE_RUN` is dropped. A real tap
/// (a twitch of tens of milliseconds, or a pause first) and a drag (the app's press and hold needs
/// 0.45 s of stillness) are kept; a press with no release within `TAP_WINDOW` (the Pencil after
/// hovering) goes through that much later.
const TAP_AFTER_MOVE: Duration = Duration::from_millis(120);
const MOVE_RUN: Duration = Duration::from_millis(150);
const STROKE_GAP: Duration = Duration::from_millis(150);
const TAP_WINDOW: Duration = Duration::from_millis(40);

impl Translator {
    /// `width`×`height`: the size the portal reports for the stream; the picture's own size
    /// follows with `set_size` once frames arrive.
    pub fn new(width: f64, height: f64, command: CommandKey) -> Translator {
        Translator {
            width,
            height,
            box_w: width,
            box_h: height,
            command,
            last_motion: None,
            held: BTreeMap::new(),
            buttons: Vec::new(),
            command_down: 0,
            command_spent: false,
            space_spent: false,
            synthetic: BTreeMap::new(),
            in_gesture: false,
            stop_at: None,
            filter_taps: true,
            last_move: None,
            moves_since: None,
            pending_left: None,
            dropped_taps: 0,
        }
    }

    /// The stream's size in its own pixels, the space absolute motion is given in (GNOME at scale
    /// 2: 3840×2160 for a monitor the portal calls 1920×1080).
    /// `--keep-all-taps`: every tap the device sends clicks.
    pub fn keep_all_taps(mut self, keep: bool) -> Translator {
        self.filter_taps = !keep;
        self
    }

    /// The held-back left press goes out after all (its release did not come at once).
    fn flush_pending(&mut self, out: &mut Vec<Action>) {
        if self.pending_left.take().is_some() && !self.buttons.contains(&BTN_LEFT) {
            self.buttons.push(BTN_LEFT);
            out.push(Action::Button { button: BTN_LEFT, pressed: true });
        }
    }

    pub fn set_size(&mut self, width: f64, height: f64) {
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            self.last_motion = None;
        }
    }

    /// To `x`, `y` (fractions of the picture). GNOME at scale 2 accepts absolute positions only
    /// inside the size it reports (1920×1080) yet reads them in the stream's pixels (3840×2160),
    /// so absolute motion reaches only the top-left quarter (measured, 2026-10-17): a place past
    /// the box is reached by the nearest point inside it, then relative motion in logical pixels.
    fn motion(&mut self, x: f64, y: f64, out: &mut Vec<Action>) {
        // At most the last pixel: the portal refuses a position at the stream's width or height.
        let p = ((x.clamp(0.0, 1.0) * self.width).min(self.width - 1.0), (y.clamp(0.0, 1.0) * self.height).min(self.height - 1.0));
        if self.last_motion == Some(p) {
            return;
        }
        self.last_motion = Some(p);
        let inside = (p.0.min(self.box_w - 1.0).max(0.0), p.1.min(self.box_h - 1.0).max(0.0));
        out.push(Action::Motion { x: inside.0, y: inside.1 });
        if inside != p {
            // Stream pixels per logical pixel.
            let (sx, sy) = (self.width / self.box_w, self.height / self.box_h);
            out.push(Action::RelMotion { dx: (p.0 - inside.0) / sx, dy: (p.1 - inside.1) / sy });
        }
    }

    fn press(&mut self, code: i32, out: &mut Vec<Action>) {
        let n = self.held.entry(code).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push(Action::Key { code, pressed: true });
        }
    }

    fn release(&mut self, code: i32, out: &mut Vec<Action>) {
        if let Some(n) = self.held.get_mut(&code) {
            *n -= 1;
            if *n == 0 {
                self.held.remove(&code);
                out.push(Action::Key { code, pressed: false });
            }
        }
    }

    fn release_all_keys(&mut self, out: &mut Vec<Action>) {
        for code in std::mem::take(&mut self.held).into_keys() {
            out.push(Action::Key { code, pressed: false });
        }
        self.synthetic.clear();
        self.command_down = 0;
        self.command_spent = false;
        self.space_spent = false;
    }

    /// The evdev key a HID usage stands for here (Command per `command`).
    fn keycode(&self, usage: u16) -> Option<i32> {
        match usage {
            0xE3 | 0xE7 if self.command == CommandKey::Ctrl => Some(if usage == 0xE3 { KEY_LEFTCTRL } else { 97 }),
            _ => hid_to_evdev(usage),
        }
    }

    pub fn handle(&mut self, event: InputEvent, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        // A held-back press: its release now drops both; anything else lets the press go first.
        if let Some(at) = self.pending_left {
            let is_release = matches!(event, InputEvent::Pointer { action: PointerAction::LeftUp, .. });
            if is_release && now.duration_since(at) <= TAP_WINDOW {
                self.pending_left = None;
                self.dropped_taps += 1;
                return out;
            }
            self.flush_pending(&mut out);
        }
        match event {
            InputEvent::Pointer { action, x, y } => {
                self.motion(x, y, &mut out);
                if action == PointerAction::Move {
                    if self.last_move.is_none_or(|l| now.duration_since(l) > STROKE_GAP) {
                        self.moves_since = Some(now);
                    }
                    self.last_move = Some(now);
                }
                if action == PointerAction::LeftDown && self.filter_taps && !self.buttons.contains(&BTN_LEFT) {
                    let ends_a_move = match (self.last_move, self.moves_since) {
                        (Some(l), Some(s)) => now.duration_since(l) <= TAP_AFTER_MOVE && l.duration_since(s) >= MOVE_RUN,
                        _ => false,
                    };
                    if ends_a_move {
                        self.pending_left = Some(now);
                        return out;
                    }
                }
                let (button, pressed) = match action {
                    PointerAction::Move => return out,
                    PointerAction::LeftDown => (BTN_LEFT, true),
                    PointerAction::LeftUp => (BTN_LEFT, false),
                    PointerAction::RightDown => (BTN_RIGHT, true),
                    PointerAction::RightUp => (BTN_RIGHT, false),
                };
                if pressed && !self.buttons.contains(&button) {
                    self.buttons.push(button);
                    out.push(Action::Button { button, pressed });
                } else if !pressed && self.buttons.contains(&button) {
                    self.buttons.retain(|b| *b != button);
                    out.push(Action::Button { button, pressed });
                }
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                self.motion(x, y, &mut out);
                out.push(Action::Axis { dx: -dx * self.width, dy: -dy * self.height });
            }
            InputEvent::ScrollGesture { phase, x, y } => {
                self.motion(x, y, &mut out);
                match phase {
                    ScrollPhase::Began => {
                        self.in_gesture = true;
                        self.stop_at = None;
                    }
                    ScrollPhase::Ended => self.stop_at = Some(now + STOP_AFTER_LIFT),
                    ScrollPhase::MomentumBegan => self.stop_at = None,
                    ScrollPhase::MomentumEnded => {
                        self.stop_at = None;
                        if self.in_gesture {
                            self.in_gesture = false;
                            out.push(Action::AxisStop);
                        }
                    }
                }
            }
            InputEvent::Text(text) => {
                self.release_all_keys(&mut out);
                for c in text.chars() {
                    let tap = |out: &mut Vec<Action>, code: i32| {
                        out.push(Action::Key { code, pressed: true });
                        out.push(Action::Key { code, pressed: false });
                    };
                    match c {
                        '\n' | '\r' => tap(&mut out, KEY_ENTER),
                        '\t' => tap(&mut out, KEY_TAB),
                        '\u{8}' => tap(&mut out, KEY_BACKSPACE),
                        c if c.is_control() => {}
                        c => {
                            let sym = keysym(c);
                            out.push(Action::Keysym { sym, pressed: true });
                            out.push(Action::Keysym { sym, pressed: false });
                        }
                    }
                }
            }
            InputEvent::Key { hid_usage, down, modifiers } => self.key(hid_usage, down, modifiers, &mut out),
        }
        out
    }

    fn key(&mut self, usage: u16, down: bool, modifiers: u64, out: &mut Vec<Action>) {
        let is_command = usage == 0xE3 || usage == 0xE7;
        if is_command {
            if down {
                self.command_down += 1;
            } else {
                self.command_down = self.command_down.saturating_sub(1);
            }
            if !down && self.command_spent {
                if self.command_down == 0 {
                    self.command_spent = false;
                }
                return;
            }
        }
        // The Spotlight key: ⌘Space taps Super.
        if usage == 0x2C {
            let command_held = self.command_down > 0 || modifiers & MOD_COMMAND != 0;
            if down && command_held && self.command == CommandKey::Ctrl {
                if self.command_down > 0 {
                    // The Ctrl the Command keys hold (left, right) goes up for the Super tap.
                    self.release(KEY_LEFTCTRL, out);
                    self.release(97, out);
                }
                self.command_spent = self.command_down > 0;
                self.space_spent = true;
                out.push(Action::Key { code: KEY_LEFTMETA, pressed: true });
                out.push(Action::Key { code: KEY_LEFTMETA, pressed: false });
                return;
            }
            if !down && self.space_spent {
                self.space_spent = false;
                return;
            }
        }
        let Some(code) = self.keycode(usage) else { return };
        let is_modifier = (0xE0..=0xE7).contains(&usage);
        if down {
            if !is_modifier {
                // Modifier bits whose keys are not down: press them around this key.
                let mut extra = Vec::new();
                let command_code = if self.command == CommandKey::Ctrl { KEY_LEFTCTRL } else { KEY_LEFTMETA };
                for (bit, mcode) in [(MOD_SHIFT, KEY_LEFTSHIFT), (MOD_CONTROL, KEY_LEFTCTRL), (MOD_OPTION, KEY_LEFTALT), (MOD_COMMAND, command_code)] {
                    if modifiers & bit != 0 && !self.held_any(mcode) && !extra.contains(&mcode) {
                        extra.push(mcode);
                    }
                }
                for &m in &extra {
                    self.press(m, out);
                }
                if !extra.is_empty() {
                    self.synthetic.insert(usage, extra);
                }
            }
            self.press(code, out);
        } else {
            self.release(code, out);
            if let Some(extra) = self.synthetic.remove(&usage) {
                for m in extra.into_iter().rev() {
                    self.release(m, out);
                }
            }
        }
    }

    fn held_any(&self, code: i32) -> bool {
        let right = match code {
            KEY_LEFTCTRL => 97,
            KEY_LEFTSHIFT => 54,
            KEY_LEFTALT => 100,
            KEY_LEFTMETA => 126,
            _ => code,
        };
        self.held.contains_key(&code) || self.held.contains_key(&right)
    }

    /// The axis stop owed after a lift with no momentum, and a held-back press whose release did
    /// not come.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if self.pending_left.is_some_and(|at| now.duration_since(at) > TAP_WINDOW) {
            self.flush_pending(&mut out);
        }
        if let Some(t) = self.stop_at {
            if now >= t && self.in_gesture {
                self.stop_at = None;
                self.in_gesture = false;
                out.push(Action::AxisStop);
            }
        }
        out
    }

    /// The device left: every key and button it holds goes up.
    pub fn release_all(&mut self) -> Vec<Action> {
        self.pending_left = None;
        let mut out = Vec::new();
        for button in std::mem::take(&mut self.buttons) {
            out.push(Action::Button { button, pressed: false });
        }
        self.release_all_keys(&mut out);
        if self.in_gesture {
            self.in_gesture = false;
            self.stop_at = None;
            out.push(Action::AxisStop);
        }
        out
    }
}

/// X11 keysym for a character: Latin-1 as itself, anything else as 0x01000000 + code point.
pub fn keysym(c: char) -> i32 {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) { cp as i32 } else { (0x0100_0000 | cp) as i32 }
}

/// USB HID keyboard usage → evdev keycode (Linux's `usb_kbd_keycode` table).
pub fn hid_to_evdev(usage: u16) -> Option<i32> {
    const T: [u8; 256] = [
        0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, //
        50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3, //
        4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, //
        27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, //
        65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106, //
        105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, //
        72, 73, 82, 83, 86, 127, 116, 117, 183, 184, 185, 186, 187, 188, 189, 190, //
        191, 192, 193, 194, 134, 138, 130, 132, 128, 129, 131, 137, 133, 135, 136, 113, //
        115, 114, 0, 0, 0, 121, 0, 89, 93, 124, 92, 94, 95, 0, 0, 0, //
        122, 123, 90, 91, 85, 0, 0, 0, 0, 0, 0, 0, 111, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
        29, 42, 56, 125, 97, 54, 100, 126, 164, 166, 165, 163, 161, 115, 114, 113, //
        150, 158, 159, 128, 136, 177, 178, 176, 142, 152, 173, 140, 0, 0, 0, 0, //
    ];
    T.get(usage as usize).copied().filter(|&c| c != 0).map(i32::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Translator {
        Translator::new(1920.0, 1080.0, CommandKey::Ctrl)
    }
    fn key(u: u16, down: bool) -> InputEvent {
        InputEvent::Key { hid_usage: u, down, modifiers: 0 }
    }
    fn k(code: i32, pressed: bool) -> Action {
        Action::Key { code, pressed }
    }

    #[test]
    fn parses_swifts_json() {
        assert_eq!(
            InputEvent::parse(br#"{"pointer":{"_0":"leftDown","x":0.5,"y":0.25}}"#),
            Some(InputEvent::Pointer { action: PointerAction::LeftDown, x: 0.5, y: 0.25 })
        );
        assert_eq!(
            InputEvent::parse(br#"{"scroll":{"x":0.1,"y":0.2,"dx":0,"dy":-0.01}}"#),
            Some(InputEvent::Scroll { x: 0.1, y: 0.2, dx: 0.0, dy: -0.01 })
        );
        assert_eq!(InputEvent::parse(r#"{"text":{"_0":"hé"}}"#.as_bytes()), Some(InputEvent::Text("hé".into())));
        assert_eq!(
            InputEvent::parse(br#"{"scrollGesture":{"_0":"momentumBegan","x":0,"y":1}}"#),
            Some(InputEvent::ScrollGesture { phase: ScrollPhase::MomentumBegan, x: 0.0, y: 1.0 })
        );
        assert_eq!(
            InputEvent::parse(br#"{"key":{"hidUsage":4,"down":true,"modifiers":1048576}}"#),
            Some(InputEvent::Key { hid_usage: 4, down: true, modifiers: MOD_COMMAND })
        );
        assert_eq!(InputEvent::parse(br#"{"key":{"hidUsage":70000,"down":true,"modifiers":0}}"#), None);
        assert_eq!(InputEvent::parse(br#"{"pointer":{"_0":"middleDown","x":0,"y":0}}"#), None);
        assert_eq!(InputEvent::parse(br#"{"gesture":{}}"#), None);
    }

    #[test]
    fn hid_table() {
        assert_eq!(hid_to_evdev(0x04), Some(30)); // a
        assert_eq!(hid_to_evdev(0x1D), Some(44)); // z
        assert_eq!(hid_to_evdev(0x1E), Some(2)); // 1
        assert_eq!(hid_to_evdev(0x28), Some(KEY_ENTER));
        assert_eq!(hid_to_evdev(0x29), Some(1)); // Esc
        assert_eq!(hid_to_evdev(0x2C), Some(KEY_SPACE));
        assert_eq!(hid_to_evdev(0x3A), Some(59)); // F1
        assert_eq!(hid_to_evdev(0x4F), Some(106)); // →
        assert_eq!(hid_to_evdev(0x52), Some(103)); // ↑
        assert_eq!(hid_to_evdev(0xE0), Some(KEY_LEFTCTRL));
        assert_eq!(hid_to_evdev(0xE3), Some(KEY_LEFTMETA));
        assert_eq!(hid_to_evdev(0xE6), Some(100));
        assert_eq!(hid_to_evdev(0x00), None);
        assert_eq!(hid_to_evdev(300), None);
    }

    #[test]
    fn click_lands_on_the_monitor() {
        let mut tr = t();
        let now = Instant::now();
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::LeftDown, x: 0.5, y: 0.25 }, now);
        assert_eq!(a, vec![Action::Motion { x: 960.0, y: 270.0 }, Action::Button { button: BTN_LEFT, pressed: true }]);
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::LeftUp, x: 0.5, y: 0.25 }, now);
        assert_eq!(a, vec![Action::Button { button: BTN_LEFT, pressed: false }]);
        // An up for a button not down is nothing.
        assert!(tr.handle(InputEvent::Pointer { action: PointerAction::RightUp, x: 0.5, y: 0.25 }, now).is_empty());
        // Out of range clamps to the monitor.
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::Move, x: 1.5, y: -1.0 }, now);
        assert_eq!(a, vec![Action::Motion { x: 1919.0, y: 0.0 }]);
    }

    #[test]
    fn motion_follows_the_stream_size() {
        let mut tr = t();
        let now = Instant::now();
        tr.handle(InputEvent::Pointer { action: PointerAction::Move, x: 0.2, y: 0.3 }, now);
        tr.set_size(3840.0, 2160.0); // GNOME at scale 2: the stream's own pixels
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::Move, x: 0.2, y: 0.3 }, now);
        assert_eq!(a, vec![Action::Motion { x: 768.0, y: 648.0 }]);
        // Past the 1920×1080 box the portal accepts: its edge, then the rest relative (logical).
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::Move, x: 0.8, y: 0.75 }, now);
        assert_eq!(a, vec![Action::Motion { x: 1919.0, y: 1079.0 }, Action::RelMotion { dx: 576.5, dy: 270.5 }]);
        // The far corner.
        let a = tr.handle(InputEvent::Pointer { action: PointerAction::Move, x: 1.0, y: 1.0 }, now);
        assert_eq!(a, vec![Action::Motion { x: 1919.0, y: 1079.0 }, Action::RelMotion { dx: 960.0, dy: 540.0 }]);
        // At scale 1 the box is the picture: never relative.
        let mut one = t();
        let a = one.handle(InputEvent::Pointer { action: PointerAction::Move, x: 1.0, y: 1.0 }, now);
        assert_eq!(a, vec![Action::Motion { x: 1919.0, y: 1079.0 }]);
    }

    fn mv(tr: &mut Translator, at: Instant, x: f64) -> Vec<Action> {
        tr.handle(InputEvent::Pointer { action: PointerAction::Move, x, y: 0.5 }, at)
    }
    fn press(tr: &mut Translator, at: Instant, down: bool) -> Vec<Action> {
        let action = if down { PointerAction::LeftDown } else { PointerAction::LeftUp };
        tr.handle(InputEvent::Pointer { action, x: 0.5, y: 0.5 }, at)
    }
    fn buttons(a: &[Action]) -> Vec<Action> {
        a.iter().filter(|x| matches!(x, Action::Button { .. })).cloned().collect()
    }

    #[test]
    fn a_tap_that_ends_a_move_is_dropped() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // The iPad's case: 650 ms of moves, the tap 69 ms after the last.
        let mut tr = t();
        for i in 0..30 {
            mv(&mut tr, ms(i * 22), 0.3 + i as f64 * 0.001);
        }
        assert!(buttons(&press(&mut tr, ms(29 * 22 + 69), true)).is_empty());
        assert!(buttons(&press(&mut tr, ms(29 * 22 + 70), false)).is_empty());
        assert_eq!(tr.dropped_taps, 1);
        assert!(tr.tick(ms(2000)).is_empty());

        // A real tap: a 66 ms twitch, then the tap.
        let mut tr = t();
        for i in 0..4 {
            mv(&mut tr, ms(i * 22), 0.5);
        }
        assert_eq!(buttons(&press(&mut tr, ms(85), true)), vec![Action::Button { button: BTN_LEFT, pressed: true }]);
        assert_eq!(buttons(&press(&mut tr, ms(86), false)), vec![Action::Button { button: BTN_LEFT, pressed: false }]);

        // A tap after a pause.
        let mut tr = t();
        for i in 0..30 {
            mv(&mut tr, ms(i * 22), 0.3);
        }
        assert_eq!(buttons(&press(&mut tr, ms(29 * 22 + 568), true)).len(), 1);

        // The Pencil after hovering: a press whose release does not come at once goes through,
        // 40 ms late, and its drag and release as usual.
        let mut tr = t();
        for i in 0..30 {
            mv(&mut tr, ms(i * 22), 0.3);
        }
        let at = 29 * 22 + 10;
        assert!(buttons(&press(&mut tr, ms(at), true)).is_empty());
        assert_eq!(buttons(&tr.tick(ms(at + 50))), vec![Action::Button { button: BTN_LEFT, pressed: true }]);
        assert_eq!(buttons(&press(&mut tr, ms(at + 400), false)), vec![Action::Button { button: BTN_LEFT, pressed: false }]);
        // Or a move first: the press goes out before it.
        let mut tr = t();
        for i in 0..30 {
            mv(&mut tr, ms(i * 22), 0.3);
        }
        press(&mut tr, ms(at), true);
        let a = mv(&mut tr, ms(at + 15), 0.31);
        assert_eq!(buttons(&a), vec![Action::Button { button: BTN_LEFT, pressed: true }]);

        // --keep-all-taps.
        let mut tr = t().keep_all_taps(true);
        for i in 0..30 {
            mv(&mut tr, ms(i * 22), 0.3);
        }
        assert_eq!(buttons(&press(&mut tr, ms(29 * 22 + 69), true)).len(), 1);
    }

    #[test]
    fn scroll_is_natural_and_stops_after_the_coast() {
        let mut tr = t();
        let now = Instant::now();
        tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::Began, x: 0.5, y: 0.5 }, now);
        // Finger down the glass: the content moves down, which is scrolling up on Wayland.
        let a = tr.handle(InputEvent::Scroll { x: 0.5, y: 0.5, dx: 0.0, dy: 0.01 }, now);
        assert_eq!(a, vec![Action::Axis { dx: -0.0, dy: -10.8 }]);
        assert!(tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::Ended, x: 0.5, y: 0.5 }, now).is_empty());
        tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::MomentumBegan, x: 0.5, y: 0.5 }, now);
        assert!(tr.tick(now + Duration::from_secs(1)).is_empty(), "no stop during the device's own coast");
        let a = tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::MomentumEnded, x: 0.5, y: 0.5 }, now);
        assert_eq!(a, vec![Action::AxisStop]);

        // A lift with no momentum: the stop 0.12 s later.
        tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::Began, x: 0.5, y: 0.5 }, now);
        tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::Ended, x: 0.5, y: 0.5 }, now);
        assert!(tr.tick(now + Duration::from_millis(50)).is_empty());
        assert_eq!(tr.tick(now + Duration::from_millis(130)), vec![Action::AxisStop]);
        assert!(tr.tick(now + Duration::from_millis(500)).is_empty());
    }

    #[test]
    fn command_is_ctrl_and_spotlight_is_super() {
        let mut tr = t();
        let now = Instant::now();
        assert_eq!(tr.handle(key(0xE3, true), now), vec![k(KEY_LEFTCTRL, true)]);
        assert_eq!(tr.handle(key(0x06, true), now), vec![k(46, true)]); // ⌘C → Ctrl+C
        assert_eq!(tr.handle(key(0x06, false), now), vec![k(46, false)]);
        // ⌘Space: Ctrl up, Super tapped, then the Space and Command ups are nothing.
        assert_eq!(tr.handle(key(0x2C, true), now), vec![k(KEY_LEFTCTRL, false), k(KEY_LEFTMETA, true), k(KEY_LEFTMETA, false)]);
        assert!(tr.handle(key(0x2C, false), now).is_empty());
        assert!(tr.handle(key(0xE3, false), now).is_empty());
        // Afterwards Space is Space again.
        assert_eq!(tr.handle(key(0x2C, true), now), vec![k(KEY_SPACE, true)]);

        let mut sup = Translator::new(100.0, 100.0, CommandKey::Super);
        assert_eq!(sup.handle(key(0xE3, true), now), vec![k(KEY_LEFTMETA, true)]);
        assert_eq!(sup.handle(key(0x2C, true), now), vec![k(KEY_SPACE, true)]);
    }

    #[test]
    fn control_and_command_share_ctrl() {
        let mut tr = t();
        let now = Instant::now();
        assert_eq!(tr.handle(key(0xE0, true), now), vec![k(KEY_LEFTCTRL, true)]);
        assert!(tr.handle(key(0xE3, true), now).is_empty());
        assert!(tr.handle(key(0xE0, false), now).is_empty(), "Command still holds Ctrl");
        assert_eq!(tr.handle(key(0xE3, false), now), vec![k(KEY_LEFTCTRL, false)]);
    }

    #[test]
    fn modifier_bits_without_their_keys() {
        let mut tr = t();
        let now = Instant::now();
        // An older device's ⌘S: one key carrying the command bit.
        let a = tr.handle(InputEvent::Key { hid_usage: 0x16, down: true, modifiers: MOD_COMMAND | MOD_SHIFT }, now);
        assert_eq!(a, vec![k(KEY_LEFTSHIFT, true), k(KEY_LEFTCTRL, true), k(31, true)]);
        let a = tr.handle(InputEvent::Key { hid_usage: 0x16, down: false, modifiers: MOD_COMMAND | MOD_SHIFT }, now);
        assert_eq!(a, vec![k(31, false), k(KEY_LEFTCTRL, false), k(KEY_LEFTSHIFT, false)]);
        // With Shift's own key down, only Command is added.
        tr.handle(key(0xE1, true), now);
        let a = tr.handle(InputEvent::Key { hid_usage: 0x16, down: true, modifiers: MOD_COMMAND | MOD_SHIFT }, now);
        assert_eq!(a, vec![k(KEY_LEFTCTRL, true), k(31, true)]);
    }

    #[test]
    fn text_lets_go_of_keys_first() {
        let mut tr = t();
        let now = Instant::now();
        tr.handle(key(0xE1, true), now);
        let a = tr.handle(InputEvent::Text("Hé\n".into()), now);
        assert_eq!(
            a,
            vec![
                k(KEY_LEFTSHIFT, false),
                Action::Keysym { sym: 'H' as i32, pressed: true },
                Action::Keysym { sym: 'H' as i32, pressed: false },
                Action::Keysym { sym: 0xe9, pressed: true },
                Action::Keysym { sym: 0xe9, pressed: false },
                k(KEY_ENTER, true),
                k(KEY_ENTER, false),
            ]
        );
        assert!(tr.handle(key(0xE1, false), now).is_empty(), "already up");
        assert_eq!(keysym('€'), 0x0100_20ac);
    }

    #[test]
    fn leaving_lets_go() {
        let mut tr = t();
        let now = Instant::now();
        tr.handle(InputEvent::Pointer { action: PointerAction::LeftDown, x: 0.1, y: 0.1 }, now);
        tr.handle(key(0xE3, true), now);
        tr.handle(InputEvent::ScrollGesture { phase: ScrollPhase::Began, x: 0.1, y: 0.1 }, now);
        assert_eq!(
            tr.release_all(),
            vec![Action::Button { button: BTN_LEFT, pressed: false }, k(KEY_LEFTCTRL, false), Action::AxisStop]
        );
        assert!(tr.release_all().is_empty());
    }
}
