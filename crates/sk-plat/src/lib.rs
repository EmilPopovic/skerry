//! Wayland and X11 protocol handling, keyboard mapping, pointer input, monitors
//! and scaling, the clipboard, and window lifecycle.
//!
//! Does not own: drawing anything.
//!
//! [ADR 0002](../../../docs/adr/0002-own-platform-layer.md) argues why this is
//! ours and prices it. What arrives from here is already policy: logical
//! coordinates, resolved keysyms, counted clicks, coalesced scroll — the things
//! a general-purpose windowing crate either decides differently or leaves out.
//!
//! # Two backends, chosen at run time
//!
//! [`Window`] is an enum, not a trait object and not a `#[cfg]`.
//! [ADR 0009](../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)
//! argues it: the common machine has both servers — this one runs a Wayland
//! compositor with an Xwayland beside it — so which one to speak is a property
//! of the session and not of the build, and a build flag cannot see a session.
//!
//! Nothing above this crate learns which backend it has. `tools/check-invariants.py`
//! enforces that: no crate above `sk-plat` may name either variant.
//!
//! # What is here and what is owed
//!
//! Windowing, input, scale, the clipboard and the primary selection are here on
//! both servers. **Drag-and-drop and IME are not yet**, and M0 is not finished
//! without them; Spike D proved each of them against real GTK and Qt peers, and
//! the ordering invariants it found are recorded in [`wayland`] and in
//! [`clipboard`].

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod clipboard;
pub mod drag;
pub mod event;
mod input;
pub mod keys;
mod wait;
pub mod wayland;
pub mod window;
pub mod x11;

use std::time::Duration;

use sk_model::{Problem, ProblemKind};
use sk_util::Waker;

pub use clipboard::{Clipboard, Content, Intent, Selection, decode_copied_files, decode_uri};
pub use drag::{Drag, DragAction};
pub use event::{Button, Event, Key, KeyEvent, Modifiers, Named, Point, Size};
pub use window::{Capabilities, Cursor, Edge, RawHandles, WindowConfig, WindowState};

/// Which display server to talk to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    /// A Wayland compositor.
    Wayland,
    /// An X server, including an Xwayland.
    X11,
}

impl Backend {
    /// What the environment says to use, in the order a session sets it.
    ///
    /// `SKERRY_BACKEND` first, because a machine that has both needs a way to
    /// exercise either without a second binary — which is what the scenarios
    /// under `Xvfb` do. Then `WAYLAND_DISPLAY`, then `DISPLAY`: a Wayland session
    /// almost always has an Xwayland too, and preferring X11 there would pick the
    /// emulation over the real thing.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        match std::env::var("SKERRY_BACKEND").as_deref() {
            Ok("wayland") => return Some(Self::Wayland),
            Ok("x11") => return Some(Self::X11),
            _ => {}
        }
        // Empty means unset. A session that exports `WAYLAND_DISPLAY=` — which
        // is how a script says "not that one" — was otherwise read as a Wayland
        // session with a socket named nothing.
        let named = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        if named("WAYLAND_DISPLAY") {
            return Some(Self::Wayland);
        }
        named("DISPLAY").then_some(Self::X11)
    }

    /// The name, for a log line and a diagnostic dump.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

/// A window on whichever display server this session runs.
///
/// Every method forwards. The forwarding is the whole of this type and it
/// contains no logic, so a mistake in it is a compile error rather than a
/// behaviour — which is the trade [ADR 0009](../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)
/// made against a vtable.
///
/// The variants differ in size — 1,808 bytes against 1,064, measured — and that
/// is left alone rather than boxed. The padding multiplies by the number of
/// these that exist, which is **one** for the life of the process; a `Box` would
/// trade 744 bytes of stack, once, for a pointer chase on every `pump`.
#[allow(clippy::large_enum_variant, reason = "one per process; see above")]
pub enum Window {
    /// A `wl_surface` and an `xdg_toplevel`.
    Wayland(wayland::Window),
    /// An X11 window and its window manager.
    X11(x11::Window),
}

/// Forwards a `&self` method to whichever backend this is.
macro_rules! forward {
    ($self:ident, $method:ident $(, $argument:expr)*) => {
        match $self {
            Self::Wayland(window) => window.$method($($argument),*),
            Self::X11(window) => window.$method($($argument),*),
        }
    };
}

impl Window {
    /// Connects to the display server this session runs, and maps a window.
    ///
    /// The error names **both** servers when neither is reachable, because
    /// "cannot connect" over one of them is the answer to a question nobody
    /// asked: a person whose `DISPLAY` is unset and whose compositor is not
    /// running needs to be told which one was looked for.
    pub fn open(config: &WindowConfig) -> Result<Self, Problem> {
        match Backend::from_env() {
            Some(Backend::Wayland) => wayland::Window::open(config).map(Self::Wayland),
            Some(Backend::X11) => x11::Window::open(config).map(Self::X11),
            None => Err(Problem::plain(
                ProblemKind::NoDisplay,
                "neither WAYLAND_DISPLAY nor DISPLAY is set",
            )),
        }
    }

    /// Which backend this turned out to be.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        match self {
            Self::Wayland(_) => Backend::Wayland,
            Self::X11(_) => Backend::X11,
        }
    }

    /// What this display server turned out to support.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        forward!(self, capabilities)
    }

    /// What the display server last said about the window.
    #[must_use]
    pub fn state(&self) -> WindowState {
        forward!(self, state)
    }

    /// Whether the frame has to be drawn here.
    #[must_use]
    pub fn needs_own_frame(&self) -> bool {
        forward!(self, needs_own_frame)
    }

    /// Asks the display server to move the window. Whether it agreed.
    pub fn start_move(&self) -> bool {
        forward!(self, start_move)
    }

    /// Asks the display server to resize the window from `edge`.
    pub fn start_resize(&self, edge: Edge) -> bool {
        forward!(self, start_resize, edge)
    }

    /// Maximizes or unmaximizes.
    pub fn set_maximized(&self, yes: bool) {
        forward!(self, set_maximized, yes);
    }

    /// Minimizes.
    pub fn minimize(&self) {
        forward!(self, minimize);
    }

    /// Asks for the window manager's own menu at `at`.
    pub fn show_window_menu(&self, at: Point) {
        forward!(self, show_window_menu, at);
    }

    /// Gives the pointer an image, if it does not already have this one.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        forward!(self, set_cursor, cursor);
    }

    /// Asks the window to close, as though the button had been pressed.
    pub fn request_close(&mut self) {
        forward!(self, request_close);
    }

    /// The size in logical pixels.
    #[must_use]
    pub fn logical(&self) -> Size {
        forward!(self, logical)
    }

    /// The size in device pixels.
    #[must_use]
    pub fn physical(&self) -> (u32, u32) {
        forward!(self, physical)
    }

    /// The scale as a numerator over 120.
    #[must_use]
    pub fn scale_120(&self) -> u32 {
        forward!(self, scale_120)
    }

    /// The scale as a factor, for arithmetic that cannot stay in integers.
    #[must_use]
    pub fn scale(&self) -> f32 {
        forward!(self, scale)
    }

    /// The display-server objects a GPU backend needs.
    #[must_use]
    pub fn raw_handles(&self) -> RawHandles {
        forward!(self, raw_handles)
    }

    /// Whether the window should stop: the user asked, or the connection died.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        forward!(self, close_requested)
    }

    /// Why the connection died, if it did.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        forward!(self, failure)
    }

    /// What the peer is currently offering on each selection.
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        forward!(self, clipboard)
    }

    /// Publishes `content` on `selection`.
    pub fn set_selection(&mut self, selection: Selection, content: Content) {
        forward!(self, set_selection, selection, content);
    }

    /// Asks the peer for `mime` on `selection`, as a descriptor to read.
    pub fn request_selection(
        &mut self,
        selection: Selection,
        mime: &str,
    ) -> Option<std::os::fd::OwnedFd> {
        forward!(self, request_selection, selection, mime)
    }

    /// Starts a drag carrying `content`. Whether the display server took it.
    ///
    /// Needs the serial of a button press on both servers, which is what makes a
    /// drag legal — and there is no event for the refusal, so a `false` here is
    /// the only notice a caller gets.
    pub fn start_drag(&mut self, content: Content, actions: &[DragAction]) -> bool {
        forward!(self, start_drag, content, actions)
    }

    /// Whether a drag out of this window is still in flight.
    #[must_use]
    pub fn dragging_out(&self) -> bool {
        forward!(self, dragging_out)
    }

    /// The drag hovering over this window, if one is.
    #[must_use]
    pub fn dragging(&self) -> Option<&Drag> {
        forward!(self, dragging)
    }

    /// Which drag actions this display server can express at all.
    ///
    /// Wayland has three of the five; X11 has all of them. A menu offering one
    /// this session cannot do is a menu entry that silently does something else.
    #[must_use]
    pub fn drag_actions(&self) -> &'static [DragAction] {
        forward!(self, drag_actions)
    }

    /// Tells the source what would happen if the drag were dropped now.
    ///
    /// `None` declines. Said on **every** motion: a target that accepts once and
    /// then goes quiet shows a refusal cursor over rows it would in fact take.
    pub fn accept_drag(&mut self, mime: Option<&str>, action: DragAction) {
        forward!(self, accept_drag, mime, action);
    }

    /// Abandons a drag out of this window, for a gesture that ended inside it.
    ///
    /// The drop is then the caller's to perform, and the display server has
    /// nothing left to arrange. Necessary rather than tidy: a compositor that
    /// never sends `drop` or `dnd_finished` — Spike D measured one — otherwise
    /// leaves a source alive for the rest of the session.
    pub fn cancel_drag(&mut self) {
        forward!(self, cancel_drag);
    }

    /// Ends a drop without reading it, for a drag this window started itself.
    ///
    /// The source has to be told either way, or it keeps the pointer grabbed
    /// waiting for a confirmation that never arrives.
    pub fn finish_drop(&mut self) {
        forward!(self, finish_drop);
    }

    /// Reads what was dropped, as a descriptor to read at leisure.
    pub fn take_drop(&mut self, mime: &str) -> Option<std::os::fd::OwnedFd> {
        forward!(self, take_drop, mime)
    }

    /// Blocks until something happens, then queues everything that did.
    ///
    /// This is where "zero CPU at idle" lives on both backends. The thread
    /// sleeps in `poll` on the display connection and, optionally, on a job
    /// pool's eventfd. There is no timer and no polling interval; `timeout`
    /// exists for an animation that has a next frame, and key repeat shortens it
    /// internally.
    pub fn pump(&mut self, wake: Option<&Waker>, timeout: Option<Duration>) {
        forward!(self, pump, wake, timeout);
    }

    /// Everything that has happened since the last drain, oldest first.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        // Not `forward!`: the two backends return different iterator types, so
        // the arms are collected into one. A `VecDeque::drain` on either side,
        // which is what both of these already are.
        let events: Vec<Event> = match self {
            Self::Wayland(window) => window.drain_events().collect(),
            Self::X11(window) => window.drain_events().collect(),
        };
        events.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment is process-global, so these run under one lock rather
    /// than as four tests that pass alone and fail together.
    #[test]
    fn the_backend_comes_from_the_session_and_can_be_forced() {
        let restore: Vec<(&str, Option<std::ffi::OsString>)> =
            ["SKERRY_BACKEND", "WAYLAND_DISPLAY", "DISPLAY"]
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
        // SAFETY: single-threaded test, and every variable is restored below.
        let set = |name: &str, value: Option<&str>| unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        };

        set("SKERRY_BACKEND", None);
        set("WAYLAND_DISPLAY", None);
        set("DISPLAY", None);
        assert_eq!(Backend::from_env(), None);

        set("DISPLAY", Some(":0"));
        assert_eq!(Backend::from_env(), Some(Backend::X11));

        // A Wayland session almost always has an Xwayland too, and preferring
        // X11 there would pick the emulation over the real thing.
        set("WAYLAND_DISPLAY", Some("wayland-1"));
        assert_eq!(Backend::from_env(), Some(Backend::Wayland));

        set("SKERRY_BACKEND", Some("x11"));
        assert_eq!(Backend::from_env(), Some(Backend::X11));
        set("SKERRY_BACKEND", Some("wayland"));
        assert_eq!(Backend::from_env(), Some(Backend::Wayland));

        // An unrecognised value falls through rather than failing: a typo in a
        // shell profile should not make the program refuse to start.
        set("SKERRY_BACKEND", Some("mir"));
        assert_eq!(Backend::from_env(), Some(Backend::Wayland));

        for (name, value) in restore {
            set(name, value.as_deref().and_then(|value| value.to_str()));
        }
    }
}
