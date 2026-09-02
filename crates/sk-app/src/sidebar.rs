//! The list of places down the left, and where each row of it is.
//!
//! Three sections, in the order Windows and every Linux file manager put them:
//! the directories a person keeps things in, the ones they pinned, and the
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
    /// Whether this row is a line in the bookmarks file — a place somebody
    /// chose, rather than one the machine has.
    ///
    /// **A field rather than a `Kind`**, because it decides what can be done to
    /// the row and not what it looks like: only a pinned place can be unpinned or
    /// dragged into a different order, and inferring that from `Kind::Other`
    /// would make the rule "whatever `user_dirs` happens not to return" — which
    /// is true today and is not a sentence anybody could rely on
    /// ([ADR 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md)).
    pub pinned: bool,
}

/// What a write to the list of places was doing.
///
/// Carried back from the job so the line can say what happened rather than
/// "done": three verbs, and a failure that names the one that failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edit {
    /// A folder was added to the list.
    Pinned,
    /// One was taken off it.
    Unpinned,
    /// The list was put in a different order.
    Reordered,
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

/// How wide the sidebar is until somebody drags it.
///
/// 220 is what Windows 11 and Nautilus both settle near, and it is wide enough
/// for `VirtualBox VMs` at the body text size without truncation.
///
/// **Here rather than in `Metrics`**, where it was: it is a number a person sets
/// by dragging and it goes in the session file, and state does not live in the
/// theme. What the theme still owns is a sidebar row's height and its heading's
/// size, which are drawing.
pub const DEFAULT_WIDTH: f32 = 220.0;

/// The narrowest and widest the sidebar can be dragged.
///
/// The floor is where "Documents" stops fitting beside its mark, which is what a
/// person dragging the divider to zero is asking for and not what they want —
/// closing it is `F9` and is a different gesture. The ceiling is where the list
/// starts losing more than it gains.
pub const NARROWEST: f32 = 150.0;
/// The widest the sidebar can be dragged. See [`NARROWEST`].
pub const WIDEST: f32 = 480.0;

/// Everything the sidebar shows, and whether it is shown at all.
pub struct Sidebar {
    /// The sections, in display order. Empty until the job that reads them
    /// finishes, which is a few milliseconds after start-up.
    pub sections: Vec<Section>,
    /// Whether the sidebar is open. Remembered across a toggle, so `F9` twice
    /// is where you were.
    pub open: bool,
    /// How wide it has been dragged, in logical pixels.
    ///
    /// The window's rather than the folder's, and in the session file beside the
    /// column widths for the same reason: a width is a decision about how much of
    /// a name somebody wants to read.
    pub width: f32,
    /// The row under the pointer, as an index into [`Sidebar::rows`].
    pub hover: Option<usize>,
    /// How many times the contents have arrived.
    ///
    /// A diagnostic, and the only observable effect of a mount the sidebar has
    /// no row for: plugging in a disk changes the sections, and mounting a
    /// `tmpfs` — the only mount a test can make without being root — changes
    /// nothing but this.
    pub loads: u64,
    loaded: Receiver<Vec<Section>>,
    sender: Sender<Vec<Section>>,
    wake: WakeHandle,
}

impl Sidebar {
    /// An empty sidebar, and the job that fills it.
    #[must_use]
    pub fn new(jobs: &Jobs, wake: WakeHandle, open: bool, width: f32) -> Self {
        let (sender, loaded) = std::sync::mpsc::channel();
        let sidebar = Self {
            sections: Vec::new(),
            open,
            width: width.clamp(NARROWEST, WIDEST),
            hover: None,
            loads: 0,
            loaded,
            sender,
            wake,
        };
        sidebar.reload(jobs);
        sidebar
    }

    /// Reads the three sources again, on a job.
    ///
    /// What `F5` does, and what a mount appearing does — the watcher polls
    /// `/proc/self/mountinfo`, which is why this is a method rather than
    /// something that happens once at start-up.
    pub fn reload(&self, jobs: &Jobs) {
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        // **Not cancellable, and the token says so.** Three small file reads
        // that finish in under a millisecond: a cancel would arrive after the
        // work, and the reply is dropped anyway when the receiver is gone.
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
            self.loads += 1;
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

    /// The pinned places, in the order they are drawn.
    ///
    /// What a reorder is a permutation of, so it is the order on screen and not
    /// the order in the file — the two are the same until a bookmark's directory
    /// goes missing, and `sk_desktop::reorder` refuses a request it cannot map
    /// onto the file rather than guessing.
    #[must_use]
    pub fn pinned(&self) -> Vec<PathBuf> {
        self.rows().filter(|entry| entry.pinned).map(|entry| entry.path.clone()).collect()
    }

    /// Whether `path` is one of the pinned places.
    #[must_use]
    pub fn is_pinned(&self, path: &Path) -> bool {
        self.rows().any(|entry| entry.pinned && entry.path == path)
    }

    /// Whether `path` is already a row of any section.
    ///
    /// What makes a folder unpinnable: a well-known directory is on the list
    /// from `user-dirs.dirs`, a mount from `mountinfo`, and `build` drops a
    /// bookmark that duplicates a well-known directory — so pinning one would
    /// write a line that changes nothing on screen, which is worse than a dimmed
    /// row saying it cannot be done.
    #[must_use]
    pub fn holds(&self, path: &Path) -> bool {
        self.rows().any(|entry| entry.path == path)
    }

    /// The width the divider would set from a pointer at `x`, clamped.
    #[must_use]
    pub fn dragged_to(x: f32) -> f32 {
        x.clamp(NARROWEST, WIDEST)
    }
}

/// `order` with the row at `from` moved into the gap at `at`.
///
/// **A gap, not a row**, because "after the last one" has to be expressible:
/// `at` runs from 0 above the first row to `order.len()` below the last. Taking
/// the row out before putting it back is what makes the two indices comparable —
/// dropping a row into the gap just below itself names the row it came from, and
/// is not a move.
///
/// Nothing where the order would not change, so a drag that ended where it began
/// writes no file.
#[must_use]
pub fn moved(order: &[PathBuf], from: usize, at: usize) -> Option<Vec<PathBuf>> {
    if from >= order.len() || at > order.len() {
        return None;
    }
    let mut out = order.to_vec();
    let row = out.remove(from);
    out.insert(if at > from { at - 1 } else { at }, row);
    (out != order).then_some(out)
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
pub(crate) fn sample(root: &Path, pinned: bool) -> Vec<Section> {
    let place = |label: &str, path: PathBuf, kind: Kind| Entry {
        path,
        label: label.to_owned(),
        mark: Mark::Place(kind),
        remote: false,
        pinned: false,
    };
    let mut sections = vec![Section {
        title: "sidebar.places",
        entries: vec![
            place(sk_model::i18n::lookup("sidebar.home"), root.to_path_buf(), Kind::Home),
            place("documents", root.join("documents"), Kind::Documents),
            place("build", root.join("build"), Kind::Other),
        ],
    }];
    // **Off by default, so that adding it did not re-bless fifty-five
    // references.** The sidebar is in every frame, so a row added to this list is
    // a diff in every one of them — and a sweep of fifty-five blessings is a
    // sweep nobody reviews. One frame asks for it.
    if pinned {
        sections.push(Section {
            title: "sidebar.pinned",
            entries: vec![
                Entry { pinned: true, ..place("photos", root.join("photos"), Kind::Other) },
                Entry { pinned: true, ..place("work", root.join("work"), Kind::Other) },
            ],
        });
    }
    sections.push(Section {
        title: "sidebar.volumes",
        entries: vec![Entry {
            path: PathBuf::from("/"),
            label: sk_model::i18n::lookup("sidebar.root").to_owned(),
            mark: Mark::Volume,
            remote: false,
            pinned: false,
        }],
    });
    sections
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
        sections.push(Section {
            title: "sidebar.pinned",
            entries: marks.iter().map(|place| Entry { pinned: true, ..row(place) }).collect(),
        });
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
    Entry {
        path: place.path.clone(),
        label,
        mark: Mark::Place(place.kind),
        remote: false,
        pinned: false,
    }
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
    Entry { path: mount.at.clone(), label, mark: Mark::Volume, remote: mount.remote, pinned: false }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Entry, Mark, Section, Sidebar, build};
    use sk_desktop::Kind;
    use std::path::{Path, PathBuf};

    /// A sidebar holding `sections`, with no job behind it.
    ///
    /// The channel and the wake handle are what `Sidebar::new` needs a pool for,
    /// and neither is read by anything under test here — what is under test is
    /// the arrangement.
    fn laid_out(sections: Vec<Section>) -> Sidebar {
        let waker = sk_util::Waker::new().expect("eventfd");
        let (sender, loaded) = std::sync::mpsc::channel();
        Sidebar {
            sections,
            open: true,
            width: super::DEFAULT_WIDTH,
            hover: None,
            loads: 0,
            loaded,
            sender,
            wake: waker.handle(),
        }
    }

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
                    pinned: false,
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

    /// The gap arithmetic, which is where an off-by-one would put a row one
    /// place from where a hand let it go.
    #[test]
    fn a_row_moves_into_the_gap_it_was_dropped_on() {
        let order: Vec<PathBuf> = ["a", "b", "c"].iter().map(PathBuf::from).collect();
        let names = |held: Option<Vec<PathBuf>>| {
            held.map(|it| {
                it.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>()
            })
        };
        // Above the first, and the gap immediately below itself: neither moves
        // anything, and neither writes a file.
        assert_eq!(super::moved(&order, 0, 0), None);
        assert_eq!(super::moved(&order, 0, 1), None);
        assert_eq!(
            names(super::moved(&order, 0, 2)),
            Some(vec!["b".into(), "a".into(), "c".into()])
        );
        assert_eq!(
            names(super::moved(&order, 0, 3)),
            Some(vec!["b".into(), "c".into(), "a".into()])
        );
        assert_eq!(
            names(super::moved(&order, 2, 0)),
            Some(vec!["c".into(), "a".into(), "b".into()])
        );
        assert_eq!(
            names(super::moved(&order, 1, 0)),
            Some(vec!["b".into(), "a".into(), "c".into()])
        );
        // Out of range from a stale drag, rather than a panic on `remove`.
        assert_eq!(super::moved(&order, 3, 0), None);
        assert_eq!(super::moved(&order, 0, 4), None);
        assert_eq!(super::moved(&[], 0, 0), None);
    }

    /// What can be pinned, unpinned and dragged, from the flags on the rows.
    #[test]
    fn only_a_pinned_row_is_one_the_list_can_change() {
        let sections = vec![
            Section {
                title: "sidebar.places",
                entries: vec![Entry {
                    path: PathBuf::from("/home/emil"),
                    label: "Home".to_owned(),
                    mark: Mark::Place(Kind::Home),
                    remote: false,
                    pinned: false,
                }],
            },
            Section {
                title: "sidebar.pinned",
                entries: ["/mnt/photos", "/srv/work"]
                    .iter()
                    .map(|path| Entry {
                        path: PathBuf::from(path),
                        label: (*path).to_owned(),
                        mark: Mark::Place(Kind::Other),
                        remote: false,
                        pinned: true,
                    })
                    .collect(),
            },
        ];
        let sidebar = laid_out(sections);
        assert_eq!(sidebar.pinned(), [PathBuf::from("/mnt/photos"), PathBuf::from("/srv/work")]);
        assert!(sidebar.is_pinned(Path::new("/srv/work")));
        assert!(!sidebar.is_pinned(Path::new("/home/emil")), "a well-known directory is not a pin");
        assert!(sidebar.holds(Path::new("/home/emil")), "and it is still a place");
        assert!(!sidebar.holds(Path::new("/etc")));
    }

    /// The divider's floor and ceiling, which is what stops a drag to zero from
    /// being a sidebar nobody can find again.
    #[test]
    fn the_divider_stops_at_a_width_that_can_still_be_read() {
        assert!((super::Sidebar::dragged_to(0.0) - super::NARROWEST).abs() < f32::EPSILON);
        assert!((super::Sidebar::dragged_to(4000.0) - super::WIDEST).abs() < f32::EPSILON);
        assert!((super::Sidebar::dragged_to(300.0) - 300.0).abs() < f32::EPSILON);
    }
}
