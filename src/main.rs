mod config;
mod feed;
mod pipeline;
mod testsrc;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use gstreamer as gst;
use gstreamer::prelude::*;
use log::{error, info, warn};

use crate::config::Config;
use crate::feed::StopFlag;
use crate::pipeline::{EncoderMode, NullOnDrop};

/// Restart backoff doubles from MIN to MAX, and resets once a session has
/// run for STABLE.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
const STABLE: Duration = Duration::from_secs(30);
const EOS_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cfg = Config::parse();
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

    if !cfg.test_pattern {
        bail!("desktop capture isn't implemented yet; run with --test-pattern");
    }

    let shutdown: StopFlag = Arc::new(AtomicBool::new(false));
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

/// Runs sessions until shutdown, restarting with backoff whenever one fails
/// (RTSP server gone, pipeline error).
fn supervise(cfg: &Config, mode: EncoderMode, shutdown: &AtomicBool) {
    let mut backoff = BACKOFF_MIN;
    while !shutdown.load(Ordering::SeqCst) {
        let started = Instant::now();
        match session(cfg, mode, shutdown) {
            Ok(()) => return,
            Err(e) => warn!("session ended: {e:#}"),
        }
        if started.elapsed() >= STABLE {
            backoff = BACKOFF_MIN;
        }
        info!("restarting in {} s", backoff.as_secs());
        let deadline = Instant::now() + backoff;
        while Instant::now() < deadline && !shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One pipeline lifetime. Returns Ok on a requested shutdown, Err when the
/// pipeline fails.
fn session(cfg: &Config, mode: EncoderMode, shutdown: &AtomicBool) -> Result<()> {
    let stream = pipeline::build(cfg, mode)?;
    let _null = NullOnDrop(stream.pipeline.clone());
    let stop: StopFlag = Arc::new(AtomicBool::new(false));
    let threads = testsrc::start(&stream, cfg.fps, &stop);
    let stop_sources = || {
        stop.store(true, Ordering::SeqCst);
        for t in threads {
            let _ = t.join();
        }
    };

    if let Err(e) = stream.pipeline.set_state(gst::State::Playing) {
        stop_sources();
        bail!("pipeline won't start: {e}");
    }
    info!("publishing to {}", cfg.rtsp_url);

    let bus = stream.pipeline.bus().unwrap();
    let result = loop {
        if shutdown.load(Ordering::SeqCst) {
            break Ok(());
        }
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(e) => {
                let src = e.src().map(|s| s.path_string()).unwrap_or_default();
                let debug = e.debug().map(|d| format!(" ({d})")).unwrap_or_default();
                break Err(anyhow::anyhow!("{src}: {}{debug}", e.error()));
            }
            gst::MessageView::Eos(_) => break Err(anyhow::anyhow!("unexpected end of stream")),
            gst::MessageView::Warning(w) => warn!("{}", w.error()),
            _ => {}
        }
    };

    stop_sources();
    if result.is_ok() {
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
    result
}
