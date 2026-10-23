//! Generated A/V sync pattern, pushed through the same `appsrc`s as real
//! capture. At the start of every second (in running time) the frame flashes
//! white and a 1 kHz beep plays, both for FLASH; a bar sweeps across the
//! frame once per second to show motion and frame pacing.

use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};

use gstreamer as gst;
use gstreamer_app as gst_app;
use log::debug;

use crate::feed::{PipelineClock, StopFlag};
use crate::pipeline::Stream;

/// Rendered small; the encoder chain scales it to the output size.
const WIDTH: usize = 640;
const HEIGHT: usize = 360;
const FLASH: gst::ClockTime = gst::ClockTime::from_mseconds(100);

const RATE: u64 = 48_000;
const CHANNELS: usize = 2;
/// 10 ms, matching the real capture's node.latency.
const CHUNK: u64 = 480;
const BEEP_HZ: f64 = 1000.0;

pub fn start(stream: &Stream, fps: u32, stop: &StopFlag) -> Vec<JoinHandle<()>> {
    stream.video.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .field("width", WIDTH as i32)
            .field("height", HEIGHT as i32)
            .field("framerate", gst::Fraction::new(fps as i32, 1))
            .build(),
    ));
    stream.audio.set_caps(Some(
        &gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", RATE as i32)
            .field("channels", CHANNELS as i32)
            .build(),
    ));

    let spawn = |name: &str,
                 src: &gst_app::AppSrc,
                 f: fn(gst_app::AppSrc, PipelineClock, u64, StopFlag)| {
        let (src, pipeline, stop) = (src.clone(), stream.pipeline.clone(), stop.clone());
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Some(clock) = PipelineClock::wait_playing(&pipeline, &stop) {
                    f(src, clock, fps as u64, stop);
                }
            })
            .expect("spawn source thread")
    };
    vec![
        spawn("test-video", &stream.video, video_loop),
        spawn("test-audio", &stream.audio, audio_loop),
    ]
}

/// PTS of frame `n`; exact at every second boundary.
fn frame_pts(n: u64, fps: u64) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(n * 1_000_000_000 / fps)
}

fn video_loop(src: gst_app::AppSrc, clock: PipelineClock, fps: u64, stop: StopFlag) {
    let mut n = (clock.now().nseconds() * fps).div_ceil(1_000_000_000);
    let bar_w = WIDTH / 32;
    while !stop.load(Ordering::SeqCst) {
        let pts = frame_pts(n, fps);
        clock.wait_until(pts);

        let in_second = gst::ClockTime::from_nseconds(pts.nseconds() % 1_000_000_000);
        let flash = in_second < FLASH;
        let mut frame = vec![if flash { 0xff } else { 0x20 }; WIDTH * HEIGHT * 4];
        if !flash {
            let x0 = (WIDTH - bar_w) * (n % fps) as usize / fps as usize;
            for row in frame.as_chunks_mut::<{ WIDTH * 4 }>().0 {
                row[x0 * 4..(x0 + bar_w) * 4].fill(0xff);
            }
        }

        let mut buf = gst::Buffer::from_mut_slice(frame);
        let b = buf.get_mut().unwrap();
        b.set_pts(pts);
        b.set_duration(frame_pts(n + 1, fps) - pts);
        if let Err(e) = src.push_buffer(buf) {
            debug!("test video stopped: {e:?}");
            return;
        }
        n += 1;
    }
}

fn audio_loop(src: gst_app::AppSrc, clock: PipelineClock, _fps: u64, stop: StopFlag) {
    let sample_pts = |s: u64| gst::ClockTime::from_nseconds(s * 1_000_000_000 / RATE);
    let beep_samples = FLASH.nseconds() * RATE / 1_000_000_000;
    // Start on a chunk boundary at or after now.
    let mut s = (clock.now().nseconds() * RATE / 1_000_000_000).div_ceil(CHUNK) * CHUNK;
    while !stop.load(Ordering::SeqCst) {
        // Real capture delivers a chunk once it has been recorded.
        clock.wait_until(sample_pts(s + CHUNK));

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
        let b = buf.get_mut().unwrap();
        b.set_pts(sample_pts(s));
        b.set_duration(sample_pts(s + CHUNK) - sample_pts(s));
        if let Err(e) = src.push_buffer(buf) {
            debug!("test audio stopped: {e:?}");
            return;
        }
        s += CHUNK;
    }
}
