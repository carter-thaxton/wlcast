//! ext-image-copy-capture-v1: a capture session on an output's image
//! capture source. The session announces buffer constraints; each frame
//! copies into a buffer we attach.

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self as frame, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self as session, ExtImageCopyCaptureSessionV1,
};

use super::shm::ShmBuffer;
use super::{FrameState, State, offer_format};

pub struct Ext {
    manager: ExtImageCopyCaptureManagerV1,
    sources: ExtOutputImageCaptureSourceManagerV1,
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    frame: Option<ExtImageCopyCaptureFrameV1>,
}

impl Ext {
    pub fn start(
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        cursor: bool,
    ) -> Result<Self> {
        let manager: ExtImageCopyCaptureManagerV1 = globals
            .bind(qh, 1..=1, ())
            .context("ext_image_copy_capture_manager_v1")?;
        let sources: ExtOutputImageCaptureSourceManagerV1 = globals
            .bind(qh, 1..=1, ())
            .context("ext_output_image_capture_source_manager_v1")?;
        let source = sources.create_source(output, qh, ());
        let options = if cursor {
            Options::PaintCursors
        } else {
            Options::empty()
        };
        let session = manager.create_session(&source, options, qh, ());
        Ok(Self {
            manager,
            sources,
            source,
            session,
            frame: None,
        })
    }

    /// Captures into `buffer`. A fresh buffer is marked fully damaged; after
    /// that the compositor only needs to copy what changed, since we never
    /// write to the buffer ourselves.
    pub fn capture(&mut self, qh: &QueueHandle<State>, buffer: &ShmBuffer, fresh: bool) {
        let frame = self.session.create_frame(qh, ());
        frame.attach_buffer(&buffer.buffer);
        if fresh {
            frame.damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
        }
        frame.capture();
        self.frame = Some(frame);
    }

    pub fn discard_frame(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
    }
}

impl Drop for Ext {
    fn drop(&mut self) {
        self.discard_frame();
        self.session.destroy();
        self.source.destroy();
        self.sources.destroy();
        self.manager.destroy();
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let c = &mut state.constraints;
        match event {
            session::Event::BufferSize { width, height } => {
                c.begin_update();
                c.size = Some((width, height));
            }
            session::Event::ShmFormat { format } => {
                c.begin_update();
                offer_format(state, format);
            }
            session::Event::DmabufDevice { .. } | session::Event::DmabufFormat { .. } => {
                c.begin_update()
            }
            session::Event::Done => c.done = true,
            session::Event::Stopped => {
                state.stopped = Some("the capture session was stopped".into())
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use frame::FailureReason as R;
        match event {
            frame::Event::Ready => {
                state.frame = FrameState::Ready(state.clock.time());
            }
            frame::Event::Failed { reason } => {
                let (reason, retry) = match reason {
                    WEnum::Value(R::BufferConstraints) => ("buffer constraints changed", true),
                    WEnum::Value(R::Stopped) => ("session stopped", false),
                    _ => ("unknown reason", true),
                };
                state.frame = FrameState::Failed {
                    reason: reason.into(),
                    retry,
                };
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ExtImageCaptureSourceV1);
