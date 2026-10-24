//! Native Wayland screen capture for wlroots-based compositors (labwc, sway,
//! ...): ext-image-copy-capture-v1, or wlr-screencopy-unstable-v1 (v3) where
//! that's all there is.
//!
//! Both copy an output into a shared-memory buffer we provide.  The compositor
//! completes a capture only once something on screen has changed, and we ask
//! for at most --fps captures per second; the feed's keepalive repeats frames
//! while the screen is still.

mod ext;
mod shm;
mod wlr;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use log::{debug, info, warn};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use self::shm::{ShmBuffer, pick_format};
use super::{VideoBackend, keepalive_interval};
use crate::config::Config;
use crate::feed::{self, Feed, Running};

/// Give up after this many failed captures in a row.
const MAX_FAILURES: u32 = 5;
/// Longest wait for Wayland events, so stop requests are noticed promptly.
const MAX_WAIT: Duration = Duration::from_millis(100);
/// How often to log the capture rate (at debug level).
const STATS_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Ext,
    Wlr,
}

/// Which native capture protocol the compositor offers, if any.  `None` also
/// when there's no Wayland connection.
pub fn probe() -> Option<Protocol> {
    let conn = Connection::connect_to_env().ok()?;
    let (globals, _queue) = registry_queue_init::<Probe>(&conn).ok()?;
    let has = |name: &str, version: u32| {
        globals.contents().with_list(|list| {
            list.iter()
                .any(|g| g.interface == name && g.version >= version)
        })
    };
    if has(ExtImageCopyCaptureManagerV1::interface().name, 1)
        && has(ExtOutputImageCaptureSourceManagerV1::interface().name, 1)
    {
        Some(Protocol::Ext)
    } else if has(ZwlrScreencopyManagerV1::interface().name, 3) {
        Some(Protocol::Wlr)
    } else {
        None
    }
}

struct Probe;

impl Dispatch<WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct Wayland(pub Protocol);

impl VideoBackend for Wayland {
    fn name(&self) -> &'static str {
        match self.0 {
            Protocol::Ext => "ext-image-copy-capture",
            Protocol::Wlr => "wlr-screencopy",
        }
    }

    fn start(&self, cfg: &Config, feed: Feed) -> Result<Running> {
        let mut running = Running::new();
        let stop = running.stop_flag();
        let (protocol, output, cursor, fps) = (self.0, cfg.output.clone(), cfg.cursor, cfg.fps);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let thread_feed = feed.clone();
        let t = thread::Builder::new()
            .name("wl-capture".into())
            .spawn(move || {
                let mut capture = match Capture::connect(
                    protocol,
                    output.as_deref(),
                    cursor,
                    thread_feed.clone(),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                if let Err(e) = capture.run(fps, &stop) {
                    thread_feed.fail(format!("wayland capture: {e:#}"));
                }
            })?;
        running.add_thread(t);
        ready_rx
            .recv()
            .map_err(|_| anyhow!("wayland capture thread exited during setup"))??;
        running.keepalive(&feed, keepalive_interval(cfg.fps));
        Ok(running)
    }
}

struct Output {
    global: u32,
    proxy: WlOutput,
    name: Option<String>,
}

/// What the compositor said about the buffer it wants.
#[derive(Default)]
struct Constraints {
    size: Option<(u32, u32)>,
    /// wlr-screencopy dictates the stride; for ext we pick it.
    stride: Option<u32>,
    formats: Vec<Format>,
    done: bool,
}

impl Constraints {
    /// Constraint events after `done` start a new set.
    fn begin_update(&mut self) {
        if self.done {
            *self = Self::default();
        }
    }
}

enum FrameState {
    Idle,
    /// wlr: frame created, waiting for its buffer constraints.
    AwaitingBuffer,
    /// wlr: constraints known, buffer not yet attached.
    BufferKnown,
    Copying,
    /// Copied at this clock time.
    Ready(gst::ClockTime),
    Failed {
        reason: String,
        retry: bool,
    },
}

/// Event-handler state shared by both protocols.
pub struct State {
    clock: gst::Clock,
    outputs: Vec<Output>,
    /// Registry name of the output being captured.
    captured: Option<u32>,
    constraints: Constraints,
    frame: FrameState,
    /// Set when capture can't continue.
    stopped: Option<String>,
}

enum Protocols {
    Ext(ext::Ext),
    Wlr(wlr::Wlr),
}

struct Capture {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    _globals: GlobalList,
    shm: WlShm,
    state: State,
    protocol: Protocols,
    buffer: Option<ShmBuffer>,
    feed: Feed,
    failures: u32,
    /// Frames captured since `stats_since`.
    captured: u32,
    stats_since: Instant,
}

impl Capture {
    fn connect(
        protocol: Protocol,
        want_output: Option<&str>,
        cursor: bool,
        feed: Feed,
    ) -> Result<Self> {
        let conn =
            Connection::connect_to_env().context("can't connect to the Wayland compositor")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("Wayland registry")?;
        let qh = queue.handle();
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;

        let mut state = State {
            clock: feed::clock(),
            outputs: Vec::new(),
            captured: None,
            constraints: Constraints::default(),
            frame: FrameState::Idle,
            stopped: None,
        };
        for g in globals.contents().clone_list() {
            if g.interface == WlOutput::interface().name {
                let proxy = globals.registry().bind::<WlOutput, _, _>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    state.outputs.len(),
                );
                state.outputs.push(Output {
                    global: g.name,
                    proxy,
                    name: None,
                });
            }
        }
        queue.roundtrip(&mut state).context("Wayland roundtrip")?; // output names

        let names: Vec<String> = state.outputs.iter().map(Output::label).collect();
        let index = match want_output {
            Some(want) => names
                .iter()
                .position(|n| n == want)
                .ok_or_else(|| anyhow!("no output named {want} (outputs: {})", names.join(", ")))?,
            None if names.is_empty() => bail!("the compositor has no outputs"),
            None => 0,
        };
        let output = &state.outputs[index];
        info!(
            "wayland: capturing output {} (outputs: {})",
            names[index],
            names.join(", ")
        );
        state.captured = Some(output.global);
        let output = output.proxy.clone();

        let protocol = match protocol {
            Protocol::Ext => Protocols::Ext(ext::Ext::start(&globals, &qh, &output, cursor)?),
            Protocol::Wlr => Protocols::Wlr(wlr::Wlr::new(&globals, &qh, output, cursor)?),
        };
        Ok(Self {
            conn,
            queue,
            qh,
            _globals: globals,
            shm,
            state,
            protocol,
            buffer: None,
            feed,
            failures: 0,
            captured: 0,
            stats_since: Instant::now(),
        })
    }

    fn run(&mut self, fps: u32, stop: &AtomicBool) -> Result<()> {
        let period = Duration::from_secs_f64(1.0 / fps as f64);
        let mut next_due = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(why) = self.state.stopped.take() {
                bail!(why);
            }

            let now = Instant::now();
            match std::mem::replace(&mut self.state.frame, FrameState::Idle) {
                FrameState::Idle if now >= next_due => {
                    if self.request()? {
                        // Keep the pace even if a capture took longer than
                        // a frame period, without bursting to catch up.
                        next_due = (next_due + period).max(now);
                    }
                }
                FrameState::BufferKnown => self.copy()?,
                FrameState::Ready(at) => {
                    self.failures = 0;
                    self.deliver(at);
                }
                FrameState::Failed { reason, retry } => {
                    self.discard_frame();
                    self.failures += 1;
                    if !retry || self.failures >= MAX_FAILURES {
                        bail!("capture failed: {reason}");
                    }
                    debug!("wayland: capture failed ({reason}); retrying");
                }
                other => self.state.frame = other,
            }

            let wait = match self.state.frame {
                FrameState::Idle => next_due.saturating_duration_since(Instant::now()),
                _ => MAX_WAIT,
            };
            self.wait(wait.min(MAX_WAIT))?;
        }
    }

    /// Starts a capture.  Returns false if it has to wait (ext: constraints
    /// not known yet).
    fn request(&mut self) -> Result<bool> {
        match &mut self.protocol {
            Protocols::Ext(ext) => {
                if !self.state.constraints.done {
                    return Ok(false);
                }
                let fresh = Self::ensure_buffer(
                    &mut self.buffer,
                    &self.state.constraints,
                    &self.shm,
                    &self.qh,
                    &self.feed,
                )?;
                ext.capture(&self.qh, self.buffer.as_ref().unwrap(), fresh);
                self.state.frame = FrameState::Copying;
            }
            Protocols::Wlr(wlr) => {
                self.state.constraints = Constraints::default();
                wlr.request(&self.qh);
                self.state.frame = FrameState::AwaitingBuffer;
            }
        }
        Ok(true)
    }

    /// wlr: copies into our buffer once the frame's constraints are known.
    fn copy(&mut self) -> Result<()> {
        if let Protocols::Wlr(wlr) = &mut self.protocol {
            let fresh = Self::ensure_buffer(
                &mut self.buffer,
                &self.state.constraints,
                &self.shm,
                &self.qh,
                &self.feed,
            )?;
            wlr.copy(self.buffer.as_ref().unwrap(), fresh);
            self.state.frame = FrameState::Copying;
        }
        Ok(())
    }

    /// Makes sure the buffer matches the constraints.  Returns true if it was
    /// (re)created, in which case its contents are undefined.
    fn ensure_buffer(
        buffer: &mut Option<ShmBuffer>,
        c: &Constraints,
        shm: &WlShm,
        qh: &QueueHandle<State>,
        feed: &Feed,
    ) -> Result<bool> {
        let (width, height) = c
            .size
            .ok_or_else(|| anyhow!("compositor gave no buffer size"))?;
        let (format, gst_format) = pick_format(&c.formats)
            .ok_or_else(|| anyhow!("no supported shm format among {:?}", c.formats))?;
        let stride = c.stride.unwrap_or(width * 4);
        if let Some(b) = buffer
            && (b.width, b.height, b.stride, b.format) == (width, height, stride, format)
        {
            return Ok(false);
        }
        info!("wayland: {gst_format} {width}x{height}");
        *buffer = None; // free the old one first
        *buffer = Some(ShmBuffer::new(shm, qh, width, height, stride, format)?);
        feed.set_caps(
            gst::Caps::builder("video/x-raw")
                .field("format", gst_format)
                .field("width", width as i32)
                .field("height", height as i32)
                .field("framerate", gst::Fraction::new(0, 1))
                .build(),
        );
        Ok(true)
    }

    fn deliver(&mut self, at: gst::ClockTime) {
        self.discard_frame();
        self.captured += 1;
        let elapsed = self.stats_since.elapsed();
        if elapsed >= STATS_EVERY {
            debug!(
                "wayland: {:.1} frames/s captured (the rest are repeats)",
                self.captured as f64 / elapsed.as_secs_f64()
            );
            (self.captured, self.stats_since) = (0, Instant::now());
        }
        if let Some(b) = &self.buffer {
            self.feed.push(gst::Buffer::from_mut_slice(b.packed()), at);
        }
    }

    fn discard_frame(&mut self) {
        match &mut self.protocol {
            Protocols::Ext(ext) => ext.discard_frame(),
            Protocols::Wlr(wlr) => wlr.discard_frame(),
        }
    }

    /// Flushes requests, then waits up to `timeout` for events and
    /// dispatches them.
    fn wait(&mut self, timeout: Duration) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.conn.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let ts = Timespec {
                tv_sec: timeout.as_secs() as _,
                tv_nsec: timeout.subsec_nanos() as _,
            };
            let ready = match poll(&mut fds, Some(&ts)) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(e).context("poll"),
            };
            if ready {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e).context("reading Wayland events"),
                }
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

impl Output {
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("output-{}", self.global))
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::GlobalRemove { name } if state.captured == Some(name) => {
                state.stopped = Some("the captured output was removed".into());
            }
            wl_registry::Event::Global { interface, .. }
                if interface == WlOutput::interface().name =>
            {
                info!("wayland: an output was added");
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(o) = state.outputs.get_mut(*index)
        {
            o.name = Some(name);
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlBuffer);

/// Adds an offered shm format to the constraints.
fn offer_format(state: &mut State, format: WEnum<Format>) {
    match format {
        WEnum::Value(f) => state.constraints.formats.push(f),
        WEnum::Unknown(v) => warn!("wayland: unknown shm format {v:#x}"),
    }
}
