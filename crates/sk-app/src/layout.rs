//! Where the regions are, in logical pixels.
//!
//! One function, computed once per frame, and every hit test reads it. The
//! alternative — each region computing its own rectangle at draw time and each
//! event handler recomputing it — is how a click lands one row off at 125%.

use sk_plat::Size;
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
}

/// Every region of the window.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    /// The path bar.
    pub path: Area,
    /// The column header strip.
    pub header: Area,
    /// The scrolling list.
    pub body: Area,
    /// The status strip.
    pub status: Area,
    /// The icon box's left edge and width, relative to the window.
    pub icon: Area,
    /// The name column.
    pub name: Area,
    /// The size column.
    pub size: Area,
    /// The modified column.
    pub modified: Area,
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
    pub fn compute(size: Size, metrics: &Metrics) -> Self {
        let full = size.width.max(1.0);
        let path = Area { x: 0.0, y: 0.0, width: full, height: metrics.path_height };
        let header = Area { x: 0.0, y: path.bottom(), width: full, height: metrics.header_height };
        let status = Area {
            x: 0.0,
            y: (size.height - metrics.status_height).max(header.bottom()),
            width: full,
            height: metrics.status_height,
        };
        let body = Area {
            x: 0.0,
            y: header.bottom(),
            width: full,
            height: (status.y - header.bottom()).max(0.0),
        };

        let gutter = metrics.gutter;
        let content = full - gutter * 2.0;
        let mut size_width = metrics.size_column;
        let mut modified_width = metrics.modified_column;
        // Shed the metadata columns rather than the name, widest first.
        if content - metrics.icon_column - size_width - modified_width < 100.0 {
            modified_width = 0.0;
        }
        if content - metrics.icon_column - size_width < 100.0 {
            size_width = 0.0;
        }

        let icon = Area { x: gutter, y: 0.0, width: metrics.icon_column, height: 0.0 };
        let name_x = icon.right();
        let name_width = (full - gutter - size_width - modified_width - name_x).max(0.0);
        Self {
            path,
            header,
            body,
            status,
            icon,
            name: Area { x: name_x, y: 0.0, width: name_width, height: 0.0 },
            size: Area { x: name_x + name_width, y: 0.0, width: size_width, height: 0.0 },
            modified: Area {
                x: name_x + name_width + size_width,
                y: 0.0,
                width: modified_width,
                height: 0.0,
            },
        }
    }

    /// The display row at logical `y`, or `None` outside the list.
    #[must_use]
    pub fn row_at(&self, y: f32, offset: f32, row_height: f32) -> Option<usize> {
        if y < self.body.y || y >= self.body.bottom() || row_height <= 0.0 {
            return None;
        }
        Some(((y - self.body.y + offset) / row_height) as usize)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Layout;
    use sk_plat::Size;
    use sk_ui::Metrics;

    #[test]
    fn the_regions_tile_the_window_with_no_gap_and_no_overlap() {
        let metrics = Metrics::default();
        let layout = Layout::compute(Size::new(1100.0, 720.0), &metrics);
        assert!((layout.path.bottom() - layout.header.y).abs() < f32::EPSILON);
        assert!((layout.header.bottom() - layout.body.y).abs() < f32::EPSILON);
        assert!((layout.body.bottom() - layout.status.y).abs() < f32::EPSILON);
        assert!((layout.status.bottom() - 720.0).abs() < f32::EPSILON);
    }

    #[test]
    fn the_columns_tile_the_row_and_the_name_takes_what_is_left() {
        let metrics = Metrics::default();
        let layout = Layout::compute(Size::new(1100.0, 720.0), &metrics);
        assert!((layout.icon.right() - layout.name.x).abs() < f32::EPSILON);
        assert!((layout.name.right() - layout.size.x).abs() < f32::EPSILON);
        assert!((layout.size.right() - layout.modified.x).abs() < f32::EPSILON);
        assert!((layout.modified.right() - (1100.0 - metrics.gutter)).abs() < f32::EPSILON);
        assert!(layout.name.width > 700.0, "{}", layout.name.width);
    }

    #[test]
    fn a_narrow_window_sheds_metadata_columns_rather_than_the_name() {
        let metrics = Metrics::default();
        let wide = Layout::compute(Size::new(1100.0, 720.0), &metrics);
        assert!(wide.modified.width > 0.0 && wide.size.width > 0.0);

        let medium = Layout::compute(Size::new(320.0, 720.0), &metrics);
        assert!(medium.modified.width.abs() < f32::EPSILON, "the widest column goes first");
        assert!(medium.name.width >= 100.0);

        let narrow = Layout::compute(Size::new(200.0, 720.0), &metrics);
        assert!(narrow.size.width.abs() < f32::EPSILON);
        assert!(narrow.name.width > 0.0, "the name never disappears");
    }

    #[test]
    fn a_window_shorter_than_its_chrome_still_produces_a_valid_layout() {
        let layout = Layout::compute(Size::new(400.0, 20.0), &Metrics::default());
        assert!(layout.body.height >= 0.0);
        assert!(layout.status.y >= layout.header.bottom());
    }

    #[test]
    fn a_click_maps_to_the_row_under_it_at_any_scroll_offset() {
        let layout = Layout::compute(Size::new(1100.0, 720.0), &Metrics::default());
        let top = layout.body.y;
        assert_eq!(layout.row_at(top + 1.0, 0.0, 26.0), Some(0));
        assert_eq!(layout.row_at(top + 27.0, 0.0, 26.0), Some(1));
        assert_eq!(layout.row_at(top + 1.0, 260.0, 26.0), Some(10));
        assert_eq!(layout.row_at(layout.path.y + 1.0, 0.0, 26.0), None, "not in the path bar");
        assert_eq!(layout.row_at(layout.status.y + 1.0, 0.0, 26.0), None);
    }
}
