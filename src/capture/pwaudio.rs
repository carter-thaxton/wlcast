//! System audio: the default sink's monitor, captured with PipeWire.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use log::{debug, info};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};

use super::pw::{as_pod, check_state, pod_bytes, spawn, watch_core};
use crate::feed::{self, Feed, Running};

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const FRAME_BYTES: usize = 2 * CHANNELS as usize; // S16LE
/// Timestamps follow the sample count, and resync to arrival time when they
/// drift further than MAX_DRIFT from it for RESYNC_AFTER chunks in a row
/// (xruns, graph restarts, clock drift), or by more than MAX_JUMP at once.
/// A single late callback doesn't count: the samples weren't late.
const MAX_DRIFT: gst::ClockTime = gst::ClockTime::from_mseconds(20);
const RESYNC_AFTER: u32 = 10;
const MAX_JUMP: gst::ClockTime = gst::ClockTime::from_mseconds(200);

struct State {
    feed: Feed,
    clock: gst::Clock,
    next_pts: Option<gst::ClockTime>,
    drifting: u32,
}

/// Starts capturing the default sink's monitor, or the sink named `target`.
pub fn start(running: &mut Running, target: Option<String>, feed: Feed) -> Result<()> {
    feed.set_caps(
        gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", RATE as i32)
            .field("channels", CHANNELS as i32)
            .build(),
    );
    let format_pod = pod_bytes(enum_format());
    spawn("pw-audio", running, move |mainloop| {
        let context = pw::context::ContextRc::new(mainloop, None).context("PipeWire context")?;
        let core = context.connect_rc(None).context("connect to PipeWire")?;
        let core_listener = watch_core(&core, &feed, "audio");
        let mut props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => "wlcast-audio",
            *pw::keys::NODE_LATENCY => "480/48000",
            "stream.capture.sink" => "true",
        };
        if let Some(t) = &target {
            props.insert("target.object", t.as_str());
        }
        let stream = pw::stream::StreamRc::new(core.clone(), "wlcast-audio", props)?;
        let state = State {
            feed: feed.clone(),
            clock: feed::clock(),
            next_pts: None,
            drifting: 0,
        };
        let listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, st, old, new| check_state(&st.feed, "audio", &old, &new))
            .param_changed(|_, _, id, param| {
                if id == spa::param::ParamType::Format.as_raw()
                    && let Some(param) = param
                {
                    let mut info = AudioInfoRaw::new();
                    if info.parse(param).is_ok() {
                        info!(
                            "audio: {:?} {} Hz, {} ch",
                            info.format(),
                            info.rate(),
                            info.channels()
                        );
                    }
                }
            })
            .process(on_process)
            .register()?;
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut [as_pod(&format_pod)],
        )?;
        Ok(Box::new((context, core, core_listener, stream, listener)))
    })
}

fn enum_format() -> spa::pod::Object {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    }
}

fn on_process(stream: &pw::stream::Stream, st: &mut State) {
    let now = st.clock.time();
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(d) = datas.first_mut() else { return };
    let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
    let Some(bytes) = d.data() else { return };
    let Some(samples) = bytes.get(offset..offset + size) else {
        return;
    };
    let frames = samples.len() / FRAME_BYTES;
    if frames == 0 {
        return;
    }

    // The chunk ended at about `now`. Follow the sample count while it stays
    // close to that, so timestamps don't pick up callback jitter.
    let duration = gst::ClockTime::from_nseconds(frames as u64 * 1_000_000_000 / RATE as u64);
    let arrival = now.saturating_sub(duration);
    let pts = match st.next_pts {
        Some(next) => {
            let off = next.absdiff(arrival);
            st.drifting = if off > MAX_DRIFT { st.drifting + 1 } else { 0 };
            if st.drifting >= RESYNC_AFTER || off > MAX_JUMP {
                debug!(
                    "audio: resync by {} ms",
                    (arrival.nseconds() as i64 - next.nseconds() as i64) / 1_000_000
                );
                st.drifting = 0;
                arrival
            } else {
                next
            }
        }
        None => arrival,
    };
    st.next_pts = Some(pts + duration);

    let mut buf = gst::Buffer::from_slice(samples[..frames * FRAME_BYTES].to_vec());
    buf.get_mut().unwrap().set_duration(duration);
    st.feed.push(buf, pts);
}
