//! The list of places down the left, and where each row of it is.
//!
//! Three sections, in the order Windows and every Linux file manager put them:
//! the directories a person keeps things in, the ones they bookmarked, and the
//! filesystems that are mounted. What is in each comes from `sk-desktop` and
//! `sk-fs`; what is here is the arrangement, the geometry and the hit test.
//!
//! **Built on a job.** It reads `user-dirs.dirs`, GTK's `bookmarks` and
//! `/proc/self/mountinfo`, and stats every candidate directory to drop the ones
//! that are not there. None of that is a frame's work.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use sk_desktop::{Kind, Place};
use sk_fs::Mount;
use sk_util::{Cancel, Lane, WakeHandle};

use crate::pane::Jobs;

/// One row of the sidebar.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Where clicking it goes.
    pub path: PathBuf,
    /// What it is called.
    pub label: String,
    /// Which mark to draw and which colour to draw it in.
    pub mark: Mark,
    /// Whether reaching it may block for a network timeout. Drawn, because a
    /// row that takes eight seconds should have said so before it was clicked.
    pub remote: bool,
}

/// What kind of thing a row points at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// One of the well-known user directories, or a bookmark.
    Place(Kind),
    /// A mounted filesystem.
    Volume,
}

/// A heading and the rows under it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Section {
    /// Catalogue key for the heading.
    pub title: &'static str,
    /// The rows.
    pub entries: Vec<Entry>,
}

/// Everything the sidebar shows, and whether it is shown at all.
pub struct Sidebar {
    /// The sections, in display order. Empty until the job that reads them
    /// finishes, which is a few milliseconds after start-up.
    pub sections: Vec<Section>,
    /// Whether the sidebar is open. Remembered across a toggle, so `F9` twice
    /// is where you were.
    pub open: bool,
    /// The row under the pointer, as an index into [`Sidebar::rows`].
    pub hover: Option<usize>,
    loaded: Receiver<Vec<Section>>,
    sender: Sender<Vec<Section>>,
    wake: WakeHandle,
}

impl Sidebar {
    /// An empty sidebar, and the job that fills it.
    #[must_use]
    pub fn new(jobs: &Jobs, wake: WakeHandle, open: bool) -> Self {
        let (sender, loaded) = std::sync::mpsc::channel();
        let sidebar = Self { sections: Vec::new(), open, hover: None, loaded, sender, wake };
        sidebar.reload(jobs);
        sidebar
    }

    /// Reads the three sources again, on a job.
    ///
    /// What `F5` does, and what a mount appearing would do if anything were
    /// watching `/proc/self/mountinfo` — which is owed, and is why this is a
    /// method rather than something that happens once at start-up.
    pub fn reload(&self, jobs: &Jobs) {
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        jobs.pool.spawn(Lane::Parallel, Cancel::new(), move |_| {
            if sender.send(build()).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of a completed read. Returns whether anything arrived.
    pub fn drain(&mut self) -> bool {
        let mut any = false;
        while let Ok(sections) = self.loaded.try_recv() {
            self.sections = sections;
            any = true;
        }
        if any {
            self.hover = None;
        }
        any
    }

    /// Every row, in display order, with its section's heading before it.
    pub fn rows(&self) -> impl Iterator<Item = &Entry> + '_ {
        self.sections.iter().flat_map(|section| section.entries.iter())
    }

    /// How many rows there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sections.iter().map(|section| section.entries.len()).sum()
    }

    /// The row at an index into the flattened list.
    #[must_use]
    pub fn at(&self, index: usize) -> Option<&Entry> {
        self.rows().nth(index)
    }
}

/// Which row of `sections` is the directory shown, by longest match.
///
/// The longest match rather than the first: with `/` and `/home/emil` both on
/// the list, standing in `/home/emil/Downloads` should light Home and not the
/// filesystem. An exact match wins over both.
pub(crate) fn current(sections: &[Section], path: &Path) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, entry) in sections.iter().flat_map(|section| section.entries.iter()).enumerate() {
        if !path.starts_with(&entry.path) {
            continue;
        }
        let depth = entry.path.components().count();
        if best.is_none_or(|(_, found)| depth > found) {
            best = Some((index, depth));
        }
    }
    best.map(|(index, _)| index)
}

/// A fixed set of places, for a golden frame.
///
/// **Not the machine's.** What `build` returns depends on `user-dirs.dirs`,
/// GTK's `bookmarks` and what is mounted — three things that differ between two
/// machines and between two weeks on one machine, and a reference image that
/// embeds them is a reference image nobody else can reproduce. The readers have
/// their own tests; what a golden frame is for is the drawing, and this gives it
/// the same rows every time.
///
/// The paths are `root`'s own subdirectories, so the current-location highlight
/// has something to land on.
#[must_use]
pub(crate) fn sample(root: &Path) -> Vec<Section> {
    let place = |label: &str, path: PathBuf, kind: Kind| Entry {
        path,
        label: label.to_owned(),
        mark: Mark::Place(kind),
        remote: false,
    };
    vec![
        Section {
            title: "sidebar.places",
            entries: vec![
                place(sk_model::i18n::lookup("sidebar.home"), root.to_path_buf(), Kind::Home),
                place("documents", root.join("documents"), Kind::Documents),
                place("build", root.join("build"), Kind::Other),
            ],
        },
        Section {
            title: "sidebar.volumes",
            entries: vec![Entry {
                path: PathBuf::from("/"),
                label: sk_model::i18n::lookup("sidebar.root").to_owned(),
                mark: Mark::Volume,
                remote: false,
            }],
        },
    ]
}

/// Reads the three sources and arranges them.
pub(crate) fn build() -> Vec<Section> {
    let places = sk_desktop::user_dirs();
    // A bookmark that is already one of the user directories is the same row
    // twice: this machine's `bookmarks` file lists Documents, Music, Pictures,
    // Videos and Downloads, all of which are above it already.
    let known: Vec<&PathBuf> = places.iter().map(|place| &place.path).collect();
    let marks: Vec<Place> =
        sk_desktop::bookmarks().into_iter().filter(|place| !known.contains(&&place.path)).collect();
    let mounts = sk_fs::mounts();

    let mut sections = Vec::with_capacity(3);
    sections.push(Section { title: "sidebar.places", entries: places.iter().map(row).collect() });
    if !marks.is_empty() {
        sections
            .push(Section { title: "sidebar.bookmarks", entries: marks.iter().map(row).collect() });
    }
    if !mounts.is_empty() {
        sections.push(Section {
            title: "sidebar.volumes",
            entries: mounts.iter().map(volume).collect(),
        });
    }
    sections
}

fn row(place: &Place) -> Entry {
    let label = if place.kind == Kind::Home {
        sk_model::i18n::lookup("sidebar.home").to_owned()
    } else {
        place.label.clone()
    };
    Entry { path: place.path.clone(), label, mark: Mark::Place(place.kind), remote: false }
}

/// A mount's row. The root filesystem is named rather than shown as `/`, and a
/// mount point with no useful name falls back to what backs it.
fn volume(mount: &Mount) -> Entry {
    let label = if mount.is_root() {
        sk_model::i18n::lookup("sidebar.root").to_owned()
    } else {
        mount
            .at
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| mount.source.clone())
    };
    Entry { path: mount.at.clone(), label, mark: Mark::Volume, remote: mount.remote }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Entry, Mark, Section, build};
    use sk_desktop::Kind;
    use std::path::{Path, PathBuf};

    fn fake(paths: &[&str]) -> Vec<Section> {
        vec![Section {
            title: "sidebar.places",
            entries: paths
                .iter()
                .map(|path| Entry {
                    path: PathBuf::from(path),
                    label: (*path).to_owned(),
                    mark: Mark::Place(Kind::Other),
                    remote: false,
                })
                .collect(),
        }]
    }

    fn at(sections: &[Section], path: &str) -> Option<usize> {
        super::current(sections, Path::new(path))
    }

    /// The longest match, not the first: `/` and `/home/emil` are both on the
    /// list, and standing in `/home/emil/Downloads` should light Home.
    #[test]
    fn the_deepest_matching_place_is_the_current_one() {
        let sections = fake(&["/", "/home/emil", "/home/emil/Downloads"]);
        assert_eq!(at(&sections, "/home/emil/Downloads/thing"), Some(2));
        assert_eq!(at(&sections, "/home/emil/Documents"), Some(1));
        assert_eq!(at(&sections, "/etc"), Some(0));
        assert_eq!(at(&fake(&["/home/emil"]), "/etc"), None);
    }

    /// `starts_with` is by component, so a sibling whose name merely begins the
    /// same way is not a match.
    #[test]
    fn a_sibling_with_a_longer_name_is_not_inside_a_place() {
        assert_eq!(at(&fake(&["/home/emil"]), "/home/emilia"), None);
    }

    /// This machine's own three sources: whatever they hold, every row points at
    /// a directory that exists and nothing appears twice.
    #[test]
    fn what_this_machine_offers_is_a_list_of_places_that_exist() {
        let sections = build();
        assert!(!sections.is_empty(), "no sections at all");
        let rows: Vec<&Entry> =
            sections.iter().flat_map(|section| section.entries.iter()).collect();
        for entry in &rows {
            assert!(entry.path.is_dir(), "{entry:?}");
            assert!(!entry.label.is_empty(), "{entry:?}");
        }
        let mut paths: Vec<_> = rows.iter().map(|entry| &entry.path).collect();
        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "the same place twice: {rows:?}");
    }
}
