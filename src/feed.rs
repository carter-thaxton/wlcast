//! Timing shared by all sources. The daemon owns timestamps: every buffer's
//! PTS is a running time on the pipeline clock, so audio and video from
//! different sources line up.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

#[derive(Clone)]
pub struct PipelineClock {
    clock: gst::Clock,
    base_time: gst::ClockTime,
}

impl PipelineClock {
    /// Blocks until the pipeline is PLAYING (when its clock and base time
    /// are fixed). Returns `None` if `stop` is set first.
    pub fn wait_playing(pipeline: &gst::Pipeline, stop: &AtomicBool) -> Option<Self> {
        while !stop.load(Ordering::SeqCst) {
            if pipeline.current_state() == gst::State::Playing
                && let (Some(clock), Some(base_time)) = (pipeline.clock(), pipeline.base_time())
            {
                return Some(Self { clock, base_time });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// Current running time.
    pub fn now(&self) -> gst::ClockTime {
        self.clock.time().saturating_sub(self.base_time)
    }

    /// Sleeps until the given running time.
    pub fn wait_until(&self, running_time: gst::ClockTime) {
        let id = self.clock.new_single_shot_id(self.base_time + running_time);
        let _ = id.wait();
    }
}

/// Set to ask source threads to exit.
pub type StopFlag = Arc<AtomicBool>;
