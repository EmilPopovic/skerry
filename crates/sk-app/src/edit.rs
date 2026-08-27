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
    done: Receiver<Outcome>,
    sender: Sender<Outcome>,
    wake: WakeHandle,
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
        // Two hundred is a long session's worth of mutations and a bounded
        // amount of memory. Rule 3 asks for visible history, not infinite.
        Self {
            history: History::new(200),
            naming: None,
            name_after_load: None,
            naming_owed: false,
            pending: None,
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
        self.pending.is_some()
    }

    /// Makes a directory in `parent`, choosing a free name.
    pub fn new_directory(&mut self, parent: PathBuf, jobs: &Jobs) {
        self.start(Pending::Doing, jobs, move || {
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
        self.start(Pending::Doing, jobs, move || Outcome::Did(ops::rename(&path, &name)));
    }

    /// Moves `paths` to the trash, one operation each.
    ///
    /// One at a time and not one batch, because the queue with honest progress is
    /// M3's and this is not it: what a person selects here is a handful of files,
    /// and each is its own undo entry.
    pub fn trash(&mut self, path: PathBuf, jobs: &Jobs) {
        self.start(Pending::Doing, jobs, move || Outcome::Did(ops::trash(&path)));
    }

    /// Undoes the most recent change.
    pub fn undo(&mut self, jobs: &Jobs) {
        let Some(change) = self.history.peek_undo().cloned() else { return };
        self.start(Pending::Undoing, jobs, move || Outcome::Undid(ops::revert(&change)));
    }

    /// Does the most recently undone change again.
    pub fn redo(&mut self, jobs: &Jobs) {
        let Some(change) = self.history.peek_redo().cloned() else { return };
        self.start(Pending::Redoing, jobs, move || Outcome::Redid(ops::apply(&change)));
    }

    /// Takes delivery of a finished operation.
    pub fn drain(&mut self) -> Option<Landed> {
        let outcome = self.done.try_recv().ok()?;
        self.pending = None;
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
                Some(Landed::Notice(sk_model::i18n::lookup("notice.done").to_owned(), subject))
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
        work: impl FnOnce() -> Outcome + Send + 'static,
    ) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(kind);
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        // `Parallel`: one syscall on the device the listing is already on, and
        // the pane's lane is for reads that can flood it.
        jobs.pool.spawn(Lane::Parallel, Cancel::new(), move |_| {
            if sender.send(work()).is_ok() {
                wake.wake();
            }
        });
    }
}
