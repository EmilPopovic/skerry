// SPDX-License-Identifier: MIT OR Apache-2.0
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
use sk_util::{Cancel, Lane, Pause, WakeHandle};

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
    /// A mutation did not start: something is already called that, and only a
    /// person can say what should happen to it.
    Clashed(Box<Clash>),
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

/// How much there turned out to be, once something counted it.
///
/// **The whole reason there was no ETA.** A rate needs a total, and the total is
/// not known until something has walked to the last file — so this is that walk,
/// running beside the copy on its own job. Metadata is far cheaper than the data
/// the copy is moving, so it almost always finishes first; where it does not, the
/// line goes on saying what has happened, which is what it said before.
#[derive(Clone, Copy, Debug, Default)]
struct Total {
    bytes: u64,
    files: u64,
    /// Whether the walk reached the end. **Nothing is estimated from a partial
    /// total**: a percentage that counts up to 40% and then starts over because
    /// the walk found more is worse than no percentage at all.
    complete: bool,
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
    /// Where a walk through the history is heading, as a count of applied entries.
    ///
    /// `None` when nothing is walking, which is every ordinary undo and redo —
    /// those are a walk of one step that nobody has to track.
    walking: Option<usize>,
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
    /// When the last progress update was sent, shared by every job of one queue.
    ///
    /// **One instant for the queue, not one per item.** Each entry is its own
    /// job, so a throttle starting at each job's own start swallows every update
    /// from an entry that finishes inside the window — and a queue of twenty
    /// folders that each copy in 80 ms reported *no bytes at all*, so the line
    /// never showed a rate and the ETA could never appear. Found by a scenario
    /// asking for the ETA; the one before it only asked whether the line said
    /// "Copying".
    reported: std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
    /// The latch the whole queue waits on, held for as long as it is paused.
    ///
    /// One for the queue rather than one per job, so a pause taken between two
    /// files is still a pause when the next one starts.
    pause: Pause,
    /// How much the queue has to move, once the walk beside it has finished.
    total: Total,
    totals: Receiver<Total>,
    totals_tx: Sender<Total>,
    /// The walk's own token, so a cancelled queue does not leave one running.
    counting: Option<Cancel>,
    /// Whether the queue in flight was stopped, so the notice says so once.
    cancelled_queue: bool,
    /// Whether the queue in flight is picking up what a stopped one left.
    ///
    /// **A flag on the run, not a verb of its own.** It turns an unanswered
    /// conflict into [`sk_fs::ops::Conflict::Resume`], which merges into what is
    /// there instead of refusing it — and only where nobody has answered,
    /// because a person's Replace or Keep both is a decision about this file and
    /// outranks the queue's provenance.
    resuming: bool,
    /// What a stopped queue still owes, until it is picked up or replaced.
    ///
    /// **[Rule 7](../../../docs/03-principles.md#7-honest-progress)'s last
    /// clause**: an operation stays "visible, cancellable and resumable", and
    /// this is the third word. Stop used to drop the rest of the queue on the
    /// floor — the files already moved were recorded, and the ones still owed
    /// were simply gone, so picking up meant selecting them again and knowing
    /// which ones had already arrived.
    ///
    /// **In memory and not in a file.** A queue that survived the process would
    /// be an offer to continue a copy whose sources may have moved, which is the
    /// same argument [ADR
    /// 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md)
    /// makes for leaving the undo stack out of the session.
    stopped: Option<Stopped>,
    /// What a queued operation has changed so far, recorded as one entry when it
    /// drains. A cancelled queue still records what it did: those files moved,
    /// and an undo has to be able to bring them back.
    batch: Vec<Change>,
    /// How far the operation in flight has got, and when it started.
    running: Option<Running>,
    progress: Receiver<Progress>,
    progress_tx: Sender<Progress>,
    /// What the queue is doing to each path it holds.
    doing: Queued,
    /// Paths still to work through, next one last.
    queue: Vec<PathBuf>,
    /// The one the job in flight is working on, so a stop can keep it.
    ///
    /// It is off the queue by then — `next_in_queue` pops it — and it is the
    /// item a resume has something to merge into, so it is the one thing a
    /// stopped queue must not forget.
    running_path: Option<PathBuf>,
    /// The clash a person is being asked about, while one is open.
    ///
    /// The queue does not advance while this is set: that is what makes it a
    /// question rather than a notification.
    pub clash: Option<Clash>,
    /// Which of the answers the panel is on.
    pub answering: usize,
    /// Whether the answer applies to every remaining clash.
    pub for_the_rest: bool,
    /// The standing answer, once one has been given for the rest.
    standing: Option<Answer>,
    /// The answer for the one item about to run, taken by the next job.
    once: Option<Answer>,
    /// How many were asked for, so the notice can count them.
    queued: usize,
    done: Receiver<Outcome>,
    sender: Sender<Outcome>,
    wake: WakeHandle,
}

/// What a stopped queue still owes.
struct Stopped {
    /// What it was doing. The merge is a flag on the run rather than a change to
    /// the verb — [`Edits::resuming`] — because a move carries no conflict answer
    /// of its own and would otherwise be the one verb that could not be picked
    /// up.
    doing: Queued,
    /// Everything still owed, including the item that was interrupted: it is
    /// the one the merge is for.
    owed: Vec<PathBuf>,
    /// How many the queue was originally given, so the line keeps counting the
    /// same total rather than restarting at the remainder.
    queued: usize,
    /// How many of them had finished.
    done: usize,
    /// The verb, for the line that says what stopped.
    verb: &'static str,
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
        /// What to do when the name is already taken there.
        conflict: sk_fs::ops::Conflict,
    },
    /// Delete for good.
    Erase,
    /// Move into this directory.
    Move {
        /// Where they end up.
        into: PathBuf,
    },
}

/// A name the queue wants that is already taken, and the two files under it.
///
/// **Both files' facts, gathered on the job that found the clash.** "Replace or
/// keep both" is not a question anybody can answer about two names; it is a
/// question about a 4 MB file from this morning and a 12 MB one from last year,
/// and the frame loop may not `statx` to find that out.
pub struct Clash {
    /// What the queue is placing.
    pub source: PathBuf,
    /// Where it is going.
    pub into: PathBuf,
    /// What is already called that.
    pub existing: PathBuf,
    /// The file that is there.
    pub theirs: Option<sk_fs::facts::Facts>,
    /// The one arriving.
    pub ours: Option<sk_fs::facts::Facts>,
    /// Whether this is a move rather than a copy, so the panel says which.
    pub moving: bool,
    /// How many are still to come after this one, so "for the rest" is offered
    /// only when there is a rest.
    pub rest: usize,
}

impl Clash {
    /// The answers this one offers, which is not the same set for a move.
    #[must_use]
    pub fn answers(&self) -> &'static [Answer] {
        if self.moving { &Answer::FOR_A_MOVE } else { &Answer::FOR_A_COPY }
    }
}

/// What to do about a name that is already taken.
///
/// **Not commands, and deliberately.** These are the four answers to one
/// question the program asked, the way the Open With list is the applications
/// this machine has rather than four registry entries — putting them in the
/// registry would put "Skip" in the palette, where it would mean nothing and be
/// runnable at any time. Rule 2 is kept by there being no other way to reach
/// them: the panel is the question, and the question is the only thing that
/// opens it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Put what is there in the Trash and place this one.
    Replace,
    /// Place this one beside it, under a free name.
    KeepBoth,
    /// Leave both alone and go on.
    Skip,
    /// Stop the whole queue here.
    Stop,
}

impl Answer {
    /// What a copy offers, in the order the panel puts them.
    pub const FOR_A_COPY: [Self; 4] = [Self::Replace, Self::KeepBoth, Self::Skip, Self::Stop];

    /// What a move offers.
    ///
    /// **No "keep both".** A move that renamed would leave two files under names
    /// neither of them chose and no way to tell which is which, which is why
    /// [`sk_fs::ops::Conflict::Rename`] is a copy's answer only. Offering it here
    /// would be offering a button the engine refuses.
    pub const FOR_A_MOVE: [Self; 3] = [Self::Replace, Self::Skip, Self::Stop];

    /// What it is called, through the catalogue.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Replace => "clash.replace",
            Self::KeepBoth => "clash.keep_both",
            Self::Skip => "clash.skip",
            Self::Stop => "clash.stop",
        }
    }
}

/// What a finished operation asks the rest of the program to do.
pub enum Landed {
    /// Say this, and put the cursor on that path if it is in the listing.
    Notice(String, Option<PathBuf>),
    /// Say this; nothing moved.
    Failed(String),
}

/// Whether `source` would land on a name that is already taken, and what the
/// two files are.
///
/// **Runs on a job**: two `statx` calls and a `readlink`, which is what `facts`
/// costs, and rule 1 has no exemption for a cheap syscall.
///
/// `None` when there is nothing to ask about: the name is free, the answer is
/// already standing, or the copy is into the folder the file came from — which
/// is a duplicate and has never asked.
fn clash_at(
    source: &std::path::Path,
    into: &std::path::Path,
    moving: bool,
    answer: Option<Answer>,
    rest: usize,
) -> Option<Clash> {
    if answer.is_some() {
        return None;
    }
    if !moving && source.parent() == Some(into) {
        return None;
    }
    let existing = into.join(source.file_name()?);
    let theirs = sk_fs::facts::describe(&existing)?;
    Some(Clash {
        source: source.to_path_buf(),
        into: into.to_path_buf(),
        existing,
        ours: sk_fs::facts::describe(source),
        theirs: Some(theirs),
        moving,
        rest,
    })
}

impl Edits {
    /// An empty stack and nothing in flight.
    #[must_use]
    pub fn new(wake: WakeHandle) -> Self {
        let (sender, done) = std::sync::mpsc::channel();
        let (progress_tx, progress) = std::sync::mpsc::channel();
        let (totals_tx, totals) = std::sync::mpsc::channel();
        // Two hundred is a long session's worth of mutations and a bounded
        // amount of memory. Rule 3 asks for visible history, not infinite.
        Self {
            history: History::new(200),
            walking: None,
            naming: None,
            name_after_load: None,
            naming_owed: false,
            pending: None,
            stop: None,
            reported: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            pause: Pause::new(),
            total: Total::default(),
            totals,
            totals_tx,
            counting: None,
            cancelled_queue: false,
            resuming: false,
            stopped: None,
            batch: Vec::new(),
            running: None,
            progress,
            progress_tx,
            doing: Queued::Trash,
            queue: Vec::new(),
            running_path: None,
            clash: None,
            answering: 0,
            for_the_rest: false,
            standing: None,
            once: None,
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

    /// Whether a question is open and waiting to be answered.
    #[must_use]
    pub const fn asking(&self) -> bool {
        self.clash.is_some()
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
    pub fn trash_all(&mut self, paths: Vec<PathBuf>, jobs: &Jobs) -> bool {
        self.queue_all(Queued::Trash, paths, jobs)
    }

    /// Deletes several paths for good, one at a time.
    ///
    /// Nothing here asks whether the caller is sure: the confirmation is the
    /// command's, because it is the command that knows what a person just
    /// pressed twice.
    pub fn erase_all(&mut self, paths: Vec<PathBuf>, jobs: &Jobs) -> bool {
        self.queue_all(Queued::Erase, paths, jobs)
    }

    /// Writes a text file's new contents, as one undoable change.
    ///
    /// **One entry on the one stack**, which is [rule
    /// 3](../../../docs/03-principles.md#3-everything-is-reversible): editing a
    /// file is a mutation like any other, and the thing that makes it safe for
    /// the editor to save without asking is that `Ctrl+Z` takes it back. The old
    /// bytes travel on the change — see [`sk_fs::ops::Change::Edited`] — because
    /// after the write there is nowhere else to read them from.
    pub fn write_text(&mut self, path: PathBuf, after: String, jobs: &Jobs) {
        self.start(Pending::Doing, jobs, move |_| Outcome::Did(ops::edit_text(&path, &after)));
    }

    /// Copies or moves several paths into `into`, one at a time.
    ///
    /// `moving` is what tells a paste after Cut from a paste after Copy. The same
    /// one-at-a-time policy as [`Edits::trash_all`], for the same reasons, and the
    /// same one undo entry each.
    #[must_use]
    pub fn paste_all(
        &mut self,
        paths: Vec<PathBuf>,
        into: PathBuf,
        moving: bool,
        jobs: &Jobs,
    ) -> bool {
        let doing = if moving {
            Queued::Move { into }
        } else {
            // **A copy renames and a move refuses.** Pasting into the folder you
            // copied from is the ordinary way to duplicate something, and
            // refusing it is what every version of this program did until now; a
            // *move* under a name nobody chose would leave two files and no way
            // to tell which is which.
            Queued::Copy { into, conflict: sk_fs::ops::Conflict::Rename }
        };
        self.queue_all(doing, paths, jobs)
    }

    /// Copies each path into the folder it is already in, under a free name.
    pub fn duplicate_all(&mut self, paths: Vec<PathBuf>, jobs: &Jobs) -> bool {
        let Some(into) = paths.first().and_then(|path| path.parent()).map(Path::to_path_buf) else {
            return false;
        };
        let doing = Queued::Copy { into, conflict: sk_fs::ops::Conflict::Rename };
        self.queue_all(doing, paths, jobs)
    }

    /// Queues a batch. Whether it was taken.
    ///
    /// **One operation at a time**, which is what M3 chose and what
    /// [08-roadmap.md](../../../docs/08-roadmap.md) still records: a second batch
    /// arriving while one runs is refused. What is new is that the refusal is
    /// *reported*. It used to return here with the paths dropped on the floor and
    /// nothing said, which is the one thing this program does not do — a drop that
    /// silently loses somebody's files looks exactly like a drop that did not
    /// happen, and the caller is the only one who can say which of the two it was.
    fn queue_all(&mut self, doing: Queued, paths: Vec<PathBuf>, jobs: &Jobs) -> bool {
        if paths.is_empty() {
            return false;
        }
        if self.pending.is_some() {
            return false;
        }
        self.doing = doing;
        self.queued = paths.len();
        // A fresh batch replaces whatever a stopped one still owed: two queues at
        // once is the thing `pending` exists to prevent, and an offer to resume
        // something a person has moved on from is a button that does the wrong
        // thing quietly.
        self.stopped = None;
        self.resuming = false;
        // Reversed, so `pop` takes them in the order they were given — which is
        // display order, so the notice counts down the list a person is looking
        // at.
        self.queue = paths;
        self.queue.reverse();
        self.reported = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        self.start_counting(jobs);
        self.next_in_queue(jobs);
        true
    }

    /// Walks everything the queue holds, so the line can say how much is left.
    ///
    /// **Its own job, beside the copy rather than before it.** Counting first
    /// would make every paste wait on a walk it does not need to start, and the
    /// walk is the cheap half: `statx` per entry against the bytes the copy is
    /// actually moving.
    ///
    /// Trash and Erase are not counted. Neither moves bytes, so a rate over them
    /// would be a number with no meaning, and the line already counts entries.
    fn start_counting(&mut self, jobs: &Jobs) {
        self.total = Total::default();
        if let Some(running) = self.counting.take() {
            running.cancel();
        }
        if matches!(self.doing, Queued::Trash | Queued::Erase) {
            return;
        }
        let paths = self.queue.clone();
        let sender = self.totals_tx.clone();
        let wake = self.wake.clone();
        let token = Cancel::new();
        self.counting = Some(token.clone());
        jobs.pool.spawn(Lane::Parallel, token, move |cancel| {
            let mut carried = Total::default();
            for path in &paths {
                let mut here = sk_index::size::Progress::default();
                sk_index::size::measure(path, cancel, &mut |progress| {
                    here = progress;
                    let running = Total {
                        bytes: carried.bytes + progress.bytes,
                        files: carried.files + progress.files,
                        complete: false,
                    };
                    if sender.send(running).is_ok() {
                        wake.wake();
                    }
                });
                if cancel.is_cancelled() {
                    return;
                }
                carried.bytes += here.bytes;
                carried.files += here.files;
            }
            carried.complete = true;
            if sender.send(carried).is_ok() {
                wake.wake();
            }
        });
    }

    /// Holds the queue where it is, or lets it go.
    ///
    /// **Between files, which is where a cancel also takes effect.** The engine
    /// checks both at the same checkpoints, so a 4 GiB file paused half way
    /// finishes that file and stops before the next. A finer grain would need
    /// the per-extent loop to take a token, which is the same change that would
    /// make a single huge file cancellable — and it is not, today.
    pub fn hold(&mut self, held: bool) {
        if held {
            self.pause.hold();
        } else {
            self.pause.release();
        }
    }

    /// Whether the queue is standing still.
    #[must_use]
    pub fn held(&self) -> bool {
        self.pause.held()
    }

    /// How much the walk beside the queue has counted, and whether it finished.
    #[must_use]
    pub const fn counted(&self) -> (u64, bool) {
        (self.total.bytes, self.total.complete)
    }

    /// Starts the next queued operation, if nothing is in flight.
    pub fn next_in_queue(&mut self, jobs: &Jobs) {
        if self.pending.is_some() || self.clash.is_some() {
            return;
        }
        let Some(path) = self.queue.pop() else {
            self.queued = 0;
            self.running_path = None;
            self.standing = None;
            self.for_the_rest = false;
            return;
        };
        self.running_path = Some(path.clone());
        let doing = self.doing.clone();
        let verb = match doing {
            Queued::Trash => "verb.trashing",
            Queued::Erase => "verb.deleting",
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
        // The answer this one item runs under, whether it came from the panel
        // just now or from a "for the rest" given several items ago.
        let answer = self.once.take().or(self.standing);
        let resuming = self.resuming;
        let queue_left = self.queue.len();
        let pause = self.pause.clone();
        let reported = std::sync::Arc::clone(&self.reported);
        self.start(Pending::Doing, jobs, move |cancel| {
            // Throttled to ten a second: a copy of ten thousand small files would
            // otherwise wake the frame loop ten thousand times, and the loop
            // exists to sleep.
            let mut watch = |progress: sk_fs::copy::Progress| {
                // **The pause happens here, and it is not throttled.** The engine
                // calls this at every point it also checks the cancel token —
                // once per file, per symlink and per directory — so this closure
                // is the checkpoint, and putting the wait behind the throttle
                // would make a pause land up to a tenth of a second of files
                // later than it was asked for.
                pause.wait(cancel);
                let Ok(mut last) = reported.lock() else { return };
                if last.elapsed() < std::time::Duration::from_millis(100) {
                    return;
                }
                *last = std::time::Instant::now();
                drop(last);
                let update =
                    Progress { done: progress.done, bytes: carried_bytes + progress.bytes };
                if updates.send(update).is_ok() {
                    wake.wake();
                }
            };
            let (into, moving) = match &doing {
                Queued::Copy { into, .. } => (Some(into.clone()), false),
                Queued::Move { into } => (Some(into.clone()), true),
                Queued::Trash | Queued::Erase => (None, false),
            };
            // **Asked here, on the job.** Whether a name is taken is a `statx`,
            // and rule 1 has no exemption for a fast one — so the question is
            // put where the work already is, and what comes back is either the
            // clash or the change.
            // **A resumed copy does not ask about the name it is merging into.**
            // That destination is the half-copied tree this run exists to finish,
            // and the question has an answer nobody chose from the panel. A
            // resumed *move* still asks: within one filesystem a move is a
            // rename, which either happened or did not, so a destination that is
            // there is a real clash rather than a remainder.
            let merging = resuming && !moving;
            if let Some(into) = &into
                && !merging
                && let Some(clash) = clash_at(&path, into, moving, answer, queue_left)
            {
                return Outcome::Clashed(Box::new(clash));
            }
            let conflict = match answer {
                Some(Answer::Replace) => sk_fs::ops::Conflict::Replace,
                Some(Answer::KeepBoth) => sk_fs::ops::Conflict::Rename,
                // **A resume merges where nobody has answered.** A person's
                // Replace or Keep both is a decision about this file and outranks
                // where the queue came from.
                None if resuming => sk_fs::ops::Conflict::Resume,
                _ => sk_fs::ops::Conflict::Refuse,
            };
            Outcome::Did(match doing {
                Queued::Trash => ops::trash(&path),
                Queued::Erase => ops::erase(&path, cancel),
                // **A paste into the folder the file came from renames**, with
                // nothing asked: that is what Duplicate is and what pasting
                // where you copied from means, and a panel asking "replace this
                // with itself?" would be a question with no right answer.
                Queued::Copy { into, conflict: policy } => {
                    let same = path.parent() == Some(into.as_path());
                    let conflict = if same { policy } else { conflict };
                    ops::copy_into(&path, &into, conflict, cancel, &mut watch)
                }
                Queued::Move { into } => ops::move_into(&path, &into, conflict, cancel, &mut watch),
            })
        });
    }

    /// Answers the open question and lets the queue go on.
    pub fn answer(&mut self, answer: Answer, jobs: &Jobs) {
        let Some(clash) = self.clash.take() else { return };
        if self.for_the_rest {
            self.standing = Some(answer);
        }
        match answer {
            Answer::Stop => {
                self.queue.clear();
                self.queued = 0;
                self.standing = None;
                self.for_the_rest = false;
                self.cancelled_queue = true;
                self.finish_the_batch();
                return;
            }
            // The item was put back so an answer could run it; skipping takes it
            // off again.
            Answer::Skip => {
                self.queue.pop();
                if self.queue.is_empty() {
                    self.finish_the_batch();
                }
            }
            Answer::Replace | Answer::KeepBoth => self.once = Some(answer),
        }
        let _ = clash;
        self.next_in_queue(jobs);
    }

    /// Moves the panel's highlight, wrapping at both ends.
    pub fn step_answer(&mut self, delta: isize) {
        let Some(offered) = self.clash.as_ref().map(Clash::answers) else { return };
        let Ok(from) = isize::try_from(self.answering) else { return };
        let Ok(span) = isize::try_from(offered.len()) else { return };
        self.answering = usize::try_from(from.wrapping_add(delta).rem_euclid(span)).unwrap_or(0);
    }

    /// The answer the panel is on, or `Stop` when there is no panel.
    #[must_use]
    pub fn chosen_answer(&self) -> Answer {
        let Some(offered) = self.clash.as_ref().map(Clash::answers) else { return Answer::Stop };
        offered.get(self.answering).copied().unwrap_or(Answer::Stop)
    }

    /// Records what a drained queue changed, as one undo entry.
    fn finish_the_batch(&mut self) {
        self.running_path = None;
        self.resuming = false;
        if !self.batch.is_empty() {
            let changes = std::mem::take(&mut self.batch);
            self.history.record(Change::Batch(changes));
        }
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

    /// Walks the history to the point where `applied` entries have been applied.
    ///
    /// **A jump is N single steps, not a new operation.** Every one of them is the
    /// same `revert` or `apply` that `Ctrl+Z` uses, run one at a time on a job,
    /// because that is the only path whose inverses have been tested — and because
    /// a batch of thirty files is already a queue, so a jump over four of them is
    /// not something to run concurrently.
    ///
    /// Nothing starts here if something is already running: the step machinery
    /// below takes over as each one lands.
    pub fn walk_to(&mut self, applied: usize, jobs: &Jobs) {
        let wanted = applied.min(self.history.len());
        if wanted == self.history.applied() {
            self.walking = None;
            return;
        }
        self.walking = Some(wanted);
        self.take_a_step(jobs);
    }

    /// One step towards the point a walk is heading for, or an end to the walk.
    ///
    /// **A refused step ends the walk where it stands.** `History::undo` puts the
    /// entry back when its revert fails — a folder somebody has since put files in
    /// — and retrying it would be an infinite loop over a message the person has
    /// already read. So the walk stops and what has been undone stays undone,
    /// which is also what pressing `Ctrl+Z` four times by hand would have left.
    fn take_a_step(&mut self, jobs: &Jobs) {
        let Some(wanted) = self.walking else { return };
        if self.pending.is_some() {
            return;
        }
        let at = self.history.applied();
        if at == wanted {
            self.walking = None;
            return;
        }
        let before = at;
        if at > wanted {
            self.undo(jobs);
        } else {
            self.redo(jobs);
        }
        // Nothing started, so there is nothing to wait for and no progress to be
        // made: an empty branch is an end to the walk rather than a spin.
        if self.pending.is_none() && self.history.applied() == before {
            self.walking = None;
        }
    }

    /// Takes delivery of a finished operation.
    pub fn drain(&mut self) -> Option<Landed> {
        let outcome = self.done.try_recv().ok()?;
        self.pending = None;
        self.stop = None;
        if let Outcome::Clashed(clash) = outcome {
            // Back on the queue, so whichever answer comes back runs it. `pop`
            // takes the last, so this is the next one out.
            self.queue.push(clash.source.clone());
            self.answering = 0;
            self.clash = Some(*clash);
            return None;
        }
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
                    Change::Trashed { .. } | Change::Erased { .. } => None,
                    kept => Some(kept.subject().to_path_buf()),
                };
                // **Never on the undo stack.** There is nothing to walk back,
                // and an entry that refused would put every earlier change out
                // of reach — so `Ctrl+Z` after a delete undoes the rename before
                // it, which is what a person means by it.
                let recordable = !matches!(change, Change::Erased { .. });
                // **One gesture is one undo.** A queue of two hundred pastes
                // used to record two hundred entries, so `Ctrl+Z` after it took
                // back the last file and left the other hundred and ninety-nine
                // — an undo that did a two-hundredth of what was asked. The
                // batch is recorded when the queue drains, below.
                if recordable {
                    if self.queued > 1 {
                        self.batch.push(change);
                    } else {
                        self.history.record(change);
                    }
                }
                let message = if self.queued > 1 {
                    let done = self.queued - self.queue.len();
                    // **Named by what the queue is doing.** This said "Moved 10
                    // of 10 to Trash" for a paste, because the queue was
                    // generalised from trashing and its wording was not. Three
                    // keys rather than a verb slotted into one sentence: a
                    // sentence assembled from parts is untranslatable by
                    // construction, which is what `i18n` exists to prevent.
                    let key = match self.doing {
                        Queued::Trash => "notice.trashed",
                        Queued::Erase => "notice.deleted",
                        Queued::Copy { .. } => "notice.copied_in",
                        Queued::Move { .. } => "notice.moved_in",
                    };
                    sk_model::i18n::format(key, &[&done, &self.queued])
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
                    self.finish_the_batch();
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
                // The rest of a queue goes with the one that failed, and so does
                // the rest of a walk.
                self.walking = None;
                self.queue.clear();
                self.queued = 0;
                Some(Landed::Failed(problem.message()))
            }
            Outcome::Undid(Err(problem)) => {
                // A refused step ends the walk where it stands — see `take_a_step`.
                self.walking = None;
                Some(Landed::Failed(problem.message()))
            }
            // Taken above, before the queue's bookkeeping runs: a clash is the
            // absence of an outcome, not one of them.
            Outcome::Clashed(_) => None,
        }
    }

    /// Issues the next step of a walk, if one is still going.
    ///
    /// **Called after every drain**, because `drain` has no `Jobs` to start work
    /// with — and putting one there would make taking delivery of an outcome and
    /// starting the next operation the same call, which is the half-done shape
    /// `History`'s own API is built to avoid.
    pub fn continue_walking(&mut self, jobs: &Jobs) {
        if self.walking.is_some() {
            self.take_a_step(jobs);
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
        // **Kept, not dropped.** What is left includes the item in flight: it is
        // the one that stopped part way, and resuming is exactly the merge
        // `Conflict::Resume` performs — see `Stopped`.
        if self.cancellable() {
            let mut owed = std::mem::take(&mut self.queue);
            owed.extend(self.running_path.clone());
            let verb = self.running.as_ref().map_or("verb.copying", |it| it.verb);
            self.stopped = (!owed.is_empty()).then(|| Stopped {
                doing: self.doing.clone(),
                queued: self.queued,
                done: self.queued.saturating_sub(owed.len()),
                owed,
                verb,
            });
        }
        self.queue.clear();
        if let Some(stop) = self.stop.as_ref() {
            stop.cancel();
        }
        if let Some(counting) = self.counting.take() {
            counting.cancel();
        }
        // **Released, or a queue stopped while paused never stops.** The latch
        // outlives the job holding it, so a cancel that left it closed would
        // hold the *next* operation too, and the only thing on screen saying so
        // would be a progress line that had already gone.
        self.pause.release();
    }

    /// Whether there is something worth stopping.
    #[must_use]
    pub fn cancellable(&self) -> bool {
        self.busy() && !matches!(self.doing, Queued::Trash)
    }

    /// Whether a stopped queue is waiting to be picked up.
    #[must_use]
    pub fn resumable(&self) -> bool {
        self.stopped.is_some() && !self.busy()
    }

    /// How much a stopped queue still owes: what is left, of how many.
    #[must_use]
    pub fn owed(&self) -> Option<(usize, usize)> {
        self.stopped.as_ref().map(|it| (it.owed.len(), it.queued))
    }

    /// Picks up what a stopped queue left.
    ///
    /// **The item that was interrupted goes first**, because it is the one with
    /// something on disk to merge into — and the queue pops from the back, so it
    /// is pushed last. Everything the stopped queue had already finished stays
    /// finished: `Conflict::Resume` skips a file that is there at the source's
    /// size and enters a directory that is already there, which is what makes
    /// pressing this twice cost a walk and no bytes.
    pub fn resume(&mut self, jobs: &Jobs) {
        let Some(stopped) = self.stopped.take() else { return };
        if self.pending.is_some() {
            return;
        }
        self.doing = stopped.doing;
        self.queued = stopped.queued;
        self.resuming = true;
        // `owed` is in the order the queue would have worked through it, with the
        // interrupted item appended; `queue` is popped from the back.
        self.queue = stopped.owed;
        self.queue.reverse();
        if let Some(front) = self.queue.pop() {
            self.queue.insert(0, front);
        }
        self.reported = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        self.batch.clear();
        self.start_counting(jobs);
        self.next_in_queue(jobs);
        let _ = stopped.done;
    }

    /// What to say about the operation in flight, if anything.
    ///
    /// **Three sentences, and which one appears says how much is known.** What
    /// has been done, always. The rate, once there is a quarter of a second of
    /// sample. The time left, once the walk beside the copy has reached the last
    /// file — and not before, because an estimate against a total that is still
    /// climbing counts up to 40% and then starts over.
    ///
    /// [Rule 7](../../../docs/03-principles.md) is that progress is honest, and
    /// a bar derived from a prediction is what it exists to forbid. This is a
    /// prediction from two measurements: a total that was counted and a rate that
    /// was observed.
    #[must_use]
    pub fn progress_line(&self) -> Option<String> {
        // **A stopped queue keeps saying so.** Rule 7 is that a long operation
        // stays visible, and the notice that reports the stop expires after four
        // seconds — after which a person who looked away would have nothing on
        // screen telling them that six of nine files are still where they were.
        if let Some(stopped) = self.stopped.as_ref().filter(|_| self.running.is_none()) {
            return Some(sk_model::i18n::format(
                "notice.stopped",
                &[&sk_model::i18n::lookup(stopped.verb), &stopped.owed.len(), &stopped.queued],
            ));
        }
        let running = self.running.as_ref()?;
        let elapsed = running.started.elapsed().as_secs_f64();
        let queued = self.queued.max(1);
        let of = queued - self.queue.len();
        let verb = sk_model::i18n::lookup(running.verb);
        if self.pause.held() {
            return Some(sk_model::i18n::format(
                "notice.paused",
                &[&verb, &of, &queued, &sk_ui::format::size(running.bytes)],
            ));
        }
        // A rate needs a sample. Under a quarter of a second of it is noise, and
        // a number that jumps between 4 MB/s and 900 MB/s is worse than none.
        let Some(rate) =
            (elapsed > 0.25 && running.bytes > 0).then(|| (running.bytes as f64 / elapsed) as u64)
        else {
            return Some(sk_model::i18n::format("notice.progress", &[&verb, &of, &queued]));
        };
        let done = sk_ui::format::size(running.bytes);
        let speed = sk_ui::format::size(rate);
        let left = (self.total.complete && rate > 0)
            .then(|| self.total.bytes.saturating_sub(running.bytes) / rate);
        Some(match left {
            Some(seconds) => sk_model::i18n::format(
                "notice.progress_eta",
                &[
                    &verb,
                    &of,
                    &queued,
                    &done,
                    &sk_ui::format::size(self.total.bytes),
                    &speed,
                    &sk_ui::format::how_long(seconds),
                ],
            ),
            None => sk_model::i18n::format(
                "notice.progress_rate",
                &[&verb, &of, &queued, &done, &speed],
            ),
        })
    }

    /// How far along the whole queue is, from 0 to 1, once that is knowable.
    ///
    /// `None` until the walk has finished, for the reason the ETA is: a fraction
    /// of a total that is still growing goes backwards.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        let running = self.running.as_ref()?;
        (self.total.complete && self.total.bytes > 0)
            .then(|| (running.bytes as f32 / self.total.bytes as f32).clamp(0.0, 1.0))
    }

    /// Takes delivery of progress updates. Returns whether anything moved.
    pub fn drain_progress(&mut self) -> bool {
        let mut moved = false;
        while let Ok(total) = self.totals.try_recv() {
            self.total = total;
            moved = true;
        }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Edits, Queued};
    use crate::pane::Jobs;
    use sk_util::{Pool, Waker};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// An `Edits` with a real pool behind it, and the channels it answers on.
    struct Harness {
        edits: Edits,
        jobs: Jobs,
        _waker: Waker,
        _updates: mpsc::Receiver<crate::pane::Message>,
    }

    impl Harness {
        fn new() -> Self {
            let waker = Waker::new().expect("eventfd");
            let (sender, updates) = mpsc::channel();
            let jobs = Jobs {
                pool: std::sync::Arc::new(Pool::with_width(2)),
                wake: waker.handle(),
                sender,
                mime: None,
            };
            Self { edits: Edits::new(waker.handle()), jobs, _waker: waker, _updates: updates }
        }

        /// Drains outcomes until the queue is idle, or the deadline passes.
        ///
        /// **`next_in_queue` is the caller's**, which is `App::drain_edits` in the
        /// program and this loop here: the engine holds no pool, so the thing
        /// that advances the queue is whoever has one.
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let landed = self.edits.drain();
                if landed.is_some() {
                    self.edits.next_in_queue(&self.jobs);
                    continue;
                }
                if !self.edits.busy() {
                    return;
                }
                // **Yielded rather than slept**, which is `sk_fs::watch`'s own
                // test loop and the reason `tools/check-invariants.py` forbids
                // the other one anywhere in a crate: the rule is about the frame
                // path and a grep cannot tell a test from it.
                std::thread::yield_now();
            }
            panic!(
                "the queue never went idle: busy={} queue={} owed={:?}",
                self.edits.busy(),
                self.edits.queue.len(),
                self.edits.owed(),
            );
        }
    }

    fn scratch(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-edits-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A stopped queue keeps what it owes, and picking it up finishes exactly
    /// that — [rule 7](../../../docs/03-principles.md#7-honest-progress)'s third
    /// word.
    ///
    /// Stopped *before the pool has run anything*, which is the state a person
    /// reaches by pressing Stop early: all three are still owed, and the one
    /// that was in flight is owed too, because a copy that stopped part way is
    /// the one with something to merge into.
    #[test]
    fn a_stopped_queue_keeps_what_it_owes_and_a_resume_finishes_it() {
        let root = scratch("resume");
        let into = root.join("into");
        std::fs::create_dir(&into).unwrap();
        let paths: Vec<PathBuf> = ["one", "two", "three"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::write(&path, b"contents").unwrap();
                path
            })
            .collect();

        let mut harness = Harness::new();
        assert!(harness.edits.paste_all(paths, into.clone(), false, &harness.jobs));
        harness.edits.cancel();
        let (owed, of) = harness.edits.owed().expect("a stopped queue owes nothing");
        assert_eq!(of, 3);
        assert!(owed > 0 && owed <= 3, "{owed} of {of} owed");

        // The queue goes idle, the offer stands until it is taken — and the line
        // says so, because the notice that reported the stop expires and rule 7
        // says a long operation stays visible.
        harness.settle();
        assert!(harness.edits.resumable());
        let line = harness.edits.progress_line().expect("the strip says nothing about the stop");
        assert!(line.contains("stopped"), "{line}");

        harness.edits.resume(&harness.jobs);
        harness.settle();
        assert!(harness.edits.owed().is_none(), "the offer outlived the resume");
        for name in ["one", "two", "three"] {
            assert_eq!(
                std::fs::read(into.join(name)).unwrap_or_default(),
                b"contents",
                "{name} did not arrive"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A fresh batch replaces the offer, rather than leaving a button that would
    /// pick up something the person has moved on from.
    #[test]
    fn a_new_operation_forgets_what_a_stopped_one_owed() {
        let root = scratch("replaced");
        let into = root.join("into");
        std::fs::create_dir(&into).unwrap();
        let first = root.join("first");
        std::fs::write(&first, b"x").unwrap();

        let mut harness = Harness::new();
        assert!(harness.edits.paste_all(vec![first.clone()], into.clone(), false, &harness.jobs));
        harness.edits.cancel();
        harness.settle();
        assert!(harness.edits.resumable());

        let second = root.join("second");
        std::fs::write(&second, b"y").unwrap();
        assert!(harness.edits.paste_all(vec![second], into.clone(), false, &harness.jobs));
        assert!(!harness.edits.resumable(), "the old offer survived a new batch");
        harness.settle();
        assert!(into.join("second").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Trash is not stoppable, so it leaves nothing to pick up.
    #[test]
    fn a_verb_that_cannot_be_stopped_leaves_no_offer() {
        let mut harness = Harness::new();
        assert!(!harness.edits.cancellable());
        harness.edits.cancel();
        assert!(!harness.edits.resumable());
        assert!(matches!(harness.edits.doing, Queued::Trash));
    }
}
