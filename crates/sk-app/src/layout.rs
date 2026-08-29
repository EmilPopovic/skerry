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
    /// The path bar.
    pub path: Area,
    /// The list of places down the left, or empty when it is closed or shed.
    ///
    /// Below the path strip and not beside it, which is where Windows 11 puts
    /// it: the address bar spans the window and the navigation pane starts under
    /// it. So the column headers begin where this ends.
    pub sidebar: Area,
    /// Everything the pane tree divides between its leaves: below the path
    /// strip, right of the sidebar, above the status strip.
    ///
    /// Where the header, the list and the columns used to be. They are per leaf
    /// now ([ADR 0010](../adr/0010-a-split-tree-of-tab-strips.md)) and live in
    /// [`crate::panes::PaneLayout`].
    pub content: Area,
    /// The status strip.
    pub status: Area,

    /// Back, forward, parent and reload, at the left of the path strip.
    ///
    /// In the strip rather than above it, which is where Windows puts them and
    /// costs no vertical space: the path bar is already the title bar, and a
    /// second row for four buttons is 44 logical pixels of list for four marks.
    pub tools: [Area; 4],
    /// The three window buttons — minimise, maximise, close, left to right.
    ///
    /// All empty when the compositor draws the frame, which is every desktop
    /// except GNOME.
    pub buttons: [Area; 3],
    /// Where a press starts moving the window.
    ///
    /// The path strip doubles as the title bar rather than a title bar being
    /// added above it: a strip that says where you are is already the strip a
    /// person reaches for to drag the window, and a second one costs 36 logical
    /// pixels of list for a duplicate of the same information.
    pub drag: Area,
    /// How far in from an edge a press starts a resize. Zero with server-side
    /// decorations, and zero when the window is maximized or tiled — its edges
    /// are not free to move then.
    pub resize_border: f32,
}

/// The icon, name, size and modified columns for a list of `body`'s width.
///
/// The two metadata columns keep their width and the name takes what is left,
/// because a filename is the only cell whose useful length is unbounded. Below
/// the width where the name would fall under a hundred logical pixels the
/// metadata columns give way instead, widest first.
pub(crate) fn columns(body: Area, metrics: &Metrics) -> (Area, Area, Area, Area) {
    let gutter = metrics.gutter;
    let available = body.width - gutter * 2.0;
    let mut size_width = metrics.size_column;
    let mut modified_width = metrics.modified_column;
    if available - metrics.icon_column - size_width - modified_width < 100.0 {
        modified_width = 0.0;
    }
    if available - metrics.icon_column - size_width < 100.0 {
        size_width = 0.0;
    }

    let icon = Area { x: body.x + gutter, y: 0.0, width: metrics.icon_column, height: 0.0 };
    let name_x = icon.right();
    let name_width = (body.right() - gutter - size_width - modified_width - name_x).max(0.0);
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
    /// The two metadata columns keep their width and the name column takes what
    /// is left, because a filename is the only cell whose useful length is
    /// unbounded. Below the width where the name column would fall under a
    /// hundred logical pixels the metadata columns give way instead, so a
    /// narrow window shows names rather than three unreadable columns.
    #[must_use]
    pub fn compute_with(size: Size, metrics: &Metrics, frame: Frame, sidebar: bool) -> Self {
        let full = size.width.max(1.0);
        // Only the buttons the compositor actually offers, packed against the
        // right edge: a slot left empty for an absent action reads as a bug, and
        // a button that does nothing reads as a worse one.
        let offered = frame.offered();
        let strip = metrics.button_width * offered.iter().filter(|it| **it).count() as f32;
        // The tools come off the left of the strip and the breadcrumb starts
        // after them. Shed together below the width where the path would have
        // nothing left, the same way the metadata columns shed.
        let tools_width = metrics.tool_width * 4.0;
        let room = full - strip;
        let tools = std::array::from_fn(|index| {
            if room - tools_width < 120.0 {
                return Area::default();
            }
            Area {
                x: metrics.tool_width * index as f32,
                y: 0.0,
                width: metrics.tool_width,
                height: metrics.path_height,
            }
        });
        let toolbar = tools.iter().map(|tool| tool.width).sum::<f32>();
        let path = Area {
            x: toolbar,
            y: 0.0,
            width: (room - toolbar).max(0.0),
            height: metrics.path_height,
        };
        let mut cursor = full - strip;
        let mut buttons = [Area::default(); 3];
        for (slot, offered) in buttons.iter_mut().zip(offered) {
            if !offered {
                continue;
            }
            *slot = Area {
                x: cursor,
                y: 0.0,
                width: metrics.button_width,
                height: metrics.path_height,
            };
            cursor += metrics.button_width;
        }
        let drag = if frame.is_own() { path } else { Area::default() };
        // A maximized or tiled window has no free edges, so there is nothing to
        // grab — and an invisible six-pixel band that swallows clicks at the top
        // of a maximized list is worse than no resize at all.
        let resize_border = match frame {
            Frame::Own { edges: true, .. } => metrics.resize_border,
            _ => 0.0,
        };
        let status = Area {
            x: 0.0,
            y: (size.height - metrics.status_height).max(path.bottom() + metrics.header_height),
            width: full,
            height: metrics.status_height,
        };
        // Shed below the width where the list would be narrower than the
        // sidebar: a window too small for both is a window that came for the
        // list. The same rule the metadata columns follow.
        let aside = if sidebar && full - metrics.sidebar_width >= 320.0 {
            Area {
                x: 0.0,
                y: path.bottom(),
                width: metrics.sidebar_width,
                height: (status.y - path.bottom()).max(0.0),
            }
        } else {
            Area::default()
        };
        let content = Area {
            x: aside.width,
            y: path.bottom(),
            width: full - aside.width,
            height: (status.y - path.bottom()).max(0.0),
        };

        Self {
            window: size,
            path,
            sidebar: aside,
            content,
            status,
            tools,
            buttons,
            drag,
            resize_border,
        }
    }

    /// The navigation button at a point, if any.
    ///
    /// Split out of `compute_with` only because that function has a length limit
    /// worth keeping; the reasoning is in the doc on [`Layout::compute`].
    #[must_use]
    pub fn tool_at(&self, x: f32, y: f32) -> Option<Tool> {
        Tool::ALL
            .into_iter()
            .zip(self.tools)
            .find_map(|(tool, area)| (area.width > 0.0 && area.contains(x, y)).then_some(tool))
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

impl crate::panes::PaneLayout {
    /// The display row at logical `y`, or `None` outside this pane's list.
    pub(crate) fn row_at(&self, y: f32, offset: f32, row_height: f32) -> Option<usize> {
        if y < self.body.y || y >= self.body.bottom() || row_height <= 0.0 {
            return None;
        }
        Some(((y - self.body.y + offset) / row_height) as usize)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Button, Frame, Layout, Tool};
    use sk_plat::{Edge, Size};
    use sk_ui::Metrics;

    /// The geometry of a window holding one pane, which is what every test here
    /// used to be able to read off the `Layout` itself.
    fn one_leaf(layout: &Layout, metrics: &Metrics) -> crate::panes::PaneLayout {
        let panes = crate::panes::Panes::new(crate::pane::Pane::new(std::path::PathBuf::from("/")));
        let (mut leaves, mut dividers) = (Vec::new(), Vec::new());
        panes.layout(layout.content, metrics, &mut leaves, &mut dividers);
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
        assert!((all.path.right() - all.buttons[0].x).abs() < f32::EPSILON, "a gap before the row");

        let two = Layout::compute_with(
            size,
            &metrics,
            Frame::Own { edges: true, minimize: false, maximize: true },
            false,
        );
        assert!(two.buttons[0].width <= 0.0, "an absent action was given a slot");
        assert!((two.buttons[2].right() - 1100.0).abs() < f32::EPSILON);
        assert!((two.buttons[1].right() - two.buttons[2].x).abs() < f32::EPSILON, "a hole");
        assert!(two.path.width > all.path.width, "the path bar did not take the freed width");

        let served = Layout::compute_with(size, &metrics, Frame::None, false);
        assert!(served.buttons.iter().all(|button| button.width <= 0.0));
        assert!(served.drag.width <= 0.0, "a compositor-drawn title bar is not ours to drag");
        // The whole strip minus the four navigation buttons, which are there
        // whoever draws the frame.
        assert!((served.path.right() - 1100.0).abs() < f32::EPSILON);
        assert!((served.path.x - served.tools[3].right()).abs() < f32::EPSILON);
    }

    /// The sidebar takes the left of everything below the path strip, the
    /// header and the list start where it ends, and both shed at the width
    /// where the list would be narrower than the sidebar is wide.
    #[test]
    fn the_sidebar_takes_the_left_and_the_list_starts_where_it_ends() {
        let metrics = Metrics::default();
        let open = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, true);
        assert!((open.sidebar.width - metrics.sidebar_width).abs() < f32::EPSILON);
        assert!(open.sidebar.x <= f32::EPSILON);
        // Below the path strip, down to the status strip, with no gap at either.
        assert!((open.sidebar.y - open.path.bottom()).abs() < f32::EPSILON);
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

    /// The four navigation buttons come off the left of the strip, the
    /// breadcrumb starts after them, and both shed at the same width — a path
    /// bar with buttons and no path is worse than either alone.
    #[test]
    fn the_navigation_buttons_take_the_left_of_the_strip_and_shed_together() {
        let metrics = Metrics::default();
        let wide = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, false);
        assert!(wide.tools.iter().all(|tool| tool.width > 0.0));
        assert!(wide.tools[0].x <= f32::EPSILON, "they start at the left edge");
        for pair in wide.tools.windows(2) {
            assert!((pair[0].right() - pair[1].x).abs() < f32::EPSILON, "a gap in the row");
        }
        assert!(
            (wide.tools[3].right() - wide.path.x).abs() < f32::EPSILON,
            "the path does not start where the buttons end"
        );
        assert!((wide.path.right() - 1100.0).abs() < f32::EPSILON);

        // Narrow enough that the path would have nothing left.
        let narrow = Layout::compute_with(Size::new(200.0, 720.0), &metrics, Frame::None, false);
        assert!(narrow.tools.iter().all(|tool| tool.width <= 0.0));
        assert!(narrow.path.x <= f32::EPSILON);
        assert!((narrow.path.width - 200.0).abs() < f32::EPSILON);
    }

    /// Each button is found by the rectangle it is drawn from, and each names
    /// the command it runs — rule 2, so the dimming and the click agree.
    #[test]
    fn a_press_on_a_navigation_button_finds_that_button() {
        let layout =
            Layout::compute_with(Size::new(1100.0, 720.0), &Metrics::default(), Frame::None, false);
        for (tool, area) in Tool::ALL.into_iter().zip(layout.tools) {
            let (x, y) = (area.x + area.width / 2.0, area.y + area.height / 2.0);
            assert_eq!(layout.tool_at(x, y), Some(tool));
        }
        assert_eq!(layout.tool_at(600.0, 20.0), None, "the breadcrumb is not a button");
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
        assert!((layout.path.bottom() - layout.content.y).abs() < f32::EPSILON);
        assert!((layout.content.bottom() - layout.status.y).abs() < f32::EPSILON);
        assert!((layout.status.bottom() - 720.0).abs() < f32::EPSILON);
        // And inside the content, the one leaf tiles it in turn: the tab strip
        // first, then the column header, then the list.
        let pane = one_leaf(&layout, &metrics);
        assert!((pane.tabs.y - layout.content.y).abs() < f32::EPSILON);
        assert!((pane.tabs.bottom() - pane.header.y).abs() < f32::EPSILON);
        assert!((pane.header.bottom() - pane.body.y).abs() < f32::EPSILON);
        assert!((pane.body.bottom() - layout.content.bottom()).abs() < f32::EPSILON);
    }

    #[test]
    fn the_columns_tile_the_row_and_the_name_takes_what_is_left() {
        let metrics = Metrics::default();
        let window = Layout::compute_with(Size::new(1100.0, 720.0), &metrics, Frame::None, false);
        let layout = one_leaf(&window, &metrics);
        assert!((layout.icon.right() - layout.name.x).abs() < f32::EPSILON);
        assert!((layout.name.right() - layout.size.x).abs() < f32::EPSILON);
        assert!((layout.size.right() - layout.modified.x).abs() < f32::EPSILON);
        assert!((layout.modified.right() - (1100.0 - metrics.gutter)).abs() < f32::EPSILON);
        assert!(layout.name.width > 700.0, "{}", layout.name.width);
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
        assert_eq!(layout.row_at(top + 1.0, 0.0, 26.0), Some(0));
        assert_eq!(layout.row_at(top + 27.0, 0.0, 26.0), Some(1));
        assert_eq!(layout.row_at(top + 1.0, 260.0, 26.0), Some(10));
        assert_eq!(layout.row_at(window.path.y + 1.0, 0.0, 26.0), None, "not in the path bar");
        assert_eq!(layout.row_at(window.status.y + 1.0, 0.0, 26.0), None);
    }
}
