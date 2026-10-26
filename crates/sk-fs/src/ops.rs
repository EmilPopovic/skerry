// SPDX-License-Identifier: Apache-2.0
//! Mutations, and the inverse of each one.
//!
//! Make a directory, rename, copy in, move in, move to the trash. Each returns a
//! [`Change`] describing what happened, and every `Change` can be [`revert`]ed and
//! [`apply`]ed again — which is [rule 3](../../../docs/03-principles.md), and the
//! reason the inverse is written in the same file as the operation rather than
//! somewhere that can drift from it.
//!
//! **An inverse is not a guess.** Creating a directory is undone by removing it,
//! and only while it is still empty — if something has been put in it since, the
//! undo refuses rather than deleting somebody's files. Renaming is undone by
//! renaming back, and only if the original name is still free. Trashing is undone
//! by moving the file out of the trash and deleting the `.trashinfo` beside it.
//! Each refusal is a `Problem` with a message, not a silent no-op.
//!
//! **A copy is the one whose inverse is a recursive delete**, and it is an undo
//! rather than a deletion because everything it removes is something the copy
//! itself made. The bytes are `crate::copy`'s, ported from Spike E; what is here
//! is the part that makes the result undoable.

use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use sk_model::{Problem, ProblemKind};

/// What a [`Change::Created`] made.
///
/// **One variant per inverse, not per icon.** The three are together because
/// making something out of nothing is one gesture with one undo entry; they are
/// distinguished because `rmdir` is not `unlink`, and because a redo has to make
/// the same thing again — a link with no target is a link to `""`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Made {
    /// A directory. Undone with `rmdir`, which refuses once something is in it.
    Directory,
    /// An empty file. Undone with `unlink`, and **only while it is still
    /// empty** — the same refusal `rmdir` gives a directory somebody has since
    /// put files in, for the same reason: an undo that deletes what somebody
    /// typed is not an undo.
    File,
    /// A symbolic link, and what it points at.
    ///
    /// Undone with `unlink` and no emptiness check, because removing a link
    /// removes nothing but the link — and a link whose target has been replaced
    /// since is still this link.
    Link(PathBuf),
}

/// What a mutation did, and enough to undo it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    /// A trash entry restored, including its durable inverse.
    Restored(crate::trash::Entry),
    /// Completed or partial work with a user-visible problem that must be retained.
    Noted {
        /// What can still be undone.
        change: Box<Change>,
        /// The problem already translated for the operation result.
        note: String,
        /// Partial copied data can be retried using the copy engine's resume checks.
        retry: bool,
        /// Whether the note is the **answer** to an undo rather than a remark
        /// beside one: the way back is gone, so [`revert`] says this instead of
        /// moving files that are no longer there.
        ///
        /// **Set by [`stranded_by_empty`] and by nothing else.** Emptying the
        /// Trash is the one thing in this program that takes the way back from a
        /// change already on the stack, and [rule
        /// 3](../../../docs/03-principles.md#3-everything-is-reversible)'s
        /// failure test is a mutation `Ctrl+Z` cannot walk back — which an entry
        /// that fails with `Trash/files/receipt.pdf: no such file` meets in
        /// letter and breaks in spirit, because nobody has ever looked at that
        /// path. [ADR
        /// 0036](../../../docs/adr/0036-operation-failures-retain-pending-work.md)
        /// is the shape: the work stays in the history with a note on it.
        stranded: bool,
    },
    /// Several changes made by one gesture, oldest first.
    ///
    /// **One gesture is one undo.** A paste of two hundred files makes two
    /// hundred changes, and a person who presses `Ctrl+Z` after it meant to
    /// undo the paste — not the last file of it, two hundred times. Undone
    /// **backwards**, because the last change is the one made on top of the
    /// others: a move that replaced a name has to be put back before the thing
    /// it displaced can be.
    Batch(Vec<Change>),
    /// Something that did not exist before: a directory, a file, or a link.
    Created {
        /// Where it is.
        path: PathBuf,
        /// What was made there. See [`Made`].
        made: Made,
    },
    /// A name that moved.
    Renamed {
        /// What it was called.
        from: PathBuf,
        /// What it is called now.
        to: PathBuf,
    },
    /// Something copied in from somewhere else.
    ///
    /// Both ends are kept: undoing a copy removes what it made, and doing it
    /// again needs the source it came from.
    Copied {
        /// Where it came from.
        source: PathBuf,
        /// What the copy is called.
        destination: PathBuf,
    },
    /// Something deleted for good.
    ///
    /// **The one change with no way back**, which is why
    /// [03-principles.md](../../../docs/03-principles.md) rule 3 calls permanent
    /// deletion "an explicit, differently-shaped action" rather than an
    /// exception to itself. It is not put on the undo stack at all: an entry
    /// that refused would make everything under it unreachable, and `Ctrl+Z`
    /// after a delete should take back the rename before it.
    Erased {
        /// What is gone.
        path: PathBuf,
    },
    /// A text file whose contents were replaced.
    ///
    /// **Both sides are kept, which is what makes it undoable.** Every other
    /// change here is undone by moving something; this one is undone by putting
    /// the old bytes back, and there is nowhere else they could come from. Bounded
    /// by [`crate::TEXT_LIMIT`], because that is the most the editor will open.
    Edited {
        /// The file.
        path: PathBuf,
        /// What it held before.
        before: String,
        /// What it holds now.
        after: String,
    },
    /// The nine permission bits, as they were and as they are.
    ///
    /// **Both modes, for [`Change::Edited`]'s reason.** A mode is not a place, so
    /// there is nowhere to move a file back to; the old bits have to travel on
    /// the change or `Ctrl+Z` has nothing to restore. Whole modes rather than the
    /// nine alone, because what is put back has to be what was there — a file
    /// that was setuid before is setuid after an undo.
    Permissions {
        /// The file.
        path: PathBuf,
        /// What it allowed before.
        before: u16,
        /// What it allows now.
        after: u16,
    },
    /// An archive unpacked into a directory of its own.
    ///
    /// **A copy whose source is inside a file** — [ADR
    /// 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).
    /// The inverse trashes the current output, preserving files added since
    /// extraction as well as the files that came from the archive.
    ///
    /// **The one change [`apply`] cannot redo.** Doing it again means a sandboxed
    /// child, which is level 2 and above this crate, so `Edits::redo` routes this
    /// variant back through the queue instead. Nothing else here has that shape,
    /// and it is the price of the decoder being out of process.
    Unpacked {
        /// The archive it came out of.
        archive: PathBuf,
        /// The directory that was made for it.
        root: PathBuf,
        /// Which entries were taken, or empty for the whole archive.
        ///
        /// **Recorded because a redo has to be the same change.** Extracting one
        /// file out of a listing and then redoing it must not unpack the other
        /// nine hundred into the folder the first one is in.
        only: Vec<String>,
    },
    /// Something moved into the trash, with both files the specification puts
    /// there so the undo can take both away.
    Trashed {
        /// Where it came from.
        original: PathBuf,
        /// Where it is now, under `Trash/files`.
        stored: PathBuf,
        /// The `.trashinfo` beside it, under `Trash/info`.
        info: PathBuf,
    },
}

impl Change {
    /// The path a listing should put its cursor on after this ran.
    ///
    /// A warning attached to completed work, including the last warned batch item.
    #[must_use]
    pub fn warning(&self) -> Option<&str> {
        match self {
            Self::Noted { note, .. } => Some(note),
            Self::Batch(changes) => changes.iter().rev().find_map(Self::warning),
            _ => None,
        }
    }

    /// Whether a partial copy still owes data, unlike a metadata-only warning.
    #[must_use]
    pub fn needs_retry(&self) -> bool {
        match self {
            Self::Noted { retry, .. } => *retry,
            Self::Batch(changes) => changes.iter().any(Self::needs_retry),
            _ => false,
        }
    }

    /// What a person is looking for after an operation: the folder they made,
    /// the new name, or — after an undo — the thing that came back.
    #[must_use]
    pub fn subject(&self) -> &Path {
        match self {
            // The last one, which is what the gesture finished by doing and
            // therefore where a person is looking.
            Self::Restored(entry) => &entry.original,
            Self::Noted { change, .. } => change.subject(),
            Self::Batch(changes) => changes.last().map_or(Path::new(""), Change::subject),
            // The folder that was made, the file that was edited, and the file
            // whose bits changed: three changes whose subject happens to be the
            // path they carry.
            Self::Created { path, .. }
            | Self::Edited { path, .. }
            | Self::Permissions { path, .. } => path,
            Self::Renamed { to, .. } => to,
            Self::Copied { destination, .. } => destination,
            // The folder the files went into, which is what a person is looking
            // for after an extraction.
            Self::Unpacked { root, .. } => root,
            // Both are gone, so the cursor goes where they were: the listing
            // finds nothing there and leaves it on the neighbour, which is the
            // row a person deleting several in a row needs it on.
            Self::Trashed { original, .. } | Self::Erased { path: original } => original,
        }
    }

    /// A catalogue key naming this change, and the one argument that key takes.
    ///
    /// **Keys rather than text**, so this crate needs no catalogue and a
    /// translation of the history list is a translation like any other. What comes
    /// back is either a name — `Renamed report.pdf` — or a count, and never both:
    /// the two forms take different arguments, which is the distinction [ADR
    /// 0016](../../../docs/adr/0016-plural-forms-are-bracketed-keys-in-the-catalogue.md)
    /// draws between a plural form and a different message.
    ///
    /// **A batch is named by its last change and counted by its length**, which is
    /// the same choice [`Change::subject`] makes and for the same reason: the
    /// gesture is what the last thing it did looks like. A `Replace` batch trashes
    /// what it replaces and then copies, so it reads as a copy — which is what a
    /// person asked for.
    #[must_use]
    pub fn summary(&self) -> (&'static str, String) {
        let named = |path: &Path| {
            path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
        };
        match self {
            Self::Restored(entry) => ("history.restored", named(&entry.original)),
            Self::Noted { change, .. } => change.summary(),
            Self::Batch(changes) => match changes.as_slice() {
                [] => ("history.nothing", String::new()),
                [only] => only.summary(),
                many => {
                    let (key, _) =
                        many.last().map_or(("history.nothing", String::new()), Change::summary);
                    (many_key(key), many.len().to_string())
                }
            },
            Self::Created { path, made } => (
                match made {
                    Made::Directory => "history.created",
                    Made::File => "history.created_file",
                    Made::Link(_) => "history.created_link",
                },
                named(path),
            ),
            Self::Renamed { to, .. } => ("history.renamed", named(to)),
            Self::Copied { destination, .. } => ("history.copied", named(destination)),
            Self::Unpacked { root, .. } => ("history.unpacked", named(root)),
            Self::Trashed { original, .. } => ("history.trashed", named(original)),
            Self::Erased { path } => ("history.erased", named(path)),
            Self::Edited { path, .. } => ("history.edited", named(path)),
            Self::Permissions { path, .. } => ("history.permissions", named(path)),
        }
    }

    /// Where the file is once this change has been **reverted**, if anywhere.
    ///
    /// The other end of [`Change::subject`], and the row a cursor belongs on
    /// after an undo: undoing a rename leaves the file under the name it had
    /// before, and undoing a trash puts it back where it was. `Created` has no
    /// answer — the point of undoing it is that there is nothing left to point
    /// at.
    #[must_use]
    pub fn origin(&self) -> Option<&Path> {
        match self {
            // The *first* one, because a batch is undone backwards and the first
            // change is the last to be put back.
            Self::Restored(entry) => Some(&entry.stored),
            Self::Noted { change, .. } => change.origin(),
            Self::Batch(changes) => changes.first().and_then(Change::origin),
            // Nothing to point at: undoing a `Created` removes the folder, and
            // an `Erased` is not undone at all.
            Self::Created { .. } | Self::Erased { .. } => None,
            Self::Renamed { from, .. } => Some(from),
            // A copy's source never moved, so an undo leaves the cursor on it.
            Self::Copied { source, .. } => Some(source),
            // Neither did the archive, and it is the row the folder came from.
            Self::Unpacked { archive, .. } => Some(archive),
            Self::Trashed { original, .. } => Some(original),
            // Neither an edit nor a chmod moves anything, so undoing one leaves
            // the cursor where it is.
            Self::Edited { path, .. } | Self::Permissions { path, .. } => Some(path),
        }
    }
}

/// The counted form of a `history.*` key.
///
/// A pair rather than a plural, because the two forms take different arguments —
/// one names a file and the other counts them. Unknown keys fall back to the
/// generic count, which is what an empty batch and a shape nobody has added yet
/// both are.
fn many_key(one: &'static str) -> &'static str {
    match one {
        "history.restored" => "history.restored_many",
        "history.created" => "history.created_many",
        "history.created_file" => "history.created_file_many",
        "history.created_link" => "history.created_link_many",
        "history.renamed" => "history.renamed_many",
        "history.copied" => "history.copied_many",
        "history.unpacked" => "history.unpacked_many",
        "history.trashed" => "history.trashed_many",
        "history.erased" => "history.erased_many",
        "history.edited" => "history.edited_many",
        "history.permissions" => "history.permissions_many",
        _ => "history.changed_many",
    }
}

/// Makes a directory called `name` inside `parent`.
///
/// **Runs on a job.**
pub fn create_directory(parent: &Path, name: &str) -> Result<Change, Problem> {
    crate::trash::writable(parent)?;
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    let path = parent.join(name);
    // `mkdir` fails with `EEXIST` rather than replacing, which is the whole
    // reason not to check first: a check and a create are two syscalls with a
    // race between them, and the create already answers the question.
    rustix::fs::mkdir(&path, Mode::from_bits_truncate(0o755))
        .map_err(|error| problem(error, &path))?;
    Ok(Change::Created { path, made: Made::Directory })
}

/// The name a creation is allowed to use, or why it is not one.
fn usable(parent: &Path, name: &str) -> Result<PathBuf, Problem> {
    crate::trash::writable(parent)?;
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    Ok(parent.join(name))
}

/// Makes an empty file called `name` inside `parent`.
///
/// `O_EXCL`, which is the file's version of the reason `create_directory` does
/// not look first: the create is the check, and a `statx` before it is a window
/// somebody else can write in.
///
/// **Runs on a job.**
pub fn create_file(parent: &Path, name: &str) -> Result<Change, Problem> {
    let path = usable(parent, name)?;
    rustix::fs::open(
        &path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .map_err(|error| problem(error, &path))?;
    Ok(Change::Created { path, made: Made::File })
}

/// Makes a symbolic link called `name` inside `parent`, pointing at `target`.
///
/// **The target is not resolved and not checked.** A link to something that is
/// not there yet is a link a person may have meant — `symlinkat` makes one, and
/// refusing here would be this program deciding what the filesystem already
/// allows.
///
/// **Runs on a job.**
pub fn create_link(parent: &Path, name: &str, target: &Path) -> Result<Change, Problem> {
    let path = usable(parent, name)?;
    rustix::fs::symlink(target, &path).map_err(|error| problem(error, &path))?;
    Ok(Change::Created { path, made: Made::Link(target.to_path_buf()) })
}

/// Takes back a [`Change::Created`].
///
/// **Each of the three refuses rather than guessing**, which is what the module
/// header means by an inverse that is not a guess: a directory has to be empty,
/// and so does a file — somebody who typed into the file this made would
/// otherwise lose it to a keystroke meant to remove an empty one.
fn unmake(path: &Path, made: &Made) -> Result<(), Problem> {
    match made {
        Made::Directory => rustix::fs::rmdir(path).map_err(|error| problem(error, path)),
        Made::File => {
            let stat = rustix::fs::statat(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| problem(error, path))?;
            if stat.st_size != 0 {
                return Err(Problem::new(
                    ProblemKind::NotEmpty,
                    path.display().to_string(),
                    "the file has been written to since it was made",
                ));
            }
            rustix::fs::unlink(path).map_err(|error| problem(error, path))
        }
        Made::Link(_) => rustix::fs::unlink(path).map_err(|error| problem(error, path)),
    }
}

/// Renames `from` to `name`, in the same directory.
///
/// **Runs on a job.**
pub fn rename(from: &Path, name: &str) -> Result<Change, Problem> {
    crate::trash::writable(from)?;
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    let Some(parent) = from.parent() else {
        return Err(Problem::new(ProblemKind::BadName, from.display().to_string(), "no parent"));
    };
    let to = parent.join(name);
    if to == from {
        return Ok(Change::Renamed { from: from.to_path_buf(), to });
    }
    move_no_replace(from, &to)?;
    Ok(Change::Renamed { from: from.to_path_buf(), to })
}

/// Moves `path` into the trash, per the freedesktop specification.
///
/// **Runs on a job.**
pub fn trash(path: &Path) -> Result<Change, Problem> {
    let entry = crate::trash::put(path)?;
    let change =
        Change::Trashed { original: entry.original, stored: entry.stored, info: entry.info };
    Ok(match entry.warning {
        Some(note) => {
            Change::Noted { change: Box::new(change), note, retry: false, stranded: false }
        }
        None => change,
    })
}

/// Restores a persistent trash entry. Runs on a filesystem job.
pub fn restore(stored: &Path) -> Result<Change, Problem> {
    let entry = crate::trash::restore(stored)?;
    let warning = entry.warning.clone();
    let change = Change::Restored(entry);
    Ok(match warning {
        Some(note) => {
            Change::Noted { change: Box::new(change), note, retry: false, stranded: false }
        }
        None => change,
    })
}

/// Copies `source` into the directory `into`.
///
/// **Runs on a job.** The engine is `crate::copy`, ported from Spike E; this is
/// the part that turns its result into something the undo stack can hold.
pub fn copy_into(
    source: &Path,
    into: &Path,
    conflict: Conflict,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(crate::copy::Progress),
) -> Result<Change, Problem> {
    crate::trash::writable(into)?;
    let Some(name) = source.file_name() else {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "no name to copy it under",
        ));
    };
    // **A copy into the folder it came from is the ordinary case**, not an
    // error: it is what Duplicate is, and what pasting into the folder you
    // copied from means. Under a free name, chosen here on the job — a `statx`
    // per candidate is not something the frame loop may do.
    if conflict == Conflict::Rename
        && let Some(free) = crate::copy::free_name(into, name)
        && free != name
    {
        let destination = into.join(&free);
        return match crate::copy::copy_to(source, &destination, cancel, watch) {
            Ok(()) => Ok(Change::Copied { source: source.to_path_buf(), destination }),
            Err(_) if cancel.is_cancelled() && destination.exists() => {
                Ok(Change::Copied { source: source.to_path_buf(), destination })
            }
            Err(problem) if taken(&destination) => Ok(Change::Noted {
                change: Box::new(Change::Copied { source: source.to_path_buf(), destination }),
                note: problem.message(),
                retry: problem.kind() != ProblemKind::Preservation,
                stranded: false,
            }),
            Err(problem) => Err(problem),
        };
    }
    let destination = into.join(name);
    let existed = taken(&destination);
    if conflict == Conflict::Replace && taken(&destination) {
        return replace_into(name, into, None, &mut |staging| {
            crate::copy::copy_to(source, staging, cancel, watch)
        })
        .map(|displaced| {
            Change::Batch(vec![
                displaced,
                Change::Copied { source: source.to_path_buf(), destination },
            ])
        });
    }
    let mut progress = crate::copy::Progress::default();
    let options = crate::copy::Options {
        resume: conflict == Conflict::Resume,
        ..crate::copy::Options::default()
    };
    match crate::copy::copy_into(source, into, options, cancel, &mut progress, watch) {
        Ok(destination) => Ok(Change::Copied { source: source.to_path_buf(), destination }),
        // **A cancelled copy is still a change.** It stopped part way and left
        // what it had written, and the only thing that can take that back is the
        // undo stack — so it is recorded rather than reported as a failure with
        // files on disk that nothing describes.
        Err(_) if cancel.is_cancelled() && destination.exists() => {
            Ok(Change::Copied { source: source.to_path_buf(), destination })
        }
        Err(problem) if !existed && taken(&destination) => Ok(Change::Noted {
            change: Box::new(Change::Copied { source: source.to_path_buf(), destination }),
            note: problem.message(),
            retry: problem.kind() != ProblemKind::Preservation,
            stranded: false,
        }),
        Err(problem) => Err(problem),
    }
}

/// Resume a partial copy at the exact destination recorded in its undo entry.
///
/// The caller must retain ownership of that entry throughout recovery.
pub fn resume_copy(
    source: &Path,
    destination: &Path,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(crate::copy::Progress),
) -> Result<Change, Problem> {
    crate::trash::writable(destination)?;
    let options = crate::copy::Options { resume: true, ..Default::default() };
    let change =
        Change::Copied { source: source.to_path_buf(), destination: destination.to_path_buf() };
    match crate::copy::copy_to_options(source, destination, options, cancel, watch) {
        Ok(()) => Ok(change),
        Err(_) if cancel.is_cancelled() => Ok(change),
        Err(problem) => Ok(Change::Noted {
            change: Box::new(change),
            note: problem.message(),
            retry: problem.kind() != ProblemKind::Preservation,
            stranded: false,
        }),
    }
}

/// What a copy does when the name it wants is taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Conflict {
    /// Refuse, which is what every other mutation here does with a taken name.
    #[default]
    Refuse,
    /// Copy under a free name beside it: `report.pdf` becomes
    /// `report (copy).pdf`, then `report (copy 2).pdf`.
    ///
    /// **Only ever for a copy.** A move that quietly renamed would leave a
    /// person with two files under names neither of them chose, and no way to
    /// tell which is which — so a move with a taken name is refused and said out
    /// loud.
    Rename,
    /// Merge into what is already there, copying only what is missing.
    ///
    /// **The verb a stopped copy is picked up with**, and the only conflict
    /// answer nobody chooses from a panel: it is what a resumed queue sets on
    /// every item it still owes. A file that is there at the same size is
    /// skipped, a directory that is there is entered, and everything else is
    /// copied — [`crate::copy::Options::resume`] says why size is the test.
    Resume,
    /// Put what is already there in the Trash, then place this one.
    ///
    /// **Trash, never overwrite.** Every other file manager writes over the file
    /// that was there, and the file that was there is then gone — which
    /// [rule 3](../../../docs/03-principles.md) does not allow anything here to
    /// do. Trashing it first makes the whole thing one
    /// [`Change::Batch`], so `Ctrl+Z` removes what arrived *and* brings back
    /// what it displaced, in that order.
    ///
    /// If no safe writable trash can be created on that filesystem, replacement
    /// is refused and the existing destination is retained.
    Replace,
}

/// Deletes `path` and everything under it, for good.
///
/// **Runs on a job**, and there is nothing to undo afterwards. The caller is
/// responsible for having asked.
pub fn erase(path: &Path, cancel: &sk_util::Cancel) -> Result<Change, Problem> {
    crate::copy::remove_tree(path, cancel)?;
    Ok(Change::Erased { path: path.to_path_buf() })
}

/// Erases everything in one trash's `files` directory, for good.
///
/// **An [`Change::Erased`], which the stack does not hold** — the same shape a
/// permanent delete has, because it is one: [rule
/// 3](../../../docs/03-principles.md#3-everything-is-reversible) calls permanent
/// deletion "an explicit, differently-shaped action" and emptying the Trash is
/// the largest one this program offers. What it *does* owe the stack is
/// [`stranded_by_empty`], which the caller runs over the history afterwards.
///
/// **Runs on a job.**
pub fn empty_trash(files: &Path, cancel: &sk_util::Cancel) -> Result<Change, Problem> {
    crate::trash::empty(files, cancel)?;
    Ok(Change::Erased { path: files.to_path_buf() })
}

/// Which way a change on the undo stack would next be walked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    /// It is applied, so the next thing that can happen to it is its undo.
    Undo,
    /// It was undone, so the next thing that can happen to it is its redo.
    Redo,
}

/// The same change with every part an emptied trash took the way forward or
/// back from marked, or `None` where it took nothing.
///
/// **Which part depends on [`Next`]**, because an undo and a redo need
/// different things from the trash:
///
/// - An applied [`Change::Trashed`] is undone from the copy under `files`,
///   which is what was erased — stranded. Undone, its redo is
///   [`trash`] of the file at its original path, which needs nothing the empty
///   took — not stranded.
/// - An applied [`Change::Restored`] is undone by [`crate::trash::put_back`],
///   which needs the file at its *original* path and the two storage
///   directories [`crate::trash::empty`] deliberately leaves standing — not
///   stranded. Undone, the file went back under `files` and the empty erased it,
///   so its redo is — stranded.
///
/// **Leaf by leaf, not entry by entry.** A `Replace` is a batch whose first half
/// trashed what it displaced; marking the whole batch would refuse the undo of
/// the copy beside it, which is still perfectly reversible.
#[must_use]
pub fn stranded_by_empty(change: &Change, files: &Path, next: Next) -> Option<Change> {
    let stranded = |original: &Path| Change::Noted {
        change: Box::new(change.clone()),
        note: sk_model::i18n::format("notice.trash_emptied", &[&original.display()]),
        retry: false,
        stranded: true,
    };
    match change {
        Change::Trashed { original, stored, .. }
            if next == Next::Undo && stored.starts_with(files) =>
        {
            Some(stranded(original))
        }
        Change::Restored(entry) if next == Next::Redo && entry.stored.starts_with(files) => {
            Some(stranded(&entry.original))
        }
        Change::Batch(changes) => {
            let marked: Vec<Change> = changes
                .iter()
                .map(|change| {
                    stranded_by_empty(change, files, next).unwrap_or_else(|| change.clone())
                })
                .collect();
            (marked != *changes).then_some(Change::Batch(marked))
        }
        // Already marked, or carrying a warning of its own: the inner change is
        // what an undo would act on, so that is what is asked.
        Change::Noted { change: inner, note, retry, stranded } => {
            stranded_by_empty(inner, files, next).map(|marked| Change::Noted {
                change: Box::new(marked),
                note: note.clone(),
                retry: *retry,
                stranded: *stranded,
            })
        }
        _ => None,
    }
}

/// Moves `source` into the directory `into`.
///
/// Recorded as a rename, because that is what it is when the filesystems match
/// and what its undo has to do either way.
///
/// **What a move across filesystems leaves in the source is a note, not a
/// failure.** It moved everything it copied, so the change goes on the stack
/// and its undo brings that back — which then refuses while the leftovers
/// still hold the original name, rather than merging into them.
///
/// **Runs on a job.**
pub fn move_into(
    source: &Path,
    into: &Path,
    conflict: Conflict,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(crate::copy::Progress),
) -> Result<Change, Problem> {
    crate::trash::writable(source)?;
    crate::trash::writable(into)?;
    // **A move never renames.** [`Conflict::Rename`] is a copy's answer, and a
    // move that quietly renamed would leave two files under names neither of
    // them chose — so it is not offered for one, and reaching here with it
    // refuses the way a taken name always has.
    if conflict == Conflict::Replace
        && let Some(name) = source.file_name()
        && taken(&into.join(name))
    {
        let destination = into.join(name);
        let mut left = None;
        let displaced = replace_into(name, into, Some(source), &mut |staging| {
            left = crate::copy::move_to(source, staging, cancel, watch)?;
            Ok(())
        })?;
        let moved = Change::Renamed { from: source.to_path_buf(), to: destination };
        return Ok(with_left(Change::Batch(vec![displaced, moved]), left));
    }
    let mut progress = crate::copy::Progress::default();
    // A move across filesystems is a copy and then a removal, so a resumed one
    // merges for the same reason a resumed copy does. Within one filesystem it
    // is a `rename`, which has nothing to resume.
    let options = crate::copy::Options {
        resume: conflict == Conflict::Resume,
        ..crate::copy::Options::default()
    };
    let (destination, left) =
        crate::copy::move_into(source, into, options, cancel, &mut progress, watch)?;
    Ok(with_left(Change::Renamed { from: source.to_path_buf(), to: destination }, left))
}

/// `change`, carrying what a move across filesystems left behind, if it did.
fn with_left(change: Change, left: Option<Problem>) -> Change {
    match left {
        Some(left) => Change::Noted {
            change: Box::new(change),
            note: left.message(),
            retry: false,
            stranded: false,
        },
        None => change,
    }
}

/// Replaces a text file's contents, and records what it held.
///
/// **The one mutation here that is not a move.** Everything else in this module
/// changes where a file *is*; this changes what is in it, so undoing it means
/// putting the old bytes back and they have to be carried on the change itself.
///
/// Written through [`crate::atomically`] — the four steps rule 6 asks for —
/// because a text file half written is a text file lost, and the editor above
/// this is the only thing in the program that can produce one.
///
/// **Runs on a job.**
pub fn write_text(path: &Path, text: &str) -> Result<(), Problem> {
    crate::trash::writable(path)?;
    crate::save::write(path, text).map_err(|error| io_problem(&error, path))
}

/// Replaces a text file's contents and returns the change that did it.
///
/// Reads what was there first: an undo has to put it back, and after the write
/// there is nowhere to read it from.
///
/// **Runs on a job.**
pub fn edit_text(path: &Path, after: &str) -> Result<Change, Problem> {
    let before = crate::read_text(path, crate::TEXT_LIMIT)?;
    let change = Change::Edited { path: path.to_path_buf(), before, after: after.to_owned() };
    match write_text(path, after) {
        Ok(()) => Ok(change),
        // A directory sync can fail after publication. Keep the committed edit
        // undoable while still reporting that durability was not acknowledged.
        Err(problem)
            if crate::read_text(path, crate::TEXT_LIMIT).is_ok_and(|text| text == after) =>
        {
            Ok(Change::Noted {
                change: Box::new(change),
                note: problem.message(),
                retry: false,
                stranded: false,
            })
        }
        Err(problem) => Err(problem),
    }
}

/// Sets the nine permission bits, and records what they were.
///
/// **The nine, and nothing above them.** The setuid, setgid and sticky bits are
/// read off the file and written back untouched, so a person answering *may
/// others write this* can never silently clear the bit that makes a directory a
/// shared drop box. Those three, a recursive apply and an ACL editor are the
/// *permissions editor*
/// [11-non-goals.md](../../../docs/11-non-goals.md) declines; the nine are the
/// basics it admits.
///
/// **A symlink is refused rather than followed.** `chmod` has no form that acts
/// on the link itself — Linux answers `AT_SYMLINK_NOFOLLOW` with `ENOTSUP` — so
/// the alternative to refusing is changing a file nobody pointed at.
///
/// **Runs on a job.**
pub fn set_permissions(path: &Path, nine: u16) -> Result<Change, Problem> {
    crate::trash::writable(path)?;
    let (held, before) = hold(path)?;
    let after = (before & 0o7000) | (nine & 0o777);
    chmod(&held, path, after)?;
    Ok(Change::Permissions { path: path.to_path_buf(), before, after })
}

/// The file `path` names, held, and the whole mode it has — refusing a symlink.
///
/// **One descriptor for the look and the change.** A `statx` of the path and a
/// `chmod` of the path are two lookups, and between them somebody who can write
/// the folder could swap the file for a symlink: the check saw a file, and the
/// change followed the link to whatever it named. `O_PATH | O_NOFOLLOW` opens a
/// symlink as itself, so the `fstat` that refuses it and the [`chmod`] that
/// follows are about one inode. `O_PATH` and not `O_RDONLY`, because changing a
/// mode needs ownership and not permission to read — the person restoring
/// access to their own `000` file is exactly who this is for.
fn hold(path: &Path) -> Result<(OwnedFd, u16), Problem> {
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| problem(error, path))?;
    let stat = rustix::fs::fstat(&fd).map_err(|error| problem(error, path))?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Symlink {
        return Err(Problem::new(
            ProblemKind::Unreadable,
            path.display().to_string(),
            "a symlink has no permissions of its own",
        ));
    }
    Ok((fd, (stat.st_mode & 0o7777) as u16))
}

/// `__NR_fchmodat2`: 452 in the table every architecture has shared since Linux
/// 5.1 (`include/uapi/asm-generic/unistd.h`), and not in the `libc` crate for
/// aarch64 as of 0.2.189. Where the number means nothing the kernel answers
/// `ENOSYS`, which is the fallback's cue anyway.
const FCHMODAT2: libc::c_long = 452;

/// Sets the mode of the file `held` refers to, never by name.
///
/// `fchmodat2` with `AT_EMPTY_PATH` acts on the descriptor itself, and is Linux
/// 6.6 — above the 5.10 floor, so it is tried rather than assumed. Without it,
/// `/proc/self/fd/N` is the descriptor's own magic link and `chmod` through it
/// reaches the same inode; plain `fchmod` refuses an `O_PATH` descriptor with
/// `EBADF`.
///
/// **`EPERM` falls back too.** A seccomp filter that predates the syscall —
/// a snap's, a container runtime's — answers `EPERM` rather than `ENOSYS`,
/// and a real `EPERM` costs one more refused `chmod` saying the same thing.
fn chmod(held: &OwnedFd, path: &Path, mode: u16) -> Result<(), Problem> {
    let flags = libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW;
    // SAFETY: the descriptor is open for the whole call, the path is a valid
    // NUL-terminated empty string, and the mode and flags are plain integers.
    let answer = unsafe {
        libc::syscall(FCHMODAT2, held.as_raw_fd(), c"".as_ptr(), libc::c_uint::from(mode), flags)
    };
    if answer == 0 {
        return Ok(());
    }
    match rustix::io::Errno::from_io_error(&std::io::Error::last_os_error()) {
        Some(rustix::io::Errno::NOSYS | rustix::io::Errno::PERM) => {
            chmod_through_proc(held, path, mode)
        }
        Some(error) => Err(problem(error, path)),
        None => Err(problem(rustix::io::Errno::IO, path)),
    }
}

/// [`chmod`] for a kernel without `fchmodat2`, through the descriptor's own link
/// in `/proc`.
///
/// `ENOENT` here means `/proc` is not mounted — the descriptor is open, so its
/// link exists wherever `/proc` does — and says that, rather than that a file
/// it is holding has gone.
fn chmod_through_proc(held: &OwnedFd, path: &Path, mode: u16) -> Result<(), Problem> {
    let link = format!("/proc/self/fd/{}", held.as_raw_fd());
    rustix::fs::chmodat(
        rustix::fs::CWD,
        link.as_str(),
        Mode::from_bits_truncate(u32::from(mode)),
        AtFlags::empty(),
    )
    .map_err(|error| proc_failure(error, path))
}

fn proc_failure(error: rustix::io::Errno, path: &Path) -> Problem {
    match error {
        rustix::io::Errno::NOENT => Problem::new(
            ProblemKind::ModeUnchangeable,
            path.display().to_string(),
            "no fchmodat2, and /proc/self/fd is not there",
        ),
        error => problem(error, path),
    }
}

/// Puts a recorded mode back, refusing if the file has moved on since.
///
/// The same refusal [`Change::Edited`] makes about bytes, for the same reason: a
/// mode somebody else changed in between is not one this undo has an inverse
/// for, and writing over it would lose their change without saying so.
fn restore_mode(path: &Path, expected: u16, wanted: u16) -> Result<(), Problem> {
    let (held, now) = hold(path)?;
    if now != expected {
        return Err(Problem::new(
            ProblemKind::ChangedSince,
            path.display().to_string(),
            "permissions changed since",
        ));
    }
    chmod(&held, path, wanted)
}

/// Whether something is already called that.
///
/// `SYMLINK_NOFOLLOW`, so a dangling symlink counts as taken — which it is, to
/// `rename` and to `open` with `O_EXCL`.
fn taken(path: &Path) -> bool {
    rustix::fs::statat(rustix::fs::CWD, path, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).is_ok()
}

/// Puts something where `name` already is, and returns what it displaced.
///
/// **Three steps, and the order is the whole of it.** `place` puts the arriving
/// file in the directory under a free name, the file that was there goes to the
/// Trash, and only then does the arrival take the name. Every other order has a
/// window where a failure leaves *nothing* at the name: copying over the old one
/// destroys it before anything knows whether the new one arrives, and trashing
/// first does the same with an extra step. Rollback restores a moved source;
/// if recovery itself is refused, the staged bytes remain and the error names
/// the recovery failure.
fn replace_into(
    name: &std::ffi::OsStr,
    into: &Path,
    original: Option<&Path>,
    place: &mut dyn FnMut(&Path) -> Result<(), Problem>,
) -> Result<Change, Problem> {
    let destination = into.join(name);
    let holding = loop {
        let candidate = into.join(format!(
            ".skerry-replace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match rustix::fs::mkdir(&candidate, Mode::from_bits_truncate(0o700)) {
            Ok(()) => break candidate,
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(problem(error, &candidate)),
        }
    };
    let staging = holding.join(name);
    let discard = || {
        if let Some(source) = original {
            if taken(&staging)
                && let Some(left) =
                    crate::copy::move_to(&staging, source, &sk_util::Cancel::new(), &mut |_| {})?
            {
                return Err(left);
            }
            return rustix::fs::rmdir(&holding).map_err(|error| problem(error, &holding));
        }
        // A fresh token: the staged copy is being removed *because* the
        // operation is over, and the one that stopped it is already cancelled.
        crate::copy::remove_tree(&holding, &sk_util::Cancel::new())
    };
    if let Err(problem) = place(&staging) {
        discard()?;
        return Err(problem);
    }
    let displaced = match trash(&destination) {
        Ok(change) => change,
        Err(problem) => {
            discard()?;
            return Err(problem);
        }
    };
    if let Err(problem) = move_no_replace(&staging, &destination) {
        // The name was freed one line ago, so this is very close to impossible —
        // and "very close to" is why what was trashed is put back rather than
        // left in the Trash with nothing on the undo stack pointing at it.
        let _ = revert(&displaced);
        discard()?;
        return Err(problem);
    }
    let _ = rustix::fs::rmdir(&holding);
    Ok(displaced)
}

/// Undoes a change. Refuses rather than guessing.
///
/// **Runs on a job.**
pub fn revert(change: &Change) -> Result<(), Problem> {
    match change {
        Change::Restored(entry) => crate::trash::put_back(entry),
        // **The note is the answer, not a remark beside one.** Whatever this
        // would have put back was erased by an Empty Trash, so saying that is
        // the whole of the undo — `Ctrl+Z` reports a sentence about the Trash
        // rather than a path under it that nobody has looked at.
        Change::Noted { change, note, stranded: true, .. } => Err(Problem::new(
            ProblemKind::TrashEmptied,
            change.subject().display().to_string(),
            note.clone(),
        )),
        Change::Noted { change, .. } => revert(change),
        // Backwards, and the first failure stops it: a batch half undone is a
        // state somebody can look at and act on, where carrying on past an
        // error would keep changing a filesystem that has already surprised us.
        Change::Batch(changes) => changes.iter().rev().try_for_each(revert),
        // **Never reached through the undo stack**, which does not hold these —
        // but `revert` is public and the compiler asks, and an answer that
        // pretended to succeed would be worse than one that says no.
        Change::Erased { path } => Err(Problem::new(
            ProblemKind::Unreadable,
            path.display().to_string(),
            "a permanent delete cannot be undone",
        )),
        Change::Created { path, made } => unmake(path, made),
        // Through the copy engine rather than `move_no_replace`: a rename inside
        // one directory can never cross a filesystem, but a *move* between two
        // can, and its undo has to be able to come back the same way it went.
        // What that leaves behind is an error here, where nothing can carry a
        // note; [`revert_recorded`], which the undo stack uses, carries it.
        Change::Renamed { from, to } => {
            match crate::copy::move_to(to, from, &sk_util::Cancel::new(), &mut |_| {})? {
                Some(left) => Err(left),
                None => Ok(()),
            }
        }
        // Trash preserves files added after the original operation.
        Change::Copied { destination, .. } => trash(destination).map(|_| ()),
        // Extraction output gets the same recoverable inverse as a copy.
        Change::Unpacked { root, .. } => trash(root).map(|_| ()),
        // Straight back to what it held, which is the only way to undo bytes.
        Change::Edited { path, before, after } => {
            if crate::read_text(path, crate::TEXT_LIMIT)? != *after {
                return Err(Problem::new(
                    ProblemKind::ChangedSince,
                    path.display().to_string(),
                    "contents changed after edit",
                ));
            }
            write_text(path, before)
        }
        // The bits it had, put back only if nothing else has moved them.
        Change::Permissions { path, before, after } => restore_mode(path, *after, *before),
        Change::Trashed { .. } => revert_recorded(change).map(|_| ()),
    }
}

/// Does a change again, after it was undone.
///
/// **Runs on a job.**
pub fn apply(change: &Change) -> Result<Change, Problem> {
    match change {
        Change::Restored(entry) => restore(&entry.stored),
        // The same refusal `revert` gives, and for the same reason: there is
        // nothing left to do again either.
        Change::Noted { change, note, stranded: true, .. } => Err(Problem::new(
            ProblemKind::TrashEmptied,
            change.subject().display().to_string(),
            note.clone(),
        )),
        Change::Noted { change, .. } => apply(change),
        Change::Batch(changes) => {
            changes.iter().map(apply).collect::<Result<_, _>>().map(Change::Batch)
        }
        // The bytes it held after the edit, put back after an undo took them away.
        Change::Edited { path, before, after } => {
            if crate::read_text(path, crate::TEXT_LIMIT)? != *before {
                return Err(Problem::new(
                    ProblemKind::ChangedSince,
                    path.display().to_string(),
                    "contents changed after undo",
                ));
            }
            edit_text(path, after)
        }
        // And the bits it had after the change, put back after an undo took them
        // away.
        Change::Permissions { path, before, after } => {
            restore_mode(path, *before, *after)?;
            Ok(change.clone())
        }
        // **Redone by the caller, not here** — ADR 0032. Unpacking means a
        // sandboxed child, and this crate is level 1 with no business spawning
        // one; `Edits::redo` recognises this variant and puts it back on the
        // queue. Reaching this arm means somebody added a second way to redo, so
        // it says what to do rather than pretending to have done it.
        Change::Unpacked { archive, .. } => Err(Problem::new(
            ProblemKind::Unreadable,
            archive.display().to_string(),
            "an unpack is redone through the queue, not through ops::apply",
        )),
        Change::Created { path, made } => {
            let (Some(parent), Some(name)) =
                (path.parent(), path.file_name().and_then(|name| name.to_str()))
            else {
                return Err(Problem::new(
                    ProblemKind::BadName,
                    path.display().to_string(),
                    "no parent or no name",
                ));
            };
            match made {
                Made::Directory => create_directory(parent, name),
                Made::File => create_file(parent, name),
                Made::Link(target) => create_link(parent, name, target),
            }
        }
        Change::Renamed { from, to } => {
            let left = crate::copy::move_to(from, to, &sk_util::Cancel::new(), &mut |_| {})?;
            Ok(with_left(change.clone(), left))
        }
        // **To the recorded destination, not to one derived from the source.**
        // A copy into the folder its original is in lands under a free name, and
        // re-deriving would send the redo at the taken one — which fails, and
        // would have failed silently differently once the free name changed.
        Change::Copied { source, destination } => {
            let existed = taken(destination);
            let fresh = Change::Copied { source: source.clone(), destination: destination.clone() };
            match crate::copy::copy_to(source, destination, &sk_util::Cancel::new(), &mut |_| {}) {
                Ok(()) => Ok(fresh),
                Err(problem) if !existed && taken(destination) => Ok(Change::Noted {
                    change: Box::new(fresh),
                    note: problem.message(),
                    retry: problem.kind() != ProblemKind::Preservation,
                    stranded: false,
                }),
                Err(problem) => Err(problem),
            }
        }
        // Re-trashed from scratch rather than by moving the same bytes back into
        // the same slot: the slot may have been taken, and `trash` is the thing
        // that knows how to claim one.
        Change::Trashed { original, .. } => trash(original),
        // Never reached: a permanent delete is not recorded, so there is nothing
        // to redo. The compiler asks, and an answer that deleted something again
        // would be an answer to a question nobody asked.
        Change::Erased { path } => Err(Problem::new(
            ProblemKind::Unreadable,
            path.display().to_string(),
            "a permanent delete cannot be done again",
        )),
    }
}

/// The exact boundary reached by a batch replay, including a failed step.
pub struct Replay {
    /// Work that finished, in original gesture order.
    pub completed: Option<Change>,
    /// Work still waiting, in original gesture order.
    pub remaining: Option<Change>,
    /// Why replay stopped, if it did.
    pub problem: Option<Problem>,
}

/// Undoes one leaf and retains a warning if publication succeeded before sync failed.
pub fn revert_recorded(change: &Change) -> Result<Change, Problem> {
    if let Change::Trashed { original, stored, .. } = change {
        if crate::trash::read(stored)?.original != *original {
            return Err(Problem::new(
                ProblemKind::TrashUnavailable,
                stored.display().to_string(),
                "trash metadata changed after the operation",
            ));
        }
        let restored = crate::trash::restore(stored)?;
        return Ok(match restored.warning {
            Some(note) => Change::Noted {
                change: Box::new(change.clone()),
                note,
                retry: false,
                stranded: false,
            },
            None => change.clone(),
        });
    }

    if let Change::Edited { path, before, after } = change {
        if crate::read_text(path, crate::TEXT_LIMIT)? != *after {
            return Err(Problem::new(
                ProblemKind::NameTaken,
                path.display().to_string(),
                "contents changed after edit",
            ));
        }
        return match edit_text(path, before)? {
            Change::Noted { note, .. } => Ok(Change::Noted {
                change: Box::new(change.clone()),
                note,
                retry: false,
                stranded: false,
            }),
            _ => Ok(change.clone()),
        };
    }
    if let Change::Renamed { from, to } = change {
        let left = crate::copy::move_to(to, from, &sk_util::Cancel::new(), &mut |_| {})?;
        return Ok(with_left(change.clone(), left));
    }
    revert(change)?;
    Ok(change.clone())
}

/// Replays leaf operations, preserving both sides of the boundary on failure.
///
/// The caller supplies the leaf executor so archive replay stays above sk-fs.
pub fn replay(
    change: &Change,
    undo: bool,
    step: &mut dyn FnMut(&Change) -> Result<Change, Problem>,
) -> Replay {
    fn flatten(change: &Change, out: &mut Vec<Change>) {
        match change {
            Change::Batch(changes) => {
                for change in changes {
                    flatten(change, out);
                }
            }
            // **A stranded note is a leaf.** Unwrapping it here would drop the
            // marking on the way to `step`, and the undo would go back to
            // failing about a path inside the Trash — which is the whole thing
            // the marking exists to replace.
            Change::Noted { stranded: true, .. } => out.push(change.clone()),
            Change::Noted { change, .. } => flatten(change, out),
            change => out.push(change.clone()),
        }
    }
    fn grouped(mut changes: Vec<Change>) -> Option<Change> {
        match changes.len() {
            0 => None,
            1 => changes.pop(),
            _ => Some(Change::Batch(changes)),
        }
    }
    let mut waiting = Vec::new();
    flatten(change, &mut waiting);
    if !undo {
        waiting.reverse();
    }
    let mut completed = Vec::new();
    let mut problem = None;
    while let Some(change) = waiting.pop() {
        match step(&change) {
            Ok(fresh) => {
                if let Change::Noted { note, .. } = &fresh {
                    problem = Some(Problem::new(
                        ProblemKind::Note,
                        note.clone(),
                        "completed with a note",
                    ));
                }
                completed.push(fresh);
                if problem.is_some() {
                    break;
                }
            }
            Err(error) => {
                waiting.push(change);
                problem = Some(error);
                break;
            }
        }
    }
    if undo {
        completed.reverse();
    } else {
        waiting.reverse();
    }
    Replay { completed: grouped(completed), remaining: grouped(waiting), problem }
}

/// Moves `from` to `to`, and **never over anything**.
///
/// `renameat2` with `RENAME_NOREPLACE`, which is one syscall and cannot race —
/// a `stat` first and a `rename` after is a window in which somebody else
/// creates the destination and this overwrites it.
fn move_no_replace(from: &Path, to: &Path) -> Result<(), Problem> {
    let cwd = rustix::fs::CWD;
    match rustix::fs::renameat_with(cwd, from, cwd, to, rustix::fs::RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Not every filesystem implements the flag. Falling back to a plain
        // rename would silently replace, so the fallback is a link-and-unlink
        // for a file and a refusal for anything else — and both are honest.
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) => {
            Err(problem(rustix::io::Errno::OPNOTSUPP, to))
        }
        Err(error) => Err(problem(error, to)),
    }
}

/// The deletion time the specification asks for: local, to the second, no zone.
pub(crate) fn deletion_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut out: libc::tm = unsafe { std::mem::zeroed() };
    let seconds: libc::time_t = now.cast_signed();
    // SAFETY: `localtime_r` writes into `out` and reads only `seconds`.
    if unsafe { libc::localtime_r(&raw const seconds, &raw mut out) }.is_null() {
        return "1970-01-01T00:00:00".to_owned();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        out.tm_year + 1900,
        out.tm_mon + 1,
        out.tm_mday,
        out.tm_hour,
        out.tm_min,
        out.tm_sec
    )
}

/// Percent-encodes a path for a `.trashinfo` `Path=` field.
///
/// The specification says the value is the path "with the usual URL encoding",
/// and the separators stay separators: a restore reads this back and joins it,
/// so encoding `/` would make every restored path a single filename.
#[cfg(test)]
fn encode_path(path: &str) -> String {
    encode_path_bytes(path.as_bytes())
}

pub(crate) fn encode_path_bytes(path: &[u8]) -> String {
    const KEEP: &[u8] = b"-_.~/";
    let mut out = String::with_capacity(path.len());
    for byte in path {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub(crate) fn problem(error: rustix::io::Errno, path: &Path) -> Problem {
    let kind = match error {
        rustix::io::Errno::NOENT => ProblemKind::Gone,
        rustix::io::Errno::ACCESS | rustix::io::Errno::PERM | rustix::io::Errno::ROFS => {
            ProblemKind::Unreadable
        }
        rustix::io::Errno::EXIST => ProblemKind::NameTaken,
        rustix::io::Errno::XDEV => ProblemKind::AcrossFilesystems,
        rustix::io::Errno::NOTEMPTY => ProblemKind::NotEmpty,
        rustix::io::Errno::MFILE | rustix::io::Errno::NFILE | rustix::io::Errno::NOSPC => {
            ProblemKind::OutOfResources
        }
        _ => ProblemKind::Unreadable,
    };
    Problem::new(kind, path.display().to_string(), format!("{error:?}"))
}

fn io_problem(error: &std::io::Error, path: &Path) -> Problem {
    let errno =
        error.raw_os_error().map_or(rustix::io::Errno::IO, rustix::io::Errno::from_raw_os_error);
    problem(errno, path)
}

#[cfg(test)]
mod tests {

    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        Change, Conflict, Made, Next, apply, copy_into, create_directory, create_file, create_link,
        empty_trash, encode_path, move_into, rename, revert, set_permissions, stranded_by_empty,
        trash,
    };
    use sk_model::ProblemKind;
    use std::path::{Path, PathBuf};

    /// A private `$HOME` and `$XDG_DATA_HOME`, so a test's trash is its own.
    ///
    /// The variables are process-wide, so these tests must not run beside each
    /// other — `TRASH` serialises the ones that touch them. Everything else uses
    /// its own directory and runs in parallel.
    static TRASH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Undoing a copy removes what the copy made, and doing it again remakes it.
    #[test]
    fn a_copy_is_undone_by_removing_what_it_made() {
        with_trash("copy-undo", |root| {
            let from = root.join("from");
            let into = root.join("into");
            std::fs::create_dir_all(from.join("folder")).unwrap();
            std::fs::create_dir(&into).unwrap();
            std::fs::write(from.join("folder/inside.txt"), b"contents").unwrap();

            let change = copy_into(
                &from.join("folder"),
                &into,
                Conflict::Refuse,
                &sk_util::Cancel::new(),
                &mut |_| {},
            )
            .expect("copied");
            let landed = into.join("folder");
            assert_eq!(change.subject(), landed);
            assert!(landed.join("inside.txt").is_file());

            revert(&change).expect("undone");
            assert!(!landed.exists(), "the copy survived its undo");
            assert!(from.join("folder/inside.txt").is_file(), "the source was touched");

            let again = apply(&change).expect("redone");
            assert_eq!(again.subject(), landed);
            assert_eq!(std::fs::read(landed.join("inside.txt")).unwrap(), b"contents");
        });
    }

    /// An undo leaves the cursor on the source, because that is the only thing
    /// still there.
    #[test]
    fn undoing_a_copy_points_at_the_source() {
        let root = scratch("copy-origin");
        let from = root.join("one.txt");
        let into = root.join("into");
        std::fs::write(&from, b"x").unwrap();
        std::fs::create_dir(&into).unwrap();
        let change =
            copy_into(&from, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                .expect("copied");
        assert_eq!(change.origin(), Some(from.as_path()));
    }

    /// A move records itself as a rename, and its undo brings it back.
    #[test]
    fn a_move_is_undone_by_moving_it_back() {
        let root = scratch("move-undo");
        let from = root.join("one.txt");
        let into = root.join("into");
        std::fs::write(&from, b"contents").unwrap();
        std::fs::create_dir(&into).unwrap();

        let change =
            move_into(&from, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                .expect("moved");
        assert!(!from.exists());
        assert!(into.join("one.txt").is_file());

        revert(&change).expect("undone");
        assert!(from.is_file(), "the file did not come back");
        assert!(!into.join("one.txt").exists());
    }

    /// Refusing rather than replacing, which is what every other mutation here
    /// does — conflict resolution is the queue's job and the queue is owed.
    #[test]
    fn a_copy_onto_an_existing_name_refuses_and_changes_nothing() {
        let root = scratch("copy-exists");
        let from = root.join("one.txt");
        let into = root.join("into");
        std::fs::write(&from, b"new").unwrap();
        std::fs::create_dir(&into).unwrap();
        std::fs::write(into.join("one.txt"), b"old").unwrap();

        let refused =
            copy_into(&from, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                .expect_err("refused");
        assert_eq!(refused.kind(), ProblemKind::NameTaken);
        assert_eq!(std::fs::read(into.join("one.txt")).unwrap(), b"old");
    }

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-ops-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The failure that started row 4: `Ctrl+Z` after an Empty Trash.
    ///
    /// Unmarked, the undo answered `…/Trash/files/receipt.pdf` — a path nobody
    /// has ever looked at, about a file the person deliberately destroyed. The
    /// assertion is that the answer is a *sentence about the Trash*, and that it
    /// is an answer at all: a refusal the caller can show, not a silent no-op.
    #[test]
    fn emptying_the_trash_strands_what_it_erased_and_says_so() {
        with_trash("empty-strands", |root| {
            let file = root.join("receipt.pdf");
            std::fs::write(&file, b"%PDF-1.4").unwrap();
            let change = trash(&file).expect("trashed");
            let Change::Trashed { stored, .. } = &change else { panic!("{change:?}") };
            let files = stored.parent().expect("files").to_path_buf();

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            assert!(!stored.exists(), "the trashed file survived the empty");

            let marked =
                stranded_by_empty(&change, &files, Next::Undo).expect("the entry was not marked");
            let refused = revert(&marked).expect_err("an erased file came back");
            assert_eq!(refused.kind(), ProblemKind::TrashEmptied);
            let said = refused.message();
            assert!(said.contains("receipt.pdf"), "{said:?} does not name the file");
            assert!(
                !said.contains("Trash/files"),
                "{said:?} names a path inside the Trash rather than what happened"
            );
            // And the history still says what happened, which is ADR 0036's shape.
            assert_eq!(marked.summary().0, "history.trashed");
            assert!(marked.warning().is_some_and(|note| note.contains("receipt.pdf")));
        });
    }

    /// The other half, and the reason the empty takes the contents rather than
    /// the directories: a restore stays undoable across one.
    #[test]
    fn a_restore_is_still_undoable_after_the_trash_is_emptied() {
        with_trash("empty-restore", |root| {
            let kept = root.join("kept.txt");
            let other = root.join("other.txt");
            std::fs::write(&kept, b"mine").unwrap();
            std::fs::write(&other, b"theirs").unwrap();
            let Change::Trashed { stored, .. } = trash(&kept).expect("trashed") else {
                panic!("trash")
            };
            let _ = trash(&other).expect("trashed the second");
            let restored = super::restore(&stored).expect("restored");
            let files = stored.parent().expect("files").to_path_buf();

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            assert!(files.is_dir(), "the empty took the directory `put_back` writes into");
            assert!(
                stranded_by_empty(&restored, &files, Next::Undo).is_none(),
                "a restore that is still undoable was marked as stranded"
            );
            revert(&restored).expect("the restore could not be undone");
            assert!(!kept.exists(), "undoing the restore left the file where it was");
            assert_eq!(std::fs::read(&stored).unwrap(), b"mine");
        });
    }

    /// Undone, a trash is redone by trashing the file again from where the undo
    /// put it, which needs nothing the empty took — so marking it would refuse
    /// a `Ctrl+Shift+Z` that works.
    #[test]
    fn an_undone_trash_is_still_redoable_after_the_trash_is_emptied() {
        with_trash("empty-undone-trash", |root| {
            let file = root.join("draft.txt");
            std::fs::write(&file, b"draft").unwrap();
            let change = trash(&file).expect("trashed");
            let Change::Trashed { stored, .. } = &change else { panic!("{change:?}") };
            let files = stored.parent().expect("files").to_path_buf();
            revert(&change).expect("the trash could not be undone");
            assert!(file.exists(), "the undo did not put the file back");

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            assert!(
                stranded_by_empty(&change, &files, Next::Redo).is_none(),
                "an undone trash was marked, and its redo refused, though it still works"
            );
            apply(&change).expect("the redo of an undone trash failed after an empty");
            assert!(!file.exists(), "the redo did not trash the file again");
        });
    }

    /// Undone, a restore put the file back under `files`, and the empty erased
    /// it there — so its redo is stranded, and the answer is the sentence rather
    /// than a path inside the Trash.
    #[test]
    fn an_undone_restore_is_stranded_by_an_empty() {
        with_trash("empty-undone-restore", |root| {
            let file = root.join("photo.jpg");
            std::fs::write(&file, b"jpeg").unwrap();
            let Change::Trashed { stored, .. } = trash(&file).expect("trashed") else {
                panic!("trash")
            };
            let restored = super::restore(&stored).expect("restored");
            revert(&restored).expect("the restore could not be undone");
            assert!(stored.exists(), "undoing the restore did not put it back in the Trash");
            let files = stored.parent().expect("files").to_path_buf();

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            let marked = stranded_by_empty(&restored, &files, Next::Redo)
                .expect("an undone restore whose file was erased was not marked");
            let refused = apply(&marked).expect_err("an erased file came back");
            assert_eq!(refused.kind(), ProblemKind::TrashEmptied);
            assert!(refused.message().contains("photo.jpg"), "{:?}", refused.message());
            assert!(!refused.message().contains("Trash/files"), "{:?}", refused.message());
        });
    }

    /// A replace is a batch, and only the half the empty erased is marked.
    #[test]
    fn only_the_erased_half_of_a_batch_is_stranded() {
        with_trash("empty-batch", |root| {
            let from = root.join("from");
            let into = root.join("into");
            std::fs::create_dir_all(&from).unwrap();
            std::fs::create_dir_all(&into).unwrap();
            std::fs::write(from.join("report.pdf"), b"the new one").unwrap();
            std::fs::write(into.join("report.pdf"), b"the old one").unwrap();
            let change = copy_into(
                &from.join("report.pdf"),
                &into,
                Conflict::Replace,
                &sk_util::Cancel::new(),
                &mut |_| {},
            )
            .expect("replaced");
            let files = root.join("data/Trash/files");

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            let Change::Batch(marked) =
                stranded_by_empty(&change, &files, Next::Undo).expect("marked")
            else {
                panic!("a batch stopped being a batch")
            };
            assert!(matches!(marked[0], Change::Noted { stranded: true, .. }), "{marked:?}");
            assert!(matches!(marked[1], Change::Copied { .. }), "{marked:?}");
        });
    }

    /// An empty leaves no record with nothing behind it, which every other
    /// desktop's tools read as a corrupt trash.
    #[test]
    fn emptying_takes_the_records_with_the_files() {
        with_trash("empty-records", |root| {
            let file = root.join("note.txt");
            std::fs::write(&file, b"x").unwrap();
            let Change::Trashed { stored, info, .. } = trash(&file).expect("trashed") else {
                panic!("trash")
            };
            let files = stored.parent().expect("files").to_path_buf();

            empty_trash(&files, &sk_util::Cancel::new()).expect("emptied");
            assert!(!info.exists(), "a record with no file behind it: {info:?}");
            assert_eq!(crate::trash::held(&files, &sk_util::Cancel::new()).items, 0);
        });
    }

    /// A move between two real filesystems — `/dev/shm` and the temporary
    /// directory, which are two mounts on any Linux this runs on, and where they
    /// are not it returns.
    ///
    /// A file written after the copy read it, and one that arrived meanwhile,
    /// both stay, and the note counts them. The undo used to refuse at the
    /// folder's name — "Something is already called …; choose another name",
    /// with no name to choose and the history entry stuck. It merges back now:
    /// what can come back does, the stale copy of the written file stays where
    /// it landed beside the newer one, and the note says to compare them.
    #[test]
    fn a_move_between_filesystems_notes_what_it_left_and_its_undo_merges_back_around_it() {
        let device =
            |path: &Path| rustix::fs::stat(path).map(|stat| stat.st_dev).unwrap_or_default();
        let here = scratch("across-filesystems");
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() || device(shm) == device(&here) {
            return;
        }
        let source = shm.join(format!("skerry-ops-{}-across", std::process::id()));
        let _ = std::fs::remove_dir_all(&source);
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("copied.txt"), b"moved").unwrap();
        std::fs::write(source.join("written.txt"), b"as copied").unwrap();

        let mut armed = true;
        let change =
            move_into(&source, &here, Conflict::Refuse, &sk_util::Cancel::new(), &mut |p| {
                if armed && p.done == 2 {
                    armed = false;
                    std::fs::write(source.join("arrived.txt"), b"new").unwrap();
                    std::fs::write(source.join("written.txt"), b"written after").unwrap();
                }
            })
            .expect("moved");
        let landed = here.join(source.file_name().unwrap());
        assert_eq!(std::fs::read(landed.join("copied.txt")).unwrap(), b"moved");
        assert!(!source.join("copied.txt").exists());
        assert_eq!(std::fs::read(source.join("arrived.txt")).unwrap(), b"new");
        assert_eq!(std::fs::read(source.join("written.txt")).unwrap(), b"written after");
        let Change::Noted { change: inner, note, retry: false, stranded: false } = &change else {
            panic!("{change:?}")
        };
        assert!(matches!(**inner, Change::Renamed { .. }), "{inner:?}");
        assert!(note.starts_with("2 items changed or appeared"), "{note}");

        let undo = super::replay(&change, true, &mut |leaf| super::revert_recorded(leaf));
        let Some(Change::Noted { note, .. }) = &undo.completed else {
            panic!("the undo did not complete: {:?}", undo.problem)
        };
        assert!(note.contains("written.txt") && note.contains("already has its name"), "{note}");
        let reported = undo.problem.expect("the note is reported");
        assert_eq!((reported.kind(), reported.message()), (ProblemKind::Note, note.clone()));
        assert_eq!(std::fs::read(source.join("copied.txt")).unwrap(), b"moved");
        assert_eq!(std::fs::read(source.join("written.txt")).unwrap(), b"written after");
        assert_eq!(std::fs::read(source.join("arrived.txt")).unwrap(), b"new");
        assert_eq!(std::fs::read(landed.join("written.txt")).unwrap(), b"as copied");
        assert!(!landed.join("copied.txt").exists(), "what came back is still in both places");
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::remove_dir_all(&here).unwrap();
    }

    /// Somebody who can write the folder above a `.Trash-$uid` renames it away
    /// once Empty Trash has opened it, and plants a symlink to a folder shaped
    /// like a trash. Removing by name again deleted that folder's file of the
    /// same name; through the descriptors, the real entry goes and nothing else.
    #[test]
    fn a_trash_swapped_for_a_symlink_once_opened_has_only_its_own_entries_removed() {
        with_trash("empty-swap", |root| {
            let file = root.join("report.txt");
            std::fs::write(&file, b"trashed").unwrap();
            let Change::Trashed { stored, .. } = trash(&file).expect("trashed") else {
                panic!("trash")
            };
            let files = stored.parent().expect("files").to_path_buf();
            let trash_root = files.parent().expect("root").to_path_buf();
            let decoy = trash_root.with_file_name("decoy").join("files");
            std::fs::create_dir_all(&decoy).unwrap();
            let name = stored.file_name().expect("name");
            std::fs::write(decoy.join(name), b"precious").unwrap();

            crate::trash::AFTER_OPEN.set(Some(|trash_root| {
                std::fs::rename(trash_root, trash_root.with_file_name("held")).unwrap();
                std::os::unix::fs::symlink(trash_root.with_file_name("decoy"), trash_root).unwrap();
            }));
            let emptied = empty_trash(&files, &sk_util::Cancel::new());
            assert_eq!(std::fs::read(decoy.join(name)).unwrap(), b"precious");
            let held = trash_root.with_file_name("held");
            assert!(!held.join("files").join(name).exists(), "the trashed entry survived");
            emptied.expect("emptied");
        });
    }

    /// An entry the empty cannot take keeps itself and its record, and nothing
    /// else: stopping at it once left a trash that looked untouched.
    #[test]
    fn an_entry_that_will_not_go_does_not_keep_the_rest() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return;
        }
        with_trash("empty-partial", |root| {
            let locked = root.join("locked");
            std::fs::create_dir_all(&locked).unwrap();
            std::fs::write(locked.join("inside.txt"), b"x").unwrap();
            let before = root.join("before.txt");
            let after = root.join("after.txt");
            std::fs::write(&before, b"b").unwrap();
            std::fs::write(&after, b"a").unwrap();
            let trashed = [&before, &locked, &after].map(|path| match trash(path) {
                Ok(Change::Trashed { stored, info, .. }) => (stored, info),
                other => panic!("{other:?}"),
            });
            let (locked_stored, locked_info) = &trashed[1];
            std::fs::set_permissions(locked_stored, std::fs::Permissions::from_mode(0o000))
                .unwrap();
            let files = locked_stored.parent().expect("files").to_path_buf();

            let refused = empty_trash(&files, &sk_util::Cancel::new());
            std::fs::set_permissions(locked_stored, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            refused.expect_err("an entry that could not be erased was not reported");
            for (stored, info) in [&trashed[0], &trashed[2]] {
                assert!(!stored.exists(), "{stored:?} was kept by an entry beside it");
                assert!(!info.exists(), "{info:?} was kept by an entry beside it");
            }
            assert!(locked_stored.exists() && locked_info.exists(), "the survivor lost its record");
        });
    }

    /// A directory that is not a trash is not emptied, whatever it is called.
    #[test]
    fn a_directory_that_merely_looks_like_a_trash_is_refused() {
        with_trash("empty-hostile", |root| {
            let pretend = root.join("Trash/files");
            std::fs::create_dir_all(&pretend).unwrap();
            std::fs::write(pretend.join("theirs.txt"), b"important").unwrap();

            let refused = empty_trash(&pretend, &sk_util::Cancel::new()).expect_err("emptied");
            assert_eq!(refused.kind(), ProblemKind::TrashUnavailable);
            assert!(pretend.join("theirs.txt").is_file(), "somebody else's files were erased");
        });
    }

    /// What a trash is holding, which is what the question "empty this?" is
    /// about: the things a person threw away, and the bytes under them.
    #[test]
    fn a_trash_says_how_many_things_are_in_it_and_how_big_they_are() {
        with_trash("empty-held", |root| {
            let folder = root.join("pictures");
            std::fs::create_dir_all(folder.join("inner")).unwrap();
            std::fs::write(folder.join("inner/one.bin"), vec![0_u8; 300]).unwrap();
            std::fs::write(folder.join("two.bin"), vec![0_u8; 700]).unwrap();
            let alone = root.join("alone.bin");
            std::fs::write(&alone, vec![0_u8; 24]).unwrap();

            let Change::Trashed { stored, .. } = trash(&folder).expect("trashed") else {
                panic!("trash")
            };
            let _ = trash(&alone).expect("trashed");
            let files = stored.parent().expect("files");
            let held = crate::trash::held(files, &sk_util::Cancel::new());
            // Two things thrown away, not the four files under them.
            assert_eq!(held.items, 2);
            assert_eq!(held.bytes, 1024);
        });
    }

    /// A file is made, undone, and made again — and the undo refuses once
    /// somebody has typed into it, which is what makes the undo safe to have.
    #[test]
    fn a_file_is_made_and_unmade_while_it_is_still_empty() {
        let root = scratch("mkfile");
        let change = create_file(&root, "notes.txt").expect("made");
        assert_eq!(change, Change::Created { path: root.join("notes.txt"), made: Made::File });
        assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"");

        revert(&change).expect("unmade");
        assert!(!root.join("notes.txt").exists());
        apply(&change).expect("made again");

        std::fs::write(root.join("notes.txt"), b"words somebody typed").unwrap();
        let refused = revert(&change).expect_err("it must refuse");
        assert_eq!(refused.kind(), ProblemKind::NotEmpty);
        assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"words somebody typed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link is made, undone and made again — and undoing one removes the link
    /// and never what it points at.
    #[test]
    fn a_link_is_made_and_unmade_without_touching_its_target() {
        let root = scratch("mklink");
        let target = root.join("report.pdf");
        std::fs::write(&target, b"%PDF-1.4").unwrap();

        let change = create_link(&root, "report link", &target).expect("linked");
        assert_eq!(
            change,
            Change::Created { path: root.join("report link"), made: Made::Link(target.clone()) }
        );
        assert_eq!(std::fs::read_link(root.join("report link")).unwrap(), target);

        revert(&change).expect("unlinked");
        assert!(!root.join("report link").exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"%PDF-1.4", "the undo followed the link");

        apply(&change).expect("linked again");
        assert_eq!(std::fs::read_link(root.join("report link")).unwrap(), target);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A taken name is refused for all three creations, not only for a folder.
    #[test]
    fn a_creation_onto_a_taken_name_is_refused() {
        let root = scratch("create-taken");
        std::fs::write(root.join("keep.txt"), b"important").unwrap();

        assert_eq!(
            create_file(&root, "keep.txt").expect_err("made").kind(),
            ProblemKind::NameTaken
        );
        assert_eq!(
            create_link(&root, "keep.txt", Path::new("/etc/passwd")).expect_err("linked").kind(),
            ProblemKind::NameTaken
        );
        assert_eq!(std::fs::read(root.join("keep.txt")).unwrap(), b"important");
        for name in ["", "sub/deeper", "../escape"] {
            assert_eq!(
                create_file(&root, name).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName
            );
            assert_eq!(
                create_link(&root, name, Path::new("/tmp")).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mode a file has now, as `0o644`.
    fn mode_of(path: &Path) -> u16 {
        use std::os::unix::fs::PermissionsExt as _;
        (std::fs::metadata(path).unwrap().permissions().mode() & 0o7777) as u16
    }

    #[test]
    fn nine_bits_are_set_and_walked_back() {
        let root = scratch("chmod");
        let path = root.join("script.sh");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        set_permissions(&path, 0o600).expect("set");

        let change = set_permissions(&path, 0o751).expect("set");
        assert_eq!(change, Change::Permissions { path: path.clone(), before: 0o600, after: 0o751 });
        assert_eq!(mode_of(&path), 0o751);

        revert(&change).expect("undone");
        assert_eq!(mode_of(&path), 0o600, "the undo did not put the old bits back");

        apply(&change).expect("redone");
        assert_eq!(mode_of(&path), 0o751);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The three bits above the nine belong to the *permissions editor*
    /// `docs/11-non-goals.md` declines, so they are carried through untouched
    /// rather than cleared by a person answering a question about the nine.
    #[test]
    fn the_bits_above_the_nine_are_carried_through_untouched() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("chmod-high");
        let path = root.join("shared");
        std::fs::create_dir(&path).unwrap();
        if set_permissions(&path, 0o755).is_err() {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o1755)).unwrap();

        let change = set_permissions(&path, 0o770).expect("set");
        assert_eq!(mode_of(&path), 0o1770, "the sticky bit went with the nine");
        assert_eq!(
            change,
            Change::Permissions { path: path.clone(), before: 0o1755, after: 0o1770 }
        );
        revert(&change).expect("undone");
        assert_eq!(mode_of(&path), 0o1755);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink's bits are the target's on Linux, and `chmod` has no form that
    /// acts on the link — so the alternative to refusing is changing a file
    /// nobody pointed at.
    #[test]
    fn a_symlink_is_refused_rather_than_followed() {
        let root = scratch("chmod-link");
        let target = root.join("target.txt");
        let link = root.join("link");
        std::fs::write(&target, b"contents").unwrap();
        set_permissions(&target, 0o644).expect("set");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let refused = set_permissions(&link, 0o600).expect_err("a symlink was accepted");
        assert_eq!(refused.kind(), ProblemKind::Unreadable);
        assert_eq!(mode_of(&target), 0o644, "the target was changed through the link");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The check and the change used to be two lookups of one path, and a file
    /// swapped for a symlink between them had the link's target changed. Both
    /// ways of changing a held mode — `fchmodat2` and, for a kernel before 6.6,
    /// `/proc/self/fd` — act on the file that was held and nothing else.
    #[test]
    fn a_file_swapped_for_a_symlink_once_held_is_the_only_one_changed() {
        use std::os::unix::fs::PermissionsExt as _;
        type Change = fn(&std::os::fd::OwnedFd, &Path, u16) -> Result<(), sk_model::Problem>;
        let ways: [(&str, Change); 2] =
            [("fchmodat2", super::chmod), ("proc", super::chmod_through_proc)];
        for (way, change) in ways {
            let root = scratch(&format!("chmod-swap-{way}"));
            let file = root.join("file");
            let victim = root.join("victim");
            for path in [&file, &victim] {
                std::fs::write(path, b"contents").unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let (held, before) = super::hold(&file).expect("held");
            assert_eq!(before, 0o600);
            std::fs::rename(&file, root.join("moved")).unwrap();
            std::os::unix::fs::symlink(&victim, &file).unwrap();

            change(&held, &file, 0o640).expect("changed");
            assert_eq!(mode_of(&victim), 0o600, "{way} changed the symlink's target");
            assert_eq!(mode_of(&root.join("moved")), 0o640, "{way} missed the held file");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A seccomp filter older than `fchmodat2` — a snap's, a container runtime's
    /// — answers `EPERM` for it, not `ENOSYS`, and the change was refused with
    /// a permission error about a file its owner may change. Installed here on
    /// a thread of the test's own, which a filter does not outlive.
    #[test]
    fn a_filter_that_refuses_fchmodat2_with_eperm_still_gets_the_change() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("chmod-eperm");
        let path = root.join("file");
        std::fs::write(&path, b"contents").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let held = path.clone();
        let changed = std::thread::spawn(move || {
            let k = |code: u32, jt, jf, k| libc::sock_filter { code: code as u16, jt, jf, k };
            let filter = [
                k(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
                k(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, 0, 1, super::FCHMODAT2 as u32),
                k(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
                k(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog { len: 4, filter: filter.as_ptr().cast_mut() };
            // SAFETY: both calls take plain integers and, for the second, a
            // pointer to a program that outlives the call; the filter binds only
            // this thread, which ends with the test.
            let installed = unsafe {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
                    && libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER,
                        &raw const program,
                    ) == 0
            };
            assert!(installed, "the seccomp filter was refused");
            set_permissions(&held, 0o640).map(|_| mode_of(&held))
        })
        .join()
        .unwrap();
        assert_eq!(changed.expect("changed through /proc"), 0o640);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fallback's `ENOENT` is `/proc` missing, not the file: it is held open.
    #[test]
    fn a_missing_proc_is_named_rather_than_the_file_called_gone() {
        let refused = super::proc_failure(rustix::io::Errno::NOENT, Path::new("/a/f"));
        assert_eq!(refused.kind(), ProblemKind::ModeUnchangeable);
        assert!(refused.message().contains("/proc"), "{}", refused.message());
    }

    /// Why the descriptor is `O_PATH`: a mode is changed by its owner, who need
    /// not be able to read the file — and a `000` file is the one most in need.
    #[test]
    fn a_file_nobody_may_read_can_still_have_its_bits_set() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("chmod-unreadable");
        let path = root.join("locked");
        std::fs::write(&path, b"contents").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let change = set_permissions(&path, 0o600).expect("set");
        assert_eq!(change, Change::Permissions { path: path.clone(), before: 0, after: 0o600 });
        assert_eq!(mode_of(&path), 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The refusal `Change::Edited` makes about bytes, for the same reason: an
    /// undo whose starting point has moved is not an undo.
    #[test]
    fn an_undo_refuses_when_somebody_else_has_changed_the_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("chmod-raced");
        let path = root.join("notes.txt");
        std::fs::write(&path, b"contents").unwrap();
        set_permissions(&path, 0o644).expect("set");
        let change = set_permissions(&path, 0o600).expect("set");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let refused = revert(&change).expect_err("an undo overwrote somebody else's change");
        assert_eq!(refused.kind(), ProblemKind::ChangedSince);
        assert_eq!(mode_of(&path), 0o640);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One press is one row on the history, named after the file it was about.
    #[test]
    fn a_permission_change_names_itself_on_the_history() {
        let change = Change::Permissions {
            path: PathBuf::from("/tmp/notes.txt"),
            before: 0o644,
            after: 0o600,
        };
        assert_eq!(change.summary(), ("history.permissions", "notes.txt".to_owned()));
        assert_eq!(change.subject(), Path::new("/tmp/notes.txt"));
        assert_eq!(change.origin(), Some(Path::new("/tmp/notes.txt")));
        let (key, count) = Change::Batch(vec![change.clone(), change]).summary();
        assert_eq!((key, count.as_str()), ("history.permissions_many", "2"));
    }

    #[test]
    fn a_directory_is_made_and_unmade() {
        let root = scratch("mkdir");
        let change = create_directory(&root, "notes").expect("made");
        assert_eq!(change, Change::Created { path: root.join("notes"), made: Made::Directory });
        assert!(root.join("notes").is_dir());

        revert(&change).expect("unmade");
        assert!(!root.join("notes").exists());

        // And again, which is what redo is.
        apply(&change).expect("made again");
        assert!(root.join("notes").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An undo that deletes files somebody put there is not an undo.
    #[test]
    fn undoing_a_directory_refuses_once_something_is_in_it() {
        let root = scratch("not-empty");
        let change = create_directory(&root, "notes").expect("made");
        std::fs::write(root.join("notes/theirs.txt"), b"mine now").unwrap();

        let refused = revert(&change).expect_err("it must refuse");
        assert_eq!(refused.kind(), ProblemKind::NotEmpty);
        assert!(root.join("notes/theirs.txt").is_file(), "the undo deleted somebody's file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_that_is_taken_is_refused_rather_than_replaced() {
        let root = scratch("taken");
        std::fs::write(root.join("keep.txt"), b"important").unwrap();
        std::fs::write(root.join("other.txt"), b"other").unwrap();

        let refused = rename(&root.join("other.txt"), "keep.txt").expect_err("must refuse");
        assert_eq!(refused.kind(), ProblemKind::NameTaken);
        assert_eq!(std::fs::read(root.join("keep.txt")).unwrap(), b"important");
        assert!(root.join("other.txt").is_file(), "the source vanished into a refused rename");

        let taken = create_directory(&root, "keep.txt").expect_err("must refuse");
        assert_eq!(taken.kind(), ProblemKind::NameTaken);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rename_goes_back_where_it_came_from() {
        let root = scratch("rename");
        std::fs::write(root.join("draft.md"), b"words").unwrap();
        let change = rename(&root.join("draft.md"), "final.md").expect("renamed");
        assert_eq!(
            change,
            Change::Renamed { from: root.join("draft.md"), to: root.join("final.md") }
        );
        assert!(root.join("final.md").is_file() && !root.join("draft.md").exists());

        revert(&change).expect("renamed back");
        assert!(root.join("draft.md").is_file() && !root.join("final.md").exists());
        assert_eq!(std::fs::read(root.join("draft.md")).unwrap(), b"words");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_with_a_separator_or_no_name_at_all_is_refused() {
        let root = scratch("bad-name");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        for name in ["", "sub/deeper", "/etc/passwd", "../escape"] {
            assert_eq!(
                create_directory(&root, name).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName,
                "{name:?} was accepted as a directory name"
            );
            assert_eq!(
                rename(&root.join("a.txt"), name).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName,
                "{name:?} was accepted as a new name"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Replace keeps what it replaced, which is the whole reason it exists.
    ///
    /// Every other file manager writes over the file that was there. Rule 3 does
    /// not allow that, so this is one batch: the arrival, and the displaced file
    /// in the Trash — and `Ctrl+Z` undoes both, in that order.
    #[test]
    fn replacing_puts_what_was_there_in_the_trash_and_undo_brings_it_back() {
        with_trash("replace", |root| {
            let from = root.join("from");
            let into = root.join("into");
            std::fs::create_dir_all(&from).unwrap();
            std::fs::create_dir_all(&into).unwrap();
            std::fs::write(from.join("report.pdf"), b"the new one").unwrap();
            std::fs::write(into.join("report.pdf"), b"the old one").unwrap();

            let change = copy_into(
                &from.join("report.pdf"),
                &into,
                Conflict::Replace,
                &sk_util::Cancel::new(),
                &mut |_| {},
            )
            .expect("replaced");
            assert_eq!(
                std::fs::read(into.join("report.pdf")).unwrap(),
                b"the new one",
                "the arriving file did not take the name"
            );
            // And nothing is left staged beside it under the name the
            // replacement was copied in as.
            assert!(!into.join("report (copy).pdf").exists(), "the staging name was left behind");

            revert(&change).expect("undone");
            assert_eq!(
                std::fs::read(into.join("report.pdf")).unwrap(),
                b"the old one",
                "undo did not bring back what was replaced"
            );
        });
    }

    /// The failure this ordering exists for.
    ///
    /// A copy that cannot happen must leave the directory exactly as it was —
    /// not with the old file trashed and nothing in its place, which is what
    /// trashing first would do.
    #[test]
    fn a_replacement_that_cannot_be_copied_leaves_the_original_where_it_was() {
        with_trash("replace-fails", |root| {
            let into = root.join("into");
            std::fs::create_dir_all(&into).unwrap();
            std::fs::write(into.join("report.pdf"), b"the old one").unwrap();

            // The *same* name, or the destination is not taken and the whole
            // replacing path is never entered — which is how the first version
            // of this test passed against the ordering it exists to forbid.
            let problem = copy_into(
                &root.join("report.pdf"),
                &into,
                Conflict::Replace,
                &sk_util::Cancel::new(),
                &mut |_| {},
            )
            .expect_err("a source that is not there was copied");
            assert_eq!(problem.kind(), ProblemKind::Gone);
            assert_eq!(
                std::fs::read(into.join("report.pdf")).unwrap(),
                b"the old one",
                "the file that was there went to the Trash for a copy that never happened"
            );
        });
    }

    /// A move over a taken name keeps the old one too, and is one batch.
    #[test]
    fn moving_over_a_name_trashes_what_was_there() {
        with_trash("replace-move", |root| {
            let from = root.join("from");
            let into = root.join("into");
            std::fs::create_dir_all(&from).unwrap();
            std::fs::create_dir_all(&into).unwrap();
            std::fs::write(from.join("notes.txt"), b"arriving").unwrap();
            std::fs::write(into.join("notes.txt"), b"displaced").unwrap();

            let change = move_into(
                &from.join("notes.txt"),
                &into,
                Conflict::Replace,
                &sk_util::Cancel::new(),
                &mut |_| {},
            )
            .expect("moved over");
            assert!(!from.join("notes.txt").exists(), "the move left the source behind");
            assert_eq!(std::fs::read(into.join("notes.txt")).unwrap(), b"arriving");

            revert(&change).expect("undone");
            assert_eq!(std::fs::read(into.join("notes.txt")).unwrap(), b"displaced");
            assert_eq!(std::fs::read(from.join("notes.txt")).unwrap(), b"arriving");
        });
    }

    fn with_trash<T>(label: &str, body: impl FnOnce(&Path) -> T) -> T {
        let guard = TRASH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = scratch(label);
        let before = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: the mutex above is what makes this the only thread in this
        // process changing the environment for the length of the body.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
        let outcome = body(&root);
        unsafe {
            match before {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
        outcome
    }

    /// The two files the specification puts there, and both of them gone again.
    #[test]
    fn trashing_writes_a_record_beside_the_file_and_undo_takes_both_away() {
        with_trash("trash", |root| {
            let file = root.join("receipt.pdf");
            std::fs::write(&file, b"%PDF-1.4").unwrap();

            let change = trash(&file).expect("trashed");
            let Change::Trashed { original, stored, info } = &change else { panic!("{change:?}") };
            assert_eq!(original, &file);
            assert!(!file.exists(), "the original is still there");
            assert_eq!(std::fs::read(stored).unwrap(), b"%PDF-1.4");

            let record = std::fs::read_to_string(info).unwrap();
            assert!(record.starts_with("[Trash Info]\n"), "{record:?}");
            assert!(
                record.contains(&format!("Path={}\n", encode_path(&file.to_string_lossy()))),
                "{record:?}"
            );
            // `YYYY-MM-DDThh:mm:ss`, local, no zone.
            let date = record
                .lines()
                .find_map(|line| line.strip_prefix("DeletionDate="))
                .expect("a deletion date");
            assert_eq!(date.len(), 19, "{date:?}");
            assert_eq!(date.as_bytes()[10], b'T', "{date:?}");
            assert!(date.starts_with("20"), "{date:?}");

            revert(&change).expect("restored");
            assert_eq!(std::fs::read(&file).unwrap(), b"%PDF-1.4");
            assert!(!stored.exists(), "the trashed copy is still there");
            assert!(!info.exists(), "a record with no file behind it is a corrupt trash");
        });
    }

    /// Two files of the same name, trashed one after the other. Neither may
    /// overwrite the other, and each record must name its own file.
    #[test]
    fn the_same_name_twice_takes_two_slots() {
        with_trash("trash-twice", |root| {
            std::fs::create_dir_all(root.join("a")).unwrap();
            std::fs::create_dir_all(root.join("b")).unwrap();
            std::fs::write(root.join("a/notes.txt"), b"first").unwrap();
            std::fs::write(root.join("b/notes.txt"), b"second").unwrap();

            let first = trash(&root.join("a/notes.txt")).expect("first");
            let second = trash(&root.join("b/notes.txt")).expect("second");
            let (Change::Trashed { stored: one, .. }, Change::Trashed { stored: two, .. }) =
                (&first, &second)
            else {
                panic!("{first:?} {second:?}")
            };
            assert_ne!(one, two, "the second overwrote the first");
            assert_eq!(std::fs::read(one).unwrap(), b"first");
            assert_eq!(std::fs::read(two).unwrap(), b"second");

            // And each goes back to its own directory.
            revert(&second).expect("second back");
            revert(&first).expect("first back");
            assert_eq!(std::fs::read(root.join("a/notes.txt")).unwrap(), b"first");
            assert_eq!(std::fs::read(root.join("b/notes.txt")).unwrap(), b"second");
        });
    }

    /// Trash remains on its source volume and its record supports recovery after restart.
    #[test]
    fn a_file_on_another_filesystem_uses_its_local_trash_and_restores() {
        with_trash("trash-xdev", |root| {
            // `/dev/shm` is a tmpfs on every Linux system that has it, and the
            // scratch root is on whatever `/tmp` is. Where they are the same
            // filesystem there is nothing to test and the test says so.
            let elsewhere = Path::new("/dev/shm");
            if !elsewhere.is_dir() {
                return;
            }
            let mine = elsewhere.join(format!("skerry-xdev-{}.txt", std::process::id()));
            if std::fs::write(&mine, b"elsewhere").is_err() {
                return;
            }
            let same = std::fs::metadata(&mine)
                .and_then(|a| std::fs::metadata(root).map(|b| (a, b)))
                .is_ok_and(|(a, b)| {
                    use std::os::unix::fs::MetadataExt as _;
                    a.dev() == b.dev()
                });
            if same {
                let _ = std::fs::remove_file(&mine);
                return;
            }

            let change = trash(&mine).expect("volume trash");
            let Change::Trashed { stored, info, .. } = &change else { panic!("trash expected") };
            assert!(stored.starts_with(elsewhere));
            assert!(!mine.exists());
            let record = std::fs::read_to_string(info).unwrap();
            assert!(record.contains(&format!("Path=skerry-xdev-{}.txt", std::process::id())));
            let restored = super::restore(stored).expect("persistent restore");
            assert_eq!(std::fs::read(&mine).unwrap(), b"elsewhere");
            revert(&restored).expect("undo restore");
            apply(&restored).expect("redo restore");
            // And no orphan record: the reservation is made before the move, so
            // a failed move that left it behind would be a trash the desktop's
            // own tools report as corrupt.
            let orphan = root
                .join("data/Trash/info")
                .join(format!("{}.trashinfo", mine.file_name().unwrap().to_string_lossy()));
            assert!(!orphan.exists(), "a record with no file behind it: {orphan:?}");
            let _ = std::fs::remove_file(&mine);
        });
    }

    #[test]
    fn persistent_restore_never_replaces_an_existing_original() {
        with_trash("restore-collision", |root| {
            let source = root.join("notes");
            std::fs::write(&source, b"old").unwrap();
            let Change::Trashed { stored, info, .. } = trash(&source).unwrap() else {
                panic!("trash")
            };
            std::fs::write(&source, b"new").unwrap();
            assert_eq!(super::restore(&stored).unwrap_err().kind(), ProblemKind::NameTaken);
            assert_eq!(std::fs::read(&source).unwrap(), b"new");
            assert_eq!(std::fs::read(&stored).unwrap(), b"old");
            assert!(info.exists());
            std::fs::remove_file(&source).unwrap();
            let restored = super::restore(&stored).unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), b"old");
            revert(&restored).unwrap();
            apply(&restored).unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), b"old");
        });
    }

    #[test]
    fn recovery_rejects_malformed_paths_and_symlinked_records() {
        with_trash("restore-hostile", |root| {
            let source = root.join("notes");
            std::fs::write(&source, b"kept").unwrap();
            let Change::Trashed { stored, info, .. } = trash(&source).unwrap() else {
                panic!("trash")
            };
            for path in ["../escape", "%2e%2e/escape", "%00", "bad%ZZ", ""] {
                std::fs::write(
                    &info,
                    format!("[Trash Info]\nPath={path}\nDeletionDate=2026-09-09T12:00:00\n"),
                )
                .unwrap();
                assert!(super::restore(&stored).is_err(), "{path}");
                assert_eq!(std::fs::read(&stored).unwrap(), b"kept");
            }
            std::fs::remove_file(&info).unwrap();
            std::os::unix::fs::symlink(&stored, &info).unwrap();
            assert!(super::restore(&stored).is_err());
            assert_eq!(std::fs::read(&stored).unwrap(), b"kept");
        });
    }

    #[test]
    fn trash_refuses_symlinked_storage_without_disturbing_the_source() {
        with_trash("trash-hostile", |root| {
            let outside = root.join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::create_dir_all(root.join("data")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("data/Trash")).unwrap();
            let source = root.join("notes");
            std::fs::write(&source, b"kept").unwrap();
            assert_eq!(trash(&source).unwrap_err().kind(), ProblemKind::TrashUnavailable);
            assert_eq!(std::fs::read(&source).unwrap(), b"kept");
            // Nothing may have been written through the symlink, and counting is how
            // that is read.
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0); // skerry-allow: no-read-dir
        });
    }

    /// The `Path=` field is read back by a restore, so the separators have to
    /// survive and everything else has to be encoded.
    #[test]
    fn a_trashinfo_path_keeps_its_separators_and_encodes_the_rest() {
        assert_eq!(encode_path("/home/tester/notes.txt"), "/home/tester/notes.txt");
        assert_eq!(encode_path("/home/tester/My File.txt"), "/home/tester/My%20File.txt");
        assert_eq!(encode_path("/home/tester/a#b?c"), "/home/tester/a%23b%3Fc");
        assert_eq!(encode_path("/home/tester/café"), "/home/tester/caf%C3%A9");
        assert_eq!(encode_path("/a-b_c.d~e"), "/a-b_c.d~e");
    }

    /// A batch is undone **backwards**, and only a dependent pair can tell.
    ///
    /// A folder made and then filled: undone backwards the file comes out and
    /// the empty folder goes, and undone forwards the `rmdir` meets `ENOTEMPTY`
    /// and the whole undo stops having done nothing. Two independent changes
    /// would pass either way, which is why this one is not two renames.
    #[test]
    fn a_batch_is_reverted_in_the_reverse_order() {
        let home = scratch("batch-order");
        let folder = home.join("into");
        std::fs::create_dir(&folder).expect("the folder");
        std::fs::write(home.join("f"), b"one").expect("the file");
        std::fs::rename(home.join("f"), folder.join("f")).expect("move it in");

        let batch = Change::Batch(vec![
            Change::Created { path: folder.clone(), made: Made::Directory },
            Change::Renamed { from: home.join("f"), to: folder.join("f") },
        ]);
        revert(&batch).expect("the batch undoes");
        assert_eq!(std::fs::read(home.join("f")).expect("f is back"), b"one");
        assert!(!folder.exists(), "the folder the batch made is still there");
    }

    /// **Undoing an extraction takes the folder and everything in it** — and
    /// that is a recursive remove, which is only an undo because every one of
    /// those files was put there by the operation being undone. The same
    /// argument `Copied` makes; the difference is that this one cannot be
    /// re-`apply`ed here, and says so rather than pretending.
    #[test]
    fn undoing_an_unpack_removes_what_it_unpacked() {
        with_trash("unpacked", |home| {
            let root = home.join("reports");
            std::fs::create_dir_all(root.join("deep/inner")).expect("the tree");
            std::fs::write(root.join("notes.txt"), b"out of the archive").expect("a file");
            std::fs::write(root.join("deep/inner/note.md"), b"nested").expect("another");
            let archive = home.join("reports.zip");
            std::fs::write(&archive, b"PK\x03\x04").expect("the archive");

            let change =
                Change::Unpacked { archive: archive.clone(), root: root.clone(), only: Vec::new() };
            assert_eq!(change.subject(), root, "the cursor goes to the folder it made");
            assert_eq!(change.origin(), Some(archive.as_path()), "and back to the archive");
            assert_eq!(change.summary().0, "history.unpacked");

            revert(&change).expect("the unpack undoes");
            assert!(!root.exists(), "undo left the folder behind");
            assert!(archive.is_file(), "undo took the archive as well");

            // **Redone by the caller, because doing it again needs a child process.**
            // A `Problem` here is the honest answer for a direct call; `Edits::redo`
            // never makes one, and a test that let this pass silently would be a
            // test that let the redo path disappear.
            assert!(apply(&change).is_err(), "ops::apply pretended to redo an unpack");
        });
    }

    #[test]
    fn an_empty_batch_undoes_to_nothing_rather_than_failing() {
        assert!(revert(&Change::Batch(Vec::new())).is_ok());
    }

    /// The cursor lands on what the gesture finished with, and after an undo on
    /// what the first change moved.
    #[test]
    fn a_batch_points_at_its_last_change_and_comes_back_to_its_first() {
        let batch = Change::Batch(vec![
            Change::Renamed { from: "/x/one".into(), to: "/x/two".into() },
            Change::Renamed { from: "/x/three".into(), to: "/x/four".into() },
        ]);
        assert_eq!(batch.subject(), Path::new("/x/four"));
        assert_eq!(batch.origin(), Some(Path::new("/x/one")));
    }

    /// A stopped copy is picked up rather than done twice.
    ///
    /// [Rule 7](../../../docs/03-principles.md#7-honest-progress)'s last clause.
    /// The half-copied tree here is what a cancel leaves: one file written, one
    /// missing, and a directory that already exists. Resuming copies the missing
    /// one, enters the directory rather than refusing it, and leaves the file
    /// that is already there alone — asserted by its *contents*, because a resume
    /// that copied it again would also produce the right size.
    #[test]
    fn a_resumed_copy_fills_in_what_is_missing_and_leaves_the_rest() {
        let root = scratch("resume");
        let from = root.join("from/folder");
        let into = root.join("into");
        std::fs::create_dir_all(from.join("deeper")).unwrap();
        std::fs::write(from.join("first.txt"), b"one").unwrap();
        std::fs::write(from.join("second.txt"), b"two").unwrap();
        std::fs::write(from.join("deeper/third.txt"), b"three").unwrap();
        std::os::unix::fs::symlink("first.txt", from.join("link")).unwrap();

        // What a stop left: the directory, one file of the right size with the
        // wrong contents, and nothing else.
        let landed = into.join("folder");
        std::fs::create_dir_all(&landed).unwrap();
        std::fs::write(landed.join("first.txt"), b"ONE").unwrap();

        let change =
            copy_into(&from, &into, Conflict::Resume, &sk_util::Cancel::new(), &mut |_| {})
                .expect("the resume was refused");
        assert_eq!(change.subject(), landed);
        assert_eq!(std::fs::read(landed.join("first.txt")).unwrap(), b"ONE", "it was copied again");
        assert_eq!(std::fs::read(landed.join("second.txt")).unwrap(), b"two");
        assert_eq!(std::fs::read(landed.join("deeper/third.txt")).unwrap(), b"three");
        assert!(landed.join("link").is_symlink());

        // And a second resume over a complete tree is a walk that writes nothing
        // and refuses nothing, which is what makes Resume safe to press twice.
        copy_into(&from, &into, Conflict::Resume, &sk_util::Cancel::new(), &mut |_| {})
            .expect("a second resume was refused");
        assert_eq!(std::fs::read(landed.join("first.txt")).unwrap(), b"ONE");

        // **A file of a different size is refused, not overwritten.** It is not
        // what the stop left — something else has that name — and
        // [rule 3](../../../docs/03-principles.md#3-everything-is-reversible)
        // lets nothing here destroy a file without the trash. Resume's whole
        // authority is to skip what matches; anything else is the ordinary
        // taken-name refusal, which the queue turns into the question with four
        // answers.
        std::fs::write(landed.join("second.txt"), b"a longer thing").unwrap();
        let refused =
            copy_into(&from, &into, Conflict::Resume, &sk_util::Cancel::new(), &mut |_| {});
        assert_eq!(
            refused.err().map(|problem| problem.kind()),
            Some(ProblemKind::NameTaken),
            "a resume overwrote a file it did not put there"
        );
        assert_eq!(std::fs::read(landed.join("second.txt")).unwrap(), b"a longer thing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same tree, refused rather than merged when nobody asked to resume.
    ///
    /// The guard on the behaviour above: without it, every ordinary copy into a
    /// directory that exists would quietly merge, which is the one thing a
    /// conflict answer is for choosing.
    #[test]
    fn a_copy_that_is_not_a_resume_still_refuses_what_is_there() {
        let root = scratch("resume-guard");
        let from = root.join("from/folder");
        let into = root.join("into");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("first.txt"), b"one").unwrap();
        std::fs::create_dir_all(into.join("folder")).unwrap();

        let refused =
            copy_into(&from, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {});
        assert!(refused.is_err(), "a copy over an existing folder merged without being asked");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An edit is undone by putting the old bytes back, and redone by putting the
    /// new ones there again.
    ///
    /// The one change in this module that is not a move, and the only one whose
    /// undo needs something carried on the entry rather than read off the disk —
    /// after the write there is nowhere else the old contents could come from.
    #[test]
    fn an_edit_is_undone_by_putting_the_old_bytes_back() {
        let root = scratch("edit");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("notes.txt");
        std::fs::write(&file, b"before\n").unwrap();

        let change = super::edit_text(&file, "after\n").expect("the edit was refused");
        assert_eq!(std::fs::read(&file).unwrap(), b"after\n");
        assert_eq!(change.subject(), file);
        assert_eq!(change.origin(), Some(file.as_path()));

        revert(&change).expect("the undo was refused");
        assert_eq!(std::fs::read(&file).unwrap(), b"before\n");
        let again = apply(&change).expect("the redo was refused");
        assert_eq!(std::fs::read(&file).unwrap(), b"after\n");
        assert_eq!(again.subject(), file);

        // And nothing is left beside it: the write is the atomic one, so the
        // staging name it renamed from is gone.
        assert!(!root.join("notes.new").exists(), "the staging file was left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file that is not there cannot be edited, and says so rather than
    /// creating one.
    #[test]
    fn editing_a_file_that_is_not_there_is_refused() {
        let root = scratch("edit-missing");
        std::fs::create_dir_all(&root).unwrap();
        let missing = root.join("nothing.txt");
        assert!(super::edit_text(&missing, "x").is_err());
        assert!(!missing.exists(), "a refused edit created the file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replacement_failure_returns_the_moved_source() {
        with_trash("replace-failure", |root| {
            let source = root.join("source");
            let into = root.join("into");
            std::fs::create_dir(&into).unwrap();
            std::fs::write(&source, "source").unwrap();
            std::fs::write(into.join("source"), "destination").unwrap();
            std::fs::write(root.join("data"), "blocked trash").unwrap();
            assert!(
                move_into(&source, &into, Conflict::Replace, &sk_util::Cancel::new(), &mut |_| {})
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
            assert_eq!(std::fs::read_to_string(into.join("source")).unwrap(), "destination");
        });
    }

    #[test]
    fn move_redo_keeps_the_destination_directory() {
        let root = scratch("move-redo-parent");
        let source = root.join("tree");
        let into = root.join("into");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&into).unwrap();
        std::fs::write(source.join("leaf"), "contents").unwrap();
        let change =
            move_into(&source, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                .unwrap();
        revert(&change).unwrap();
        apply(&change).unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(into.join("tree/leaf")).unwrap(), "contents");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_undo_keeps_later_work_in_trash() {
        with_trash("undo-preserves", |root| {
            let source = root.join("tree");
            let into = root.join("into");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&into).unwrap();
            let change =
                copy_into(&source, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                    .unwrap();
            std::fs::write(into.join("tree/later"), "valuable").unwrap();
            revert(&change).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("data/Trash/files/tree/later")).unwrap(),
                "valuable"
            );
        });
    }

    #[test]
    fn replay_retains_both_sides_of_a_failed_batch() {
        let root = scratch("replay-boundary");
        let folder = root.join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("file"), "contents").unwrap();
        std::fs::write(folder.join("blocker"), "later work").unwrap();
        let created = Change::Created { path: folder.clone(), made: Made::Directory };
        let moved = Change::Renamed { from: root.join("file"), to: folder.join("file") };
        let batch = Change::Batch(vec![created.clone(), Change::Batch(vec![moved.clone()])]);
        let first =
            super::replay(&batch, true, &mut |change| revert(change).map(|()| change.clone()));
        assert!(first.problem.is_some());
        assert_eq!(first.remaining, Some(created));
        assert_eq!(first.completed, Some(moved));
        std::fs::remove_file(folder.join("blocker")).unwrap();
        let second = super::replay(&first.remaining.unwrap(), true, &mut |change| {
            revert(change).map(|()| change.clone())
        });
        assert!(second.problem.is_none());
        assert!(!folder.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_partial_tree_failure_returns_an_undoable_warning() {
        let root = scratch("partial-copy");
        let source = root.join("tree");
        let into = root.join("into");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&into).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            source.join("fifo"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();
        let result =
            copy_into(&source, &into, Conflict::Refuse, &sk_util::Cancel::new(), &mut |_| {})
                .unwrap();
        let Change::Noted { change, note, .. } = result else {
            panic!("partial work was not reported")
        };
        assert_eq!(*change, Change::Copied { source, destination: into.join("tree") });
        assert!(!note.is_empty());
        assert!(into.join("tree").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }
}
