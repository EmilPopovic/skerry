// SPDX-License-Identifier: Apache-2.0
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
//!
//! **And a place opens.** A row with a twisty shows the directories under it,
//! indented, each of which opens in turn — which is the tree M2 owes the sidebar,
//! and is not the inline Tree [ADR
//! 0013](../../../docs/adr/0013-one-cell-geometry-behind-every-uniform-view.md)
//! declines: a sidebar row is a place to go rather than a file to act on, so
//! there is no selection, no sort and no columns to keep in step. Every
//! expansion is one read on a job, and what is drawn is [`Row`] — the sections'
//! entries with whatever is open under them, flattened, because a heading and a
//! row are the same kind of thing to a hit test.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use sk_desktop::{Kind, Place};
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
    /// How much room is left on it and how much it holds, for a volume that
    /// could say.
    ///
    /// Drawn small and dim at the row's right edge, which is where every other
    /// file manager puts it — and it is the number a person opens a file manager
    /// to find out. **Both halves, because a figure is not a proportion**: 40 GB
    /// free is nearly empty on a laptop's disk and nearly full on a camera card,
    /// and the gauge beside the figure is the half that says which.
    pub free: Option<(u64, u64)>,
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
    /// The block device behind a volume row: `/dev/sdb1`.
    ///
    /// **Present whether it is mounted or not**, because it is what
    /// [`sk_desktop::udisks`] is given for all three of Mount, Unmount and
    /// Eject. `None` for a place, and for a network mount, which has no device.
    pub device: Option<PathBuf>,
    /// Whether this is a volume nothing has mounted.
    ///
    /// **[ADR 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).**
    /// Its `path` is empty, because where a disk *will* be mounted is not known
    /// until something mounts it — and this is the field that says the emptiness
    /// is deliberate rather than a path somebody forgot to fill in. A press on
    /// such a row mounts it and then goes there.
    pub unmounted: bool,
    /// What a trash row is holding, once the walk that counted it has answered.
    ///
    /// **Its own job and not part of [`build`]**, because a trash with two
    /// hundred thousand files in it is a walk to the leaves and the rest of the
    /// list must not wait behind it: the row appears with its name and the
    /// figure arrives after. `None` on every row that is not a trash, and on a
    /// trash whose walk has not landed yet.
    pub held: Option<sk_fs::trash::Held>,
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
    /// Persistent recoverable items on a mounted filesystem.
    Trash,
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

/// One drawn row: a place, or a directory under one.
///
/// **Flattened, with the heading carried by the row that starts a section.** The
/// alternative is a nested loop in the view and a hit test that has to walk the
/// same nesting to turn a `y` back into a row — which is two descriptions of one
/// arrangement, and the second one is the one that goes wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    /// Where pressing it goes.
    pub path: PathBuf,
    /// What it is called.
    pub label: String,
    /// Which mark to draw.
    pub mark: Mark,
    /// Whether reaching it may block for a network timeout.
    pub remote: bool,
    /// How much room is left on it and how much it holds.
    pub free: Option<(u64, u64)>,
    /// Whether it is a line in the bookmarks file.
    pub pinned: bool,
    /// How deep: 0 for a place, 1 for a directory inside one.
    pub depth: usize,
    /// Whether what is under it is being shown.
    pub open: bool,
    /// The section heading this row is the first under, if it is.
    pub heading: Option<&'static str>,
    /// The block device behind a volume row. See [`Entry::device`].
    pub device: Option<PathBuf>,
    /// What a trash row is holding. See [`Entry::held`].
    pub held: Option<sk_fs::trash::Held>,
    /// Whether it is a volume nothing has mounted. See [`Entry::unmounted`].
    pub unmounted: bool,
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
    /// The rows as drawn: the sections' entries, with whatever is open under
    /// each of them.
    ///
    /// Rebuilt when the sections, the expansions or a directory's contents
    /// change — never per frame, and never per press: a hit test and a frame that
    /// disagreed about which row is where would be a click that opened the row
    /// above the one under the pointer.
    visible: Vec<Row>,
    /// The places somebody has opened, in no particular order.
    opened: Vec<PathBuf>,
    /// What each opened directory holds, once the job that read it has answered.
    ///
    /// Subdirectories only, and dotted names left out: a sidebar is a list of
    /// places you go, and `.cache` beside `Documents` is noise in the one list
    /// that has to stay scannable.
    children: Vec<(PathBuf, Vec<String>)>,
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
    /// What each trash turned out to be holding, from the walk that counted it.
    count_rx: Receiver<Vec<(PathBuf, sk_fs::trash::Held)>>,
    count_tx: Sender<Vec<(PathBuf, sk_fs::trash::Held)>>,
    /// The walk's own token, so a list that has been rebuilt stops the walk over
    /// the trashes the old one named.
    counting: Option<Cancel>,
    wake: WakeHandle,
}

impl Sidebar {
    /// An empty sidebar, and the job that fills it.
    #[must_use]
    pub fn new(jobs: &Jobs, wake: WakeHandle, open: bool, width: f32) -> Self {
        let (sender, loaded) = std::sync::mpsc::channel();
        let (count_tx, count_rx) = std::sync::mpsc::channel();
        let sidebar = Self {
            sections: Vec::new(),
            visible: Vec::new(),
            opened: Vec::new(),
            children: Vec::new(),
            open,
            width: width.clamp(NARROWEST, WIDEST),
            hover: None,
            loads: 0,
            loaded,
            sender,
            count_rx,
            count_tx,
            counting: None,
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

    /// Takes delivery of a completed read.
    ///
    /// `None` where nothing arrived. Otherwise the mount points that **were**
    /// mounted before this read and are not now — a disk unplugged, or one
    /// unmounted from anywhere on the machine. Empty where the list only grew,
    /// which is every read at start-up.
    ///
    /// **Returned rather than kept**, because what to do about it is the
    /// window's: a tab standing on a disk that has gone is a tab with no
    /// directory, and this module draws a list rather than deciding what a pane
    /// shows.
    pub fn drain(&mut self) -> Option<Vec<PathBuf>> {
        // **The first read decides whether to look at all.** This runs every
        // frame, and comparing two lists of mount points on a frame that has no
        // answer waiting is an allocation per frame for nothing.
        let Ok(first) = self.loaded.try_recv() else { return None };
        self.counting.take().inspect(Cancel::cancel);
        let before = std::mem::replace(&mut self.sections, first);
        self.loads += 1;
        while let Ok(more) = self.loaded.try_recv() {
            self.sections = more;
            self.loads += 1;
        }
        self.hover = None;
        self.settle();
        Some(vanished(&before, &self.sections))
    }

    /// Starts the walk that counts what each trash is holding.
    ///
    /// **Separate from [`Sidebar::reload`], and after it.** `build` is three
    /// small file reads and a `statx` per candidate; this is a walk to the
    /// leaves of every trash on the machine, and a list that waited for it would
    /// be a sidebar missing for as long as somebody's Trash is large.
    pub fn count_the_trashes(&mut self, jobs: &Jobs) {
        let trashes: Vec<PathBuf> = self
            .sections
            .iter()
            .flat_map(|section| section.entries.iter())
            .filter(|entry| entry.mark == Mark::Trash)
            .map(|entry| entry.path.clone())
            .collect();
        if trashes.is_empty() {
            return;
        }
        let count_tx = self.count_tx.clone();
        let wake = self.wake.clone();
        let token = Cancel::new();
        self.counting = Some(token.clone());
        jobs.pool.spawn(Lane::Parallel, token, move |cancel| {
            let each: Vec<(PathBuf, sk_fs::trash::Held)> = trashes
                .into_iter()
                .map(|path| {
                    let held = sk_fs::trash::held(&path, cancel);
                    (path, held)
                })
                .collect();
            if !cancel.is_cancelled() && count_tx.send(each).is_ok() {
                wake.wake();
            }
        });
    }

    /// Takes delivery of the walk. Whether anything the list draws changed.
    pub fn took_the_count(&mut self) -> bool {
        let mut changed = false;
        while let Ok(counted) = self.count_rx.try_recv() {
            for section in &mut self.sections {
                for entry in &mut section.entries {
                    let Some((_, held)) = counted.iter().find(|(path, _)| *path == entry.path)
                    else {
                        continue;
                    };
                    changed |= entry.held != Some(*held);
                    entry.held = Some(*held);
                }
            }
        }
        if changed {
            self.settle();
        }
        changed
    }

    /// Every trash on the list, and what each is holding.
    ///
    /// **What Empty Trash is given**, and the reason the command needs no
    /// syscall of its own: the walk that found these rows already ran on a job,
    /// so the frame loop is reading a list it has been handed.
    #[must_use]
    pub fn trashes(&self) -> Vec<(PathBuf, sk_fs::trash::Held)> {
        self.visible
            .iter()
            .filter(|row| row.mark == Mark::Trash)
            .map(|row| (row.path.clone(), row.held.unwrap_or_default()))
            .collect()
    }

    /// Every drawn row, in display order.
    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.visible
    }

    /// How many rows there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    /// The row at an index into the flattened list.
    #[must_use]
    pub fn at(&self, index: usize) -> Option<&Row> {
        self.visible.get(index)
    }

    /// Rebuilds the drawn rows from the sections and what is open.
    fn settle(&mut self) {
        let mut rows = Vec::with_capacity(self.visible.len().max(8));
        for section in &self.sections {
            let mut heading = Some(section.title);
            for entry in &section.entries {
                let open = self.is_open(&entry.path);
                rows.push(Row {
                    path: entry.path.clone(),
                    label: entry.label.clone(),
                    mark: entry.mark,
                    remote: entry.remote,
                    free: entry.free,
                    pinned: entry.pinned,
                    depth: 0,
                    open,
                    heading: heading.take(),
                    device: entry.device.clone(),
                    held: entry.held,
                    unmounted: entry.unmounted,
                });
                if open {
                    self.push_children(&entry.path, 1, &mut rows);
                }
            }
        }
        self.visible = rows;
    }

    /// The open directories under `parent`, deepest last.
    ///
    /// **Bounded by what is open**, which is what makes recursion safe here: a
    /// cycle needs a symlink and `children` holds only directories a read
    /// reported, so the depth is the number of twisties somebody has pressed.
    fn push_children(&self, parent: &Path, depth: usize, rows: &mut Vec<Row>) {
        let Some((_, names)) = self.children.iter().find(|(path, _)| path == parent) else {
            return;
        };
        for name in names {
            let path = parent.join(name);
            let open = self.is_open(&path);
            rows.push(Row {
                path: path.clone(),
                label: name.clone(),
                mark: Mark::Place(Kind::Other),
                remote: false,
                free: None,
                pinned: false,
                depth,
                open,
                heading: None,
                device: None,
                held: None,
                unmounted: false,
            });
            if open {
                self.push_children(&path, depth + 1, rows);
            }
        }
    }

    /// Whether what is under `path` is being shown.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        self.opened.iter().any(|it| it == path)
    }

    /// Opens or closes `path`. Whether its contents now have to be read.
    ///
    /// **Read every time it is opened, not once.** A directory changes while a
    /// window is open, and a sidebar that showed what was there ten minutes ago
    /// is the stale listing rule 1 exists to prevent, one panel over.
    pub fn toggle(&mut self, path: &Path) -> bool {
        if self.is_open(path) {
            // Everything under it closes with it: leaving the descendants in the
            // open list would make reopening one row restore a tree somebody
            // collapsed on purpose.
            self.opened.retain(|it| !it.starts_with(path));
            self.settle();
            return false;
        }
        self.opened.push(path.to_path_buf());
        self.settle();
        true
    }

    /// Takes delivery of one directory's subdirectories.
    ///
    /// **Sorted here, by the order the listing uses.** A directory read arrives
    /// in whatever order the filesystem hands it out, and two lists of the same
    /// folder ordered differently is [rule
    /// 8](../../../docs/03-principles.md) broken in the one place a person can
    /// see both at once.
    pub fn took(&mut self, path: PathBuf, mut names: Vec<String>) {
        names.sort_by(|left, right| sk_model::natural_cmp(left, right));
        self.children.retain(|(it, _)| *it != path);
        self.children.push((path, names));
        self.settle();
    }

    /// The places that are open, for the session file.
    #[must_use]
    pub fn open_places(&self) -> &[PathBuf] {
        &self.opened
    }

    /// Opens the places a session file named, without reading them yet.
    ///
    /// The reads are the caller's to start — they are jobs, and this crate's half
    /// of the sidebar owns no pool.
    pub fn open_these(&mut self, places: &[PathBuf]) {
        self.opened = places.to_vec();
        self.settle();
    }

    /// The mount points, which is what a search of everything covers.
    ///
    /// Over the sections rather than the drawn rows: a directory opened under a
    /// volume is inside that filesystem, not another one.
    #[must_use]
    pub fn volumes(&self) -> Vec<PathBuf> {
        self.sections
            .iter()
            .flat_map(|section| section.entries.iter())
            // **And mounted**, because a search walks a path and an unmounted
            // volume has none — ADR 0030. Including one put an empty root into
            // the walk, which is a walk that starts at `""` and finds nothing:
            // measured as `a-search-covers-one-folder-or-every-disk` never
            // leaving the folder it began in.
            .filter(|entry| entry.mark == Mark::Volume && !entry.remote && !entry.unmounted)
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// The pinned places, in the order they are drawn.
    ///
    /// What a reorder is a permutation of, so it is the order on screen and not
    /// the order in the file — the two are the same until a bookmark's directory
    /// goes missing, and `sk_desktop::reorder` refuses a request it cannot map
    /// onto the file rather than guessing.
    #[must_use]
    pub fn pinned(&self) -> Vec<PathBuf> {
        self.places().filter(|entry| entry.pinned).map(|entry| entry.path.clone()).collect()
    }

    /// Whether `path` is one of the pinned places.
    #[must_use]
    pub fn is_pinned(&self, path: &Path) -> bool {
        self.places().any(|entry| entry.pinned && entry.path == path)
    }

    /// The sections' own entries, which are the places themselves.
    ///
    /// **Not the drawn rows.** A directory shown because somebody opened a
    /// twisty over it is not on the list of places, so pinning it is exactly the
    /// thing to offer — and `holds` asking the drawn rows would have refused it.
    fn places(&self) -> impl Iterator<Item = &Entry> + '_ {
        self.sections.iter().flat_map(|section| section.entries.iter())
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
        self.places().any(|entry| entry.path == path)
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
pub(crate) fn current(rows: &[Row], path: &Path) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, row) in rows.iter().enumerate() {
        if !path.starts_with(&row.path) {
            continue;
        }
        let depth = row.path.components().count();
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
pub(crate) fn sample(root: &Path, pinned: bool, opened: bool) -> Vec<Row> {
    let row = |label: &str, path: PathBuf, kind: Kind| Row {
        path,
        label: label.to_owned(),
        mark: Mark::Place(kind),
        remote: false,
        free: None,
        pinned: false,
        depth: 0,
        open: false,
        heading: None,
        device: None,
        held: None,
        unmounted: false,
    };
    let mut rows = vec![
        Row {
            heading: Some("sidebar.places"),
            ..row(sk_model::i18n::lookup("sidebar.home"), root.to_path_buf(), Kind::Home)
        },
        row("documents", root.join("documents"), Kind::Documents),
        row("build", root.join("build"), Kind::Other),
    ];
    // **One place open, with what is under it.** The tree is drawn from the same
    // rows the machine's own sidebar produces, so a frame of it needs a fixture
    // with a depth in it — and `documents` is the one row every other frame here
    // already has.
    if opened {
        rows[1].open = true;
        for (at, name) in ["invoices", "letters"].into_iter().enumerate() {
            rows.insert(
                2 + at,
                Row { depth: 1, ..row(name, root.join("documents").join(name), Kind::Other) },
            );
        }
    }
    // **Off by default, so that adding it did not re-bless fifty-five
    // references.** The sidebar is in every frame, so a row added to this list is
    // a diff in every one of them — and a sweep of fifty-five blessings is a
    // sweep nobody reviews. One frame asks for it.
    if pinned {
        rows.push(Row {
            pinned: true,
            heading: Some("sidebar.pinned"),
            ..row("photos", root.join("photos"), Kind::Other)
        });
        rows.push(Row { pinned: true, ..row("work", root.join("work"), Kind::Other) });
    }
    rows.push(Row {
        heading: Some("sidebar.volumes"),
        path: PathBuf::from("/"),
        label: sk_model::i18n::lookup("sidebar.root").to_owned(),
        mark: Mark::Volume,
        remote: false,
        // Fixed figures, because a golden frame of this machine's own disk is a
        // frame nobody else can reproduce. Four fifths full, which is where the
        // gauge is unmistakably a gauge.
        free: Some((112_089_935_872, 500_107_862_016)),
        pinned: false,
        depth: 0,
        open: false,
        device: Some(PathBuf::from("/dev/sda2")),
        held: None,
        unmounted: false,
    });
    // **And one nothing has mounted** — ADR 0030. A row with a device, no path
    // and no gauge is a shape only this sample can put in a golden frame, and it
    // is the row the reported defect was about.
    rows.push(Row {
        path: PathBuf::new(),
        label: "Camera".to_owned(),
        mark: Mark::Volume,
        remote: false,
        free: None,
        pinned: false,
        depth: 0,
        open: false,
        heading: None,
        device: Some(PathBuf::from("/dev/sdb1")),
        held: None,
        unmounted: true,
    });
    rows
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
    // **Volumes and not mounts** — ADR 0030. A USB stick nothing has mounted is
    // not in the mount table, which is the whole of why one did not appear here.
    let mounts = sk_fs::volumes();

    let mut sections = Vec::with_capacity(3);
    let mut entries: Vec<Entry> = places.iter().map(row).collect();
    if let Ok(path) = sk_fs::trash::home_files() {
        entries.push(trash_row(path, sk_model::i18n::lookup("sidebar.trash").to_owned()));
    }
    for mount in &mounts {
        if mount.remote {
            continue;
        }
        if let Some(at) = &mount.at {
            for path in sk_fs::trash::locations(at) {
                if !entries.iter().any(|entry| entry.path == path) {
                    let label = sk_model::i18n::format("sidebar.volume_trash", &[&at.display()]);
                    entries.push(trash_row(path, label));
                }
            }
        }
    }
    sections.push(Section { title: "sidebar.places", entries });
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

/// The mount points that were mounted in `before` and are not in `after`.
///
/// **A volume that is still listed but is no longer mounted counts too.** A disk
/// unplugged vanishes from the list; one unmounted stays as a row with no path.
/// Either way the directory a tab was standing in has gone, which is the only
/// thing the caller is asking about.
///
/// A place is not a volume and never counts: a bookmark whose directory was
/// deleted is a row that fails to open, not a disk that left.
fn vanished(before: &[Section], after: &[Section]) -> Vec<PathBuf> {
    let mounted = |sections: &[Section]| -> Vec<PathBuf> {
        sections
            .iter()
            .flat_map(|section| section.entries.iter())
            .filter(|entry| entry.mark == Mark::Volume && !entry.unmounted)
            .map(|entry| entry.path.clone())
            .collect()
    };
    let now = mounted(after);
    mounted(before).into_iter().filter(|at| !now.contains(at)).collect()
}

fn trash_row(path: PathBuf, label: String) -> Entry {
    Entry {
        path,
        label,
        mark: Mark::Trash,
        remote: false,
        free: None,
        pinned: false,
        device: None,
        held: None,
        unmounted: false,
    }
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
        free: None,
        pinned: false,
        device: None,
        held: None,
        unmounted: false,
    }
}

/// A volume's row. The root filesystem is named rather than shown as `/`, and a
/// volume nothing has mounted is a row with no path and no bar.
fn volume(volume: &sk_fs::Volume) -> Entry {
    // **The filesystem's own label first.** It is the name somebody gave the
    // disk, and it is what is printed on the outside of a USB stick; the mount
    // point is where the system happened to put it. The root filesystem keeps its
    // word, because "the filesystem" is what a person calls that one whatever it
    // is labelled.
    let label = if volume.is_root() {
        sk_model::i18n::lookup("sidebar.root").to_owned()
    } else {
        volume.name()
    };
    Entry {
        // **Empty for an unmounted volume, and `unmounted` says why.** Where a
        // disk will land is udisks2's to decide and it decides it at mount time,
        // so there is no path to put here and nothing to invent one from.
        path: volume.at.clone().unwrap_or_default(),
        label,
        mark: Mark::Volume,
        remote: volume.remote,
        free: volume.free,
        pinned: false,
        device: volume.device.clone(),
        held: None,
        unmounted: !volume.is_mounted(),
    }
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
        let (count_tx, count_rx) = std::sync::mpsc::channel();
        let mut sidebar = Sidebar {
            sections,
            visible: Vec::new(),
            opened: Vec::new(),
            children: Vec::new(),
            open: true,
            width: super::DEFAULT_WIDTH,
            hover: None,
            loads: 0,
            loaded,
            sender,
            count_rx,
            count_tx,
            counting: None,
            wake: waker.handle(),
        };
        sidebar.settle();
        sidebar
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
                    free: None,
                    pinned: false,
                    device: None,
                    held: None,
                    unmounted: false,
                })
                .collect(),
        }]
    }

    fn at(sections: &[Section], path: &str) -> Option<usize> {
        super::current(laid_out(sections.to_vec()).rows(), Path::new(path))
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

    /// This machine's own three sources: whatever they hold, every row a press
    /// navigates to points at a directory that exists, and nothing appears
    /// twice.
    ///
    /// **Except an unmounted volume, whose path is empty on purpose** — ADR
    /// 0030. Where a disk will land is udisks2's to decide at mount time, so the
    /// row carries a device and no path, and a press on it mounts rather than
    /// navigates. Its own claims are below it.
    #[test]
    fn what_this_machine_offers_is_a_list_of_places_that_exist() {
        let sections = build();
        assert!(!sections.is_empty(), "no sections at all");
        let rows: Vec<&Entry> =
            sections.iter().flat_map(|section| section.entries.iter()).collect();
        for entry in &rows {
            assert!(!entry.label.is_empty(), "{entry:?}");
            if entry.unmounted {
                assert_eq!(entry.path, PathBuf::new(), "an unmounted row has a path: {entry:?}");
                assert!(entry.device.is_some(), "an unmounted row has no device: {entry:?}");
                assert!(entry.free.is_none(), "an unmounted row reported room: {entry:?}");
                continue;
            }
            assert!(entry.path.is_dir(), "{entry:?}");
        }
        let mut paths: Vec<_> =
            rows.iter().filter(|entry| !entry.unmounted).map(|entry| &entry.path).collect();
        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "the same place twice: {rows:?}");
        // And two unmounted volumes are two devices, never two of one.
        let mut devices: Vec<_> =
            rows.iter().filter(|entry| entry.unmounted).map(|entry| &entry.device).collect();
        let devices_count = devices.len();
        devices.sort_unstable();
        devices.dedup();
        assert_eq!(devices.len(), devices_count, "the same device twice: {rows:?}");
    }

    /// **What a disk leaving looks like from here**, and the three things that
    /// are not one: a volume that only became unmounted still counts, because
    /// the directory a tab was standing in has gone either way; a volume that
    /// appeared counts for nothing; and a *place* that went is a bookmark whose
    /// directory was deleted, which is a row that fails to open rather than a
    /// disk that left.
    #[test]
    fn a_volume_that_stops_being_mounted_is_one_that_went() {
        let volume = |at: &str, mounted: bool| Entry {
            path: if mounted { PathBuf::from(at) } else { PathBuf::new() },
            label: at.to_owned(),
            mark: Mark::Volume,
            remote: false,
            free: None,
            pinned: false,
            device: Some(PathBuf::from("/dev/sdb1")),
            held: None,
            unmounted: !mounted,
        };
        let place = |at: &str| Entry {
            path: PathBuf::from(at),
            label: at.to_owned(),
            mark: Mark::Place(Kind::Other),
            remote: false,
            free: None,
            pinned: true,
            device: None,
            held: None,
            unmounted: false,
        };
        let sections = |entries: Vec<Entry>| vec![Section { title: "sidebar.volumes", entries }];

        let before = sections(vec![
            volume("/", true),
            volume("/run/media/emil/STICK", true),
            volume("/run/media/emil/CARD", true),
            place("/home/emil/repos"),
        ]);
        // The stick was unplugged, the card was unmounted and is still listed, a
        // new disk appeared, and the bookmark is gone.
        let after = sections(vec![
            volume("/", true),
            volume("/run/media/emil/CARD", false),
            volume("/run/media/emil/NEW", true),
        ]);
        let mut gone = super::vanished(&before, &after);
        gone.sort();
        assert_eq!(
            gone,
            vec![PathBuf::from("/run/media/emil/CARD"), PathBuf::from("/run/media/emil/STICK"),]
        );

        // And nothing changing is nothing gone, which is every read but the
        // interesting one.
        assert!(super::vanished(&before, &before).is_empty());
        assert!(super::vanished(&[], &after).is_empty(), "the first read of all");
    }

    /// A volume's row carries a proportion, not just a figure.
    ///
    /// **Both halves or neither**, because the gauge divides by the total: a row
    /// that knew the free space and not the size would draw a bar of nothing, and
    /// `statvfs` answers both or is not called at all.
    #[test]
    fn a_volume_says_how_much_is_left_and_how_much_there_is() {
        let sections = build();
        let volumes: Vec<&Entry> = sections
            .iter()
            .flat_map(|section| section.entries.iter())
            .filter(|entry| entry.mark == Mark::Volume)
            .collect();
        for volume in &volumes {
            let Some((free, total)) = volume.free else { continue };
            assert!(total > 0, "a volume that holds nothing: {volume:?}");
            assert!(free <= total, "more free than it holds: {volume:?}");
        }
        // And the sample the golden frames draw is one, so a frame of the gauge
        // is a frame of a proportion somebody could have.
        let drawn = super::sample(std::path::Path::new("/"), false, false);
        let volume = drawn.iter().find(|row| row.mark == Mark::Volume).expect("a volume row");
        let (free, total) = volume.free.expect("the sample says how full it is");
        assert!(free < total && free * 10 > total, "an unconvincing gauge: {free}/{total}");
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
                    free: None,
                    pinned: false,
                    device: None,
                    held: None,
                    unmounted: false,
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
                        free: None,
                        pinned: true,
                        device: None,
                        held: None,
                        unmounted: false,
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

    /// A place opens, what is under it is indented, and closing it takes the
    /// whole subtree with it.
    #[test]
    fn a_place_opens_and_what_is_under_it_is_indented() {
        let mut sidebar = laid_out(fake(&["/home/emil", "/srv"]));
        assert_eq!(sidebar.len(), 2);
        assert!(sidebar.toggle(Path::new("/home/emil")), "opening asked for no read");
        // Open, and nothing under it until the read lands: a twisty that waits
        // is the honest state, not a row that guesses.
        assert_eq!(sidebar.len(), 2);
        assert!(sidebar.is_open(Path::new("/home/emil")));

        // Handed over in readdir order and drawn in the listing's order, which
        // is the one a person can see beside it.
        sidebar.took(PathBuf::from("/home/emil"), vec!["repos".to_owned(), "notes".to_owned()]);
        let shape: Vec<(usize, &str)> =
            sidebar.rows().iter().map(|row| (row.depth, row.label.as_str())).collect();
        assert_eq!(shape, [(0, "/home/emil"), (1, "notes"), (1, "repos"), (0, "/srv")]);

        // A child opens too, and its own children go under it.
        assert!(sidebar.toggle(Path::new("/home/emil/repos")));
        sidebar.took(PathBuf::from("/home/emil/repos"), vec!["skerry".to_owned()]);
        let shape: Vec<(usize, &str)> =
            sidebar.rows().iter().map(|row| (row.depth, row.label.as_str())).collect();
        assert_eq!(
            shape,
            [(0, "/home/emil"), (1, "notes"), (1, "repos"), (2, "skerry"), (0, "/srv")]
        );

        // **Closing the place closes what was open inside it.** Leaving the
        // descendants on the list would make reopening one row restore a tree
        // somebody collapsed on purpose.
        assert!(!sidebar.toggle(Path::new("/home/emil")), "closing asked for a read");
        assert_eq!(sidebar.len(), 2);
        assert!(!sidebar.is_open(Path::new("/home/emil/repos")));
        assert!(sidebar.open_places().is_empty());
    }

    /// A row shown because a twisty was opened is not a place, so it can be
    /// pinned — and the section headings still belong to the rows that start
    /// them.
    #[test]
    fn a_row_under_a_place_is_not_itself_a_place() {
        let mut sidebar = laid_out(fake(&["/home/emil"]));
        sidebar.toggle(Path::new("/home/emil"));
        sidebar.took(PathBuf::from("/home/emil"), vec!["repos".to_owned()]);
        assert!(sidebar.holds(Path::new("/home/emil")));
        assert!(!sidebar.holds(Path::new("/home/emil/repos")), "a tree row refused a pin");
        assert_eq!(sidebar.rows()[0].heading, Some("sidebar.places"));
        assert_eq!(sidebar.rows()[1].heading, None, "a child started a section");

        // And the highlight follows the deepest row, which is what the tree is
        // for: standing in `repos` lights `repos` rather than the place above it.
        assert_eq!(super::current(sidebar.rows(), Path::new("/home/emil/repos/skerry")), Some(1));
    }
}
