// SPDX-License-Identifier: Apache-2.0
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
    /// Today, yesterday, the previous week, the previous month, this year, and
    /// older — see [`sk_ui::format::day_bands`], which decides where each begins.
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
    /// A band with nothing in it is never drawn, which is what happens to "in
    /// the future" almost always, to "earlier this year" through January, and to
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
                "band.previous_7_days",
                "band.previous_30_days",
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

/// Which of a grouping's bands are collapsed.
///
/// **A bitmask over band bytes, not a set of them.** A grouping has at most nine
/// bands — seven stretches of time, eight sizes, nine kinds — so the whole answer
/// fits in sixteen bits, is `Copy`, and compares in one instruction. A collection
/// would allocate to say "nothing is closed", which is what it says almost always.
///
/// **A collapse and not a filter, which is the difference that makes it honest.**
/// A closed band keeps its heading, and the heading says how many entries are
/// behind it. The name filter clears on every navigation because "arriving
/// somewhere that silently hides most of what is there is not a view, it is a
/// trap" — this hides nothing silently. It clears on navigation all the same, for
/// the other half of that reason: band four means a different set of files in every
/// folder, so carrying one across a navigation would close something nobody chose.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Closed(u16);

impl Closed {
    /// The most bands this can speak about: sixteen, against the nine any
    /// grouping produces.
    ///
    /// A byte past the end is **clamped rather than dropped**, exactly the way
    /// [`Grouping::label`] clamps. The byte and the label have to agree about which
    /// band they mean, or a heading would close a band other than the one it names.
    const MOST: u8 = 16;

    fn bit(band: u8) -> u16 {
        1 << band.min(Self::MOST - 1)
    }

    /// Whether `band` is collapsed.
    #[must_use]
    pub fn has(self, band: u8) -> bool {
        self.0 & Self::bit(band) != 0
    }

    /// Collapses `band`, or opens it.
    pub fn toggle(&mut self, band: u8) {
        self.0 ^= Self::bit(band);
    }

    /// Opens everything. Whether anything was closed.
    pub fn open_everything(&mut self) -> bool {
        std::mem::take(&mut self.0) != 0
    }

    /// Whether anything at all is collapsed.
    #[must_use]
    pub fn any(self) -> bool {
        self.0 != 0
    }
}

/// The first display row an open band holds, or nothing when all of them are shut.
///
/// Where a cursor goes when the band it was in closes under it.
pub fn first_open(runs: &[(u8, usize)], closed: Closed) -> Option<usize> {
    if runs.is_empty() {
        return Some(0);
    }
    runs.iter().find(|(band, _)| !closed.has(*band)).map(|(_, first)| *first)
}

/// Calls `each` with each band's display rows and whether that band is collapsed.
///
/// **One walk, because the two questions that need it are opposites.** Which rows
/// must leave a selection, and which rows `Ctrl+A` may put into one — and
/// `sk_model::Selection::select_all`'s own comment says why they have to agree: a
/// selection that contains something invisible is a selection a later operation
/// acts on by surprise. Closing a band is the second way to make one, after hidden
/// files.
///
/// An ungrouped listing is one band, open, which is what makes the ungrouped case
/// the same code path rather than a branch at each caller.
pub fn each_band(
    runs: &[(u8, usize)],
    count: usize,
    closed: Closed,
    mut each: impl FnMut(std::ops::Range<usize>, bool),
) {
    if runs.is_empty() {
        each(0..count, false);
        return;
    }
    for (at, (band, first)) in runs.iter().enumerate() {
        let end = runs.get(at + 1).map_or(count, |(_, next)| *next);
        each((*first).min(count)..end.min(count), closed.has(*band));
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
    /// How many entries the *run* holds, drawn or not.
    ///
    /// Not `cells.len()`, which is zero while the band is collapsed. The heading
    /// says this number, which is the whole reason a collapse is not a filter:
    /// nothing is hidden without a line on screen saying how much.
    held: usize,
    /// Whether it is collapsed: a heading, and no rows anybody can reach.
    closed: bool,
    /// The y its heading sits at, in content coordinates.
    top: f32,
    /// Its entries, over the strip below the heading. Empty while collapsed.
    cells: Cells,
}

impl Band {
    /// How many entries it lays out. Zero while collapsed.
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
        closed: Closed,
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
            // **A collapsed band lays out no cells at all**, and that one line is
            // what makes every method below answer "nothing" for its rows: an empty
            // [`Cells`] has no lines, no content height and no index at any point,
            // so `visible` yields an empty range for it, `touching` touches none of
            // it, and `index_at` inside its strip is already `None` because a
            // heading is not an entry. Guards in those three were the first draft
            // and each was unreachable.
            //
            // Not cells of zero *height*: `Cells` answers from the rectangle it was
            // given, and a rectangle with no height still contains its own top edge.
            // The count is the thing that has to be zero.
            let shut = closed.has(*kind);
            let laid = if shut { 0 } else { held };
            bands.push(Band {
                kind: *kind,
                first: *first,
                held,
                closed: shut,
                top,
                cells: strip(top, laid),
            });
            top += heading + lines(laid, cells.columns()) as f32 * cells.cell().1;
        }
        // A listing always has a band, so every method below can take one rather
        // than asking whether there is one.
        if bands.is_empty() {
            bands.push(Band {
                kind: 0,
                first: 0,
                held: count,
                closed: false,
                top: 0.0,
                cells: strip(0.0, count),
            });
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
    ///
    /// **A collapsed band's heading, for a row inside one.** Nothing draws such a
    /// row, so there is no honest rectangle — and an empty [`Cells`] would answer
    /// with its own origin, which is a position on top of the heading rather than
    /// a statement that there is none. A re-sort can move the cursor's entry into a
    /// band that was already shut, so this is reachable however carefully the
    /// selection is tidied.
    pub(crate) fn at(&self, index: usize, offset: f32) -> Area {
        let band = self.band_of(index);
        if band.closed {
            return Area { y: self.body.y + band.top - offset, height: self.heading, ..self.body };
        }
        band.cells.at(index.saturating_sub(band.first), offset)
    }

    /// The top and bottom of an entry's line, in content coordinates.
    ///
    /// **The heading is part of the span.** Revealing the first entry of a band
    /// has to bring its heading with it, or the cursor lands under a heading that
    /// is scrolled off and the pane says nothing about which band it is in.
    pub(crate) fn span(&self, index: usize) -> (f32, f32) {
        let band = self.band_of(index);
        if band.closed {
            // Revealing a row nothing draws reveals the heading it is behind,
            // which is the thing a person has to press to see it.
            return (band.top, band.top + self.heading);
        }
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
        // **A collapsed band is not somewhere a key can land.** Every direction
        // below walks `placed` rather than counting indices, because with a band
        // shut the indices are no longer contiguous: a listing of twelve can show
        // rows 0–2 and 7–11, and `from + 1` from row 2 is a row nobody can see.
        let Some(here) = self.open_at(from) else {
            return self.first_shown().unwrap_or(0);
        };
        match step {
            Step::Left => self.neighbour(here, from, false),
            Step::Right => self.neighbour(here, from, true),
            Step::Up => self.vertically(here, from, false),
            Step::Down => self.vertically(here, from, true),
        }
    }

    /// The open band an index falls in, if it falls in one.
    fn open_at(&self, index: usize) -> Option<&Band> {
        self.placed
            .iter()
            .find(|band| !band.closed && index >= band.first && index < band.first + band.held)
    }

    /// The first row any open band holds.
    fn first_shown(&self) -> Option<usize> {
        self.placed.iter().find(|band| !band.closed).map(|band| band.first)
    }

    /// The open band after this one, or before it.
    fn beside(&self, band: &Band, forward: bool) -> Option<&Band> {
        if forward {
            self.placed.iter().find(|it| it.first > band.first && !it.closed)
        } else {
            self.placed.iter().rev().find(|it| it.first < band.first && !it.closed)
        }
    }

    /// The previous or next entry in reading order, skipping what is collapsed.
    fn neighbour(&self, band: &Band, from: usize, forward: bool) -> usize {
        if forward {
            if from + 1 < band.first + band.held {
                return from + 1;
            }
            self.beside(band, true).map_or(from, |next| next.first)
        } else {
            if from > band.first {
                return from - 1;
            }
            self.beside(band, false).map_or(from, |prior| prior.first + prior.held - 1)
        }
    }

    /// One line down or up, crossing into the neighbouring open band where it must.
    fn vertically(&self, band: &Band, from: usize, down: bool) -> usize {
        let columns = self.columns();
        let local = from.saturating_sub(band.first);
        let column = local % columns;
        if down {
            if local + columns < band.held {
                return from + columns;
            }
            let Some(next) = self.beside(band, true) else { return from };
            next.first + column.min(next.held - 1)
        } else {
            if local >= columns {
                return from - columns;
            }
            let Some(prior) = self.beside(band, false) else { return from };
            // The last line of the band above, at the same column where it has
            // one — a ragged last line is shorter than the columns.
            let last_line = prior.first + (lines(prior.held, columns) - 1) * columns;
            (last_line + column).min(prior.first + prior.held - 1)
        }
    }

    /// The entries that are on screen at this offset, in display order.
    ///
    /// **Not one range, because with a band collapsed the rows are not
    /// contiguous.** A listing of twelve with the middle band shut shows rows 0–3
    /// and 8–11, and a bounding range over those two would hand the caller four
    /// rows that are drawn nowhere — which is what a draw loop then draws, on top
    /// of the closed band's heading. One range per open band, chained.
    pub(crate) fn visible(&self, offset: f32) -> impl Iterator<Item = usize> + '_ {
        let (top, bottom) = (offset, offset + self.body.height);
        let height = self.cell().1;
        let columns = self.columns();
        let heading = self.heading;
        self.placed.iter().flat_map(move |band| {
            // **Not `band.cells.visible`.** That asks "what fits in this band's
            // own strip", and the question here is what fits in the *pane's*
            // viewport, which is a different rectangle and usually a larger one.
            let cells_top = band.cells_top(heading);
            if cells_top + band.cells.content_height() < top || cells_top > bottom {
                return band.first..band.first;
            }
            let first_line = ((top - cells_top) / height).floor().max(0.0) as usize;
            let last_line =
                (((bottom - cells_top) / height).ceil().max(0.0) as usize).min(band.cells.lines());
            let from = band.first + first_line * columns;
            let to = (band.first + last_line * columns).min(band.first + band.count());
            from.min(to)..to
        })
    }

    /// Every entry whose cell the rectangle touches, in index order.
    pub(crate) fn touching(&self, rect: Area, offset: f32, mut each: impl FnMut(usize)) {
        for band in &self.placed {
            // A collapsed band's `Cells` is empty, so a rubber band cannot reach
            // into one — see `Bands::new`.
            band.cells.touching(rect, offset, |local| each(band.first + local));
        }
    }

    /// Each heading that is on screen.
    pub(crate) fn headings(&self, offset: f32) -> Vec<Heading> {
        if self.heading <= 0.0 {
            return Vec::new();
        }
        self.placed
            .iter()
            .filter_map(|band| {
                let y = self.body.y + band.top - offset;
                let on_screen = y + self.heading > self.body.y && y < self.body.bottom();
                on_screen.then_some(Heading {
                    kind: band.kind,
                    at: Area { x: self.body.x, y, width: self.body.width, height: self.heading },
                    held: band.held,
                    closed: band.closed,
                })
            })
            .collect()
    }

    /// Which band's heading is under a point, if one is.
    ///
    /// **The pointer's way in.** [`Self::index_at`] already answers `None` on a
    /// heading, because a heading is not an entry; this is the other half of that
    /// sentence — it is not an entry, and it *is* a control.
    pub(crate) fn heading_under(&self, x: f32, y: f32, offset: f32) -> Option<u8> {
        self.headings(offset).into_iter().find(|it| it.at.contains(x, y)).map(|it| it.kind)
    }
}

/// One band's heading, on screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Heading {
    /// Which band it names.
    pub(crate) kind: u8,
    /// Where it is drawn, and what a press on it hits.
    pub(crate) at: Area,
    /// How many entries the band holds, drawn or not.
    pub(crate) held: usize,
    /// Whether the band is collapsed.
    pub(crate) closed: bool,
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
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Bands, Closed, Grouping, lines};
    use crate::cells::{Mode, Step};
    use crate::layout::Area;

    fn body() -> Area {
        Area { x: 100.0, y: 50.0, width: 800.0, height: 400.0 }
    }

    /// Three bands of four, laid out as one column so the arithmetic is legible.
    fn stacked() -> Bands {
        Bands::new(Mode::Details, body(), 20.0, &[(1, 0), (2, 4), (6, 8)], 12, Closed::default())
    }

    /// No runs is one band with no heading, which is the ungrouped pane.
    #[test]
    fn no_grouping_is_one_band_and_no_heading_at_all() {
        let flat = Bands::new(Mode::Details, body(), 20.0, &[], 10, Closed::default());
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
        let first = bands.headings(0.0)[0].at;
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
            let bands = Bands::new(
                mode,
                body(),
                20.0,
                &[(1, 0), (2, 5), (4, 6), (6, 13)],
                20,
                Closed::default(),
            );
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
        let bands = Bands::new(Mode::Grid, wide, 26.0, &[(1, 0), (5, 6)], 9, Closed::default());
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
        let bands = Bands::new(
            Mode::Details,
            body(),
            20.0,
            &[(1, 0), (3, 0), (6, 4)],
            8,
            Closed::default(),
        );
        assert_eq!(bands.count(), 2, "an empty band was drawn");
        let kinds: Vec<u8> = bands.headings(0.0).into_iter().map(|it| it.kind).collect();
        assert_eq!(kinds, [3, 6]);
    }

    /// What is visible covers the screen, and a band scrolled past is skipped.
    #[test]
    fn the_visible_range_walks_the_bands_it_overlaps() {
        let bands = stacked();
        let shown: Vec<usize> = bands.visible(0.0).collect();
        assert_eq!(shown.first(), Some(&0));
        assert!(shown.len() >= 4, "the first screen holds at least a band");
        // In display order, and every row exactly once.
        assert!(shown.windows(2).all(|pair| pair[0] < pair[1]), "out of order: {shown:?}");
        // Scrolled to the bottom, and past it, nothing is past the last entry.
        for offset in [bands.content_height(), 1e9] {
            assert!(bands.visible(offset).all(|row| row < 12), "a row past the end at {offset}");
        }
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
        let flat = Bands::new(Mode::Details, body(), 20.0, &[], 10, Closed::default());
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
        assert_eq!(Bands::new(Mode::Details, body(), 20.0, &[], 100, Closed::default()).page(), 20);
        // The grid pages by lines times columns.
        let grid = Bands::new(Mode::Grid, body(), 20.0, &[], 100, Closed::default());
        let lines = (400.0 / grid.cell().1).floor() as usize;
        assert_eq!(grid.page(), lines * grid.columns());
        // A body shorter than one cell still pages by something.
        let squat = Area { x: 0.0, y: 0.0, width: 800.0, height: 4.0 };
        assert!(Bands::new(Mode::Details, squat, 20.0, &[], 50, Closed::default()).page() >= 1);
    }

    /// Band 2 of `stacked` shut: three bands of four, the middle one collapsed.
    fn with_the_middle_shut() -> (Bands, Closed) {
        let mut closed = Closed::default();
        closed.toggle(2);
        let bands = Bands::new(Mode::Details, body(), 20.0, &[(1, 0), (2, 4), (6, 8)], 12, closed);
        (bands, closed)
    }

    /// **A collapsed band keeps its heading and lends out no rows.** Which is the
    /// whole design: the heading is what a person presses to get the rows back, so
    /// a collapse that took it away would be a filter nobody could undo.
    #[test]
    fn a_collapsed_band_keeps_its_heading_and_hides_its_rows() {
        let (shut, _) = with_the_middle_shut();
        let open = stacked();

        // Three headings either way, and the shut one says how many it holds.
        assert_eq!(shut.count(), 3, "a collapsed band lost its heading");
        let headings = shut.headings(0.0);
        assert_eq!(headings.len(), 3);
        assert_eq!(headings.iter().filter(|it| it.closed).count(), 1);
        let middle = headings.iter().find(|it| it.kind == 2).expect("band 2 has no heading");
        assert!(middle.closed);
        assert_eq!(middle.held, 4, "the heading forgot what it is holding");
        assert!(headings.iter().filter(|it| !it.closed).all(|it| it.held == 4));

        // Exactly four rows' worth shorter: the heading stays, the lines go.
        let lost = open.content_height() - shut.content_height();
        assert!((lost - 4.0 * 20.0).abs() < 0.01, "it shrank by {lost}, not four rows");

        // None of its rows are drawn, and the ones around it still are.
        let rows: Vec<usize> = shut.visible(0.0).collect();
        assert!(rows.contains(&0) && rows.contains(&8), "an open band went with it");
        assert!(!rows.iter().any(|row| (4..8).contains(row)), "a shut band drew rows: {rows:?}");

        // A press inside its strip is not an entry, and a rubber band over the
        // whole pane does not reach in.
        let strip = middle.at;
        assert_eq!(shut.index_at(strip.x + 50.0, strip.y + 2.0, 0.0), None);
        let mut touched = Vec::new();
        shut.touching(body(), 0.0, |row| touched.push(row));
        assert!(!touched.iter().any(|row| (4..8).contains(row)), "touched: {touched:?}");
        assert_eq!(touched.len(), 8, "the open bands lost rows too");
    }

    /// **The keys walk over a collapsed band rather than into it.** With the
    /// indices no longer contiguous, `from + 1` from row 3 is a row nobody can see
    /// — which is why every direction asks `placed` instead of counting.
    #[test]
    fn every_direction_steps_over_a_collapsed_band() {
        let (shut, _) = with_the_middle_shut();
        // One column, so Down and Right are the same move and both are testable.
        assert_eq!(shut.columns(), 1);

        for forward in [Step::Right, Step::Down] {
            assert_eq!(shut.step(3, forward), 8, "{forward:?} landed inside the shut band");
            assert_eq!(shut.step(8, forward), 9);
            assert_eq!(shut.step(11, forward), 11, "{forward:?} ran off the end");
        }
        for back in [Step::Left, Step::Up] {
            assert_eq!(shut.step(8, back), 3, "{back:?} landed inside the shut band");
            assert_eq!(shut.step(1, back), 0);
            assert_eq!(shut.step(0, back), 0, "{back:?} ran off the front");
        }

        // And a cursor that is already inside one — which a re-sort can arrange —
        // is taken to the first row anybody can see rather than left there.
        assert_eq!(shut.step(5, Step::Down), 0);
    }

    /// A row nothing draws still answers `at` and `span`, with its heading.
    ///
    /// **Reachable however carefully the selection is tidied**: a re-sort can move
    /// the cursor's entry into a band that was already shut. An empty `Cells` would
    /// answer with its own origin, which is a rectangle on top of the heading
    /// pretending to be a row.
    #[test]
    fn a_hidden_row_is_where_its_heading_is() {
        let (shut, _) = with_the_middle_shut();
        let heading =
            shut.headings(0.0).into_iter().find(|it| it.kind == 2).expect("band 2 has no heading");
        for row in 4..8 {
            let at = shut.at(row, 0.0);
            assert!((at.y - heading.at.y).abs() < 0.01, "row {row} is at {}", at.y);
            let (top, bottom) = shut.span(row);
            assert!(bottom > top, "row {row} has no span");
            assert!(bottom - top <= 24.01, "row {row} spans {} of nothing", bottom - top);
        }
    }

    /// The bitmask clamps a byte past its width, exactly as `label` clamps.
    ///
    /// The byte and the label have to agree about which band they mean, or a
    /// heading would close a band other than the one it names.
    #[test]
    fn a_band_byte_past_the_mask_is_clamped_and_not_dropped() {
        let mut closed = Closed::default();
        assert!(!closed.any());
        closed.toggle(u8::MAX);
        assert!(closed.any(), "a byte past the end was dropped");
        assert!(closed.has(u8::MAX));
        assert!(closed.has(15), "the clamp is not the same on both sides");
        assert!(!closed.has(0));
        assert!(closed.open_everything());
        assert!(!closed.open_everything(), "opening nothing reported a change");
    }

    /// Every band's rows, once, whether or not the pane is grouped.
    #[test]
    fn each_band_covers_the_listing_exactly_once() {
        let mut closed = Closed::default();
        closed.toggle(2);
        let runs = [(1u8, 0usize), (2, 4), (6, 8)];
        let mut seen: Vec<usize> = Vec::new();
        let mut hidden: Vec<usize> = Vec::new();
        super::each_band(&runs, 12, closed, |rows, shut| {
            for row in rows {
                seen.push(row);
                if shut {
                    hidden.push(row);
                }
            }
        });
        assert_eq!(seen, (0..12).collect::<Vec<_>>());
        assert_eq!(hidden, vec![4, 5, 6, 7]);
        assert_eq!(super::first_open(&runs, closed), Some(0));

        // An ungrouped listing is one open band, which is what keeps the callers
        // free of a special case.
        let mut flat = Vec::new();
        super::each_band(&[], 5, closed, |rows, shut| flat.push((rows, shut)));
        assert_eq!(flat, vec![(0..5, false)]);
        assert_eq!(super::first_open(&[], closed), Some(0));

        // Everything shut: there is nowhere for a cursor to go.
        let mut all = Closed::default();
        for band in [1u8, 2, 6] {
            all.toggle(band);
        }
        assert_eq!(super::first_open(&runs, all), None);
    }

    /// An empty listing answers every question without dividing by zero.
    #[test]
    fn an_empty_listing_has_no_cells_and_no_arithmetic_faults() {
        for mode in [Mode::Details, Mode::Grid] {
            for runs in [&[][..], &[(1u8, 0usize)][..]] {
                let bands = Bands::new(mode, body(), 20.0, runs, 0, Closed::default());
                assert_eq!(bands.visible(0.0).count(), 0, "{mode:?} {runs:?}");
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
                // **And it resolves.** These keys sit in an array rather than
                // inside a `lookup(…)` call, which is the one shape
                // `tools/check-invariants.py` cannot see — verified by putting a
                // key that is in no catalogue here and watching it report `ok`.
                // A missing key renders as itself, so a renamed band with no
                // catalogue line would draw `band.previous_7_days` as a heading.
                assert_ne!(sk_model::i18n::lookup(key), key, "{key} is not in the catalogue");
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
