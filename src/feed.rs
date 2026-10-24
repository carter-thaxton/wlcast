//! The link between a capture source and whichever pipeline is running.
//!
//! Sources outlive pipelines: an RTSP reconnect rebuilds the pipeline but
//! keeps the portal session and PipeWire streams. Sources timestamp buffers
//! with the system clock (which every pipeline uses), and the feed converts
//! that to the current pipeline's running time. Buffers pushed while no
//! pipeline is attached are dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use log::debug;

/// The clock all sources timestamp with and all pipelines run on.
pub fn clock() -> gst::Clock {
    gst::SystemClock::obtain()
}

#[derive(Clone, Default)]
pub struct Feed(Arc<Mutex<State>>);

/// Why a source stopped.
#[derive(Clone, Debug)]
pub enum Stop {
    /// Something broke; restart and recover.
    Failed(String),
    /// The user stopped it (e.g. "Stop sharing" in the desktop); exit.
    Requested(String),
}

#[derive(Default)]
struct State {
    caps: Option<gst::Caps>,
    target: Option<Target>,
    stopped: Option<Stop>,
    /// Last buffer pushed and its capture time, for keepalive repeats.
    last: Option<(gst::Buffer, gst::ClockTime)>,
}

struct Target {
    src: gst_app::AppSrc,
    base_time: gst::ClockTime,
    last_pts: Option<gst::ClockTime>,
}

impl Feed {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Sets the format of the buffers that follow.
    pub fn set_caps(&self, caps: gst::Caps) {
        let mut st = self.state();
        if let Some(t) = &st.target {
            t.src.set_caps(Some(&caps));
        }
        st.last = None;
        st.caps = Some(caps);
    }

    /// Pushes a buffer captured at `capture_time` (on [`clock`]).
    pub fn push(&self, buf: gst::Buffer, capture_time: gst::ClockTime) {
        let mut st = self.state();
        st.last = Some((buf.clone(), capture_time));
        if let Some(t) = &mut st.target {
            t.push(buf, capture_time);
        }
    }

    /// Re-pushes the last buffer if nothing was pushed for `idle`. Damage-
    /// driven capture sends nothing while the screen is still, but encoders
    /// and newly joined viewers need a steady stream of frames.
    pub fn repeat_if_idle(&self, idle: gst::ClockTime) {
        let now = clock().time();
        let mut st = self.state();
        let Some((buf, at)) = st.last.clone() else {
            return;
        };
        if now.saturating_sub(at) < idle {
            return;
        }
        st.last = Some((buf.clone(), now));
        if let Some(t) = &mut st.target {
            t.push(buf, now);
        }
    }

    /// Records a fatal source error; the supervisor restarts the capture.
    pub fn fail(&self, msg: impl Into<String>) {
        let mut st = self.state();
        if st.stopped.is_none() {
            st.stopped = Some(Stop::Failed(msg.into()));
        }
    }

    /// Records that the user asked the capture to stop; wlcast exits. This
    /// overrides a failure, which a stop can cause on the way.
    pub fn request_stop(&self, msg: impl Into<String>) {
        self.state().stopped = Some(Stop::Requested(msg.into()));
    }

    /// Why the source stopped, if it has.
    pub fn stopped(&self) -> Option<Stop> {
        self.state().stopped.clone()
    }

    /// Starts feeding a pipeline's appsrc. Call once the pipeline is PLAYING.
    pub fn attach(&self, src: gst_app::AppSrc, base_time: gst::ClockTime) {
        let mut st = self.state();
        if let Some(caps) = &st.caps {
            src.set_caps(Some(caps));
        }
        st.target = Some(Target {
            src,
            base_time,
            last_pts: None,
        });
    }

    pub fn detach(&self) {
        self.state().target = None;
    }
}

impl Target {
    fn push(&mut self, mut buf: gst::Buffer, capture_time: gst::ClockTime) {
        let Some(mut pts) = capture_time.checked_sub(self.base_time) else {
            return; // captured before this pipeline started
        };
        // Keep PTS strictly increasing even if pushes race.
        if let Some(last) = self.last_pts
            && pts <= last
        {
            pts = last + gst::ClockTime::from_nseconds(1);
        }
        self.last_pts = Some(pts);
        buf.make_mut().set_pts(pts);
        if let Err(e) = self.src.push_buffer(buf) {
            debug!("{}: push failed: {e:?}", self.src.name());
        }
    }
}

/// A running source. Stops when dropped.
pub struct Running {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    on_stop: Vec<Box<dyn FnOnce() + Send>>,
}

impl Running {
    pub fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            on_stop: Vec::new(),
        }
    }

    /// Set when the source should stop.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn add_thread(&mut self, t: JoinHandle<()>) {
        self.threads.push(t);
    }

    /// Runs `f` when stopping, after the stop flag is set and before joining
    /// threads (in reverse order of registration).
    pub fn on_stop(&mut self, f: impl FnOnce() + Send + 'static) {
        self.on_stop.push(Box::new(f));
    }

    /// Repeats the feed's last buffer whenever it has been idle for `idle`.
    pub fn keepalive(&mut self, feed: &Feed, idle: Duration) {
        let (feed, stop) = (feed.clone(), self.stop_flag());
        let idle_ct = gst::ClockTime::from_nseconds(idle.as_nanos() as u64);
        let t = thread::Builder::new()
            .name("keepalive".into())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(idle / 5);
                    feed.repeat_if_idle(idle_ct);
                }
            })
            .expect("spawn keepalive thread");
        self.add_thread(t);
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        while let Some(f) = self.on_stop.pop() {
            f();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
