//! Where the entries of a listing sit inside a pane's body.
//!
//! **One value answers every geometric question a view has**, which is [ADR
//! 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md).
//! Before it, `body.y + row * row_height - scroll` was written out in fifteen
//! places across `sk-app`, each of them assuming one column and one fixed row
//! height — so a second view meant fifteen `match` arms and a third meant thirty.
//!
//! **Details is a one-column grid rather than a special case.** That is the whole
//! reason this is a seam and not a wrapper over two implementations: `columns` is
//! 1, the cell is the body's full width, and the paint pass, the hit test, the
//! page size, the scrollbar's content height, the reveal and all four direction
//! keys fall out of the same arithmetic for both modes.
//!
//! Pure arithmetic over five numbers, so it is testable without a window, a GPU
//! or a directory — which is where the coverage for every future mode belongs.

use crate::layout::Area;

/// How a pane lays its entries out.
///
/// Uniform-cell modes only. ADR 0013 records why Tree is not a variant here and
/// why Group By sits above this rather than inside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// One entry per row, with the size and modified columns beside the name.
    #[default]
    Details,
    /// A grid of square-ish cells, the name under the mark.
    Grid,
    /// One column per level of the path, the pane's own directory rightmost.
    ///
    /// **Not a fourth arithmetic.** A column is Details over a narrower body, so
    /// everything here treats this exactly as [`Mode::Details`] — one cell per
    /// row, the full width of whatever area it is handed. What differs is *which*
    /// area that is, and that is [`crate::trail`]'s answer because only the trail
    /// knows how many columns there are. See [ADR
    /// 0015](../../../docs/adr/0015-a-column-is-a-directory-and-a-choice.md).
    Columns,
}

impl Mode {
    /// The modes, in the order the switcher lays them out.
    ///
    /// Least dense first, which is the order every file manager that has a
    /// switcher uses: a grid of large cells, then a row per entry.
    pub const ALL: [Self; 3] = [Self::Grid, Self::Details, Self::Columns];

    /// The command each one runs.
    ///
    /// **Rule 2, the same way [`crate::layout::Tool`] does it.** A button in the
    /// switcher is not a second way to change the mode, it is the same action the
    /// palette and `Ctrl+1` reach — so the button's name, its state and its effect
    /// all come from the one registry entry rather than from three places that
    /// have to agree.
    #[must_use]
    pub const fn command(self) -> &'static str {
        match self {
            Self::Details => "view.details",
            Self::Grid => "view.grid",
            Self::Columns => "view.columns",
        }
    }

    /// The token this is written as in the session file.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Details => "details",
            Self::Grid => "grid",
            Self::Columns => "columns",
        }
    }

    /// The mode a session file's token names, if it names one.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "details" => Some(Self::Details),
            "grid" => Some(Self::Grid),
            "columns" => Some(Self::Columns),
            _ => None,
        }
    }
}

/// The narrowest a grid cell is drawn, in row heights.
///
/// **Derived from `row_height` rather than fixed, so zoom works.** `Ctrl+=`
/// changes the row height and a person expects it to change the grid too; a
/// constant here would leave the cells the same size and only the text inside
/// them growing. 4.5 row heights is 117 logical pixels at the default 26, which
/// is wide enough for a name of about sixteen characters before it elides.
const GRID_MIN_WIDTH: f32 = 4.5;

/// How tall a grid cell is, in row heights.
///
/// **From the content, not from the width.** Deriving the height from the cell's
/// width made a wide pane's cells 205 pixels tall with two lines of them on
/// screen — the mark ballooned, the scrolling went strange, and the same folder
/// looked like a different program at two window sizes. A cell holds a mark and
/// one line of name whatever it is wide, so its height is that and does not move.
pub(crate) const GRID_HEIGHT: f32 = 3.4;

/// The strip down the right of the body the scrollbar thumb sits in.
///
/// Reserved from the grid's *column arithmetic* and not from the body, so the
/// last cell in a line stops short of the thumb while a click in the strip still
/// lands on that cell — `index_at` clamps a column past the last one, which is
/// what makes those two compatible.
///
/// **Details reserves it too, from its columns.** It used not to, on the grounds
/// that a row's fill is already inset by the gutter — which was true and was not
/// enough: the thumb sits at the pane's right edge, the fill ran to within five
/// pixels of it, and the thumb was drawn across the focus ring of any selected
/// row. Reserving eleven pixels from the column arithmetic is what puts the fill
/// and the thumb in different places rather than nearly the same one.
pub(crate) const TRACK: f32 = 11.0;

/// A direction key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    Up,
    Down,
    Left,
    Right,
}

/// Where a pane's entries are, this frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Cells {
    mode: Mode,
    body: Area,
    /// One cell's size. In Details the width is the body's.
    cell: (f32, f32),
    /// How many cells sit side by side. Always at least one.
    columns: usize,
    /// How many entries there are to place.
    count: usize,
}

impl Cells {
    /// Works out the geometry for `count` entries in `body`.
    pub(crate) fn new(mode: Mode, body: Area, row_height: f32, count: usize) -> Self {
        let row_height = row_height.max(1.0);
        let (cell, columns) = match mode {
            // One column of full-width cells for both, because a trail's column
            // *is* Details over a narrower body — which is what makes this a
            // third mode rather than a third arithmetic.
            Mode::Details | Mode::Columns => ((body.width, row_height), 1),
            Mode::Grid => {
                // The track is only worth reserving where a cell still fits
                // beside it. Subtracting it unconditionally collapsed a pane
                // narrower than eleven pixels to a one-by-one cell, and a cell
                // one pixel tall put every entry on its own line thousands deep.
                let least = GRID_MIN_WIDTH * row_height;
                let usable = (body.width - TRACK).max(least.min(body.width)).max(1.0);
                // At least one, however narrow the pane is dragged: a pane too
                // thin for a cell still has to place its entries somewhere, and
                // zero columns is a division by zero in five methods.
                let columns = (usable / least).floor().max(1.0);
                ((usable / columns, (GRID_HEIGHT * row_height).round()), columns as usize)
            }
        };
        Self { mode, body, cell, columns, count }
    }

    /// Which mode this is the geometry for.
    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }

    /// How many cells sit side by side.
    pub(crate) const fn columns(&self) -> usize {
        self.columns
    }

    /// One cell's width and height.
    pub(crate) const fn cell(&self) -> (f32, f32) {
        self.cell
    }

    /// How many entries it places.
    pub(crate) const fn len(&self) -> usize {
        self.count
    }

    /// How many rows of cells the entries make.
    pub(crate) const fn lines(&self) -> usize {
        self.count.div_ceil(self.columns)
    }

    /// How tall the whole listing is, which is what the scrollbar measures.
    pub(crate) fn content_height(&self) -> f32 {
        self.lines() as f32 * self.cell.1
    }

    /// Where one entry is drawn, at this scroll offset.
    ///
    /// Returned whether or not it is on screen, because the caller that reveals a
    /// cursor needs the rectangle of a cell that is deliberately off it.
    pub(crate) fn at(&self, index: usize, offset: f32) -> Area {
        let line = index / self.columns;
        let column = index % self.columns;
        Area {
            x: self.body.x + column as f32 * self.cell.0,
            y: self.body.y + line as f32 * self.cell.1 - offset,
            width: self.cell.0,
            height: self.cell.1,
        }
    }

    /// The top and bottom of an entry's line, relative to the content.
    ///
    /// What `reveal` compares against the scroll offset. The *line's* extent
    /// rather than the cell's, because scrolling is vertical and every cell on a
    /// line shares it.
    pub(crate) fn span(&self, index: usize) -> (f32, f32) {
        let line = (index / self.columns) as f32;
        (line * self.cell.1, (line + 1.0) * self.cell.1)
    }

    /// The entry at a point, if there is one there.
    ///
    /// `None` outside these cells, past the last line, or in the ragged tail of
    /// the last line — a click to the right of the final cell is a click on
    /// nothing, which is what clears a selection.
    ///
    /// **Bounded by the content, not by a viewport.** It used to refuse any point
    /// outside `body`, which is right while `body` is the pane and wrong the
    /// moment it is a band's strip: a strip is positioned in *content*
    /// coordinates and the point arrives in screen ones, so the check compared
    /// two different spaces and lost every entry that was scrolled. Whether the
    /// point is on screen at all is [`crate::groups::Bands`]'s question, and it
    /// asks it once for the pane rather than once per band.
    pub(crate) fn index_at(&self, x: f32, y: f32, offset: f32) -> Option<usize> {
        if x < self.body.x || x >= self.body.right() {
            return None;
        }
        let line = ((y - self.body.y + offset) / self.cell.1).floor();
        if line < 0.0 || line >= self.lines() as f32 {
            return None;
        }
        let column =
            ((x - self.body.x) / self.cell.0).floor().clamp(0.0, self.columns as f32 - 1.0);
        let index = line as usize * self.columns + column as usize;
        (index < self.count).then_some(index)
    }

    /// Every entry whose cell the rectangle touches, in index order.
    ///
    /// **A rubber band over a grid selects a block, not a range.** In Details the
    /// block is one column wide and the indices come out contiguous, which is
    /// what the band did when it was arithmetic in `select_band`; in a grid the
    /// same rectangle picks a rectangle of cells and the indices are not
    /// contiguous at all. A callback rather than a `Vec` because this runs on
    /// every motion event of a drag.
    pub(crate) fn touching(&self, rect: Area, offset: f32, mut each: impl FnMut(usize)) {
        if self.count == 0 {
            return;
        }
        let top = rect.y - self.body.y + offset;
        let bottom = rect.bottom() - self.body.y + offset;
        // **Nothing at all when the rectangle misses.** Without this a band whose
        // strip is entirely above the band clamped `top` to zero and handed back
        // its first line, so a rubber band over one band selected a row from
        // every band below it.
        if bottom < 0.0 || top > self.content_height() {
            return;
        }
        let first_line = (top / self.cell.1).floor().max(0.0) as usize;
        let last_line = (bottom / self.cell.1).floor().max(0.0) as usize;
        let left = ((rect.x - self.body.x) / self.cell.0).floor().max(0.0) as usize;
        let right = ((rect.right() - self.body.x) / self.cell.0).floor().max(0.0) as usize;
        for line in first_line..=last_line.min(self.lines().saturating_sub(1)) {
            for column in left..=right.min(self.columns - 1) {
                let index = line * self.columns + column;
                if index < self.count {
                    each(index);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cells, Mode};
    use crate::layout::Area;

    fn body() -> Area {
        Area { x: 100.0, y: 50.0, width: 800.0, height: 400.0 }
    }

    fn details(count: usize) -> Cells {
        Cells::new(Mode::Details, body(), 20.0, count)
    }

    fn grid(count: usize) -> Cells {
        Cells::new(Mode::Grid, body(), 20.0, count)
    }

    /// Details is a one-column grid, which is the claim the seam rests on.
    #[test]
    fn details_is_one_column_and_the_body_wide() {
        let cells = details(10);
        assert_eq!(cells.columns(), 1);
        assert!((cells.cell().0 - 800.0).abs() < 0.01 && (cells.cell().1 - 20.0).abs() < 0.01);
        assert_eq!(cells.lines(), 10);
        assert!((cells.content_height() - 200.0).abs() < 0.01);
        // The old arithmetic, which every call site used to write out.
        assert!((cells.at(3, 0.0).y - (50.0 + 3.0 * 20.0)).abs() < 0.01);
        assert!((cells.at(3, 15.0).y - (50.0 + 3.0 * 20.0 - 15.0)).abs() < 0.01);
    }

    /// The grid divides what it may use exactly, leaving the thumb's strip alone.
    #[test]
    fn a_grid_cell_divides_the_body_less_the_scrollbar_with_nothing_left_over() {
        let cells = grid(20);
        let usable = 800.0 - super::TRACK;
        assert_eq!(cells.columns(), 8, "789 over 4.5 row heights of 20 floors to 8");
        let (width, _) = cells.cell();
        assert!((width * 8.0 - usable).abs() < 0.01, "eight cells of {width} miss {usable}");
        // The last cell stops short of the body's edge, which is the strip the
        // thumb is drawn in.
        let last = cells.at(7, 0.0);
        assert!(last.right() <= 900.0 - super::TRACK + 0.01, "the last cell reaches the thumb");
        // Eight across, so twenty entries make three lines and the last is ragged.
        assert_eq!(cells.lines(), 3);
    }

    /// A cell's height does not move when the pane gets wider.
    ///
    /// The defect this replaced: the height came from the width, so a wide pane
    /// had cells 205 pixels tall and two lines of them on screen, and the same
    /// folder looked like a different program at two window sizes.
    #[test]
    fn a_cell_is_the_same_height_however_wide_the_pane_is() {
        let heights: Vec<f32> = [200.0, 600.0, 1600.0, 4000.0]
            .into_iter()
            .map(|width| {
                let body = Area { x: 0.0, y: 0.0, width, height: 400.0 };
                Cells::new(Mode::Grid, body, 20.0, 50).cell().1
            })
            .collect();
        for height in &heights {
            assert!((height - heights[0]).abs() < 0.01, "{heights:?} are not all one height");
        }
        // And it is the row height times the constant, so zoom moves it.
        assert!((heights[0] - (super::GRID_HEIGHT * 20.0).round()).abs() < 0.01);
    }

    /// Zooming the rows zooms the cells, which is what `Ctrl+=` has to mean.
    #[test]
    fn a_taller_row_makes_a_bigger_cell_and_fewer_columns() {
        let body = Area { x: 0.0, y: 0.0, width: 900.0, height: 400.0 };
        let small = Cells::new(Mode::Grid, body, 20.0, 100);
        let large = Cells::new(Mode::Grid, body, 40.0, 100);
        assert!(large.cell().1 > small.cell().1, "a taller row did not make a taller cell");
        assert!(large.cell().0 > small.cell().0, "a taller row did not make a wider cell");
        assert!(large.columns() < small.columns(), "a bigger cell did not mean fewer of them");
    }

    /// A click in the thumb's strip still lands on the last cell in that line.
    ///
    /// The reason the track comes out of the column arithmetic and not out of the
    /// body: reserving it from the body would make those eleven pixels select
    /// nothing at all.
    #[test]
    fn a_click_beside_the_thumb_still_lands_on_the_last_cell() {
        let cells = grid(12);
        let last_in_line = cells.columns() - 1;
        let in_the_track = 100.0 + 800.0 - 4.0;
        assert_eq!(cells.index_at(in_the_track, 52.0, 0.0), Some(last_in_line));
    }

    /// However thin the pane, there is a column, because zero divides nothing.
    #[test]
    fn a_pane_too_narrow_for_a_cell_still_has_one_column() {
        let thin = Area { x: 0.0, y: 0.0, width: 12.0, height: 100.0 };
        let cells = Cells::new(Mode::Grid, thin, 20.0, 5);
        assert_eq!(cells.columns(), 1);
        assert!(cells.cell().0 > 0.0 && cells.cell().1 > 0.0);
        assert_eq!(cells.index_at(6.0, 10.0, 0.0), Some(0));
    }

    /// A click lands on the cell under it, and on nothing past the last one.
    #[test]
    fn a_point_finds_its_cell_and_the_ragged_tail_finds_nothing() {
        // One more than a line holds, so there is a ragged second line whatever
        // the column count works out to.
        let cells = grid(0);
        let columns = cells.columns();
        let cells = grid(columns + 2);
        let first = cells.at(0, 0.0);
        assert_eq!(cells.index_at(first.x + 2.0, first.y + 2.0, 0.0), Some(0));
        let second = cells.at(1, 0.0);
        assert_eq!(cells.index_at(second.x + 2.0, second.y + 2.0, 0.0), Some(1));
        // The second line holds two, so its third column is nothing at all.
        let third_of_line_two = cells.at(columns + 2, 0.0);
        let probe = (third_of_line_two.x + 2.0, third_of_line_two.y + 2.0);
        assert_eq!(cells.index_at(probe.0, probe.1, 0.0), None);
        // Outside the body entirely.
        assert_eq!(cells.index_at(5.0, 5.0, 0.0), None);
    }

    /// Scrolling moves what a point hits, which is the offset going in.
    #[test]
    fn a_point_finds_a_different_cell_once_the_list_has_scrolled() {
        let cells = details(100);
        let top = body().y + 2.0;
        assert_eq!(cells.index_at(200.0, top, 0.0), Some(0));
        assert_eq!(cells.index_at(200.0, top, 200.0), Some(10));
    }

    /// An empty listing answers every question without dividing by zero.
    #[test]
    fn an_empty_listing_has_no_cells_and_no_arithmetic_faults() {
        for cells in [details(0), grid(0)] {
            assert_eq!(cells.lines(), 0);
            assert!(cells.content_height().abs() < 0.01);
            assert_eq!(cells.index_at(200.0, body().y + 2.0, 0.0), None);
            assert_eq!(cells.lines(), 0);
        }
    }

    /// A band over Details picks a run; the same band over a grid picks a block.
    #[test]
    fn a_band_picks_a_run_in_details_and_a_block_in_a_grid() {
        let band = Area { x: 100.0, y: 90.0, width: 200.0, height: 45.0 };

        let mut run = Vec::new();
        details(100).touching(band, 0.0, |index| run.push(index));
        // Rows are 20 tall from y=50, so the band spans rows 2, 3 and 4.
        assert_eq!(run, [2, 3, 4]);

        let cells = grid(30);
        let (width, height) = cells.cell();
        let columns = cells.columns();
        // A band two columns wide and two lines tall, from the first cell.
        let block = Area { x: 101.0, y: 51.0, width, height };
        let mut picked = Vec::new();
        cells.touching(block, 0.0, |index| picked.push(index));
        assert_eq!(picked, [0, 1, columns, columns + 1], "a grid band takes a block, not a range");
    }

    /// A band past the end of a ragged last line takes what is there.
    #[test]
    fn a_band_over_the_ragged_tail_takes_only_real_entries() {
        let cells = grid(grid(0).columns() + 2);
        let whole = Area { x: 100.0, y: 50.0, width: 800.0, height: 400.0 };
        let mut picked = Vec::new();
        cells.touching(whole, 0.0, |index| picked.push(index));
        let all: Vec<usize> = (0..cells.columns() + 2).collect();
        assert_eq!(picked, all, "a band over everything must take everything");
    }

    /// The tokens the session file writes round-trip.
    #[test]
    fn every_mode_survives_the_session_file() {
        for mode in [Mode::Details, Mode::Grid] {
            assert_eq!(Mode::from_token(mode.token()), Some(mode), "{mode:?}");
        }
        assert_eq!(Mode::from_token("tree"), None, "a mode that does not exist yet");
        // A scroll offset must never be read as a mode: the tolerant parser in
        // `session` tells the two apart by exactly this.
        assert_eq!(Mode::from_token("0"), None);
        assert_eq!(Mode::from_token("120"), None);
    }
}
