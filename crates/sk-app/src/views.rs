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

/// One folder's view.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct View {
    /// Details or grid.
    pub mode: Mode,
    /// Whether it is cut into headings, and by what.
    pub grouping: Grouping,
}

impl View {
    /// Whether this is what a folder nobody has chosen for looks like.
    fn is_default(self) -> bool {
        self == Self::default()
    }
}

/// What every folder was last set to, most recently chosen first.
#[derive(Default)]
pub struct Views {
    /// Ordered, because the order *is* the recency and the cap drops the tail.
    ///
    /// A `Vec` rather than a map: two hundred and fifty-six entries walked
    /// linearly is nothing beside the `openat` that reads them.
    held: Vec<(PathBuf, View)>,
}

impl Views {
    /// The view for `path`: its own, or the nearest ancestor's, or nothing.
    ///
    /// **`None` rather than the default**, and the difference matters. A pane
    /// restored from a session already has a layout; a folder nobody has chosen
    /// for has no opinion about it, and answering with the default would throw
    /// away the restored one. Nothing means *leave it alone*.
    #[must_use]
    pub fn for_path(&self, path: &Path) -> Option<View> {
        let mut at = Some(path);
        while let Some(here) = at {
            if let Some((_, view)) = self.held.iter().find(|(kept, _)| kept == here) {
                return Some(*view);
            }
            at = here.parent();
        }
        None
    }

    /// Remembers `view` for `path`. Whether anything changed.
    ///
    /// Setting a folder back to the default **forgets** it rather than recording
    /// the default, so a person can undo a choice and the file stays a list of
    /// decisions. The folder moves to the front either way, because that is the
    /// recency the cap reads.
    pub fn remember(&mut self, path: &Path, view: View) -> bool {
        let was = self.held.iter().position(|(kept, _)| kept == path);
        if view.is_default() {
            // Only a change if there was something to forget. An ancestor may
            // still apply, which is what makes "set it back" mean "inherit
            // again" rather than "pin the default here".
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
                view.mode.token(),
                view.grouping.token(),
                path.display()
            );
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
        for line in lines {
            let Some(rest) = line.strip_prefix("view ") else { continue };
            let Some((mode, rest)) = rest.split_once(' ') else { continue };
            let Some((grouping, path)) = rest.split_once(' ') else { continue };
            let (Some(mode), Some(grouping)) =
                (Mode::from_token(mode), Grouping::from_token(grouping))
            else {
                continue;
            };
            let path = Path::new(path);
            // Relative paths are never written and cannot be matched against a
            // pane's, which is always absolute.
            if !path.is_absolute() {
                continue;
            }
            held.push((path.to_path_buf(), View { mode, grouping }));
            if held.len() >= MOST {
                break;
            }
        }
        Some(Self { held })
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
/// The same three steps [`crate::session::write`] takes, and for the same reason:
/// a reader sees the whole old file or the whole new one, never a torn one.
pub fn write(text: &str) -> bool {
    let Some(file) = path() else { return false };
    let Some(directory) = file.parent() else { return false };
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    let staging = file.with_extension("new");
    if std::fs::write(&staging, text).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    if std::fs::rename(&staging, &file).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{MOST, View, Views};
    use crate::cells::Mode;
    use crate::groups::Grouping;
    use std::path::{Path, PathBuf};

    fn grid() -> View {
        View { mode: Mode::Grid, grouping: Grouping::None }
    }

    fn grouped() -> View {
        View { mode: Mode::Details, grouping: Grouping::Time }
    }

    /// A folder with no record of its own takes the nearest ancestor's.
    #[test]
    fn a_subfolder_inherits_from_the_nearest_folder_that_has_a_view() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a/Pictures"), grid());
        views.remember(Path::new("/home/a/Pictures/2001/raw"), grouped());

        assert_eq!(views.for_path(Path::new("/home/a/Pictures")), Some(grid()));
        // Inherited from `Pictures`, two levels up.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001")), Some(grid()));
        // Its own record wins over the ancestor's.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001/raw")), Some(grouped()));
        // And deeper than the record inherits the record.
        assert_eq!(views.for_path(Path::new("/home/a/Pictures/2001/raw/x")), Some(grouped()));
        // A folder under neither is the default.
        assert_eq!(views.for_path(Path::new("/home/a/Documents")), None, "an opinion nobody gave");
    }

    /// Setting a folder back to the default forgets it, so an ancestor applies.
    #[test]
    fn setting_the_default_forgets_the_folder_rather_than_pinning_it() {
        let mut views = Views::default();
        views.remember(Path::new("/home/a"), grid());
        views.remember(Path::new("/home/a/b"), grouped());
        assert_eq!(views.len(), 2);

        assert!(views.remember(Path::new("/home/a/b"), View::default()), "nothing was forgotten");
        assert_eq!(views.len(), 1, "the default was recorded rather than removed");
        // And `b` now inherits from `a` again rather than being pinned flat.
        assert_eq!(views.for_path(Path::new("/home/a/b")), Some(grid()));
        // Forgetting what was never remembered changes nothing.
        assert!(!views.remember(Path::new("/home/a/c"), View::default()));
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
        assert_eq!(views.for_path(Path::new("/home/a/275")), Some(grid()));
        assert_eq!(views.for_path(Path::new("/home/a/0")), None, "the oldest was kept");
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
        assert_eq!(back.for_path(Path::new("/home/a/my pictures")), Some(grid()));
        assert_eq!(back.for_path(Path::new("/home/a/Downloads")), Some(grouped()));
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
            Some(View { mode: Mode::Grid, grouping: Grouping::Time })
        );
    }
}
