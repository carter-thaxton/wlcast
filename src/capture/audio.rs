//! System audio that never holds up video.  PipeWire captures the default
//! sink's monitor when it can; silence fills in when it can't: PipeWire
//! missing or restarting, no sink to monitor, or --audio false.  The stream
//! always has an audio track, so its format stays the same throughout.
//!
//! Real audio and silence share one timeline, so timestamps stay continuous
//! across the switch in either direction.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use log::{debug, info, warn};

use super::pwaudio;
use crate::feed::{self, Feed, Running, Stop};

pub const RATE: u64 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_BYTES: usize = 2 * CHANNELS; // S16LE
/// Silence comes in 10 ms chunks, like the real capture (node.latency).
const CHUNK: u64 = 480;
/// Fill with silence once real audio has been missing this long.
const FILL_AFTER: gst::ClockTime = gst::ClockTime::from_mseconds(100);
/// Try PipeWire again this long after it failed.
const RETRY: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(10);

/// Real-audio timestamps follow the sample count, and resync to arrival time
/// when they drift further than MAX_DRIFT from it for RESYNC_AFTER chunks in
/// a row (xruns, graph restarts, clock drift), or by more than MAX_JUMP at
/// once.  A single late callback doesn't count: the samples weren't late.
const MAX_DRIFT: gst::ClockTime = gst::ClockTime::from_mseconds(20);
const RESYNC_AFTER: u32 = 10;
const MAX_JUMP: gst::ClockTime = gst::ClockTime::from_mseconds(200);

pub fn caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("rate", RATE as i32)
        .field("channels", CHANNELS as i32)
        .build()
}

fn duration_of(frames: u64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(frames * 1_000_000_000 / RATE)
}

pub type SharedTimeline = Arc<Mutex<Timeline>>;

pub struct Timeline {
    /// Where the next chunk starts.
    next_pts: Option<gst::ClockTime>,
    /// When real audio last arrived, or when waiting for it began.
    last_real: gst::ClockTime,
    drifting: u32,
}

impl Timeline {
    fn new(now: gst::ClockTime) -> Self {
        Self {
            next_pts: None,
            last_real: now,
            drifting: 0,
        }
    }

    /// Timestamps a chunk of `frames` real samples that arrived at `now`.
    pub fn real(&mut self, frames: u64, now: gst::ClockTime) -> (gst::ClockTime, gst::ClockTime) {
        let duration = duration_of(frames);
        // The chunk ended at about `now`.
        let arrival = now.saturating_sub(duration);
        let pts = match self.next_pts {
            Some(next) => {
                let off = next.absdiff(arrival);
                self.drifting = if off > MAX_DRIFT {
                    self.drifting + 1
                } else {
                    0
                };
                if self.drifting >= RESYNC_AFTER || off > MAX_JUMP {
                    debug!(
                        "audio: resync by {} ms",
                        (arrival.nseconds() as i64 - next.nseconds() as i64) / 1_000_000
                    );
                    self.drifting = 0;
                    arrival
                } else {
                    next
                }
            }
            None => arrival,
        };
        self.next_pts = Some(pts + duration);
        self.last_real = now;
        (pts, duration)
    }

    /// If real audio has been missing for FILL_AFTER, pushes silence up to
    /// `now`, continuing the timeline.
    fn fill(&mut self, now: gst::ClockTime, mut push: impl FnMut(gst::Buffer, gst::ClockTime)) {
        if now.saturating_sub(self.last_real) < FILL_AFTER {
            return;
        }
        let duration = duration_of(CHUNK);
        let mut pts = match self.next_pts {
            // Don't backfill a long gap (e.g. after the feed was idle).
            Some(next) if now.saturating_sub(next) <= MAX_JUMP => next,
            _ => now.saturating_sub(duration),
        };
        while pts + duration <= now {
            let mut buf = gst::Buffer::from_mut_slice(vec![0u8; CHUNK as usize * FRAME_BYTES]);
            buf.get_mut().unwrap().set_duration(duration);
            push(buf, pts);
            pts += duration;
        }
        self.next_pts = Some(pts);
    }
}

/// Starts audio: PipeWire capture of the default sink's monitor (or of
/// `target`) if `enabled`, with silence filling any gaps.
pub fn start(enabled: bool, target: Option<String>, feed: Feed) -> Running {
    feed.set_caps(caps());
    let mut running = Running::new();
    let stop = running.stop_flag();
    let t = thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            let clock = feed::clock();
            let timeline: SharedTimeline = Arc::new(Mutex::new(Timeline::new(clock.time())));
            let stopping = || stop.load(std::sync::atomic::Ordering::SeqCst);
            if !enabled {
                info!("audio: disabled; sending silence");
            }
            let mut next_try = Instant::now();
            let mut warned = false;
            while !stopping() {
                if enabled && Instant::now() >= next_try {
                    let mut pw = Running::new();
                    match pwaudio::start(&mut pw, target.clone(), feed.clone(), timeline.clone()) {
                        Ok(()) => {
                            if warned {
                                info!("audio: PipeWire capture is back");
                            }
                            warned = false;
                            // Run until PipeWire fails, filling any gaps.
                            while !stopping() {
                                if let Some(Stop::Failed(e)) = feed.stopped() {
                                    warn!("{e}; sending silence until audio is back");
                                    warned = true;
                                    feed.clear_stopped();
                                    break;
                                }
                                timeline
                                    .lock()
                                    .unwrap()
                                    .fill(clock.time(), |b, t| feed.push(b, t));
                                thread::sleep(TICK);
                            }
                        }
                        Err(e) if !warned => {
                            warn!(
                                "audio unavailable ({e:#}); sending silence, retrying every {} s",
                                RETRY.as_secs()
                            );
                            warned = true;
                        }
                        Err(e) => debug!("audio still unavailable: {e:#}"),
                    }
                    drop(pw);
                    // Tearing the stream down can report one more failure.
                    feed.clear_stopped();
                    next_try = Instant::now() + RETRY;
                }
                timeline
                    .lock()
                    .unwrap()
                    .fill(clock.time(), |b, t| feed.push(b, t));
                thread::sleep(TICK);
            }
        })
        .expect("spawn audio thread");
    running.add_thread(t);
    running
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(n)
    }

    fn fill(t: &mut Timeline, now: gst::ClockTime) -> Vec<gst::ClockTime> {
        let mut pts = Vec::new();
        t.fill(now, |_, p| pts.push(p));
        pts
    }

    #[test]
    fn silence_waits_then_fills_continuously() {
        gst::init().unwrap();
        let mut t = Timeline::new(ms(1000));
        assert!(fill(&mut t, ms(1050)).is_empty(), "too early");
        let first = fill(&mut t, ms(1150));
        assert!(!first.is_empty());
        let next = fill(&mut t, ms(1200));
        // Chunks are 10 ms apart with no gap between calls.
        let all: Vec<_> = first.iter().chain(&next).collect();
        for w in all.windows(2) {
            assert_eq!(*w[1] - *w[0], ms(10));
        }
    }

    #[test]
    fn real_audio_continues_the_silence_timeline() {
        gst::init().unwrap();
        let mut t = Timeline::new(ms(0));
        let silence = fill(&mut t, ms(500));
        let end = *silence.last().unwrap() + ms(10);
        // A real 10 ms chunk arriving just after: no jump.
        let (pts, _) = t.real(480, ms(505));
        assert_eq!(pts, end);
        // And no silence while real audio flows.
        assert!(fill(&mut t, ms(540)).is_empty());
    }

    #[test]
    fn silence_continues_the_real_timeline() {
        gst::init().unwrap();
        let mut t = Timeline::new(ms(0));
        let (pts, dur) = t.real(480, ms(20));
        let silence = fill(&mut t, ms(200));
        assert_eq!(silence[0], pts + dur);
    }
}
