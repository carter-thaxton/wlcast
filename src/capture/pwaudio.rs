//! The default sink's monitor, captured with PipeWire.  Managed by
//! super::audio, which fills gaps with silence and retries on failure.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use log::info;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};

use super::audio::{CHANNELS, FRAME_BYTES, RATE, SharedTimeline};
use super::pw::{as_pod, check_state, pod_bytes, spawn, watch_core};
use crate::feed::{self, Feed, Running};

struct State {
    feed: Feed,
    clock: gst::Clock,
    timeline: SharedTimeline,
}

/// Starts capturing the default sink's monitor, or the sink named `target`,
/// into `feed`, timestamped on `timeline`.  Failures are reported on `feed`.
pub fn start(
    running: &mut Running,
    target: Option<String>,
    feed: Feed,
    timeline: SharedTimeline,
) -> Result<()> {
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
            timeline,
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
    info.set_rate(RATE as u32);
    info.set_channels(CHANNELS as u32);
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

    // Push while holding the timeline, so silence fill can't interleave.
    let mut timeline = st.timeline.lock().unwrap();
    let (pts, duration) = timeline.real(frames as u64, now);
    let mut buf = gst::Buffer::from_slice(samples[..frames * FRAME_BYTES].to_vec());
    buf.get_mut().unwrap().set_duration(duration);
    st.feed.push(buf, pts);
}
