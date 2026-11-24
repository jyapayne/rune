//! A `wl_shm` buffer the compositor copies frames into: one memfd, one pool, one buffer, kept
//! by a capture for as long as its size and format stay the same, and read with plain reads.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;

use super::client::Client;

pub(super) struct ShmBuffer {
    file: File,
    pool: WlShmPool,
    pub(super) buffer: WlBuffer,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) format: Format,
}

impl ShmBuffer {
    pub(super) fn new(shm: &WlShm, qh: &QueueHandle<Client>, [width, height]: [u32; 2], stride: u32, format: Format) -> io::Result<ShmBuffer> {
        let len = u64::from(stride) * u64::from(height);
        let too_big = || io::Error::new(io::ErrorKind::InvalidInput, "capture buffer too large");
        let size = i32::try_from(len).map_err(|_| too_big())?;
        let (w, h, s) = (i32::try_from(width).map_err(|_| too_big())?, i32::try_from(height).map_err(|_| too_big())?, i32::try_from(stride).map_err(|_| too_big())?);
        let fd = memfd_create("rune-capture", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, len)?;
        let pool = shm.create_pool(fd.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(0, w, h, s, format, qh, ());
        Ok(ShmBuffer { file: File::from(fd), pool, buffer, width, height, stride, format })
    }

    pub(super) fn fits(&self, [width, height]: [u32; 2], stride: u32, format: Format) -> bool {
        self.width == width && self.height == height && self.stride == stride && self.format == format
    }

    /// The buffer's bytes, read into a vector of its own.
    pub(super) fn read(&self) -> io::Result<Vec<u8>> {
        let len = self.stride as usize * self.height as usize;
        let mut bytes = Vec::with_capacity(len);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        file.take(len as u64).read_to_end(&mut bytes)?;
        if bytes.len() < len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "capture buffer shorter than its size"));
        }
        Ok(bytes)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
