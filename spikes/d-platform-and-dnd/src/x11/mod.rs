//! The X11 arm: `x11rb`, ICCCM selections and XDND, with no toolkit above them.
//!
//! Under this session X11 clients are Xwayland clients, which makes every check
//! here test two things at once — our own protocol handling, and the compositor's
//! X11-to-Wayland bridge. That is the right pairing: a user dragging from a
//! legacy application into a Wayland one is exercising exactly that path, and it
//! is the path that most often silently does nothing.

pub mod checks;
pub mod sel;
pub mod xdnd;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Window, WindowClass,
};
use std::cell::RefCell;
use std::collections::VecDeque;

use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{atom_manager, COPY_DEPTH_FROM_PARENT};

use crate::report::Report;

atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        MULTIPLE,
        TIMESTAMP,
        INCR,
        UTF8_STRING,
        TEXT,
        STRING,
        _SKERRY_SELECTION_IN,
        _SKERRY_SPIKE_D,

        // The minimum a window manager expects before it will treat a window as
        // an ordinary application window. Setting only WM_NAME produces a window
        // that maps and is viewable and that the compositor still leaves out of
        // its client list — which is enough to make the X11 clipboard bridge
        // ignore it entirely.
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,

        // XDND, version 5.
        XdndAware,
        XdndTypeList,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndActionCopy,
        XdndActionMove,
        XdndActionLink,
        XdndActionAsk,
        XdndActionPrivate,
        XdndActionList,
        XdndProxy,

        // The MIME types a file manager trades in. On X11 these are atoms with
        // the same spelling as the Wayland MIME strings, which is the one place
        // the two protocols agree.
        text_uri_list: b"text/uri-list",
        text_plain_utf8: b"text/plain;charset=utf-8",
        gnome_copied_files: b"x-special/gnome-copied-files",
    }
}

/// XDND version we implement and advertise. 5 is what GTK and Qt both speak.
pub const XDND_VERSION: u32 = 5;

pub struct X11 {
    pub conn: RustConnection,
    pub screen: usize,
    pub root: Window,
    pub window: Window,
    pub atoms: Atoms,
    pub report: Report,
    pub width: u16,
    pub height: u16,
    /// Events polled by one helper that another helper needed. X11 has a single
    /// event stream and several concurrent conversations on it — a selection
    /// transfer inside a drag, for instance — so anything that polls has to be
    /// able to put back what is not its own. The first version of this file did
    /// not, and it silently ate every `SelectionRequest` that arrived while a
    /// timestamp was being fetched.
    deferred: RefCell<VecDeque<x11rb::protocol::Event>>,
}

impl X11 {
    pub fn open(title: &str) -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        let (root, width, height) = {
            let s = &conn.setup().roots[screen_num];
            (s.root, 480u16, 320u16)
        };
        let window = conn.generate_id().ok()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::PROPERTY_CHANGE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::KEY_PRESS,
            ),
        )
        .ok()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            Atom::from(AtomEnum::WM_NAME),
            Atom::from(AtomEnum::STRING),
            title.as_bytes(),
        )
        .ok()?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_NAME,
            atoms.UTF8_STRING,
            title.as_bytes(),
        )
        .ok()?;
        // Two NUL-terminated strings: instance then class. A missing WM_CLASS is
        // why a window manager cannot match a rule against the window, and why
        // this one was invisible to `hyprctl clients`.
        conn.change_property8(
            PropMode::REPLACE,
            window,
            Atom::from(AtomEnum::WM_CLASS),
            Atom::from(AtomEnum::STRING),
            b"spike-d\0Spike-D\0",
        )
        .ok()?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_WINDOW_TYPE,
            Atom::from(AtomEnum::ATOM),
            &[atoms._NET_WM_WINDOW_TYPE_NORMAL],
        )
        .ok()?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_PID,
            Atom::from(AtomEnum::CARDINAL),
            &[std::process::id()],
        )
        .ok()?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.WM_PROTOCOLS,
            Atom::from(AtomEnum::ATOM),
            &[atoms.WM_DELETE_WINDOW],
        )
        .ok()?;
        // Being a drag target at all is one property. Forgetting it is why a
        // window silently refuses every drop.
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.XdndAware,
            Atom::from(AtomEnum::ATOM),
            &[XDND_VERSION],
        )
        .ok()?;
        conn.map_window(window).ok()?;
        conn.flush().ok()?;
        Some(Self {
            conn,
            screen: screen_num,
            root,
            window,
            atoms,
            report: Report::default(),
            width,
            height,
            deferred: RefCell::new(VecDeque::new()),
        })
    }

    /// The next event, from the deferred queue first.
    pub fn poll(&self) -> Option<x11rb::protocol::Event> {
        if let Some(e) = self.deferred.borrow_mut().pop_front() {
            return Some(e);
        }
        self.conn.poll_for_event().ok().flatten()
    }

    pub fn defer(&self, e: x11rb::protocol::Event) {
        self.deferred.borrow_mut().push_back(e);
    }

    /// Forget queued `PropertyNotify` events for one property. Needed once, and
    /// for a reason worth stating: the owner's `INCR` header is itself a property
    /// write, so it produces a `PropertyNotify` that arrives before the transfer
    /// starts. A requestor that treats it as the first chunk reads the property it
    /// has just deleted, sees zero bytes, and concludes the transfer is over —
    /// which looks exactly like an owner that sent nothing.
    pub fn forget_property_notifies(&self, window: Window, atom: Atom) {
        self.deferred.borrow_mut().retain(|e| {
            !matches!(e, x11rb::protocol::Event::PropertyNotify(p)
                if p.window == window && p.atom == atom)
        });
    }

    pub fn atom_name(&self, atom: u32) -> String {
        self.conn
            .get_atom_name(atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).to_string())
            .unwrap_or_else(|| format!("atom {atom}"))
    }

    /// A timestamp from the server rather than `CURRENT_TIME`. ICCCM says an
    /// owner that uses `CURRENT_TIME` cannot tell whether it lost a race, and
    /// zero-length property appends are the standard way to get one.
    pub fn server_time(&self) -> u32 {
        let _ = self.conn.change_property8(
            PropMode::APPEND,
            self.window,
            self.atoms._SKERRY_SPIKE_D,
            Atom::from(AtomEnum::STRING),
            &[],
        );
        let _ = self.conn.flush();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut put_back = Vec::new();
        let mut time = x11rb::CURRENT_TIME;
        while std::time::Instant::now() < deadline {
            match self.poll() {
                Some(x11rb::protocol::Event::PropertyNotify(e))
                    if e.window == self.window && e.atom == self.atoms._SKERRY_SPIKE_D =>
                {
                    time = e.time;
                    break;
                }
                Some(other) => put_back.push(other),
                None => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
        for e in put_back {
            self.defer(e);
        }
        time
    }
}
