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
use crate::server::{Control, Server};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A keyframe asked for less than this after the last one is folded into it.
const KEYFRAME_SPACING: Duration = Duration::from_millis(250);

struct Cast {
    fd: OwnedFd,
    node: u32,
    size: Option<(i32, i32)>,
}

pub fn run(server: &Server, control: Receiver<Control>, bitrate_mbps: u32) -> Result<()> {
    let cast = start_portal()?;
    match cast.size {
        Some((w, h)) => println!("Screen cast: PipeWire node {}, {w}×{h}.", cast.node),
        None => println!("Screen cast: PipeWire node {}.", cast.node),
    }

    gst::init().context("GStreamer init")?;
    let (encoder, name) = encoder(bitrate_mbps * 1000)?;
    println!("Encoder: {name}, {bitrate_mbps} Mbit/s.");
    // The fd stays open in `cast` for as long as the pipeline runs (pipewiresrc does not own it).
    let description = format!(
        "pipewiresrc fd={fd} path={node} always-copy=true do-timestamp=true keepalive-time=500 \
         ! video/x-raw \
         ! valve name=gate drop=true \
         ! videoconvert ! video/x-raw,format={{BGRx,BGRA,RGBx,RGBA,NV12}} \
         ! {encoder} \
         ! video/x-h265,profile=main \
         ! h265parse config-interval=-1 \
         ! video/x-h265,stream-format=byte-stream,alignment=au \
         ! appsink name=sink sync=false max-buffers=4 drop=false",
        fd = cast.fd.as_raw_fd(),
        node = cast.node,
    );
    let pipeline = gst::parse::launch(&description)
        .context("building the pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let gate = pipeline.by_name("gate").ok_or_else(|| anyhow!("no valve"))?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow!("no appsink"))?;

    let frames_server = server.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        frames_server.broadcast_access_unit(&protocol::convert_annex_b(map.as_slice()));
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).context("starting the pipeline")?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
    let mut last_keyframe = Instant::now() - KEYFRAME_SPACING;
    let mut keyframe_pending = false;
    let result = loop {
        match control.recv_timeout(Duration::from_millis(50)) {
            Ok(Control::Start) => {
                gate.set_property("drop", false);
                keyframe_pending = true;
            }
            Ok(Control::Stop) => gate.set_property("drop", true),
            Ok(Control::Keyframe) => keyframe_pending = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
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

fn break_err(pipeline: &gst::Pipeline) {
    let _ = pipeline.set_state(gst::State::Null);
}

/// The first HEVC encoder this machine has, as a pipeline fragment, and its name.
fn encoder(kbps: u32) -> Result<(String, &'static str)> {
    let have = |name: &str| gst::ElementFactory::find(name).is_some();
    let gop = 240; // Sill: a keyframe at least every fps × 4 frames
    if have("nvh265enc") {
        return Ok((
            format!(
                "nvh265enc preset=p1 tune=ultra-low-latency zerolatency=true bframes=0 gop-size={gop} \
                 rc-mode=cbr bitrate={kbps} repeat-sequence-header=true"
            ),
            "NVENC (nvh265enc)",
        ));
    }
    if have("vah265enc") {
        return Ok((
            format!("vah265enc b-frames=0 key-int-max={gop} rate-control=cbr bitrate={kbps} target-usage=7"),
            "VA-API (vah265enc)",
        ));
    }
    if have("x265enc") {
        return Ok((
            format!("x265enc speed-preset=ultrafast tune=zerolatency key-int-max={gop} bitrate={kbps}"),
            "software x265 (x265enc)",
        ));
    }
    bail!("no HEVC encoder in GStreamer (nvh265enc, vah265enc or x265enc)")
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("sill-linux").join("restore-token")
}

/// Runs the portal on a thread of its own, which then keeps the session (and its D-Bus
/// connection) alive for as long as this process runs.
fn start_portal() -> Result<Cast> {
    let (tx, rx) = mpsc::channel::<Result<Cast>>();
    std::thread::Builder::new().name("portal".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            match portal_session().await {
                Ok((cast, keep)) => {
                    let _ = tx.send(Ok(cast));
                    let _keep = keep;
                    std::future::pending::<()>().await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
    })?;
    rx.recv().context("the portal thread ended")?
}

async fn portal_session() -> Result<(Cast, impl Send)> {
    let proxy = Screencast::new().await.context("the screen-cast portal is not available")?;
    let session = proxy.create_session(CreateSessionOptions::default()).await.context("CreateSession")?;
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::Embedded) => CursorMode::Embedded,
        _ => CursorMode::Hidden,
    };
    let token_file = token_path();
    let token = std::fs::read_to_string(&token_file).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if token.is_none() {
        println!("Asking to share a screen: pick the monitor in the dialog on this screen.");
    }
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(SourceType::Monitor.into())
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
    if let Some(t) = streams.restore_token() {
        if let Some(dir) = token_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&token_file, t);
    }
    let stream = streams.streams().first().ok_or_else(|| anyhow!("the portal returned no stream"))?;
    let (node, size) = (stream.pipe_wire_node_id(), stream.size());
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("OpenPipeWireRemote")?;
    Ok((Cast { fd, node, size }, (proxy, session)))
}
