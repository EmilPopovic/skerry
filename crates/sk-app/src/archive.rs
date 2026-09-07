// SPDX-License-Identifier: Apache-2.0
//! An archive, from the window's side: what to call the folder it unpacks into,
//! and what the panel shows when somebody looks inside one.
//!
//! **[ADR 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).**
//! The bytes are read by a sandboxed child and everything here is over its
//! answer: a flat table of paths, which this turns into the one folder somebody
//! is looking at.
//!
//! # Why the folders are derived rather than read
//!
//! **Whether an archive declares its directories is up to whatever wrote it.**
//! `zip` and 7-Zip record them; a program calling a zip library's "write this
//! blob under this name" does not, and then `deep/inner/note.md` is the only
//! record that `deep` exists — a panel that listed the entries whose kind is
//! `Directory` would show that archive as one file and no folders. What is here
//! builds the level being looked at out of the paths themselves, which is right
//! for both kinds and costs one pass over a table already in memory.

use sk_preview::archive::{Kind, Listing};

/// One row of what the panel is showing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    /// What is drawn: the name at this level, with nothing above it.
    pub(crate) name: String,
    /// The whole path inside the archive, which is what an extraction names.
    pub(crate) path: String,
    /// The unpacked size, or everything under it for a folder.
    pub(crate) size: u64,
    /// When the archive says it was last written.
    pub(crate) mtime: i64,
    /// What it is.
    pub(crate) kind: Kind,
    /// How many entries are under it, for a folder. Zero for anything else.
    pub(crate) inside: u64,
}

/// An archive's contents, and where in them the panel is standing.
pub(crate) struct Inside {
    /// Everything the child sent, in the order it sent it.
    pub(crate) listing: Listing,
    /// The folder being looked at: empty, or a path ending in a separator.
    pub(crate) at: String,
    /// The rows at that level, rebuilt whenever it changes.
    pub(crate) rows: Vec<Row>,
    /// Which row is chosen, as an index into `rows`.
    pub(crate) chosen: usize,
    /// How far the list is scrolled.
    pub(crate) scroll: sk_ui::Scroll,
}

impl Inside {
    /// The top of `listing`.
    pub(crate) fn new(listing: Listing) -> Self {
        let mut inside = Self {
            listing,
            at: String::new(),
            rows: Vec::new(),
            chosen: 0,
            scroll: sk_ui::Scroll::default(),
        };
        inside.settle();
        inside
    }

    /// Rebuilds the rows for the level being looked at.
    fn settle(&mut self) {
        self.rows = level(&self.listing, &self.at);
        self.chosen = self.chosen.min(self.rows.len().saturating_sub(1));
    }

    /// The row the cursor is on.
    pub(crate) fn row(&self) -> Option<&Row> {
        self.rows.get(self.chosen)
    }

    /// Moves the cursor, staying inside the list.
    pub(crate) fn step(&mut self, by: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let wanted = self.chosen.cast_signed() + by;
        self.chosen = wanted.clamp(0, last.cast_signed()).cast_unsigned();
    }

    /// Scrolls so the chosen row is on screen, given the room and the row height.
    ///
    /// **Not in the drawing.** A frame that scrolled would be a frame with a side
    /// effect, and the arrow that moved the cursor is what knows the cursor moved
    /// — the same split the pane's own `reveal_cursor` makes.
    pub(crate) fn reveal(&mut self, room: f32, step: f32) {
        self.scroll.set_extent(self.rows.len() as f32 * step, room);
        let top = self.chosen as f32 * step;
        let offset = self.scroll.offset();
        if top < offset {
            self.scroll.to(top);
        } else if top + step > offset + room {
            self.scroll.to(top + step - room);
        }
    }

    /// Goes into the chosen folder, and says whether it did.
    pub(crate) fn enter(&mut self) -> bool {
        let Some(row) = self.row() else { return false };
        if row.kind != Kind::Directory {
            return false;
        }
        self.at = format!("{}/", row.path);
        self.chosen = 0;
        self.scroll = sk_ui::Scroll::default();
        self.settle();
        true
    }

    /// Goes up one level, and says whether there was one.
    ///
    /// The cursor lands on the folder just left, which is where a person who
    /// pressed `←` by mistake needs it.
    pub(crate) fn leave(&mut self) -> bool {
        if self.at.is_empty() {
            return false;
        }
        let was = self.at.trim_end_matches('/').to_owned();
        self.at = match was.rsplit_once('/') {
            Some((above, _)) => format!("{above}/"),
            None => String::new(),
        };
        self.settle();
        self.chosen = self.rows.iter().position(|row| row.path == was).unwrap_or(0);
        self.scroll = sk_ui::Scroll::default();
        true
    }

    /// Stands in `path`, however deep it is.
    ///
    /// **What `--capture` uses and the window does not**: the panel walks one
    /// level at a time, and a frame of the fourth level would otherwise be a
    /// frame of three Enter presses.
    pub(crate) fn go_to(&mut self, path: &str) {
        self.at = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        self.chosen = 0;
        self.scroll = sk_ui::Scroll::default();
        self.settle();
    }

    /// What a screen reader is given: the level being looked at, a row to a line.
    ///
    /// **Names and kinds, not sizes.** A reader moves through this to find out
    /// what is in the archive; a column of byte counts between the names is the
    /// thing that makes a spoken list unusable.
    pub(crate) fn spoken(&self) -> String {
        if self.rows.is_empty() {
            return sk_model::i18n::lookup("archive.empty").to_owned();
        }
        self.rows
            .iter()
            .map(|row| match row.kind {
                Kind::Directory => {
                    format!("{}, {}", row.name, sk_model::i18n::lookup("kind.folder"))
                }
                _ => row.name.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The trail of folders above the one being looked at, outermost first.
    pub(crate) fn trail(&self) -> Vec<&str> {
        self.at.trim_end_matches('/').split('/').filter(|part| !part.is_empty()).collect()
    }
}

/// The rows one level down from `at`.
///
/// **A folder is anything with something under it.** The entries whose kind says
/// `Directory` are folded in with the ones only implied by a longer path, so an
/// archive that records its directories and one that does not produce the same
/// list — and a folder's size and count are everything beneath it, because that
/// is the number a person deciding whether to unpack it wants.
fn level(listing: &Listing, at: &str) -> Vec<Row> {
    let mut folders: Vec<Row> = Vec::new();
    let mut files: Vec<Row> = Vec::new();
    for entry in &listing.entries {
        let Some(rest) = entry.path.strip_prefix(at) else { continue };
        if rest.is_empty() {
            continue;
        }
        match rest.split_once('/') {
            // **A first component with no name in it.** An archive can hold
            // `/etc/passwd`, which the child lists as it found it and the writer
            // refuses — and which would otherwise be drawn as a nameless folder
            // row that a person could enter and find one nameless row inside.
            Some(("", _)) => {}
            Some((name, _)) => {
                let path = format!("{at}{name}");
                match folders.iter_mut().find(|row| row.path == path) {
                    Some(row) => {
                        row.size += entry.size;
                        row.inside += 1;
                        row.mtime = row.mtime.max(entry.mtime);
                    }
                    None => folders.push(Row {
                        name: name.to_owned(),
                        path,
                        size: entry.size,
                        mtime: entry.mtime,
                        kind: Kind::Directory,
                        inside: 1,
                    }),
                }
            }
            None if entry.kind == Kind::Directory => {
                // An explicit directory entry. It may already be here from a
                // child that came first, in which case its own record adds
                // nothing: the count is of what is inside it.
                if !folders.iter().any(|row| row.path == entry.path) {
                    folders.push(Row {
                        name: rest.to_owned(),
                        path: entry.path.clone(),
                        size: 0,
                        mtime: entry.mtime,
                        kind: Kind::Directory,
                        inside: 0,
                    });
                }
            }
            None => files.push(Row {
                name: rest.to_owned(),
                path: entry.path.clone(),
                size: entry.size,
                mtime: entry.mtime,
                kind: entry.kind,
                inside: 0,
            }),
        }
    }
    // Folders first and then names, which is the order the pane's own listing
    // uses and the one a person is already reading the window in.
    folders.sort_by(|left, right| left.name.cmp(&right.name));
    files.sort_by(|left, right| left.name.cmp(&right.name));
    folders.append(&mut files);
    folders
}

/// What the folder an archive unpacks into is called.
///
/// **The name with its suffixes taken off, and both of them for a `.tar.gz`.**
/// A `notes.tar.gz` that unpacked into `notes.tar` would leave a folder named
/// after a file that is not there.
pub(crate) fn folder_name(archive: &std::path::Path) -> String {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    for suffix in [
        ".tar.gz",
        ".tar.xz",
        ".tar.zst",
        ".tar.bz2",
        ".tar.lzma",
        ".tar",
        ".tgz",
        ".txz",
        ".tzst",
        ".tbz2",
        ".tbz",
        ".zip",
        ".gz",
        ".xz",
        ".zst",
        ".bz2",
        ".lzma",
    ] {
        if let Some(stem) = name.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return stem.to_owned();
        }
    }
    // Nothing recognisable to take off — an archive somebody renamed. A folder
    // beside it needs a name that is not the archive's own.
    format!("{name} unpacked")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use sk_preview::archive::{Container, Entry, Filter, Format, Refusal};

    use super::*;

    fn listing(paths: &[(&str, u64, Kind)]) -> Listing {
        Listing {
            format: Some(Format { container: Container::Zip, filter: Filter::Store }),
            entries: paths
                .iter()
                .map(|(path, size, kind)| Entry {
                    path: (*path).to_owned(),
                    size: *size,
                    mtime: 1_700_000_000,
                    kind: *kind,
                })
                .collect(),
            count: paths.len() as u64,
            bytes: paths.iter().map(|(_, size, _)| size).sum(),
            refusal: Refusal::None,
            note: String::new(),
        }
    }

    /// **The defect this module exists for.** A zip from most tools has no
    /// directory entries at all, so a panel that trusted the kinds would show one
    /// file and no folders.
    #[test]
    fn a_folder_nothing_declared_is_still_a_folder() {
        let flat = listing(&[
            ("deep/inner/note.md", 7, Kind::File),
            ("deep/blob.bin", 200, Kind::File),
            ("hello.txt", 14, Kind::File),
        ]);
        let inside = Inside::new(flat);
        let names: Vec<&str> = inside.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["deep", "hello.txt"]);
        let deep = &inside.rows[0];
        assert_eq!(deep.kind, Kind::Directory);
        assert_eq!(deep.inside, 2, "a folder counts everything under it");
        assert_eq!(deep.size, 207, "and adds up what is under it");
    }

    /// An archive that *does* record its directories reads the same way, and the
    /// explicit entry does not become a second row beside the derived one.
    #[test]
    fn a_declared_folder_is_one_row_and_not_two() {
        let declared = listing(&[
            ("deep", 0, Kind::Directory),
            ("deep/note.md", 7, Kind::File),
            ("hello.txt", 14, Kind::File),
        ]);
        let inside = Inside::new(declared);
        let names: Vec<&str> = inside.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["deep", "hello.txt"]);
    }

    /// Walking in and out lands where a person expects, both ways.
    #[test]
    fn going_in_and_coming_back_lands_on_the_folder_it_left() {
        let tree = listing(&[
            ("a/one.txt", 1, Kind::File),
            ("b/two.txt", 2, Kind::File),
            ("b/deeper/three.txt", 3, Kind::File),
            ("z.txt", 4, Kind::File),
        ]);
        let mut inside = Inside::new(tree);
        assert_eq!(inside.rows.len(), 3);

        inside.step(1);
        assert_eq!(inside.row().map(|row| row.name.as_str()), Some("b"));
        assert!(inside.enter());
        assert_eq!(inside.at, "b/");
        let names: Vec<&str> = inside.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["deeper", "two.txt"], "one level, not the whole archive");
        assert_eq!(inside.trail(), ["b"]);

        assert!(inside.enter(), "into `deeper`");
        assert_eq!(inside.trail(), ["b", "deeper"]);
        assert!(inside.leave());
        assert_eq!(inside.row().map(|row| row.name.as_str()), Some("deeper"), "back on it");
        assert!(inside.leave());
        assert_eq!(inside.row().map(|row| row.name.as_str()), Some("b"));
        assert!(!inside.leave(), "there is nothing above the top");
    }

    /// An entry whose path is absolute is listed and is not a row with no name.
    ///
    /// The writer refuses it — a path that escapes is not repaired here — so what
    /// this is about is the panel: a nameless folder that opens onto one nameless
    /// row is worse than an entry that is simply not shown.
    #[test]
    fn an_absolute_entry_is_not_a_folder_with_no_name() {
        let hostile = listing(&[("/etc/passwd", 1, Kind::File), ("ordinary.txt", 2, Kind::File)]);
        let inside = Inside::new(hostile);
        let names: Vec<&str> = inside.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["ordinary.txt"]);
    }

    /// A file is not a folder, and pressing Enter on one does nothing rather than
    /// leaving the panel showing an empty level.
    #[test]
    fn a_file_is_not_something_to_go_into() {
        let mut inside = Inside::new(listing(&[("only.txt", 1, Kind::File)]));
        assert!(!inside.enter());
        assert!(inside.at.is_empty());
    }

    /// The cursor cannot leave the list, in either direction.
    #[test]
    fn the_cursor_stays_in_the_list() {
        let mut inside =
            Inside::new(listing(&[("a.txt", 1, Kind::File), ("b.txt", 1, Kind::File)]));
        inside.step(-5);
        assert_eq!(inside.chosen, 0);
        inside.step(9);
        assert_eq!(inside.chosen, 1);
        let mut empty = Inside::new(listing(&[]));
        empty.step(1);
        assert_eq!(empty.chosen, 0);
        assert!(empty.row().is_none());
    }

    /// Both suffixes come off a `.tar.gz`, and something unrecognisable still
    /// gets a name that is not the archive's own.
    #[test]
    fn a_folder_is_named_after_the_archive_without_its_suffixes() {
        for (archive, folder) in [
            ("/tmp/notes.tar.gz", "notes"),
            ("/tmp/notes.tgz", "notes"),
            ("/tmp/photos.zip", "photos"),
            ("/tmp/one.txt.gz", "one.txt"),
            ("/tmp/backup.tar", "backup"),
            ("/tmp/nameless", "nameless unpacked"),
            ("/tmp/.zip", ".zip unpacked"),
        ] {
            assert_eq!(folder_name(Path::new(archive)), folder, "{archive}");
        }
    }
}
