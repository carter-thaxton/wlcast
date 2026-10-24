//! Generated A/V sync pattern, pushed through the same feeds as real
//! capture.  At the start of every second (on the clock) the frame flashes
//! white and a 1 kHz beep plays, both for FLASH; a bar sweeps across the
//! frame once per second to show motion and frame pacing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::feed::{self, Feed, Running};

/// Rendered small; the encoder chain scales it to the output size.
const WIDTH: usize = 640;
const HEIGHT: usize = 360;
const FLASH: gst::ClockTime = gst::ClockTime::from_mseconds(100);

const RATE: u64 = 48_000;
const CHANNELS: usize = 2;
/// 10 ms, matching the real capture's node.latency.
const CHUNK: u64 = 480;
const BEEP_HZ: f64 = 1000.0;

pub fn start(fps: u32, video: Feed, audio: Feed) -> Running {
    video.set_caps(
        gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .field("width", WIDTH as i32)
            .field("height", HEIGHT as i32)
            .field("framerate", gst::Fraction::new(fps as i32, 1))
            .build(),
    );
    audio.set_caps(
        gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", RATE as i32)
            .field("channels", CHANNELS as i32)
            .build(),
    );

    let mut running = Running::new();
    let fps = fps as u64;
    let spawn = |name: &str, feed: Feed, f: fn(Feed, u64, Arc<AtomicBool>), stop| {
        thread::Builder::new()
            .name(name.into())
            .spawn(move || f(feed, fps, stop))
            .expect("spawn test source thread")
    };
    running.add_thread(spawn("test-video", video, video_loop, running.stop_flag()));
    running.add_thread(spawn("test-audio", audio, audio_loop, running.stop_flag()));
    running
}

fn wait_until(clock: &gst::Clock, t: gst::ClockTime) {
    let _ = clock.new_single_shot_id(t).wait();
}

/// Time of frame `n`; exact at every second boundary.
fn frame_time(n: u64, fps: u64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(n * 1_000_000_000 / fps)
}

fn video_loop(feed: Feed, fps: u64, stop: Arc<AtomicBool>) {
    let clock = feed::clock();
    let mut n = (clock.time().nseconds() * fps).div_ceil(1_000_000_000);
    let bar_w = WIDTH / 32;
    while !stop.load(Ordering::SeqCst) {
        let t = frame_time(n, fps);
        wait_until(&clock, t);

        let in_second = gst::ClockTime::from_nseconds(t.nseconds() % 1_000_000_000);
        let flash = in_second < FLASH;
        let mut frame = vec![if flash { 0xff } else { 0x20 }; WIDTH * HEIGHT * 4];
        if !flash {
            let x0 = (WIDTH - bar_w) * (n % fps) as usize / fps as usize;
            for row in frame.as_chunks_mut::<{ WIDTH * 4 }>().0 {
                row[x0 * 4..(x0 + bar_w) * 4].fill(0xff);
            }
        }

        let mut buf = gst::Buffer::from_mut_slice(frame);
        buf.get_mut()
            .unwrap()
            .set_duration(frame_time(n + 1, fps) - t);
        feed.push(buf, t);
        n += 1;
    }
}

fn audio_loop(feed: Feed, _fps: u64, stop: Arc<AtomicBool>) {
    let clock = feed::clock();
    let sample_time = |s: u64| gst::ClockTime::from_nseconds(s * 1_000_000_000 / RATE);
    let beep_samples = FLASH.nseconds() * RATE / 1_000_000_000;
    // Start on a chunk boundary at or after now.
    let mut s = (clock.time().nseconds() * RATE / 1_000_000_000).div_ceil(CHUNK) * CHUNK;
    while !stop.load(Ordering::SeqCst) {
        // Real capture delivers a chunk once it has been recorded.
        wait_until(&clock, sample_time(s + CHUNK));

        let mut data = Vec::with_capacity(CHUNK as usize * CHANNELS * 2);
        for i in s..s + CHUNK {
            let t = i % RATE;
            let v = if t < beep_samples {
                (0.5 * (2.0 * std::f64::consts::PI * BEEP_HZ * t as f64 / RATE as f64).sin()
                    * i16::MAX as f64) as i16
            } else {
                0
            };
            for _ in 0..CHANNELS {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }

        let mut buf = gst::Buffer::from_mut_slice(data);
        buf.get_mut()
            .unwrap()
            .set_duration(sample_time(s + CHUNK) - sample_time(s));
        feed.push(buf, sample_time(s));
        s += CHUNK;
    }
}
