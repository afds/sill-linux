//! Sill's wire format, as Sill's `Sources/StreamProtocol` defines it (SILL_REFERENCE.md).
//!
//! Every message is a 14-byte big-endian header (kind, timestamp as a Double's bits, keyframe
//! flag, payload length) and a payload. Video goes as kind 0 (parameter sets) and kind 1 (one
//! access unit of 4-byte length-prefixed NAL units); everything small is JSON in the shape
//! Swift's synthesized Codable gives it.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const HEADER_LEN: usize = 14;
/// Sill's `StreamMessage.maxClientPayload`: no device message is anywhere near it.
pub const MAX_CLIENT_PAYLOAD: usize = 1 << 20;
/// `SillProtocol.current`.
pub const PROTOCOL: i64 = 1;

/// The kinds this host sends or reads (StreamMessage.swift's `StreamMessageKind`).
pub mod kind {
    pub const PARAMETER_SETS: u8 = 0;
    pub const FRAME: u8 = 1;
    pub const WINDOW_LIST: u8 = 2;
    pub const THUMBNAIL: u8 = 3;
    pub const APP_ICON: u8 = 4;
    pub const APP_LIST: u8 = 5;
    pub const SELECT_SOURCE: u8 = 6;
    pub const LAUNCH_APP: u8 = 7;
    pub const INPUT: u8 = 8;
    pub const VIEWPORT: u8 = 9;
    pub const PING: u8 = 10;
    pub const PONG: u8 = 11;
    pub const CLIENT_STATS: u8 = 12;
    pub const TICK: u8 = 13;
    pub const WINDOW_COMMAND: u8 = 15;
    pub const HOST_SETTINGS: u8 = 16;
    pub const CHANGE_SETTINGS: u8 = 17;
    pub const PAIR_REQUEST: u8 = 19;
    pub const PAIR_RESULT: u8 = 20;
    pub const GOODBYE: u8 = 22;
    pub const HELLO: u8 = 23;
    pub const MAC_POINTER: u8 = 26;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    pub kind: u8,
    pub timestamp: f64,
    pub keyframe: bool,
    pub len: usize,
}

impl Header {
    pub fn parse(b: &[u8; HEADER_LEN]) -> Header {
        Header {
            kind: b[0],
            timestamp: f64::from_bits(u64::from_be_bytes(b[1..9].try_into().unwrap())),
            keyframe: b[9] == 1,
            len: u32::from_be_bytes(b[10..14].try_into().unwrap()) as usize,
        }
    }
}

/// Host wall clock in seconds since 1970, what the device subtracts from its own for frame age.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// One serialized message: header and payload.
pub fn message(kind: u8, timestamp: f64, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(HEADER_LEN + payload.len());
    d.push(kind);
    d.extend_from_slice(&timestamp.to_bits().to_be_bytes());
    d.push(keyframe as u8);
    d.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    d.extend_from_slice(payload);
    d
}

/// `ParameterSets.encoded()`: nalLen(1) count(1), then each set as length(2) + bytes.
pub fn encode_parameter_sets(sets: &[Vec<u8>]) -> Vec<u8> {
    let mut d = vec![4u8, sets.len() as u8];
    for s in sets {
        d.extend_from_slice(&(s.len() as u16).to_be_bytes());
        d.extend_from_slice(s);
    }
    d
}

/// One HEVC access unit as the device wants it.
#[derive(Debug, Default, PartialEq)]
pub struct AccessUnit {
    /// VPS, SPS and PPS found in the unit, in stream order, without start codes.
    pub parameter_sets: Vec<Vec<u8>>,
    /// The other NAL units, each with a 4-byte big-endian length prefix (hvcC style).
    pub frame: Vec<u8>,
    /// The unit holds an IRAP picture (IDR, CRA, BLA): the device may start decoding here.
    pub keyframe: bool,
}

/// Splits an Annex-B access unit (start codes, as encoders give it) into what kinds 0 and 1
/// carry. Access unit delimiters are dropped; parameter sets go to kind 0 only.
pub fn convert_annex_b(au: &[u8]) -> AccessUnit {
    let mut out = AccessUnit::default();
    for nal in annex_b_nals(au) {
        if nal.len() < 2 {
            continue;
        }
        let nal_type = (nal[0] >> 1) & 0x3f;
        match nal_type {
            35 => {} // access unit delimiter
            32..=34 => out.parameter_sets.push(nal.to_vec()),
            _ => {
                if (16..=23).contains(&nal_type) {
                    out.keyframe = true;
                }
                out.frame.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.frame.extend_from_slice(nal);
            }
        }
    }
    out
}

/// The NAL units of an Annex-B byte stream (3- or 4-byte start codes; trailing zeros trimmed).
fn annex_b_nals(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // index of the first byte after each start code
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut end = if n + 1 < starts.len() { starts[n + 1] - 3 } else { d.len() };
        while end > s && d[end - 1] == 0 {
            end -= 1; // the leading zero of the next 4-byte start code, or trailing_zero_8bits
        }
        nals.push(&d[s..end]);
    }
    nals
}

/// `StreamSource`: Swift encodes `.none` as `{"none":{}}`, `.window(7)` as `{"window":{"_0":7}}`
/// and `.desktop` as `{"desktop":{}}`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamSource {
    None,
    Window(u32),
    Desktop,
}

impl Serialize for StreamSource {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = match self {
            StreamSource::None => serde_json::json!({"none": {}}),
            StreamSource::Window(id) => serde_json::json!({"window": {"_0": id}}),
            StreamSource::Desktop => serde_json::json!({"desktop": {}}),
        };
        v.serialize(s)
    }
}

impl<'de> Deserialize<'de> for StreamSource {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_json::Value::deserialize(d)?;
        let obj = v.as_object().filter(|o| o.len() == 1).ok_or_else(|| D::Error::custom("not a StreamSource"))?;
        let (key, inner) = obj.iter().next().unwrap();
        match key.as_str() {
            "none" => Ok(StreamSource::None),
            "desktop" => Ok(StreamSource::Desktop),
            "window" => inner
                .get("_0")
                .and_then(|x| x.as_u64())
                .and_then(|x| u32::try_from(x).ok())
                .map(StreamSource::Window)
                .ok_or_else(|| D::Error::custom("bad window id")),
            other => Err(D::Error::custom(format!("unknown source {other}"))),
        }
    }
}

/// `WindowInfo`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    #[serde(rename = "bundleID")]
    pub bundle_id: String,
    pub width: i64,
    pub height: i64,
}

/// `WindowList` (kind 2). Optional fields left out when nil, as Swift's encoder does.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WindowList {
    pub mac_name: String,
    pub windows: Vec<WindowInfo>,
    pub active: StreamSource,
    #[serde(rename = "launchID", skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<i64>,
}

/// `Viewport` (kind 9).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
    pub scale: Option<f64>,
    pub fps: Option<i64>,
}

/// `ClientStats` (kind 12).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientStats {
    pub fps: i64,
    pub frame_age_ms: i64,
    pub rtt_ms: i64,
    pub device: String,
    pub frame_age_max_ms: Option<i64>,
    pub rtt_max_ms: Option<i64>,
}

/// `Hello` (kind 23): every field optional.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub app_version: Option<String>,
    pub build: Option<String>,
    pub protocol: Option<i64>,
    pub device: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let m = message(kind::FRAME, 1_760_000_000.25, true, b"abc");
        assert_eq!(m.len(), HEADER_LEN + 3);
        assert_eq!(m[0], 1);
        assert_eq!(&m[1..9], &1_760_000_000.25f64.to_bits().to_be_bytes());
        assert_eq!(m[9], 1);
        assert_eq!(&m[10..14], &[0, 0, 0, 3]);
        let h = Header::parse(m[..14].try_into().unwrap());
        assert_eq!(h, Header { kind: 1, timestamp: 1_760_000_000.25, keyframe: true, len: 3 });
    }

    #[test]
    fn parameter_sets_layout() {
        let d = encode_parameter_sets(&[vec![0x40, 1], vec![0x42, 1, 2]]);
        assert_eq!(d, vec![4, 2, 0, 2, 0x40, 1, 0, 3, 0x42, 1, 2]);
    }

    #[test]
    fn annex_b_to_length_prefixed() {
        // AUD, VPS, SPS, PPS, IDR_W_RADL, with 4- and 3-byte start codes.
        let au = [
            0, 0, 0, 1, 0x46, 0x01, 0x50, // AUD (type 35)
            0, 0, 0, 1, 0x40, 0x01, 0xAA, // VPS
            0, 0, 1, 0x42, 0x01, 0xBB, // SPS
            0, 0, 1, 0x44, 0x01, 0xCC, // PPS
            0, 0, 0, 1, 0x26, 0x01, 0xDD, 0xEE, // IDR (type 19)
        ];
        let out = convert_annex_b(&au);
        assert!(out.keyframe);
        assert_eq!(out.parameter_sets, vec![vec![0x40, 1, 0xAA], vec![0x42, 1, 0xBB], vec![0x44, 1, 0xCC]]);
        assert_eq!(out.frame, vec![0, 0, 0, 4, 0x26, 1, 0xDD, 0xEE]);
    }

    #[test]
    fn delta_is_not_keyframe() {
        let out = convert_annex_b(&[0, 0, 0, 1, 0x02, 0x01, 0x11, 0x00]); // TRAIL_R (type 1), trailing zero
        assert!(!out.keyframe);
        assert!(out.parameter_sets.is_empty());
        assert_eq!(out.frame, vec![0, 0, 0, 3, 0x02, 1, 0x11]);
    }

    #[test]
    fn stream_source_json_is_swifts() {
        assert_eq!(serde_json::to_string(&StreamSource::None).unwrap(), r#"{"none":{}}"#);
        assert_eq!(serde_json::to_string(&StreamSource::Desktop).unwrap(), r#"{"desktop":{}}"#);
        assert_eq!(serde_json::to_string(&StreamSource::Window(7)).unwrap(), r#"{"window":{"_0":7}}"#);
        for s in [r#"{"none":{}}"#, r#"{"desktop":{}}"#, r#"{"window":{"_0":42}}"#] {
            let v: StreamSource = serde_json::from_str(s).unwrap();
            assert_eq!(serde_json::to_string(&v).unwrap(), s);
        }
        assert!(serde_json::from_str::<StreamSource>(r#"{"window":{}}"#).is_err());
    }

    #[test]
    fn window_list_json() {
        let l = WindowList {
            mac_name: "linux".into(),
            windows: vec![],
            active: StreamSource::Desktop,
            launch_id: Some("L".into()),
            host_version: None,
            protocol: Some(1),
        };
        let v: serde_json::Value = serde_json::to_value(&l).unwrap();
        assert_eq!(v, serde_json::json!({"macName":"linux","windows":[],"active":{"desktop":{}},"launchID":"L","protocol":1}));
        let w = WindowInfo { id: 3, title: "t".into(), app_name: "a".into(), bundle_id: "b".into(), width: 1, height: 2 };
        assert_eq!(
            serde_json::to_value(&w).unwrap(),
            serde_json::json!({"id":3,"title":"t","appName":"a","bundleID":"b","width":1,"height":2})
        );
    }

    #[test]
    fn device_messages_decode() {
        let v: Viewport = serde_json::from_str(r#"{"width":744,"height":500,"fps":120,"localCursor":true}"#).unwrap();
        assert_eq!(v.fps, Some(120));
        assert_eq!(v.scale, None);
        let s: ClientStats = serde_json::from_str(
            r#"{"fps":60,"frameAgeMs":9,"rttMs":7,"device":"iPad14,1","frameAgeMaxMs":20,"rttMaxMs":-1}"#,
        )
        .unwrap();
        assert_eq!(s.rtt_max_ms, Some(-1));
        let h: Hello = serde_json::from_str(r#"{"appVersion":"0.5.1","build":"4","protocol":1,"device":"iPad"}"#).unwrap();
        assert_eq!(h.device.as_deref(), Some("iPad"));
        let _: Hello = serde_json::from_str("{}").unwrap();
    }
}
