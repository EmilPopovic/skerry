//! One pane: a path, the listing at that path, and what is selected in it.
//!
//! The pane never touches the filesystem. It asks the job pool to, and applies
//! what comes back — which is what makes a hung mount a stale row rather than a
//! frozen window.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use sk_fs::{META_CHUNK, MetaRow};
use sk_model::{EntryId, Flags, Listing, Problem, Selection, Sort, SortColumn};
use sk_util::{Cancel, Lane, Pool, WakeHandle};

/// What a directory read reports back.
///
/// The protocol lives here rather than in `sk-fs`, because deciding how a read is
/// broken into jobs is deciding *what* to enumerate, which the crate table gives
/// to this level and not to that one.
pub enum Update {
    /// Every name, with metadata still pending.
    Names {
        /// The listing.
        listing: Box<Listing>,
        /// The names in entry order, for the metadata jobs.
        names: Arc<Vec<String>>,
        /// Microseconds the name pass took.
        elapsed_us: u64,
        /// Microseconds the first sort took, on the job rather than on a frame.
        sort_us: u64,
    },
    /// Metadata for one slice of the names.
    Meta(Vec<MetaRow>),
    /// A display order computed on a job.
    Ordered {
        /// Which sort produced it. Two sorts of the same generation can be in
        /// flight — a header clicked twice — and without this the one that lands
        /// last wins, which is not the one that was asked for last.
        epoch: u64,
        /// The permutation.
        order: Vec<EntryId>,
        /// Microseconds it took.
        elapsed_us: u64,
    },
    /// The read failed, with a message a human can act on.
    Failed(Problem),
}

/// One update from one directory read, tagged with the read it came from.
pub struct Message {
    /// Which read produced it. A pane discards anything older than its current
    /// read, so navigating away from a million-entry directory costs nothing and
    /// cannot make the new listing flicker.
    pub generation: u64,
    /// What the read found.
    pub update: Update,
}

/// What a pane needs in order to start a read.
pub struct Jobs {
    /// The one pool.
    pub pool: Pool,
    /// How a finished job wakes the frame loop.
    pub wake: WakeHandle,
    /// Where updates go.
    pub sender: Sender<Message>,
}

/// Directories remembered for Back and Forward.
///
/// A session that visits more than this has stopped using Back as an undo for a
/// wrong turn and started using it as a log, which is what Recents is for. The
/// oldest entry is dropped rather than the navigation refused.
const HISTORY_DEPTH: usize = 128;

/// Entries a reorder will do on the calling thread.
///
/// Measured: 6,167 entries take 4.3 ms and 200,000 take 272 ms. 8,192 is the
/// largest power of two whose reorder fits inside one 8 ms frame with room to
/// spare, and below it a round trip through the pool costs more than the sort.
const SORT_INLINE: usize = 8_192;

/// A pane.
pub struct Pane {
    /// Where it is.
    pub path: PathBuf,
    /// What is there.
    pub listing: Listing,
    /// What is selected, and where the cursor is.
    pub selection: Selection,
    /// How far down.
    pub scroll: sk_ui::Scroll,
    /// In what order.
    pub sort: Sort,
    /// Whether dotfiles are shown.
    pub show_hidden: bool,
    /// Whether a read is still running.
    pub loading: bool,
    /// Why the last read failed, if it did.
    pub problem: Option<Problem>,
    /// The display row under the pointer.
    pub hover: Option<usize>,
    /// Microseconds for the name pass of the last read.
    pub names_us: u64,
    /// Microseconds for the metadata pass of the last read.
    pub meta_us: u64,
    /// Microseconds for the last reorder.
    pub sort_us: u64,

    /// Where Back goes, newest last.
    back: Vec<PathBuf>,
    /// Where Forward goes, newest last. Cleared by any fresh navigation.
    forward: Vec<PathBuf>,

    cancel: Cancel,
    generation: u64,
    /// Which lane this directory's device belongs on, probed once per read.
    lane: Lane,
    /// Metadata jobs dispatched, and how many have come back.
    meta_expected: usize,
    meta_done: usize,
    /// When the metadata jobs were dispatched, so `meta_us` is wall clock across
    /// the fan-out rather than the sum of per-job times.
    meta_started: Option<Instant>,
    /// Set when metadata arrived and the current sort depends on it.
    reorder_owed: bool,
    /// Whether a sort job is in flight, so a second one is not queued behind it.
    sorting: bool,
    /// Cancels the sort, and **only** the sort.
    ///
    /// Separate from `cancel` because they retire on different schedules. The
    /// read's token is shared by the names job and every `statx` chunk; a second
    /// header click used to cancel it to retire a stale sort, and take the
    /// outstanding metadata with it. `sk_fs::metadata` stops early and still
    /// reports, so `meta_done` reached `meta_expected`, `loading` went false, and
    /// most of the directory kept a blank size and date until `F5`.
    sort_cancel: Cancel,
    /// Which sort is current. An `Ordered` from any earlier one is discarded.
    sort_epoch: u64,
    /// A name to put the cursor on once the names land.
    ///
    /// Set by [`Pane::go_up`], because coming out of a folder should leave the
    /// cursor on the folder you came out of — and the listing that contains it
    /// does not exist yet at the moment the decision is made.
    select_after_load: Option<String>,
}

impl Pane {
    /// An empty pane at `path`. Nothing is read until [`Pane::reload`].
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            listing: Listing::default(),
            selection: Selection::default(),
            scroll: sk_ui::Scroll::default(),
            sort: Sort::default(),
            show_hidden: false,
            loading: false,
            problem: None,
            hover: None,
            names_us: 0,
            meta_us: 0,
            sort_us: 0,
            back: Vec::new(),
            forward: Vec::new(),
            cancel: Cancel::new(),
            generation: 0,
            lane: Lane::Parallel,
            meta_expected: 0,
            meta_done: 0,
            meta_started: None,
            reorder_owed: false,
            sorting: false,
            sort_cancel: Cancel::new(),
            sort_epoch: 0,
            select_after_load: None,
        }
    }

    /// Navigates to `path`, remembering where it came from.
    pub fn open(&mut self, path: PathBuf, jobs: &Jobs) {
        if path == self.path {
            return;
        }
        // A fresh navigation ends the forward branch, which is what Back and
        // Forward mean everywhere a person has met them before.
        self.forward.clear();
        self.back.push(std::mem::replace(&mut self.path, path));
        if self.back.len() > HISTORY_DEPTH {
            self.back.remove(0);
        }
        self.arrive(jobs);
    }

    /// Goes back, if there is anywhere to go. Returns whether it moved.
    pub fn go_back(&mut self, jobs: &Jobs) -> bool {
        let Some(previous) = self.back.pop() else { return false };
        self.forward.push(std::mem::replace(&mut self.path, previous));
        // Coming back to a directory should land the cursor on whatever was left
        // from, the same way going up does.
        self.select_after_load = leaf_of(self.forward.last());
        self.arrive(jobs);
        true
    }

    /// Goes forward, if Back was used. Returns whether it moved.
    pub fn go_forward(&mut self, jobs: &Jobs) -> bool {
        let Some(next) = self.forward.pop() else { return false };
        self.back.push(std::mem::replace(&mut self.path, next));
        self.arrive(jobs);
        true
    }

    /// Whether Back and Forward have anywhere to go, for a menu or a toolbar.
    #[must_use]
    pub fn can_go(&self) -> (bool, bool) {
        (!self.back.is_empty(), !self.forward.is_empty())
    }

    /// Goes to `$HOME`, or does nothing if the environment has none.
    pub fn go_home(&mut self, jobs: &Jobs) {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            self.open(home, jobs);
        }
    }

    /// Resets everything that is about the old directory, then reads the new one.
    fn arrive(&mut self, jobs: &Jobs) {
        self.listing = Listing::default();
        self.selection = Selection::default();
        self.scroll.to(0.0);
        self.problem = None;
        self.hover = None;
        self.reload(jobs);
    }

    /// Re-reads the current path.
    ///
    /// The previous read is cancelled first, and its generation is retired, so
    /// two reads of the same directory cannot interleave into one listing.
    pub fn reload(&mut self, jobs: &Jobs) {
        self.cancel.cancel();
        self.cancel = Cancel::new();
        // A sort of the listing that is being replaced has nothing to order.
        // Clearing `sorting` here is not cosmetic: `apply` drops an `Ordered`
        // from a retired generation, so the flag would otherwise stay true for
        // the life of the pane and the next header click would cancel a live
        // read.
        self.sort_cancel.cancel();
        self.sort_cancel = Cancel::new();
        self.sorting = false;
        self.generation += 1;
        self.loading = true;
        self.names_us = 0;
        self.meta_us = 0;
        self.meta_expected = 0;
        self.meta_done = 0;
        self.meta_started = None;
        // The lane is probed per directory, not per entry: a read on an
        // unresponsive network mount gets one worker between all of them, and a
        // local one gets the whole pool.
        self.lane = sk_fs::lane_for(&self.path);

        let generation = self.generation;
        let path = self.path.clone();
        let sender = jobs.sender.clone();
        let wake = jobs.wake.clone();
        let sort = self.sort;
        let show_hidden = self.show_hidden;
        jobs.pool.spawn(self.lane, self.cancel.clone(), move |cancel| {
            let update = match sk_fs::names(&path, cancel) {
                Ok(found) => {
                    // The first sort happens **here**, on the job, not when the
                    // listing is applied. At 200,000 entries it costs a quarter
                    // of a second, and a quarter of a second on the frame loop is
                    // rule 1's canonical failure.
                    let mut listing = found.listing;
                    let started = std::time::Instant::now();
                    listing.set_order(listing.compute_order(sort, show_hidden));
                    Update::Names {
                        listing: Box::new(listing),
                        names: found.names,
                        elapsed_us: found.elapsed_us,
                        sort_us: started.elapsed().as_micros() as u64,
                    }
                }
                // A cancelled read has nothing to report: the pane has already
                // moved on, and a failure message would be about a directory
                // nobody is looking at.
                Err(_) if cancel.is_cancelled() => return,
                Err(problem) => Update::Failed(problem),
            };
            // A closed channel means the window is gone, and the job should stop
            // rather than finish reading a million entries for nobody.
            if sender.send(Message { generation, update }).is_err() {
                cancel.cancel();
                return;
            }
            wake.wake();
        });
    }

    /// Dispatches one metadata job per slice of the names.
    ///
    /// This is Spike B's fan-out. A single directory of a million entries is
    /// **3.4× faster** with `statx` spread across the pool, and its p99 goes from
    /// 11.9 s to 1.2 s — and measured here, a 200,000-entry directory went from
    /// 186 ms on one thread to the figure in
    /// [docs/09-performance-budgets.md](../../../docs/09-performance-budgets.md).
    fn dispatch_metadata(&mut self, names: &Arc<Vec<String>>, jobs: &Jobs) {
        let chunks = names.len().div_ceil(META_CHUNK);
        self.meta_expected = chunks;
        self.meta_done = 0;
        self.meta_started = Some(Instant::now());
        if chunks == 0 {
            self.loading = false;
            return;
        }
        for chunk in 0..chunks {
            let start = chunk * META_CHUNK;
            let end = (start + META_CHUNK).min(names.len());
            let names = Arc::clone(names);
            let path = self.path.clone();
            let sender = jobs.sender.clone();
            let wake = jobs.wake.clone();
            let generation = self.generation;
            jobs.pool.spawn(self.lane, self.cancel.clone(), move |cancel| {
                let rows =
                    sk_fs::metadata(&path, &names[start..end], EntryId(start as u32), cancel);
                if sender.send(Message { generation, update: Update::Meta(rows) }).is_err() {
                    cancel.cancel();
                    return;
                }
                wake.wake();
            });
        }
    }

    /// Which pool lane this directory's device belongs on.
    #[must_use]
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// Cancels everything this pane started.
    ///
    /// Called when the window closes: a directory read of a million entries must
    /// not hold the process open after there is nothing to show it to.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.sort_cancel.cancel();
    }

    /// Re-reads the current path, keeping the selection and the scroll position.
    ///
    /// What a change notification triggers, as distinct from [`Pane::reload`]:
    /// a file appearing must not move the cursor or scroll the view, because the
    /// person may be reading a row at the time. The cursor is restored by name.
    pub fn refresh(&mut self, jobs: &Jobs) {
        self.select_after_load = self
            .selection
            .cursor()
            .map(|id| self.listing.name(id).to_owned())
            .or_else(|| self.select_after_load.take());
        // The scroll is left exactly as it is. It used to be read and written
        // back, which looks like a no-op and is not: `Scroll::to` sets the
        // animation's *target*, so a refresh arriving mid-fling stopped the
        // fling dead — and a folder being written into refreshes four times a
        // second. `reload` does not touch the scroll, and `set_extent` clamps
        // it on the next frame if the directory got shorter.
        self.reload(jobs);
    }

    /// Goes to the parent directory, keeping the child selected.
    pub fn go_up(&mut self, jobs: &Jobs) {
        let Some(parent) = self.path.parent().map(Path::to_path_buf) else { return };
        let leaving = self.path.file_name().map(|name| name.to_string_lossy().into_owned());
        self.open(parent, jobs);
        // Remembered across the read and applied when the names land, because
        // "up" should leave the cursor on the folder you came out of.
        self.select_after_load = leaving;
    }

    /// Applies one update, ignoring anything from a retired read.
    pub fn apply(&mut self, message: Message, jobs: &Jobs) -> bool {
        if message.generation != self.generation {
            return false;
        }
        match message.update {
            Update::Names { listing, names, elapsed_us, sort_us } => {
                self.carry_selection(&listing);
                self.listing = *listing;
                self.names_us = elapsed_us;
                self.sort_us = sort_us;
                self.restore_cursor();
                self.dispatch_metadata(&names, jobs);
            }
            Update::Meta(rows) => {
                let mut kinds_changed = false;
                for row in rows {
                    let was = self.listing.flags(row.id).has(Flags::DIR);
                    self.listing.set_meta(row.id, row.size, row.mtime, row.extra);
                    kinds_changed |= was != self.listing.flags(row.id).has(Flags::DIR);
                }
                // A name sort does not depend on metadata, so it is never
                // redone; the other two are redone once, when the last slice
                // lands. Reordering per slice would resort the whole listing
                // every two thousand entries and the user would watch the rows
                // churn.
                //
                // **Grouping directories first does depend on it.** `getdents64`
                // reports a symlink as a symlink, and only `statx` says whether
                // it points at a directory — so a link to a folder drew a folder
                // icon, opened on Enter, and sat down among the files. On a
                // filesystem that answers `DT_UNKNOWN` at all — XFS without
                // `ftype=1`, every 9p mount — nothing grouped first until the
                // list was re-sorted by hand.
                self.reorder_owed |=
                    self.sort.column != SortColumn::Name || (self.sort.dirs_first && kinds_changed);
                self.meta_done += 1;
                if self.meta_done >= self.meta_expected {
                    self.loading = false;
                    if let Some(started) = self.meta_started.take() {
                        self.meta_us = started.elapsed().as_micros() as u64;
                    }
                    if std::mem::take(&mut self.reorder_owed) {
                        self.reorder(jobs);
                    }
                }
            }
            Update::Ordered { epoch, order, elapsed_us } => {
                // A sort that was overtaken still finishes and still sends. It
                // carries the same generation as the one that overtook it, so
                // only the epoch can tell them apart.
                if epoch != self.sort_epoch {
                    return false;
                }
                self.sorting = false;
                self.sort_us = elapsed_us;
                self.listing.set_order(order);
                self.restore_cursor();
                self.forget_hidden_cursor();
            }
            Update::Failed(problem) => {
                self.loading = false;
                self.problem = Some(problem);
            }
        }
        true
    }

    /// Rebuilds the display order, on this thread or on a job.
    ///
    /// Inline below [`SORT_INLINE`] entries and on a job above it. The threshold
    /// is measured rather than chosen: 6,167 entries reorder in 4.3 ms, which is
    /// inside the 8 ms frame budget and cheaper than a round trip through the
    /// pool, while 200,000 take 272 ms, which is not a frame at all.
    pub fn reorder(&mut self, jobs: &Jobs) {
        if self.listing.rows() <= SORT_INLINE && self.listing.len() <= SORT_INLINE {
            let started = std::time::Instant::now();
            self.listing.reorder(self.sort, self.show_hidden);
            self.sort_us = started.elapsed().as_micros() as u64;
            self.forget_hidden_cursor();
            return;
        }
        if self.sorting {
            // A second request while one is in flight would race to publish. The
            // pending one is discarded and this one replaces it, because what the
            // user last asked for is what they want to see.
            self.sort_cancel.cancel();
            self.sort_cancel = Cancel::new();
        }
        self.sorting = true;
        self.sort_epoch += 1;
        // A clone rather than a borrow: the job cannot hold the pane's listing,
        // and metadata slices are still landing on it.
        let snapshot = self.listing.clone();
        let (sort, show_hidden) = (self.sort, self.show_hidden);
        let sender = jobs.sender.clone();
        let wake = jobs.wake.clone();
        let generation = self.generation;
        let epoch = self.sort_epoch;
        jobs.pool.spawn(self.lane, self.sort_cancel.clone(), move |cancel| {
            let started = std::time::Instant::now();
            let order = snapshot.compute_order(sort, show_hidden);
            if cancel.is_cancelled() {
                return;
            }
            let elapsed_us = started.elapsed().as_micros() as u64;
            if sender
                .send(Message { generation, update: Update::Ordered { epoch, order, elapsed_us } })
                .is_ok()
            {
                wake.wake();
            }
        });
    }

    /// Sorts by `column`, reversing if it is already the sort column.
    pub fn sort_by(&mut self, column: SortColumn, jobs: &Jobs) {
        self.sort = self.sort.toggled(column);
        self.reorder(jobs);
    }

    /// Shows or hides dotfiles.
    pub fn toggle_hidden(&mut self, jobs: &Jobs) {
        self.show_hidden = !self.show_hidden;
        self.reorder(jobs);
    }

    /// Drops a cursor the current order does not show.
    ///
    /// Dropped rather than moved: clamping it silently puts the focus ring on an
    /// unrelated file, which is worse than having no focus ring.
    fn forget_hidden_cursor(&mut self) {
        let cursor = self.selection.cursor();
        if cursor.is_some_and(|id| self.listing.row_of(id).is_none()) {
            self.selection.set_cursor(None);
        }
    }

    /// The display row the cursor is on.
    #[must_use]
    pub fn cursor_row(&self) -> Option<usize> {
        self.selection.cursor().and_then(|id| self.listing.row_of(id))
    }

    /// Moves the cursor by `delta` rows, extending the selection if asked.
    pub fn move_cursor(&mut self, delta: isize, extend: bool) {
        let rows = self.listing.rows();
        if rows == 0 {
            return;
        }
        let current = self.cursor_row();
        let next = match current {
            // With no cursor, the first move lands on an end rather than
            // somewhere in the middle: down goes to the top, up to the bottom.
            None if delta > 0 => 0,
            None => rows - 1,
            Some(row) => row.saturating_add_signed(delta).min(rows - 1),
        };
        let Some(id) = self.listing.at(next) else { return };
        if extend {
            self.selection.extend_to(id, self.listing.order());
        } else {
            self.selection.replace_with(id);
        }
    }

    /// Moves the cursor to an absolute row.
    pub fn cursor_to(&mut self, row: usize, extend: bool) {
        let Some(id) = self.listing.at(row.min(self.listing.rows().saturating_sub(1))) else {
            return;
        };
        if extend {
            self.selection.extend_to(id, self.listing.order());
        } else {
            self.selection.replace_with(id);
        }
    }

    /// Moves the cursor to the first name starting with `prefix`.
    ///
    /// Case-insensitive and ASCII-folded only. Full Unicode case folding is a
    /// table and belongs with the collation work; for a jump-to-name this is the
    /// behaviour every file manager has and nobody notices.
    pub fn jump_to(&mut self, prefix: &str) -> bool {
        let needle = prefix.to_lowercase();
        let found = (0..self.listing.rows()).find(|row| {
            self.listing
                .at(*row)
                .is_some_and(|id| self.listing.name(id).to_lowercase().starts_with(&needle))
        });
        if let Some(row) = found {
            self.cursor_to(row, false);
        }
        found.is_some()
    }

    /// The entry to act on when there is one: the cursor, or the topmost
    /// selected entry.
    ///
    /// Topmost on screen, not lowest `EntryId` — with no cursor to point at,
    /// the only defensible answer is the one at the top of the list.
    #[must_use]
    pub fn focused(&self) -> Option<EntryId> {
        self.selection.cursor().or_else(|| self.selection.in_order(self.listing.order()).next())
    }

    /// Opens the focused entry if it is a directory. Returns whether it did.
    pub fn open_focused(&mut self, jobs: &Jobs) -> bool {
        let Some(id) = self.focused() else { return false };
        if !self.listing.flags(id).has(Flags::DIR) {
            return false;
        }
        let child = self.path.join(self.listing.name(id));
        self.open(child, jobs);
        true
    }

    /// Carries the selection across a re-read, by name.
    ///
    /// `EntryId` is an index into the listing it came from. A watcher-driven
    /// refresh rebuilds that listing, so bit *n* afterwards is a different file:
    /// with five files written into the folder underneath a `Ctrl+A`, the status
    /// line went on saying three selected and the highlight had moved to three
    /// rows nobody had touched. Anything acting on the selection acted on those.
    ///
    /// Costs nothing when nothing is selected, which is the ordinary case. When
    /// something is, it is one hash set of the selected names and one pass over
    /// the arriving listing — no strings are copied, because both listings are
    /// alive at this point and one can be looked up in the other.
    fn carry_selection(&mut self, arriving: &Listing) {
        self.selection.set_cursor(None);
        if self.selection.is_empty() {
            self.selection.fit(arriving.len());
            return;
        }
        let kept: std::collections::HashSet<&str> =
            self.selection.iter().map(|id| self.listing.name(id)).collect();
        let mut carried = Selection::default();
        carried.fit(arriving.len());
        for index in 0..arriving.len() {
            let id = EntryId(index as u32);
            if kept.contains(arriving.name(id)) {
                carried.select(id);
            }
        }
        self.selection = carried;
    }

    /// Puts the cursor back on a remembered name, leaving the selection alone.
    fn restore_cursor(&mut self) {
        let Some(wanted) = self.select_after_load.take() else { return };
        let found = (0..self.listing.rows())
            .filter_map(|row| self.listing.at(row))
            .find(|id| self.listing.name(*id) == wanted);
        let Some(id) = found else { return };
        if self.selection.is_empty() {
            // Coming out of a folder leaves that folder focused *and* selected,
            // which is what arriving anywhere else with the keyboard does.
            self.selection.replace_with(id);
        } else {
            // A refresh, where the selection has just been carried across by
            // name. `cursor_to` would be a click, and a click clears it.
            self.selection.set_cursor(Some(id));
        }
    }
}

/// The final component of a path, for putting a cursor back on it.
fn leaf_of(path: Option<&PathBuf>) -> Option<String> {
    path?.file_name().map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Jobs, Message, Pane, SORT_INLINE, Update};
    use sk_model::EntryId;
    use sk_util::{Pool, Waker};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Harness {
        jobs: Jobs,
        updates: mpsc::Receiver<super::Message>,
        _waker: Waker,
    }

    impl Harness {
        fn new() -> Self {
            let waker = Waker::new().expect("eventfd");
            let (sender, updates) = mpsc::channel();
            let jobs = Jobs { pool: Pool::with_width(4), wake: waker.handle(), sender };
            Self { jobs, updates, _waker: waker }
        }

        /// Applies updates until the pane says it is done, or the deadline.
        fn settle(&self, pane: &mut Pane) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.updates.recv_timeout(Duration::from_millis(20)) {
                    Ok(message) => {
                        pane.apply(message, &self.jobs);
                    }
                    Err(_) if !pane.loading => break,
                    Err(_) => {}
                }
            }
            // Drain anything that landed while the last slice was applied.
            while let Ok(message) = self.updates.try_recv() {
                pane.apply(message, &self.jobs);
            }
        }
    }

    fn names_of(pane: &Pane) -> Vec<String> {
        (0..pane.listing.rows())
            .filter_map(|row| pane.listing.at(row))
            .map(|id| pane.listing.name(id).to_owned())
            .collect()
    }

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-nav-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for name in ["parent-one.txt", "parent-two.txt", "parent-three.txt"] {
            std::fs::write(root.join(name), b"p").unwrap();
        }
        for name in ["child-a.txt", "child-b.txt", "child-c.txt", "child-d.txt"] {
            std::fs::write(root.join("sub").join(name), b"c").unwrap();
        }
        root
    }

    /// `getdents64` says "symlink" and only `statx` says "and it points at a
    /// directory", so grouping folders first cannot be settled from the names
    /// pass alone. Before the reorder was owed on a kind change, `to-sub` drew a
    /// folder icon, opened on Enter, and sat below the files.
    #[test]
    fn a_symlink_to_a_directory_sorts_with_the_directories() {
        let root = fixture("symlink");
        if std::os::unix::fs::symlink(root.join("sub"), root.join("to-sub")).is_err() {
            return;
        }
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        assert_eq!(
            names_of(&pane),
            vec!["sub", "to-sub", "parent-one.txt", "parent-three.txt", "parent-two.txt"],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The selection is a bitset indexed by `EntryId`, and a refresh rebuilds
    /// the listing those indices point into. Carried by name, the same files
    /// stay selected; carried by index, three files nobody touched become
    /// selected and every operation acts on those.
    #[test]
    fn a_refresh_keeps_the_files_that_were_selected_and_not_their_row_numbers() {
        let root = fixture("carry");
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        let wanted = ["parent-two.txt", "parent-three.txt"];
        for name in wanted {
            let id = (0..pane.listing.len())
                .map(|index| EntryId(index as u32))
                .find(|id| pane.listing.name(*id) == name)
                .expect(name);
            pane.selection.select(id);
        }
        assert_eq!(pane.selection.count(), 2);

        // Five names that sort ahead of everything already there, so every
        // `EntryId` moves.
        for index in 0..5 {
            std::fs::write(root.join(format!("aa-{index}.txt")), b"new").unwrap();
        }
        pane.refresh(&harness.jobs);
        harness.settle(&mut pane);

        let mut still: Vec<String> = pane
            .selection
            .in_order(pane.listing.order())
            .map(|id| pane.listing.name(id).to_owned())
            .collect();
        still.sort();
        assert_eq!(still, vec!["parent-three.txt", "parent-two.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And the cursor landing back on its name does not collapse the selection
    /// to that one row, which is what `cursor_to` would have done — once per
    /// refresh, so up to four times a second while a download runs.
    #[test]
    fn a_refresh_does_not_collapse_a_selection_to_the_cursor() {
        let root = fixture("collapse");
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        let order = pane.listing.order().to_vec();
        pane.selection.select_all(&order);
        pane.selection.set_cursor(order.first().copied());
        let before = pane.selection.count();
        assert!(before > 1);

        pane.refresh(&harness.jobs);
        harness.settle(&mut pane);
        assert_eq!(pane.selection.count(), before, "the refresh collapsed the selection");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Retiring a stale sort must not retire the read. They shared one token,
    /// so a second click on a column header cancelled every outstanding `statx`
    /// chunk — and those jobs report anyway, so the pane believed it had
    /// finished loading with most of its rows still blank.
    #[test]
    fn re_sorting_does_not_cancel_the_metadata_that_is_still_arriving() {
        let harness = Harness::new();
        let mut pane = Pane::new(std::env::temp_dir());
        pane.reload(&harness.jobs);
        let read = pane.cancel.clone();
        // Above `SORT_INLINE`, which is the only case that reaches a job and so
        // the only case that has ever had two sorts in flight at once.
        for index in 0..=SORT_INLINE {
            pane.listing.push(&format!("entry-{index:06}"), sk_model::Flags::NONE);
        }

        pane.reorder(&harness.jobs);
        assert!(pane.sorting, "a listing this size should sort on a job");
        pane.reorder(&harness.jobs);

        assert!(!read.is_cancelled(), "the second sort cancelled the directory read");
    }

    /// Two sorts of one listing can both be in flight and both finish. Only the
    /// one that was asked for last may be shown, or the rows are ordered by the
    /// previous column while the header underline says the new one.
    #[test]
    fn an_overtaken_sort_does_not_publish_over_the_one_that_overtook_it() {
        let root = fixture("epoch");
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        let current = pane.listing.order().to_vec();
        let mut backwards = current.clone();
        backwards.reverse();
        let stale = Message {
            generation: pane.generation,
            update: Update::Ordered {
                epoch: pane.sort_epoch.wrapping_sub(1),
                order: backwards,
                elapsed_us: 0,
            },
        };
        assert!(!pane.apply(stale, &harness.jobs), "a retired sort was accepted");
        assert_eq!(pane.listing.order(), current.as_slice());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Entering a directory shows that directory, and nothing from the one it
    /// was entered from.
    #[test]
    fn navigating_into_a_directory_replaces_the_listing_entirely() {
        let root = fixture("into");
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        let parent = names_of(&pane);
        assert_eq!(
            parent,
            vec!["sub", "parent-one.txt", "parent-three.txt", "parent-two.txt",],
            "the parent listing is wrong before navigation even happens"
        );

        let sub = (0..pane.listing.len())
            .map(|index| EntryId(index as u32))
            .find(|id| pane.listing.name(*id) == "sub")
            .expect("sub");
        pane.selection.replace_with(sub);
        assert!(pane.open_focused(&harness.jobs), "sub is a directory");
        harness.settle(&mut pane);

        assert_eq!(pane.path, root.join("sub"));
        assert_eq!(
            names_of(&pane),
            vec!["child-a.txt", "child-b.txt", "child-c.txt", "child-d.txt",]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And going back up shows the parent, with the cursor on what was left.
    #[test]
    fn going_up_shows_the_parent_and_lands_the_cursor_on_the_child() {
        let root = fixture("up");
        let harness = Harness::new();
        let mut pane = Pane::new(root.join("sub"));
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        assert_eq!(names_of(&pane).len(), 4);

        pane.go_up(&harness.jobs);
        harness.settle(&mut pane);
        assert_eq!(pane.path, root);
        assert_eq!(
            names_of(&pane),
            vec!["sub", "parent-one.txt", "parent-three.txt", "parent-two.txt",]
        );
        let cursor = pane.selection.cursor().expect("a cursor on the folder we came out of");
        assert_eq!(pane.listing.name(cursor), "sub");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two navigations in a row, with no settling between them: the second one
    /// wins and the first leaves nothing behind.
    #[test]
    fn a_second_navigation_before_the_first_lands_discards_the_first() {
        let root = fixture("race");
        let harness = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        pane.open(root.join("sub"), &harness.jobs);
        harness.settle(&mut pane);

        assert_eq!(
            names_of(&pane),
            vec!["child-a.txt", "child-b.txt", "child-c.txt", "child-d.txt",]
        );
        assert_eq!(pane.listing.len(), 4, "no entries from the abandoned read");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_that_is_gone_reports_a_problem_and_an_empty_listing() {
        let harness = Harness::new();
        let mut pane = Pane::new(Path::new("/nonexistent-skerry-nav").to_path_buf());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        assert!(pane.problem.is_some());
        assert_eq!(pane.listing.rows(), 0);
        assert!(!pane.loading);
    }
}
