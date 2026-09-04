// SPDX-License-Identifier: MIT OR Apache-2.0
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

use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use sk_model::{Problem, ProblemKind};

/// What a mutation did, and enough to undo it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    /// Several changes made by one gesture, oldest first.
    ///
    /// **One gesture is one undo.** A paste of two hundred files makes two
    /// hundred changes, and a person who presses `Ctrl+Z` after it meant to
    /// undo the paste — not the last file of it, two hundred times. Undone
    /// **backwards**, because the last change is the one made on top of the
    /// others: a move that replaced a name has to be put back before the thing
    /// it displaced can be.
    Batch(Vec<Change>),
    /// A directory that did not exist before.
    Created {
        /// Where it is.
        path: PathBuf,
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
    /// What a person is looking for after an operation: the folder they made,
    /// the new name, or — after an undo — the thing that came back.
    #[must_use]
    pub fn subject(&self) -> &Path {
        match self {
            // The last one, which is what the gesture finished by doing and
            // therefore where a person is looking.
            Self::Batch(changes) => changes.last().map_or(Path::new(""), Change::subject),
            // The folder that was made, and the file that was edited: two
            // different changes whose subject happens to be the path they carry.
            Self::Created { path } | Self::Edited { path, .. } => path,
            Self::Renamed { to, .. } => to,
            Self::Copied { destination, .. } => destination,
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
            Self::Batch(changes) => match changes.as_slice() {
                [] => ("history.nothing", String::new()),
                [only] => only.summary(),
                many => {
                    let (key, _) =
                        many.last().map_or(("history.nothing", String::new()), Change::summary);
                    (many_key(key), many.len().to_string())
                }
            },
            Self::Created { path } => ("history.created", named(path)),
            Self::Renamed { to, .. } => ("history.renamed", named(to)),
            Self::Copied { destination, .. } => ("history.copied", named(destination)),
            Self::Trashed { original, .. } => ("history.trashed", named(original)),
            Self::Erased { path } => ("history.erased", named(path)),
            Self::Edited { path, .. } => ("history.edited", named(path)),
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
            Self::Batch(changes) => changes.first().and_then(Change::origin),
            // Nothing to point at: undoing a `Created` removes the folder, and
            // an `Erased` is not undone at all.
            Self::Created { .. } | Self::Erased { .. } => None,
            Self::Renamed { from, .. } => Some(from),
            // A copy's source never moved, so an undo leaves the cursor on it.
            Self::Copied { source, .. } => Some(source),
            Self::Trashed { original, .. } => Some(original),
            // An edit moves nothing, so undoing one leaves the cursor where it is.
            Self::Edited { path, .. } => Some(path),
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
        "history.created" => "history.created_many",
        "history.renamed" => "history.renamed_many",
        "history.copied" => "history.copied_many",
        "history.trashed" => "history.trashed_many",
        "history.erased" => "history.erased_many",
        "history.edited" => "history.edited_many",
        _ => "history.changed_many",
    }
}

/// Makes a directory called `name` inside `parent`.
///
/// **Runs on a job.**
pub fn create_directory(parent: &Path, name: &str) -> Result<Change, Problem> {
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    let path = parent.join(name);
    // `mkdir` fails with `EEXIST` rather than replacing, which is the whole
    // reason not to check first: a check and a create are two syscalls with a
    // race between them, and the create already answers the question.
    rustix::fs::mkdir(&path, Mode::from_bits_truncate(0o755))
        .map_err(|error| problem(error, &path))?;
    Ok(Change::Created { path })
}

/// Renames `from` to `name`, in the same directory.
///
/// **Runs on a job.**
pub fn rename(from: &Path, name: &str) -> Result<Change, Problem> {
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
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(Problem::new(ProblemKind::BadName, path.display().to_string(), "no name"));
    };
    let home = trash_home()?;
    let (files, info) = (home.join("files"), home.join("info"));
    // The data directory may not exist at all on a fresh account, and `mkdir`
    // does not make parents. The parents get the ordinary mode; the three trash
    // directories get **0700**, because the specification says a trash is
    // private and a world-readable one leaks every filename in it.
    if let Some(parent) = home.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(io_problem(&error, parent));
    }
    for directory in [&home, &files, &info] {
        if let Err(error) = rustix::fs::mkdir(directory, Mode::from_bits_truncate(0o700))
            && error != rustix::io::Errno::EXIST
        {
            return Err(problem(error, directory));
        }
    }

    // **The `.trashinfo` reserves the name, not the move.** The specification
    // says to create it with `O_EXCL`, because that is the one step that can
    // claim a name atomically — two programs trashing `notes.txt` at the same
    // moment must not both believe they own `files/notes.txt`.
    let (stored, info_path) = claim(&files, &info, name)?;
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_path(&path.to_string_lossy()),
        deletion_date()
    );
    std::fs::write(&info_path, record).map_err(|error| io_problem(&error, &info_path))?;

    if let Err(error) = move_no_replace(path, &stored) {
        // The reservation is ours and the move failed, so the reservation goes:
        // an `info` entry with no file behind it is a trash the desktop's own
        // tools report as corrupt.
        let _ = std::fs::remove_file(&info_path);
        return Err(error);
    }
    Ok(Change::Trashed { original: path.to_path_buf(), stored, info: info_path })
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
            Err(problem) => Err(problem),
        };
    }
    let destination = into.join(name);
    if conflict == Conflict::Replace && taken(&destination) {
        return replace_into(name, into, &mut |staging| {
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
        Err(problem) => Err(problem),
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
    /// The cost is that this fails where the trash does — a file on a drive with
    /// no per-volume `.Trash-$uid`, which is the same gap
    /// [`ProblemKind::AcrossFilesystems`] names. Reported rather than downgraded
    /// to an overwrite: "Replace" that silently became "destroy" on some drives
    /// and not others is the kind of difference nobody can see until it costs
    /// them something.
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

/// Moves `source` into the directory `into`.
///
/// Recorded as a rename, because that is what it is when the filesystems match
/// and what its undo has to do either way.
///
/// **Runs on a job.**
pub fn move_into(
    source: &Path,
    into: &Path,
    conflict: Conflict,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(crate::copy::Progress),
) -> Result<Change, Problem> {
    // **A move never renames.** [`Conflict::Rename`] is a copy's answer, and a
    // move that quietly renamed would leave two files under names neither of
    // them chose — so it is not offered for one, and reaching here with it
    // refuses the way a taken name always has.
    if conflict == Conflict::Replace
        && let Some(name) = source.file_name()
        && taken(&into.join(name))
    {
        let destination = into.join(name);
        let displaced = replace_into(name, into, &mut |staging| {
            crate::copy::move_file(source, staging, crate::copy::Options::default()).map(|_| ())
        })?;
        let moved = Change::Renamed { from: source.to_path_buf(), to: destination };
        return Ok(Change::Batch(vec![displaced, moved]));
    }
    let mut progress = crate::copy::Progress::default();
    // A move across filesystems is a copy and then a removal, so a resumed one
    // merges for the same reason a resumed copy does. Within one filesystem it
    // is a `rename`, which has nothing to resume.
    let options = crate::copy::Options {
        resume: conflict == Conflict::Resume,
        ..crate::copy::Options::default()
    };
    let destination = crate::copy::move_into(source, into, options, cancel, &mut progress, watch)?;
    Ok(Change::Renamed { from: source.to_path_buf(), to: destination })
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
    if crate::atomically(path, text) {
        return Ok(());
    }
    Err(Problem::new(
        ProblemKind::Unreadable,
        path.display().to_string(),
        "the file could not be written",
    ))
}

/// Replaces a text file's contents and returns the change that did it.
///
/// Reads what was there first: an undo has to put it back, and after the write
/// there is nowhere to read it from.
///
/// **Runs on a job.**
pub fn edit_text(path: &Path, after: &str) -> Result<Change, Problem> {
    let before = crate::read_text(path, crate::TEXT_LIMIT)?;
    write_text(path, after)?;
    Ok(Change::Edited { path: path.to_path_buf(), before, after: after.to_owned() })
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
/// first does the same with an extra step. Here a failure at any point leaves
/// the directory as it was.
fn replace_into(
    name: &std::ffi::OsStr,
    into: &Path,
    place: &mut dyn FnMut(&Path) -> Result<(), Problem>,
) -> Result<Change, Problem> {
    let destination = into.join(name);
    let Some(free) = crate::copy::free_name(into, name) else {
        return Err(Problem::new(
            ProblemKind::NameTaken,
            destination.display().to_string(),
            "no free name to stage a replacement under",
        ));
    };
    let staging = into.join(&free);
    let discard = || {
        // A fresh token: the staged copy is being removed *because* the
        // operation is over, and the one that stopped it is already cancelled.
        let _ = crate::copy::remove_tree(&staging, &sk_util::Cancel::new());
    };
    if let Err(problem) = place(&staging) {
        discard();
        return Err(problem);
    }
    let displaced = match trash(&destination) {
        Ok(change) => change,
        Err(problem) => {
            discard();
            return Err(problem);
        }
    };
    if let Err(problem) = move_no_replace(&staging, &destination) {
        // The name was freed one line ago, so this is very close to impossible —
        // and "very close to" is why what was trashed is put back rather than
        // left in the Trash with nothing on the undo stack pointing at it.
        let _ = revert(&displaced);
        discard();
        return Err(problem);
    }
    Ok(displaced)
}

/// Undoes a change. Refuses rather than guessing.
///
/// **Runs on a job.**
pub fn revert(change: &Change) -> Result<(), Problem> {
    match change {
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
        // `rmdir`, not a recursive remove: if something has been put in it since,
        // this fails with `ENOTEMPTY` and the undo refuses. An undo that deletes
        // files somebody added is not an undo.
        Change::Created { path } => rustix::fs::rmdir(path).map_err(|error| problem(error, path)),
        // Through the copy engine rather than `move_no_replace`: a rename inside
        // one directory can never cross a filesystem, but a *move* between two
        // can, and its undo has to be able to come back the same way it went.
        Change::Renamed { from, to } => {
            crate::copy::move_file(to, from, crate::copy::Options::default()).map(|_| ())
        }
        // Recursive, unlike `Created`'s `rmdir`: what the copy made is what is
        // being taken away, and all of it was made by the copy. Anything added to
        // it since is inside a directory this operation created, which is the one
        // case where a recursive remove is still an undo.
        Change::Copied { destination, .. } => {
            crate::copy::remove_tree(destination, &sk_util::Cancel::new())
        }
        // Straight back to what it held, which is the only way to undo bytes.
        Change::Edited { path, before, .. } => write_text(path, before).map(|_| ()),
        Change::Trashed { original, stored, info } => {
            move_no_replace(stored, original)?;
            // The file is back; a leftover `.trashinfo` is the corruption the
            // claim step exists to avoid, so its removal is not optional.
            std::fs::remove_file(info).map_err(|error| io_problem(&error, info))
        }
    }
}

/// Does a change again, after it was undone.
///
/// **Runs on a job.**
pub fn apply(change: &Change) -> Result<Change, Problem> {
    match change {
        Change::Batch(changes) => {
            changes.iter().map(apply).collect::<Result<_, _>>().map(Change::Batch)
        }
        // The bytes it held after the edit, put back after an undo took them away.
        Change::Edited { path, after, .. } => write_text(path, after).map(|()| change.clone()),
        Change::Created { path } => {
            let (Some(parent), Some(name)) =
                (path.parent(), path.file_name().and_then(|name| name.to_str()))
            else {
                return Err(Problem::new(
                    ProblemKind::BadName,
                    path.display().to_string(),
                    "no parent or no name",
                ));
            };
            create_directory(parent, name)
        }
        Change::Renamed { from, to } => {
            let Some(name) = to.file_name().and_then(|name| name.to_str()) else {
                return Err(Problem::new(
                    ProblemKind::BadName,
                    to.display().to_string(),
                    "no name",
                ));
            };
            rename(from, name)
        }
        // **To the recorded destination, not to one derived from the source.**
        // A copy into the folder its original is in lands under a free name, and
        // re-deriving would send the redo at the taken one — which fails, and
        // would have failed silently differently once the free name changed.
        Change::Copied { source, destination } => {
            crate::copy::copy_to(source, destination, &sk_util::Cancel::new(), &mut |_| {})?;
            Ok(Change::Copied { source: source.clone(), destination: destination.clone() })
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

/// `$XDG_DATA_HOME/Trash`, or the specification's fallback.
fn trash_home() -> Result<PathBuf, Problem> {
    if let Some(data) =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|path| path.is_absolute())
    {
        return Ok(data.join("Trash"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute());
    home.map(|home| home.join(".local/share/Trash")).ok_or_else(|| {
        Problem::plain(ProblemKind::Unreadable, "neither XDG_DATA_HOME nor HOME is set")
    })
}

/// Claims a free name in the trash, returning where the file and its record go.
///
/// The `info` file is created empty with `O_EXCL` — that is the atomic step, and
/// the specification names it as such. The name is then `name`, `name.2`,
/// `name.3` and so on until one is free.
fn claim(files: &Path, info: &Path, name: &str) -> Result<(PathBuf, PathBuf), Problem> {
    for attempt in 1..1000u32 {
        let candidate = if attempt == 1 { name.to_owned() } else { format!("{name}.{attempt}") };
        let record = info.join(format!("{candidate}.trashinfo"));
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        match rustix::fs::open(&record, flags, Mode::from_bits_truncate(0o600)) {
            Ok(_) => return Ok((files.join(candidate), record)),
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(problem(error, &record)),
        }
    }
    Err(Problem::new(ProblemKind::Unreadable, name, "a thousand names in the trash are taken"))
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
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) => {
            if rustix::fs::statat(cwd, to, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
                return Err(problem(rustix::io::Errno::EXIST, to));
            }
            rustix::fs::rename(from, to).map_err(|error| problem(error, to))
        }
        Err(error) => Err(problem(error, to)),
    }
}

/// The deletion time the specification asks for: local, to the second, no zone.
fn deletion_date() -> String {
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
fn encode_path(path: &str) -> String {
    const KEEP: &[u8] = b"-_.~/";
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn problem(error: rustix::io::Errno, path: &Path) -> Problem {
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
        Change, Conflict, apply, copy_into, create_directory, encode_path, move_into, rename,
        revert, trash,
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
        let root = scratch("copy-undo");
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

    #[test]
    fn a_directory_is_made_and_unmade() {
        let root = scratch("mkdir");
        let change = create_directory(&root, "notes").expect("made");
        assert_eq!(change, Change::Created { path: root.join("notes") });
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

    /// A trash on another filesystem needs the per-volume `.Trash-$uid` the
    /// specification describes, which is owed. Until then this must refuse and
    /// say so — never half-move anything.
    #[test]
    fn a_file_on_another_filesystem_is_refused_and_left_alone() {
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

            let refused = trash(&mine).expect_err("must refuse");
            assert_eq!(refused.kind(), ProblemKind::AcrossFilesystems);
            assert_eq!(std::fs::read(&mine).unwrap(), b"elsewhere", "the file was disturbed");
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

    /// The `Path=` field is read back by a restore, so the separators have to
    /// survive and everything else has to be encoded.
    #[test]
    fn a_trashinfo_path_keeps_its_separators_and_encodes_the_rest() {
        assert_eq!(encode_path("/home/emil/notes.txt"), "/home/emil/notes.txt");
        assert_eq!(encode_path("/home/emil/My File.txt"), "/home/emil/My%20File.txt");
        assert_eq!(encode_path("/home/emil/a#b?c"), "/home/emil/a%23b%3Fc");
        assert_eq!(encode_path("/home/emil/café"), "/home/emil/caf%C3%A9");
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
            Change::Created { path: folder.clone() },
            Change::Renamed { from: home.join("f"), to: folder.join("f") },
        ]);
        revert(&batch).expect("the batch undoes");
        assert_eq!(std::fs::read(home.join("f")).expect("f is back"), b"one");
        assert!(!folder.exists(), "the folder the batch made is still there");
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
}
