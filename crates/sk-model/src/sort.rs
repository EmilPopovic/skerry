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

/// Where a listing is cut into bands, as epoch seconds, newest edge first.
///
/// **Numbers, not a calendar.** Which band a file falls in is six comparisons;
/// working out where the edges are needs a timezone database and belongs at a
/// level that may have one — `sk_ui::format::day_bands` produces these.
///
/// Empty means one band, which is every listing that is not grouped, and is what
/// makes grouping cost nothing when it is off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Banding {
    edges: [i64; Self::MOST],
    count: u8,
}

impl Banding {
    /// The most edges a banding may carry.
    pub const MOST: usize = 6;

    /// A banding over `edges`, which must run newest first.
    #[must_use]
    pub fn new(edges: &[i64]) -> Self {
        let mut held = [0i64; Self::MOST];
        let count = edges.len().min(Self::MOST);
        held[..count].copy_from_slice(&edges[..count]);
        Self { edges: held, count: count as u8 }
    }

    /// Whether there is any banding at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// How many bands the edges make: one more than there are edges.
    #[must_use]
    pub const fn bands(&self) -> usize {
        self.count as usize + 1
    }

    /// Which band a time falls in. `0` is the newest.
    ///
    /// The count of edges it is older than, which works out to the band index
    /// because the edges run newest first — and it stays correct when two edges
    /// are equal, which is a band nothing can fall into rather than a mistake.
    #[must_use]
    pub fn band_of(&self, seconds: i64) -> u8 {
        self.edges[..self.count as usize].iter().filter(|edge| seconds < **edge).count() as u8
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
    /// Where the entries are cut into bands, ahead of every other key.
    ///
    /// **A grouping, not an order**, which is exactly what `dirs_first` is and
    /// why it sits beside it: `descending` reverses the order *within* a band and
    /// never the bands themselves. Empty for a listing that is not grouped.
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
