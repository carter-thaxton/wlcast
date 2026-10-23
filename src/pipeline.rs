//! GStreamer pipeline construction and the encoder probe.
//!
//! Every source pushes raw video and audio into two `appsrc`s; everything
//! after them (scaling, encoding, Opus, RTSP) is built here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use log::{debug, info, warn};

use crate::config::{Config, EncoderChoice};

/// Elements every pipeline needs, whatever the encoder.
const REQUIRED_ELEMENTS: &[&str] = &[
    "appsrc",
    "appsink",
    "queue",
    "identity",
    "videorate",
    "h264parse",
    "audioconvert",
    "audioresample",
    "opusenc",
    "rtspclientsink",
];

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_FRAMES: u64 = 30;

/// Fails with a clear message if any always-required element is missing.
pub fn check_elements() -> Result<()> {
    let missing: Vec<_> = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if !missing.is_empty() {
        bail!("missing GStreamer elements: {}", missing.join(", "));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderMode {
    VaLpCbr,
    VaLpCqp,
    VaCbr,
    VaCqp,
    X264,
}

impl EncoderMode {
    /// Probe order: low-power VA first, x264 last.
    const ALL: [EncoderMode; 5] = [
        Self::VaLpCbr,
        Self::VaLpCqp,
        Self::VaCbr,
        Self::VaCqp,
        Self::X264,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::VaLpCbr => "va-lp-cbr",
            Self::VaLpCqp => "va-lp-cqp",
            Self::VaCbr => "va-cbr",
            Self::VaCqp => "va-cqp",
            Self::X264 => "x264",
        }
    }

    fn is_va(self) -> bool {
        self != Self::X264
    }

    fn elements(self) -> &'static [&'static str] {
        match self {
            Self::VaLpCbr | Self::VaLpCqp => &["vapostproc", "vah264lpenc"],
            Self::VaCbr | Self::VaCqp => &["vapostproc", "vah264enc"],
            Self::X264 => &["videoconvertscale", "x264enc"],
        }
    }

    fn missing_elements(self) -> Vec<&'static str> {
        self.elements()
            .iter()
            .copied()
            .filter(|e| gst::ElementFactory::find(e).is_none())
            .collect()
    }

    /// Raw video in, H.264 out (no B-frames, keyframe every ~2 s).
    fn chain(self, cfg: &Config) -> String {
        let (w, h, br, qp) = (cfg.width, cfg.height, cfg.bitrate, cfg.qp);
        let keyint = cfg.fps * 2;
        let va_in =
            format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width={w},height={h}");
        let common = format!("key-int-max={keyint} b-frames=0");
        let cqp = format!("rate-control=cqp qpi={qp} qpp={}", qp + 2);
        match self {
            Self::VaLpCbr => {
                format!("{va_in} ! vah264lpenc rate-control=cbr bitrate={br} {common}")
            }
            Self::VaLpCqp => format!("{va_in} ! vah264lpenc {cqp} {common}"),
            Self::VaCbr => format!("{va_in} ! vah264enc rate-control=cbr bitrate={br} {common}"),
            Self::VaCqp => format!("{va_in} ! vah264enc {cqp} {common}"),
            Self::X264 => format!(
                "videoconvertscale ! video/x-raw,format=I420,width={w},height={h},pixel-aspect-ratio=1/1 \
                 ! x264enc tune=zerolatency speed-preset=superfast bframes=0 key-int-max={keyint} \
                   bitrate={br} threads=2 \
                 ! video/x-h264,profile=constrained-baseline"
            ),
        }
    }
}

/// Picks the encoder: the forced mode if one was given, otherwise the first
/// mode that passes the probe.
pub fn select_encoder(cfg: &Config) -> Result<EncoderMode> {
    let forced = match cfg.encoder {
        EncoderChoice::Auto => None,
        EncoderChoice::VaLpCbr => Some(EncoderMode::VaLpCbr),
        EncoderChoice::VaLpCqp => Some(EncoderMode::VaLpCqp),
        EncoderChoice::VaCbr => Some(EncoderMode::VaCbr),
        EncoderChoice::VaCqp => Some(EncoderMode::VaCqp),
        EncoderChoice::X264 => Some(EncoderMode::X264),
    };
    if let Some(mode) = forced {
        let missing = mode.missing_elements();
        if !missing.is_empty() {
            bail!(
                "encoder {} needs missing elements: {}",
                mode.name(),
                missing.join(", ")
            );
        }
        info!("encoder: {} (forced)", mode.name());
        return Ok(mode);
    }

    for mode in EncoderMode::ALL {
        let missing = mode.missing_elements();
        if !missing.is_empty() {
            info!(
                "probe: {} skipped (missing {})",
                mode.name(),
                missing.join(", ")
            );
            continue;
        }
        match probe(cfg, mode) {
            Ok(elapsed) => {
                info!(
                    "encoder: {} (probe took {} ms)",
                    mode.name(),
                    elapsed.as_millis()
                );
                return Ok(mode);
            }
            Err(e) => info!("probe: {} failed: {e:#}", mode.name()),
        }
    }
    bail!("no working H.264 encoder found")
}

/// Pushes a few frames through the encoder chain and waits for the first
/// encoded buffer.
///
/// A VA encoder that can't do the requested rate control may log a warning
/// and fall back instead of failing, so such a warning counts as failure.
/// That check needs GStreamer's debug log, which a build without it lacks.
fn probe(cfg: &Config, mode: EncoderMode) -> Result<Duration> {
    let rc_fallback = Arc::new(AtomicBool::new(false));
    let log_fn = mode.is_va().then(|| {
        if std::env::var_os("GST_DEBUG").is_none() {
            gst::log::set_threshold_for_name("va*", gst::DebugLevel::Warning);
        }
        let flag = rc_fallback.clone();
        gst::log::add_log_function(move |cat, level, _file, _func, _line, _obj, msg| {
            if level <= gst::DebugLevel::Warning
                && cat.name().starts_with("va")
                && msg.get().is_some_and(|m| mentions_rate_control(m.as_str()))
            {
                flag.store(true, Ordering::SeqCst);
            }
        })
    });

    let result = run_probe(cfg, mode);

    if let Some(f) = log_fn {
        gst::log::remove_log_function(f);
    }
    let elapsed = result?;
    if rc_fallback.load(Ordering::SeqCst) {
        bail!("encoder fell back to a different rate control");
    }
    Ok(elapsed)
}

fn run_probe(cfg: &Config, mode: EncoderMode) -> Result<Duration> {
    let desc = format!(
        "appsrc name=src format=time ! {} ! h264parse ! appsink name=out sync=false",
        mode.chain(cfg)
    );
    let pipeline = launch(&desc)?;
    let src = app_src(&pipeline, "src")?;
    let out = pipeline
        .by_name("out")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| anyhow!("no appsink in probe pipeline"))?;

    let (w, h) = (cfg.width as usize, cfg.height as usize);
    src.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .field("width", w as i32)
            .field("height", h as i32)
            .field("framerate", gst::Fraction::new(cfg.fps as i32, 1))
            .build(),
    ));

    let started = Instant::now();
    let _guard = NullOnDrop(pipeline.clone());
    pipeline
        .set_state(gst::State::Playing)
        .context("probe pipeline won't start")?;

    let frame_dur = gst::ClockTime::SECOND / cfg.fps as u64;
    for n in 0..PROBE_FRAMES {
        let mut buf = gst::Buffer::from_mut_slice(vec![0x40u8; w * h * 4]);
        let b = buf.get_mut().unwrap();
        b.set_pts(frame_dur * n);
        b.set_duration(frame_dur);
        if src.push_buffer(buf).is_err() {
            break; // the bus says why
        }
    }
    let _ = src.end_of_stream();

    let bus = pipeline.bus().unwrap();
    while started.elapsed() < PROBE_TIMEOUT {
        if out
            .try_pull_sample(gst::ClockTime::from_mseconds(50))
            .is_some()
        {
            return Ok(started.elapsed());
        }
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.view() {
                gst::MessageView::Error(e) => bail!("{}", e.error()),
                _ => bail!("reached end of stream without output"),
            }
        }
    }
    bail!("timed out after {} s", PROBE_TIMEOUT.as_secs())
}

/// A built streaming pipeline. Sources set the caps on `video` and `audio`
/// and push timestamped buffers into them.
pub struct Stream {
    pub pipeline: gst::Pipeline,
    pub video: gst_app::AppSrc,
    pub audio: gst_app::AppSrc,
}

pub fn build(cfg: &Config, mode: EncoderMode) -> Result<Stream> {
    let (a_delay, v_delay) = cfg.branch_delays_ms();
    let desc = format!(
        "rtspclientsink name=sink protocols=tcp latency=0 \
         appsrc name=video is-live=true format=time max-buffers=2 leaky-type=downstream \
           ! videorate ! video/x-raw,framerate={fps}/1 \
           ! queue max-size-buffers=2 leaky=downstream \
           ! {enc} ! h264parse config-interval=-1 ! {vdelay} ! sink. \
         appsrc name=audio is-live=true format=time \
           ! queue ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
           ! opusenc bitrate={abr} frame-size=10 ! queue ! {adelay} ! sink.",
        fps = cfg.fps,
        enc = mode.chain(cfg),
        vdelay = delay_queue("vdelay", v_delay),
        abr = cfg.audio_bitrate,
        adelay = delay_queue("adelay", a_delay),
    );
    let pipeline = launch(&desc)?;
    pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("no rtspclientsink in pipeline"))?
        .set_property("location", &cfg.rtsp_url);
    for (name, ms) in [("vdelay", v_delay), ("adelay", a_delay)] {
        let pad = pipeline
            .by_name(name)
            .and_then(|q| q.static_pad("src"))
            .ok_or_else(|| anyhow!("no {name} queue"))?;
        pad.set_offset(ms as i64 * 1_000_000);
    }
    Ok(Stream {
        video: app_src(&pipeline, "video")?,
        audio: app_src(&pipeline, "audio")?,
        pipeline,
    })
}

/// The last queue on a branch, just before the sink. Delaying a branch takes
/// two things, and either alone measurably does nothing at the receiver:
/// `build` shifts the running time on the queue's src pad (moving the RTP
/// timestamps), and the queue holds the data back by the same amount (so it
/// is also sent that much later). `identity ts-offset`, which the prototype
/// used, only affects clock sync and doesn't shift timestamps.
fn delay_queue(name: &str, ms: u64) -> String {
    if ms == 0 {
        return format!("queue name={name}");
    }
    format!(
        "queue name={name} max-size-buffers=0 max-size-bytes=0 max-size-time={} min-threshold-time={}",
        (ms + 1000) * 1_000_000,
        ms * 1_000_000
    )
}

fn mentions_rate_control(msg: &str) -> bool {
    // Same as the prototype's `grep -i 'rate.control'`.
    let m = msg.to_ascii_lowercase();
    m.match_indices("rate").any(|(i, _)| {
        m.get(i + 5..)
            .is_some_and(|rest| rest.starts_with("control"))
    })
}

fn launch(desc: &str) -> Result<gst::Pipeline> {
    debug!("pipeline: {desc}");
    gst::parse::launch(desc)
        .with_context(|| format!("can't build pipeline: {desc}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("pipeline description didn't produce a pipeline"))
}

fn app_src(pipeline: &gst::Pipeline, name: &str) -> Result<gst_app::AppSrc> {
    pipeline
        .by_name(name)
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| anyhow!("no appsrc named {name}"))
}

/// Sets a pipeline to NULL when dropped.
pub struct NullOnDrop(pub gst::Pipeline);

impl Drop for NullOnDrop {
    fn drop(&mut self) {
        if self.0.set_state(gst::State::Null).is_err() {
            warn!("failed to stop pipeline");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_control_matches_like_grep() {
        assert!(mentions_rate_control("Unsupported Rate Control mode"));
        assert!(mentions_rate_control("rate-control cbr not supported"));
        assert!(mentions_rate_control("rate_control"));
        assert!(!mentions_rate_control("ratecontrol"));
        assert!(!mentions_rate_control("framerate changed"));
    }

    #[test]
    fn delay_queue_sized_for_delay() {
        assert_eq!(delay_queue("q", 0), "queue name=q");
        assert!(
            delay_queue("q", 100)
                .ends_with("max-size-time=1100000000 min-threshold-time=100000000")
        );
    }
}
