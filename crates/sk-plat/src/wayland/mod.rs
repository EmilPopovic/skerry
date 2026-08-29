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
mod text;

pub use text::TextArea;
use text::TextInput;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsFd;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags};
use sk_model::{Problem, ProblemKind};
use sk_util::Waker;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device_manager, wl_output, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel};

use crate::event::{Event, Point, Size};
use crate::wait::{shorter, to_timespec};
use crate::window::{
    Capabilities, Cursor, Edge, RawHandles, WindowConfig, WindowState, physical_of,
};

use self::seat::Seat;

/// Both directions of drag-and-drop, for one window.
///
/// Grouped the way the clipboard's transport is, and for the same reason: eight
/// fields that are all empty until a drag starts and all empty again after one,
/// scattered through a struct whose other fields last the life of the window.
#[derive(Default)]
struct Drags {
    /// The types of the offer currently arriving, kept apart from the
    /// clipboard's because a drag's `enter` comes *after* them.
    offer_types_in_flight: Vec<String>,
    /// The offer a drag over this window is carrying.
    ///
    /// **Not destroyed on `leave`** — see the comment where that is handled.
    offer: Option<wayland_client::protocol::wl_data_offer::WlDataOffer>,
    /// What that offer holds, in neutral terms.
    over: Option<crate::drag::Drag>,
    /// The serial the `enter` came with, which every `accept` needs.
    serial: u32,
    /// Set between `drop` and the caller finishing with the offer.
    dropped: bool,
    /// The source of a drag *out* of this window, while one is in flight.
    source: Option<wayland_client::protocol::wl_data_source::WlDataSource>,
    /// What that drag is carrying, served on demand after the drop.
    carrying: Option<crate::Content>,
    /// Whether a drag out is still in flight.
    out: bool,
}

/// The neutral actions a `dnd_action` bitfield names.
fn actions_from(actions: wl_data_device_manager::DndAction) -> Vec<crate::drag::DragAction> {
    use crate::drag::DragAction;
    let mut named = Vec::new();
    for (bit, action) in [
        (wl_data_device_manager::DndAction::Copy, DragAction::Copy),
        (wl_data_device_manager::DndAction::Move, DragAction::Move),
        (wl_data_device_manager::DndAction::Ask, DragAction::Ask),
    ] {
        if actions.contains(bit) {
            named.push(action);
        }
    }
    named
}

/// The `dnd_action` bitfield for a set of neutral actions.
///
/// Silently drops the two Wayland cannot express, which is what
/// [`crate::DragAction::on_wayland`] exists to let a caller avoid asking for.
fn actions_of(actions: &[crate::drag::DragAction]) -> wl_data_device_manager::DndAction {
    use crate::drag::DragAction;
    let mut bits = wl_data_device_manager::DndAction::None;
    for action in actions {
        bits |= match action {
            DragAction::Copy => wl_data_device_manager::DndAction::Copy,
            DragAction::Move => wl_data_device_manager::DndAction::Move,
            DragAction::Ask => wl_data_device_manager::DndAction::Ask,
            DragAction::Link | DragAction::Private => wl_data_device_manager::DndAction::None,
        };
    }
    bits
}

/// The protocol shape for a cursor, which is the only Wayland-specific thing
/// about [`Cursor`].
const fn shape_of(cursor: Cursor) -> wp_cursor_shape_device_v1::Shape {
    use wp_cursor_shape_device_v1::Shape;
    match cursor {
        Cursor::Arrow => Shape::Default,
        Cursor::Hand => Shape::Pointer,
        Cursor::Text => Shape::Text,
        Cursor::Grabbing => Shape::Grabbing,
        Cursor::Divider { horizontal: true } => Shape::ColResize,
        Cursor::Divider { horizontal: false } => Shape::RowResize,
        Cursor::Resize(edge) => match edge {
            Edge::Top => Shape::NResize,
            Edge::Bottom => Shape::SResize,
            Edge::Left => Shape::WResize,
            Edge::Right => Shape::EResize,
            Edge::TopLeft => Shape::NwResize,
            Edge::TopRight => Shape::NeResize,
            Edge::BottomLeft => Shape::SwResize,
            Edge::BottomRight => Shape::SeResize,
        },
    }
}

/// The protocol shape for a resize edge.
const fn resize_edge_of(edge: Edge) -> xdg_toplevel::ResizeEdge {
    match edge {
        Edge::Top => xdg_toplevel::ResizeEdge::Top,
        Edge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
        Edge::Left => xdg_toplevel::ResizeEdge::Left,
        Edge::Right => xdg_toplevel::ResizeEdge::Right,
        Edge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        Edge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
        Edge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
        Edge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
    }
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
    clipboard: clipboard::Selections,
    /// The compositor's cursor namer, where it has one.
    ///
    /// `wp_cursor_shape_v1` is staging and recent — mutter 45, kwin 5.27, sway
    /// 1.9. Without it the pointer keeps whatever image the client it came from
    /// left on it, which the protocol says is undefined and which in practice
    /// means an I-beam from a text field sitting over the file list. Recorded as
    /// a capability rather than worked around: the fallback is an `XCursor` theme
    /// parser and a shm buffer, and that is a library.
    cursor: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    /// The input method's end of the conversation, where the compositor offers
    /// one.
    ///
    /// **Enabled only while a field has the keyboard.** The protocol says an
    /// enabled text input is a request for an input method to be shown, so a
    /// client that enabled it at start-up would put a Japanese candidate window
    /// over a file list nobody is typing into.
    text_input: Option<TextInput>,
    /// The compositor's touchpad gesture manager, where it has one.
    ///
    /// Held rather than consumed at open, because a seat can gain a pointer
    /// after the window is up — a docked laptop, a mouse plugged in — and the
    /// swipe and pinch objects are made from the pointer.
    gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    /// What the pointer was last told to be, so it is told only on a change.
    shape: Cursor,
    /// Selection writes the peer has not finished reading.
    ///
    /// A pipe holds 64 KiB and a whole-directory selection is megabytes, so the
    /// tail of a transfer waits here for room rather than blocking the loop.
    transfers: Vec<crate::clipboard::Transfer>,
    /// An offer whose types are still arriving, before the `selection` event
    /// that adopts it.
    pending_offer: Option<wayland_client::protocol::wl_data_offer::WlDataOffer>,
    pending_types: Vec<String>,
    pending_primary_offer: Option<
        wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    >,
    pending_primary_types: Vec<String>,
    /// Both directions of drag-and-drop.
    drags: Drags,

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
    /// What the compositor said about the window, accumulated across one
    /// configure and published on the `xdg_surface.configure` that ends it.
    shell: WindowState,
    /// The states of the configure currently being received.
    incoming: WindowState,
    close_requested: bool,
    /// Whether the frame is drawn here by request rather than by necessity.
    own_frame: bool,
    /// Why the connection died, if it did.
    ///
    /// A protocol error is not recoverable: every later dispatch fails the same
    /// way. Without recording it the frame loop spun on a dead socket, printing
    /// the library's complaint on every pass and never exiting — measured under a
    /// nested mutter whose GL renderer refused the swapchain's dmabuf, where the
    /// program burned a core until it was killed.
    fatal: Option<String>,
    events: VecDeque<Event>,
}

impl State {
    /// A surface-local fixed-point position, in logical pixels.
    ///
    /// `wl_data_device` sends `wl_fixed`, which is already in surface
    /// coordinates — the same space everything above this crate works in, so
    /// this is a conversion of units and not of spaces.
    #[expect(clippy::unused_self, reason = "a method so both call sites read the same")]
    fn logical_point(&self, x: f64, y: f64) -> Point {
        Point::new(x as f32, y as f32)
    }
}

/// Every global this window asks the compositor for, bound once.
///
/// A struct rather than eleven `let`s in `open`, because the *version ranges*
/// are the interesting part and they belong together: each is a pair of numbers
/// this program has been tested against, and a compositor advertising something
/// newer is bound at the version we know.
struct Globals {
    compositor: wl_compositor::WlCompositor,
    wm_base: xdg_wm_base::XdgWmBase,
    wl_seat: wl_seat::WlSeat,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    decorations: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    primary_manager:
        Option<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
    dmabuf: Option<(
        zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    )>,
    cursor_shapes: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    text_inputs: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
}

impl Globals {
    fn bind(globals: &GlobalList, handle: &QueueHandle<State>) -> Result<Self, Problem> {
        Ok(Self {
            compositor: bind(globals, handle, 1..=6).ok_or_else(|| missing("wl_compositor"))?,
            wm_base: bind(globals, handle, 1..=6).ok_or_else(|| missing("xdg_wm_base"))?,
            wl_seat: bind(globals, handle, 5..=9).ok_or_else(|| missing("wl_seat"))?,
            viewporter: bind(globals, handle, 1..=1),
            fractional: bind(globals, handle, 1..=1),
            decorations: bind(globals, handle, 1..=1),
            data_manager: bind(globals, handle, 3..=3),
            primary_manager: bind(globals, handle, 1..=1),
            dmabuf: ask_which_gpu(globals, handle),
            cursor_shapes: bind(globals, handle, 1..=2),
            gestures: bind(globals, handle, 1..=3),
            text_inputs: bind(globals, handle, 1..=1),
        })
    }
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

        let Globals {
            compositor,
            wm_base,
            wl_seat,
            viewporter,
            fractional,
            decorations,
            data_manager,
            primary_manager,
            dmabuf,
            cursor_shapes,
            gestures,
            text_inputs,
        } = Globals::bind(&globals, &handle)?;
        let clipboard =
            clipboard::open(&handle, &wl_seat, data_manager.as_ref(), primary_manager.as_ref());
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
        let decoration = dress(&toplevel, &handle, decorations.as_ref(), config);
        surface.commit();

        let capabilities = Capabilities {
            fractional_scale: fractional.is_some(),
            viewporter: viewporter.is_some(),
            server_side_decorations: false,
            keyboard: false,
            pointer: false,
            cursor_shape: cursor_shapes.is_some(),
            gestures: gestures.is_some(),
            input_method: text_inputs.is_some(),
            clipboard: data_manager.is_some(),
            primary_selection: primary_manager.is_some(),
            render_device: None,
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
            cursor: None,
            text_input: text_inputs.map(|manager| TextInput::new(&manager, &wl_seat, &handle)),
            gestures,
            shape: Cursor::Arrow,
            transfers: Vec::new(),
            pending_offer: None,
            pending_types: Vec::new(),
            pending_primary_offer: None,
            pending_primary_types: Vec::new(),
            drags: Drags::default(),
            scale_120: 120,
            fallback_scale: 1,
            logical: config.logical,
            reported: None,
            configured: false,
            shell: WindowState::before_any_configure(),
            incoming: WindowState::before_any_configure(),
            close_requested: false,
            own_frame: config.own_frame,
            fatal: None,
            events: VecDeque::new(),
        };

        // Two round trips: the first delivers the configure and the scale, the
        // second delivers the seat capabilities and the keymap that the first
        // one's `get_keyboard` asked for. The cursor device is made after them,
        // because it is made from the `wl_pointer` and there is no pointer until
        // the seat has said there is one.
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(|error| {
                Problem::plain(ProblemKind::NoDisplay, format!("roundtrip: {error}"))
            })?;
        }
        probe_scale(&globals, &handle, &mut queue, &mut state);
        state.events.clear();
        state.cursor = dress_pointer(cursor_shapes, &state.seat, &handle);

        forget_which_gpu(dmabuf);

        let mut window = Self { connection, queue, state };
        window.apply_geometry();
        Ok(window)
    }

    /// What the compositor supports.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.state.capabilities
    }

    /// What the compositor last said about the window.
    #[must_use]
    pub fn state(&self) -> WindowState {
        self.state.shell
    }

    /// Whether the window has to draw its own frame.
    ///
    /// True on GNOME, which does not advertise `zxdg_decoration_manager_v1` at
    /// all — so on the most common Linux desktop a client that does not draw one
    /// has no title bar, no close button and no resize edges.
    #[must_use]
    pub fn needs_own_frame(&self) -> bool {
        self.state.own_frame || !self.state.capabilities.server_side_decorations
    }

    /// Starts an interactive move, as if the title bar had been grabbed.
    ///
    /// The serial has to come from a real button press: it is the protocol's way
    /// of saying a window cannot start dragging itself.
    /// Returns whether the compositor was asked — a press with no serial behind
    /// it is not a grab the protocol will accept, and the caller has to do
    /// something else with it rather than swallow it.
    #[must_use = "a refused grab is a press the caller still has to account for"]
    pub fn start_move(&self) -> bool {
        let Some(serial) = self.state.seat.button_serial else { return false };
        self.state.toplevel._move(self.state.seat.seat(), serial);
        true
    }

    /// Starts an interactive resize from `edge`. Returns whether it was asked.
    #[must_use = "a refused grab is a press the caller still has to account for"]
    pub fn start_resize(&self, edge: Edge) -> bool {
        let Some(serial) = self.state.seat.button_serial else { return false };
        self.state.toplevel.resize(self.state.seat.seat(), serial, resize_edge_of(edge));
        true
    }

    /// Maximizes or restores.
    pub fn set_maximized(&self, yes: bool) {
        if yes {
            self.state.toplevel.set_maximized();
        } else {
            self.state.toplevel.unset_maximized();
        }
    }

    /// Minimizes, if the compositor has such a thing.
    pub fn minimize(&self) {
        if self.state.shell.can_minimize {
            self.state.toplevel.set_minimized();
        }
    }

    /// Opens the compositor's own window menu at a point in the window.
    pub fn show_window_menu(&self, at: Point) {
        let Some(serial) = self.state.seat.button_serial else { return };
        if !self.state.shell.can_show_menu {
            return;
        }
        self.state.toplevel.show_window_menu(
            self.state.seat.seat(),
            serial,
            at.x as i32,
            at.y as i32,
        );
    }

    /// Sets the pointer's image, if the compositor names shapes.
    ///
    /// Only on a change and only with a real `wl_pointer.enter` serial, which is
    /// the protocol's rule: a client may dress the pointer while it is over its
    /// own surface and not otherwise.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        if self.state.shape == cursor {
            return;
        }
        let Some(device) = &self.state.cursor else { return };
        let Some(serial) = self.state.seat.enter_serial else { return };
        device.set_shape(serial, shape_of(cursor));
        self.state.shape = cursor;
    }

    /// Tells the input method whether a field has the keyboard, and where.
    ///
    /// `None` disables it, which is what takes a candidate window off the
    /// screen. Called every frame with the same answer, it sends nothing.
    pub fn set_text_area(&mut self, area: Option<TextArea>) {
        if let Some(input) = self.state.text_input.as_mut() {
            input.focus(area);
        }
    }

    /// Asks the window to close, as the title bar's button does.
    pub fn request_close(&mut self) {
        self.state.close_requested = true;
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
        RawHandles::Wayland {
            display: self.connection.backend().display_ptr().cast::<c_void>(),
            surface: self.state.surface.id().as_ptr().cast::<c_void>(),
        }
    }

    /// Whether the window should stop: the user asked, or the connection died.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        self.state.close_requested || self.state.fatal.is_some()
    }

    /// Why the connection died, if it did.
    ///
    /// `None` for an ordinary close. A caller that ignores this exits silently on
    /// a compositor error, which is the difference between "you closed it" and
    /// "it vanished" — and only one of those is worth telling somebody about.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.state.fatal.as_deref()
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
        if self.state.fatal.is_some() {
            return;
        }
        if let Err(error) = self.queue.flush() {
            self.state.fatal = Some(format!("flush: {error}"));
            return;
        }

        // A transfer whose pipe has room since last time finishes here, before
        // anything decides to sleep.
        crate::clipboard::resume_all(&mut self.state.transfers);

        // Anything already buffered is dispatched without touching the socket.
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(dispatched) if dispatched > 0 => {
                self.after_dispatch();
                return;
            }
            Ok(_) => {}
            Err(error) => {
                self.state.fatal = Some(format!("dispatch: {error}"));
                return;
            }
        }

        let timeout = shorter(timeout, self.state.seat.next_repeat_in());
        let Some(guard) = self.queue.prepare_read() else {
            if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
                self.state.fatal = Some(format!("dispatch: {error}"));
                return;
            }
            self.after_dispatch();
            return;
        };

        let display = self.connection.as_fd();
        let wake_fd = wake.map(Waker::as_fd);
        let mut fds = Vec::with_capacity(2 + self.state.transfers.len());
        fds.push(PollFd::new(&display, PollFlags::IN));
        if let Some(fd) = &wake_fd {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        // An unfinished selection write waits for room in its pipe the same way
        // everything else here waits: in the one `poll`. No thread, no timer, and
        // idle stays idle because a full pipe is simply not ready.
        let waiting = fds.len();
        let pipes: Vec<_> =
            self.state.transfers.iter().map(crate::clipboard::Transfer::as_fd).collect();
        for pipe in &pipes {
            fds.push(PollFd::new(pipe, PollFlags::OUT));
        }
        let spec = timeout.map(to_timespec);
        let readable = rustix::event::poll(&mut fds, spec.as_ref()).unwrap_or(0);

        if readable > 0 && fds[0].revents().contains(PollFlags::IN) {
            match guard.read() {
                Ok(_) => {}
                // Another reader took the events first. There is no other reader
                // here, and it is not an error in any case.
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    self.state.fatal = Some(format!("read: {error}"));
                    return;
                }
            }
        } else {
            drop(guard);
        }
        if wake_fd.is_some() && fds[1].revents().contains(PollFlags::IN) {
            if let Some(wake) = wake {
                wake.drain();
            }
            self.state.events.push_back(Event::Woken);
        }
        if fds[waiting..].iter().any(|fd| !fd.revents().is_empty()) {
            crate::clipboard::resume_all(&mut self.state.transfers);
        }

        if let Err(error) = self.queue.dispatch_pending(&mut self.state) {
            self.state.fatal = Some(format!("dispatch: {error}"));
            return;
        }
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

/// Names the window and asks for a frame, before the first commit.
fn dress(
    toplevel: &xdg_toplevel::XdgToplevel,
    handle: &QueueHandle<State>,
    decorations: Option<&zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    config: &WindowConfig,
) -> Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1> {
    toplevel.set_title(config.title.clone());
    toplevel.set_app_id(config.app_id.clone());
    // A window that cannot be made smaller than its content is a window that
    // cannot be tiled. 320x240 is small enough to be useless and large enough
    // that the layout code never sees a zero.
    toplevel.set_min_size(320, 240);

    let object = decorations?.get_toplevel_decoration(toplevel, handle, ());
    // A request, not an instruction: mutter answers `client_side` and means it,
    // Hyprland answers `server_side` whatever it is asked. So the answer is
    // recorded rather than assumed and the fallback is ours either way.
    object.set_mode(if config.own_frame {
        zxdg_toplevel_decoration_v1::Mode::ClientSide
    } else {
        zxdg_toplevel_decoration_v1::Mode::ServerSide
    });
    Some(object)
}

/// Makes the cursor-shape device, if there is a manager and a pointer.
///
/// After the seat's round trips, because the device is made from the
/// `wl_pointer` and there is no pointer until the seat has said there is one.
/// The manager is let go straight after: it exists to make this one object.
fn dress_pointer(
    manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    seat: &Seat,
    handle: &QueueHandle<State>,
) -> Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1> {
    let manager = manager?;
    let device = seat.pointer().map(|pointer| manager.get_pointer(pointer, handle, ()));
    manager.destroy();
    device
}

/// Asks the compositor which GPU it renders on, and nothing else.
///
/// `zwp_linux_dmabuf_v1` version 4 is where `get_default_feedback` and
/// `main_device` arrive. Nothing else in this program uses the protocol: it is
/// bound, asked one question, and destroyed as soon as the answer lands.
type WhichGpu = Option<(
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
)>;

fn ask_which_gpu(globals: &GlobalList, handle: &QueueHandle<State>) -> WhichGpu {
    let dmabuf: zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 = bind(globals, handle, 4..=5)?;
    let feedback = dmabuf.get_default_feedback(handle, ());
    Some((dmabuf, feedback))
}

/// Lets it go. Keeping the feedback would make the compositor send a fresh
/// tranche every time it changed its mind about scanout, and nothing here can
/// act on that.
fn forget_which_gpu(asked: WhichGpu) {
    if let Some((dmabuf, feedback)) = asked {
        feedback.destroy();
        dmabuf.destroy();
    }
}

/// A `dev_t` split into its major and minor numbers.
///
/// glibc's packing, not the kernel's sixteen-bit one: the major is split across
/// bits 8–19 and 32–63, and the minor across 0–7 and 20–31. Written out rather
/// than taken from `libc::major`, which is a macro in C and an inline function
/// with no stable Rust binding.
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
            // The `xdg_surface.configure` is what ends a configure sequence, so
            // this is the moment the accumulated states become true.
            state.shell = state.incoming;
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
            xdg_toplevel::Event::Configure { width, height, states } => {
                // Zero means "you choose", which is what a compositor sends for
                // a floating window it has no opinion about.
                if width > 0 && height > 0 {
                    state.logical = (width as u32, height as u32);
                }
                // The states are a fresh, complete list every configure: absent
                // means false, so they are collected rather than merged. The
                // capabilities are **not** part of this — `wm_capabilities` is
                // its own event with its own cadence — so they are carried over.
                state.incoming = WindowState {
                    can_maximize: state.incoming.can_maximize,
                    can_minimize: state.incoming.can_minimize,
                    can_show_menu: state.incoming.can_show_menu,
                    ..WindowState::default()
                };
                for value in states.chunks_exact(4) {
                    let raw = u32::from_ne_bytes([value[0], value[1], value[2], value[3]]);
                    match xdg_toplevel::State::try_from(raw) {
                        Ok(xdg_toplevel::State::Maximized) => state.incoming.maximized = true,
                        Ok(xdg_toplevel::State::Fullscreen) => state.incoming.fullscreen = true,
                        Ok(xdg_toplevel::State::Activated) => state.incoming.activated = true,
                        Ok(
                            xdg_toplevel::State::TiledLeft
                            | xdg_toplevel::State::TiledRight
                            | xdg_toplevel::State::TiledTop
                            | xdg_toplevel::State::TiledBottom,
                        ) => state.incoming.tiled = true,
                        _ => {}
                    }
                }
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let (mut maximize, mut minimize, mut menu) = (false, false, false);
                for value in capabilities.chunks_exact(4) {
                    let raw = u32::from_ne_bytes([value[0], value[1], value[2], value[3]]);
                    match xdg_toplevel::WmCapabilities::try_from(raw) {
                        Ok(xdg_toplevel::WmCapabilities::Maximize) => maximize = true,
                        Ok(xdg_toplevel::WmCapabilities::Minimize) => minimize = true,
                        Ok(xdg_toplevel::WmCapabilities::WindowMenu) => menu = true,
                        _ => {}
                    }
                }
                // Into `incoming` only, like every other configure input: the
                // `xdg_surface.configure` that ends the sequence is what makes
                // any of it true. Writing `shell` here as well let a compositor
                // add or remove a title-bar button before the size it belongs
                // with had been applied.
                state.incoming.can_maximize = maximize;
                state.incoming.can_minimize = minimize;
                state.incoming.can_show_menu = menu;
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

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `main_device` is a `dev_t` in the sender's byte order, per the protocol.
        if let zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } = event
            && let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice())
        {
            state.capabilities.render_device = Some(u64::from_ne_bytes(bytes));
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

/// Maps the surface with a one-pixel buffer, so the compositor says what scale it
/// is at before the first real frame is drawn.
///
/// **`wp_fractional_scale_v1` sends `preferred_scale` when the surface enters an
/// output, and a surface enters an output when it is mapped** — which happens on
/// the first commit that carries a buffer. So a client that waits for the scale
/// before drawing waits forever, and one that draws first draws a frame at 100%
/// and corrects it on the next: measured on Hyprland at an output scale of 1.25,
/// the first frame went out at 1.0.
///
/// One pixel of transparent `ARGB8888` is enough to map it. The memory is never
/// written and never mapped into this process: a fresh `memfd` reads as zeros,
/// which is the colour wanted.
///
/// Best effort throughout. A compositor with no `wl_shm`, or one that refuses the
/// pool, leaves the scale where it was and the second frame corrects it — which
/// is the behaviour this replaces, not a regression on it.
fn probe_scale(
    globals: &GlobalList,
    handle: &QueueHandle<State>,
    queue: &mut EventQueue<State>,
    state: &mut State,
) {
    if !state.capabilities.fractional_scale {
        return;
    }
    let Some(shm): Option<wl_shm::WlShm> = bind(globals, handle, 1..=2) else { return };
    let Ok(memory) =
        rustix::fs::memfd_create("skerry-scale-probe", rustix::fs::MemfdFlags::CLOEXEC)
    else {
        return;
    };
    // One pixel, four bytes.
    if rustix::fs::ftruncate(&memory, 4).is_err() {
        return;
    }
    let pool = shm.create_pool(memory.as_fd(), 4, handle, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, handle, ());
    state.surface.attach(Some(&buffer), 0, 0);
    state.surface.damage_buffer(0, 0, 1, 1);
    state.surface.commit();

    // Two round trips rather than a wait: the scale arrives on the first one that
    // follows the map, and a client that blocks until it does is a client that
    // never starts on a compositor that does not send it.
    for _ in 0..2 {
        if queue.roundtrip(state).is_err() {
            break;
        }
        if state.reported.is_some() {
            break;
        }
    }
    buffer.destroy();
    pool.destroy();
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
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
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
