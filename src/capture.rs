//! The Desktop: the screen-cast portal hands over a PipeWire stream of one monitor, and a
//! GStreamer pipeline turns it into HEVC access units for the server.
//!
//! pipewiresrc → valve (closed while nobody watches) → videoconvert (passthrough for the
//! formats the encoder takes) → HEVC encoder (NVENC, else VA-API, else x265; Main profile, no
//! B-frames, low latency) → h265parse (parameter sets before every IDR, one access unit per
//! buffer, Annex-B) → appsink → `protocol::convert_annex_b` → `Server::broadcast_access_unit`.
//!
//! The portal asks once; its restore token (persist mode 2) is kept in
//! `$XDG_STATE_HOME/sill-linux/restore-token`, so later runs start without the dialog.

use crate::protocol;
use crate::input::{Action, CommandKey, Translator};
use crate::server::{Control, InputCmd, Server};
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
    fd: OwnedFd,
    node: u32,
    size: Option<(i32, i32)>,
}

/// `input`: the Command key's meaning, or None for view only (no remote control asked for).
pub fn run(server: &Server, control: Receiver<Control>, bitrate_mbps: u32, input: Option<CommandKey>) -> Result<()> {
    let (cast, input_tx) = start_portal(input)?;
    if let Some(tx) = input_tx {
        server.set_input(tx);
    }
    match cast.size {
        Some((w, h)) => println!("Screen cast: PipeWire node {}, {w}×{h}.", cast.node),
        None => println!("Screen cast: PipeWire node {}.", cast.node),
    }

    gst::init().context("GStreamer init")?;
    let (formats, encoder, name) = encoder(bitrate_mbps * 1000)?;
    // Shared memory by default. SILL_LINUX_DMABUF=1 tries zero copy for NVENC (the compositor's
    // DMA-BUF imported into GL, which nvh265enc reads), but on GNOME 50 with an RTX 3060 glupload
    // read the buffer with the wrong layout (NVIDIA's tiled modifiers) and the picture was garbage.
    let dmabuf = name.starts_with("NVENC") && std::env::var_os("SILL_LINUX_DMABUF").is_some();
    println!("Encoder: {name}, {bitrate_mbps} Mbit/s, {}.", if dmabuf { "DMA-BUF through GL" } else { "shared memory" });
    // The fd stays open in `cast` for as long as the pipeline runs (pipewiresrc does not own it).
    let description = format!(
        "pipewiresrc fd={fd} path={node} {input} \
         ! {encoder} \
         ! video/x-h265,profile=main \
         ! h265parse config-interval=-1 \
         ! video/x-h265,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=4 drop=false",
        fd = cast.fd.as_raw_fd(),
        node = cast.node,
        input = input_chain(dmabuf, formats),
    );
    let pipeline = gst::parse::launch(&description)
        .context("building the pipeline")?
        .dynamic_cast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let gate = pipeline.by_name("gate").ok_or_else(|| anyhow!("no valve"))?;
    let sink = pipeline
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
                        FRAME_SIZE.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
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

    // Counters for the once-a-second line: buffers out of pipewiresrc, and encoded units.
    let captured = Arc::new(AtomicU64::new(0));
    let encoded = Arc::new(AtomicU64::new(0));
    if let Some(pad) = gate.static_pad("sink") {
        let c = captured.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    if let Some(pad) = sink.static_pad("sink") {
        let c = encoded.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let mut streaming = false;
    let mut last_stats = Instant::now();

    // The system clock, not the one pipewiresrc offers: PipeWire's stops while the compositor sends
    // nothing (a still screen), and pipewiresrc's keep-alive resend waits on the pipeline clock.
    pipeline.use_clock(Some(&gst::SystemClock::obtain()));
    pipeline.set_state(gst::State::Playing).context("starting the pipeline")?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    let mut last_keyframe = Instant::now() - KEYFRAME_SPACING;
    let mut keyframe_pending = false;
    let result = loop {
        match control.recv_timeout(Duration::from_millis(50)) {
            Ok(Control::Start) => {
                streaming = true;
                gate.set_property("drop", false);
                keyframe_pending = true;
            }
            Ok(Control::Stop) => {
                streaming = false;
                gate.set_property("drop", true);
            }
            Ok(Control::Keyframe) => keyframe_pending = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let (cap, enc) = (captured.swap(0, Ordering::Relaxed), encoded.swap(0, Ordering::Relaxed));
            if streaming {
                let inputs = INPUT_EVENTS.swap(0, Ordering::Relaxed);
                println!("[1s] cap {cap} enc {enc} {} in {inputs}", server.take_stats());
            }
        }
        if keyframe_pending && last_keyframe.elapsed() >= KEYFRAME_SPACING {
            keyframe_pending = false;
            last_keyframe = Instant::now();
            let event = gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build();
            sink.send_event(event);
        }
        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(e) => {
                    break_err(&pipeline);
                    return Err(anyhow!("{} ({:?})", e.error(), e.debug()));
                }
                MessageView::Eos(_) => {
                    break_err(&pipeline);
                    bail!("the screen cast ended (the portal session was closed)");
                }
                _ => {}
            }
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    drop(cast);
    result
}

/// What goes between pipewiresrc's own settings and the encoder: the memory it hands over, the
/// valve (closed while nobody watches; it passes caps on, so opening it renegotiates nothing),
/// and the conversion to what the encoder takes.
fn input_chain(dmabuf: bool, formats: &str) -> String {
    let valve = "valve name=gate drop=true drop-mode=forward-sticky-events";
    if dmabuf {
        format!(
            "keepalive-time=500 ! video/x-raw(memory:DMABuf) ! {valve} \
             ! glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=RGBA"
        )
    } else {
        format!(
            "always-copy=true keepalive-time=500 ! video/x-raw ! {valve} \
             ! videoconvert ! video/x-raw,format={{{formats}}}"
        )
    }
}

fn break_err(pipeline: &gst::Pipeline) {
    let _ = pipeline.set_state(gst::State::Null);
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
                "nvh265enc preset=p1 tune=ultra-low-latency zerolatency=true bframes=0 gop-size={gop} \
                 rc-mode=cbr bitrate={kbps} repeat-sequence-header=true"
            ),
            "NVENC (nvh265enc)",
        ));
    }
    if have("vah265enc") {
        return Ok((
            "NV12",
            format!("vah265enc b-frames=0 key-int-max={gop} rate-control=cbr bitrate={kbps} target-usage=7"),
            "VA-API (vah265enc)",
        ));
    }
    if have("x265enc") {
        return Ok((
            "I420",
            format!("x265enc speed-preset=ultrafast tune=zerolatency key-int-max={gop} bitrate={kbps}"),
            "software x265 (x265enc)",
        ));
    }
    bail!("no HEVC encoder in GStreamer (nvh265enc, vah265enc or x265enc)")
}

fn state_file(name: &str) -> PathBuf {
    crate::identity::state_dir().join(name)
}

fn read_token(path: &PathBuf) -> Option<String> {
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

/// Runs the portal on a thread of its own, which then keeps the session (and its D-Bus
/// connection) alive for as long as this process runs. With `input`, the session is a
/// RemoteDesktop one with the screen cast in it, and input runs on that thread too.
fn start_portal(input: Option<CommandKey>) -> Result<(PortalCast, Option<UnboundedSender<InputCmd>>)> {
    let (tx, rx) = mpsc::channel::<Result<(PortalCast, Option<UnboundedSender<InputCmd>>)>>();
    std::thread::Builder::new().name("portal".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
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
        });
    })?;
    rx.recv().context("the portal thread ended")?
}

async fn cursor_mode(proxy: &Screencast) -> CursorMode {
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
    Ok((PortalCast { fd, node, size }, (proxy, session)))
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
    tokio::spawn(run_input(rd, session, node, w, h, command, input_rx));
    Ok((PortalCast { fd, node, size }, input_tx, sc))
}

/// Input events counted for the stats line.
static INPUT_EVENTS: AtomicU64 = AtomicU64::new(0);
/// The encoded picture's size, width << 32 | height (0 before the first frame): the space the
/// portal takes absolute motion in.
static FRAME_SIZE: AtomicU64 = AtomicU64::new(0);

fn frame_size() -> Option<(f64, f64)> {
    let v = FRAME_SIZE.load(Ordering::Relaxed);
    (v != 0).then(|| ((v >> 32) as f64, (v & 0xffff_ffff) as f64))
}

/// Each device's events through its own `Translator`, executed in order on the portal.
async fn run_input(
    rd: RemoteDesktop,
    session: Session<RemoteDesktop>,
    stream: u32,
    width: f64,
    height: f64,
    command: CommandKey,
    mut rx: UnboundedReceiver<InputCmd>,
) {
    let mut translators: HashMap<u64, Translator> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut failed = false;
    loop {
        let actions: Vec<Action> = tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(InputCmd::Event(id, event)) => {
                    INPUT_EVENTS.fetch_add(1, Ordering::Relaxed);
                    let t = translators.entry(id).or_insert_with(|| Translator::new(width, height, command));
                    if let Some((w, h)) = frame_size() {
                        t.set_size(w, h);
                    }
                    t.handle(event, Instant::now())
                }
                Some(InputCmd::Left(id)) => translators.remove(&id).map(|mut t| t.release_all()).unwrap_or_default(),
            },
            _ = tick.tick() => {
                let now = Instant::now();
                translators.values_mut().flat_map(|t| t.tick(now)).collect()
            }
        };
        for action in actions {
            if let Err(e) = perform(&rd, &session, stream, &action).await {
                if !failed {
                    failed = true;
                    eprintln!("Input: the portal refused {action:?}: {e} (further failures not shown)");
                }
            }
        }
    }
}

async fn perform(rd: &RemoteDesktop, session: &Session<RemoteDesktop>, stream: u32, action: &Action) -> ashpd::Result<()> {
    let state = |pressed: bool| if pressed { KeyState::Pressed } else { KeyState::Released };
    match *action {
        Action::Motion { x, y } => rd.notify_pointer_motion_absolute(session, stream, x, y, Default::default()).await,
        Action::Button { button, pressed } => rd.notify_pointer_button(session, button, state(pressed), Default::default()).await,
        Action::Axis { dx, dy } => rd.notify_pointer_axis(session, dx, dy, NotifyPointerAxisOptions::default()).await,
        Action::AxisStop => rd.notify_pointer_axis(session, 0.0, 0.0, NotifyPointerAxisOptions::default().set_finish(true)).await,
        Action::Key { code, pressed } => rd.notify_keyboard_keycode(session, code, state(pressed), Default::default()).await,
        Action::Keysym { sym, pressed } => rd.notify_keyboard_keysym(session, sym, state(pressed), Default::default()).await,
    }
}
