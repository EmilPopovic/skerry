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
            cancel: Cancel::new(),
            generation: 0,
            lane: Lane::Parallel,
            meta_expected: 0,
            meta_done: 0,
            meta_started: None,
            reorder_owed: false,
            sorting: false,
            select_after_load: None,
        }
    }

    /// Navigates to `path` and starts reading it.
    pub fn open(&mut self, path: PathBuf, jobs: &Jobs) {
        self.path = path;
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
                self.listing = *listing;
                self.names_us = elapsed_us;
                self.sort_us = sort_us;
                self.selection.fit(self.listing.len());
                self.restore_cursor();
                self.dispatch_metadata(&names, jobs);
            }
            Update::Meta(rows) => {
                for row in rows {
                    self.listing.set_meta(row.id, row.size, row.mtime, row.extra);
                }
                // A name sort does not depend on metadata, so it is never
                // redone; the other two are redone once, when the last slice
                // lands. Reordering per slice would resort the whole listing
                // every two thousand entries and the user would watch the rows
                // churn.
                self.reorder_owed |= self.sort.column != SortColumn::Name;
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
            Update::Ordered { order, elapsed_us } => {
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
            self.cancel.cancel();
            self.cancel = Cancel::new();
        }
        self.sorting = true;
        // A clone rather than a borrow: the job cannot hold the pane's listing,
        // and metadata slices are still landing on it.
        let snapshot = self.listing.clone();
        let (sort, show_hidden) = (self.sort, self.show_hidden);
        let sender = jobs.sender.clone();
        let wake = jobs.wake.clone();
        let generation = self.generation;
        jobs.pool.spawn(self.lane, self.cancel.clone(), move |cancel| {
            let started = std::time::Instant::now();
            let order = snapshot.compute_order(sort, show_hidden);
            if cancel.is_cancelled() {
                return;
            }
            let elapsed_us = started.elapsed().as_micros() as u64;
            if sender
                .send(Message { generation, update: Update::Ordered { order, elapsed_us } })
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

    /// The entry to act on when there is one: the cursor, or the only selected
    /// entry.
    #[must_use]
    pub fn focused(&self) -> Option<EntryId> {
        self.selection.cursor().or_else(|| self.selection.iter().next())
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

    fn restore_cursor(&mut self) {
        let Some(wanted) = self.select_after_load.take() else { return };
        for row in 0..self.listing.rows() {
            if self.listing.at(row).is_some_and(|id| self.listing.name(id) == wanted) {
                self.cursor_to(row, false);
                return;
            }
        }
    }
}
