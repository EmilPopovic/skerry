// SPDX-License-Identifier: Apache-2.0
//! Every atom this backend interns, in one round trip.
//!
//! Interning is a request per name and a reply per request. Asking for
//! thirty-odd names one at a time is thirty round trips before the window is
//! mapped, which on a remote display is thirty times the latency of the link.
//! `x11rb`'s `atom_manager!` sends them all and then collects the replies, which
//! is one.

x11rb::atom_manager! {
    /// The atoms, interned together.
    pub Atoms: AtomsCookie {
        // ICCCM: what every window manager since 1989 reads.
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TAKE_FOCUS,
        WM_STATE,
        WM_CHANGE_STATE,
        WM_CLIENT_MACHINE,

        // EWMH: what a modern one reads instead.
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_FOCUSED,
        _NET_WM_MOVERESIZE,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_SUPPORTING_WM_CHECK,
        _NET_FRAME_EXTENTS,
        _NET_ACTIVE_WINDOW,

        // The frame. `_MOTIF_WM_HINTS` is the only thing that has ever expressed
        // "do not decorate this" on X11, and every reparenting window manager
        // still honours it.
        _MOTIF_WM_HINTS,

        // Scale and keyboard, both read off the root window.
        RESOURCE_MANAGER,
        _XKB_RULES_NAMES,

        // XDND, which is a convention made of client messages and properties.
        XdndAware,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndTypeList,
        XdndActionCopy,
        XdndActionMove,
        XdndActionLink,
        XdndActionPrivate,

        // Selections.
        CLIPBOARD,
        TARGETS,
        MULTIPLE,
        TIMESTAMP,
        SAVE_TARGETS,
        INCR,
        UTF8_STRING,
        // Ours: where conversion results are deposited on our own window, one
        // property per conversion in flight. See `Selections::free_property`.
        SKERRY_SELECTION_0,
        SKERRY_SELECTION_1,
        SKERRY_SELECTION_2,
        SKERRY_SELECTION_3,
        SKERRY_SELECTION_4,
        SKERRY_SELECTION_5,
        SKERRY_SELECTION_6,
        SKERRY_SELECTION_7,

        // The types a file manager publishes and reads.
        text_uri_list: b"text/uri-list",
        text_plain_utf8: b"text/plain;charset=utf-8",
        text_plain: b"text/plain",
        gnome_copied_files: b"x-special/gnome-copied-files",
        kde_cutselection: b"application/x-kde-cutselection",
    }
}

impl Atoms {
    /// The properties a conversion may land in on our own window.
    pub(super) const fn conversions(&self) -> [x11rb::protocol::xproto::Atom; 8] {
        [
            self.SKERRY_SELECTION_0,
            self.SKERRY_SELECTION_1,
            self.SKERRY_SELECTION_2,
            self.SKERRY_SELECTION_3,
            self.SKERRY_SELECTION_4,
            self.SKERRY_SELECTION_5,
            self.SKERRY_SELECTION_6,
            self.SKERRY_SELECTION_7,
        ]
    }
}
