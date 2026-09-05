// SPDX-License-Identifier: Apache-2.0
//! An `shm` surface, painted by hand. There is no renderer here on purpose:
//! Spike A already measured the renderer, and what is under test in this spike
//! is the protocol. What the pixels have to do is be *verifiable* — hence the
//! checkerboard, which is one physical pixel per cell and therefore survives
//! only if the compositor maps our buffer to the output one to one.

use std::os::fd::AsFd;

use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use rustix::mm::{mmap, MapFlags, ProtFlags};
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Connection, QueueHandle};

use super::App;

pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pixels: &'static mut [u32],
    pool: wl_shm_pool::WlShmPool,
    pub buffer: wl_buffer::WlBuffer,
}

/// One physical pixel per cell. Any resampling turns the two colours into a
/// gradient, which is what [Spike A](../../a-million-rows/README.md) found the
/// hard way when a 1146x1402 buffer was stretched to 1425x1750.
pub const CHECKER_ORIGIN: (u32, u32) = (16, 16);
pub const CHECKER_SIZE: (u32, u32) = (64, 64);
pub const CHECKER_DARK: u32 = 0x00_10_10_10;
pub const CHECKER_LIGHT: u32 = 0x00_f0_f0_f0;

impl Canvas {
    pub fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<App>, width: u32, height: u32) -> Self {
        let len = (width * height * 4) as usize;
        let fd = memfd_create("skerry-spike-d", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&fd, len as u64).expect("ftruncate");
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                fd.as_fd(),
                0,
            )
        }
        .expect("mmap");
        // The mapping outlives every use in a spike that exits when the check
        // set is done; leaking it is cheaper than threading a lifetime through
        // every Dispatch impl.
        let pixels = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, (width * height) as usize) };

        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        Self { width, height, pixels, pool, buffer }
    }

    pub fn paint(&mut self, state: &Paint) {
        let bg = if state.dnd_hover { 0x00_20_50_20 } else { 0x00_18_18_1c };
        self.pixels.fill(bg);

        for y in 0..CHECKER_SIZE.1 {
            for x in 0..CHECKER_SIZE.0 {
                let px = CHECKER_ORIGIN.0 + x;
                let py = CHECKER_ORIGIN.1 + y;
                if px < self.width && py < self.height {
                    let c = if (x + y) % 2 == 0 { CHECKER_DARK } else { CHECKER_LIGHT };
                    self.pixels[(py * self.width + px) as usize] = c;
                }
            }
        }

        // The focus ring is a golden-frame test in the real product (rule 5); here
        // it is only a way to see keyboard focus without rendering text.
        if state.keyboard_focus {
            self.stroke(4, 4, self.width - 8, self.height - 8, 0x00_4c_9a_ff);
        }
        // The text field: one bar per committed character, one lighter bar per
        // preedit character. The actual strings go to the log, where they can be
        // compared byte for byte.
        let mut x = CHECKER_ORIGIN.0;
        let y = CHECKER_ORIGIN.1 + CHECKER_SIZE.1 + 16;
        for _ in 0..state.committed_chars.min(40) {
            self.fill(x, y, 6, 20, 0x00_e0_e0_e0);
            x += 8;
        }
        for _ in 0..state.preedit_chars.min(40) {
            self.fill(x, y, 6, 20, 0x00_80_80_a0);
            x += 8;
        }
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, c: u32) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                self.pixels[(yy * self.width + xx) as usize] = c;
            }
        }
    }

    fn stroke(&mut self, x: u32, y: u32, w: u32, h: u32, c: u32) {
        self.fill(x, y, w, 2, c);
        self.fill(x, y + h - 2, w, 2, c);
        self.fill(x, y, 2, h, c);
        self.fill(x + w - 2, y, 2, h, c);
    }

    pub fn destroy(&self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Default)]
pub struct Paint {
    pub keyboard_focus: bool,
    pub dnd_hover: bool,
    pub committed_chars: usize,
    pub preedit_chars: usize,
}

impl wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl wayland_client::Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
