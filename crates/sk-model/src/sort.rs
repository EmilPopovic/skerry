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
}

impl Default for Sort {
    fn default() -> Self {
        Self { column: SortColumn::Name, descending: false, dirs_first: true }
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
