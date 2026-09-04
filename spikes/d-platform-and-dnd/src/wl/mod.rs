// SPDX-License-Identifier: MIT OR Apache-2.0
//! The Wayland arm: `wayland-client` and nothing above it. Every policy decision
//! — which scale to use, which MIME types to offer, which drag action to accept —
//! is in this directory, which is the whole argument of
//! [ADR 0002](../../../docs/adr/0002-own-platform-layer.md) stated as code.

pub mod checks;
pub mod data;
pub mod hang;
pub mod ime;
pub mod paint;
pub mod seat;
pub mod vpointer;

use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_compositor, wl_data_device_manager, wl_output, wl_registry, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2;
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

use crate::report::Report;

/// Logical size. Small enough to place two of them side by side on a 2880x1800
/// screen without a compositor tiling rule getting a vote.
pub const LOGICAL: (u32, u32) = (480, 320);

pub struct Globals {
    pub compositor: wl_compositor::WlCompositor,
    pub shm: wl_shm::WlShm,
    pub seat: wl_seat::WlSeat,
    pub wm_base: xdg_wm_base::XdgWmBase,
    pub frac: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    pub ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    pub psm: Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    pub tim: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub imm: Option<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    pub vpm: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    /// An instrument only — see [hang.rs](hang.rs).
    pub dcm: Option<
        wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    >,
    /// Every advertised interface with its version, so a missing capability is a
    /// recorded fact rather than a guess about why a check failed.
    pub advertised: Vec<(String, u32)>,
}

pub struct App {
    pub globals: Globals,
    pub report: Report,

    pub surface: wl_surface::WlSurface,
    pub xdg_surface: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
    pub viewport: Option<wp_viewport::WpViewport>,
    pub canvas: Option<paint::Canvas>,
    pub paint: paint::Paint,

    /// `wp_fractional_scale_v1` reports a numerator over 120. Storing it as the
    /// numerator rather than as an `f32` is deliberate: 1.25 is exact, 1.1 is
    /// not, and the buffer size has to be derived from the integer the compositor
    /// actually sent.
    pub scale_120: u32,
    pub scale_events: u32,
    pub configured: bool,
    pub configure_serial: Option<u32>,
    pub buffer_px: (u32, u32),
    pub closed: bool,
    pub frame_pending: bool,

    pub seat_state: seat::SeatState,
    pub data: data::DataState,
    pub ime: ime::ImeState,
    pub hang: hang::HangState,
}

impl App {
    pub fn connect() -> (Connection, EventQueue<App>, App) {
        let conn = Connection::connect_to_env().expect("no Wayland display");
        let (list, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
        let qh = queue.handle();

        let advertised = list
            .contents()
            .clone_list()
            .into_iter()
            .map(|g| (g.interface, g.version))
            .collect::<Vec<_>>();

        let compositor = list.bind(&qh, 1..=6, ()).expect("wl_compositor");
        let shm = list.bind(&qh, 1..=1, ()).expect("wl_shm");
        let seat = list.bind(&qh, 5..=9, ()).expect("wl_seat");
        let wm_base: xdg_wm_base::XdgWmBase = list.bind(&qh, 1..=6, ()).expect("xdg_wm_base");

        let globals = Globals {
            frac: opt(&list, &qh, 1..=1),
            viewporter: opt(&list, &qh, 1..=1),
            ddm: opt(&list, &qh, 3..=3),
            psm: opt(&list, &qh, 1..=1),
            tim: opt(&list, &qh, 1..=1),
            imm: opt(&list, &qh, 1..=1),
            vpm: opt(&list, &qh, 1..=2),
            dcm: opt(&list, &qh, 1..=2),
            compositor,
            shm,
            seat,
            wm_base,
            advertised,
        };

        let surface = globals.compositor.create_surface(&qh, ());
        // The order matters: the fractional-scale object and the viewport are
        // created before the first commit, so the first configure already carries
        // the scale and there is never a frame at the wrong size. Spike A's bug
        // was the absence of exactly this.
        let viewport = globals.viewporter.as_ref().map(|v| v.get_viewport(&surface, &qh, ()));
        if let Some(f) = &globals.frac {
            f.get_fractional_scale(&surface, &qh, ());
        }
        let xdg_surface = globals.wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title("skerry spike D".into());
        toplevel.set_app_id("dev.skerry.spike-d".into());
        surface.commit();

        let seat_state = seat::SeatState::new(&globals.seat, &qh);

        let mut app = App {
            globals,
            report: Report::default(),
            surface,
            xdg_surface,
            toplevel,
            viewport,
            canvas: None,
            paint: paint::Paint::default(),
            scale_120: 120,
            scale_events: 0,
            configured: false,
            configure_serial: None,
            buffer_px: (0, 0),
            closed: false,
            frame_pending: false,
            seat_state,
            data: data::DataState::default(),
            ime: ime::ImeState::default(),
            hang: hang::HangState::default(),
        };
        queue.roundtrip(&mut app).expect("roundtrip");
        (conn, queue, app)
    }

    /// The buffer is sized from the integer the compositor sent, and the viewport
    /// destination is the logical size. Together those two make one buffer pixel
    /// one output pixel; either one alone does not.
    pub fn redraw(&mut self, qh: &QueueHandle<App>) {
        let w = (LOGICAL.0 * self.scale_120).div_ceil(120);
        let h = (LOGICAL.1 * self.scale_120).div_ceil(120);
        if self.canvas.as_ref().map(|c| (c.width, c.height)) != Some((w, h)) {
            if let Some(old) = self.canvas.take() {
                old.destroy();
            }
            self.canvas = Some(paint::Canvas::new(&self.globals.shm, qh, w, h));
        }
        self.buffer_px = (w, h);
        let canvas = self.canvas.as_mut().unwrap();
        canvas.paint(&self.paint);
        if let Some(v) = &self.viewport {
            v.set_destination(LOGICAL.0 as i32, LOGICAL.1 as i32);
        } else {
            // Without viewporter there is no way to express a fractional scale,
            // and the only honest fallback is an integer scale plus a smaller
            // logical window.
            self.surface.set_buffer_scale((self.scale_120 / 120).max(1) as i32);
        }
        self.surface.attach(Some(&canvas.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, w as i32, h as i32);
        self.surface.commit();
    }

    pub fn pump(&mut self, queue: &mut EventQueue<App>, dur: Duration) {
        let end = Instant::now() + dur;
        while Instant::now() < end {
            queue.flush().ok();
            if queue.dispatch_pending(self).unwrap_or(0) == 0 {
                std::thread::sleep(Duration::from_millis(2));
                if let Some(g) = queue.prepare_read() {
                    g.read().ok();
                }
            }
        }
        queue.dispatch_pending(self).ok();
    }

    /// Dispatch until `done` or the deadline. Returns whether `done` came true,
    /// so a check can distinguish "the compositor said no" from "nothing arrived".
    pub fn wait_for(
        &mut self,
        queue: &mut EventQueue<App>,
        dur: Duration,
        done: impl Fn(&App) -> bool,
    ) -> bool {
        let end = Instant::now() + dur;
        while Instant::now() < end {
            if done(self) {
                return true;
            }
            queue.flush().ok();
            queue.dispatch_pending(self).ok();
            if done(self) {
                return true;
            }
            if let Some(guard) = queue.prepare_read() {
                guard.read().ok();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        done(self)
    }
}

fn opt<I>(list: &GlobalList, qh: &QueueHandle<App>, ver: std::ops::RangeInclusive<u32>) -> Option<I>
where
    I: Proxy + 'static,
    App: Dispatch<I, ()>,
{
    list.bind(qh, ver, ()).ok()
}

// ---------------------------------------------------------------------------
// Dispatch for the objects that carry state this spike cares about.
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A compositor that pings and is not ponged kills the window. This is the
        // shortest example of what "the policy above the protocol is ours" costs.
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        surf: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surf.ack_configure(serial);
            app.configure_serial = Some(serial);
            app.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            app.closed = true;
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.scale_120 = scale;
            app.scale_events += 1;
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! noop_dispatch {
    ($($ty:path),* $(,)?) => { $(
        impl Dispatch<$ty, ()> for App {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )* };
}

noop_dispatch!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wl_data_device_manager::WlDataDeviceManager,
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
);
