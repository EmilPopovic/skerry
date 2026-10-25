// SPDX-License-Identifier: Apache-2.0
//! The X11 window: `x11rb` and our own policy above it.
//!
//! [ADR 0009](../../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)
//! is why this exists beside `wayland` rather than instead of it, and why the
//! choice between them is made at run time.
//!
//! Three things here are decisions rather than protocol, and each is the kind
//! that is invisible when it is right:
//!
//! * **X11 speaks device pixels and everything above `sk-plat` speaks logical
//!   ones.** The conversion happens here and nowhere else, in exactly the two
//!   places Wayland's does — once for the window size and once for the pointer.
//!   Wayland is handed logical sizes and derives physical; X11 is handed
//!   physical and derives logical. The two are mirror images and both feed the
//!   same [`Event::Resized`].
//! * **The scale comes from `Xft.dpi` and nothing else.** RANDR's physical size
//!   is available and is not consulted; the ADR argues that at length.
//! * **Auto-repeat is turned off in the server.** X11's own repeat arrives as a
//!   release-then-press pair indistinguishable from fast typing, so XKB's
//!   detectable auto-repeat is asked for and the repeat is generated here — by
//!   the same [`crate::input::Repeater`] Wayland uses, so a held key behaves
//!   identically on both.

mod atoms;
mod cursor;
mod keyboard;
mod selection;
mod xdnd;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use sk_model::{Problem, ProblemKind};
use sk_util::Waker;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event as X11Event;
use x11rb::protocol::xfixes;
use x11rb::protocol::xkb as xkbext;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

use crate::clipboard::{Clipboard, Content, Selection, Transfer, resume_all};
use crate::event::{Button, Event, Point, Size};
use crate::input::{Clicks, Repeater, WHEEL_NOTCH};
use crate::wait::{shorter, to_timespec};
use crate::window::{
    Capabilities, Cursor, Edge, RawHandles, WindowConfig, WindowState, physical_of,
};

use self::atoms::Atoms;

/// Everything a selection request needs, in one value.
///
/// A macro rather than a method: a method borrows the whole of `self`, and every
/// caller here also needs `self.selections` mutably. Expanded in place, the
/// borrow checker sees five disjoint fields, which is what they are.
macro_rules! ask {
    ($self:ident) => {
        selection::Ask {
            connection: &$self.connection,
            atoms: &$self.atoms,
            types: $self.types,
            window: $self.id,
            time: $self.time,
        }
    };
}

/// The smallest window a layout is guaranteed to survive, in logical pixels.
///
/// The same pair Wayland's `set_min_size` uses: small enough to be useless and
/// large enough that the layout code never sees a zero.
const MIN_LOGICAL: (u32, u32) = (320, 240);

/// What `Xft.dpi` is when nobody has said, and the divisor that turns a DPI into
/// a scale. 96 is the number X11 has meant by "100%" since before it was a
/// question anyone asked.
const BASE_DPI: f32 = 96.0;

/// `_NET_WM_STATE` actions, from the EWMH.
const STATE_REMOVE: u32 = 0;
const STATE_ADD: u32 = 1;

/// `WM_CHANGE_STATE`'s `IconicState`, from ICCCM section 4.1.4. Minimizing a
/// window on X11 is telling it to become an icon, which is what 1989 called it.
const ICONIC_STATE: u32 = 3;

/// `_NET_WM_MOVERESIZE` directions, in the EWMH's order: the eight edges
/// starting at the top left and going clockwise, then `MOVE`.
const MOVERESIZE_MOVE: u32 = 8;

/// An X11 window and everything derived from it.
pub struct Window {
    connection: XCBConnection,
    screen: usize,
    root: xproto::Window,
    id: xproto::Window,
    atoms: Atoms,
    types: selection::Types,
    /// The visual the window was created with, which the Vulkan surface needs
    /// to agree with.
    visual: xproto::Visualid,

    capabilities: Capabilities,
    keyboard: Option<keyboard::Keyboard>,
    repeat: Repeater,
    clicks: Clicks,
    cursors: cursor::Cursors,
    shape: Cursor,

    selections: selection::Selections,
    xdnd: xdnd::Xdnd,
    clipboard: Clipboard,
    transfers: Vec<Transfer>,

    /// The scale as a numerator over 120, the same representation Wayland's
    /// `wp_fractional_scale_v1` uses — so nothing above this crate learns that
    /// one server states it and the other is asked.
    scale_120: u32,
    /// Fixed by `--scale`, in which case `Xft.dpi` is not consulted.
    scale_forced: bool,
    /// What the server says the window is, in device pixels. X11's is the
    /// authoritative size and the logical one is derived, which is the opposite
    /// of Wayland.
    physical: (u32, u32),
    logical: (u32, u32),
    reported: Option<((u32, u32), u32)>,

    shell: WindowState,
    /// Where the pointer is, in logical pixels.
    position: Point,
    /// The last timestamp the server sent, which every selection request needs.
    ///
    /// `CurrentTime` is legal and is a race: it means "whenever this arrives",
    /// so two clients asking at once can both believe they own the selection.
    time: xproto::Timestamp,
    close_requested: bool,
    own_frame: bool,
    /// No window manager is running, so nothing else will draw a frame.
    unmanaged: bool,
    fatal: Option<String>,
    events: VecDeque<Event>,
}

impl Window {
    /// Connects to the X server and maps a window.
    pub fn open(config: &WindowConfig) -> Result<Self, Problem> {
        let (connection, screen) = XCBConnection::connect(None).map_err(|error| {
            Problem::plain(ProblemKind::NoDisplay, format!("X11 connect: {error}"))
        })?;
        let setup = connection.setup();
        let root_screen = setup
            .roots
            .get(screen)
            .ok_or_else(|| Problem::plain(ProblemKind::NoDisplay, "no such X screen"))?;
        let root = root_screen.root;
        let visual = root_screen.root_visual;
        let depth = root_screen.root_depth;
        let black = root_screen.black_pixel;

        let atoms = Atoms::new(&connection)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .ok_or_else(|| Problem::plain(ProblemKind::NoDisplay, "could not intern X11 atoms"))?;

        let mut capabilities = probe(&connection, root, &atoms);
        let scale_120 = config
            .scale_120
            .unwrap_or_else(|| scale_from_resources(&connection, root, atoms.RESOURCE_MANAGER));
        let physical = physical_of(config.logical, scale_120);

        let window = create_window(&connection, root, visual, depth, black, physical)?;

        let mut window = Self {
            connection,
            screen,
            root,
            id: window,
            types: selection::Types::new(&atoms),
            atoms,
            visual,
            keyboard: None,
            repeat: Repeater::default(),
            clicks: Clicks::default(),
            cursors: cursor::Cursors::default(),
            shape: Cursor::Arrow,
            selections: selection::Selections::default(),
            xdnd: xdnd::Xdnd::default(),
            clipboard: Clipboard::default(),
            transfers: Vec::new(),
            scale_120,
            scale_forced: config.scale_120.is_some(),
            physical,
            logical: config.logical,
            reported: None,
            shell: WindowState::before_any_configure(),
            position: Point::default(),
            time: x11rb::CURRENT_TIME,
            close_requested: false,
            own_frame: config.own_frame,
            unmanaged: !capabilities.server_side_decorations,
            fatal: None,
            events: VecDeque::new(),
            capabilities: Capabilities::default(),
        };
        window.shell.can_show_menu = false;
        window.dress(config);
        window.keyboard =
            keyboard::Keyboard::new(&window.connection, window.root, window.atoms._XKB_RULES_NAMES);
        capabilities.keyboard = window.keyboard.is_some();
        window.cursors = cursor::Cursors::open(&window.connection, window.screen, scale_120);
        capabilities.cursor_shape = window.cursors.themed();
        window.capabilities = capabilities;

        xdnd::Xdnd::advertise(&window.connection, &window.atoms, window.id);
        window.detectable_auto_repeat();
        window.watch_resources();
        let watching = selection::Selections::watch(&window.connection, &window.atoms, window.id);
        window.capabilities.clipboard = watching;
        window.capabilities.primary_selection = watching;
        // What is already on the clipboard, before anybody touches it: without
        // this, Paste is dimmed until the *next* time somebody copies.
        window.ask_targets(Selection::Clipboard);
        window.apply_cursor();

        // Mapped last, so every property a window manager reads at map time is
        // already there. A title set afterwards is a title some window managers
        // never read.
        if let Err(error) = window.connection.map_window(window.id) {
            return Err(Problem::plain(ProblemKind::NoDisplay, format!("map: {error}")));
        }
        if let Err(error) = window.connection.flush() {
            return Err(Problem::plain(ProblemKind::NoDisplay, format!("flush: {error}")));
        }
        window.take_focus_if_unmanaged();
        window.publish_geometry();
        Ok(window)
    }

    /// Focuses the window where nothing else will.
    ///
    /// X11's default focus is `PointerRoot`: keystrokes go to whatever window
    /// the pointer happens to be over. A window manager fixes that by focusing
    /// what it maps; with none running — a bare `Xvfb`, which is what the
    /// scenarios use — nothing does, and the window is one the keyboard reaches
    /// only while the pointer sits on it.
    ///
    /// **Only when unmanaged.** A client that takes focus from under a running
    /// window manager is a client that steals it, which is the behaviour every
    /// focus-stealing-prevention feature exists to stop.
    fn take_focus_if_unmanaged(&mut self) {
        if !self.unmanaged {
            return;
        }
        let _ = self.connection.set_input_focus(
            xproto::InputFocus::POINTER_ROOT,
            self.id,
            x11rb::CURRENT_TIME,
        );
        let _ = self.connection.flush();
    }

    /// Names the window, claims its protocols, and asks about the frame.
    ///
    /// Every one of these is a property a window manager reads once, at map
    /// time. Failures are dropped rather than propagated: a server that refuses
    /// to store `_NET_WM_PID` still has a usable window, and the alternative is
    /// refusing to start over a hint.
    fn dress(&mut self, config: &WindowConfig) {
        let connection = &self.connection;
        let window = self.id;
        let atoms = &self.atoms;

        let _ = connection.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms.WM_PROTOCOLS,
            xproto::AtomEnum::ATOM,
            &[atoms.WM_DELETE_WINDOW],
        );
        let _ = connection.change_property8(
            xproto::PropMode::REPLACE,
            window,
            atoms._NET_WM_NAME,
            atoms.UTF8_STRING,
            config.title.as_bytes(),
        );
        let _ = connection.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::Atom::from(xproto::AtomEnum::WM_NAME),
            xproto::Atom::from(xproto::AtomEnum::STRING),
            config.title.as_bytes(),
        );
        // ICCCM 4.1.2.5: instance then class, each NUL-terminated. The instance
        // is what `xprop` shows first and what a window manager's per-window
        // rules match on, so it is the binary's name and not the reverse-DNS id.
        let class = wm_class(&config.app_id);
        let _ = connection.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::Atom::from(xproto::AtomEnum::WM_CLASS),
            xproto::Atom::from(xproto::AtomEnum::STRING),
            &class,
        );
        let _ = connection.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms._NET_WM_PID,
            xproto::AtomEnum::CARDINAL,
            &[std::process::id()],
        );
        // ICCCM 4.1.2.9: the machine the client runs on, so a window manager
        // can say where a remote window came from. `rustix` is already here;
        // taking a crate for `gethostname(2)` would be a row in the budget for
        // one syscall.
        let host = rustix::system::uname();
        let _ = connection.change_property8(
            xproto::PropMode::REPLACE,
            window,
            atoms.WM_CLIENT_MACHINE,
            xproto::Atom::from(xproto::AtomEnum::STRING),
            host.nodename().to_bytes(),
        );
        let _ = connection.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms._NET_WM_WINDOW_TYPE,
            xproto::AtomEnum::ATOM,
            &[atoms._NET_WM_WINDOW_TYPE_NORMAL],
        );

        let minimum = physical_of(MIN_LOGICAL, self.scale_120);
        let hints = x11rb::properties::WmSizeHints {
            min_size: Some((
                minimum.0.try_into().unwrap_or(i32::MAX),
                minimum.1.try_into().unwrap_or(i32::MAX),
            )),
            ..x11rb::properties::WmSizeHints::default()
        };
        let _ = hints.set_normal_hints(connection, window);

        if config.own_frame {
            // Undocumented by any standard, honoured by every reparenting window
            // manager, and the whole of X11 decoration control. Five CARDINALs:
            // flags bit 1 means "read the decorations field", which is zero.
            const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
            let _ = connection.change_property32(
                xproto::PropMode::REPLACE,
                window,
                atoms._MOTIF_WM_HINTS,
                atoms._MOTIF_WM_HINTS,
                &[MWM_HINTS_DECORATIONS, 0, 0, 0, 0],
            );
        }
    }

    /// Asks XKB to stop synthesising a release before every repeat.
    ///
    /// Without this a held key arrives as press, release, press, release — which
    /// is what a person typing very fast also looks like, and a text field that
    /// cannot tell them apart cannot coalesce a run of typing into one undo.
    fn detectable_auto_repeat(&mut self) {
        let connection = &self.connection;
        if xkbext::use_extension(connection, 1, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_none()
        {
            return;
        }
        let _ = xkbext::per_client_flags(
            connection,
            xkbext::ID::USE_CORE_KBD.into(),
            xkbext::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            xkbext::PerClientFlag::DETECTABLE_AUTO_REPEAT,
            xkbext::BoolCtrl::from(0u32),
            xkbext::BoolCtrl::from(0u32),
            xkbext::BoolCtrl::from(0u32),
        );
        if let Some(controls) = xkbext::get_controls(connection, xkbext::ID::USE_CORE_KBD.into())
            .ok()
            .and_then(|c| c.reply().ok())
        {
            // The server states a delay in milliseconds and an interval in
            // milliseconds; the shared repeater takes a rate, because that is
            // what Wayland states. Zero interval means the server is not
            // repeating and neither should we.
            let rate = if controls.repeat_delay > 0 && controls.repeat_interval > 0 {
                1000 / i32::from(controls.repeat_interval)
            } else {
                0
            };
            self.repeat.set_rate(controls.repeat_delay.into(), rate);
        }
    }

    /// Watches the root window for the resource database changing.
    ///
    /// This is how a scale change reaches a running window on X11: a settings
    /// daemon rewrites `RESOURCE_MANAGER` and every client re-reads `Xft.dpi`.
    /// Selecting for property changes on the root is what every toolkit does and
    /// it is the only notification there is.
    fn watch_resources(&mut self) {
        let aux =
            xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE);
        let _ = self.connection.change_window_attributes(self.root, &aux);
    }

    /// What this X server turned out to support.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// What the window manager last said about the window.
    #[must_use]
    pub fn state(&self) -> WindowState {
        self.shell
    }

    /// Whether the frame has to be drawn here.
    ///
    /// True when it was asked for, and true when no window manager is running at
    /// all — under a bare `Xvfb` nothing else will draw one, and a window with no
    /// frame and no way to close it is not testable.
    #[must_use]
    pub fn needs_own_frame(&self) -> bool {
        self.own_frame || self.unmanaged
    }

    /// Asks the window manager to move the window.
    pub fn start_move(&self) -> bool {
        self.moveresize(MOVERESIZE_MOVE)
    }

    /// Asks the window manager to resize the window from `edge`.
    pub fn start_resize(&self, edge: Edge) -> bool {
        // EWMH `_NET_WM_MOVERESIZE`: 0 is the top left corner and the rest go
        // clockwise, so the order below is the specification's and not a choice.
        let direction = match edge {
            Edge::TopLeft => 0,
            Edge::Top => 1,
            Edge::TopRight => 2,
            Edge::Right => 3,
            Edge::BottomRight => 4,
            Edge::Bottom => 5,
            Edge::BottomLeft => 6,
            Edge::Left => 7,
        };
        self.moveresize(direction)
    }

    fn moveresize(&self, direction: u32) -> bool {
        if self.unmanaged {
            return false;
        }
        // The pointer's position in root coordinates, which is what the message
        // carries: the window manager grabs the pointer and needs to know where
        // the grab started.
        let Some(pointer) =
            self.connection.query_pointer(self.id).ok().and_then(|c| c.reply().ok())
        else {
            return false;
        };
        // Releasing the pointer first is required by the EWMH: the window
        // manager is about to grab it, and a client that still holds an implicit
        // grab from the press makes that grab fail silently.
        let _ = self.connection.ungrab_pointer(self.time);
        self.client_message(
            self.root,
            self.atoms._NET_WM_MOVERESIZE,
            [
                u32::from(pointer.root_x.cast_unsigned()),
                u32::from(pointer.root_y.cast_unsigned()),
                direction,
                1,
                1,
            ],
        );
        let _ = self.connection.flush();
        true
    }

    /// Maximizes or unmaximizes.
    ///
    /// Both axes at once: a window maximized in one direction only is a state
    /// EWMH allows and nothing in this program means by "maximized".
    pub fn set_maximized(&self, yes: bool) {
        let action = if yes { STATE_ADD } else { STATE_REMOVE };
        self.client_message(
            self.id,
            self.atoms._NET_WM_STATE,
            [
                action,
                self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                1,
                0,
            ],
        );
        let _ = self.connection.flush();
    }

    /// Minimizes.
    pub fn minimize(&self) {
        self.client_message(self.id, self.atoms.WM_CHANGE_STATE, [ICONIC_STATE, 0, 0, 0, 0]);
        let _ = self.connection.flush();
    }

    /// Asks for the window manager's own menu.
    ///
    /// **Does nothing, and says so through [`WindowState::can_show_menu`].**
    /// `xdg_toplevel.show_window_menu` has no EWMH equivalent; the
    /// `_GTK_SHOW_WINDOW_MENU` GTK uses instead is answered by mutter alone, and
    /// a capability that works on one window manager is not a capability.
    pub fn show_window_menu(&self, _at: Point) {}

    /// Asks the window manager to bring this window forward.
    ///
    /// **`_NET_ACTIVE_WINDOW`, with source indication 2.** Two means *a pager*,
    /// which is the EWMH's way of saying "a user asked for this through another
    /// program" — window managers honour it where they would refuse an
    /// application raising itself, and that distinction is exactly the one a
    /// `FileManager1` call sits on.
    ///
    /// The token is ignored here: X11's startup notification is a different
    /// mechanism from Wayland's activation token, and the timestamp below is what
    /// this protocol wants in its place. The argument exists because nothing above
    /// `sk-plat` may know which display server it has — ADR 0009.
    pub fn raise(&mut self, _token: Option<&str>) -> bool {
        let _ = self.connection.configure_window(
            self.id,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        );
        self.client_message(self.root, self.atoms._NET_ACTIVE_WINDOW, [2, self.time, 0, 0, 0]);
        let _ = self.connection.flush();
        true
    }

    /// A `ClientMessage` to the root or to ourselves, in the EWMH's shape.
    fn client_message(&self, target: xproto::Window, kind: xproto::Atom, data: [u32; 5]) {
        let event = xproto::ClientMessageEvent::new(32, self.id, kind, data);
        // To the root and not to the window: a window manager selects for
        // `SubstructureRedirect` on the root, which is where it hears requests
        // about its children.
        let destination = if target == self.id { self.root } else { target };
        let _ = self.connection.send_event(
            false,
            destination,
            xproto::EventMask::SUBSTRUCTURE_NOTIFY | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        );
    }

    /// Where the caret is, for an input method.
    ///
    /// **Nothing to tell on X11.** There is no input method connected: the
    /// equivalent of `text-input-v3` here is XIM or ibus, and
    /// [`Capabilities::input_method`](crate::Capabilities::input_method) reports
    /// the absence. The method exists so that nothing above `sk-plat` has to
    /// know which display server it is on — ADR 0009.
    pub fn set_text_area(&mut self, area: Option<crate::TextArea>) {
        let _ = area;
    }

    /// Gives the pointer an image, if it does not already have this one.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        if cursor == self.shape {
            return;
        }
        self.shape = cursor;
        self.apply_cursor();
    }

    fn apply_cursor(&mut self) {
        let handle = self.cursors.resolve(&self.connection, self.shape);
        let aux = xproto::ChangeWindowAttributesAux::new().cursor(handle);
        let _ = self.connection.change_window_attributes(self.id, &aux);
        let _ = self.connection.flush();
    }

    /// Asks the window to close, as though the button had been pressed.
    pub fn request_close(&mut self) {
        self.close_requested = true;
    }

    /// The size in logical pixels.
    #[must_use]
    pub fn logical(&self) -> Size {
        Size::new(self.logical.0 as f32, self.logical.1 as f32)
    }

    /// The size in device pixels.
    #[must_use]
    pub const fn physical(&self) -> (u32, u32) {
        self.physical
    }

    /// The scale as a numerator over 120.
    #[must_use]
    pub const fn scale_120(&self) -> u32 {
        self.scale_120
    }

    /// The scale as a factor.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale_120 as f32 / 120.0
    }

    /// The X objects a GPU backend needs.
    #[must_use]
    pub fn raw_handles(&self) -> RawHandles {
        RawHandles::X11 {
            connection: self.connection.get_raw_xcb_connection().cast::<c_void>(),
            screen: i32::try_from(self.screen).unwrap_or(0),
            window: self.id,
            visual: self.visual,
        }
    }

    /// Whether the window should stop.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        self.close_requested || self.fatal.is_some()
    }

    /// Why the connection died, if it did.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// What the peer is currently offering on each selection.
    #[must_use]
    pub const fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// The drag hovering over this window, if one is.
    #[must_use]
    pub const fn dragging(&self) -> Option<&crate::Drag> {
        self.xdnd.incoming()
    }

    /// Whether a drag out of this window is still in flight.
    #[must_use]
    pub const fn dragging_out(&self) -> bool {
        self.xdnd.outgoing()
    }

    /// What the selection machinery is holding. See [`crate::Selections`].
    #[must_use]
    pub fn selections(&self) -> crate::Selections {
        crate::Selections {
            awaiting: self.selections.awaiting(),
            sending: self.transfers.len(),
            unmatched: self.selections.unmatched(),
        }
    }

    /// Which actions this display server can express.
    ///
    /// All five: XDND is the vocabulary the union was taken from.
    #[must_use]
    pub const fn drag_actions(&self) -> &'static [crate::DragAction] {
        use crate::DragAction;
        &[
            DragAction::Copy,
            DragAction::Move,
            DragAction::Link,
            DragAction::Ask,
            DragAction::Private,
        ]
    }

    /// Tells the source what would happen if the drag were dropped now.
    pub fn accept_drag(&mut self, mime: Option<&str>, action: crate::DragAction) {
        self.xdnd.accept(&self.connection, &self.atoms, self.id, mime, action);
    }

    /// Starts a drag carrying `content`. Whether it began.
    pub fn start_drag(&mut self, content: Content, actions: &[crate::DragAction]) -> bool {
        let grab = xdnd::Grab {
            window: self.id,
            time: self.time,
            cursor: self.cursors.resolve(&self.connection, crate::Cursor::Dragging),
        };
        let started =
            self.xdnd.start(&self.connection, &self.atoms, &grab, content.clone(), actions);
        // The bytes are served by the selection machinery, which is what carries
        // an `INCR` — and a directory's worth of URIs is one.
        if started {
            self.selections.set_extra(self.atoms.XdndSelection, Some(content), self.time);
        }
        started
    }

    /// Abandons a drag out of this window. See the Wayland half for why.
    pub fn cancel_drag(&mut self) {
        self.xdnd.cancel(&self.connection, &self.atoms, self.id, self.time);
        self.selections.set_extra(self.atoms.XdndSelection, None, self.time);
    }

    /// Ends a drop without reading it. See the Wayland half for why.
    pub fn finish_drop(&mut self) {
        self.xdnd.finish(&self.connection, &self.atoms, self.id);
    }

    /// Reads what was dropped, as a descriptor.
    pub fn take_drop(&mut self, mime: &str) -> Option<std::os::fd::OwnedFd> {
        let ask = ask!(self);
        self.xdnd.take(&ask, &mut self.selections, &mut self.transfers, mime)
    }

    /// Publishes `content` on `selection`.
    pub fn set_selection(&mut self, selection: Selection, content: Content) {
        let offered = self.selections.publish(&ask!(self), selection, content);
        match selection {
            Selection::Clipboard => {
                self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
                self.clipboard.offer_types = offered;
            }
            Selection::Primary => self.clipboard.primary_types = offered,
        }
    }

    /// Asks the peer for `mime` on `selection`, as a descriptor to read.
    pub fn request_selection(
        &mut self,
        selection: Selection,
        mime: &str,
    ) -> Option<std::os::fd::OwnedFd> {
        self.selections.request(&ask!(self), selection, mime)
    }

    /// Blocks until something happens, then queues everything that did.
    pub fn pump(&mut self, wake: Option<&Waker>, timeout: Option<Duration>) {
        if self.fatal.is_some() {
            return;
        }
        resume_all(&mut self.transfers);

        // Anything the library already buffered is handled without touching the
        // socket, and without sleeping when there was something to do.
        if self.drain_server() {
            self.after_events();
            return;
        }
        if let Err(error) = self.connection.flush() {
            self.fatal = Some(format!("flush: {error}"));
            return;
        }

        // A conversion in flight is a wake-up with an owner: it is abandoned
        // at its deadline whether or not the peer ever says anything again.
        let timeout = shorter(timeout, self.repeat.next_in());
        let timeout = shorter(timeout, self.selections.next_deadline(Instant::now()));
        let display = unsafe_borrowed_fd(self.connection.as_raw_fd());
        let wake_fd = wake.map(Waker::as_fd);
        let mut fds = Vec::with_capacity(2 + self.transfers.len());
        fds.push(PollFd::new(&display, PollFlags::IN));
        if let Some(fd) = &wake_fd {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        // An unfinished selection write waits for room in its pipe in the same
        // `poll` as everything else. No thread, no timer, and idle stays idle
        // because a full pipe is simply not ready.
        let waiting = fds.len();
        let pipes: Vec<_> = self.transfers.iter().map(Transfer::as_fd).collect();
        for pipe in &pipes {
            fds.push(PollFd::new(pipe, PollFlags::OUT));
        }
        let spec = timeout.map(to_timespec);
        let ready = rustix::event::poll(&mut fds, spec.as_ref()).unwrap_or(0);

        if wake_fd.is_some() && ready > 0 && fds[1].revents().contains(PollFlags::IN) {
            if let Some(wake) = wake {
                wake.drain();
            }
            self.events.push_back(Event::Woken);
        }
        if fds[waiting..].iter().any(|fd| !fd.revents().is_empty()) {
            resume_all(&mut self.transfers);
        }
        self.drain_server();
        self.after_events();
    }

    /// Everything that has happened since the last drain, oldest first.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.events.drain(..)
    }

    /// Reads every event the server has for us. Whether there was any.
    fn drain_server(&mut self) -> bool {
        let mut any = false;
        loop {
            match self.connection.poll_for_event() {
                Ok(Some(event)) => {
                    any = true;
                    self.handle(event);
                }
                Ok(None) => return any,
                Err(error) => {
                    self.fatal = Some(format!("X11: {error}"));
                    return any;
                }
            }
        }
    }

    fn after_events(&mut self) {
        self.repeat.emit(&mut self.events);
        self.selections.expire(Instant::now());
        self.publish_geometry();
    }

    /// Reports the size upward exactly once per change.
    fn publish_geometry(&mut self) {
        let current = (self.logical, self.scale_120);
        if self.reported == Some(current) {
            return;
        }
        self.reported = Some(current);
        self.events.push_back(Event::Resized {
            logical: Size::new(self.logical.0 as f32, self.logical.1 as f32),
            physical: self.physical,
            scale_120: self.scale_120,
        });
    }

    /// Recomputes the logical size from the device size and the scale.
    ///
    /// The direction that matters: X11 states device pixels, so the logical size
    /// is derived here. Wayland states logical pixels and derives the device
    /// size. Both end up in the same [`Event::Resized`] and nothing above this
    /// crate can tell which way round it was.
    fn relogical(&mut self) {
        let scale = self.scale_120.max(1);
        self.logical = (
            (self.physical.0 * 120).div_ceil(scale).max(1),
            (self.physical.1 * 120).div_ceil(scale).max(1),
        );
    }

    /// A device-pixel position from the server, in logical pixels.
    fn logical_point(&self, x: i16, y: i16) -> Point {
        let scale = self.scale().max(f32::MIN_POSITIVE);
        Point::new(f32::from(x) / scale, f32::from(y) / scale)
    }

    fn handle(&mut self, event: X11Event) {
        match event {
            X11Event::ConfigureNotify(event) if event.window == self.id => {
                let physical = (u32::from(event.width).max(1), u32::from(event.height).max(1));
                if physical != self.physical {
                    self.physical = physical;
                    self.relogical();
                }
            }
            X11Event::ClientMessage(event) if event.window == self.id => {
                if self.xdnd.message(
                    &self.connection,
                    &self.atoms,
                    self.id,
                    &event,
                    &mut self.events,
                ) {
                    return;
                }
                let data = event.data.as_data32();
                if event.type_ == self.atoms.WM_PROTOCOLS
                    && data.first() == Some(&self.atoms.WM_DELETE_WINDOW)
                {
                    self.events.push_back(Event::CloseRequested);
                }
            }
            X11Event::PropertyNotify(event) => self.property_changed(&event),
            X11Event::FocusIn(_) => {
                self.shell.activated = true;
                self.events.push_back(Event::Focused(true));
            }
            X11Event::FocusOut(_) => {
                self.shell.activated = false;
                // Every held key is released by a focus change, so a repeat that
                // outlives the focus would type into a window that is not ours.
                self.repeat.clear();
                self.events.push_back(Event::Focused(false));
            }
            X11Event::KeyPress(event) => self.key(event.detail, event.state, event.time, true),
            X11Event::KeyRelease(event) => self.key(event.detail, event.state, event.time, false),
            X11Event::ButtonPress(event) => self.pointer_button(&event, true),
            X11Event::ButtonRelease(event) => {
                if self.xdnd.outgoing() {
                    self.time = event.time;
                    if !self.xdnd.released(&self.connection, &self.atoms, self.id, event.time) {
                        self.selections.set_extra(self.atoms.XdndSelection, None, self.time);
                    }
                }
                // The button still comes up for the caller either way: a drag
                // that ended on a peer is still a release here, and the code
                // above this crate has state of its own to let go of.
                self.pointer_button(&event, false);
            }
            X11Event::MotionNotify(event) => {
                self.time = event.time;
                // **Both, while a drag is going out.** The XDND conversation
                // needs every motion, and so does the caller: our own
                // `XdndPosition` comes back to us for a drag that stays over this
                // window, but a caller that waited only for that would have
                // nothing to draw if the round trip were slow — and a highlight
                // that lags the pointer is a drop that lands somewhere else.
                if self.xdnd.outgoing() {
                    let at = xdnd::Where { window: self.id, root: self.root, time: event.time };
                    self.xdnd.moved(&self.connection, &self.atoms, at, event.root_x, event.root_y);
                }
                self.pointer_state(event.state);
                self.position = self.logical_point(event.event_x, event.event_y);
                let mods = self.keyboard.as_ref().map(|k| k.mods).unwrap_or_default();
                self.events.push_back(Event::PointerMoved { position: self.position, mods });
            }
            X11Event::LeaveNotify(event) => {
                self.time = event.time;
                // A click that leaves the window is still that click: the
                // multi-click run ends where the pointer goes somewhere else,
                // which is the same rule Wayland's `leave` follows.
                self.clicks = Clicks::default();
                self.events.push_back(Event::PointerLeft);
            }
            X11Event::EnterNotify(event) => {
                self.time = event.time;
                self.position = self.logical_point(event.event_x, event.event_y);
            }
            X11Event::SelectionRequest(event) => self.selections.answer(&ask!(self), &event),
            X11Event::SelectionNotify(event) => {
                let ask = ask!(self);
                if let Some(offered) = self.selections.arrived(&ask, &mut self.transfers, &event) {
                    // A `TARGETS` reply, which is the answer to "what is on the
                    // clipboard" — the question X11 has no other way to ask.
                    if offered.primary {
                        self.clipboard.primary_types = offered.mimes;
                    } else {
                        self.clipboard.offer_types = offered.mimes;
                    }
                }
            }
            X11Event::SelectionClear(event) => {
                self.selections.lost(&self.atoms, event.selection);
            }
            X11Event::XfixesSelectionNotify(event) => self.selection_changed(&event),
            X11Event::Error(error) => {
                // An X11 error is asynchronous and names the request that caused
                // it, which is the only way to find out that a property nobody
                // reads was refused. Not fatal: the connection is still good.
                self.note_error(&error);
            }
            _ => {}
        }
    }

    /// Somebody took a selection. What they offer is a second question.
    ///
    /// `TARGETS` is how X11 asks it, and the answer arrives later as a
    /// `SelectionNotify`. Our own copy is skipped: we already know what is in it,
    /// and asking would be a round trip to answer a question we are the answer
    /// to.
    fn selection_changed(&mut self, event: &xfixes::SelectionNotifyEvent) {
        // Qt refuses conversions older than its ownership timestamp. An input
        // event from before this selection existed cannot timestamp TARGETS.
        self.time = event.timestamp;
        if event.owner == self.id {
            return;
        }
        let which = if event.selection == self.atoms.CLIPBOARD {
            self.clipboard.generation = self.clipboard.generation.wrapping_add(1);
            self.clipboard.offer_types.clear();
            Selection::Clipboard
        } else {
            Selection::Primary
        };
        self.ask_targets(which);
    }

    /// Asks whoever owns a selection what types they are offering.
    ///
    /// The reply arrives later as a `SelectionNotify`, which is where
    /// [`crate::Clipboard::offered`] is filled in. Nothing blocks: a peer that
    /// never answers leaves the offer list as it was, which is the same as not
    /// having asked.
    fn ask_targets(&mut self, selection: Selection) {
        self.selections.ask_targets(&ask!(self), selection);
    }

    fn note_error(&mut self, error: &x11rb::x11_utils::X11Error) {
        // Bounded, because a request issued once a frame that always fails would
        // otherwise grow this without limit.
        const MOST: usize = 16;
        if self.capabilities.advertised.len() < MOST {
            self.capabilities
                .advertised
                .push((format!("error:{:?}", error.error_kind), u32::from(error.major_opcode)));
        }
    }

    fn property_changed(&mut self, event: &xproto::PropertyNotifyEvent) {
        self.time = event.time;
        if event.window == self.root && event.atom == self.atoms.RESOURCE_MANAGER {
            self.rescale();
        }
        if event.window == self.id && event.atom == self.atoms._NET_WM_STATE {
            self.read_wm_state();
        }
        if event.window == self.id {
            self.selections.property_changed(
                &self.connection,
                &self.atoms,
                &mut self.transfers,
                event,
            );
        }
    }

    /// Re-reads `Xft.dpi` and resizes the window to keep the same logical size.
    ///
    /// The logical size is what a person arranged; the device size is a
    /// consequence. Keeping the device size instead would make every window
    /// shrink when the desktop's scale went up, which is not what "make
    /// everything bigger" means.
    fn rescale(&mut self) {
        if self.scale_forced {
            return;
        }
        let scale = scale_from_resources(&self.connection, self.root, self.atoms.RESOURCE_MANAGER);
        if scale == self.scale_120 {
            return;
        }
        self.scale_120 = scale;
        self.cursors.rescale(scale);
        self.apply_cursor();
        let physical = physical_of(self.logical, scale);
        let aux = xproto::ConfigureWindowAux::new().width(physical.0).height(physical.1);
        let _ = self.connection.configure_window(self.id, &aux);
        let _ = self.connection.flush();
        // Not applied here: the server answers with a `ConfigureNotify` and that
        // is the size that is real. Applying it now would report a size the
        // window does not have yet, which is the frame-at-the-wrong-scale defect
        // in a different costume.
    }

    fn read_wm_state(&mut self) {
        let Some(reply) = self
            .connection
            .get_property(false, self.id, self.atoms._NET_WM_STATE, xproto::AtomEnum::ATOM, 0, 64)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        let states: Vec<u32> = reply.value32().into_iter().flatten().collect();
        let has = |atom: u32| states.contains(&atom);
        // Both axes: a window maximized horizontally alone is a state EWMH
        // allows and nothing here means by "maximized".
        self.shell.maximized = has(self.atoms._NET_WM_STATE_MAXIMIZED_HORZ)
            && has(self.atoms._NET_WM_STATE_MAXIMIZED_VERT);
        self.shell.fullscreen = has(self.atoms._NET_WM_STATE_FULLSCREEN);
    }

    /// One button, pressed or released.
    fn pointer_button(&mut self, event: &xproto::ButtonPressEvent, pressed: bool) {
        self.time = event.time;
        self.pointer_state(event.state);
        self.position = self.logical_point(event.event_x, event.event_y);
        self.button(event.detail, event.time, pressed);
    }

    fn pointer_state(&mut self, state: xproto::KeyButMask) {
        if let Some(keyboard) = &mut self.keyboard {
            keyboard.absorb(state.bits());
        }
    }

    fn key(&mut self, keycode: u8, state: xproto::KeyButMask, time: u32, down: bool) {
        self.time = time;
        // The mask on the event is the state *before* it, so a press of `Ctrl`
        // itself reports no Ctrl — which is right, and is why the modifier is
        // absorbed before the event is built rather than after.
        let Some(keyboard) = &mut self.keyboard else { return };
        keyboard.absorb(state.bits());
        let (event, keysym, repeats) = keyboard.event(keycode, down);
        if down {
            self.repeat.press(&event, keysym, repeats);
            self.events.push_back(Event::KeyDown(event));
        } else {
            self.repeat.release(keysym);
            self.events.push_back(Event::KeyUp(event));
        }
    }

    fn button(&mut self, detail: u8, time: u32, pressed: bool) {
        let mods = self.keyboard.as_ref().map(|k| k.mods).unwrap_or_default();
        // Buttons 4 to 7 are the wheel, which X11 reports as presses because it
        // predates wheels. A release of one is the other half of the same notch
        // and scrolling twice for it is the classic X11 double-scroll.
        if let Some(delta) = wheel_delta(detail) {
            if pressed {
                self.events.push_back(Event::Scroll { delta, precise: false, time, mods });
            }
            return;
        }
        let button = button_of(detail);
        let clicks = if pressed { self.clicks.count(time, self.position) } else { 0 };
        self.events.push_back(Event::Button {
            button,
            pressed,
            position: self.position,
            clicks,
            mods,
        });
    }
}

/// Creates the toplevel, with every event this backend needs selected on it.
///
/// Separate from [`Window::open`] because it is the one part that is purely
/// protocol: no policy, no capabilities, and the event mask is a list a reader
/// should be able to check against what `handle` matches on.
fn create_window(
    connection: &XCBConnection,
    root: xproto::Window,
    visual: xproto::Visualid,
    depth: u8,
    background: u32,
    physical: (u32, u32),
) -> Result<xproto::Window, Problem> {
    let window = connection
        .generate_id()
        .map_err(|error| Problem::plain(ProblemKind::NoDisplay, format!("id: {error}")))?;
    let aux = xproto::CreateWindowAux::new()
        // Black rather than nothing: an unset background is whatever was on
        // screen, and the first frame arrives one round trip later.
        .background_pixel(background)
        .event_mask(
            xproto::EventMask::EXPOSURE
                | xproto::EventMask::STRUCTURE_NOTIFY
                | xproto::EventMask::KEY_PRESS
                | xproto::EventMask::KEY_RELEASE
                | xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE
                | xproto::EventMask::POINTER_MOTION
                | xproto::EventMask::ENTER_WINDOW
                | xproto::EventMask::LEAVE_WINDOW
                | xproto::EventMask::FOCUS_CHANGE
                | xproto::EventMask::PROPERTY_CHANGE,
        );
    connection
        .create_window(
            depth,
            window,
            root,
            0,
            0,
            physical.0.try_into().unwrap_or(u16::MAX),
            physical.1.try_into().unwrap_or(u16::MAX),
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )
        .map_err(|error| {
            Problem::plain(ProblemKind::NoDisplay, format!("create_window: {error}"))
        })?;
    Ok(window)
}

/// X11 button numbers, from the core protocol.
///
/// Every number is a button: X11 has no reserved values, and a mouse with nine
/// of them reports nine. The wheel is handled before this — see [`wheel_delta`].
const fn button_of(detail: u8) -> Button {
    match detail {
        1 => Button::Primary,
        2 => Button::Middle,
        3 => Button::Secondary,
        other => Button::Other(other as u32),
    }
}

/// The scroll a wheel "button" means, or nothing for a real button.
fn wheel_delta(detail: u8) -> Option<Point> {
    Some(match detail {
        4 => Point::new(0.0, -WHEEL_NOTCH),
        5 => Point::new(0.0, WHEEL_NOTCH),
        6 => Point::new(-WHEEL_NOTCH, 0.0),
        7 => Point::new(WHEEL_NOTCH, 0.0),
        _ => return None,
    })
}

/// `WM_CLASS`: instance then class, each NUL-terminated.
///
/// The instance is the last component of the app id lowercased — `skerry` — and
/// the class is that with its first letter raised, which is the convention every
/// X11 window manager's rule syntax was written against.
fn wm_class(app_id: &str) -> Vec<u8> {
    let leaf = app_id.rsplit('.').next().unwrap_or(app_id);
    let instance = leaf.to_lowercase();
    let mut class = instance.clone();
    if let Some(first) = class.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let mut bytes = Vec::with_capacity(instance.len() + class.len() + 2);
    bytes.extend_from_slice(instance.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(class.as_bytes());
    bytes.push(0);
    bytes
}

/// `Xft.dpi` from the X resource database, as a scale numerator over 120.
fn scale_from_resources(
    connection: &impl x11rb::connection::Connection,
    root: xproto::Window,
    atom: xproto::Atom,
) -> u32 {
    let text = connection
        .get_property(false, root, atom, xproto::AtomEnum::STRING, 0, 64 * 1024)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
        .unwrap_or_default();
    scale_of(&text)
}

/// Parses a resource database and turns its `Xft.dpi` into a scale over 120.
///
/// Split out from the request so it can be tested without an X server, which is
/// the only part of this worth testing: the round trip is `x11rb`'s.
fn scale_of(resources: &str) -> u32 {
    let dpi = resources
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|value| value.trim().parse::<f32>().ok());
    let Some(dpi) = dpi.filter(|dpi| dpi.is_finite() && *dpi > 0.0) else {
        return 120;
    };
    // Clamped rather than trusted. A database is a text file a person can edit,
    // and an `Xft.dpi` of 6000 is a window a compositor will refuse to allocate
    // a buffer for — which reads as "the program will not start" and not as
    // "that number was wrong".
    let scale = (dpi / BASE_DPI * 120.0).round();
    (scale as u32).clamp(60, 600)
}

/// What this X server turned out to support.
fn probe(connection: &XCBConnection, root: xproto::Window, atoms: &Atoms) -> Capabilities {
    let mut advertised = Vec::new();
    let mut present = |name: &str| {
        let known = connection
            .query_extension(name.as_bytes())
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.present);
        if known {
            advertised.push((name.to_owned(), 1));
        }
        known
    };
    present("RANDR");
    present("XKEYBOARD");
    present("XFIXES");
    present("RENDER");

    // A window manager is running exactly when `_NET_SUPPORTING_WM_CHECK` names
    // a window that exists. Without one nothing will decorate, maximize or move
    // this window, and the frame has to be drawn here — which is the state a
    // bare `Xvfb` is in.
    let managed = connection
        .get_property(false, root, atoms._NET_SUPPORTING_WM_CHECK, xproto::AtomEnum::WINDOW, 0, 1)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().and_then(|mut it| it.next()))
        .is_some_and(|window| window != 0);

    Capabilities {
        // Nothing between this process and the screen resamples anything on X11:
        // the buffer is the size it is drawn at, whatever the scale is.
        fractional_scale: true,
        viewporter: true,
        server_side_decorations: managed,
        keyboard: false,
        pointer: true,
        cursor_shape: false,
        // XInput 2.4 has gesture events and this backend reads core ones, so a
        // touchpad here sends scroll and nothing else. Reported rather than
        // pretended: ADR 0007's whole point is that a capability is probed and
        // said out loud.
        gestures: false,
        // XIM is a protocol over properties and `ClientMessage`s and ibus is
        // D-Bus; neither is written, so a CJK input method cannot reach this
        // window on X11. Compose and dead keys work here, and are not this.
        input_method: false,
        clipboard: true,
        primary_selection: true,
        render_device: None,
        advertised,
    }
}

/// The X connection's descriptor, borrowed for the length of one `poll`.
///
/// `XCBConnection` owns it and closes it on drop, and it outlives every borrow
/// here because the borrow does not escape `pump`.
fn unsafe_borrowed_fd(raw: std::os::fd::RawFd) -> BorrowedFd<'static> {
    // SAFETY: `raw` comes from the live `XCBConnection` held by this `Window`,
    // which is not dropped while `pump` runs, and the returned borrow does not
    // escape it.
    unsafe { BorrowedFd::borrow_raw(raw) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_with_no_dpi_is_one_hundred_percent() {
        assert_eq!(scale_of(""), 120);
        assert_eq!(scale_of("Xcursor.theme:\tAdwaita\n"), 120);
    }

    #[test]
    fn the_dpi_every_desktop_writes_becomes_the_scale_wayland_would_send() {
        assert_eq!(scale_of("Xft.dpi:\t96\n"), 120);
        assert_eq!(scale_of("Xft.dpi:\t120\n"), 150);
        assert_eq!(scale_of("Xft.dpi:\t144\n"), 180);
        assert_eq!(scale_of("Xft.dpi:\t192\n"), 240);
    }

    #[test]
    fn a_dpi_among_other_resources_is_still_found() {
        let database = "Xcursor.size:\t24\nXft.dpi:\t120\nXft.antialias:\t1\n";
        assert_eq!(scale_of(database), 150);
    }

    /// A resource database is a text file a person can edit.
    #[test]
    fn an_absurd_dpi_is_clamped_rather_than_believed() {
        assert_eq!(scale_of("Xft.dpi:\t6000\n"), 600);
        assert_eq!(scale_of("Xft.dpi:\t1\n"), 60);
        assert_eq!(scale_of("Xft.dpi:\t0\n"), 120);
        assert_eq!(scale_of("Xft.dpi:\t-96\n"), 120);
        assert_eq!(scale_of("Xft.dpi:\tnot a number\n"), 120);
    }

    #[test]
    fn the_window_class_is_the_pair_a_window_manager_matches_on() {
        assert_eq!(wm_class("dev.skerry.Skerry"), b"skerry\0Skerry\0".to_vec());
        assert_eq!(wm_class("skerry"), b"skerry\0Skerry\0".to_vec());
    }

    #[test]
    fn the_wheel_is_four_buttons_and_the_rest_are_buttons() {
        assert_eq!(wheel_delta(4), Some(Point::new(0.0, -WHEEL_NOTCH)));
        assert_eq!(wheel_delta(5), Some(Point::new(0.0, WHEEL_NOTCH)));
        assert_eq!(wheel_delta(6), Some(Point::new(-WHEEL_NOTCH, 0.0)));
        assert_eq!(wheel_delta(7), Some(Point::new(WHEEL_NOTCH, 0.0)));
        assert_eq!(wheel_delta(1), None);
        assert_eq!(wheel_delta(8), None);
    }

    /// X11 numbers the middle button 2 and the secondary 3; getting those the
    /// other way round swaps paste and the context menu.
    #[test]
    fn the_button_numbers_are_the_core_protocols() {
        assert_eq!(button_of(1), Button::Primary);
        assert_eq!(button_of(2), Button::Middle);
        assert_eq!(button_of(3), Button::Secondary);
        assert_eq!(button_of(8), Button::Other(8));
    }
}
