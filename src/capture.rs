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
use crate::server::{Apply, Control, InputCmd, Server};
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

fn captured_size() -> Option<(f64, f64)> {
    let v = CAPTURED_SIZE.load(Ordering::Relaxed);
    (v != 0).then(|| ((v >> 32) as f64, (v & 0xffff_ffff) as f64))
}

/// `input`: the Command key's meaning, or None for view only (no remote control asked for).
pub fn run(server: &Server, control: Receiver<Control>, input: Option<CommandKey>) -> Result<()> {
    let (cast, input_tx) = start_capture(input)?;
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
    loop {
        println!("Encoder: {name}, {}.", if dmabuf { "DMA-BUF through GL" } else { "shared memory" });
        let shape = TailShape { gl, dmabuf, formats, encoder: encoder_desc.clone(), logical: cast.size };
        match run_pipeline(server, &control, &cast, &shape, &mut apply, &mut streaming)? {
            Outcome::Ended => break,
            Outcome::NoDmaBuf(why) if dmabuf => {
                println!("DMA-BUF did not work here ({why}); using shared memory.");
                dmabuf = false;
            }
            Outcome::NoDmaBuf(why) => bail!(why),
        }
    }
    drop(cast);
    Ok(())
}

enum Outcome {
    Ended,
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
) -> Result<Outcome> {
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
        drop = !*streaming,
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
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
            c.fetch_add(1, Ordering::Relaxed);
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
    let result = loop {
        match control.recv_timeout(Duration::from_millis(50)) {
            Ok(Control::Start) => {
                *streaming = true;
                gate.set_property("drop", false);
                keyframe_pending = true;
            }
            Ok(Control::Stop) => {
                *streaming = false;
                gate.set_property("drop", true);
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
                    match replace_tail(&pipeline, &gate, &head, tail, shape, apply, server, &counters, *streaming) {
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
struct TailShape {
    gl: bool,
    dmabuf: bool,
    formats: &'static str,
    encoder: String,
    logical: Option<(i32, i32)>,
}

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
    println!("Stream settings: {res}, up to {} fps, {} Mbit/s.", a.fps, a.kbps() / 1000);
}

fn build_tail(shape: &TailShape, a: &Apply, server: &Server, counters: &(Arc<AtomicU64>, Arc<AtomicU64>)) -> Result<Tail> {
    let size = match (a.retina, shape.logical) {
        (false, Some((w, h))) => format!(",width={w},height={h}"),
        _ => String::new(),
    };
    let scale = if shape.gl {
        format!("glcolorscale ! video/x-raw(memory:GLMemory),format=RGBA{size}")
    } else {
        let first = shape.formats.split(',').next().unwrap_or("I420");
        format!("videoscale ! video/x-raw,format={first}{size}")
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
/// new one linked and started, the valve opened again if devices watch. The valve's sticky
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
    streaming: bool,
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
    if streaming {
        gate.set_property("drop", false);
    }
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
fn start_capture(input: Option<CommandKey>) -> Result<(PortalCast, Option<UnboundedSender<InputCmd>>)> {
    let (tx, rx) = mpsc::channel::<Result<(PortalCast, Option<UnboundedSender<InputCmd>>)>>();
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
                match mutter_session(input).await {
                    Ok((cast, input_tx, keep)) => {
                        let _ = tx.send(Ok((cast, input_tx)));
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
async fn mutter_session(input: Option<CommandKey>) -> Result<(PortalCast, Option<UnboundedSender<InputCmd>>, impl Send)> {
    let conn = ashpd::zbus::Connection::session().await.context("the session bus")?;
    let wanted = MONITOR.lock().unwrap().clone();
    let connector = crate::mutter::monitor_connector(&conn, wanted.as_deref()).await?;
    let mut session = crate::mutter::Session::new(conn).await?;
    let cursor = if SHOW_CURSOR.load(Ordering::Relaxed) { 1 } else { 0 };
    let stream = session.record(crate::mutter::Source::Monitor(&connector), cursor).await?;
    println!("Capture: GNOME's own screen-cast API, monitor {connector}{}.", if input.is_some() { ", with remote control" } else { "" });
    let cast = PortalCast { fd: None, node: stream.node, size: stream.size };
    let session = Arc::new(session);
    let input_tx = input.map(|command| {
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (w, h) = stream.size.map(|(w, h)| (w as f64, h as f64)).unwrap_or((1920.0, 1080.0));
        let sink = InputSink::Mutter { session: session.clone(), stream: stream.clone() };
        tokio::spawn(run_input(sink, w, h, command, true, input_rx));
        input_tx
    });
    Ok((cast, input_tx, session))
}

async fn portal_thread(input: Option<CommandKey>, tx: mpsc::Sender<Result<(PortalCast, Option<UnboundedSender<InputCmd>>)>>) {
    {
        {
            match input {
                Some(command) => match remote_session(command).await {
                    Ok((cast, input_tx, keep)) => {
                        let _ = tx.send(Ok((cast, Some(input_tx))));
                        let _keep = keep;
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                },
                None => match screencast_session().await {
                    Ok((cast, keep)) => {
                        let _ = tx.send(Ok((cast, None)));
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
    Mutter { session: Arc<crate::mutter::Session>, stream: crate::mutter::Stream },
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
                    if let Some((w, h)) = captured_size() {
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
        InputSink::Mutter { session, stream } => {
            use crate::mutter::{AXIS_FINISH, AXIS_SOURCE_FINGER};
            match *action {
                Action::Motion { x, y } => session.motion_absolute(stream, x, y).await,
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
