//! Screen frames from a PipeWire video node (the portal's screencast).
//!
//! Offers only shared-memory formats (no DMA-BUF modifiers), so frames can be
//! copied straight into GStreamer buffers.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use log::{info, warn};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{object, property};
use pw::spa::utils::{Fraction, Rectangle, SpaTypes};

use super::pw::{as_pod, check_state, pod_bytes, spawn, watch_core};
use crate::config::Config;
use crate::feed::{self, Feed, Running};

/// 4-byte formats we accept, with their GStreamer names.
const FORMATS: &[(VideoFormat, &str)] = &[
    (VideoFormat::BGRx, "BGRx"),
    (VideoFormat::BGRA, "BGRA"),
    (VideoFormat::RGBx, "RGBx"),
    (VideoFormat::RGBA, "RGBA"),
    (VideoFormat::xRGB, "xRGB"),
    (VideoFormat::xBGR, "xBGR"),
    (VideoFormat::ARGB, "ARGB"),
    (VideoFormat::ABGR, "ABGR"),
];

#[derive(Clone, Copy)]
struct Layout {
    width: usize,
    height: usize,
}

struct State {
    feed: Feed,
    clock: gst::Clock,
    layout: Option<Layout>,
    warned_short: bool,
}

/// Starts capturing node `node` on the PipeWire remote `fd`.
pub fn start(
    running: &mut Running,
    fd: OwnedFd,
    node: u32,
    cfg: &Config,
    feed: Feed,
) -> Result<()> {
    let format_pod = pod_bytes(enum_format(cfg));
    spawn("pw-video", running, move |mainloop| {
        let context = pw::context::ContextRc::new(mainloop, None).context("PipeWire context")?;
        let core = context
            .connect_fd_rc(fd, None)
            .context("connect to portal's PipeWire remote")?;
        let core_listener = watch_core(&core, &feed, "video");
        let stream = pw::stream::StreamRc::new(
            core.clone(),
            "wlcast-video",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )?;
        let state = State {
            feed: feed.clone(),
            clock: feed::clock(),
            layout: None,
            warned_short: false,
        };
        let listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, st, old, new| check_state(&st.feed, "video", &old, &new))
            .param_changed(on_param_changed)
            .process(on_process)
            .register()?;
        stream.connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [as_pod(&format_pod)],
        )?;
        Ok(Box::new((context, core, core_listener, stream, listener)))
    })
}

fn enum_format(cfg: &Config) -> spa::pod::Object {
    let fps = cfg.fps;
    object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        // Keep in sync with FORMATS. The first is the default.
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::xRGB,
            VideoFormat::xBGR,
            VideoFormat::ARGB,
            VideoFormat::ABGR
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: cfg.width,
                height: cfg.height
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        // Screencasts are usually variable rate (0/1); cap the rate at ours.
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
    )
}

fn on_param_changed(
    _: &pw::stream::Stream,
    st: &mut State,
    id: u32,
    param: Option<&spa::pod::Pod>,
) {
    let Some(param) = param else { return };
    if id != spa::param::ParamType::Format.as_raw() {
        return;
    }
    let mut info = VideoInfoRaw::new();
    if let Err(e) = info.parse(param) {
        st.feed.fail(format!("video: can't parse format: {e:?}"));
        return;
    }
    let Some(&(_, name)) = FORMATS.iter().find(|f| f.0 == info.format()) else {
        st.feed
            .fail(format!("video: unexpected format {:?}", info.format()));
        return;
    };
    let size = info.size();
    info!("video: {name} {}x{}", size.width, size.height);
    st.layout = Some(Layout {
        width: size.width as usize,
        height: size.height as usize,
    });
    st.feed.set_caps(
        gst::Caps::builder("video/x-raw")
            .field("format", name)
            .field("width", size.width as i32)
            .field("height", size.height as i32)
            .field("framerate", gst::Fraction::new(0, 1))
            .build(),
    );
}

fn on_process(stream: &pw::stream::Stream, st: &mut State) {
    let now = st.clock.time();
    // Use the newest frame; older ones go back to PipeWire when dropped.
    let mut newest = None;
    while let Some(b) = stream.dequeue_buffer() {
        newest = Some(b);
    }
    let (Some(mut buffer), Some(layout)) = (newest, st.layout) else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(d) = datas.first_mut() else { return };
    let chunk = d.chunk();
    let (size, offset, stride) = (
        chunk.size() as usize,
        chunk.offset() as usize,
        chunk.stride(),
    );
    // Size 0 means no new picture (e.g. a cursor-only update).
    if size == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
        return;
    }
    let row = layout.width * 4;
    let stride = if stride > 0 { stride as usize } else { row };
    let Some(bytes) = d.data() else { return };
    let needed = offset + stride * (layout.height - 1) + row;
    if bytes.len() < needed {
        if !st.warned_short {
            warn!(
                "video: frame smaller than its format ({} < {needed} bytes); skipping",
                bytes.len()
            );
            st.warned_short = true;
        }
        return;
    }

    let mut frame = Vec::with_capacity(row * layout.height);
    for y in 0..layout.height {
        let start = offset + y * stride;
        frame.extend_from_slice(&bytes[start..start + row]);
    }
    st.feed.push(gst::Buffer::from_mut_slice(frame), now);
}
