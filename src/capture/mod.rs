//! Capture sources. A video backend fills the video feed; system audio (or
//! the test pattern) fills the audio feed.

mod portal;
mod pw;
mod pwaudio;
mod pwvideo;
mod testsrc;

use anyhow::Result;
use log::info;

use crate::config::Config;
use crate::feed::{Feed, Running};

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

    /// Why capture stopped working, if it has.
    pub fn failure(&self) -> Option<String> {
        self.video.failure().or_else(|| self.audio.failure())
    }
}
