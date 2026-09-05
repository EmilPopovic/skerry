// SPDX-License-Identifier: Apache-2.0
//! How each folder was last laid out, remembered across sessions.
//!
//! **A choice about a folder, not about a window.** The layout in
//! [`crate::session`] is where the tabs and splits were; this is what a folder
//! *is* — a Downloads full of documents wants a details list grouped by time and
//! a Pictures wants a grid, and they want it every time they are opened, in any
//! tab of any window. So it is its own file beside the session rather than a
//! column in it.
//!
//! # Nearest ancestor wins
//!
//! A folder with no record of its own takes its parent's, or its parent's
//! parent's. That is [08-roadmap.md](../../../docs/08-roadmap.md)'s "per-folder
//! view settings with subtree inheritance", and it is what makes the feature
//! usable rather than tedious: setting a grid on `~/Pictures` means the albums
//! under it are grids too, without visiting each one.
//!
//! # A colour is not inherited
//!
//! The same file also holds the colour somebody gave a folder, in a list of its
//! own — and that one does **not** inherit. Colouring `~/Projects` red so it
//! stands out would turn every folder inside it red, and a colour every folder
//! has is a colour that means nothing. See [`crate::colours`].
//!
//! # Only what was asked for
//!
//! A record is written when somebody changes a view and **removed when they set
//! it back to the default**, so the file holds decisions rather than a log of
//! every folder ever opened. That is also what keeps it small enough to read at
//! start-up without a format that needs an index.

use std::path::{Path, PathBuf};

use crate::cells::Mode;
use crate::groups::Grouping;

/// The format this build writes and the only one it reads.
const VERSION: u32 = 1;

/// How many folders are remembered.
///
/// **A cap, because this file is read before the first frame.** Two hundred and
/// fifty-six decisions is more than anybody makes and about 12 KB; without a cap
/// it grows for the life of an install and start-up pays for all of it. The
/// least recently chosen is dropped, which is the one most likely to be a folder
/// that no longer exists.
const MOST: usize = 256;

/// How a folder's *contents* are laid out: a row each, or a cell each.
///
/// **Not [`Mode`], which has a third variant.** A column view shows a *path* — N
/// directories at once — so "this folder is a column view" is a statement about
/// how somebody was moving around rather than about what is in the folder. With
/// the subtree inheritance in [`Views::for_path`] it is worse than meaningless:
/// setting a pane to columns anywhere recorded it against that folder, every
/// directory beneath it then inherited "show a path", and switching one
/// destination back to a details list and stepping out put the whole subtree into
/// columns.
///
/// The field below said "Details or grid" in a comment before `Mode` grew its
/// third variant. This is that sentence with a type instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Layout {
    /// One entry per row.
    #[default]
    Details,
    /// A grid of cells.
    Grid,
}

impl Layout {
    /// The token this is written as in the file.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Details => "details",
            Self::Grid => "grid",
        }
    }

    /// The layout a token names, if it names one this build records.
    ///
    /// A file written by a build that recorded `columns` reads as no record at
    /// all, which is the same thing a file written before this feature existed
    /// reads as — and both are folders with no opinion, which take the default.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "details" => Some(Self::Details),
            "grid" => Some(Self::Grid),
            _ => None,
        }
    }

    /// The pane mode this lays a folder out as.
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            Self::Details => Mode::Details,
            Self::Grid => Mode::Grid,
        }
    }

    /// The layout a pane mode records as, or nothing where it records as none.
    #[must_use]
    pub const fn of(mode: Mode) -> Option<Self> {
        match mode {
            Mode::Details => Some(Self::Details),
            Mode::Grid => Some(Self::Grid),
            // A pane in columns is showing a path. There is nothing here to say
            // about the folder it happens to be standing in.
            Mode::Columns => None,
        }
    }
}

/// One folder's view.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct View {
    /// Details or grid, and it cannot be anything else.
    pub layout: Layout,
    /// Whether it is cut into headings, and by what.
    pub grouping: Grouping,
}

/// What every folder was last set to, most recently chosen first.
#[derive(Default)]
pub struct Views {
    /// Ordered, because the order *is* the recency and the cap drops the tail.
    ///
    /// A `Vec` rather than a map: two hundred and fifty-six entries walked
    /// linearly is nothing beside the `openat` that reads them.
    held: Vec<(PathBuf, View)>,
    /// The folders somebody gave a colour, most recent first.
    ///
    /// **Its own list, with its own cap.** A colour is not a view: it does not
    /// inherit, it is far cheaper to give out — a person can colour thirty
    /// folders in a minute — and it must not compete with view records for the
    /// same two hundred and fifty-six slots, or colouring a directory would
    /// silently forget that Pictures is a grid.
    colours: Vec<(PathBuf, crate::colours::Colour)>,
}

impl Views {
    /// The view for `path`: its own, or the nearest ancestor's, or the default.
    ///
    /// **The default, not "leave it alone".** An earlier version answered `None`
    /// for a folder nobody had chosen for, so the pane kept whatever the folder
    /// before it had — set Downloads grouped, walk into Pictures, and Pictures
    /// was grouped too; un-group it, come back, and it was grouped again, because
    /// nothing had ever been recorded for it either way. A folder with no opinion
    /// gets the default, which is the only answer that does not leak.
    #[must_use]
    pub fn for_path(&self, path: &Path) -> View {
        let mut at = Some(path);
        while let Some(here) = at {
            if let Some((_, view)) = self.held.iter().find(|(kept, _)| kept == here) {
                return *view;
            }
            at = here.parent();
        }
        View::default()
    }

    /// The record `path` is left with when a pane in `mode` chooses `grouping`.
    ///
    /// **A pane in columns chooses a grouping and no layout.** It is showing a
    /// path rather than a folder, so it has nothing to say about how this one is
    /// laid out — and [`Layout`] has no way to write it down, which is the whole
    /// reason that type exists. The layout half stays at whatever the folder
    /// already answers, its own or an ancestor's, so the choice that *was* made
    /// is the only thing that moves.
    ///
    /// Not [`Layout::default`] for the columns case. It reads the same on a
    /// folder nobody has chosen for and is wrong on every album under a
    /// `~/Pictures` set to a grid: grouping one of them from a trail would write
    /// `details` over the grid it inherits and un-grid it.
    #[must_use]
    pub fn chosen(&self, path: &Path, mode: Mode, grouping: crate::groups::Grouping) -> View {
        let layout = Layout::of(mode).unwrap_or_else(|| self.for_path(path).layout);
        View { layout, grouping }
    }

    /// What `path` would get with no record of its own.
    fn inherited(&self, path: &Path) -> View {
        path.parent().map_or_else(View::default, |parent| self.for_path(parent))
    }

    /// Remembers `view` for `path`. Whether anything changed.
    ///
    /// **A record is kept unless it says what would be inherited anyway.** Not
    /// "unless it is the default": with a grid remembered on `~/Pictures`, a
    /// person who sets one album back to a plain list is saying something that
    /// differs from what it would inherit, and dropping that record as "just the
    /// default" would hand the album back the grid. The redundant case is the one
    /// worth dropping, and it keeps the file a list of decisions.
    pub fn remember(&mut self, path: &Path, view: View) -> bool {
        let was = self.held.iter().position(|(kept, _)| kept == path);
        if view == self.inherited(path) {
            // Only a change if there was something to forget.
            return was.map(|at| self.held.remove(at)).is_some();
        }
        if let Some(at) = was {
            let changed = self.held[at].1 != view;
            let entry = self.held.remove(at);
            self.held.insert(0, (entry.0, view));
            return changed || at != 0;
        }
        self.held.insert(0, (path.to_path_buf(), view));
        self.held.truncate(MOST);
        true
    }

    /// How many folders are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// The colour `path` was given, if it was given one.
    ///
    /// **Exactly `path`.** No ancestor walk, which is the one way this differs
    /// from every other record in this file — see the module header.
    #[must_use]
    pub fn colour_of(&self, path: &Path) -> Option<crate::colours::Colour> {
        self.colours.iter().find(|(kept, _)| kept == path).map(|(_, colour)| *colour)
    }

    /// The coloured children of one directory, as names.
    ///
    /// **One pass over the list per directory, not one per row.** The listing's
    /// rows all share a parent, so the question "which of these has a colour" is
    /// asked once and answered against names — which costs no path joining and
    /// no allocation per row.
    pub fn coloured_in<'a>(
        &'a self,
        parent: &'a Path,
    ) -> impl Iterator<Item = (&'a std::ffi::OsStr, crate::colours::Colour)> + 'a {
        self.colours.iter().filter_map(move |(path, colour)| {
            if path.parent() != Some(parent) {
                return None;
            }
            Some((path.file_name()?, *colour))
        })
    }

    /// Gives `path` a colour, or takes its colour away. Whether anything changed.
    pub fn colour(&mut self, path: &Path, colour: Option<crate::colours::Colour>) -> bool {
        let was = self.colours.iter().position(|(kept, _)| kept == path);
        let Some(colour) = colour else {
            return was.map(|at| self.colours.remove(at)).is_some();
        };
        if let Some(at) = was {
            let changed = self.colours[at].1 != colour;
            let entry = self.colours.remove(at);
            self.colours.insert(0, (entry.0, colour));
            return changed || at != 0;
        }
        self.colours.insert(0, (path.to_path_buf(), colour));
        self.colours.truncate(MOST);
        true
    }

    /// How many folders have a colour. For the tests; the program asks the
    /// records themselves.
    #[cfg(test)]
    fn coloured_for_tests(&self) -> usize {
        self.colours.len()
    }

    /// Whether nothing is remembered. For the tests; the program asks `len`.
    #[cfg(test)]
    fn is_empty_for_tests(&self) -> bool {
        self.held.is_empty()
    }

    /// The file's text.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("skerry {VERSION}\n");
        for (path, view) in &self.held {
            // The path last, so one with a space in it needs no quoting — the
            // same shape [`crate::session`]'s tab line has, for the same reason.
            let _ = writeln!(
                out,
                "view {} {} {}",
                view.layout.token(),
                view.grouping.token(),
                path.display()
            );
        }
        for (path, colour) in &self.colours {
            let _ = writeln!(out, "colour {} {}", colour.token(), path.display());
        }
        out
    }

    /// Reads what `text` holds, or nothing if this build does not understand it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let (name, version) = lines.next()?.split_once(' ')?;
        if name != "skerry" || version.parse::<u32>().ok()? != VERSION {
            return None;
        }
        let mut held = Vec::new();
        let mut colours = Vec::new();
        for line in lines {
            if let Some(rest) = line.strip_prefix("colour ") {
                let Some((token, path)) = rest.split_once(' ') else { continue };
                let path = Path::new(path);
                let Some(colour) = crate::colours::Colour::from_token(token) else { continue };
                if path.is_absolute() && colours.len() < MOST {
                    colours.push((path.to_path_buf(), colour));
                }
                continue;
            }
            let Some(rest) = line.strip_prefix("view ") else { continue };
            let Some((layout, rest)) = rest.split_once(' ') else { continue };
            let Some((grouping, path)) = rest.split_once(' ') else { continue };
            // A line naming a layout this build does not record — `columns`, which
            // an earlier one wrote — is skipped, the same as one from a build that
            // had no such field. Both are folders with no opinion.
            let (Some(layout), Some(grouping)) =
                (Layout::from_token(layout), Grouping::from_token(grouping))
            else {
                continue;
            };
            let path = Path::new(path);
            // Relative paths are never written and cannot be matched against a
            // pane's, which is always absolute.
            if !path.is_absolute() {
                continue;
            }
            held.push((path.to_path_buf(), View { layout, grouping }));
            if held.len() >= MOST {
                break;
            }
        }
        Some(Self { held, colours })
    }
}

/// Where the file lives.
///
/// `$XDG_STATE_HOME/skerry/views`, beside the session. **State and not cache**:
/// losing it loses a decision somebody made, which is the basedir
/// specification's own line between the two.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("skerry").join("views"))
}

/// Reads the file, or an empty set where there is not one.
///
/// **Runs before the frame loop starts**, which is the one place a filesystem
/// call is not a rule 1 violation: there is no frame to block yet, and the
/// alternative is opening every folder in the wrong layout and correcting it a
/// moment later. One `openat` and one read of a file at most about 12 KB.
#[must_use]
pub fn load() -> Views {
    path()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|text| Views::parse(&text))
        .unwrap_or_default()
}

/// Writes `text` where [`path`] says, whole and atomically.
///
/// The same [`sk_fs::atomically`] the session file and the pinned places use. It
/// was a second copy of those steps before, with the `fsync` missing — which is
/// what a duplicated four-step write costs.
pub fn write(text: &str) -> bool {
    path().is_some_and(|file| sk_fs::atomically(&file, text))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Layout, MOST, View, Views};
    use crate::groups::Grouping;
    use std::path::{Path, PathBuf};

    fn grid() -> View {
        View { layout: Layout::Grid, grouping: Grouping::None }
    }

    fn grouped() -> View {
        View { layout: Layout::Details, grouping: Grouping::Time }
    }

    /// A folder with no record of its own takes the nearest ancestor's.
    #[test]
    fn a_subfolder_inherits_from_the_nearest_folder_that_has_a_view() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a/Pictures"), grid());
        views.remember(Path::new("/home/a/Pictures/2001/raw"), grouped());

        assert_eq!(views.for_path(Path::new("/home/a/Pictures")), grid());
        // Inherited from `Pictures`, two levels up.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001")), grid());
        // Its own record wins over the ancestor's.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001/raw")), grouped());
        // And deeper than the record inherits the record.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001/raw/x")), grouped());
        // A folder under neither is the default.
        assert_eq!(views.for_path(Path::new("/home/a/Documents")), View::default());
    }

    /// A record is dropped when it is redundant, and kept when it says something.
    ///
    /// **Not "dropped when it is the default".** With a grid on `~/Pictures`, an
    /// album set back to a plain list is saying something that differs from what
    /// it would inherit; dropping that as "just the default" hands the album the
    /// grid straight back.
    #[test]
    fn a_record_is_dropped_only_when_it_says_what_would_be_inherited() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a"), grid());

        // Redundant: `b` would inherit the grid anyway.
        assert!(!views.remember(Path::new("/home/a/b"), grid()), "a redundant record was kept");
        assert_eq!(views.len(), 1);

        // Not redundant, even though it is the global default: `b` would have
        // inherited a grid and this says otherwise.
        assert!(views.remember(Path::new("/home/a/b"), View::default()));
        assert_eq!(views.len(), 2, "an album set back to a list was forgotten");
        assert_eq!(views.for_path(Path::new("/home/a/b")), View::default());
        assert_eq!(views.for_path(Path::new("/home/a")), grid(), "the parent moved");

        // And where nothing is inherited, the default *is* redundant.
        let mut plain = Views::default();
        assert!(!plain.remember(Path::new("/home/z"), View::default()));
        assert!(plain.is_empty_for_tests());
    }

    /// A folder nobody has chosen for does not keep the last folder's view.
    ///
    /// **The reported defect, as a sequence.** Set Downloads grouped, walk into
    /// Pictures and Pictures was grouped; un-group Pictures, go to Downloads and
    /// back, and it was grouped again — because "no record" answered *leave it
    /// alone*, and what it was left alone with was the previous folder's layout.
    #[test]
    fn a_view_set_on_one_folder_does_not_follow_you_into_another() {
        let mut views = Views::default();
        let downloads = Path::new("/home/a/Downloads");
        let pictures = Path::new("/home/a/Pictures");

        views.remember(downloads, grouped());
        assert_eq!(views.for_path(downloads), grouped());
        // Walking into Pictures gets Pictures' answer, which is the default.
        assert_eq!(views.for_path(pictures), View::default(), "Downloads followed us");

        // Un-grouping Pictures is already its answer, so nothing is recorded.
        assert!(!views.remember(pictures, View::default()));
        // Downloads, then back: still the default, still not grouped.
        assert_eq!(views.for_path(downloads), grouped());
        assert_eq!(views.for_path(pictures), View::default(), "it came back grouped");
    }

    /// The most recently chosen folder is first, and the cap drops the oldest.
    #[test]
    fn the_cap_drops_the_least_recently_chosen() {
        let mut views = Views::default();
        for at in 0..(MOST + 20) {
            views.remember(&PathBuf::from(format!("/home/a/{at}")), grid());
        }
        assert_eq!(views.len(), MOST, "the cap did not hold");
        // The newest is kept and the oldest is gone.
        assert_eq!(views.for_path(Path::new("/home/a/275")), grid());
        assert_eq!(views.for_path(Path::new("/home/a/0")), View::default(), "the oldest was kept");
    }

    /// Choosing again moves a folder back to the front.
    #[test]
    fn choosing_a_folder_again_makes_it_the_most_recent() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a"), grid());
        views.remember(Path::new("/home/b"), grid());
        // `a` is second now; choosing it again puts it first, which is what the
        // cap reads as recency.
        assert!(views.remember(Path::new("/home/a"), grid()));
        assert_eq!(views.render().lines().nth(1).unwrap_or_default(), "view grid flat /home/a");
        // The same choice in the same place a second time changes nothing.
        assert!(!views.remember(Path::new("/home/a"), grid()));
    }

    /// What is written is what is read, paths with spaces included.
    #[test]
    fn the_file_round_trips_and_a_path_may_hold_spaces() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a/my pictures"), grid());
        views.remember(Path::new("/home/a/Downloads"), grouped());
        let text = views.render();
        let back = Views::parse(&text).expect("what we wrote we cannot read");
        assert_eq!(back.len(), 2);
        assert_eq!(back.for_path(Path::new("/home/a/my pictures")), grid());
        assert_eq!(back.for_path(Path::new("/home/a/Downloads")), grouped());
    }

    /// A file this build does not understand is no views, not a crash.
    #[test]
    fn a_file_from_another_build_is_refused_and_a_bad_line_is_skipped() {
        assert!(Views::parse("skerry 99\nview grid flat /home/a\n").is_none());
        assert!(Views::parse("").is_none());
        assert!(Views::parse("not-skerry 1\n").is_none());

        // A line naming a mode that does not exist, a relative path, and a line
        // that is not a view at all — each skipped, the rest kept.
        let mixed = "skerry 1\n\
             view tree flat /home/a\n\
             view grid flat relative/path\n\
             something else entirely\n\
             view grid by-time /home/a/good\n";
        let views = Views::parse(mixed).expect("the whole file was refused for one bad line");
        assert_eq!(views.len(), 1);
        assert_eq!(
            views.for_path(Path::new("/home/a/good")),
            View { layout: Layout::Grid, grouping: Grouping::Time }
        );
    }

    /// **The scenario that found this.** Set a pane to columns anywhere, walk
    /// somewhere deep, set *that* folder to a details list, and step back out: the
    /// parent inherited the columns record from the ancestor the pane happened to
    /// be standing in when the mode was chosen, and so did every other directory
    /// under it.
    ///
    /// It cannot happen now because the record has no way to say it. This asserts
    /// the shape rather than the type, so it would fail again if a conversion
    /// somewhere started answering `Layout::Details` for a pane in columns.
    #[test]
    fn a_pane_in_columns_leaves_no_record_to_inherit() {
        assert_eq!(Layout::of(crate::cells::Mode::Columns), None);
        assert_eq!(Layout::of(crate::cells::Mode::Details), Some(Layout::Details));
        assert_eq!(Layout::of(crate::cells::Mode::Grid), Some(Layout::Grid));

        // A subtree with one real decision in it: the album is a grid.
        let mut views = Views::default();
        views.remember(Path::new("/home/emil/Pictures"), grid());
        // Nothing recorded for the folders a trail walked through, so a folder
        // deep inside still inherits the one decision anybody made.
        assert_eq!(views.for_path(Path::new("/home/emil/Pictures/2026/June")), grid());
        // And a folder outside it takes the default, not whatever a pane was in.
        assert_eq!(views.for_path(Path::new("/home/emil/Downloads")), View::default());
    }

    /// A file written by a build that recorded `columns` reads as no record for
    /// that folder — the same as a file that never mentioned it. Which is what
    /// makes the fix retroactive rather than something a person has to undo by
    /// hand, folder by folder, through a subtree they never chose anything for.
    #[test]
    fn a_columns_record_from_an_older_build_reads_as_no_record() {
        let text = concat!(
            "skerry 1\n",
            "view columns flat /home/emil\n",
            "view grid flat /home/emil/Pictures\n",
        );
        let views = Views::parse(text).expect("a line it cannot use is not a broken file");
        // **Asserted on what was kept, not on what a folder answers.** A build that
        // read `columns` as `details` would answer `View::default()` here too —
        // the default *is* details — so the question has to be whether there is a
        // record at all. There is one line in the file this build would write, and
        // it is the grid.
        let file = views.render();
        let kept: Vec<&str> = file.lines().filter(|line| line.starts_with("view ")).collect();
        assert_eq!(kept, vec!["view grid flat /home/emil/Pictures"], "a columns line was kept");
        assert_eq!(
            views.for_path(Path::new("/home/emil/a/b/c")),
            View::default(),
            "and a whole subtree inherited it"
        );
        assert_eq!(
            views.for_path(Path::new("/home/emil/Pictures")),
            grid(),
            "the rest still reads"
        );
    }

    /// A pane in columns does not hand a subtree the default layout either.
    ///
    /// The columns case cannot write `Layout::default()`: on an album under a
    /// `~/Pictures` that is a grid it reads identically on the folder itself and
    /// takes the grid away from it.
    #[test]
    fn grouping_from_a_trail_leaves_an_inherited_grid_alone() {
        use crate::cells::Mode;
        use crate::groups::Grouping;

        let mut views = Views::default();
        views.remember(Path::new("/home/a/Pictures"), grid());
        let album = Path::new("/home/a/Pictures/2001");

        let held = views.chosen(album, Mode::Columns, Grouping::Time);
        assert_eq!(held.layout, Layout::Grid, "the album lost the grid it inherits");
        assert_eq!(held.grouping, Grouping::Time);

        views.remember(album, held);
        assert_eq!(views.for_path(album), View { layout: Layout::Grid, grouping: Grouping::Time });
        // And the album with no grouping is redundant again, so nothing is kept.
        assert!(views.remember(album, views.chosen(album, Mode::Columns, Grouping::None)));
        assert_eq!(views.len(), 1, "an album back to what it inherits was recorded");
    }

    /// A colour is one folder's and never its children's.
    ///
    /// The one way this record differs from every other in the file, and the
    /// reason it is a second list: inherited, a red `~/Projects` would make
    /// every folder under it red, and a colour every folder has is a colour that
    /// says nothing.
    #[test]
    fn a_colour_belongs_to_one_folder_and_is_not_inherited() {
        use crate::colours::Colour;
        let mut views = Views::default();
        let projects = Path::new("/home/emil/projects");
        assert!(views.colour(projects, Some(Colour::Red)));
        assert_eq!(views.colour_of(projects), Some(Colour::Red));
        assert_eq!(views.colour_of(Path::new("/home/emil/projects/skerry")), None);
        assert_eq!(views.colour_of(Path::new("/home/emil")), None);

        // And it does not disturb the view records, which have their own list and
        // their own inheritance.
        assert!(views.is_empty_for_tests(), "a colour was written as a view");

        // Setting the same colour again changes nothing; a different one does;
        // and clearing removes the record rather than writing "no colour".
        assert!(!views.colour(projects, Some(Colour::Red)));
        assert!(views.colour(projects, Some(Colour::Blue)));
        assert_eq!(views.coloured_for_tests(), 1);
        assert!(views.colour(projects, None));
        assert_eq!(views.colour_of(projects), None);
        assert_eq!(views.coloured_for_tests(), 0);
        assert!(!views.colour(projects, None), "clearing nothing changed something");
    }

    /// The colours survive the file, beside the views and without disturbing
    /// them — and a colour this build does not have reads as no colour.
    #[test]
    fn colours_round_trip_through_the_file_beside_the_views() {
        use crate::colours::Colour;
        let mut views = Views::default();
        views.remember(
            Path::new("/home/emil/pictures"),
            View { layout: Layout::Grid, grouping: Grouping::None },
        );
        assert!(views.colour(Path::new("/home/emil/projects"), Some(Colour::Red)));
        assert!(views.colour(Path::new("/home/emil/wor k"), Some(Colour::Purple)));

        let text = views.render();
        assert!(text.contains("colour red /home/emil/projects"), "{text}");
        let read = Views::parse(&text).expect("refused its own file");
        assert_eq!(read.colour_of(Path::new("/home/emil/projects")), Some(Colour::Red));
        // A path with a space in it, which is why the path is last on the line.
        assert_eq!(read.colour_of(Path::new("/home/emil/wor k")), Some(Colour::Purple));
        assert_eq!(read.for_path(Path::new("/home/emil/pictures")).layout, Layout::Grid);

        // A newer build's seventh colour, and a line that is neither.
        let odd = text.replace("colour red", "colour magenta")
            + "nonsense
";
        let read = Views::parse(&odd).expect("one unknown line refused the whole file");
        assert_eq!(read.colour_of(Path::new("/home/emil/projects")), None);
        assert_eq!(read.colour_of(Path::new("/home/emil/wor k")), Some(Colour::Purple));
    }

    /// The children of one directory, which is what a frame asks for.
    #[test]
    fn the_coloured_children_of_a_directory_are_found_by_name() {
        use crate::colours::Colour;
        let mut views = Views::default();
        views.colour(Path::new("/home/emil/one"), Some(Colour::Red));
        views.colour(Path::new("/home/emil/two"), Some(Colour::Green));
        views.colour(Path::new("/home/emil/deeper/three"), Some(Colour::Blue));
        let mut found: Vec<(String, Colour)> = views
            .coloured_in(Path::new("/home/emil"))
            .map(|(name, colour)| (name.to_string_lossy().into_owned(), colour))
            .collect();
        found.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(found, [("one".to_owned(), Colour::Red), ("two".to_owned(), Colour::Green)]);
        assert_eq!(views.coloured_in(Path::new("/nowhere")).count(), 0);
    }
}
