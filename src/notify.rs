//! Desktop notifications (`org.freedesktop.Notifications`, through `gdbus`, which every GNOME
//! system has), for the pairing code when this host runs without a visible terminal (a user
//! service). Best effort: no notification server, no notification; the terminal line stays.

use std::process::Command;
use std::sync::Mutex;

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// One notification that can be replaced and closed (the pairing code's).
pub struct Note {
    id: Mutex<Option<u32>>,
}

impl Default for Note {
    fn default() -> Self {
        Note { id: Mutex::new(None) }
    }
}

/// GVariant text for a string: single quotes, backslash and quote escaped.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

impl Note {
    /// Shows `summary`/`body`, replacing this note's earlier one; it stays until closed (critical
    /// urgency: GNOME keeps it on screen while a device waits for the code).
    pub fn show(&self, summary: &str, body: &str) {
        if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut id = self.id.lock().unwrap();
        let replaces = id.unwrap_or(0);
        let out = Command::new("gdbus")
            .args([
                "call", "--session",
                "--dest", "org.freedesktop.Notifications",
                "--object-path", "/org/freedesktop/Notifications",
                "--method", "org.freedesktop.Notifications.Notify",
                "Sill for Linux", &replaces.to_string(), "video-display",
                &quote(summary), &quote(body), "[]", "{'urgency': <byte 2>}", "0",
            ])
            .output();
        // "(uint32 7,)"
        if let Ok(o) = out {
            let text = String::from_utf8_lossy(&o.stdout);
            *id = text.split_whitespace().nth(1).and_then(|n| n.trim_end_matches(",)").parse().ok()).or(*id);
        }
    }

    pub fn close(&self) {
        if let Some(id) = self.id.lock().unwrap().take() {
            let _ = Command::new("gdbus")
                .args([
                    "call", "--session",
                    "--dest", "org.freedesktop.Notifications",
                    "--object-path", "/org/freedesktop/Notifications",
                    "--method", "org.freedesktop.Notifications.CloseNotification",
                    &id.to_string(),
                ])
                .output();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting() {
        assert_eq!(super::quote("Noah's iPad"), r"'Noah\'s iPad'");
        assert_eq!(super::quote(r"a\b"), r"'a\\b'");
    }
}
