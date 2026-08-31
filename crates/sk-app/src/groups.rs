//! Headings, and the entries laid out under them.
//!
//! **This is [ADR
//! 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)'s
//! "above the seam".** That decision put grouping deliberately outside
//! [`crate::cells::Cells`] — grouping changes what index maps to what position
//! and not how a cell is shaped, and doing it inside would make `Cells` answer
//! two questions. So this owns the vertical arrangement of bands and asks `Cells`
//! for everything inside one.
//!
//! **Orthogonal to the view mode, and it has to be.** A grid of this month's
//! photographs and a details list of this month's invoices are the same grouping
//! over two geometries; every method below works out the band and then defers to
//! the cell arithmetic, which is the same arithmetic Details and Grid share.
//!
//! # Where the order comes from
//!
//! Not from here. [`sk_model::Banding`] is a prefix byte ahead of the sort's
//! column, so the display order already has the bands as contiguous runs, each
//! sorted by whatever the pane is sorted by. This reads those runs and lays them
//! out; it never reorders anything.
//!
//! **Three quantities, one mechanism.** Time and size band on a column the
//! listing already holds, so they cost the sort a byte. Kind does not: it is
//! [`sk_desktop::Category`], which needs the desktop's `globs2`, so the byte is
//! worked out for every entry on the job that computes the order and comes back
//! with it — see `Pane::kinds` and the numbers in
//! [09-performance-budgets.md](../../../docs/09-performance-budgets.md).

use crate::cells::{Cells, Mode, Step};
use crate::layout::Area;

/// How a pane cuts its entries into headings.
///
/// **Three quantities, one mechanism.** Each is a prefix byte ahead of the
/// sort's column ([`sk_model::Banding`]), so the display order already has the
/// bands as contiguous runs and none of them is a second pass over the listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Grouping {
    /// One band, no headings.
    #[default]
    None,
    /// Today, yesterday, this week, this month, this year, and older.
    Time,
    /// Gigantic down to empty, with folders ahead of them.
    Size,
    /// Folders, images, audio, video, archives, code, documents, programs, and
    /// anything else — [`sk_desktop::Category`], which is also what colours a
    /// row's mark, so the heading and the mark under it always agree.
    Kind,
}

impl Grouping {
    /// The four, in the order the View menu lists them.
    pub const ALL: [Self; 4] = [Self::None, Self::Time, Self::Size, Self::Kind];

    /// The token this is written as in the session file.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::None => "flat",
            Self::Time => "by-time",
            Self::Size => "by-size",
            Self::Kind => "by-kind",
        }
    }

    /// The grouping a session file's token names, if it names one.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|it| it.token() == token)
    }

    /// The catalogue key for one of this grouping's bands, first band first.
    ///
    /// A band with nothing in it is never drawn, which is what happens to
    /// "earlier this week" on a Monday, to "in the future" almost always, and to
    /// six of the nine kinds in most folders.
    ///
    /// **Clamped, deliberately.** A byte past the end is the band for "anything
    /// this level could not classify" — a kind byte the sort never received, say
    /// — and the last key of every set is the one that says so.
    #[must_use]
    pub fn label(self, band: u8) -> &'static str {
        let keys: &[&'static str] = match self {
            Self::None => &["band.older"],
            Self::Time => &[
                "band.future",
                "band.today",
                "band.yesterday",
                "band.this_week",
                "band.this_month",
                "band.this_year",
                "band.older",
            ],
            Self::Size => &[
                "band.folders",
                "band.gigantic",
                "band.huge",
                "band.large",
                "band.medium",
                "band.small",
                "band.tiny",
                "band.empty",
            ],
            Self::Kind => &[
                "band.folders",
                "band.images",
                "band.audio",
                "band.video",
                "band.archives",
                "band.code",
                "band.documents",
                "band.programs",
                "band.other",
            ],
        };
        keys[(band as usize).min(keys.len() - 1)]
    }

    /// What a pane sorted this way bands on.
    ///
    /// `now` is only read by [`Self::Time`], whose edges come from the local
    /// calendar and move at midnight — which is why this is asked every frame
    /// rather than once. See `App::rebandif_needed`.
    #[must_use]
    pub fn banding(self, now: i64) -> sk_model::Banding {
        use sk_model::{Banding, Cuts};
        match self {
            Self::None => Banding::None,
            Self::Time => Banding::Time(Cuts::new(&sk_ui::format::day_bands(now))),
            Self::Size => Banding::Size(Cuts::new(&sk_ui::format::SIZE_EDGES)),
            Self::Kind => Banding::Kind,
        }
    }
}

/// How tall a heading is, in row heights.
///
/// Derived from `row_height` for the reason the grid cell is: `Ctrl+=` has to
/// move everything in the pane, not the text inside a strip that stays put.
const HEADING: f32 = 1.2;

/// One band, placed.
///
/// **It carries its own [`Cells`].** That is what makes this composition rather
/// than a second copy of the cell arithmetic: a band is a `Cells` over the strip
/// of body below its heading, so `at`, `index_at` and `touching` are the same
/// code Details and Grid already share, asked of a smaller rectangle. The first
/// draft reimplemented all three with a band offset added by hand, which is the
/// duplication [ADR
/// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)
/// exists to prevent.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Band {
    /// Which band it is, for the label.
    kind: u8,
    /// Its first entry in display order.
    first: usize,
    /// The y its heading sits at, in content coordinates.
    top: f32,
    /// Its entries, over the strip below the heading.
    cells: Cells,
}

impl Band {
    /// How many entries it holds. Never zero — an empty band is not placed.
    const fn count(&self) -> usize {
        self.cells.len()
    }

    /// The content-relative y its entries begin at.
    fn cells_top(&self, heading: f32) -> f32 {
        self.top + heading
    }
}

/// Where a pane's headings and entries are, this frame.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Bands {
    cells: Cells,
    body: Area,
    /// Zero when there are no headings, which is what makes the ungrouped case
    /// the same code path rather than a branch at every call site.
    heading: f32,
    placed: Vec<Band>,
}

impl Bands {
    /// Lays `runs` out in `body`.
    ///
    /// `runs` is the band of each contiguous run of the display order and the row
    /// it starts at, which is what [`crate::pane::Pane`] recomputes whenever the
    /// order changes. Empty means one band and no heading.
    pub(crate) fn new(
        mode: Mode,
        body: Area,
        row_height: f32,
        runs: &[(u8, usize)],
        count: usize,
    ) -> Self {
        let cells = Cells::new(mode, body, row_height, count);
        let heading = if runs.is_empty() { 0.0 } else { (HEADING * row_height).round() };
        // A band's own `Cells` over the strip below its heading. The pane-wide one
        // above settles the columns and the cell size, so every band agrees about
        // both — a grid whose columns changed per band is a grid nobody can read
        // down.
        let strip = |top: f32, held: usize| {
            let high = lines(held, cells.columns()) as f32 * cells.cell().1;
            let at = Area { x: body.x, y: body.y + top + heading, width: body.width, height: high };
            Cells::new(mode, at, row_height, held)
        };
        let mut bands = Vec::with_capacity(runs.len().max(1));
        let mut top = 0.0;
        for (at, (kind, first)) in runs.iter().enumerate() {
            let end = runs.get(at + 1).map_or(count, |(_, next)| *next);
            let held = end.saturating_sub(*first);
            if held == 0 {
                continue;
            }
            bands.push(Band { kind: *kind, first: *first, top, cells: strip(top, held) });
            top += heading + lines(held, cells.columns()) as f32 * cells.cell().1;
        }
        // A listing always has a band, so every method below can take one rather
        // than asking whether there is one.
        if bands.is_empty() {
            bands.push(Band { kind: 0, first: 0, top: 0.0, cells: strip(0.0, count) });
        }
        Self { cells, body, heading, placed: bands }
    }

    /// How many cells sit side by side.
    pub(crate) const fn columns(&self) -> usize {
        self.cells.columns()
    }

    /// One cell's width and height.
    pub(crate) const fn cell(&self) -> (f32, f32) {
        self.cells.cell()
    }

    /// Which view mode this is the geometry for.
    pub(crate) const fn mode(&self) -> Mode {
        self.cells.mode()
    }

    /// How many headings are placed. One and no heading when ungrouped.
    pub(crate) fn count(&self) -> usize {
        if self.heading > 0.0 { self.placed.len() } else { 0 }
    }

    /// How tall the whole listing is, headings included.
    pub(crate) fn content_height(&self) -> f32 {
        self.placed
            .last()
            .map_or(0.0, |band| band.cells_top(self.heading) + band.cells.content_height())
    }

    /// The band an entry is in.
    fn band_of(&self, index: usize) -> &Band {
        // At most seven, so a scan beats a search and reads better than one.
        self.placed.iter().rev().find(|band| index >= band.first).unwrap_or_else(|| &self.placed[0])
    }

    /// Where one entry is drawn, at this scroll offset.
    pub(crate) fn at(&self, index: usize, offset: f32) -> Area {
        let band = self.band_of(index);
        band.cells.at(index.saturating_sub(band.first), offset)
    }

    /// The top and bottom of an entry's line, in content coordinates.
    ///
    /// **The heading is part of the span.** Revealing the first entry of a band
    /// has to bring its heading with it, or the cursor lands under a heading that
    /// is scrolled off and the pane says nothing about which band it is in.
    pub(crate) fn span(&self, index: usize) -> (f32, f32) {
        let band = self.band_of(index);
        let local = index.saturating_sub(band.first);
        let (top, bottom) = band.cells.span(local);
        let origin = band.cells_top(self.heading);
        let first_line = local < self.columns();
        (if first_line { band.top } else { origin + top }, origin + bottom)
    }

    /// The entry at a point, if there is one there.
    ///
    /// `None` on a heading as well as outside the body: a heading is not an entry
    /// and a click on one selects nothing, which is what a click on empty space
    /// does everywhere else in the pane.
    pub(crate) fn index_at(&self, x: f32, y: f32, offset: f32) -> Option<usize> {
        if !self.body.contains(x, y) {
            return None;
        }
        let content = y - self.body.y + offset;
        for band in &self.placed {
            let cells_top = band.cells_top(self.heading);
            if content < band.top || content >= cells_top + band.cells.content_height() {
                continue;
            }
            // A heading is not an entry, and a click on one selects nothing.
            if content < cells_top {
                return None;
            }
            return band.cells.index_at(x, y, offset).map(|local| band.first + local);
        }
        None
    }

    /// How many entries a page key moves.
    pub(crate) fn page(&self) -> usize {
        let lines = (self.body.height / self.cell().1).floor().max(1.0) as usize;
        (lines * self.columns()).max(1)
    }

    /// Where a direction key goes from `from`.
    ///
    /// Left and Right are previous and next across the whole listing, headings
    /// included — walking off the end of a band lands in the next one, which is
    /// what a reading order means. Up and Down move by a line and **keep the
    /// column** when they cross into another band, clamped to what that band's
    /// edge line actually holds.
    pub(crate) fn step(&self, from: usize, step: Step) -> usize {
        let total: usize = self.placed.iter().map(Band::count).sum();
        if total == 0 {
            return 0;
        }
        let last = total - 1;
        let from = from.min(last);
        match step {
            Step::Left => from.saturating_sub(1),
            Step::Right => (from + 1).min(last),
            Step::Up => self.vertically(from, false),
            Step::Down => self.vertically(from, true),
        }
    }

    /// One line down or up, crossing into the neighbouring band where it must.
    fn vertically(&self, from: usize, down: bool) -> usize {
        let columns = self.columns();
        let band = *self.band_of(from);
        let local = from.saturating_sub(band.first);
        let column = local % columns;
        if down {
            if local + columns < band.count() {
                return from + columns;
            }
            let Some(next) = self.placed.iter().find(|it| it.first > band.first) else {
                return from;
            };
            next.first + column.min(next.count() - 1)
        } else {
            if local >= columns {
                return from - columns;
            }
            let Some(prior) = self.placed.iter().rev().find(|it| it.first < band.first) else {
                return from;
            };
            // The last line of the band above, at the same column where it has
            // one — a ragged last line is shorter than the columns.
            let lines_above = prior.cells.lines();
            let last_line = prior.first + (lines_above - 1) * columns;
            (last_line + column).min(prior.first + prior.count() - 1)
        }
    }

    /// The entries that are on screen at this offset.
    pub(crate) fn visible(&self, offset: f32) -> std::ops::Range<usize> {
        let (top, bottom) = (offset, offset + self.body.height);
        let height = self.cell().1;
        let mut from = usize::MAX;
        let mut to = 0;
        for band in &self.placed {
            // **Not `band.cells.visible`.** That asks "what fits in this band's
            // own strip", and the question here is what fits in the *pane's*
            // viewport, which is a different rectangle and usually a larger one.
            let cells_top = band.cells_top(self.heading);
            let band_lines = band.cells.lines();
            if cells_top + band.cells.content_height() < top || cells_top > bottom {
                continue;
            }
            let first_line = ((top - cells_top) / height).floor().max(0.0) as usize;
            let last_line =
                (((bottom - cells_top) / height).ceil().max(0.0) as usize).min(band_lines);
            from = from.min(band.first + first_line * self.columns());
            to = to.max((band.first + last_line * self.columns()).min(band.first + band.count()));
        }
        if from == usize::MAX { 0..0 } else { from..to.max(from) }
    }

    /// Every entry whose cell the rectangle touches, in index order.
    pub(crate) fn touching(&self, rect: Area, offset: f32, mut each: impl FnMut(usize)) {
        for band in &self.placed {
            band.cells.touching(rect, offset, |local| each(band.first + local));
        }
    }

    /// Each heading that is on screen: which band it names, and where it goes.
    pub(crate) fn headings(&self, offset: f32) -> Vec<(u8, Area)> {
        if self.heading <= 0.0 {
            return Vec::new();
        }
        self.placed
            .iter()
            .filter_map(|band| {
                let y = self.body.y + band.top - offset;
                let on_screen = y + self.heading > self.body.y && y < self.body.bottom();
                on_screen.then_some((
                    band.kind,
                    Area { x: self.body.x, y, width: self.body.width, height: self.heading },
                ))
            })
            .collect()
    }
}

/// How many lines `count` entries make at `columns` across.
fn lines(count: usize, columns: usize) -> usize {
    count.div_ceil(columns.max(1))
}

/// The band of each run in the display order, and the row it starts at.
///
/// **Runs, because the order is already banded.** `sk_model::Banding` is a prefix
/// byte ahead of the sort's column, so entries of one band are contiguous; this
/// walks the order once and records where each run begins. Recomputed when the
/// order changes rather than per frame, because at two hundred thousand entries
/// per frame it would be the frame.
#[must_use]
pub fn runs_of(
    listing: &sk_model::Listing,
    bands: sk_model::Banding,
    kinds: &[u8],
) -> Vec<(u8, usize)> {
    if bands.is_empty() {
        return Vec::new();
    }
    let mut runs: Vec<(u8, usize)> = Vec::new();
    for row in 0..listing.rows() {
        let Some(id) = listing.at(row) else { continue };
        let band = listing.band_of(id, bands, kinds);
        if runs.last().is_none_or(|(last, _)| *last != band) {
            runs.push((band, row));
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::{Bands, Grouping, lines};
    use crate::cells::{Mode, Step};
    use crate::layout::Area;

    fn body() -> Area {
        Area { x: 100.0, y: 50.0, width: 800.0, height: 400.0 }
    }

    /// Three bands of four, laid out as one column so the arithmetic is legible.
    fn stacked() -> Bands {
        Bands::new(Mode::Details, body(), 20.0, &[(1, 0), (2, 4), (6, 8)], 12)
    }

    /// No runs is one band with no heading, which is the ungrouped pane.
    #[test]
    fn no_grouping_is_one_band_and_no_heading_at_all() {
        let flat = Bands::new(Mode::Details, body(), 20.0, &[], 10);
        assert_eq!(flat.count(), 0, "an ungrouped pane drew a heading");
        assert!(flat.headings(0.0).is_empty());
        // And it places entries exactly where the plain cell geometry would.
        let cells = crate::cells::Cells::new(Mode::Details, body(), 20.0, 10);
        for index in [0usize, 3, 9] {
            assert_eq!(flat.at(index, 0.0), cells.at(index, 0.0), "entry {index} moved");
        }
        assert!((flat.content_height() - cells.content_height()).abs() < 0.01);
    }

    /// A heading takes a line of its own, and the entries start under it.
    #[test]
    fn every_band_costs_a_heading_and_its_own_lines() {
        let bands = stacked();
        assert_eq!(bands.count(), 3);
        let heading = (super::HEADING * 20.0).round();
        // Three headings and twelve one-column lines of 20.
        let wanted = 3.0 * heading + 12.0 * 20.0;
        assert!((bands.content_height() - wanted).abs() < 0.01, "{}", bands.content_height());
        // The first entry sits under the first heading, not at the top.
        assert!((bands.at(0, 0.0).y - (body().y + heading)).abs() < 0.01);
        // The fifth entry is the second band's first: two headings above it.
        assert!((bands.at(4, 0.0).y - (body().y + 2.0 * heading + 4.0 * 20.0)).abs() < 0.01);
    }

    /// A click on a heading selects nothing.
    #[test]
    fn a_heading_is_not_an_entry() {
        let bands = stacked();
        let (_, first) = bands.headings(0.0)[0];
        assert_eq!(bands.index_at(first.x + 4.0, first.y + 2.0, 0.0), None);
        // And the entry under it is found.
        let below = bands.at(0, 0.0);
        assert_eq!(bands.index_at(below.x + 4.0, below.y + 2.0, 0.0), Some(0));
        // As is the first entry of the second band, under the second heading.
        let second = bands.at(4, 0.0);
        assert_eq!(bands.index_at(second.x + 4.0, second.y + 2.0, 0.0), Some(4));
    }

    /// Every entry's rectangle round-trips through the hit test.
    ///
    /// The cheapest whole-geometry check there is: if a band's origin is off by a
    /// heading anywhere, some entry is drawn where the hit test does not find it.
    #[test]
    fn every_entry_is_found_where_it_is_drawn() {
        for mode in [Mode::Details, Mode::Grid] {
            let bands = Bands::new(mode, body(), 20.0, &[(1, 0), (2, 5), (4, 6), (6, 13)], 20);
            for index in 0..20 {
                // Scrolled so the entry is on screen, because a hit test outside
                // the body is `None` by design and would prove nothing.
                let offset = bands.span(index).0;
                let cell = bands.at(index, offset);
                let found = bands.index_at(cell.x + 2.0, cell.y + 2.0, offset);
                assert_eq!(found, Some(index), "{mode:?} lost entry {index} at {cell:?}");
            }
        }
    }

    /// Down off the end of a band lands in the next one, keeping the column.
    #[test]
    fn a_direction_key_crosses_a_band_and_keeps_its_column() {
        // Four columns, two bands: 0..6 and 6..9. The width is chosen so the
        // cell arithmetic lands on four rather than asserted to.
        let wide = Area { x: 0.0, y: 0.0, width: 500.0, height: 400.0 };
        let bands = Bands::new(Mode::Grid, wide, 26.0, &[(1, 0), (5, 6)], 9);
        assert_eq!(bands.columns(), 4, "the fixture is not four columns wide");
        // Entry 1 is line 0 column 1; down is entry 5, still column 1.
        assert_eq!(bands.step(1, Step::Down), 5);
        // Entry 5 is the last of band 0's ragged line; down crosses to band 1 at
        // the same column, which band 1 has.
        assert_eq!(bands.step(5, Step::Down), 7);
        // Up from band 1 goes back to band 0's last line, same column.
        assert_eq!(bands.step(7, Step::Up), 5);
        // Column 3 of band 1 does not exist — three entries — so down from
        // column 3 of band 0 clamps to its last.
        assert_eq!(bands.step(3, Step::Down), 8, "clamped into a ragged band");
        // The ends stay put.
        assert_eq!(bands.step(0, Step::Up), 0);
        assert_eq!(bands.step(8, Step::Down), 8);
        // Left and Right walk across the boundary, because reading order does.
        assert_eq!(bands.step(5, Step::Right), 6);
        assert_eq!(bands.step(6, Step::Left), 5);
    }

    /// Revealing a band's first entry brings its heading with it.
    #[test]
    fn the_span_of_a_first_entry_includes_the_heading_above_it() {
        let bands = stacked();
        let heading = (super::HEADING * 20.0).round();
        // Entry 4 opens the second band, so its span starts at the heading.
        let (top, bottom) = bands.span(4);
        assert!((top - (heading + 4.0 * 20.0)).abs() < 0.01, "the heading is not in the span");
        assert!(bottom > top);
        // Entry 5 is not a first, so its span is just its line.
        let (top, _) = bands.span(5);
        assert!((top - (2.0 * heading + 5.0 * 20.0)).abs() < 0.01);
    }

    /// A band with nothing in it is never placed.
    #[test]
    fn an_empty_band_is_not_given_a_heading() {
        // Two runs beginning at the same row: the first holds nothing.
        let bands = Bands::new(Mode::Details, body(), 20.0, &[(1, 0), (3, 0), (6, 4)], 8);
        assert_eq!(bands.count(), 2, "an empty band was drawn");
        let kinds: Vec<u8> = bands.headings(0.0).into_iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [3, 6]);
    }

    /// What is visible covers the screen, and a band scrolled past is skipped.
    #[test]
    fn the_visible_range_walks_the_bands_it_overlaps() {
        let bands = stacked();
        let range = bands.visible(0.0);
        assert_eq!(range.start, 0);
        assert!(range.end >= 4, "the first screen holds at least a band");
        // Scrolled to the bottom, the range ends at the last entry.
        let deep = bands.visible(bands.content_height());
        assert!(deep.end <= 12);
        // And never past the end.
        assert!(bands.visible(1e9).end <= 12);
    }

    /// A band selects a block, and the band above it is left alone.
    #[test]
    fn a_band_selection_stops_at_the_heading_below_it() {
        let bands = stacked();
        // A rectangle over the first band's four rows only.
        let first = bands.at(0, 0.0);
        let fourth = bands.at(3, 0.0);
        let rect =
            Area { x: first.x, y: first.y, width: first.width, height: fourth.bottom() - first.y };
        let mut picked = Vec::new();
        bands.touching(rect, 0.0, |index| picked.push(index));
        assert_eq!(picked, [0, 1, 2, 3], "the band bled into its neighbour");
    }

    /// In Details, Left and Right are Up and Down — one column makes four keys two.
    ///
    /// The claim [ADR
    /// 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)
    /// makes about why neither key needs a mode-dependent binding, asserted
    /// against the geometry the pane actually uses.
    #[test]
    fn one_column_makes_the_four_directions_two() {
        let flat = Bands::new(Mode::Details, body(), 20.0, &[], 10);
        assert_eq!(flat.columns(), 1);
        for from in [0usize, 4, 9] {
            assert_eq!(flat.step(from, Step::Left), flat.step(from, Step::Up), "at {from}");
            assert_eq!(flat.step(from, Step::Right), flat.step(from, Step::Down), "at {from}");
        }
        // And they clamp at both ends rather than wrapping.
        assert_eq!(flat.step(0, Step::Up), 0);
        assert_eq!(flat.step(9, Step::Down), 9);
    }

    /// A page is a screenful of lines, and never nothing.
    #[test]
    fn a_page_is_a_screenful_and_at_least_one_entry() {
        // 400 tall over 20-pixel rows is twenty rows, one column.
        assert_eq!(Bands::new(Mode::Details, body(), 20.0, &[], 100).page(), 20);
        // The grid pages by lines times columns.
        let grid = Bands::new(Mode::Grid, body(), 20.0, &[], 100);
        let lines = (400.0 / grid.cell().1).floor() as usize;
        assert_eq!(grid.page(), lines * grid.columns());
        // A body shorter than one cell still pages by something.
        let squat = Area { x: 0.0, y: 0.0, width: 800.0, height: 4.0 };
        assert!(Bands::new(Mode::Details, squat, 20.0, &[], 50).page() >= 1);
    }

    /// An empty listing answers every question without dividing by zero.
    #[test]
    fn an_empty_listing_has_no_cells_and_no_arithmetic_faults() {
        for mode in [Mode::Details, Mode::Grid] {
            for runs in [&[][..], &[(1u8, 0usize)][..]] {
                let bands = Bands::new(mode, body(), 20.0, runs, 0);
                assert_eq!(bands.visible(0.0), 0..0, "{mode:?} {runs:?}");
                assert_eq!(bands.step(0, Step::Down), 0);
                assert_eq!(bands.index_at(200.0, body().y + 2.0, 0.0), None);
                assert!(bands.page() >= 1);
                let mut touched = 0;
                bands.touching(body(), 0.0, |_| touched += 1);
                assert_eq!(touched, 0, "an empty listing selected something");
            }
        }
    }

    /// The tokens the session file writes round-trip, and every grouping has a
    /// label for every band it can produce.
    #[test]
    fn every_grouping_survives_the_session_file_and_labels_all_of_its_bands() {
        for grouping in Grouping::ALL {
            assert_eq!(Grouping::from_token(grouping.token()), Some(grouping), "{grouping:?}");
            // **Every band, and one past the end.** A `label` that indexed
            // instead of clamping would panic on a byte the sort never received,
            // which is exactly what an unclassified kind is.
            let mut seen: Vec<&str> = Vec::new();
            for band in 0..=u8::MAX {
                let key = grouping.label(band);
                assert!(!key.is_empty());
                if !seen.contains(&key) {
                    seen.push(key);
                }
            }
            let wanted = match grouping {
                Grouping::None => 1,
                Grouping::Time => sk_ui::format::BANDS,
                Grouping::Size => sk_ui::format::SIZE_BANDS,
                Grouping::Kind => sk_desktop::Category::ALL.len(),
            };
            assert_eq!(seen.len(), wanted, "{grouping:?} has the wrong number of labels");
        }
        assert_eq!(Grouping::from_token("by-shape"), None, "a grouping that does not exist");
        // And it must never be read as a mode or a number: the session's tolerant
        // parser tells the three apart by exactly this.
        assert_eq!(Grouping::from_token("grid"), None);
        assert_eq!(Grouping::from_token("0"), None);
    }

    #[test]
    fn lines_rounds_up_and_never_divides_by_zero() {
        assert_eq!(lines(0, 4), 0);
        assert_eq!(lines(1, 4), 1);
        assert_eq!(lines(8, 4), 2);
        assert_eq!(lines(9, 4), 3);
        assert_eq!(lines(9, 0), 9, "no columns is one column");
    }
}
