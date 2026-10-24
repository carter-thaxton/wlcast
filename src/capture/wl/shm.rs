//! Shared-memory buffers the compositor copies frames into.

use std::fs::File;
use std::os::fd::AsFd;

use anyhow::{Context, Result};
use memmap2::Mmap;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;

use super::State;

/// 4-byte formats we accept, in order of preference, with their GStreamer
/// names (wl_shm formats are little-endian, GStreamer's are byte order).
const FORMATS: &[(Format, &str)] = &[
    (Format::Xrgb8888, "BGRx"),
    (Format::Argb8888, "BGRA"),
    (Format::Xbgr8888, "RGBx"),
    (Format::Abgr8888, "RGBA"),
];

/// Picks our preferred format among those the compositor offers.
pub fn pick_format(offered: &[Format]) -> Option<(Format, &'static str)> {
    FORMATS.iter().copied().find(|(f, _)| offered.contains(f))
}

pub struct ShmBuffer {
    pub buffer: WlBuffer,
    pool: WlShmPool,
    map: Mmap,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
}

impl ShmBuffer {
    pub fn new(
        shm: &WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        stride: u32,
        format: Format,
    ) -> Result<Self> {
        let size = stride as usize * height as usize;
        let file = File::from(
            memfd_create("wlcast-capture", MemfdFlags::CLOEXEC).context("memfd_create")?,
        );
        file.set_len(size as u64).context("sizing capture buffer")?;
        // SAFETY: the memfd is ours; only the compositor writes to it, and
        // only between a capture request and its ready event.
        let map = unsafe { Mmap::map(&file) }.context("mapping capture buffer")?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        Ok(Self {
            buffer,
            pool,
            map,
            width,
            height,
            stride,
            format,
        })
    }

    /// The frame, with rows packed (no stride padding).
    pub fn packed(&self) -> Vec<u8> {
        let row = self.width as usize * 4;
        let mut out = Vec::with_capacity(row * self.height as usize);
        for y in 0..self.height as usize {
            let start = y * self.stride as usize;
            out.extend_from_slice(&self.map[start..start + row]);
        }
        out
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
