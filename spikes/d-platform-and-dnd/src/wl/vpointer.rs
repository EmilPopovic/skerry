// SPDX-License-Identifier: MIT OR Apache-2.0
//! Synthetic pointer input over `zwlr_virtual_pointer_v1`. Without this, every
//! drag-and-drop pair in the matrix is a human moving a mouse, which means the
//! matrix cannot be re-run after an edit and therefore cannot be trusted — the
//! same reason [12-measurement.md](../../../docs/12-measurement.md) insists a gate
//! nobody has watched fail is decoration.
//!
//! It is an instrument, not a shipping dependency: nothing in `sk-plat` will ever
//! create a virtual pointer.

use std::time::{Duration, Instant};

use wayland_client::protocol::wl_pointer;
use wayland_client::QueueHandle;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1;

use super::App;

/// `linux/input-event-codes.h`.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;

pub struct Pointer {
    p: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    extent: (u32, u32),
    t0: Instant,
}

impl Pointer {
    pub fn new(app: &App, qh: &QueueHandle<App>, extent: (u32, u32)) -> Option<Self> {
        let mgr = app.globals.vpm.as_ref()?;
        let p = mgr.create_virtual_pointer(Some(&app.globals.seat), qh, ());
        Some(Self { p, extent, t0: Instant::now() })
    }

    fn now(&self) -> u32 {
        self.t0.elapsed().as_millis() as u32
    }

    pub fn move_to(&self, x: u32, y: u32) {
        self.p.motion_absolute(self.now(), x, y, self.extent.0, self.extent.1);
        self.p.frame();
    }

    pub fn press(&self, button: u32) {
        self.p.button(self.now(), button, wl_pointer::ButtonState::Pressed);
        self.p.frame();
    }

    pub fn release(&self, button: u32) {
        self.p.button(self.now(), button, wl_pointer::ButtonState::Released);
        self.p.frame();
    }

    /// A drag is a press, *several* motions and a release. One motion is not
    /// enough: GTK and Qt both apply a drag threshold, and a compositor coalesces
    /// a press and a release at the same position into a click.
    pub fn drag(
        &self,
        app: &mut App,
        queue: &mut wayland_client::EventQueue<App>,
        from: (u32, u32),
        to: (u32, u32),
        steps: u32,
        step_dwell: Duration,
    ) {
        self.move_to(from.0, from.1);
        app.pump(queue, Duration::from_millis(60));
        self.press(BTN_LEFT);
        app.pump(queue, Duration::from_millis(60));
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let x = from.0 as f64 + (to.0 as f64 - from.0 as f64) * t;
            let y = from.1 as f64 + (to.1 as f64 - from.1 as f64) * t;
            self.move_to(x.round() as u32, y.round() as u32);
            app.pump(queue, step_dwell);
        }
        // Settle before releasing: the target has to have accepted a type, and a
        // release that races the accept produces a drop the peer then refuses.
        app.pump(queue, Duration::from_millis(250));
        self.release(BTN_LEFT);
        app.pump(queue, Duration::from_millis(200));
    }

    pub fn destroy(self) {
        self.p.destroy();
    }
}
