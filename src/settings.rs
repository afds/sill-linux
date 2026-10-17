//! The host settings a device sees and changes from the Sill app's Settings panel (kinds 16 and
//! 17, Sill's `HostSettings.swift`), with Sill's rules (`DeviceSettings.accepted`): a device sets
//! only the Mac menu's values, and a change is saved, as Sill.app saves it.
//!
//! What they mean here:
//! - Quality (`bitrate`): one of Sill's presets, per 60 fps (doubled at 120).
//! - Resolution (`captureScale`): 2 is Retina, the stream's own pixels (3840×2160 for a 4K
//!   monitor at scale 2); 1 is Standard, the monitor's logical size (1920×1080), scaled on the GPU.
//!   Standard is this host's default (Sill.app's is Retina): the app's panel is far smaller than a
//!   4K picture, so the same bitrate spends more on each pixel.
//! - Frame Rate limit (`maxFPS`): 60 or 120; the stream runs at the lowest of it, the highest rate
//!   a connected device asks for, and what the compositor sends.
//! - Prioritize Speed: this host always runs NVENC at its lowest-latency preset, so it says on and
//!   refuses off (the panel shows "kept its setting").
//! - Virtual Display: not on Linux (unavailable, with a note). Direct Wireless: no row (nil).

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const PRESETS: [i64; 7] = [4_000_000, 8_000_000, 15_000_000, 25_000_000, 40_000_000, 80_000_000, 150_000_000];
pub const FPS_CHOICES: [i64; 2] = [60, 120];
pub const SCALE_CHOICES: [f64; 2] = [2.0, 1.0];

/// What is saved and applied.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(rename = "maxFPS")]
    pub max_fps: i64,
    pub bitrate: i64,
    #[serde(rename = "captureScale")]
    pub capture_scale: f64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { max_fps: 120, bitrate: 15_000_000, capture_scale: 1.0 }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        std::fs::read(path)
            .ok()
            .and_then(|d| serde_json::from_slice::<Settings>(&d).ok())
            .filter(|s| PRESETS.contains(&s.bitrate) && FPS_CHOICES.contains(&s.max_fps) && SCALE_CHOICES.contains(&s.capture_scale))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self).unwrap())
    }

    /// The stream's rate: the limit, the fastest device (60 when none says), never above 120.
    pub fn fps(&self, fastest_device: Option<i64>) -> i64 {
        self.max_fps.min(fastest_device.unwrap_or(60)).clamp(24, 120)
    }

    /// The encoder's bitrate at `fps`: the preset is per 60 fps.
    pub fn bitrate_at(&self, fps: i64) -> i64 {
        self.bitrate * fps / 60
    }

    pub fn retina(&self) -> bool {
        self.capture_scale >= 1.5
    }
}

/// `StreamSettings`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StreamSettings {
    #[serde(rename = "maxFPS")]
    pub max_fps: i64,
    pub bitrate: i64,
    #[serde(rename = "captureScale")]
    pub capture_scale: f64,
    #[serde(rename = "prioritizeSpeed")]
    pub prioritize_speed: bool,
    #[serde(rename = "virtualDisplay")]
    pub virtual_display: bool,
}

/// `RunningStream`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunningStream {
    pub width: i64,
    pub height: i64,
    pub fps: i64,
    pub mbps: i64,
    pub on_virtual_display: bool,
}

/// `HostSettingsState` (kind 16); nil fields left out.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostSettingsState {
    pub settings: StreamSettings,
    pub persistent: bool,
    pub virtual_display_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtual_display_note: Option<String>,
    pub software_encoder: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<RunningStream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answering: Option<i64>,
}

/// `HostSettingsChange` (kind 17): only the fields one control changed, and a token.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct HostSettingsChange {
    pub token: Option<i64>,
    #[serde(rename = "maxFPS")]
    pub max_fps: Option<i64>,
    pub bitrate: Option<i64>,
    #[serde(rename = "captureScale")]
    pub capture_scale: Option<f64>,
    #[serde(rename = "prioritizeSpeed")]
    pub prioritize_speed: Option<bool>,
    #[serde(rename = "virtualDisplay")]
    pub virtual_display: Option<bool>,
    #[serde(rename = "directWireless")]
    pub direct_wireless: Option<bool>,
}

/// Applies what a device may set; returns the new settings and what was refused, in words for
/// the log.
pub fn accept(current: Settings, c: &HostSettingsChange) -> (Settings, Vec<String>) {
    let mut s = current;
    let mut refused = Vec::new();
    if let Some(v) = c.max_fps {
        if FPS_CHOICES.contains(&v) { s.max_fps = v } else { refused.push(format!("frame rate limit {v}")) }
    }
    if let Some(v) = c.bitrate {
        if PRESETS.contains(&v) { s.bitrate = v } else { refused.push(format!("bitrate {v}")) }
    }
    if let Some(v) = c.capture_scale {
        if SCALE_CHOICES.contains(&v) { s.capture_scale = v } else { refused.push(format!("capture scale {v}")) }
    }
    if c.prioritize_speed == Some(false) {
        refused.push("prioritize speed off (this host always encodes for the lowest latency)".into());
    }
    if c.virtual_display == Some(true) {
        refused.push("virtual display (not on Linux)".into());
    }
    if c.direct_wireless.is_some() {
        refused.push("direct wireless (not on this host)".into());
    }
    (s, refused)
}

/// The state a device is sent.
pub fn state(s: &Settings, software: bool, stream: Option<RunningStream>, answering: Option<i64>) -> HostSettingsState {
    HostSettingsState {
        settings: StreamSettings {
            max_fps: s.max_fps,
            bitrate: s.bitrate,
            capture_scale: s.capture_scale,
            prioritize_speed: true,
            virtual_display: false,
        },
        persistent: true,
        virtual_display_available: false,
        virtual_display_note: Some("Linux has no virtual display for Sill; windows stream from the screen.".into()),
        software_encoder: software,
        stream,
        answering,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_is_sills() {
        let s = Settings::default();
        let st = state(&s, false, Some(RunningStream { width: 1920, height: 1080, fps: 60, mbps: 15, on_virtual_display: false }), Some(3));
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "settings": {"maxFPS": 120, "bitrate": 15000000, "captureScale": 1.0, "prioritizeSpeed": true, "virtualDisplay": false},
                "persistent": true,
                "virtualDisplayAvailable": false,
                "virtualDisplayNote": "Linux has no virtual display for Sill; windows stream from the screen.",
                "softwareEncoder": false,
                "stream": {"width": 1920, "height": 1080, "fps": 60, "mbps": 15, "onVirtualDisplay": false},
                "answering": 3
            })
        );
        let bare = serde_json::to_value(state(&s, true, None, None)).unwrap();
        assert!(bare.get("stream").is_none() && bare.get("answering").is_none());
    }

    #[test]
    fn changes_follow_sills_rules() {
        let c: HostSettingsChange = serde_json::from_str(r#"{"token":7,"bitrate":40000000,"captureScale":2}"#).unwrap();
        let (s, refused) = accept(Settings::default(), &c);
        assert_eq!((s.bitrate, s.capture_scale, refused.len()), (40_000_000, 2.0, 0));
        assert_eq!(c.token, Some(7));

        let c: HostSettingsChange = serde_json::from_str(r#"{"token":8,"bitrate":200000000,"maxFPS":90,"captureScale":1.5}"#).unwrap();
        let (s, refused) = accept(Settings::default(), &c);
        assert_eq!(s, Settings::default());
        assert_eq!(refused.len(), 3);

        let c: HostSettingsChange = serde_json::from_str(r#"{"token":9,"prioritizeSpeed":false,"virtualDisplay":true,"directWireless":true}"#).unwrap();
        assert_eq!(accept(Settings::default(), &c).1.len(), 3);
        let c: HostSettingsChange = serde_json::from_str(r#"{"token":10,"prioritizeSpeed":true,"virtualDisplay":false}"#).unwrap();
        assert!(accept(Settings::default(), &c).1.is_empty());
    }

    #[test]
    fn rate_and_bitrate() {
        let s = Settings::default();
        assert_eq!(s.fps(None), 60);
        assert_eq!(s.fps(Some(120)), 120);
        assert_eq!(Settings { max_fps: 60, ..s }.fps(Some(120)), 60);
        assert_eq!(s.bitrate_at(120), 30_000_000);
        assert_eq!(s.bitrate_at(60), 15_000_000);
    }

    #[test]
    fn saved_and_loaded() {
        let p = std::env::temp_dir().join(format!("sill-settings-{}.json", uuid::Uuid::new_v4()));
        let s = Settings { max_fps: 60, bitrate: 40_000_000, capture_scale: 2.0 };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        std::fs::write(&p, br#"{"maxFPS":99,"bitrate":1,"captureScale":3}"#).unwrap();
        assert_eq!(Settings::load(&p), Settings::default(), "out-of-range values fall back");
        std::fs::remove_file(p).unwrap();
    }
}
