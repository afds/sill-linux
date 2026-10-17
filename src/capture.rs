//! The Desktop: the screen-cast portal hands over a PipeWire stream of one monitor, and a
//! GStreamer pipeline turns it into HEVC access units for the server.
//!
//! pipewiresrc (DMA-BUF for NVENC, else shared memory) → valve (closed while nobody watches) → scaler and `size` filter (Retina or
//! Standard, on the GPU for NVENC) → HEVC encoder (NVENC, else VA-API, else x265; Main profile, no
//! B-frames, low latency) → h265parse (parameter sets before every IDR, one access unit per
//! buffer, Annex-B) → appsink → `protocol::convert_annex_b` → `Server::broadcast_access_unit`.
//!
//! The portal asks once; its restore token (persist mode 2) is kept in
//! `$XDG_STATE_HOME/sill-linux/restore-token`, so later runs start without the dialog.

use crate::protocol;
use crate::input::{Action, CommandKey, Translator};
use crate::server::{Apply, Control, HostWindow, InputCmd, Server, Source};
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::Session;
use std::collections::HashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use ashpd::enumflags2::BitFlags;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A keyframe asked for less than this after the last one is folded into it.
const KEYFRAME_SPACING: Duration = Duration::from_millis(250);

struct PortalCast {
    /// The portal's PipeWire remote; None: this process's own PipeWire connection (Mutter's API).
    fd: Option<OwnedFd>,
    node: u32,
    size: Option<(i32, i32)>,
}

/// The size `pipewiresrc` hands over (the stream's own pixels), width << 32 | height: what the
/// portal's absolute motion is given in. 0 before the first frame.
static CAPTURED_SIZE: AtomicU64 = AtomicU64::new(0);

/// The region a window stream's buffers show (Mutter draws a window into a buffer the size of the
/// stream and says where, as each buffer's crop): x, y, width, height, 16 bits each; 0 for none.
static CROP: AtomicU64 = AtomicU64::new(0);

fn pack_crop((x, y, w, h): (u32, u32, u32, u32)) -> u64 {
    ((x as u64 & 0xffff) << 48) | ((y as u64 & 0xffff) << 32) | ((w as u64 & 0xffff) << 16) | (h as u64 & 0xffff)
}

fn unpack_crop(v: u64) -> Option<(u32, u32, u32, u32)> {
    (v != 0).then(|| ((v >> 48) as u32 & 0xffff, (v >> 32) as u32 & 0xffff, (v >> 16) as u32 & 0xffff, v as u32 & 0xffff))
}

/// The part of the buffers the devices see (a window's crop): positions from a device are
/// fractions of it. None: the whole buffer.
static INPUT_REGION: std::sync::Mutex<Option<(f64, f64, f64, f64)>> = std::sync::Mutex::new(None);

fn input_region() -> Option<(f64, f64, f64, f64)> {
    *INPUT_REGION.lock().unwrap()
}

/// The streamed window's frame inside its buffer (from the extension, kept current by
/// `watch_windows`), so the picture leaves out the window's shadow.
static FRAME_INSETS: std::sync::Mutex<Option<(u64, crate::shell::Insets)>> = std::sync::Mutex::new(None);

/// A window's crop waits this long without changing before the encoder follows it (a resize
/// changes it with every frame).
const CROP_SETTLE: Duration = Duration::from_millis(200);
/// A window stream whose buffers carry no crop streams whole after this.
const CROP_WAIT: Duration = Duration::from_millis(500);

fn captured_size() -> Option<(f64, f64)> {
    let v = CAPTURED_SIZE.load(Ordering::Relaxed);
    (v != 0).then(|| ((v >> 32) as f64, (v & 0xffff_ffff) as f64))
}

/// `input`: the Command key's meaning, or None for view only (no remote control asked for).
pub fn run(server: &Server, control: Receiver<Control>, input: Option<CommandKey>) -> Result<()> {
    let (mut cast, input_tx, recorder) = start_capture(server.clone(), input)?;
    if let Some(tx) = input_tx {
        server.set_input(tx);
    }
    match cast.size {
        Some((w, h)) => println!("Screen cast: PipeWire node {}, {w}×{h}.", cast.node),
        None => println!("Screen cast: PipeWire node {}.", cast.node),
    }

    gst::init().context("GStreamer init")?;
    let mut apply = server.current_apply();
    let (formats, encoder_desc, name) = encoder(apply.kbps())?;
    server.set_software_encoder(name.contains("software"));
    let gl = name.starts_with("NVENC");
    // NVENC takes the compositor's DMA-BUF straight into GL (zero copy; also the only way frames
    // keep coming while a fullscreen window is shown by direct scanout, mutter #3074), falling back
    // to shared memory if that does not negotiate. SILL_LINUX_SHM=1 forces shared memory.
    let mut dmabuf = gl && std::env::var_os("SILL_LINUX_SHM").is_none();
    let mut streaming = false;
    let mut source = Source::Desktop;
    let mut announced = None;
    loop {
        if announced != Some(dmabuf) {
            println!("Encoder: {name}, {}.", if dmabuf { "DMA-BUF through GL" } else { "shared memory" });
            announced = Some(dmabuf);
        }
        // Standard is the monitor's logical size; a window streams at its own size.
        let logical = if source == Source::Desktop { cast.size } else { None };
        let shape = TailShape { gl, dmabuf, formats, encoder: encoder_desc.clone(), logical, crop: None };
        let switch_to = match run_pipeline(server, &control, &cast, &shape, &mut apply, &mut streaming, source, recorder.as_ref())? {
            Outcome::Ended => break,
            Outcome::NoDmaBuf(why) if dmabuf && source == Source::Desktop => {
                println!("DMA-BUF did not work here ({why}); using shared memory.");
                dmabuf = false;
                continue;
            }
            Outcome::NoDmaBuf(why) if source == Source::Desktop => bail!(why),
            Outcome::NoDmaBuf(why) | Outcome::SourceEnded(why) => {
                println!("The window's stream ended ({why}); back to the Desktop.");
                server.showing_desktop();
                Source::Desktop
            }
            Outcome::Switch(s) => s,
        };
        let Some(rec) = recorder.as_ref() else {
            println!("Windows stream only through GNOME's own screen-cast API (not with --portal); the Desktop keeps streaming.");
            continue;
        };
        match record(rec, switch_to) {
            Ok(c) => {
                match c.size {
                    Some((w, h)) => println!("Screen cast: PipeWire node {}, {w}×{h}.", c.node),
                    None => println!("Screen cast: PipeWire node {}.", c.node),
                }
                cast = c;
                source = switch_to;
            }
            Err(e) => {
                println!("Could not stream that window: {e:#}; back to the Desktop.");
                // The old stream still runs when the new one failed; a fresh one either way.
                cast = record(rec, Source::Desktop)?;
                source = Source::Desktop;
                server.showing_desktop();
            }
        }
    }
    drop(cast);
    Ok(())
}

enum Outcome {
    Ended,
    /// A device picked another source.
    Switch(Source),
    /// A window's stream ended (the window closed).
    SourceEnded(String),
    /// The pipeline failed before its first frame (with DMA-BUF: try shared memory).
    NoDmaBuf(String),
}

fn run_pipeline(
    server: &Server,
    control: &Receiver<Control>,
    cast: &PortalCast,
    shape: &TailShape,
    apply: &mut Apply,
    streaming: &mut bool,
    source: Source,
    recorder: Option<&Recorder>,
) -> Result<Outcome> {
    let mut shape = shape.clone();
    let shape = &mut shape;
    let window = source != Source::Desktop;
    CROP.store(0, Ordering::Relaxed);
    CAPTURED_SIZE.store(0, Ordering::Relaxed);
    *INPUT_REGION.lock().unwrap() = None;
    // The source half stays for the whole run: the PipeWire stream and the valve (closed while
    // nobody watches; it passes caps on, so opening it renegotiates nothing). The fd stays open in
    // `cast` for as long as the pipeline runs (pipewiresrc dups it).
    let memory = if shape.dmabuf { "keepalive-time=500 ! video/x-raw(memory:DMABuf)" } else { "always-copy=true keepalive-time=500 ! video/x-raw" };
    // The upload and colour conversion stay too (a new glupload would not take the running
    // DMA-BUF format): a new tail starts from plain RGBA in GL memory, or raw video.
    let head = if shape.gl {
        // From DMA-BUF, glupload's direct import (an external-oes texture) is the one that gives
        // EGL the buffer's modifier: NVIDIA's buffers are tiled, and the other import read them as
        // linear (garbage, 2026-10-17).
        let upload = if shape.dmabuf { "glupload ! video/x-raw(memory:GLMemory),texture-target=external-oes" } else { "glupload" };
        format!("{upload} ! glcolorconvert ! capsfilter name=head caps=video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D")
    } else {
        let first = shape.formats.split(',').next().unwrap_or("I420");
        format!("videoconvert ! capsfilter name=head caps=video/x-raw,format={first}")
    };
    let description = format!(
        "pipewiresrc {fd} path={node} {memory} ! valve name=gate drop={drop} drop-mode=forward-sticky-events ! {head}",
        fd = cast.fd.as_ref().map(|f| format!("fd={}", f.as_raw_fd())).unwrap_or_default(),
        node = cast.node,
        drop = true,
    );
    let pipeline = gst::parse::launch(&description)
        .context("building the pipeline")?
        .dynamic_cast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let gate = pipeline.by_name("gate").ok_or_else(|| anyhow!("no valve"))?;
    let head = pipeline.by_name("head").ok_or_else(|| anyhow!("no head"))?;
    if let Some(pad) = head.static_pad("src") {
        // A new tail asks upstream to renegotiate as it links: stopped here, the half that stays
        // keeps its caps (a renegotiating pipewiresrc failed or reconnected).
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| match info.event() {
            Some(e) if e.type_() == gst::EventType::Reconfigure => gst::PadProbeReturn::Drop,
            _ => gst::PadProbeReturn::Ok,
        });
    }

    // Counters for the once-a-second line (buffers out of pipewiresrc, encoded units), the
    // captured size for input, and the frame-rate limit (frames sooner than it are dropped before
    // the encoder; the compositor sends at most its refresh rate anyway).
    let captured = Arc::new(AtomicU64::new(0));
    let encoded = Arc::new(AtomicU64::new(0));
    let encoded_total = Arc::new(AtomicU64::new(0));
    let min_interval_us = Arc::new(AtomicU64::new(0));
    if let Some(pad) = gate.static_pad("sink") {
        let c = captured.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            c.fetch_add(1, Ordering::Relaxed);
            if let Some(m) = info.buffer().and_then(|b| b.meta::<gst_video::VideoCropMeta>()) {
                let (x, y, w, h) = m.rect();
                CROP.store(pack_crop((x, y, w & !1, h & !1)), Ordering::Relaxed);
            }
            if let Some(s) = pad.current_caps().and_then(|c| c.structure(0).map(|s| s.to_owned())) {
                if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                    CAPTURED_SIZE.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    if let Some(pad) = gate.static_pad("src") {
        // A new tail asks upstream to renegotiate as it links; pipewiresrc would then renegotiate
        // its PipeWire stream (and fail, or reconnect): the tail takes the caps as they are.
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| match info.event() {
            Some(e) if e.type_() == gst::EventType::Reconfigure => gst::PadProbeReturn::Drop,
            _ => gst::PadProbeReturn::Ok,
        });
        let interval = min_interval_us.clone();
        let last = Arc::new(std::sync::Mutex::new(None::<Instant>));
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let min = Duration::from_micros(interval.load(Ordering::Relaxed));
            let mut last = last.lock().unwrap();
            let now = Instant::now();
            if last.is_some_and(|l| now.duration_since(l) < min) {
                return gst::PadProbeReturn::Drop;
            }
            *last = Some(now);
            gst::PadProbeReturn::Ok
        });
    }
    let set_rate = |a: &Apply| {
        // 90 % of the frame interval: a frame a little early is not dropped.
        min_interval_us.store(900_000 / a.fps.max(1) as u64, Ordering::Relaxed);
    };
    set_rate(apply);
    let counters = (encoded.clone(), encoded_total.clone());
    let mut tail = build_tail(shape, apply, server, &counters)?;
    pipeline.add(&tail.bin)?;
    head.link(&tail.bin).context("linking the encoder")?;
    describe(shape, apply);
    let mut last_stats = Instant::now();

    // The system clock, not the one pipewiresrc offers: PipeWire's stops while the compositor sends
    // nothing (a still screen), and pipewiresrc's keep-alive resend waits on the pipeline clock.
    pipeline.use_clock(Some(&gst::SystemClock::obtain()));
    pipeline.set_state(gst::State::Playing).context("starting the pipeline")?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    let mut last_keyframe = Instant::now() - KEYFRAME_SPACING;
    let mut keyframe_pending = *streaming;
    // A window streams once its region is known (its first frame would be the whole buffer).
    let mut waiting_crop = window;
    let started = Instant::now();
    let mut seen_crop: (u64, Instant) = (0, Instant::now());
    let mut gate_open = false;
    let result = loop {
        match control.recv_timeout(Duration::from_millis(50)) {
            Ok(Control::Start(s)) if s != source => {
                *streaming = true;
                break Ok(Outcome::Switch(s));
            }
            Ok(Control::WindowCommand(id, action)) => {
                if let Some(rec) = recorder {
                    let _ = rec.send(RecReq::Command(id, action));
                }
            }
            Ok(Control::Launch(app)) => {
                if let Some(rec) = recorder {
                    let _ = rec.send(RecReq::Launch(app));
                }
            }
            Ok(Control::Fit(id, w, h)) => {
                if let Some(rec) = recorder {
                    let _ = rec.send(RecReq::Fit(id, w, h));
                }
            }
            Ok(Control::Start(_)) => {
                *streaming = true;
                keyframe_pending = true;
            }
            Ok(Control::Stop) => {
                *streaming = false;
                if let Some(rec) = recorder {
                    let _ = rec.send(RecReq::Release);
                }
            }
            Ok(Control::Keyframe) => keyframe_pending = true,
            Ok(Control::Apply(a)) if a != *apply => {
                let new_size = a.retina != apply.retina;
                *apply = a;
                set_rate(apply);
                if new_size {
                    // A resolution change replaces the tail: changing the size live failed in
                    // GStreamer (the scaler would not renegotiate mid-stream, then pipewiresrc
                    // renegotiated and the capture ended, Retina → Standard, 2026-10-17).
                    gate_open = false;
                    match replace_tail(&pipeline, &gate, &head, tail, shape, apply, server, &counters) {
                        Ok(t) => tail = t,
                        Err(e) => break Err(e.context("changing the resolution")),
                    }
                } else {
                    tail.enc.set_property("bitrate", apply.kbps());
                }
                describe(shape, apply);
                keyframe_pending = true;
            }
            Ok(Control::Apply(_)) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(Outcome::Ended),
        }
        if window {
            let now = Instant::now();
            let observed = CROP.load(Ordering::Relaxed);
            if observed != seen_crop.0 {
                seen_crop = (observed, now);
            }
            let insets = match source {
                Source::Window(id) => FRAME_INSETS.lock().unwrap().filter(|(w, _)| *w == id).map(|(_, i)| i),
                Source::Desktop => None,
            };
            let wanted = unpack_crop(observed)
                .filter(|c| c.2 >= 2 && c.3 >= 2)
                .map(|c| insets.map(|i| i.frame_in(c)).unwrap_or(c));
            let have = shape.crop.map(|c| c.0);
            if wanted.is_some() && wanted != have && now.duration_since(seen_crop.1) >= CROP_SETTLE {
                let (x, y, w, h) = wanted.unwrap();
                let buffers = captured_size().map(|(bw, bh)| (bw as u32, bh as u32)).unwrap_or((x + w, y + h));
                // Standard: the window's logical size (the buffers' pixels over the monitor's scale).
                let ratio = match cast.size {
                    Some((lw, _)) if lw > 0 => (buffers.0 as f64 / lw as f64).max(1.0),
                    _ => 1.0,
                };
                let even = |v: f64| ((v.round() as i32) & !1).max(2);
                shape.crop = Some(((x, y, w, h), buffers));
                // A window stream takes positions in the window's logical buffer coordinates (a
                // monitor stream in its own pixels): at scale 2 the frame is half the crop.
                let region = insets.map(|i| i.frame_logical()).unwrap_or((0.0, 0.0, w as f64 / ratio, h as f64 / ratio));
                *INPUT_REGION.lock().unwrap() = Some(region);
                shape.logical = Some((even(region.2), even(region.3)));
                gate_open = false;
                match replace_tail(&pipeline, &gate, &head, tail, shape, apply, server, &counters) {
                    Ok(t) => tail = t,
                    Err(e) => break Err(e.context("cropping to the window")),
                }
                describe(shape, apply);
                waiting_crop = false;
                keyframe_pending = true;
            } else if waiting_crop && wanted.is_none() && captured_size().is_some() && started.elapsed() >= CROP_WAIT {
                waiting_crop = false; // no crop: the buffers are the window
            }
        }
        let open = *streaming && !waiting_crop;
        if open != gate_open {
            gate.set_property("drop", !open);
            gate_open = open;
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let (cap, enc) = (captured.swap(0, Ordering::Relaxed), encoded.swap(0, Ordering::Relaxed));
            if *streaming {
                let inputs = INPUT_EVENTS.swap(0, Ordering::Relaxed);
                println!("[1s] cap {cap} enc {enc} {} in {inputs}", server.take_stats());
            }
        }
        if keyframe_pending && last_keyframe.elapsed() >= KEYFRAME_SPACING {
            keyframe_pending = false;
            last_keyframe = Instant::now();
            let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
            tail.sink.send_event(event);
        }
        let mut failure = None;
        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(e) => failure = Some(format!("{} ({:?})", e.error(), e.debug())),
                MessageView::Eos(_) => failure = Some("the screen cast ended (the portal session was closed)".into()),
                _ => {}
            }
        }
        if let Some(why) = failure {
            if source != Source::Desktop {
                break Ok(Outcome::SourceEnded(why));
            }
            if encoded_total.load(Ordering::Relaxed) == 0 {
                break Ok(Outcome::NoDmaBuf(why));
            }
            break Err(anyhow!(why));
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// What every tail is made of.
#[derive(Clone)]
struct TailShape {
    gl: bool,
    dmabuf: bool,
    formats: &'static str,
    encoder: String,
    /// Standard's size; None: the stream's own.
    logical: Option<(i32, i32)>,
    /// A window's region in the buffers (even sizes) and the buffers' size.
    crop: Option<((u32, u32, u32, u32), (u32, u32))>,
}

/// The crop of a GL texture: the output's texture coordinates mapped into the region.
const CROP_SHADER: &str = "#ifdef GL_ES
precision mediump float;
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;
uniform float cx;
uniform float cy;
uniform float cw;
uniform float ch;
void main () {
  gl_FragColor = texture2D (tex, vec2 (cx + v_texcoord.x * cw, cy + v_texcoord.y * ch));
}
";

/// The half after the valve: scaler, size, encoder, parser, appsink. Rebuilt for a resolution
/// change.
struct Tail {
    bin: gst::Bin,
    enc: gst::Element,
    sink: gst_app::AppSink,
}

fn describe(shape: &TailShape, a: &Apply) {
    let res = match (a.retina, shape.logical) {
        (false, Some((w, h))) => format!("Standard ({w}×{h})"),
        (false, None) => "Standard".to_string(),
        (true, _) => "Retina (the stream's own size)".to_string(),
    };
    let res = match shape.crop {
        Some(((_, _, w, h), _)) => format!("{res}, the window's {w}×{h}"),
        None => res,
    };
    println!("Stream settings: {res}, up to {} fps, {} Mbit/s.", a.fps, a.kbps() / 1000);
}

fn build_tail(shape: &TailShape, a: &Apply, server: &Server, counters: &(Arc<AtomicU64>, Arc<AtomicU64>)) -> Result<Tail> {
    let size = match (a.retina, shape.logical) {
        (false, Some((w, h))) => format!(",width={w},height={h}"),
        _ => String::new(),
    };
    let scale = match (shape.gl, shape.crop) {
        (true, None) => format!("glcolorscale ! video/x-raw(memory:GLMemory),format=RGBA{size}"),
        (true, Some(((_, _, w, h), _))) => format!(
            "glshader name=crop ! video/x-raw(memory:GLMemory),format=RGBA,width={w},height={h} \
             ! glcolorscale ! video/x-raw(memory:GLMemory),format=RGBA{size}"
        ),
        (false, crop) => {
            let first = shape.formats.split(',').next().unwrap_or("I420");
            let cut = match crop {
                Some(((x, y, w, h), (bw, bh))) => {
                    format!("videocrop left={x} top={y} right={} bottom={} ! ", bw.saturating_sub(x + w), bh.saturating_sub(y + h))
                }
                None => String::new(),
            };
            format!("{cut}videoscale ! video/x-raw,format={first}{size}")
        }
    };
    // The encoder's bitrate is the one asked for now (the template carries the first one).
    let encoder = shape.encoder.clone();
    let description = format!(
        "{scale} ! {encoder} ! video/x-h265,profile=main ! h265parse config-interval=-1 \
         ! video/x-h265,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false max-buffers=4 drop=false"
    );
    let bin = gst::parse::bin_from_description(&description, true).context("building the encoder")?;
    let enc = bin.by_name("enc").ok_or_else(|| anyhow!("no encoder"))?;
    enc.set_property("bitrate", a.kbps());
    if let (Some(shader), Some(((x, y, w, h), (bw, bh)))) = (bin.by_name("crop"), shape.crop) {
        let f = |v: u32, of: u32| v as f32 / of.max(1) as f32;
        shader.set_property("fragment", CROP_SHADER);
        let uniforms = gst::Structure::builder("uniforms")
            .field("cx", f(x, bw))
            .field("cy", f(y, bh))
            .field("cw", f(w, bw))
            .field("ch", f(h, bh))
            .build();
        shader.set_property("uniforms", uniforms);
    }
    let sink = bin
        .by_name("sink")
        .and_then(|e| e.dynamic_cast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow!("no appsink"))?;
    let frames_server = server.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(s) = sample.caps().and_then(|c| c.structure(0)) {
                    if let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height")) {
                        frames_server.set_encoded_size(w as i64, h as i64);
                    }
                }
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        frames_server.broadcast_access_unit(&protocol::convert_annex_b(map.as_slice()));
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    if let Some(pad) = sink.static_pad("sink") {
        let (c, total) = (counters.0.clone(), counters.1.clone());
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            total.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    Ok(Tail { bin, enc, sink })
}

/// Swaps the tail while the source runs: the valve closed, the old tail stopped and removed, the
/// new one linked and started; the caller opens the valve again if devices watch. The valve's sticky
/// events (caps included) go to the new tail with its first buffer.
#[allow(clippy::too_many_arguments)]
fn replace_tail(
    pipeline: &gst::Pipeline,
    gate: &gst::Element,
    head: &gst::Element,
    old: Tail,
    shape: &TailShape,
    a: &Apply,
    server: &Server,
    counters: &(Arc<AtomicU64>, Arc<AtomicU64>),
) -> Result<Tail> {
    gate.set_property("drop", true);
    // A buffer already past the valve finishes its trip through the old tail first.
    std::thread::sleep(Duration::from_millis(40));
    head.unlink(&old.bin);
    old.bin.set_state(gst::State::Null).context("stopping the old encoder")?;
    pipeline.remove(&old.bin)?;
    let tail = build_tail(shape, a, server, counters)?;
    pipeline.add(&tail.bin)?;
    // Linked without the up-front caps check (a GL element with no context yet cannot vouch for
    // GL caps); the caps are still checked when the first buffer's arrive.
    let (src, sink) = (head.static_pad("src"), tail.bin.static_pad("sink"));
    match (src, sink) {
        (Some(src), Some(sink)) => {
            src.link_full(&sink, gst::PadLinkCheck::empty()).map_err(|e| anyhow!("linking the new encoder: {e:?}"))?;
        }
        _ => bail!("linking the new encoder: no pads"),
    }
    tail.bin.sync_state_with_parent().context("starting the new encoder")?;
    Ok(tail)
}

/// The first HEVC encoder this machine has: the input formats it takes, the element with its
/// settings, and a name for the log.
fn encoder(kbps: u32) -> Result<(&'static str, String, &'static str)> {
    // SILL_LINUX_ENCODER=nv|va|x265 picks one (testing); otherwise the first this machine has.
    let only = std::env::var("SILL_LINUX_ENCODER").ok();
    let have = |name: &str| {
        let wanted = match only.as_deref() {
            Some("nv") => name == "nvh265enc",
            Some("va") => name == "vah265enc",
            Some("x265") => name == "x265enc",
            _ => true,
        };
        wanted && gst::ElementFactory::find(name).is_some()
    };
    let gop = 240; // Sill: a keyframe at least every fps × 4 frames
    if have("nvh265enc") {
        return Ok((
            "BGRx,BGRA,RGBx,RGBA,NV12",
            format!(
                "nvh265enc name=enc preset=p1 tune=ultra-low-latency zerolatency=true bframes=0 gop-size={gop} \
                 rc-mode=cbr bitrate={kbps} repeat-sequence-header=true"
            ),
            "NVENC (nvh265enc)",
        ));
    }
    if have("vah265enc") {
        return Ok((
            "NV12",
            format!("vah265enc name=enc b-frames=0 key-int-max={gop} rate-control=cbr bitrate={kbps} target-usage=7"),
            "VA-API (vah265enc)",
        ));
    }
    if have("x265enc") {
        return Ok((
            "I420",
            format!("x265enc name=enc speed-preset=ultrafast tune=zerolatency key-int-max={gop} bitrate={kbps}"),
            "software x265 (x265enc)",
        ));
    }
    bail!("no HEVC encoder in GStreamer (nvh265enc, vah265enc or x265enc)")
}

fn state_file(name: &str) -> PathBuf {
    crate::identity::state_dir().join(name)
}

fn read_token(path: &PathBuf) -> Option<String> {
    if CHOOSE_SCREEN.load(Ordering::Relaxed) {
        return None;
    }
    std::fs::read_to_string(path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn save_token(path: &PathBuf, token: Option<&str>) {
    if let Some(t) = token {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, t);
    }
}

static FORCE_PORTAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MONITOR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// `--portal`: the portal even where GNOME's own API is here.
pub fn set_force_portal(on: bool) {
    FORCE_PORTAL.store(on, Ordering::Relaxed);
}

/// `--monitor CONNECTOR` (GNOME's API): which monitor is the Desktop; the primary one otherwise.
pub fn set_monitor(connector: Option<String>) {
    *MONITOR.lock().unwrap() = connector;
}

/// Starts the capture on a thread of its own, which then keeps the session (and its D-Bus
/// connection) alive for as long as this process runs; input runs on that thread too. GNOME's own
/// API where it is here (`mutter`), else the portal.
fn start_capture(server: Server, input: Option<CommandKey>) -> Result<(PortalCast, Option<UnboundedSender<InputCmd>>, Option<Recorder>)> {
    let (tx, rx) = mpsc::channel::<Started>();
    std::thread::Builder::new().name("capture".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let use_mutter = !FORCE_PORTAL.load(Ordering::Relaxed)
                && match ashpd::zbus::Connection::session().await {
                    Ok(c) => crate::mutter::available(&c).await,
                    Err(_) => false,
                };
            if use_mutter {
                match mutter_session(server, input).await {
                    Ok((cast, input_tx, recorder, keep)) => {
                        let _ = tx.send(Ok((cast, input_tx, Some(recorder))));
                        let _keep = keep;
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.context("GNOME's screen-cast API (--portal uses the portal instead)")));
                    }
                }
                return;
            }
            portal_thread(input, tx).await;
        });
    })?;
    rx.recv().context("the capture thread ended")?
}

/// GNOME's own API: a remote-desktop session with the monitor's screen cast linked to it. No
/// dialog; GNOME's sharing indicator shows while it runs.
async fn mutter_session(server: Server, input: Option<CommandKey>) -> Result<(PortalCast, Option<UnboundedSender<InputCmd>>, Recorder, impl Send)> {
    let conn = ashpd::zbus::Connection::session().await.context("the session bus")?;
    let wanted = MONITOR.lock().unwrap().clone();
    let connector = crate::mutter::monitor_connector(&conn, wanted.as_deref()).await?;
    let session = crate::mutter::Session::new(conn.clone()).await?;
    let cursor = if SHOW_CURSOR.load(Ordering::Relaxed) { 1 } else { 0 };
    let stream = session.record(crate::mutter::Source::Monitor(&connector), cursor).await?;
    println!("Capture: GNOME's own screen-cast API, monitor {connector}{}.", if input.is_some() { ", with remote control" } else { "" });
    let cast = PortalCast { fd: None, node: stream.node, size: stream.size };
    let shown: Shown = Arc::new(std::sync::Mutex::new((Arc::new(session), stream.clone())));
    let input_tx = input.map(|command| {
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (w, h) = stream.size.map(|(w, h)| (w as f64, h as f64)).unwrap_or((1920.0, 1080.0));
        let sink = InputSink::Mutter { shown: shown.clone() };
        tokio::spawn(run_input(sink, w, h, command, true, input_rx));
        input_tx
    });
    let (rec_tx, rec_rx) = tokio::sync::mpsc::unbounded_channel();
    let shell = crate::shell::Client::connect(&conn).await;
    match &shell {
        Some(s) => {
            println!("Windows: from the Sill for Linux GNOME Shell extension.");
            // This machine's pointer on the devices (kind 26), unless it is in the picture already.
            if cursor == 0 {
                let origin = crate::mutter::monitor_origin(&conn, &connector).await.unwrap_or((0, 0));
                let desktop = stream.size.map(|(w, h)| (origin.0 as f64, origin.1 as f64, w as f64, h as f64));
                tokio::spawn(watch_pointer(server.clone(), s.clone(), desktop));
            }
            if SHARE_CLIPBOARD.load(Ordering::Relaxed) {
                tokio::spawn(watch_clipboard(server.clone(), s.clone()));
            }
            tokio::spawn(watch_windows(server, s.clone()));
        }
        None => println!(
            "Windows: the Sill for Linux GNOME Shell extension is not running, so only the Desktop streams \
             (sill-linux --install-extension, then log out and in)."
        ),
    }
    tokio::spawn(recorder(conn, connector, shown.clone(), shell, cursor, rec_rx));
    Ok((cast, input_tx, rec_tx, shown))
}

/// Requests to the GNOME session's task: record a source, or a window's light from a device.
type Recorder = UnboundedSender<RecReq>;

enum RecReq {
    Record(Source, mpsc::Sender<Result<PortalCast>>),
    Command(u64, String),
    Launch(String),
    /// Aa: a window's frame to this size (logical pixels).
    Fit(u64, i32, i32),
    /// Nothing streams: every window Aa sized goes back.
    Release,
}

/// An app launched from a device, and when: its first window listed within `LAUNCH_WAIT` is picked.
static PENDING_LAUNCH: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);
const LAUNCH_WAIT: Duration = Duration::from_secs(20);

/// Asks the GNOME session for a source's PipeWire stream (from the pipeline's thread).
fn record(rec: &Recorder, source: Source) -> Result<PortalCast> {
    let (tx, rx) = mpsc::channel();
    rec.send(RecReq::Record(source, tx)).map_err(|_| anyhow!("the GNOME session ended"))?;
    rx.recv_timeout(Duration::from_secs(8)).map_err(|_| anyhow!("no answer from the GNOME session"))?
}

/// The session and stream shown; input goes to them.
type Shown = Arc<std::sync::Mutex<(Arc<crate::mutter::Session>, crate::mutter::Stream)>>;

/// Owns what is shown: a fresh session for every switch, the Desktop's too (Mutter starts no
/// stream added to a session already running, and a new consumer of a running stream gets no
/// first frame until something on it changes), the old one stopped once the new one runs.
async fn recorder(conn: ashpd::zbus::Connection, connector: String, shown: Shown, shell: Option<crate::shell::Client>, cursor: u32, mut rx: UnboundedReceiver<RecReq>) {
    let cast = |s: &crate::mutter::Stream| PortalCast { fd: None, node: s.node, size: s.size };
    // Windows Aa sized, with their frames before: each goes back once it no longer streams, as
    // Sill for Mac does (issue #57).
    // shortcut: always the whole old frame, also after the user resized or moved it here; Sill for
    // Mac leaves a window resized meanwhile (WindowRestore), worth it if that bites.
    let mut fitted: HashMap<u64, crate::shell::Frame> = HashMap::new();
    async fn restore(shell: &Option<crate::shell::Client>, fitted: &mut HashMap<u64, crate::shell::Frame>, keep: Option<u64>) {
        let Some(s) = shell else { return };
        let ids: Vec<u64> = fitted.keys().copied().filter(|id| Some(*id) != keep).collect();
        for id in ids {
            let f = fitted.remove(&id).unwrap();
            if s.restore(id, f).await {
                println!("Restored window {id} to {}×{} at {},{}{}.", f.width, f.height, f.x, f.y, if f.maximized { ", maximized" } else { "" });
            }
        }
    }
    while let Some(req) = rx.recv().await {
        match req {
            RecReq::Launch(app) => match &shell {
                Some(s) if s.launch(&app).await => *PENDING_LAUNCH.lock().unwrap() = Some((app, Instant::now())),
                _ => println!("Could not launch {app}."),
            },
            RecReq::Fit(id, w, h) => {
                let Some(s) = &shell else { continue };
                match s.fit(id, w, h).await {
                    Some(before) => {
                        println!("Resized window {id} to {w}×{h} for the device (Aa), from {}×{}.", before.width, before.height);
                        fitted.entry(id).or_insert(before);
                    }
                    None => println!("Window {id} is full screen or gone; not resizing."),
                }
            }
            RecReq::Release => restore(&shell, &mut fitted, None).await,
            RecReq::Record(source, reply) => {
                let keep = match source {
                    Source::Window(id) => Some(id),
                    Source::Desktop => None,
                };
                restore(&shell, &mut fitted, keep).await;
                if let (Source::Window(id), Some(s)) = (source, &shell) {
                    // Raised and focused, as Sill for Mac raises a picked window: keys go to it.
                    s.command("Activate", id).await;
                    *FRAME_INSETS.lock().unwrap() = s.geometry(id).await.map(|i| (id, i));
                }
                let recorded = async {
                    let session = crate::mutter::Session::new(conn.clone()).await?;
                    let stream = match source {
                        Source::Desktop => session.record(crate::mutter::Source::Monitor(&connector), cursor).await,
                        Source::Window(id) => session.record(crate::mutter::Source::Window(id), cursor).await,
                    };
                    match stream {
                        Ok(s) => Ok((session, s)),
                        Err(e) => {
                            session.stop().await;
                            Err(e)
                        }
                    }
                }
                .await;
                match recorded {
                    Ok((session, stream)) => {
                        let _ = reply.send(Ok(cast(&stream)));
                        let old = std::mem::replace(&mut *shown.lock().unwrap(), (Arc::new(session), stream));
                        old.0.stop().await;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            RecReq::Command(id, action) => {
                let method = match action.as_str() {
                    "close" => "Close",
                    "minimize" => "Minimize",
                    "fullScreen" => "ToggleFullscreen",
                    other => {
                        println!("Window command {other:?} is not one this host knows.");
                        continue;
                    }
                };
                match &shell {
                    Some(s) if s.command(method, id).await => {}
                    _ => println!("Window command {action} did not reach the window."),
                }
            }
        }
    }
}

static SHARE_CLIPBOARD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `--no-clipboard`: nothing of the clipboard is shared with devices.
pub fn set_share_clipboard(on: bool) {
    SHARE_CLIPBOARD.store(on, Ordering::Relaxed);
}

/// The clipboard both ways, through the extension: each new copy here goes to the devices that
/// share it (not one a password manager marks, whose text is never read, and not this host's own
/// write of a device's text); a device's text becomes the clipboard here. Only copies made while a
/// device shares it are sent, so what was on the clipboard before is not.
async fn watch_clipboard(server: Server, shell: crate::shell::Client) {
    use futures_util::StreamExt;
    if shell.clipboard_types().await.is_none() {
        println!("Clipboard: the GNOME Shell extension running is older than this host; --install-extension, then log out and in.");
        return;
    }
    let Ok(mut changes) = shell.clipboard_changes().await else { return };
    let (tx, mut writes) = tokio::sync::mpsc::unbounded_channel::<String>();
    server.set_clipboard(tx);
    println!("Clipboard: shared with devices that ask (--no-clipboard: never).");
    // The text this host just wrote: the change it makes is not sent back out.
    let mut own: Option<String> = None;
    loop {
        tokio::select! {
            change = changes.next() => {
                if change.is_none() {
                    return;
                }
                if server.clipboard_subscribers() == 0 {
                    own = None;
                    continue;
                }
                let Some(types) = shell.clipboard_types().await else { continue };
                if types.is_empty() || crate::clipboard::marked(&types) {
                    continue;
                }
                let Some(text) = shell.clipboard_text().await else { continue };
                if own.take().is_some_and(|o| o == text) {
                    continue;
                }
                server.broadcast_clipboard(&text);
            }
            write = writes.recv() => {
                let Some(text) = write else { return };
                if shell.set_clipboard(&text).await {
                    own = Some(text);
                }
            }
        }
    }
}

/// Reads this machine's pointer while devices watch a stream: at the stream's rate while it
/// moves, every 30 ms otherwise (Sill for Mac's rule), against the streamed window's frame or the
/// Desktop's monitor.
/// shortcut: the Desktop's rectangle is read once; a monitor layout change needs a restart.
async fn watch_pointer(server: Server, shell: crate::shell::Client, desktop: Option<(f64, f64, f64, f64)>) {
    let mut moving = false;
    let mut warned = false;
    loop {
        let wait = if moving { Duration::from_micros(1_000_000 / server.current_apply().fps.max(30) as u64) } else { Duration::from_millis(30) };
        tokio::time::sleep(wait).await;
        if server.client_count() == 0 || !server.streaming() {
            moving = false;
            continue;
        }
        let window = server.active_window();
        let Some((p, frame)) = shell.pointer_over(window.unwrap_or(0)).await else {
            if !warned {
                warned = true;
                println!("Pointer: the GNOME Shell extension running is older than this host; --install-extension, then log out and in.");
            }
            continue;
        };
        let rect = if window.is_some() { frame } else { desktop };
        moving = server.pointer_sample(p, rect);
    }
}

/// The window list into the server, again on every change, and thumbnails while devices watch:
/// as Sill for Mac, each at most 208×132, every 2 s for a window that changed or streams, else
/// every 6 s, and all again when a device joins.
async fn watch_windows(server: Server, shell: crate::shell::Client) {
    use futures_util::StreamExt;
    const BOX: u32 = 208;
    let mut changes = match shell.changes().await {
        Ok(c) => Some(c),
        Err(e) => {
            println!("Windows: no change signal from the extension ({e:#}); the list refreshes every 2 s.");
            None
        }
    };
    let mut list: Vec<crate::shell::ShellWindow> = Vec::new();
    let mut stamps: HashMap<u64, (crate::shell::ShellWindow, Instant)> = HashMap::new();
    let mut last_clients = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut failed = false;
    let mut apps_sent = false;
    loop {
        let changed = tokio::select! {
            c = async { match changes.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => c.is_some(),
            _ = tick.tick() => changes.is_none(),
        };
        if changed || list.is_empty() {
            match shell.list().await {
                Ok(l) => {
                    failed = false;
                    list = l;
                    server.set_windows(
                        list.iter()
                            .map(|w| HostWindow {
                                mutter_id: w.id,
                                title: w.title.clone(),
                                app_name: w.app_name.clone(),
                                app_id: w.app_id.clone(),
                                width: w.width as i64,
                                height: w.height as i64,
                            })
                            .collect(),
                    );
                }
                Err(e) if !failed => {
                    failed = true;
                    println!("Windows: the extension did not answer ({e:#}).");
                }
                Err(_) => {}
            }
            if let Some(id) = server.active_window() {
                if let Some(i) = shell.geometry(id).await {
                    *FRAME_INSETS.lock().unwrap() = Some((id, i));
                }
            }
        }
        let pending = PENDING_LAUNCH.lock().unwrap().clone();
        if let Some((app, at)) = pending {
            if at.elapsed() > LAUNCH_WAIT {
                *PENDING_LAUNCH.lock().unwrap() = None;
            } else if let Some(w) = list.iter().rev().find(|w| w.app_id == app) {
                // The topmost of the app's windows: a running app's raised one, or its new one.
                *PENDING_LAUNCH.lock().unwrap() = None;
                server.pick_window(w.id);
            }
        }
        if changed {
            continue;
        }
        if !apps_sent {
            // After the window list, once: the installed apps and their icons (64 px, as Sill
            // for Mac). shortcut: apps installed later show after a restart of this host.
            apps_sent = true;
            match shell.apps().await {
                Ok(apps) => {
                    server.set_apps(apps.clone());
                    let mut n = 0;
                    for (app, _) in &apps {
                        if let Some(png) = shell.app_icon(app, 64).await {
                            server.add_icon(app, png);
                            n += 1;
                        }
                    }
                    println!("Apps: {} installed, {n} icons.", apps.len());
                }
                Err(e) => println!("Apps: the extension did not list them ({e:#}; an older extension: --install-extension, then log out and in)."),
            }
        }
        let clients = server.client_count();
        if clients == 0 {
            stamps.clear();
            last_clients = 0;
            continue;
        }
        if clients > last_clients {
            stamps.clear();
        }
        last_clients = clients;
        stamps.retain(|id, _| list.iter().any(|w| w.id == *id));
        let active = server.active_window();
        let now = Instant::now();
        for w in &list {
            let due = match stamps.get(&w.id) {
                None => true,
                Some((info, at)) => info != w || active == Some(w.id) || now.duration_since(*at) >= Duration::from_secs(6),
            };
            if !due {
                continue;
            }
            stamps.insert(w.id, (w.clone(), now));
            if let Some(png) = shell.thumbnail(w.id, BOX).await {
                server.broadcast_thumbnail(w.id, &png);
            }
        }
    }
}

/// What the capture thread hands back: the first stream, input's channel, the GNOME session's
/// recorder (None on the portal).
type Started = Result<(PortalCast, Option<UnboundedSender<InputCmd>>, Option<Recorder>)>;

async fn portal_thread(input: Option<CommandKey>, tx: mpsc::Sender<Started>) {
    {
        {
            match input {
                Some(command) => match remote_session(command).await {
                    Ok((cast, input_tx, keep)) => {
                        let _ = tx.send(Ok((cast, Some(input_tx), None)));
                        let _keep = keep;
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                },
                None => match screencast_session().await {
                    Ok((cast, keep)) => {
                        let _ = tx.send(Ok((cast, None, None)));
                        let _keep = keep;
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                },
            }
        }
    }
}

/// The pointer in the picture only with `--show-cursor`: like Sill for Mac, the app draws its own
/// where it needs one (the trackpad held upright, Pencil hover), and a cursor in the picture makes
/// the compositor send a frame for every move.
static SHOW_CURSOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static KEEP_ALL_TAPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `--keep-all-taps`: no tap is dropped (see `input::Translator`'s tap filter).
pub fn set_keep_all_taps(keep: bool) {
    KEEP_ALL_TAPS.store(keep, Ordering::Relaxed);
}

fn keep_all_taps() -> bool {
    KEEP_ALL_TAPS.load(Ordering::Relaxed)
}

static CHOOSE_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `--choose-screen`: the saved screen choice is not used; the portal asks again (and the new
/// choice is saved).
pub fn set_choose_screen(choose: bool) {
    CHOOSE_SCREEN.store(choose, Ordering::Relaxed);
}

pub fn set_show_cursor(show: bool) {
    SHOW_CURSOR.store(show, Ordering::Relaxed);
}

async fn cursor_mode(proxy: &Screencast) -> CursorMode {
    if !SHOW_CURSOR.load(Ordering::Relaxed) {
        return CursorMode::Hidden;
    }
    match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
        _ => CursorMode::Hidden,
    }
}

/// View only: the screen-cast portal alone (`--view-only`).
async fn screencast_session() -> Result<(PortalCast, impl Send)> {
    let proxy = Screencast::new().await.context("the screen-cast portal is not available")?;
    let session = proxy.create_session(CreateSessionOptions::default()).await.context("CreateSession")?;
    let cursor = cursor_mode(&proxy).await;
    let token_file = state_file("restore-token");
    let token = read_token(&token_file);
    if token.is_none() {
        println!("Asking to share a screen: pick the monitor in the dialog on this screen.");
    }
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await
        .context("SelectSources")?
        .response()
        .context("SelectSources was refused")?;
    let streams = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .context("Start")?
        .response()
        .context("screen sharing was not allowed")?;
    save_token(&token_file, streams.restore_token());
    let stream = streams.streams().first().ok_or_else(|| anyhow!("the portal returned no stream"))?;
    let (node, size) = (stream.pipe_wire_node_id(), stream.size());
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.context("OpenPipeWireRemote")?;
    Ok((PortalCast { fd: Some(fd), node, size }, (proxy, session)))
}

/// The screen and remote control in one RemoteDesktop session: one dialog, kept with its own
/// restore token (a screen-cast token does not restore a RemoteDesktop session).
async fn remote_session(command: CommandKey) -> Result<(PortalCast, UnboundedSender<InputCmd>, impl Send)> {
    let rd = RemoteDesktop::new().await.context("the remote-desktop portal is not available (use --view-only)")?;
    let sc = Screencast::new().await.context("the screen-cast portal is not available")?;
    let session = rd.create_session(CreateSessionOptions::default()).await.context("CreateSession")?;
    let cursor = cursor_mode(&sc).await;
    let token_file = state_file("remote-desktop-token");
    let token = read_token(&token_file);
    if token.is_none() {
        println!("Asking to share a screen and allow remote control: pick the monitor and allow it in the dialog on this screen.");
    }
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await
    .context("SelectDevices")?
    .response()
    .context("SelectDevices was refused")?;
    sc.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_cursor_mode(cursor)
            .set_sources(BitFlags::from(SourceType::Monitor))
            .set_multiple(false),
    )
    .await
    .context("SelectSources")?
    .response()
    .context("SelectSources was refused")?;
    let selected = rd
        .start(&session, None, StartOptions::default())
        .await
        .context("Start")?
        .response()
        .context("screen sharing and remote control were not allowed")?;
    save_token(&token_file, selected.restore_token());
    let devices = selected.devices();
    println!(
        "Remote control: {}.",
        match (devices.contains(DeviceType::Pointer), devices.contains(DeviceType::Keyboard)) {
            (true, true) => "pointer and keyboard",
            (true, false) => "pointer only",
            (false, true) => "keyboard only",
            (false, false) => "not allowed",
        }
    );
    let stream = selected.streams().first().ok_or_else(|| anyhow!("the portal returned no stream"))?;
    let (node, size) = (stream.pipe_wire_node_id(), stream.size());
    let fd = sc.open_pipe_wire_remote(&session, Default::default()).await.context("OpenPipeWireRemote")?;
    let (w, h) = size.map(|(w, h)| (w as f64, h as f64)).unwrap_or((1920.0, 1080.0));
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(run_input(InputSink::Portal { rd, session, node }, w, h, command, false, input_rx));
    Ok((PortalCast { fd: Some(fd), node, size }, input_tx, sc))
}

/// Input events counted for the stats line.
static INPUT_EVENTS: AtomicU64 = AtomicU64::new(0);


/// Where input goes: the portal's RemoteDesktop session, or GNOME's own.
enum InputSink {
    Portal { rd: RemoteDesktop, session: Session<RemoteDesktop>, node: u32 },
    /// The session and stream shown (absolute positions are in its coordinates); they follow a
    /// switch.
    Mutter { shown: Shown },
}

/// Each device's events through its own `Translator`, executed in order. `everywhere`: absolute
/// positions reach the whole stream (GNOME's own API has no box; the portal's does).
async fn run_input(
    sink: InputSink,
    width: f64,
    height: f64,
    command: CommandKey,
    everywhere: bool,
    mut rx: UnboundedReceiver<InputCmd>,
) {
    let mut translators: HashMap<u64, Translator> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut failed = false;
    // TEST: SILL_LINUX_INPUT_LOG=1 prints every event from a device and every action sent to the
    // portal, with milliseconds since the first.
    let log = std::env::var_os("SILL_LINUX_INPUT_LOG").is_some();
    let t0 = Instant::now();
    loop {
        let actions: Vec<Action> = tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(InputCmd::Event(id, event)) => {
                    INPUT_EVENTS.fetch_add(1, Ordering::Relaxed);
                    if log {
                        println!("{:>8} ms  device {id}: {event:?}", t0.elapsed().as_millis());
                    }
                    let t = translators.entry(id).or_insert_with(|| Translator::new(width, height, command).keep_all_taps(keep_all_taps()).absolute_everywhere(everywhere));
                    if let Some((_, _, w, h)) = input_region() {
                        t.set_size(w, h);
                    } else if let Some((w, h)) = captured_size() {
                        t.set_size(w, h);
                    }
                    let dropped = t.dropped_taps;
                    let actions = t.handle(event, Instant::now());
                    if t.dropped_taps > dropped {
                        println!("Input: a tap at the end of a pointer move was not clicked (--keep-all-taps keeps them).");
                    }
                    actions
                }
                Some(InputCmd::Left(id)) => translators.remove(&id).map(|mut t| t.release_all()).unwrap_or_default(),
            },
            _ = tick.tick() => {
                let now = Instant::now();
                translators.values_mut().flat_map(|t| t.tick(now)).collect()
            }
        };
        for action in actions {
            if log {
                println!("{:>8} ms    → portal: {action:?}", t0.elapsed().as_millis());
            }
            if let Err(e) = perform(&sink, &action).await {
                if !failed {
                    failed = true;
                    eprintln!("Input: refused {action:?}: {e:#} (further failures not shown)");
                }
            }
        }
    }
}

async fn perform(sink: &InputSink, action: &Action) -> Result<()> {
    match sink {
        InputSink::Portal { rd, session, node } => {
            let state = |pressed: bool| if pressed { KeyState::Pressed } else { KeyState::Released };
            let stream = *node;
            match *action {
                Action::Motion { x, y } => rd.notify_pointer_motion_absolute(session, stream, x, y, Default::default()).await,
                Action::RelMotion { dx, dy } => rd.notify_pointer_motion(session, dx, dy, Default::default()).await,
                Action::Button { button, pressed } => rd.notify_pointer_button(session, button, state(pressed), Default::default()).await,
                Action::Axis { dx, dy } => rd.notify_pointer_axis(session, dx, dy, NotifyPointerAxisOptions::default()).await,
                Action::AxisStop => rd.notify_pointer_axis(session, 0.0, 0.0, NotifyPointerAxisOptions::default().set_finish(true)).await,
                Action::Key { code, pressed } => rd.notify_keyboard_keycode(session, code, state(pressed), Default::default()).await,
                Action::Keysym { sym, pressed } => rd.notify_keyboard_keysym(session, sym, state(pressed), Default::default()).await,
            }
            .map_err(Into::into)
        }
        InputSink::Mutter { shown } => {
            use crate::mutter::{AXIS_FINISH, AXIS_SOURCE_FINGER};
            let (session, stream) = shown.lock().unwrap().clone();
            match *action {
                Action::Motion { x, y } => {
                    let (ox, oy) = input_region().map(|r| (r.0, r.1)).unwrap_or((0.0, 0.0));
                    session.motion_absolute(&stream, x + ox, y + oy).await
                }
                Action::RelMotion { dx, dy } => session.motion_relative(dx, dy).await,
                Action::Button { button, pressed } => session.button(button, pressed).await,
                Action::Axis { dx, dy } => session.axis(dx, dy, AXIS_SOURCE_FINGER).await,
                Action::AxisStop => session.axis(0.0, 0.0, AXIS_SOURCE_FINGER | AXIS_FINISH).await,
                Action::Key { code, pressed } => session.keycode(code as u32, pressed).await,
                Action::Keysym { sym, pressed } => session.keysym(sym as u32, pressed).await,
            }
        }
    }
}
