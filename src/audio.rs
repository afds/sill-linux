//! This machine's sound to the devices (kind 43, Sill's `Audio.swift`) → 48 kHz stereo → AAC-ELD,
//! 480 frames (10 ms) a packet, 128 kbps → a format (type 1, JSON) per epoch, then one packet a
//! message (type 2). Only to a device whose hello lists "aac-eld", only while something streams
//! and Send Audio is on. What it is of, as Sill for Mac: the streamed window's app alone (its
//! PipeWire playback stream, found by its process or a child of it: browsers play from one), else
//! everything playing (the default output's monitor). Looked at again every 2 s; a change is a
//! new epoch.
//!
//! The encoder is libfdk-aac's, opened at run time (`libfdk-aac.so.2`, Ubuntu's libfdk-aac2 in
//! universe, which PipeWire's Bluetooth AAC already installs; the Fedora-stripped build keeps
//! AAC-ELD): no build dependency, and no sound where it is missing.

use crate::server::{Server, SoundOf};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::ffi::c_void;
use std::time::Duration;

pub const CODEC: &str = "aac-eld";
const RATE: u32 = 48_000;
const FRAMES: usize = 480;
const BITRATE: u32 = 128_000;

type Open = unsafe extern "C" fn(*mut *mut c_void, u32, u32) -> u32;
type SetParam = unsafe extern "C" fn(*mut c_void, u32, u32) -> u32;
type Encode = unsafe extern "C" fn(*mut c_void, *const BufDesc, *const BufDesc, *const InArgs, *mut OutArgs) -> u32;
type Info = unsafe extern "C" fn(*mut c_void, *mut InfoStruct) -> u32;
type Close = unsafe extern "C" fn(*mut *mut c_void) -> u32;

#[repr(C)]
struct BufDesc {
    num_bufs: i32,
    bufs: *mut *mut c_void,
    ids: *mut i32,
    sizes: *mut i32,
    el_sizes: *mut i32,
}
#[repr(C)]
struct InArgs {
    num_in_samples: i32,
    num_anc_bytes: i32,
}
#[repr(C)]
#[derive(Default)]
struct OutArgs {
    num_out_bytes: i32,
    num_in_samples: i32,
    num_anc_bytes: i32,
    bit_res_state: i32,
}
#[repr(C)]
struct InfoStruct {
    max_out_buf_bytes: u32,
    max_anc_bytes: u32,
    in_buf_fill_level: u32,
    input_channels: u32,
    frame_length: u32,
    n_delay: u32,
    n_delay_core: u32,
    conf_buf: [u8; 64],
    conf_size: u32,
}

/// An AAC-ELD encoder: 48 kHz stereo, 480 frames a packet.
pub struct Eld {
    handle: *mut c_void,
    encode: Encode,
    close: Close,
    /// Its AudioSpecificConfig (the format's `cookie`).
    pub cookie: Vec<u8>,
    /// The frames its output starts with before the first captured one (240).
    pub priming: u32,
}

// The handle is used from one thread at a time.
unsafe impl Send for Eld {}

impl Eld {
    pub fn open() -> Result<Eld, String> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; each symbol is cast to its C signature
        // (fdk-aac 2's aacenc_lib.h), and a missing one stops before any call.
        unsafe {
            let lib = libc::dlopen(c"libfdk-aac.so.2".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return Err("libfdk-aac.so.2 is not installed (apt install libfdk-aac2)".into());
            }
            let sym = |name: &std::ffi::CStr| {
                let p = libc::dlsym(lib, name.as_ptr());
                (!p.is_null()).then_some(p).ok_or_else(|| format!("libfdk-aac has no {name:?}"))
            };
            let open: Open = std::mem::transmute(sym(c"aacEncOpen")?);
            let set: SetParam = std::mem::transmute(sym(c"aacEncoder_SetParam")?);
            let encode: Encode = std::mem::transmute(sym(c"aacEncEncode")?);
            let info: Info = std::mem::transmute(sym(c"aacEncInfo")?);
            let close: Close = std::mem::transmute(sym(c"aacEncClose")?);
            let mut handle = std::ptr::null_mut();
            if open(&mut handle, 0, 2) != 0 {
                return Err("aacEncOpen failed".into());
            }
            // AOT 39 (ELD), the rate, stereo (mode 2), the bitrate, raw packets (transmux 0),
            // 480-frame granules, the afterburner (better quality for little time).
            for (param, value) in [(0x100, 39), (0x103, RATE), (0x106, 2), (0x101, BITRATE), (0x300, 0), (0x105, FRAMES as u32), (0x200, 1)] {
                if set(handle, param, value) != 0 {
                    close(&mut handle);
                    return Err(format!("libfdk-aac refused parameter {param:#x} = {value} (AAC-ELD)"));
                }
            }
            let mut i: InfoStruct = std::mem::zeroed();
            if encode(handle, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null_mut()) != 0 || info(handle, &mut i) != 0 || i.frame_length as usize != FRAMES {
                close(&mut handle);
                return Err("libfdk-aac could not start AAC-ELD".into());
            }
            let cookie = i.conf_buf[..(i.conf_size as usize).min(64)].to_vec();
            Ok(Eld { handle, encode, close, cookie, priming: i.n_delay })
        }
    }

    /// One packet of 480 frames (960 interleaved samples): its bytes, empty while the encoder
    /// still fills.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        assert_eq!(pcm.len(), FRAMES * 2);
        let mut out = vec![0u8; 2048];
        let (mut in_ptr, mut out_ptr) = (pcm.as_ptr() as *mut c_void, out.as_mut_ptr() as *mut c_void);
        let (mut in_id, mut in_size, mut in_el) = (0i32, (pcm.len() * 2) as i32, 2i32); // IN_AUDIO_DATA
        let (mut out_id, mut out_size, mut out_el) = (3i32, out.len() as i32, 1i32); // OUT_BITSTREAM_DATA
        let in_desc = BufDesc { num_bufs: 1, bufs: &mut in_ptr, ids: &mut in_id, sizes: &mut in_size, el_sizes: &mut in_el };
        let out_desc = BufDesc { num_bufs: 1, bufs: &mut out_ptr, ids: &mut out_id, sizes: &mut out_size, el_sizes: &mut out_el };
        let args = InArgs { num_in_samples: pcm.len() as i32, num_anc_bytes: 0 };
        let mut res = OutArgs::default();
        // SAFETY: both descriptors point at live buffers of the sizes they give.
        let err = unsafe { (self.encode)(self.handle, &in_desc, &out_desc, &args, &mut res) };
        out.truncate(if err == 0 { res.num_out_bytes.max(0) as usize } else { 0 });
        out
    }
}

impl Drop for Eld {
    fn drop(&mut self) {
        // SAFETY: the handle came from aacEncOpen and is closed once.
        unsafe { (self.close)(&mut self.handle) };
    }
}

/// The magic cookie as AudioToolbox makes and takes one for MPEG-4 AAC: an ES descriptor (ESDS)
/// around the AudioSpecificConfig, each length in four bytes, as Apple's encoder writes them. The
/// bare AudioSpecificConfig was not played by the device's decoder (2026-11-13).
pub fn esds(asc: &[u8]) -> Vec<u8> {
    let tag = |t: u8, body: &[u8]| -> Vec<u8> {
        let mut d = vec![t, 0x80, 0x80, 0x80, body.len() as u8];
        d.extend(body);
        d
    };
    // MPEG-4 audio (0x40), an audio stream (0x15), a 6144-byte buffer, the bitrate twice.
    let mut config = vec![0x40, 0x15, 0x00, 0x18, 0x00];
    config.extend(BITRATE.to_be_bytes());
    config.extend(BITRATE.to_be_bytes());
    config.extend(tag(0x05, asc));
    let mut es = vec![0x00, 0x00, 0x00]; // ES_ID 0, no flags
    es.extend(tag(0x04, &config));
    es.extend(tag(0x06, &[0x02])); // SLConfig: MP4
    tag(0x03, &es)
}

/// Each packet's stamp (the wall clock of its first frame, as the device lines the sound up with
/// the picture by it): the segment's first frame's time plus the frames since, so packets sit
/// exactly 10 ms apart, moved to the wall clock when the sound card's clock and it part (100 ppm
/// is 360 ms an hour). A buffer that came late only makes a reading late, so each second's
/// earliest reading is the one trusted: off by more than 20 ms, the stamps move by it.
#[derive(Default)]
pub struct Stamps {
    first: Option<f64>,
    earliest: Option<f64>,
    readings: u32,
}

impl Stamps {
    const PER_CHECK: u32 = 100;
    const TOLERANCE: f64 = 0.02;

    /// Packet `seq`'s stamp, `read` the wall-clock reading of its first frame.
    pub fn stamp(&mut self, seq: u32, read: f64) -> f64 {
        let packets = seq as f64 * FRAMES as f64 / RATE as f64;
        let first = *self.first.get_or_insert(read - packets);
        let off = read - (first + packets);
        self.earliest = Some(self.earliest.map_or(off, |e| e.min(off)));
        self.readings += 1;
        if self.readings >= Self::PER_CHECK {
            let earliest = self.earliest.take().unwrap_or(0.0);
            if earliest.abs() > Self::TOLERANCE {
                self.first = Some(first + earliest);
            }
            self.readings = 0;
        }
        self.first.unwrap() + packets
    }
}

/// Kind 43 type 1: how to decode the epoch's packets.
/// `source`: "app" (then `app` names it) or "desktop".
pub fn format_payload(eld: &Eld, epoch: u16, source: &str, app: Option<&str>) -> Vec<u8> {
    let mut json = serde_json::json!({
        "codec": CODEC, "sampleRate": RATE, "channels": 2, "framesPerPacket": FRAMES,
        "primingFrames": eld.priming, "bitrate": BITRATE, "epoch": epoch,
        "cookie": crate::pairing::b64(&esds(&eld.cookie)), "source": source,
    });
    if let Some(app) = app {
        json["app"] = app.into();
    }
    let mut p = vec![1u8];
    p.extend(serde_json::to_vec(&json).unwrap());
    p
}

/// Kind 43 type 2: one packet. `start`: the first of a segment (the device drops the priming).
pub fn packet_payload(epoch: u16, seq: u32, start: bool, packet: &[u8]) -> Vec<u8> {
    let mut p = vec![2u8];
    p.extend(epoch.to_be_bytes());
    p.extend(seq.to_be_bytes());
    p.push(start as u8);
    p.push(1);
    p.extend((packet.len() as u16).to_be_bytes());
    p.extend(packet);
    p
}

/// What the sound is taken from.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// The default output's monitor: everything playing.
    Everything,
    /// One app's playback stream (its PipeWire node's serial) and the app's name.
    App(u64, String),
    /// The streamed window's app plays nothing (or is not known yet): no sound.
    Nothing,
}

fn target(server: &Server) -> Target {
    match server.sound_of() {
        SoundOf::Everything => Target::Everything,
        SoundOf::Unknown => Target::Nothing,
        SoundOf::Process(pid) => {
            let dump = std::process::Command::new("pw-dump").output().ok().and_then(|o| serde_json::from_slice(&o.stdout).ok());
            dump.and_then(|d| app_stream(&d, pid, parent)).map_or(Target::Nothing, |(serial, app)| Target::App(serial, app))
        }
    }
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid …": the name can hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// In a `pw-dump`, the playback stream of process `pid` or of one under it (`parent` walks up):
/// its node's serial and its app's name; a running one first.
pub fn app_stream(dump: &serde_json::Value, pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Option<(u64, String)> {
    let objects = dump.as_array()?;
    let props = |o: &serde_json::Value| o.pointer("/info/props").cloned().unwrap_or_default();
    let number = |v: &serde_json::Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
    let client_pid = |client: u64| {
        objects.iter().find(|o| o["id"].as_u64() == Some(client)).and_then(|o| number(&props(o)["application.process.id"]))
    };
    let under = |mut p: u32| {
        for _ in 0..32 {
            if p == pid {
                return true;
            }
            match parent(p) {
                Some(q) if q > 1 && q != p => p = q,
                _ => return false,
            }
        }
        false
    };
    let mut found: Vec<(bool, u64, String)> = objects
        .iter()
        .filter(|o| props(o)["media.class"] == "Stream/Output/Audio")
        .filter_map(|o| {
            let p = props(o);
            let owner = client_pid(number(&p["client.id"])?)?;
            under(u32::try_from(owner).ok()?).then(|| {
                let name = p["application.name"].as_str().or(p["node.name"].as_str()).unwrap_or("an app");
                (o.pointer("/info/state") == Some(&"running".into()), number(&p["object.serial"]).unwrap_or(0), crate::door::clean(name, "an app"))
            })
        })
        .collect();
    // shortcut: one stream of the app (the running one), not a mix of all of them; an app with
    // several playing at once (two browser tabs) streams one.
    found.sort_by_key(|f| !f.0);
    found.into_iter().next().map(|(_, serial, name)| (serial, name))
}

/// The sound while a device that plays it watches a stream; started again after each pause, a
/// new epoch each time.
pub fn run(server: Server) {
    if let Err(e) = Eld::open() {
        return println!("Sound: none to devices ({e}).");
    }
    println!("Sound: this machine's to devices that play it, while Send Audio is on (AAC-ELD, 128 kbps; --no-audio: never).");
    server.set_sound();
    let mut epoch: u16 = 0;
    let mut said = None;
    loop {
        while !server.wants_audio() {
            std::thread::sleep(Duration::from_millis(500));
        }
        let what = target(&server);
        if said.as_ref() != Some(&what) {
            match &what {
                Target::Everything => println!("Sound: everything this machine plays."),
                Target::App(serial, app) => println!("Sound: {app}'s alone (PipeWire node {serial})."),
                Target::Nothing => println!("Sound: none (the streamed window's app plays nothing)."),
            }
            said = Some(what.clone());
        }
        if what == Target::Nothing {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        epoch = epoch.wrapping_add(1);
        if let Err(e) = capture(&server, epoch, &what) {
            println!("Sound: {e}; trying again in 5 s.");
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

fn capture(server: &Server, epoch: u16, what: &Target) -> Result<(), String> {
    let mut eld = Eld::open()?;
    let (source, app) = match what {
        Target::App(_, app) => ("app", Some(app.as_str())),
        _ => ("desktop", None),
    };
    let format = format_payload(&eld, epoch, source, app);
    let input = match what {
        // pipewire-pulse's monitor of the default output: what this machine plays.
        Target::Everything => "pulsesrc device=@DEFAULT_MONITOR@ buffer-time=40000 latency-time=10000".to_string(),
        // That stream alone; never another node when it goes (a microphone, say): the capture
        // then stays silent until the next look finds it gone.
        Target::App(serial, _) => format!(
            "pipewiresrc target-object={serial} stream-properties=\"props,node.dont-reconnect=true,node.dont-fallback=true\""
        ),
        Target::Nothing => return Ok(()),
    };
    let pipeline = gst::parse::launch(&format!(
        "{input} ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! appsink name=sink sync=false max-buffers=50"
    ))
    .map_err(|e| format!("no capture ({e})"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "no pipeline")?;
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no appsink")?;
    pipeline.set_state(gst::State::Playing).map_err(|e| format!("the capture did not start ({e})"))?;
    println!("Sound: on (epoch {epoch}).");
    let mut pcm: Vec<i16> = Vec::new();
    let mut seq: u32 = 0;
    let mut stamps = Stamps::default();
    let mut looked = std::time::Instant::now();
    let result = loop {
        if !server.wants_audio() {
            break Ok(());
        }
        if looked.elapsed() >= Duration::from_secs(2) {
            looked = std::time::Instant::now();
            if target(server) != *what {
                break Ok(());
            }
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
            if sink.is_eos() {
                break Err("the capture ended".into());
            }
            continue;
        };
        let Some(buffer) = sample.buffer() else { continue };
        let Ok(map) = buffer.map_readable() else { continue };
        pcm.extend(map.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        let now = crate::protocol::now();
        while pcm.len() >= FRAMES * 2 {
            // Its first frame's time: now, less what is held from it on.
            let read = now - pcm.len() as f64 / 2.0 / RATE as f64;
            let packet = eld.encode(&pcm[..FRAMES * 2]);
            pcm.drain(..FRAMES * 2);
            if packet.is_empty() {
                continue;
            }
            let at = stamps.stamp(seq, read);
            server.broadcast_audio(epoch, &format, &packet_payload(epoch, seq, seq == 0, &packet), at);
            seq += 1;
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    println!("Sound: off after {seq} packets ({:.1} s).", seq as f64 * FRAMES as f64 / RATE as f64);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_is_apples_esds() {
        // AudioToolbox's own for AAC-LC at 44.1 kHz stereo, 128 kbps (its ASC 12 10).
        let apple = "03808080220000000480808014401500180000 01f4000001f40005808080021210068080800102".replace(' ', "");
        let ours: String = esds(&[0x12, 0x10]).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(ours, apple);
    }

    #[test]
    fn stamps_follow_the_wall_clock_not_a_late_buffer() {
        let mut s = Stamps::default();
        let tick = FRAMES as f64 / RATE as f64;
        // Even readings: exactly 10 ms apart, with one buffer 30 ms late now and then.
        for seq in 0..300u32 {
            let late = if seq % 37 == 5 { 0.03 } else { 0.0 };
            assert!((s.stamp(seq, 100.0 + seq as f64 * tick + late) - (100.0 + seq as f64 * tick)).abs() < 1e-9);
        }
        // A sound card 1% slow (its 10 ms are 10.1 ms of the wall clock; a real one is 0.01%): the
        // stamps are moved each second once its earliest reading is 20 ms off, so they stay within
        // 20 ms and two seconds' drift of the wall clock, for good.
        let mut s = Stamps::default();
        for seq in 0..10_000u32 {
            let wall = 100.0 + seq as f64 * tick * 1.01;
            assert!((s.stamp(seq, wall) - wall).abs() < 0.02 + 2.0 * 0.01 + 1e-9, "seq {seq}");
        }
    }

    #[test]
    fn the_windows_app_stream_by_process_or_a_child() {
        // Firefox (window 100) plays from a child (101); another app (200) plays too.
        let dump = serde_json::json!([
            {"id": 30, "type": "PipeWire:Interface:Client", "info": {"props": {"application.process.id": 101}}},
            {"id": 31, "type": "PipeWire:Interface:Client", "info": {"props": {"application.process.id": "200"}}},
            {"id": 40, "type": "PipeWire:Interface:Node", "info": {"state": "running", "props":
                {"media.class": "Stream/Output/Audio", "client.id": 31, "object.serial": 7, "application.name": "Music"}}},
            {"id": 41, "type": "PipeWire:Interface:Node", "info": {"state": "idle", "props":
                {"media.class": "Stream/Output/Audio", "client.id": 30, "object.serial": 8, "application.name": "Firefox"}}},
            {"id": 42, "type": "PipeWire:Interface:Node", "info": {"state": "running", "props":
                {"media.class": "Stream/Output/Audio", "client.id": 30, "object.serial": 9, "application.name": "Firefox"}}},
            {"id": 43, "type": "PipeWire:Interface:Node", "info": {"state": "running", "props":
                {"media.class": "Stream/Input/Audio", "client.id": 30, "object.serial": 10, "application.name": "Firefox"}}},
        ]);
        let parent = |p: u32| match p { 101 => Some(100), 100 | 200 => Some(1), _ => None };
        assert_eq!(app_stream(&dump, 100, parent), Some((9, "Firefox".into())), "its running playback, not its capture");
        assert_eq!(app_stream(&dump, 200, parent), Some((7, "Music".into())));
        assert_eq!(app_stream(&dump, 300, parent), None, "an app that plays nothing");
        assert_eq!(super::parent(std::process::id()), Some(std::os::unix::process::parent_id()), "/proc's parent");
    }

    #[test]
    fn the_wire() {
        assert_eq!(packet_payload(0x0102, 7, true, &[9, 8]), [2, 1, 2, 0, 0, 0, 7, 1, 1, 0, 2, 9, 8]);
        assert_eq!(packet_payload(1, 8, false, &[5])[7], 0);
    }

    #[test]
    fn eld_packets_when_the_library_is_here() {
        let Ok(mut eld) = Eld::open() else { return }; // no libfdk-aac2 on this machine
        assert_eq!((eld.priming, eld.cookie.first()), (240, Some(&0xf8)), "AOT 39's escape, 48 kHz");
        let tone: Vec<i16> = (0..FRAMES * 2 * 20).map(|i| ((i / 2) as f64 * 0.05).sin().mul_add(8000.0, 0.0) as i16).collect();
        let sizes: Vec<usize> = tone.chunks(FRAMES * 2).map(|c| eld.encode(c).len()).collect();
        let total: usize = sizes.iter().sum();
        // 128 kbps over 10 ms: 160 bytes a packet on average.
        assert!((1000..5000).contains(&total), "{sizes:?}");
        let f: serde_json::Value = serde_json::from_slice(&format_payload(&eld, 3, "app", Some("Firefox"))[1..]).unwrap();
        assert_eq!((f["source"].as_str(), f["app"].as_str()), (Some("app"), Some("Firefox")));
        assert_eq!((f["codec"].as_str(), f["epoch"].as_i64(), f["framesPerPacket"].as_i64()), (Some("aac-eld"), Some(3), Some(480)));
    }
}
