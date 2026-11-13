//! This machine's sound to the devices (kind 43, Sill's `Audio.swift`): the default output's
//! monitor (everything playing) → 48 kHz stereo → AAC-ELD, 480 frames (10 ms) a packet, 128 kbps
//! → a format (type 1, JSON) per epoch, then one packet a message (type 2). Only to a device whose
//! hello lists "aac-eld", and only while something streams.
//!
//! The encoder is libfdk-aac's, opened at run time (`libfdk-aac.so.2`, Ubuntu's libfdk-aac2 in
//! universe, which PipeWire's Bluetooth AAC already installs; the Fedora-stripped build keeps
//! AAC-ELD): no build dependency, and no sound where it is missing.
// shortcut: a spike — the Desktop's sound for any stream (a window's app alone needs PipeWire's
// per-app nodes), always on (no Send Audio switch), packets timed by the wall clock at capture
// with no drift correction; upgrade after the sound is heard on a device.

use crate::server::Server;
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

/// Kind 43 type 1: how to decode the epoch's packets.
pub fn format_payload(eld: &Eld, epoch: u16) -> Vec<u8> {
    let json = serde_json::json!({
        "codec": CODEC, "sampleRate": RATE, "channels": 2, "framesPerPacket": FRAMES,
        "primingFrames": eld.priming, "bitrate": BITRATE, "epoch": epoch,
        "cookie": crate::pairing::b64(&esds(&eld.cookie)), "source": "desktop",
    });
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

/// The sound while a device that plays it watches a stream; started again after each pause, a
/// new epoch each time.
pub fn run(server: Server) {
    if let Err(e) = Eld::open() {
        return println!("Sound: none to devices ({e}).");
    }
    println!("Sound: this machine's to devices that play it (AAC-ELD, 128 kbps).");
    let mut epoch: u16 = 0;
    loop {
        while !server.wants_audio() {
            std::thread::sleep(Duration::from_millis(500));
        }
        epoch = epoch.wrapping_add(1);
        if let Err(e) = capture(&server, epoch) {
            println!("Sound: {e}; trying again in 5 s.");
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

fn capture(server: &Server, epoch: u16) -> Result<(), String> {
    let mut eld = Eld::open()?;
    let format = format_payload(&eld, epoch);
    // pipewire-pulse's monitor of the default output: what this machine plays, whatever plays it.
    let pipeline = gst::parse::launch(
        "pulsesrc device=@DEFAULT_MONITOR@ buffer-time=40000 latency-time=10000 ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! appsink name=sink sync=false max-buffers=50",
    )
    .map_err(|e| format!("no capture ({e})"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "no pipeline")?;
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no appsink")?;
    pipeline.set_state(gst::State::Playing).map_err(|e| format!("the capture did not start ({e})"))?;
    println!("Sound: on (epoch {epoch}).");
    let mut pcm: Vec<i16> = Vec::new();
    let mut seq: u32 = 0;
    let mut first: Option<f64> = None;
    let result = loop {
        if !server.wants_audio() {
            break Ok(());
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
            if sink.is_eos() {
                break Err("the capture ended".into());
            }
            continue;
        };
        let Some(buffer) = sample.buffer() else { continue };
        let Ok(map) = buffer.map_readable() else { continue };
        let samples: Vec<i16> = map.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        // The first frame's time: now, less what this buffer and the ones before it held.
        first.get_or_insert_with(|| crate::protocol::now() - (pcm.len() + samples.len()) as f64 / 2.0 / RATE as f64);
        pcm.extend(samples);
        while pcm.len() >= FRAMES * 2 {
            let packet = eld.encode(&pcm[..FRAMES * 2]);
            pcm.drain(..FRAMES * 2);
            if packet.is_empty() {
                continue;
            }
            let at = first.unwrap() + seq as f64 * FRAMES as f64 / RATE as f64;
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
        let f: serde_json::Value = serde_json::from_slice(&format_payload(&eld, 3)[1..]).unwrap();
        assert_eq!((f["codec"].as_str(), f["epoch"].as_i64(), f["framesPerPacket"].as_i64()), (Some("aac-eld"), Some(3), Some(480)));
    }
}
