//! Capture sources. A video backend fills the video feed; system audio (or
//! the test pattern) fills the audio feed.

mod portal;
mod pw;
mod pwaudio;
mod pwvideo;
mod testsrc;

use std::time::{Duration, Instant};

use anyhow::Result;
use log::info;

use crate::config::Config;
use crate::feed::{Feed, Running, Stop};

/// A way of capturing the screen. Every backend pushes raw frames into the
/// video feed, timestamped on [`crate::feed::clock`].
pub trait VideoBackend {
    fn name(&self) -> &'static str;
    /// Starts capturing into `feed`; capture stops when the result drops.
    fn start(&self, cfg: &Config, feed: Feed) -> Result<Running>;
}

/// Picks the screen capture backend. The ext and wlr protocol backends will
/// be chosen here from the Wayland registry; for now it's always the portal.
fn select_backend() -> Box<dyn VideoBackend> {
    Box::new(portal::Portal)
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
            let backend = select_backend();
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
