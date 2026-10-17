//! Replays the device events of an input log (`SILL_LINUX_INPUT_LOG=1`) through the translator
//! with their original timing and prints which clicks reach the portal.
//!
//! usage: cargo run --example replay-input -- /tmp/sill-input.log [--keep-all-taps]
use sill_linux::input::{Action, CommandKey, InputEvent, PointerAction, Translator};
use std::time::{Duration, Instant};

fn main() {
    let path = std::env::args().nth(1).expect("log file");
    let keep = std::env::args().any(|a| a == "--keep-all-taps");
    let t0 = Instant::now();
    let mut tr = Translator::new(1920.0, 1080.0, CommandKey::Ctrl).keep_all_taps(keep);
    for line in std::fs::read_to_string(path).unwrap().lines() {
        // "    6144 ms  device 1: Pointer { action: LeftDown, x: 0.72, y: 0.07 }"
        let Some((ms, rest)) = line.trim_start().split_once(" ms  device ") else { continue };
        let Ok(ms) = ms.trim().parse::<u64>() else { continue };
        let Some(ev) = rest.split_once(": ").map(|(_, e)| e) else { continue };
        let at = t0 + Duration::from_millis(ms);
        let parse = |k: &str| ev.split(&format!("{k}: ")).nth(1).and_then(|s| s.split([',', ' ', '}']).next()).and_then(|v| v.parse::<f64>().ok());
        let action = ["Move", "LeftDown", "LeftUp", "RightDown", "RightUp"].iter().find(|a| ev.contains(&format!("action: {a},")));
        let (Some(a), Some(x), Some(y)) = (action, parse("x"), parse("y")) else { continue };
        let action = match *a {
            "Move" => PointerAction::Move,
            "LeftDown" => PointerAction::LeftDown,
            "LeftUp" => PointerAction::LeftUp,
            "RightDown" => PointerAction::RightDown,
            _ => PointerAction::RightUp,
        };
        let before = tr.dropped_taps;
        let mut out = tr.tick(at);
        out.extend(tr.handle(InputEvent::Pointer { action, x, y }, at));
        for a in out {
            if let Action::Button { pressed, .. } = a {
                println!("{ms:>8} ms  click {}", if pressed { "down" } else { "up" });
            }
        }
        if tr.dropped_taps > before {
            println!("{ms:>8} ms  tap dropped (it ended a pointer move)");
        }
    }
}
