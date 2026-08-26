//! The Wayland window: `wayland-client` and our own policy above it.
//!
//! [ADR 0002](../../../../docs/adr/0002-own-platform-layer.md) is the reason
//! this file exists rather than a call into a windowing crate. Spike D answered
//! its revisit trigger with a number: `winit` reaches five of the nine
//! capabilities [docs/05-platform-integration.md](../../../../docs/05-platform-integration.md)
//! requires, and the four it misses are drag-source, primary selection,
//! `text-input-v3` and exact fractional scale.
//!
//! Two ordering invariants from Spike D are structural here, because each one
//! was a silent failure that looked like a compositor bug:
//!
//! * The fractional-scale object and the viewport are created **before the
//!   first commit**, so the first configure already carries the scale and there
//!   is never a frame at the wrong size.
//! * A selection may only be set with a serial from a real input event, so the
//!   serials are kept per device rather than one "latest" value.

pub mod clipboard;
mod seat;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use sk_model::{Problem, ProblemKind};
use sk_util::Waker;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_compositor, wl_data_device_manager, wl_output, wl_registry, wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel};

use crate::event::{Event, Size};

use self::seat::Seat;

/// How the window should first appear.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// The title a compositor shows in a task switcher.
    pub title: String,
    /// The `app_id`, which is how a desktop matches a window to its
    /// `.desktop` file. Reverse-DNS by freedesktop convention.
    pub app_id: String,
    /// The size in logical pixels, used until the compositor names one.
    pub logical: (u32, u32),
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: sk_model::t!("app.name").to_owned(),
            app_id: "dev.skerry.Skerry".to_owned(),
            logical: (1100, 720),
        }
    }
}

/// What this compositor turned out to support.
///
/// Probed, never compiled in — [ADR 0007](../../../../docs/adr/0007-runtime-capability-detection.md).
/// Recorded as a value rather than checked at each use so that a missing
/// capability is a fact the log carries, not a branch nobody can see.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    /// `wp_fractional_scale_v1`. Without it, scale is an integer and text at
    /// 125% is either too small or resampled.
    pub fractional_scale: bool,
    /// `wp_viewporter`. Required to express a fractional scale at all: it maps
    /// a physical-size buffer onto a logical-size surface.
    pub viewporter: bool,
    /// `zxdg_decoration_manager_v1`, and whether the compositor agreed to draw
    /// the frame itself.
    pub server_side_decorations: bool,
    /// A keyboard is present on the seat.
    pub keyboard: bool,
    /// A pointer is present on the seat.
    pub pointer: bool,
    /// `wl_data_device_manager`: the clipboard, and later drag-and-drop.
    pub clipboard: bool,
    /// `zwp_primary_selection_device_manager_v1`. Present here does **not** mean
    /// usable: Hyprland advertises it and silently discards `set_selection`
    /// unless `misc:middle_click_paste` is on, which Spike D found by
    /// elimination because nothing reports it.
    pub primary_selection: bool,
    /// Every advertised global with its version, so "why did that not work" has
    /// an answer that does not need a second run.
    pub advertised: Vec<(String, u32)>,
}

/// A window, its surface, and the input state that belongs to it.
pub struct Window {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}

struct State {
    surface: wl_surface::WlSurface,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    viewport: Option<wp_viewport::WpViewport>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,

    capabilities: Capabilities,
    seat: Seat,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    primary_manager:
        Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    clipboard: clipboard::Clipboard,
    /// An offer whose types are still arriving, before the `selection` event
    /// that adopts it.
    pending_offer: Option<wayland_client::protocol::wl_data_offer::WlDataOffer>,
    pending_types: Vec<String>,
    pending_primary_offer: Option<
        wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    >,
    pending_primary_types: Vec<String>,

    /// The compositor's scale as a numerator over 120.
    ///
    /// Kept as the integer the compositor sent rather than as an `f32`: 1.25 is
    /// exact and 1.1 is not, and every buffer dimension is derived from this.
    scale_120: u32,
    /// The integer scale, used only when there is no fractional-scale manager.
    fallback_scale: u32,
    logical: (u32, u32),
    /// What the last `Resized` reported, so an unchanged configure is not an
    /// event and does not cost a frame.
    reported: Option<((u32, u32), u32)>,
    configured: bool,
    close_requested: bool,
    events: VecDeque<Event>,
}

impl Window {
    /// Connects to the compositor and maps a window.
    pub fn open(config: &WindowConfig) -> Result<Self, Problem> {
        let connection = Connection::connect_to_env().map_err(|error| {
            Problem::plain(ProblemKind::NoDisplay, format!("wl_display connect: {error}"))
        })?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection).map_err(|error| {
            Problem::plain(ProblemKind::NoDisplay, format!("wl_registry: {error}"))
        })?;
        let handle = queue.handle();

        let advertised = globals
            .contents()
            .clone_list()
            .into_iter()
            .map(|global| (global.interface, global.version))
            .collect();

        let compositor: wl_compositor::WlCompositor =
            bind(&globals, &handle, 1..=6).ok_or_else(|| missing("wl_compositor"))?;
        let wm_base: xdg_wm_base::XdgWmBase =
            bind(&globals, &handle, 1..=6).ok_or_else(|| missing("xdg_wm_base"))?;
        let wl_seat: wl_seat::WlSeat =
            bind(&globals, &handle, 5..=9).ok_or_else(|| missing("wl_seat"))?;
        let viewporter: Option<wp_viewporter::WpViewporter> = bind(&globals, &handle, 1..=1);
        let fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1> =
            bind(&globals, &handle, 1..=1);
        let decorations: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1> =
            bind(&globals, &handle, 1..=1);
        let data_manager: Option<wl_data_device_manager::WlDataDeviceManager> =
            bind(&globals, &handle, 3..=3);
        let primary_manager: Option<
            zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        > = bind(&globals, &handle, 1..=1);

        // **Before the first commit.** A data device created after the first
        // keyboard `enter` never learns the current selection, and the symptom is
        // an empty clipboard with nothing reporting an error — which is exactly
        // how Spike D lost an afternoon.
        let mut clipboard = clipboard::Clipboard::default();
        if let Some(manager) = &data_manager {
            clipboard.device = Some(manager.get_data_device(&wl_seat, &handle, ()));
        }
        if let Some(manager) = &primary_manager {
            clipboard.primary_device = Some(manager.get_device(&wl_seat, &handle, ()));
        }

        let surface = compositor.create_surface(&handle, ());
        // The order below is the invariant, not a style: both objects exist
        // before the first commit, so the first configure already carries the
        // scale. Creating them after it produces one frame at the wrong size,
        // which is the bug Spike A spent a day on.
        let viewport = viewporter.as_ref().map(|it| it.get_viewport(&surface, &handle, ()));
        if let Some(manager) = &fractional {
            manager.get_fractional_scale(&surface, &handle, ());
        }

        let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, ());
        let toplevel = xdg_surface.get_toplevel(&handle, ());
        toplevel.set_title(config.title.clone());
        toplevel.set_app_id(config.app_id.clone());
        // A window that cannot be made smaller than its content is a window that
        // cannot be tiled. 320x240 is small enough to be useless and large
        // enough that the layout code never sees a zero.
        toplevel.set_min_size(320, 240);

        let decoration = decorations.as_ref().map(|manager| {
            let object = manager.get_toplevel_decoration(&toplevel, &handle, ());
            // Ask for a compositor-drawn frame. mutter answers `client_side`
            // and means it, so the answer is recorded and the fallback is ours.
            object.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            object
        });

        surface.commit();

        let capabilities = Capabilities {
            fractional_scale: fractional.is_some(),
            viewporter: viewporter.is_some(),
            server_side_decorations: false,
            keyboard: false,
            pointer: false,
            clipboard: data_manager.is_some(),
            primary_selection: primary_manager.is_some(),
            advertised,
        };

        let mut state = State {
            surface,
            xdg_surface,
            toplevel,
            viewport,
            decoration,
            capabilities,
            seat: Seat::new(&wl_seat, &handle),
            data_manager,
            primary_manager,
            clipboard,
            pending_offer: None,
            pending_types: Vec::new(),
            pending_primary_offer: None,
            pending_primary_types: Vec::new(),
            scale_120: 120,
            fallback_scale: 1,
            logical: config.logical,
            reported: None,
            configured: false,
            close_requested: false,
            events: VecDeque::new(),
        };

        // Two round trips: the first delivers the configure and the scale, the
        // second delivers the seat capabilities and the keymap that the first
        // one's `get_keyboard` asked for.
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(|error| {
                Problem::plain(ProblemKind::NoDisplay, format!("roundtrip: {error}"))
            })?;
        }
        state.events.clear();

        let mut window = Self { connection, queue, state };
        window.apply_geometry();
        Ok(window)
    }

    /// What the compositor supports.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.state.capabilities
    }

    /// The size to lay out and hit-test in.
    #[must_use]
    pub fn logical(&self) -> Size {
        Size::new(self.state.logical.0 as f32, self.state.logical.1 as f32)
    }

    /// The size the renderer's surface must be, in device pixels.
    #[must_use]
    pub fn physical(&self) -> (u32, u32) {
        physical_of(self.state.logical, self.state.scale_120)
    }

    /// The compositor's scale as a numerator over 120.
    #[must_use]
    pub fn scale_120(&self) -> u32 {
        self.state.scale_120
    }

    /// The scale as a factor, for arithmetic that cannot stay in integers.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.state.scale_120 as f32 / 120.0
    }

    /// The two pointers a GPU backend needs in order to create a surface.
    ///
    /// Raw rather than `raw-window-handle` types on purpose: it keeps that crate
    /// inside the GPU stack, where its budget is already accounted for, and it
    /// keeps `sk-plat` free of anything that knows what drawing is. The renderer
    /// converts them; `tools/check-invariants.py` refuses to let the word for
    /// what it converts them into appear in this file at all.
    #[must_use]
    pub fn raw_handles(&self) -> RawHandles {
        RawHandles {
            display: self.connection.backend().display_ptr().cast::<c_void>(),
            surface: self.state.surface.id().as_ptr().cast::<c_void>(),
        }
    }

    /// Whether the compositor or the user has asked the window to close.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        self.state.close_requested
    }

    /// Blocks until something happens, then queues everything that did.
    ///
    /// This is where "zero CPU at idle" lives. The thread sleeps in `poll` on the
    /// display connection and, optionally, on a job pool's eventfd. There is no
    /// timer and no polling interval; `timeout` exists for an animation that has
    /// a next frame, and key repeat shortens it internally.
    ///
    /// The [`Waker`] is taken whole rather than as a descriptor **because it has
    /// to be drained here.** An eventfd that is signalled and not read stays
    /// readable, so `poll` returns immediately, forever — measured at 100% of one
    /// core with an idle window, against a budget row that reads 0.0%. Passing
    /// the descriptor and leaving the drain to the caller is an API where that
    /// bug is one forgotten line away, so the API does not offer it.
    pub fn pump(&mut self, wake: Option<&Waker>, timeout: Option<Duration>) {
        let _ = self.queue.flush();

        // Anything already buffered is dispatched without touching the socket.
        if self.queue.dispatch_pending(&mut self.state).unwrap_or(0) > 0 {
            self.after_dispatch();
            return;
        }

        let timeout = shorter(timeout, self.state.seat.next_repeat_in());
        let Some(guard) = self.queue.prepare_read() else {
            let _ = self.queue.dispatch_pending(&mut self.state);
            self.after_dispatch();
            return;
        };

        let display = self.connection.as_fd();
        let wake_fd = wake.map(Waker::as_fd);
        let mut fds = Vec::with_capacity(2);
        fds.push(PollFd::new(&display, PollFlags::IN));
        if let Some(fd) = &wake_fd {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        let spec = timeout.map(to_timespec);
        let readable = rustix::event::poll(&mut fds, spec.as_ref()).unwrap_or(0);

        if readable > 0 && fds[0].revents().contains(PollFlags::IN) {
            let _ = guard.read();
        } else {
            drop(guard);
        }
        if fds.len() > 1 && fds[1].revents().contains(PollFlags::IN) {
            if let Some(wake) = wake {
                wake.drain();
            }
            self.state.events.push_back(Event::Woken);
        }

        let _ = self.queue.dispatch_pending(&mut self.state);
        self.after_dispatch();
    }

    /// Everything that has happened since the last drain, oldest first.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.state.events.drain(..)
    }

    fn after_dispatch(&mut self) {
        self.state.seat.emit_repeats(&mut self.state.events);
        if self.state.configured {
            self.apply_geometry();
        }
    }

    /// Publishes the negotiated size, and reports it upward exactly once.
    ///
    /// The viewport destination is the logical size and the buffer is the
    /// physical size. Together they make one buffer pixel one output pixel;
    /// either alone does not, which is the crispness gate in
    /// [docs/09-performance-budgets.md](../../../../docs/09-performance-budgets.md).
    fn apply_geometry(&mut self) {
        let state = &mut self.state;
        let current = (state.logical, state.scale_120);
        if state.reported == Some(current) {
            return;
        }
        state.reported = Some(current);

        if let Some(viewport) = &state.viewport {
            viewport.set_destination(state.logical.0.cast_signed(), state.logical.1.cast_signed());
        } else {
            // With no viewporter a fractional scale is not expressible, and the
            // only honest fallback is the integer scale the output reports.
            state.surface.set_buffer_scale(state.fallback_scale.max(1).cast_signed());
        }

        state.events.push_back(Event::Resized {
            logical: Size::new(state.logical.0 as f32, state.logical.1 as f32),
            physical: physical_of(state.logical, state.scale_120),
            scale_120: state.scale_120,
        });
    }
}

/// Pointers to the compositor objects a GPU backend needs.
#[derive(Clone, Copy, Debug)]
pub struct RawHandles {
    /// `*mut wl_display`.
    pub display: *mut c_void,
    /// `*mut wl_surface`.
    pub surface: *mut c_void,
}

fn physical_of(logical: (u32, u32), scale_120: u32) -> (u32, u32) {
    ((logical.0 * scale_120).div_ceil(120).max(1), (logical.1 * scale_120).div_ceil(120).max(1))
}

fn shorter(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (some, None) | (None, some) => some,
    }
}

fn to_timespec(duration: Duration) -> rustix::fs::Timespec {
    rustix::fs::Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

fn bind<I>(
    globals: &GlobalList,
    handle: &QueueHandle<State>,
    version: std::ops::RangeInclusive<u32>,
) -> Option<I>
where
    I: Proxy + 'static,
    State: Dispatch<I, ()>,
{
    globals.bind(handle, version, ()).ok()
}

fn missing(interface: &str) -> Problem {
    Problem::plain(ProblemKind::NoDisplay, format!("compositor does not advertise {interface}"))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A compositor that pings and is not ponged kills the window. One line,
        // and the shortest illustration of what owning the protocol costs.
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                // Zero means "you choose", which is what a compositor sends for
                // a floating window it has no opinion about.
                if width > 0 && height > 0 {
                    state.logical = (width as u32, height as u32);
                }
            }
            xdg_toplevel::Event::Close => state.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scale_120 = scale.max(1);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = event {
            state.fallback_scale = factor.max(1) as u32;
            if !state.capabilities.fractional_scale {
                state.scale_120 = state.fallback_scale * 120;
            }
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            state.capabilities.server_side_decorations =
                matches!(mode, WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide));
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

macro_rules! ignore_events {
    ($($object:path),* $(,)?) => { $(
        impl Dispatch<$object, ()> for State {
            fn event(
                _: &mut Self,
                _: &$object,
                _: <$object as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )* };
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_output::WlOutput,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
);

impl Drop for State {
    fn drop(&mut self) {
        if let Some(decoration) = &self.decoration {
            decoration.destroy();
        }
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        self.toplevel.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{physical_of, shorter};
    use std::time::Duration;

    #[test]
    fn the_physical_size_is_derived_from_the_integer_the_compositor_sent() {
        // 125% of 1100x720. Exact, because the scale never becomes a float.
        assert_eq!(physical_of((1100, 720), 150), (1375, 900));
        // 110%, which is not representable as a ratio of small integers and is
        // exactly why the numerator is kept rather than a factor.
        assert_eq!(physical_of((1100, 720), 132), (1210, 792));
        assert_eq!(physical_of((1100, 720), 120), (1100, 720));
    }

    #[test]
    fn a_surface_is_never_zero_sized() {
        assert_eq!(physical_of((0, 0), 120), (1, 1));
    }

    #[test]
    fn the_shorter_deadline_wins_and_none_means_forever() {
        let short = Some(Duration::from_millis(8));
        let long = Some(Duration::from_millis(500));
        assert_eq!(shorter(short, long), short);
        assert_eq!(shorter(long, short), short);
        assert_eq!(shorter(None, short), short);
        assert_eq!(shorter(None, None), None);
    }
}
