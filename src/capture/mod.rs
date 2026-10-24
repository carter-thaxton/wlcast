//! Capture sources. A video backend fills the video feed; system audio (or
//! the test pattern) fills the audio feed.

mod portal;
mod pw;
mod pwaudio;
mod pwvideo;
mod testsrc;
mod wl;

use std::time::{Duration, Instant};

use anyhow::Result;
use log::info;

use crate::config::{CaptureChoice, Config};
use crate::feed::{Feed, Running, Stop};

/// A way of capturing the screen. Every backend pushes raw frames into the
/// video feed, timestamped on [`crate::feed::clock`].
pub trait VideoBackend {
    fn name(&self) -> &'static str;
    /// Starts capturing into `feed`; capture stops when the result drops.
    fn start(&self, cfg: &Config, feed: Feed) -> Result<Running>;
}

/// Picks the screen capture backend: --capture if given, otherwise the best
/// protocol the compositor offers (ext-image-copy-capture, then
/// wlr-screencopy), falling back to the ScreenCast portal.
fn select_backend(cfg: &Config) -> Box<dyn VideoBackend> {
    match cfg.capture {
        CaptureChoice::Ext => Box::new(wl::Wayland(wl::Protocol::Ext)),
        CaptureChoice::Wlr => Box::new(wl::Wayland(wl::Protocol::Wlr)),
        CaptureChoice::Portal => Box::new(portal::Portal),
        CaptureChoice::Auto => match wl::probe() {
            Some(p) => Box::new(wl::Wayland(p)),
            None => Box::new(portal::Portal),
        },
    }
}

/// How long a damage-driven source may go without a new frame before the
/// last one is repeated, so the stream keeps its frame rate while the screen
/// is still: 1.5 frame periods.
fn keepalive_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(1.5 / fps as f64)
}

/// Running video and audio capture.
pub struct Capture {
    pub video: Feed,
    pub audio: Feed,
    _running: Vec<Running>,
}

impl Capture {
    pub fn start(cfg: &Config) -> Result<Self> {
        let (video, audio) = (Feed::default(), Feed::default());
        let mut running = Vec::new();
        if cfg.test_pattern {
            info!("capture: test pattern");
            running.push(testsrc::start(cfg.fps, video.clone(), audio.clone()));
        } else {
            let mut a = Running::new();
            pwaudio::start(&mut a, cfg.audio_target.clone(), audio.clone())?;
            running.push(a);
            let backend = select_backend(cfg);
            info!("capture: {}", backend.name());
            running.push(backend.start(cfg, video.clone())?);
        }
        Ok(Self {
            video,
            audio,
            _running: running,
        })
    }

    /// Why capture stopped, if it has. A requested stop wins over a failure.
    pub fn stopped(&self) -> Option<Stop> {
        match (self.video.stopped(), self.audio.stopped()) {
            (Some(r @ Stop::Requested(_)), _) | (_, Some(r @ Stop::Requested(_))) => Some(r),
            (v, a) => v.or(a),
        }
    }

    /// After a failure, waits up to `grace` in case it turns into a
    /// requested stop: a user's stop can break the stream slightly before
    /// the portal reports that the session was closed.
    pub fn settle(&self, grace: Duration) -> Option<Stop> {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(r @ Stop::Requested(_)) = self.stopped() {
                return Some(r);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.stopped()
    }
}
