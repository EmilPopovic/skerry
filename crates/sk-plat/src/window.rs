//! What a window is, in terms neither display server owns.
//!
//! These types were `wayland`'s until there was a second backend. They are here
//! rather than there because every one of them is a question both servers answer
//! and neither defines — what the window should look like when it opens, what it
//! turned out to be able to do, what the pointer should be over a given thing.
//!
//! [ADR 0009](../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md)
//! is why the seam is an enum and not a trait, and why `Capabilities` keeps
//! fields one backend always answers the same way rather than hiding them behind
//! an `Option` nobody can interpret.

use std::ffi::c_void;

/// How the window should first appear.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    /// The title a desktop shows in a task switcher.
    pub title: String,
    /// The `app_id`, which is how a desktop matches a window to its
    /// `.desktop` file. Reverse-DNS by freedesktop convention.
    ///
    /// On X11 this is the second half of `WM_CLASS`, and the first half is this
    /// with its leading component capitalised — which is what every X11 window
    /// manager has matched on since ICCCM.
    pub app_id: String,
    /// The size in logical pixels, used until the display server names one.
    pub logical: (u32, u32),
    /// The scale to use, as a numerator over 120, where one is being forced.
    ///
    /// `None` is the ordinary case: the display server states it, and on X11
    /// that means `Xft.dpi`. `Some` is `--scale` on the command line, which
    /// exists because X11 has no protocol for a client to be *told* a scale and
    /// a bare `Xvfb` has no resource database to read one from — so without this
    /// there is no way to test 125% headlessly at all.
    pub scale_120: Option<u32>,
    /// Draw the frame here rather than asking the display server to.
    ///
    /// On Wayland, two things, because the protocol only offers one of them: the
    /// decoration mode asked for becomes `client_side`, **and** the frame is
    /// drawn whatever the compositor answers. A compositor may refuse the
    /// request — Hyprland does, measured — and a preference that a compositor
    /// can silently veto is not a preference. Where it refuses, a person who
    /// asked for this gets two frames, which is what asking for it means.
    ///
    /// GNOME advertises no decoration manager at all, so there the frame is
    /// drawn regardless and this flag changes nothing.
    ///
    /// On X11 the window manager decorates, and asking it not to is
    /// `_MOTIF_WM_HINTS` — which every reparenting window manager still honours
    /// and which is the only thing that has ever expressed this on X11.
    pub own_frame: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: sk_model::t!("app.name").to_owned(),
            app_id: "dev.skerry.Skerry".to_owned(),
            logical: (1100, 720),
            scale_120: None,
            own_frame: false,
        }
    }
}

/// What this display server turned out to support.
///
/// Probed, never compiled in — [ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md).
/// Recorded as a value rather than checked at each use so that a missing
/// capability is a fact the log carries, not a branch nobody can see.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    /// Whether a non-integer scale can be expressed exactly.
    ///
    /// `wp_fractional_scale_v1` on Wayland. Without it, scale is an integer and
    /// text at 125% is either too small or resampled. Always true on X11, where
    /// nothing between this process and the screen resamples anything — the
    /// buffer is the size it is drawn at.
    pub fractional_scale: bool,
    /// `wp_viewporter`, required on Wayland to express a fractional scale at
    /// all: it maps a physical-size buffer onto a logical-size surface. X11 has
    /// no such indirection and reports true.
    pub viewporter: bool,
    /// Whether the display server draws the frame.
    ///
    /// `zxdg_decoration_manager_v1` and the compositor's answer on Wayland; on
    /// X11, whether a window manager is running at all — `_NET_SUPPORTING_WM_CHECK`
    /// — because without one nothing will decorate the window and the frame has
    /// to be drawn here.
    pub server_side_decorations: bool,
    /// A keyboard is present.
    pub keyboard: bool,
    /// A pointer is present.
    pub pointer: bool,
    /// Whether the pointer can be given an image at all.
    ///
    /// `wp_cursor_shape_v1` on Wayland. On X11 this is whether a cursor theme
    /// resolved; false means the core cursor font is in use, which is a set of
    /// bitmaps from 1987 and worth knowing about when a screenshot looks wrong.
    pub cursor_shape: bool,
    /// Whether touchpad swipe and pinch gestures arrive.
    ///
    /// `zwp_pointer_gestures_v1` on Wayland. **Always false on X11**, where the
    /// equivalent is `XInput` 2.4's gesture events — which need the whole `XInput`
    /// event pipeline, and this backend reads core events. Reported rather than
    /// worked around: a person on X11 gets scroll and no pinch, and that is a
    /// thing to be able to read off one line of a bug report rather than to
    /// discover.
    pub gestures: bool,
    /// Whether an input method can reach this window.
    ///
    /// `zwp_text_input_manager_v3` on Wayland, which is what a Chinese, Japanese
    /// or Korean input method needs — those are separate processes and the
    /// compositor is the go-between. **False on X11**, where the equivalent is
    /// XIM or ibus over D-Bus and neither is written. Dead keys and Compose
    /// sequences work either way and are not this: they need no protocol at all.
    pub input_method: bool,
    /// The clipboard: `wl_data_device_manager`, or an X11 server, which has
    /// always had selections.
    pub clipboard: bool,
    /// The middle-click selection.
    ///
    /// On Wayland, `zwp_primary_selection_device_manager_v1` — and present here
    /// does **not** mean usable: Hyprland advertises it and silently discards
    /// `set_selection` unless `misc:middle_click_paste` is on, which Spike D
    /// found by elimination because nothing reports it. On X11 it is the
    /// `PRIMARY` selection, which is where the idea came from.
    pub primary_selection: bool,
    /// The DRM device the display server renders on, as a `dev_t`.
    ///
    /// From `zwp_linux_dmabuf_v1`'s default feedback. It exists because a hybrid
    /// laptop has two, and a buffer allocated on the wrong one cannot be
    /// imported: measured, mutter picked the discrete NVIDIA card, the drawing
    /// stack picked the integrated AMD one for its low-power preference, and the
    /// compositor answered `failed to import supplied dmabufs` and killed the
    /// connection.
    ///
    /// `None` on X11: the equivalent answer is DRI3's `open` reply, which is a
    /// file descriptor rather than a device number, and nothing has yet needed
    /// it there.
    pub render_device: Option<u64>,
    /// Every advertised protocol with its version, so "why did that not work"
    /// has an answer that does not need a second run.
    ///
    /// Wayland globals on one backend, X11 extensions with their major version
    /// on the other.
    pub advertised: Vec<(String, u32)>,
}

impl Capabilities {
    /// The PCI vendor and device ids of the display server's render device.
    ///
    /// Resolved through `/sys/dev/char`, which is where a `dev_t` becomes a
    /// device one can ask questions about. `None` when there is no feedback to
    /// read, when the node has no PCI parent — a virtual or USB display — or when
    /// the files are not there, all of which are "no opinion" rather than errors:
    /// the caller then chooses an adapter the way it always did.
    #[must_use]
    pub fn render_device_ids(&self) -> Option<(u32, u32)> {
        let (major, minor) = split_dev(self.render_device?);
        let base = std::path::PathBuf::from(format!("/sys/dev/char/{major}:{minor}/device"));
        let read = |name: &str| -> Option<u32> {
            let text = std::fs::read_to_string(base.join(name)).ok()?;
            u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
        };
        Some((read("vendor")?, read("device")?))
    }
}

const fn split_dev(device: u64) -> (u64, u64) {
    (
        ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff),
        (device & 0xff) | ((device >> 12) & !0xff),
    )
}

/// What the pointer should look like over a particular thing.
///
/// A named shape rather than an image: `wp_cursor_shape_v1` hands the name to
/// the compositor, which owns the theme, the size and the scale — all three of
/// which are settings a person has already chosen once and should not choose
/// again here.
///
/// X11 has no such protocol, so the name is resolved against the theme the X
/// resource database points at and the pixels are uploaded. That is the cursor
/// library this enum was shaped to avoid, and on X11 there is no alternative to
/// it that does not look like 1987.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Cursor {
    /// The ordinary arrow.
    #[default]
    Arrow,
    /// Over something that acts on a click: a breadcrumb, a column header.
    Hand,
    /// Over text a click would put a caret into.
    Text,
    /// Over a resize edge, named by the edge.
    Resize(Edge),
    /// Over a divider between two panes, which moves along one axis only.
    ///
    /// Not `Resize(Left)`: an edge cursor points the one way the edge can go,
    /// and a divider goes both. `col-resize` and `row-resize` are the shapes
    /// every toolkit puts on a splitter, and the fallbacks are what a theme
    /// older than the CSS names has.
    Divider {
        /// Whether it moves left and right rather than up and down.
        horizontal: bool,
    },
    /// Dragging something that follows the pointer.
    Grabbing,
    /// Carrying files that a drop would put somewhere.
    ///
    /// Not [`Cursor::Grabbing`]: that is a thing being moved *within* the window
    /// — a divider, a scrollbar thumb — and this is a payload looking for a
    /// destination. Every theme has drawn them differently since before the CSS
    /// names, which is the reason they are two shapes here.
    Dragging,
}

impl Cursor {
    /// The freedesktop cursor-spec names for this shape, best first.
    ///
    /// More than one because themes disagree: the CSS name `pointer` is
    /// `hand2` in every theme older than the spec, and a theme that has neither
    /// is why the last entry is always something ancient.
    #[must_use]
    pub const fn theme_names(self) -> &'static [&'static str] {
        match self {
            Self::Arrow => &["default", "left_ptr", "arrow"],
            Self::Hand => &["pointer", "hand2", "hand1"],
            Self::Text => &["text", "xterm", "ibeam"],
            Self::Grabbing => &["grabbing", "closedhand", "hand2"],
            Self::Dragging => &["dnd-move", "move", "copy", "dnd-copy", "grabbing"],
            Self::Divider { horizontal: true } => &["col-resize", "sb_h_double_arrow", "e-resize"],
            Self::Divider { horizontal: false } => &["row-resize", "sb_v_double_arrow", "s-resize"],
            Self::Resize(edge) => edge.theme_names(),
        }
    }
}

/// Which edge or corner a resize drag is pulling.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[allow(missing_docs, reason = "each variant is the edge of that name")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Edge {
    /// The freedesktop cursor-spec names for this edge, best first.
    #[must_use]
    pub const fn theme_names(self) -> &'static [&'static str] {
        match self {
            Self::Top => &["n-resize", "top_side"],
            Self::Bottom => &["s-resize", "bottom_side"],
            Self::Left => &["w-resize", "left_side"],
            Self::Right => &["e-resize", "right_side"],
            Self::TopLeft => &["nw-resize", "top_left_corner"],
            Self::TopRight => &["ne-resize", "top_right_corner"],
            Self::BottomLeft => &["sw-resize", "bottom_left_corner"],
            Self::BottomRight => &["se-resize", "bottom_right_corner"],
        }
    }
}

/// What the display server last said about the window.
///
/// Read rather than assumed: a maximized or tiled window must not draw a rounded
/// corner over the screen edge, and a window that is not activated draws its
/// title bar dimmed the way every other window on the desktop does.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct WindowState {
    /// Filling its output.
    pub maximized: bool,
    /// Filling its output with no chrome at all.
    pub fullscreen: bool,
    /// Holding keyboard focus, as far as the shell is concerned.
    pub activated: bool,
    /// Snapped against at least one edge, so its corners are not free.
    pub tiled: bool,
    /// Whether the display server offers a maximize action at all.
    pub can_maximize: bool,
    /// Whether it offers a minimize action.
    pub can_minimize: bool,
    /// Whether it offers a window menu.
    ///
    /// Always false on X11: `xdg_toplevel.show_window_menu` has no EWMH
    /// equivalent, and the `_GTK_SHOW_WINDOW_MENU` that GTK uses instead is
    /// answered by mutter alone. Reported absent rather than faked — see
    /// [ADR 0009](../../../docs/adr/0009-x11-backend-behind-a-runtime-selected-enum.md).
    pub can_show_menu: bool,
}

impl WindowState {
    /// What to assume until a display server says otherwise.
    ///
    /// All three actions present: `wm_capabilities` is `xdg_wm_base` v5, and on
    /// anything older the actions simply exist — which is what every compositor
    /// before v5 meant by not saying.
    pub(crate) const fn before_any_configure() -> Self {
        Self {
            maximized: false,
            fullscreen: false,
            activated: false,
            tiled: false,
            can_maximize: true,
            can_minimize: true,
            can_show_menu: true,
        }
    }
}

/// The display-server objects a GPU backend needs, named per server.
///
/// Raw rather than `raw-window-handle` types on purpose: it keeps that crate
/// inside the GPU stack, where its budget is already accounted for, and it keeps
/// `sk-plat` free of anything that knows what drawing is. The renderer converts
/// them; `tools/check-invariants.py` refuses to let the word for what it converts
/// them into appear in this crate at all.
#[derive(Clone, Copy, Debug)]
pub enum RawHandles {
    /// `*mut wl_display` and `*mut wl_surface`.
    Wayland {
        /// `*mut wl_display`.
        display: *mut c_void,
        /// `*mut wl_surface`.
        surface: *mut c_void,
    },
    /// An `xcb_connection_t *`, the screen it belongs to, and a window id.
    ///
    /// `xcb` rather than `Xlib` because that is what `x11rb` holds and what
    /// `VK_KHR_xcb_surface` takes; going through `Xlib` would mean a second
    /// library loaded to hand back a pointer we already have.
    X11 {
        /// `*mut xcb_connection_t`.
        connection: *mut c_void,
        /// Which screen of that connection.
        screen: i32,
        /// The window id. Never zero for a created window.
        window: u32,
        /// The visual the window was created with.
        ///
        /// Vulkan picks one itself if this is absent, and on a server offering
        /// more than one visual of the same depth it can pick a different one
        /// from the window's — which is a `BadMatch` at surface creation.
        visual: u32,
    },
}

/// The physical pixel size of a logical size at a scale numerator over 120.
///
/// Shared by both backends because it is the one arithmetic that must not differ
/// between them: everything above `sk-plat` works in logical pixels, everything
/// below the `Painter` works in device pixels, and this is the conversion.
#[must_use]
pub(crate) fn physical_of(logical: (u32, u32), scale_120: u32) -> (u32, u32) {
    ((logical.0 * scale_120).div_ceil(120).max(1), (logical.1 * scale_120).div_ceil(120).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number that turns "which GPU is the compositor on" into a path under
    /// `/sys/dev/char`. A DRM render node is major 226; `renderD128` is minor
    /// 128 and `renderD129` is 129, which is the pair this had to tell apart.
    #[test]
    fn a_dev_t_splits_the_way_glibc_packed_it() {
        assert_eq!(split_dev(226 * 256 + 128), (226, 128));
        assert_eq!(split_dev(226 * 256 + 129), (226, 129));
        assert_eq!(split_dev(0), (0, 0));
        // A minor above 255, which is where the second field lives and where a
        // naive `dev & 0xff` loses it.
        let wide = (226u64 << 8) | (0x1_00u64 << 12) | 0x34;
        assert_eq!(split_dev(wide), (226, 0x134));
    }

    #[test]
    fn the_physical_size_is_derived_from_the_integer_the_server_sent() {
        // 125% of 1100x720. Exact, because the scale never becomes a float.
        assert_eq!(physical_of((1100, 720), 150), (1375, 900));
        // 110%, which is not representable as a ratio of small integers and is
        // exactly why the numerator is kept rather than a factor.
        assert_eq!(physical_of((1100, 720), 132), (1210, 792));
        assert_eq!(physical_of((1100, 720), 120), (1100, 720));
    }

    #[test]
    fn a_fractional_scale_rounds_up_so_no_row_is_half_covered() {
        assert_eq!(physical_of((901, 521), 150), (1127, 652));
    }

    #[test]
    fn a_surface_is_never_zero_sized() {
        assert_eq!(physical_of((0, 0), 120), (1, 1));
    }

    #[test]
    fn every_cursor_offers_a_name_old_themes_have() {
        for cursor in [
            Cursor::Arrow,
            Cursor::Hand,
            Cursor::Text,
            Cursor::Grabbing,
            Cursor::Resize(Edge::TopLeft),
        ] {
            let names = cursor.theme_names();
            assert!(names.len() >= 2, "{cursor:?} offers only {names:?}");
            assert!(names.iter().all(|name| !name.is_empty()));
        }
    }
}
