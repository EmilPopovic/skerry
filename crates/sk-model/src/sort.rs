// SPDX-License-Identifier: Apache-2.0
//! What order a listing is in.

/// Which column the order is derived from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortColumn {
    /// Natural-order collation over the name — see [`crate::natural_cmp`].
    #[default]
    Name,
    /// Size in bytes. Directories carry no meaningful size, so the name breaks
    /// the tie rather than the order being arbitrary.
    Size,
    /// Modification time in seconds since the epoch.
    Modified,
}

impl SortColumn {
    /// The catalogue key for this column's header.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Name => "column.name",
            Self::Size => "column.size",
            Self::Modified => "column.modified",
        }
    }
}

/// Where a numeric quantity is cut, as edges in **descending** order.
///
/// **Numbers, not a calendar and not a unit table.** Which band a value falls in
/// is six comparisons; working out where the edges are needs a timezone database
/// for a time and a house style for a size, and both belong at a level that has
/// one — `sk_ui::format::day_bands` and `sk_ui::format::SIZE_EDGES` produce these.
///
/// Descending, so band `0` is the one above every edge: the newest times and the
/// largest sizes come first, which is the order both readings want.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cuts {
    edges: [i64; Self::MOST],
    count: u8,
}

impl Cuts {
    /// The most edges a set of cuts may carry.
    pub const MOST: usize = 6;

    /// Cuts at `edges`, which must run largest first.
    #[must_use]
    pub fn new(edges: &[i64]) -> Self {
        let mut held = [0i64; Self::MOST];
        let count = edges.len().min(Self::MOST);
        held[..count].copy_from_slice(&edges[..count]);
        Self { edges: held, count: count as u8 }
    }

    /// Which band a value falls in. `0` is above every edge.
    ///
    /// The count of edges it falls below, which works out to the band index
    /// because the edges run largest first — and it stays correct when two edges
    /// are equal, which is a band nothing can fall into rather than a mistake.
    #[must_use]
    pub fn band_of(&self, value: i64) -> u8 {
        self.edges[..self.count as usize].iter().filter(|edge| value < **edge).count() as u8
    }
}

/// What a listing is cut into bands by, ahead of every other sort key.
///
/// **A grouping, not an order.** Like [`Sort::dirs_first`] it is not reversed by
/// `descending`, which reverses the order *within* a band and never the bands
/// themselves.
///
/// [`Self::None`] is one band, which is every listing that is not grouped, and is
/// what makes grouping cost nothing when it is off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Banding {
    /// One band, no headings.
    #[default]
    None,
    /// Cut on the modification time.
    Time(Cuts),
    /// Cut on the size the sort would use, with directories in a band of their
    /// own ahead of every size band — see [`crate::Listing::band_of`].
    Size(Cuts),
    /// Cut on a byte supplied from above.
    ///
    /// **This level never learns what the byte means.** A file's kind is a
    /// question about MIME types and the desktop's databases, which live three
    /// levels up; all a sort needs is a number to group equal values by, so that
    /// is all this carries. The bytes arrive alongside the listing — see
    /// [`crate::Listing::compute_order`] — and whoever supplies them owns their
    /// order and their labels.
    Kind,
}

impl Banding {
    /// Whether there is any banding at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// A complete sort order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    /// The column the order is derived from.
    pub column: SortColumn,
    /// Whether the order runs the other way.
    pub descending: bool,
    /// Whether directories are grouped ahead of files regardless of column.
    ///
    /// On by default because it is what every file manager a user has already
    /// learned does, and because a directory's size column is empty, so mixing
    /// them into a size sort produces a block of blanks in the middle.
    pub dirs_first: bool,
    /// What the entries are cut into bands by, ahead of every other key.
    ///
    /// **A grouping, not an order**, which is exactly what `dirs_first` is and
    /// why it sits beside it. [`Banding::None`] for a listing that is not grouped.
    pub bands: Banding,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            descending: false,
            dirs_first: true,
            bands: Banding::default(),
        }
    }
}

impl Sort {
    /// The order after clicking `column`: the same column reverses, a different
    /// column starts ascending.
    #[must_use]
    pub fn toggled(self, column: SortColumn) -> Self {
        if self.column == column {
            Self { descending: !self.descending, ..self }
        } else {
            Self { column, descending: false, ..self }
        }
    }
}
