//! Where the regions are, in logical pixels.
//!
//! One function, computed once per frame, and every hit test reads it. The
//! alternative — each region computing its own rectangle at draw time and each
//! event handler recomputing it — is how a click lands one row off at 125%.

use sk_plat::{Edge, Size};
use sk_ui::Metrics;

/// A rectangle in logical pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Area {
    pub fn right(self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(self) -> f32 {
        self.y + self.height
    }

    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// The smallest rectangle holding both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }
}

/// Every region of the window.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    /// The window, in logical pixels. Kept because a region that has to stay
    /// inside the window — a context menu, a resize band — needs the whole of it
    /// and not a rectangle in it, and reconstructing it from the status strip is
    /// arithmetic that is right until the window is shorter than its own chrome.
    pub window: Size,
    /// The window's own title bar, and **zero height when it has none**.
    ///
    /// Only the frame the window draws for itself lives here: the three window
    /// buttons and the region a press drags the window by. Where the compositor
    /// draws the frame — every desktop measured except GNOME — there is nothing
    /// left for this strip to hold, and 44 logical pixels of empty chrome above
    /// a pane that carries its own path bar is 44 pixels of list given away for
    /// nothing. So it collapses, and the panes start at the top of the window.
    pub title: Area,
    /// The list of places down the left, or empty when it is closed or shed.
    ///
    /// Beside the panes and the full height of them, because the strip that
    /// spanned the window and pushed it down is gone: each pane says where it
    /// is now, so there is no window-wide address bar for this to sit under.
    pub sidebar: Area,
    /// Everything the pane tree divides between its leaves: below the title
    /// strip, right of the sidebar, above the status strip.
    ///
    /// Where the header, the list and the columns used to be — and the four
    /// navigation buttons and the breadcrumbs with them. They are per leaf
    /// ([ADR 0010](../adr/0010-a-split-tree-of-tab-strips.md)) and live in
    /// [`crate::panes::PaneLayout`].
    pub content: Area,
    /// The status strip.
    pub status: Area,

    /// The three window buttons — minimise, maximise, close, left to right.
    ///
    /// All empty when the compositor draws the frame, which is every desktop
    /// except GNOME.
    pub buttons: [Area; 3],
    /// Where a press starts moving the window.
    ///
    /// The title strip minus the buttons, and empty when the compositor draws
    /// the frame — there is no strip then to press.
    pub drag: Area,
    /// How far in from an edge a press starts a resize. Zero with server-side
    /// decorations, and zero when the window is maximized or tiled — its edges
    /// are not free to move then.
    pub resize_border: f32,
}

/// How wide the list's columns are, in logical pixels.
///
/// **`name` is a maximum; the other two are exact.** A filename is the only cell
/// whose useful length is unbounded, so the name is the column that takes what is
/// left over — and past a certain width it stops taking it, which is the whole
/// point: without a ceiling, a 2,800-pixel window puts a file's size fifteen
/// hundred pixels from its name and the eye has to cross a desert to pair them.
/// The other two columns then sit immediately after the name rather than against
/// the pane's right edge, and the space past them is empty.
///
/// **Not in [`Metrics`], where zoom scales every field.** These are what somebody
/// dragged the boundaries to, and a width that changed underneath them when they
/// pressed `Ctrl+=` would be a setting the program overruled.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Columns {
    /// The widest the name column is drawn.
    pub name: f32,
    /// The size column.
    pub size: f32,
    /// The modified column.
    pub modified: f32,
}

impl Columns {
    /// The narrowest a column may be dragged.
    ///
    /// Forty-eight logical pixels holds `9.9 MiB` and the header label above it.
    /// Narrower is a column whose every cell is an ellipsis, which is a column
    /// that has been turned off by a person who was trying to make it small.
    pub const LEAST: f32 = 48.0;

    /// The widest, which only the name is ever near.
    ///
    /// Nine hundred is about a hundred and forty characters at the body text
    /// size. Past that a name is not being read, it is being scrolled past, and
    /// the ceiling stops a drag off the edge of an ultrawide from parking every
    /// other column out of sight.
    pub const MOST: f32 = 900.0;

    /// The same widths, inside the range a drag is allowed to reach.
    ///
    /// Applied on the way in from the session file as well as on the way out of a
    /// drag: the file is a thing that can arrive corrupt, and a `name` of zero in
    /// it would be a list with no names in it and no way to get them back.
    #[must_use]
    pub fn clamped(self) -> Self {
        let hold = |width: f32| {
            if width.is_finite() { width.clamp(Self::LEAST, Self::MOST) } else { Self::LEAST }
        };
        Self { name: hold(self.name), size: hold(self.size), modified: hold(self.modified) }
    }
}

impl Default for Columns {
    fn default() -> Self {
        // 520 is about eighty characters at the 13 px body size, which is past
        // the long tail of real filenames — and it puts the size column a third of
        // the way across a wide pane instead of against its far edge. 96 holds
        // `123.4 MiB` and 128 holds `12 September 2025`.
        Self { name: 520.0, size: 96.0, modified: 128.0 }
    }
}

/// The icon, name, size and modified columns for a list of `body`'s width.
///
/// The two metadata columns keep their width and the name takes what is left up
/// to [`Columns::name`]. Below the width where the name would fall under a
/// hundred logical pixels the metadata columns give way instead, widest first.
pub(crate) fn columns(body: Area, metrics: &Metrics, widths: Columns) -> (Area, Area, Area, Area) {
    let gutter = metrics.gutter;
    // **The scrollbar's strip is not the columns' to use.** The thumb is drawn at
    // the pane's right edge, so a column reaching that edge is a column with a
    // scrollbar across it — which is what the selected row's focus ring looked
    // like before this line existed. The grid has always reserved it.
    let edge = body.right() - crate::cells::TRACK;
    let available = edge - body.x - gutter * 2.0;
    let mut size_width = widths.size;
    let mut modified_width = widths.modified;
    if available - metrics.icon_column - size_width - modified_width < 100.0 {
        modified_width = 0.0;
    }
    if available - metrics.icon_column - size_width < 100.0 {
        size_width = 0.0;
    }

    let icon = Area { x: body.x + gutter, y: 0.0, width: metrics.icon_column, height: 0.0 };
    let name_x = icon.right();
    let fits = (edge - gutter - size_width - modified_width - name_x).max(0.0);
    let name_width = fits.min(widths.name);
    (
        icon,
        Area { x: name_x, y: 0.0, width: name_width, height: 0.0 },
        Area { x: name_x + name_width, y: 0.0, width: size_width, height: 0.0 },
        Area { x: name_x + name_width + size_width, y: 0.0, width: modified_width, height: 0.0 },
    )
}

/// Whether the window draws its own frame, and what belongs in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Frame {
    /// The compositor draws it. Every desktop measured except GNOME.
    #[default]
    None,
    /// Ours.
    Own {
        /// Whether the edges can be pulled. False when the window is maximized,
        /// tiled or fullscreen, where its edges are not free to move.
        edges: bool,
        /// Whether the compositor offers a minimise action at all. Hyprland does
        /// not, measured, and a button that does nothing is worse than no button.
        minimize: bool,
        /// Whether it offers a maximise action.
        maximize: bool,
    },
}

impl Frame {
    /// Whether this window draws its own frame.
    #[must_use]
    pub const fn is_own(self) -> bool {
        matches!(self, Self::Own { .. })
    }

    /// Which of the three buttons belong in it, in layout order.
    ///
    /// Close is not optional: `xdg_toplevel.close` exists in every version of
    /// the protocol, and a window with no way to close it is not a window.
    const fn offered(self) -> [bool; 3] {
        match self {
            Self::None => [false, false, false],
            Self::Own { minimize, maximize, .. } => [minimize, maximize, true],
        }
    }
}

/// Which navigation button.
///
/// One per pane, in that pane's own bar. The enum stays here because it is the
/// map from a drawn button to a command, and [`crate::panes::PaneLayout`] is
/// where its four rectangles are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Where the last navigation came from.
    Back,
    /// Where Back came from.
    Forward,
    /// The containing directory.
    Parent,
    /// Read this one again.
    Reload,
}

impl Tool {
    /// The four, in the order they are laid out.
    pub const ALL: [Self; 4] = [Self::Back, Self::Forward, Self::Parent, Self::Reload];

    /// The command each one runs. Rule 2: the button and the chord are the same
    /// action, and the button's dimming comes from that command's `enabled`.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Back => "go.back",
            Self::Forward => "go.forward",
            Self::Parent => "go.parent",
            Self::Reload => "go.reload",
        }
    }
}

/// Which window button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    /// Minimise.
    Minimize,
    /// Maximise, or restore when the window is already maximized.
    Maximize,
    /// Close.
    Close,
}

impl Button {
    /// The three, in the order they are laid out.
    pub const ALL: [Self; 3] = [Self::Minimize, Self::Maximize, Self::Close];

    /// The command each one runs, for the same reason [`Tool::command`] exists:
    /// the button and the chord are one action, and a press goes through the
    /// registry so that a compositor which offers no minimise cannot be
    /// minimised by clicking where the button would have been.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Minimize => "window.minimize",
            Self::Maximize => "window.maximize",
            Self::Close => "window.close",
        }
    }
}

impl Layout {
    /// Computes every region for a window of `size`.
    ///
    /// Three strips and a middle: the title bar the window draws for itself, the
    /// status strip, the sidebar down the left, and everything the pane tree
    /// divides between its leaves. Where a pane's own parts go is
    /// [`crate::panes::PaneLayout`]'s answer, because a leaf is the thing that
    /// has a path, a history and a sort.
    #[must_use]
    pub fn compute_with(size: Size, metrics: &Metrics, frame: Frame, sidebar: bool) -> Self {
        let full = size.width.max(1.0);
        // Only the buttons the compositor actually offers, packed against the
        // right edge: a slot left empty for an absent action reads as a bug, and
        // a button that does nothing reads as a worse one.
        let offered = frame.offered();
        let strip = metrics.button_width * offered.iter().filter(|it| **it).count() as f32;
        let title = Area {
            x: 0.0,
            y: 0.0,
            width: full,
            height: if frame.is_own() { metrics.path_height } else { 0.0 },
        };
        let mut cursor = full - strip;
        let mut buttons = [Area::default(); 3];
        for (slot, offered) in buttons.iter_mut().zip(offered) {
            if !offered {
                continue;
            }
            *slot = Area { x: cursor, y: 0.0, width: metrics.button_width, height: title.height };
            cursor += metrics.button_width;
        }
        // The strip minus the buttons: a press on a button is a press on that
        // button, not the start of a window drag that happens to begin there.
        let drag = if frame.is_own() {
            Area { width: (full - strip).max(0.0), ..title }
        } else {
            Area::default()
        };
        // A maximized or tiled window has no free edges, so there is nothing to
        // grab — and an invisible six-pixel band that swallows clicks at the top
        // of a maximized list is worse than no resize at all.
        let resize_border = match frame {
            Frame::Own { edges: true, .. } => metrics.resize_border,
            _ => 0.0,
        };
        let status = Area {
            x: 0.0,
            y: (size.height - metrics.status_height).max(title.bottom() + metrics.header_height),
            width: full,
            height: metrics.status_height,
        };
        // Shed below the width where the list would be narrower than the
        // sidebar: a window too small for both is a window that came for the
        // list. The same rule the metadata columns follow.
        let aside = if sidebar && full - metrics.sidebar_width >= 320.0 {
            Area {
                x: 0.0,
                y: title.bottom(),
                width: metrics.sidebar_width,
                height: (status.y - title.bottom()).max(0.0),
            }
        } else {
            Area::default()
        };
        let content = Area {
            x: aside.width,
            y: title.bottom(),
            width: full - aside.width,
            height: (status.y - title.bottom()).max(0.0),
        };

        Self { window: size, title, sidebar: aside, content, status, buttons, drag, resize_border }
    }

    /// The window button at a point, if any.
    #[must_use]
    pub fn button_at(&self, x: f32, y: f32) -> Option<Button> {
        Button::ALL
            .into_iter()
            .zip(self.buttons)
            .find_map(|(button, area)| (area.width > 0.0 && area.contains(x, y)).then_some(button))
    }

    /// The resize edge at a point, if the window has free edges to pull.
    ///
    /// Corners win over sides: a diagonal drag is what a person is reaching for
    /// when they aim at a corner, and a band that reports `Right` there makes
    /// the corner unusable.
    #[must_use]
    pub fn edge_at(&self, x: f32, y: f32) -> Option<Edge> {
        let size = self.window;
        let band = self.resize_border;
        if band <= 0.0 {
            return None;
        }
        let left = x < band;
        let right = x >= size.width - band;
        let top = y < band;
        let bottom = y >= size.height - band;
        Some(match (top, bottom, left, right) {
            (true, _, true, _) => Edge::TopLeft,
            (true, _, _, true) => Edge::TopRight,
            (_, true, true, _) => Edge::BottomLeft,
            (_, true, _, true) => Edge::BottomRight,
            (true, ..) => Edge::Top,
            (_, true, ..) => Edge::Bottom,
            (_, _, true, _) => Edge::Left,
            (_, _, _, true) => Edge::Right,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Area, Button, Columns, Frame, Layout, Tool, columns};
    use sk_plat::{Edge, Size};
    use sk_ui::Metrics;

    /// The geometry of a window holding one pane, which is what every test here
    /// used to be able to read off the `Layout` itself.
    fn one_leaf(layout: &Layout, metrics: &Metrics) -> crate::panes::PaneLayout {
        let panes = crate::panes::Panes::new(crate::pane::Pane::new(std::path::PathBuf::from("/")));
        let (mut leaves, mut dividers) = (Vec::new(), Vec::new());
        panes.layout(layout.content, metrics, Columns::default(), &mut leaves, &mut dividers);
        leaves.first().copied().unwrap_or_default()
    }

    /// A compositor that offers no minimise must not leave a hole where the
    /// button would have been: the two that exist pack against the right edge,
    /// and the path bar gets the width the third one would have taken.
    #[test]
    fn only_the_buttons_the_compositor_offers_are_laid_out() {
        let metrics = Metrics::default();
        let size = Size::new(1100.0, 720.0);
        let all = Layout::compute_with(
            size,
            &metrics,
            Frame::Own { edges: true, minimize: true, maximize: true },
            false,
        );
        assert!(all.buttons.iter().all(|button| button.width > 0.0));
        assert!((all.buttons[2].right() - 1100.0).abs() < f32::EPSILON, "close is not at the edge");
        assert!((all.drag.right() - all.buttons[0].x).abs() < f32::EPSILON, "a gap before the row");

        let two = Layout::compute_with(
            size,
            &metrics,
            Frame::Own { edges: true, minimize: false, maximize: true },
            false,
        );
        assert!(two.buttons[0].width <= 0.0, "an absent action was given a slot");
        assert!((two.buttons[2].right() - 1100.0).abs() < f32::EPSILON);
        assert!((two.buttons[1].right() - two.buttons[2].x).abs() < f32::EPSILON, "a hole");
        assert!(two.drag.width > all.drag.width, "the drag region did not take the freed width");

        // **And a compositor-drawn frame leaves no strip at all.** A title bar
        // with nothing in it above a pane that carries its own path is 44
        // logical pixels of list given away for nothing.
        let served = Layout::compute_with(size, &metrics, Frame::None, false);
        assert!(served.buttons.iter().all(|button| button.width <= 0.0));
        assert!(served.drag.width <= 0.0, "a compositor-drawn title bar is not ours to drag");
        assert!(served.title.height <= 0.0, "an empty title strip took height");
        assert!(served.content.y <= f32::EPSILON, "the panes did not start at the top");
        assert!(served.content.height > all.content.height, "the freed height went nowhere");
    }

    /// The sidebar takes the left of everything below the title strip, the
    /// panes start where it ends, and both shed at the width where the list
    /// would be narrower than the sidebar is wide.
    #[test]
    fn the_sidebar_takes_the_left_and_the_list_starts_where_it_ends() {
        let metrics = Metrics::default();
        let open = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, true);
        assert!((open.sidebar.width - metrics.sidebar_width).abs() < f32::EPSILON);
        assert!(open.sidebar.x <= f32::EPSILON);
        // Below the title strip, down to the status strip, with no gap at either.
        assert!((open.sidebar.y - open.title.bottom()).abs() < f32::EPSILON);
        assert!((open.sidebar.bottom() - open.status.y).abs() < f32::EPSILON);
        // And the panes start after it. The columns are the tree's now, so the
        // question this test asks of them is asked through one leaf's layout.
        assert!((open.content.x - open.sidebar.right()).abs() < f32::EPSILON);
        assert!((open.content.right() - 1100.0).abs() < f32::EPSILON);
        let open_pane = one_leaf(&open, &metrics);
        assert!(open_pane.icon.x > open.sidebar.right(), "a row's mark draws under the sidebar");
        assert!((open_pane.body.right() - 1100.0).abs() < f32::EPSILON);

        let closed = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, false);
        assert!(closed.sidebar.width <= 0.0);
        assert!(closed.content.x <= f32::EPSILON);
        let closed_pane = one_leaf(&closed, &metrics);
        assert!(
            closed_pane.name.x < open_pane.name.x,
            "the name column did not take the freed width"
        );

        // Narrow enough that the list would be smaller than the sidebar.
        let narrow = Layout::compute_with(Size::new(500.0, 720.0), &metrics, Frame::None, true);
        assert!(narrow.sidebar.width <= 0.0, "the sidebar did not shed");
        assert!((narrow.content.width - 500.0).abs() < f32::EPSILON);
    }

    /// Corners win over sides, or a diagonal drag is impossible: the band is six
    /// logical pixels and the corner is where two of them cross.
    #[test]
    fn a_corner_is_a_corner_and_not_whichever_side_was_checked_first() {
        let size = Size::new(800.0, 600.0);
        let layout = Layout::compute_with(
            size,
            &Metrics::default(),
            Frame::Own { edges: true, minimize: true, maximize: true },
            false,
        );
        assert_eq!(layout.edge_at(1.0, 1.0), Some(Edge::TopLeft));
        assert_eq!(layout.edge_at(799.0, 1.0), Some(Edge::TopRight));
        assert_eq!(layout.edge_at(1.0, 599.0), Some(Edge::BottomLeft));
        assert_eq!(layout.edge_at(799.0, 599.0), Some(Edge::BottomRight));
        assert_eq!(layout.edge_at(400.0, 1.0), Some(Edge::Top));
        assert_eq!(layout.edge_at(400.0, 599.0), Some(Edge::Bottom));
        assert_eq!(layout.edge_at(1.0, 300.0), Some(Edge::Left));
        assert_eq!(layout.edge_at(799.0, 300.0), Some(Edge::Right));
        assert_eq!(layout.edge_at(400.0, 300.0), None);
    }

    /// A maximized or tiled window has no edges to pull, and an invisible band
    /// that swallows clicks at the top of a maximized list is worse than no
    /// resize at all.
    #[test]
    fn a_window_with_no_free_edges_has_no_resize_band() {
        let size = Size::new(800.0, 600.0);
        let fixed = Layout::compute_with(
            size,
            &Metrics::default(),
            Frame::Own { edges: false, minimize: true, maximize: true },
            false,
        );
        assert!(fixed.resize_border <= 0.0);
        assert_eq!(fixed.edge_at(1.0, 1.0), None);
        assert!(fixed.buttons[2].width > 0.0, "a tiled window still has to close");
    }

    /// The four commands the buttons stand for, which is all this file still
    /// decides about them: where they are drawn is a leaf's question now, and
    /// `panes.rs` asks it.
    #[test]
    fn each_navigation_button_names_the_command_it_runs() {
        let commands: Vec<_> = Tool::ALL.into_iter().map(Tool::command).collect();
        assert_eq!(commands, vec!["go.back", "go.forward", "go.parent", "go.reload"]);
    }

    /// And the buttons are found by the same rectangles they are drawn from.
    #[test]
    fn a_press_on_a_button_finds_that_button() {
        let layout = Layout::compute_with(
            Size::new(1100.0, 720.0),
            &Metrics::default(),
            Frame::Own { edges: true, minimize: true, maximize: true },
            false,
        );
        for (button, area) in Button::ALL.into_iter().zip(layout.buttons) {
            let (x, y) = (area.x + area.width / 2.0, area.y + area.height / 2.0);
            assert_eq!(layout.button_at(x, y), Some(button));
        }
        assert_eq!(layout.button_at(10.0, 10.0), None, "the path bar is not a button");
    }

    #[test]
    fn the_regions_tile_the_window_with_no_gap_and_no_overlap() {
        let metrics = Metrics::default();
        let layout = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, false);
        assert!((layout.title.bottom() - layout.content.y).abs() < f32::EPSILON);
        assert!((layout.content.bottom() - layout.status.y).abs() < f32::EPSILON);
        assert!((layout.status.bottom() - 720.0).abs() < f32::EPSILON);
        // And inside the content, the one leaf tiles it in turn: the tab strip
        // first, then its own path bar, then the column header, then the list.
        let pane = one_leaf(&layout, &metrics);
        assert!((pane.tabs.y - layout.content.y).abs() < f32::EPSILON);
        assert!((pane.tabs.bottom() - pane.bar.y).abs() < f32::EPSILON);
        assert!((pane.bar.bottom() - pane.header.y).abs() < f32::EPSILON);
        assert!((pane.header.bottom() - pane.body.y).abs() < f32::EPSILON);
        assert!((pane.body.bottom() - layout.content.bottom()).abs() < f32::EPSILON);
    }

    /// The four columns touch, in order, whatever the width.
    #[test]
    fn the_columns_pack_from_the_left_with_no_gap_between_them() {
        let metrics = Metrics::default();
        for width in [420.0, 700.0, 1100.0, 2878.0] {
            let window =
                Layout::compute_with(Size::new(width, 720.0), &metrics, Frame::None, false);
            let layout = one_leaf(&window, &metrics);
            assert!((layout.icon.right() - layout.name.x).abs() < f32::EPSILON, "at {width}");
            assert!((layout.name.right() - layout.size.x).abs() < f32::EPSILON, "at {width}");
            assert!((layout.size.right() - layout.modified.x).abs() < f32::EPSILON, "at {width}");
        }
    }

    /// **The whole reason there is a ceiling.** Without one, a 2,878-pixel window
    /// gave the name column two thousand of them and put every file's size at the
    /// far right of the screen, a screen-width away from the name it belonged to.
    #[test]
    fn an_ultrawide_pane_stops_the_name_at_its_ceiling() {
        let metrics = Metrics::default();
        let widths = Columns::default();
        let window = Layout::compute_with(Size::new(2878.0, 720.0), &metrics, Frame::None, false);
        let layout = one_leaf(&window, &metrics);
        assert!((layout.name.width - widths.name).abs() < f32::EPSILON, "{}", layout.name.width);
        // And the space past the last column is empty rather than shared out.
        let past = 2878.0 - metrics.gutter - layout.modified.right();
        assert!(past > 1_900.0, "only {past} left over");
    }

    /// Under the ceiling the name still takes everything going, because a pane
    /// this size has nothing to spare and a ceiling is not a target.
    #[test]
    fn a_pane_narrower_than_the_ceiling_gives_the_name_what_is_left() {
        let metrics = Metrics::default();
        let window = Layout::compute_with(Size::new(700.0, 720.0), &metrics, Frame::None, false);
        let layout = one_leaf(&window, &metrics);
        assert!(layout.name.width < Columns::default().name, "{}", layout.name.width);
        // Everything but the gutter and the scrollbar's strip, which the columns
        // never get: a column reaching the pane's edge is a column with a thumb
        // drawn across it.
        let edge = 700.0 - metrics.gutter - crate::cells::TRACK;
        assert!(
            (layout.modified.right() - edge).abs() < f32::EPSILON,
            "{}",
            layout.modified.right()
        );
    }

    /// **The boundary a drag lets go of is the boundary the next frame draws.**
    ///
    /// `App::drag_the_column` sets a width to the distance from the column's own
    /// left edge to the pointer, and this is the other half of that round trip:
    /// the line comes back at the point it was dropped on, and the columns right
    /// of it follow. A drag that set the width from the *pane's* left edge, or
    /// from the previous boundary, would put the line somewhere near the pointer
    /// at one window width and nowhere near it at another.
    #[test]
    fn a_boundary_dragged_to_a_point_comes_back_at_that_point() {
        let metrics = Metrics::default();
        let body = Area { x: 40.0, y: 0.0, width: 1600.0, height: 500.0 };
        let widths = Columns::default();
        let (_, name, ..) = columns(body, &metrics, widths);

        for dropped_on in [420.0, 300.0, 700.0] {
            let dragged = Columns { name: dropped_on - name.x, ..widths }.clamped();
            let (_, moved, size, modified) = columns(body, &metrics, dragged);
            assert!(
                (moved.right() - dropped_on).abs() < f32::EPSILON,
                "dropped on {dropped_on}, landed at {}",
                moved.right()
            );
            assert!((size.x - dropped_on).abs() < f32::EPSILON, "the size column did not follow");
            assert!((modified.x - size.right()).abs() < f32::EPSILON);
        }
    }

    /// A width off the end of a drag, or out of a file that arrived corrupt, is
    /// held inside the range rather than refused: a `name` of zero is a list with
    /// no names in it and no boundary left to drag back.
    #[test]
    fn a_width_outside_the_range_is_held_inside_it() {
        let wild = Columns { name: 40_000.0, size: 0.0, modified: f32::NAN }.clamped();
        assert!((wild.name - Columns::MOST).abs() < f32::EPSILON);
        assert!((wild.size - Columns::LEAST).abs() < f32::EPSILON);
        assert!((wild.modified - Columns::LEAST).abs() < f32::EPSILON, "NaN fell through");
        let kept = Columns { name: 300.0, size: 80.0, modified: 100.0 };
        assert_eq!(kept.clamped(), kept, "a width already inside the range is untouched");
    }

    #[test]
    fn a_narrow_window_sheds_metadata_columns_rather_than_the_name() {
        let metrics = Metrics::default();
        let at = |width: f32| {
            let window =
                Layout::compute_with(Size::new(width, 720.0), &metrics, Frame::None, false);
            one_leaf(&window, &metrics)
        };
        let wide = at(1100.0);
        assert!(wide.modified.width > 0.0 && wide.size.width > 0.0);

        let medium = at(320.0);
        assert!(medium.modified.width.abs() < f32::EPSILON, "the widest column goes first");
        assert!(medium.name.width >= 100.0);

        let narrow = at(200.0);
        assert!(narrow.size.width.abs() < f32::EPSILON);
        assert!(narrow.name.width > 0.0, "the name never disappears");
    }

    #[test]
    fn a_window_shorter_than_its_chrome_still_produces_a_valid_layout() {
        let metrics = Metrics::default();
        let layout = Layout::compute_with(Size::new(400.0, 20.0), &metrics, Frame::None, false);
        assert!(layout.content.height >= 0.0);
        assert!(one_leaf(&layout, &metrics).body.height >= 0.0);
    }

    #[test]
    fn a_click_maps_to_the_row_under_it_at_any_scroll_offset() {
        let metrics = Metrics::default();
        let window = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, false);
        let layout = one_leaf(&window, &metrics);
        let top = layout.body.y;
        // The row arithmetic that used to live here is `crate::cells` now — one
        // geometry for every uniform view, per ADR 0013 — and its tests moved
        // with it, where they cover both modes rather than only this one.
        assert!(layout.body.contains(layout.body.x + 1.0, top + 1.0));
        assert!(!layout.body.contains(layout.body.x + 1.0, layout.bar.y + 1.0));
        assert!(!layout.body.contains(layout.body.x + 1.0, window.status.y + 1.0));
    }
}
