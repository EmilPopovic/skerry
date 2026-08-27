//! Mutations, from the frame loop's side: which one is in flight, and the name
//! being typed for the ones that need one.
//!
//! Every operation is a job, because every one of them is a syscall and rule 1
//! has no exemption for a fast syscall on a mount that has stopped answering.
//! So an operation is three moments: a command starts it, the pool does it, and
//! a later frame takes delivery of what happened.
//!
//! **One at a time.** `pending` is what makes the undo stack safe to drive
//! asynchronously: `History::peek_undo` reads the entry, a copy goes to the pool,
//! and `History::undo` pops it when the answer arrives — and the entry it pops is
//! the entry that was read, because nothing else could have started in between.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use sk_cmd::History;
use sk_fs::ops::{self, Change};
use sk_model::Problem;
use sk_util::{Cancel, Lane, WakeHandle};

use crate::field::Field;
use crate::pane::Jobs;

/// What the pool was asked to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    /// A fresh mutation, to be recorded if it works.
    Doing,
    /// The inverse of the top of the stack.
    Undoing,
    /// The top of the redo branch, again.
    Redoing,
}

/// What the pool did.
enum Outcome {
    /// A mutation finished, for better or worse.
    Did(Result<Change, Problem>),
    /// An inverse finished.
    Undid(Result<(), Problem>),
    /// A repeat finished, with the change it actually produced.
    Redid(Result<Change, Problem>),
}

/// A name being typed, and the row it belongs to.
///
/// Inline in the row rather than in a prompt at the edge of the window, because
/// the name is where a person is looking and a field somewhere else makes them
/// look away from the thing they are naming.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Naming {
    /// The display row it is drawn over.
    pub row: usize,
    /// The path it will rename.
    pub path: PathBuf,
    /// What has been typed, and where the caret is.
    pub field: Field,
}

/// One update from a running tree operation.
#[derive(Clone, Copy, Debug)]
struct Progress {
    done: u64,
    bytes: u64,
}

/// What is in flight, for the status line.
struct Running {
    /// The catalogue key for the word: copying, moving, or trashing.
    verb: &'static str,
    /// Entries finished by the operation in flight.
    done: u64,
    /// Bytes written, carried across the whole queue — so a selection of nine
    /// reads as one operation with one rate rather than nine that each start over.
    bytes: u64,
    started: std::time::Instant,
}

/// Everything the frame loop needs to know about mutations.
pub struct Edits {
    /// One stack for every mutation — [rule 3](../../../docs/03-principles.md).
    pub history: History<Change>,
    /// The name being typed, if one is.
    pub naming: Option<Naming>,
    /// A name to open a field on once the listing containing it lands.
    ///
    /// Making a folder and naming it are two steps with a directory read between
    /// them: the folder has to exist before there is a row to type in.
    pub name_after_load: Option<String>,
    /// Whether the next successful operation's subject should be named.
    ///
    /// Separate from [`Edits::name_after_load`] because the name is not known
    /// when the request is made: `new_directory` picks between `New folder` and
    /// `New folder 2` on the job, and only the outcome says which.
    pub naming_owed: bool,
    pending: Option<Pending>,
    /// The token for the operation in flight, for the ones worth stopping.
    stop: Option<Cancel>,
    /// Whether the queue in flight was stopped, so the notice says so once.
    cancelled_queue: bool,
    /// How far the operation in flight has got, and when it started.
    running: Option<Running>,
    progress: Receiver<Progress>,
    progress_tx: Sender<Progress>,
    /// What the queue is doing to each path it holds.
    doing: Queued,
    /// Paths still to work through, next one last.
    queue: Vec<PathBuf>,
    /// How many were asked for, so the notice can count them.
    queued: usize,
    done: Receiver<Outcome>,
    sender: Sender<Outcome>,
    wake: WakeHandle,
}

/// What the queue is doing to each path it holds.
///
/// One queue rather than three, because "one at a time, one undo entry each, a
/// failure stops the rest" is the same policy for all of them and a second queue
/// beside it would be a second place for that policy to drift.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Queued {
    /// Move to the trash.
    Trash,
    /// Copy into this directory.
    Copy {
        /// Where the copies go.
        into: PathBuf,
    },
    /// Move into this directory.
    Move {
        /// Where they end up.
        into: PathBuf,
    },
}

/// What a finished operation asks the rest of the program to do.
pub enum Landed {
    /// Say this, and put the cursor on that path if it is in the listing.
    Notice(String, Option<PathBuf>),
    /// Say this; nothing moved.
    Failed(String),
}

impl Edits {
    /// An empty stack and nothing in flight.
    #[must_use]
    pub fn new(wake: WakeHandle) -> Self {
        let (sender, done) = std::sync::mpsc::channel();
        let (progress_tx, progress) = std::sync::mpsc::channel();
        // Two hundred is a long session's worth of mutations and a bounded
        // amount of memory. Rule 3 asks for visible history, not infinite.
        Self {
            history: History::new(200),
            naming: None,
            name_after_load: None,
            naming_owed: false,
            pending: None,
            stop: None,
            cancelled_queue: false,
            running: None,
            progress,
            progress_tx,
            doing: Queued::Trash,
            queue: Vec::new(),
            queued: 0,
            done,
            sender,
            wake,
        }
    }

    /// Whether an operation is in flight.
    ///
    /// Every command that mutates is disabled while one is, which is what keeps
    /// the undo stack's asynchronous driving honest.
    #[must_use]
    pub fn busy(&self) -> bool {
        // The queue counts: a selection still going down it is an operation in
        // flight, and `Ctrl+Z` between two of them would undo one file out of
        // four with three more arriving behind it.
        self.pending.is_some() || !self.queue.is_empty()
    }

    /// Makes a directory in `parent`, choosing a free name.
    pub fn new_directory(&mut self, parent: PathBuf, jobs: &Jobs) {
        self.start(Pending::Doing, jobs, move |_| {
            // `New folder`, then `New folder 2`, and so on. `create_directory`
            // refuses a name that is taken, so the loop is the answer rather
            // than a check that races with it.
            let base = sk_model::i18n::lookup("name.new_folder");
            for attempt in 1..1000u32 {
                let name = if attempt == 1 { base.to_owned() } else { format!("{base} {attempt}") };
                match ops::create_directory(&parent, &name) {
                    Err(problem) if problem.kind() == sk_model::ProblemKind::NameTaken => {}
                    other => return Outcome::Did(other),
                }
            }
            Outcome::Did(ops::create_directory(&parent, base))
        });
    }

    /// Renames `path` to `name`.
    pub fn rename(&mut self, path: PathBuf, name: String, jobs: &Jobs) {
        self.start(Pending::Doing, jobs, move |_| Outcome::Did(ops::rename(&path, &name)));
    }

    /// Moves several paths to the trash, one at a time.
    ///
    /// **One at a time and one undo entry each**, which is what rule 3 asks for
    /// and what the engine can honestly do today: M3's queue is the one with
    /// progress, an ETA, a conflict dialogue and a compound undo. The notice
    /// counts them as they go, which is the part of progress that costs nothing.
    ///
    /// A failure stops the rest. Continuing would trash some of a selection and
    /// not others with only the last message visible, and "seven of nine went"
    /// is not a state a person can act on.
    pub fn trash_all(&mut self, paths: Vec<PathBuf>, jobs: &Jobs) {
        self.queue_all(Queued::Trash, paths, jobs);
    }

    /// Copies or moves several paths into `into`, one at a time.
    ///
    /// `moving` is what tells a paste after Cut from a paste after Copy. The same
    /// one-at-a-time policy as [`Edits::trash_all`], for the same reasons, and the
    /// same one undo entry each.
    pub fn paste_all(&mut self, paths: Vec<PathBuf>, into: PathBuf, moving: bool, jobs: &Jobs) {
        let doing = if moving { Queued::Move { into } } else { Queued::Copy { into } };
        self.queue_all(doing, paths, jobs);
    }

    fn queue_all(&mut self, doing: Queued, paths: Vec<PathBuf>, jobs: &Jobs) {
        if self.pending.is_some() || paths.is_empty() {
            return;
        }
        self.doing = doing;
        self.queued = paths.len();
        // Reversed, so `pop` takes them in the order they were given — which is
        // display order, so the notice counts down the list a person is looking
        // at.
        self.queue = paths;
        self.queue.reverse();
        self.next_in_queue(jobs);
    }

    /// Starts the next queued operation, if nothing is in flight.
    pub fn next_in_queue(&mut self, jobs: &Jobs) {
        if self.pending.is_some() {
            return;
        }
        let Some(path) = self.queue.pop() else {
            self.queued = 0;
            return;
        };
        let doing = self.doing.clone();
        let verb = match doing {
            Queued::Trash => "verb.trashing",
            Queued::Copy { .. } => "verb.copying",
            Queued::Move { .. } => "verb.moving",
        };
        // Carried across the queue rather than reset per entry, so nine files
        // read as one operation with one rate.
        let carried = self.running.take().filter(|_| !self.queue.is_empty());
        self.running = Some(carried.unwrap_or(Running {
            verb,
            done: 0,
            bytes: 0,
            started: std::time::Instant::now(),
        }));
        let updates = self.progress_tx.clone();
        let wake = self.wake.clone();
        let carried_bytes = self.running.as_ref().map_or(0, |it| it.bytes);
        self.start(Pending::Doing, jobs, move |cancel| {
            // Throttled to ten a second: a copy of ten thousand small files would
            // otherwise wake the frame loop ten thousand times, and the loop
            // exists to sleep.
            let mut last = std::time::Instant::now();
            let mut watch = |progress: sk_fs::copy::Progress| {
                if last.elapsed() < std::time::Duration::from_millis(100) {
                    return;
                }
                last = std::time::Instant::now();
                let update =
                    Progress { done: progress.done, bytes: carried_bytes + progress.bytes };
                if updates.send(update).is_ok() {
                    wake.wake();
                }
            };
            Outcome::Did(match doing {
                Queued::Trash => ops::trash(&path),
                Queued::Copy { into } => ops::copy_into(&path, &into, cancel, &mut watch),
                Queued::Move { into } => ops::move_into(&path, &into, cancel, &mut watch),
            })
        });
    }

    /// How many are still to go, and how many there were.
    #[must_use]
    pub fn queue_state(&self) -> (usize, usize) {
        (self.queue.len(), self.queued)
    }

    /// Undoes the most recent change.
    pub fn undo(&mut self, jobs: &Jobs) {
        let Some(change) = self.history.peek_undo().cloned() else { return };
        self.start(Pending::Undoing, jobs, move |_| Outcome::Undid(ops::revert(&change)));
    }

    /// Does the most recently undone change again.
    pub fn redo(&mut self, jobs: &Jobs) {
        let Some(change) = self.history.peek_redo().cloned() else { return };
        self.start(Pending::Redoing, jobs, move |_| Outcome::Redid(ops::apply(&change)));
    }

    /// Takes delivery of a finished operation.
    pub fn drain(&mut self) -> Option<Landed> {
        let outcome = self.done.try_recv().ok()?;
        self.pending = None;
        self.stop = None;
        // The queue carries the count and the rate across its entries, so this
        // goes only when there is nothing left to carry them for.
        if self.queue.is_empty() {
            self.running = None;
        }
        match outcome {
            Outcome::Did(Ok(change)) => {
                // **A trashed file has no row to point at.** The cursor moved to
                // the neighbour when the operation started, which is where a
                // person deleting several files in a row needs it; asking for the
                // name that has just gone would take the focus ring off again.
                let subject = match &change {
                    Change::Trashed { .. } => None,
                    kept => Some(kept.subject().to_path_buf()),
                };
                self.history.record(change);
                let message = if self.queued > 1 {
                    let done = self.queued - self.queue.len();
                    sk_model::i18n::format("notice.trashed", &[&done, &self.queued])
                } else {
                    sk_model::i18n::lookup("notice.done").to_owned()
                };
                // Not `queued > 1`, because a cancelled queue of nine has one
                // entry done and eight dropped, and "1 of 9" would read as still
                // running.
                let message = if self.cancelled_queue {
                    self.cancelled_queue = false;
                    sk_model::i18n::lookup("notice.cancelled").to_owned()
                } else {
                    message
                };
                if self.queue.is_empty() {
                    self.queued = 0;
                }
                Some(Landed::Notice(message, subject))
            }
            Outcome::Undid(Ok(())) => {
                let subject =
                    self.history.peek_undo().and_then(|change| change.origin()).map(Path::to_owned);
                // The closure does no work: the job already did it, and what is
                // left is to move the entry to the redo branch.
                let _ = self.history.undo(|_| true);
                Some(Landed::Notice(sk_model::i18n::lookup("notice.undone").to_owned(), subject))
            }
            Outcome::Redid(Ok(fresh)) => {
                let subject = fresh.subject().to_path_buf();
                let _ = self.history.redo(|_| Some(fresh.clone()));
                Some(Landed::Notice(
                    sk_model::i18n::lookup("notice.redone").to_owned(),
                    Some(subject),
                ))
            }
            Outcome::Did(Err(problem)) | Outcome::Redid(Err(problem)) => {
                // The rest of a queue goes with the one that failed.
                self.queue.clear();
                self.queued = 0;
                Some(Landed::Failed(problem.message()))
            }
            Outcome::Undid(Err(problem)) => Some(Landed::Failed(problem.message())),
        }
    }

    /// Cancels any name being typed.
    pub fn stop_naming(&mut self) -> bool {
        self.naming.take().is_some()
    }

    fn start(
        &mut self,
        kind: Pending,
        jobs: &Jobs,
        work: impl FnOnce(&Cancel) -> Outcome + Send + 'static,
    ) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(kind);
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        // `Parallel`: one syscall on the device the listing is already on, and
        // the pane's lane is for reads that can flood it.
        //
        // **Only a tree operation reads the token.** A rename, a `mkdir` or a
        // trash is one syscall that has either happened or not, and abandoning
        // one half way would leave the undo stack describing a change nobody can
        // find. A tree copy is different: it can stop between files, it leaves
        // what it wrote, and what it wrote is recorded so `Ctrl+Z` can take it
        // back.
        let stop = Cancel::new();
        self.stop = Some(stop.clone());
        jobs.pool.spawn(Lane::Parallel, stop, move |cancel| {
            if sender.send(work(cancel)).is_ok() {
                wake.wake();
            }
        });
    }

    /// Stops the operation in flight and drops the rest of the queue.
    ///
    /// A tree copy stops between files and keeps what it wrote — the undo entry
    /// covers it. Anything smaller than that ignores this and finishes, because
    /// there is nothing between its start and its end to stop at.
    pub fn cancel(&mut self) {
        self.cancelled_queue = self.busy();
        self.queue.clear();
        if let Some(stop) = self.stop.as_ref() {
            stop.cancel();
        }
    }

    /// Whether there is something worth stopping.
    #[must_use]
    pub fn cancellable(&self) -> bool {
        self.busy() && !matches!(self.doing, Queued::Trash)
    }

    /// What to say about the operation in flight, if anything.
    ///
    /// **Counted, never estimated from a guess.** The total is not known until
    /// the walk finds the last file, so this says what has been done — and the
    /// rate, once there is enough of a sample for one. [Rule
    /// 7](../../../docs/03-principles.md) is that progress is honest, and a bar
    /// derived from a prediction is what it exists to forbid.
    #[must_use]
    pub fn progress_line(&self) -> Option<String> {
        let running = self.running.as_ref()?;
        let elapsed = running.started.elapsed().as_secs_f64();
        let queued = self.queued.max(1);
        let left = self.queue.len();
        // A rate needs a sample. Under a quarter of a second of it is noise, and
        // a number that jumps between 4 MB/s and 900 MB/s is worse than none.
        let rate =
            (elapsed > 0.25 && running.bytes > 0).then(|| (running.bytes as f64 / elapsed) as u64);
        let of = queued - left;
        Some(match rate {
            Some(rate) => sk_model::i18n::format(
                "notice.progress_rate",
                &[
                    &sk_model::i18n::lookup(running.verb),
                    &of,
                    &queued,
                    &sk_ui::format::size(running.bytes),
                    &sk_ui::format::size(rate),
                ],
            ),
            None => sk_model::i18n::format(
                "notice.progress",
                &[&sk_model::i18n::lookup(running.verb), &of, &queued],
            ),
        })
    }

    /// Takes delivery of progress updates. Returns whether anything moved.
    pub fn drain_progress(&mut self) -> bool {
        let mut moved = false;
        while let Ok(update) = self.progress.try_recv() {
            if let Some(running) = self.running.as_mut() {
                running.done = update.done;
                running.bytes = update.bytes;
                moved = true;
            }
        }
        moved
    }
}
