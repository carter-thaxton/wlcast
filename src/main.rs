mod capture;
mod config;
mod feed;
mod pipeline;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use log::{error, info, warn};

use crate::capture::Capture;
use crate::config::Config;
use crate::feed::Stop;
use crate::pipeline::{EncoderMode, NullOnDrop};

/// Restart backoff doubles from MIN to MAX, and resets once things have run
/// for STABLE.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
const STABLE: Duration = Duration::from_secs(30);
const EOS_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a capture failure may take to turn into a requested stop.
const STOP_GRACE: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = Config::load();
    match run(&cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: &Config) -> Result<()> {
    gst::init().context("GStreamer init failed")?;
    #[cfg(feature = "static-gst")]
    unsafe {
        gst_init_static_plugins();
    }
    pipeline::check_elements()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .context("can't install signal handler")?;

    let mode = pipeline::select_encoder(cfg)?;
    supervise(cfg, mode, &shutdown);
    Ok(())
}

#[cfg(feature = "static-gst")]
unsafe extern "C" {
    fn gst_init_static_plugins();
}

/// Why a pipeline session ended.
enum Ended {
    Shutdown,
    /// The capture stopped: on failure (PipeWire restart, output gone),
    /// restart capture and pipeline; on request (the user stopped sharing),
    /// exit.
    Capture(Stop),
    /// The pipeline failed (RTSP server gone, ...): restart the pipeline only.
    Pipeline(anyhow::Error),
}

/// Runs until shutdown, restarting with backoff whatever fails.
fn supervise(cfg: &Config, mode: EncoderMode, shutdown: &AtomicBool) {
    let mut backoff = Backoff::new();
    while !shutdown.load(Ordering::SeqCst) {
        let capture = match Capture::start(cfg) {
            Ok(c) => c,
            Err(e) => {
                warn!("capture failed to start: {e:#}");
                backoff.wait(shutdown);
                continue;
            }
        };
        loop {
            match session(cfg, mode, &capture, shutdown) {
                Ended::Shutdown => return,
                Ended::Capture(Stop::Requested(why)) => {
                    info!("{why}; exiting");
                    return;
                }
                Ended::Capture(Stop::Failed(e)) => {
                    if let Some(Stop::Requested(why)) = capture.settle(STOP_GRACE) {
                        info!("{why}; exiting");
                        return;
                    }
                    warn!("capture failed: {e}");
                    break;
                }
                Ended::Pipeline(e) => warn!("pipeline failed: {e:#}"),
            }
            backoff.wait(shutdown);
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
        }
        drop(capture);
        backoff.wait(shutdown);
    }
}

struct Backoff {
    delay: Duration,
    since: Instant,
}

impl Backoff {
    fn new() -> Self {
        Self {
            delay: BACKOFF_MIN,
            since: Instant::now(),
        }
    }

    /// Sleeps before a restart (returning early on shutdown).
    fn wait(&mut self, shutdown: &AtomicBool) {
        if self.since.elapsed() >= STABLE {
            self.delay = BACKOFF_MIN;
        }
        info!("restarting in {} s", self.delay.as_secs());
        let deadline = Instant::now() + self.delay;
        while Instant::now() < deadline && !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        self.delay = (self.delay * 2).min(BACKOFF_MAX);
        self.since = Instant::now();
    }
}

/// One pipeline lifetime, fed by `capture`.
fn session(cfg: &Config, mode: EncoderMode, capture: &Capture, shutdown: &AtomicBool) -> Ended {
    let stream = match pipeline::build(cfg, mode) {
        Ok(s) => s,
        Err(e) => return Ended::Pipeline(e),
    };
    let _null = NullOnDrop(stream.pipeline.clone());
    if let Err(e) = stream.pipeline.set_state(gst::State::Playing) {
        return Ended::Pipeline(anyhow!("pipeline won't start: {e}"));
    }
    info!("publishing to {}", cfg.rtsp_url);

    let bus = stream.pipeline.bus().unwrap();
    let mut attached = false;
    let ended = loop {
        if shutdown.load(Ordering::SeqCst) {
            break Ended::Shutdown;
        }
        if let Some(stop) = capture.stopped() {
            break Ended::Capture(stop);
        }
        // The base time is fixed once the pipeline reaches PLAYING.
        if !attached
            && stream.pipeline.current_state() == gst::State::Playing
            && let Some(base_time) = stream.pipeline.base_time()
        {
            capture.video.attach(stream.video.clone(), base_time);
            capture.audio.attach(stream.audio.clone(), base_time);
            attached = true;
        }
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(e) => {
                let src = e.src().map(|s| s.path_string()).unwrap_or_default();
                let debug = e.debug().map(|d| format!(" ({d})")).unwrap_or_default();
                break Ended::Pipeline(anyhow!("{src}: {}{debug}", e.error()));
            }
            gst::MessageView::Eos(_) => break Ended::Pipeline(anyhow!("unexpected end of stream")),
            gst::MessageView::Warning(w) => warn!("{}", w.error()),
            _ => {}
        }
    };

    capture.video.detach();
    capture.audio.detach();
    if matches!(ended, Ended::Shutdown | Ended::Capture(Stop::Requested(_))) {
        // Clean shutdown: let the sinks see EOS so RTSP is torn down properly.
        info!("shutting down");
        let _ = stream.video.end_of_stream();
        let _ = stream.audio.end_of_stream();
        let timeout = gst::ClockTime::from_mseconds(EOS_TIMEOUT.as_millis() as u64);
        if bus
            .timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error])
            .is_none()
        {
            warn!("no EOS within {} s; stopping anyway", EOS_TIMEOUT.as_secs());
        }
    }
    ended
}
