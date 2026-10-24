//! wlr-screencopy-unstable-v1 (v3): one frame object per capture, each
//! announcing its buffer constraints before we attach a buffer.

use anyhow::{Context, Result};
use gstreamer::prelude::*;
use log::warn;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self as frame, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use super::shm::ShmBuffer;
use super::{FrameState, State, offer_format};

pub struct Wlr {
    manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
    cursor: bool,
    frame: Option<ZwlrScreencopyFrameV1>,
}

impl Wlr {
    pub fn new(
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        output: WlOutput,
        cursor: bool,
    ) -> Result<Self> {
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(qh, 3..=3, ())
            .context("zwlr_screencopy_manager_v1 v3")?;
        Ok(Self {
            manager,
            output,
            cursor,
            frame: None,
        })
    }

    /// Starts a capture; the frame then announces its buffer constraints.
    pub fn request(&mut self, qh: &QueueHandle<State>) {
        self.frame = Some(
            self.manager
                .capture_output(self.cursor as i32, &self.output, qh, ()),
        );
    }

    /// Copies into `buffer`. A fresh buffer is filled right away; after that
    /// the copy waits until the screen changes.
    pub fn copy(&mut self, buffer: &ShmBuffer, fresh: bool) {
        if let Some(f) = &self.frame {
            if fresh {
                f.copy(&buffer.buffer);
            } else {
                f.copy_with_damage(&buffer.buffer);
            }
        }
    }

    pub fn discard_frame(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
    }
}

impl Drop for Wlr {
    fn drop(&mut self) {
        self.discard_frame();
        self.manager.destroy();
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            frame::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                state.constraints.size = Some((width, height));
                state.constraints.stride = Some(stride);
                offer_format(state, format);
            }
            frame::Event::BufferDone => {
                state.constraints.done = true;
                if matches!(state.frame, FrameState::AwaitingBuffer) {
                    state.frame = FrameState::BufferKnown;
                }
            }
            frame::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags
                    && f.contains(frame::Flags::YInvert)
                {
                    warn!("wayland: frames are upside down (y_invert), which isn't handled yet");
                }
            }
            frame::Event::Ready { .. } => {
                state.frame = FrameState::Ready(state.clock.time());
            }
            frame::Event::Failed => {
                state.frame = FrameState::Failed {
                    reason: "the compositor couldn't copy the frame".into(),
                    retry: true,
                };
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ZwlrScreencopyManagerV1);
