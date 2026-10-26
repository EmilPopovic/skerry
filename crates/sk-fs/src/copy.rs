// SPDX-License-Identifier: Apache-2.0
//! Copying one file: `FICLONE`, `copy_file_range` per extent, and a
//! hole-preserving read/write floor — each probed at runtime rather than chosen
//! by a `#[cfg]` ([ADR 0007](../../../docs/adr/0007-runtime-capability-detection.md)).
//!
//! **Ported from [Spike E](../../../tools/torture/README.md), which is the
//! permanent torture matrix and is a CI job.** Two of its four defects live in
//! this file as structure rather than as comments, and both would have shipped:
//!
//! * **Nothing is ever written to the destination name.** Every copy goes to a
//!   temporary file in the destination directory and is renamed into place at the
//!   end, so "no partial file left behind" is a property of the algorithm and not
//!   a promise about error handling. `ENOSPC` at the last byte leaves nothing.
//! * **`copy_file_range` is called per data extent.** One call over a whole file
//!   turned a 1 GiB sparse file with 12 KiB of data into a gigabyte of zeros, on
//!   ext4 *and* tmpfs, and it passes a byte-for-byte comparison.
//!
//! Publication requires an atomic no-replace primitive. Where `renameat2` is
//! unavailable, hardlink publication is tried; filesystems supporting neither
//! refuse the operation rather than risking an unrelated destination.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{
    self, AtFlags, Mode, OFlags, RenameFlags, SeekFrom, Stat, Timespec, Timestamps, fchmod, fstat,
    fsync, ftruncate, futimens, openat, renameat_with, seek, statat, unlinkat,
};
use rustix::io::Errno;
use sk_model::{Problem, ProblemKind};

/// How the bytes were moved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    /// `FICLONE`: the filesystem shares the extents and copies no data. 2.0 ms
    /// against 123.4 ms for 192 MiB on btrfs, and 0 KiB of free space consumed.
    Reflink,
    /// `copy_file_range`: the kernel moves the bytes with no round trip through
    /// user space, called once per data extent so holes survive.
    KernelCopy,
    /// The floor. Holes are preserved by seeking over them, which every
    /// filesystem implementing `SEEK_HOLE` supports.
    ReadWrite,
}

/// How the finished temporary file was given its final name.
///
/// Retained in the result for diagnostics and filesystem capability reporting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placement {
    /// `renameat2(RENAME_NOREPLACE)` — atomic, and refuses an existing
    /// destination in the same syscall.
    NoReplace,
    /// `link` then `unlink` — also atomic, because `link` refuses an existing
    /// destination, and it needs no flag a filesystem might not have.
    LinkThenUnlink,
    /// Legacy diagnostic value. Never emitted: check-then-rename can overwrite
    /// a concurrently created destination and is no longer a fallback.
    CheckThenRename,
    /// A replacement the caller asked for, which needs no protection.
    Replace,
}

/// What one finished copy did.
#[derive(Clone, Debug)]
pub struct Copied {
    /// Which strategy wrote the bytes.
    pub strategy: Strategy,
    /// How the file got its name.
    pub placement: Placement,
    /// Bytes written. Zero for a reflink, which writes none.
    pub bytes: u64,
    /// Metadata the destination filesystem would not accept, reported rather than
    /// dropped — [05-platform-integration.md](../../../docs/05-platform-integration.md)
    /// requires the report, and the distinction between *not preserved* and *not
    /// preservable* is drawn by trying rather than by a list of filesystem names.
    pub unpreserved: Vec<String>,
    /// A temporary file that survived a *successful* copy.
    ///
    /// Ignoring this is what left a `.skerry-copy-*` name on NTFS and made a
    /// three-name hardlink set report a link count of four.
    pub orphan: Option<PathBuf>,
    /// Every strategy that was tried and refused, with the raw errno it refused
    /// with.
    ///
    /// **The only way to tell "this filesystem cannot" from "we never asked".**
    /// A matrix row saying `read/write` means nothing on its own: it is the same
    /// answer whether `FICLONE` was refused with `EOPNOTSUPP` or never attempted
    /// because the caller turned it off. Raw `i32` rather than `rustix::Errno`,
    /// so a caller reading this needs no dependency the crate chose.
    pub refused: Vec<(Strategy, i32)>,
}

/// What a copy may do about metadata and an existing destination.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Whether an existing destination may be replaced.
    ///
    /// `false` uses `RENAME_NOREPLACE`, so the refusal is atomic rather than a
    /// check followed by a race.
    pub replace: bool,
    /// Whether extended attributes are carried over.
    pub xattrs: bool,
    /// Whether `FICLONE` may be tried.
    ///
    /// **Off is for measuring, not for policy.** The torture matrix in
    /// `tools/torture` forces the floor so it can compare a reflink
    /// against a true byte copy on the same filesystem, and so it can exercise
    /// `read/write` on a filesystem where the kernel would never have chosen it.
    /// Nothing in the product turns these off.
    pub reflink: bool,
    /// Whether `copy_file_range` may be tried. See [`Self::reflink`].
    pub kernel_copy: bool,
    /// Whether a destination that is already there is left alone.
    ///
    /// **What makes a stopped copy resumable**, which is
    /// [rule 7](../../../docs/03-principles.md#7-honest-progress)'s last clause.
    /// A copy writes each file to a staging name and renames it, so inside a
    /// half-copied tree every file is either whole or absent — there is no such
    /// thing as half a file to continue. Resuming is therefore not "seek to an
    /// offset" but "copy what is missing": a regular file whose destination
    /// exists at the same size is skipped, an existing symlink is skipped, and a
    /// directory that is already there is entered rather than refused.
    ///
    /// Compared by size and not by content. A byte-for-byte check would read
    /// both trees in full, which is the copy this exists to avoid doing twice,
    /// and a source that changed under a stopped copy is a source that changed —
    /// the mtime is the destination's own, set from the source when it was
    /// written, so a file that differs in size differs and one that does not is
    /// taken on trust.
    pub resume: bool,
    /// Whether the destination is flushed before the copy is reported done.
    ///
    /// Off in the product, because the page cache is the kernel's business and a
    /// file manager that fsyncs every file is one that copies at the speed of the
    /// disk queue. On for a *measurement*: a timing that ends before the bytes
    /// have left memory is a timing of memcpy.
    pub fsync: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            replace: false,
            resume: false,
            xattrs: true,
            reflink: true,
            kernel_copy: true,
            fsync: false,
        }
    }
}

/// Copies one regular file.
///
/// Symlinks, directories and hardlink sets are the caller's problem: the
/// decisions they need are policy and this is mechanism. A symlink, a fifo or a
/// device at `source` is refused rather than followed or opened.
///
/// **Runs on a job.**
pub fn copy_file(source: &Path, destination: &Path, options: Options) -> Result<Copied, Problem> {
    let (from, from_name) = parent_of(source)?;
    let (to, to_name) = parent_of(destination)?;
    copy_regular(
        At { dir: from.as_fd(), name: from_name, path: source },
        At { dir: to.as_fd(), name: to_name, path: destination },
        options,
        None,
        &mut |_| true,
    )
    .map(|(copied, _)| copied)
}

/// One entry, named relative to a directory this operation already holds open.
///
/// The path is for messages and nothing else: no syscall below resolves it.
#[derive(Clone, Copy)]
struct At<'a> {
    dir: BorrowedFd<'a>,
    name: &'a OsStr,
    path: &'a Path,
}

impl At<'_> {
    fn sibling(&self, name: &OsStr) -> PathBuf {
        self.path.parent().map_or_else(|| PathBuf::from(name), |parent| parent.join(name))
    }
}

/// The directory `path` is in, held open, and the name `path` has there.
///
/// **The one place a tree operation resolves a path.** Everything below the
/// entry it names is reached through a descriptor and `O_NOFOLLOW`, so a
/// directory renamed away and replaced with a symlink half way through a walk
/// redirects nothing: the walk is still holding the directory it opened. What
/// is above the entry is the caller's choice of path, symlinks and all, as it
/// is for `cp`. `O_PATH`, because the parent is only ever a `dirfd` — a folder
/// somebody may search but not list is still one they can copy out of.
fn parent_of(path: &Path) -> Result<(OwnedFd, &OsStr), Problem> {
    let name = path.file_name().ok_or_else(|| at(Errno::INVAL, path, "no name"))?;
    let parent =
        path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let fd =
        openat(fs::CWD, parent, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|error| at(error, parent, "open parent"))?;
    Ok((fd, name))
}

/// Opens the regular file `from` names for reading, refusing anything else.
///
/// `O_NOFOLLOW`, so a name swapped for a symlink fails with `ELOOP` instead of
/// opening whatever it points at. `O_NONBLOCK`, so a name swapped for a fifo
/// answers at once instead of waiting forever for a writer — the descriptor's
/// own `fstat` then refuses it. Where the walk has already `lstat`ed the name,
/// the descriptor has to be that inode, which is the check coreutils' `cp`
/// makes before it reads a byte ("skipping file …, as it was replaced while
/// being copied").
fn open_regular(from: At<'_>, seen: Option<&Stat>) -> Result<(OwnedFd, Stat), Problem> {
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let fd = openat(from.dir, from.name, flags, Mode::empty())
        .map_err(|error| at(error, from.path, "open"))?;
    let stat = fstat(&fd).map_err(|error| at(error, from.path, "stat"))?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::RegularFile
        || seen.is_some_and(|seen| !same_file(seen, &stat))
    {
        return Err(changed(from.path));
    }
    // A regular file ignores `O_NONBLOCK` on every local filesystem, but FUSE
    // and NFS hand the open flags to a server, and the flag was only ever
    // wanted for the open.
    fs::fcntl_setfl(&fd, OFlags::empty()).map_err(|error| at(error, from.path, "open"))?;
    Ok((fd, stat))
}

/// Copies the regular file `from` names to `to`, and returns the source's
/// `fstat` alongside the result.
fn copy_regular(
    from: At<'_>,
    to: At<'_>,
    options: Options,
    seen: Option<&Stat>,
    checkpoint: &mut dyn FnMut(u64) -> bool,
) -> Result<(Copied, Stat), Problem> {
    let (source, stat) = open_regular(from, seen)?;

    // The classic truncation bug: `cp a a` that opens the destination for writing
    // empties the file before it reads anything. Comparing device and inode
    // catches it through a symlink, a hardlink and a bind mount alike, where a
    // path comparison does not.
    if let Ok(there) = statat(to.dir, to.name, AtFlags::empty()) {
        if same_file(&stat, &there) {
            return Err(at(Errno::INVAL, to.path, "the same file"));
        }
        if !options.replace {
            return Err(at(Errno::EXIST, to.path, "place"));
        }
    }

    let temp = temporary();
    // `O_EXCL`: a name that already exists means a collision with another
    // operation, and that file must not be touched.
    let into = openat(
        to.dir,
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| at(error, to.path, "create"))?;

    let size = stat.st_size.cast_unsigned();
    let mut refused = Vec::new();
    let (strategy, bytes) =
        match write_contents(&source, &into, size, options, &mut refused, checkpoint) {
            Ok(written) => written,
            Err((error, stage)) => return Err(discard(to, &temp, error, stage)),
        };

    let mut unpreserved = Vec::new();
    let theirs = foreign(&stat);
    if fchmod(&into, carried_mode(&source, &stat, theirs, &into)).is_err() {
        unpreserved.push("mode".to_owned());
    }
    if options.xattrs {
        copy_xattrs(&source, &into, &mut unpreserved, !theirs);
    }
    // Timestamps last. Every write above moves the modification time, so setting
    // it earlier sets it to a value the next write overwrites — which is what
    // makes every "is the copy newer than the original" comparison wrong.
    if futimens(&into, &times_of(&stat)).is_err() {
        unpreserved.push("times".to_owned());
    }
    // Before the placement, so what is renamed into place is what is on the
    // disk. A flush after the rename would be a flush of a name that already
    // points at bytes the caller has been told are safe.
    if options.fsync && fsync(&into).is_err() {
        unpreserved.push("fsync".to_owned());
    }
    drop(into);

    let (placement, orphan) =
        place(to, &temp, options.replace).map_err(|error| discard(to, &temp, error, "place"))?;
    let orphan = orphan.then(|| to.sibling(&temp));
    Ok((Copied { strategy, placement, bytes, unpreserved, orphan, refused }, stat))
}

/// Moves one file, which is two entirely different operations behind one word.
///
/// Within a filesystem it is a rename: atomic, instant, and it cannot lose
/// anything. Across filesystems it is a copy followed by an unlink, and **the
/// source is removed only after the copy has been placed**, and only while it
/// is still what was copied — a file written to after it was read keeps its
/// name, and the move says so. A move that unlinks first, or that trusts a
/// return value it did not check, is the classic way a file manager destroys a
/// file.
///
/// **Runs on a job.**
pub fn move_file(
    source: &Path,
    destination: &Path,
    options: Options,
) -> Result<Placement, Problem> {
    let cwd = fs::CWD;
    match place_existing((cwd, source), (cwd, destination), options.replace) {
        Ok(placement) => {
            finish_move(source, placement)?;
            return Ok(placement);
        }
        // Different filesystems. Every other error is the caller's answer.
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, destination, "rename")),
    }
    let (from, from_name) = parent_of(source)?;
    let (to, to_name) = parent_of(destination)?;
    let (copied, read) = copy_regular(
        At { dir: from.as_fd(), name: from_name, path: source },
        At { dir: to.as_fd(), name: to_name, path: destination },
        options,
        None,
        &mut |_| true,
    )?;
    if !copied.unpreserved.is_empty() || copied.orphan.is_some() {
        return Err(Problem::new(
            ProblemKind::Preservation,
            destination.display().to_string(),
            format!("{:?}", copied.unpreserved),
        ));
    }
    let left = || {
        Problem::counted(
            ProblemKind::LeftBehind,
            source.display().to_string(),
            1,
            "changed after it was copied",
        )
    };
    let now = statat(&from, from_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| at(error, source, "stat"))?;
    if !unchanged(&read, &now) {
        return Err(left());
    }
    match unlink_unchanged(from.as_fd(), from_name, &now, false)
        .map_err(|error| at(error, source, "unlink"))?
    {
        Unlinked::Removed | Unlinked::Gone => Ok(copied.placement),
        Unlinked::Kept(_) => Err(left()),
    }
}

/// Removes one file or one empty directory, permanently.
///
/// **Runs on a job.**
pub fn remove(path: &Path) -> Result<(), Problem> {
    let cwd = fs::CWD;
    let stat = statat(cwd, path, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, path, "stat"))?;
    let flags = if fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::Directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    unlinkat(cwd, path, flags).map_err(|error| at(error, path, "remove"))
}

/// How far a tree operation has got, for a caller drawing progress.
///
/// Counted rather than estimated: [rule 7](../../../docs/03-principles.md) is that
/// progress is honest, and a bar derived from a guess about how long a copy takes
/// is the thing that rule exists to forbid. The total is not known until the walk
/// finds the last file, so a caller shows what has been done and not a fraction of
/// something it made up.
#[derive(Clone, Copy, Default, Debug)]
pub struct Progress {
    /// Files, symlinks and directories finished.
    pub done: u64,
    /// Logical source bytes covered, including holes and reflinked contents.
    pub bytes: u64,
}

/// How deep a tree may nest before the operation refuses.
///
/// `PATH_MAX` bounds a real tree at about two thousand levels, and a bind mount or
/// a symlink followed by accident can make an unbounded one. Refusing names the
/// path it stopped at, where recursing until the stack runs out does not.
const MAX_DEPTH: u32 = 128;

/// Copies `source` into the directory `into`, whatever kind of thing it is.
///
/// Returns where it landed. A directory is copied recursively; a symlink is
/// recreated as a symlink rather than followed, because following one turns a
/// 40-byte link into a copy of whatever it pointed at — and a link into a
/// directory being copied turns it into an infinite one.
///
/// **Runs on a job**, and stops where `cancel` says to. A cancelled copy leaves
/// what it has already written: unwinding it would be a delete nobody asked for,
/// and the caller has the undo stack for that.
pub fn copy_into(
    source: &Path,
    into: &Path,
    options: Options,
    cancel: &sk_util::Cancel,
    progress: &mut Progress,
    watch: &mut dyn FnMut(Progress),
) -> Result<PathBuf, Problem> {
    let Some(name) = source.file_name() else {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "no name to copy it under",
        ));
    };
    let destination = into.join(name);
    // A directory cannot be copied into itself or into anything below itself: the
    // walk would find the copy it is making and copy that too, forever.
    if into == source || into.starts_with(source) {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "a folder cannot be copied into itself",
        ));
    }
    Walk::copying(options, cancel, progress, watch).copy(source, &destination)?;
    Ok(destination)
}

/// Copies `source` to exactly `destination`.
///
/// The one entry point that does not derive the name from the source, for the
/// caller that has already chosen one — a duplicate, or a copy into the folder
/// the original is in.
///
/// **Runs on a job.**
pub fn copy_to(
    source: &Path,
    destination: &Path,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(Progress),
) -> Result<(), Problem> {
    copy_to_options(source, destination, Options::default(), cancel, watch)
}

pub(crate) fn copy_to_options(
    source: &Path,
    destination: &Path,
    options: Options,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(Progress),
) -> Result<(), Problem> {
    if let Some(into) = destination.parent()
        && (into == source || into.starts_with(source))
    {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "a folder cannot be copied into itself",
        ));
    }
    let mut progress = Progress::default();
    Walk::copying(options, cancel, &mut progress, watch).copy(source, destination)
}

/// A name nothing in `into` is called, from `name`.
///
/// `report.pdf` becomes `report (copy).pdf`, then `report (copy 2).pdf`. The
/// extension is kept because it is what decides how a file opens, and a
/// `report.pdf (copy)` is a file the desktop no longer knows what to do with.
///
/// **`copy` is not translated.** Every other string a person reads goes through
/// the catalogue; this one becomes a *filename*, and a name that depends on the
/// locale of the session that made it is a name that sorts differently, scripts
/// differently and cannot be predicted from another machine.
///
/// `None` where a hundred candidates are all taken, which is a directory doing
/// something stranger than this feature is for.
///
/// **Runs on a job**: one `statx` per candidate.
#[must_use]
pub fn free_name(into: &Path, name: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt;

    if statat(fs::CWD, into.join(name), AtFlags::SYMLINK_NOFOLLOW).is_err() {
        return Some(name.to_owned());
    }
    // Split on the *last* dot that is not the first character, so `.bashrc` is a
    // stem and `archive.tar.gz` keeps only `.gz` — which is what a rename field
    // in this program already does with the same reasoning.
    let bytes = name.as_bytes();
    let dot = bytes.iter().rposition(|byte| *byte == b'.').filter(|at| *at > 0);
    let (stem, extension) = dot.map_or((bytes, &[][..]), |at| (&bytes[..at], &bytes[at..]));

    for attempt in 1..100 {
        let mut candidate = OsString::from(std::ffi::OsStr::from_bytes(stem));
        candidate.push(if attempt == 1 {
            " (copy)".to_owned()
        } else {
            format!(" (copy {attempt})")
        });
        candidate.push(std::ffi::OsStr::from_bytes(extension));
        if statat(fs::CWD, into.join(&candidate), AtFlags::SYMLINK_NOFOLLOW).is_err() {
            return Some(candidate);
        }
    }
    None
}

/// Moves `source` into the directory `into`.
///
/// A rename where the filesystems match, and a copy followed by a removal where
/// they do not — with the removal strictly after the copy, per [`move_file`].
///
/// Returns where it landed and, beside it, what the removal left in the source
/// on purpose — whatever changed or appeared there after the copy read it, and
/// any mount inside it. The move has happened either way.
///
/// **Runs on a job.**
pub fn move_into(
    source: &Path,
    into: &Path,
    options: Options,
    cancel: &sk_util::Cancel,
    progress: &mut Progress,
    watch: &mut dyn FnMut(Progress),
) -> Result<(PathBuf, Option<Problem>), Problem> {
    let Some(name) = source.file_name() else {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "no name to move it under",
        ));
    };
    let destination = into.join(name);
    if into == source || into.starts_with(source) {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "a folder cannot be moved into itself",
        ));
    }
    match place_existing((fs::CWD, source), (fs::CWD, &destination), options.replace) {
        Ok(placement) => {
            finish_move(source, placement)?;
            progress.done += 1;
            return Ok((destination, None));
        }
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, &destination, "rename")),
    }
    let left = across(source, &destination, options, false, cancel, progress, watch)?;
    Ok((destination, left))
}

/// Moves a file or directory to the exact recorded destination.
///
/// `Some` beside success for what the removal left in the source on purpose,
/// as [`move_into`] returns it.
///
/// **Between filesystems it merges into a folder already at `destination`**
/// rather than refusing at its name. That is how an undo comes back after a
/// move that left something behind: the leftovers hold the original folder's
/// name, and refusing there — "Something is already called …; choose another
/// name", with no name to choose — left a history entry nobody could walk back.
/// A folder already there is entered, nothing in it is replaced, and an entry
/// whose name is taken stays where it was and is reported.
pub fn move_to(
    source: &Path,
    destination: &Path,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(Progress),
) -> Result<Option<Problem>, Problem> {
    match place_existing((fs::CWD, source), (fs::CWD, destination), false) {
        Ok(placement) => return finish_move(source, placement).map(|()| None),
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, destination, "rename")),
    }
    if let Some(into) = destination.parent()
        && (into == source || into.starts_with(source))
    {
        return Err(Problem::new(
            ProblemKind::BadName,
            source.display().to_string(),
            "a folder cannot be moved into itself",
        ));
    }
    let mut progress = Progress::default();
    across(source, destination, Options::default(), true, cancel, &mut progress, watch)
}

/// A move between two filesystems: a copy, and then the removal of exactly what
/// it took — see [`remove_taken`]. `merge` is [`move_to`]'s.
fn across(
    source: &Path,
    destination: &Path,
    options: Options,
    merge: bool,
    cancel: &sk_util::Cancel,
    progress: &mut Progress,
    watch: &mut dyn FnMut(Progress),
) -> Result<Option<Problem>, Problem> {
    let mut taken = Taken::default();
    let mut walk = Walk {
        options,
        merge,
        held: Left::default(),
        merged: None,
        cancel,
        progress,
        watch,
        taken: Some(&mut taken),
    };
    walk.copy(source, destination)?;
    let (held, merged) = (std::mem::take(&mut walk.held), walk.merged.take());
    // Only now, and only because the copy above returned `Ok`.
    let left = remove_taken(source, &taken, cancel)?;
    Ok(left.problem(source, &held).or_else(|| {
        merged.map(|folder| {
            Problem::new(
                ProblemKind::MergedInto,
                folder.display().to_string(),
                "merged into a folder already there",
            )
        })
    }))
}

fn finish_move(source: &Path, placement: Placement) -> Result<(), Problem> {
    if placement == Placement::LinkThenUnlink {
        unlinkat(fs::CWD, source, AtFlags::empty())
            .map_err(|error| at(error, source, "unlink moved source"))?;
    }
    Ok(())
}

/// Removes a file, a symlink or a whole directory tree, permanently.
///
/// **Runs on a job.**
pub fn remove_tree(path: &Path, cancel: &sk_util::Cancel) -> Result<(), Problem> {
    let (parent, name) = parent_of(path)?;
    remove_at(&parent, Path::new(name), path, cancel)
}

/// Removes `name` under the open directory `parent`, and everything below it.
///
/// For a caller that already holds the directory and has checked it, which is
/// what Empty Trash does: resolving `parent`'s path again would be a second
/// answer to a question already answered.
pub(crate) fn remove_at(
    parent: &impl AsFd,
    name: &Path,
    subject: &Path,
    cancel: &sk_util::Cancel,
) -> Result<(), Problem> {
    let device = fstat(parent).map_err(|e| at(e, subject, "stat parent"))?.st_dev;
    let mut removal = Removal { cancel, only: None, left: Left::default() };
    removal.entry(parent.as_fd(), name, subject, device, 0)?;
    removal.left.problem(subject, &Left::default()).map_or(Ok(()), Err)
}

/// Removes from `source` what a move copied out of it, and nothing else.
///
/// **A move across filesystems is a copy and then a removal, and the source is
/// not still between the two.** Removing the whole tree deleted whatever had
/// arrived in it while the copy ran, and whatever had been written to since it
/// was read — neither of which the copy holds. So an entry goes only where its
/// inode is one the copy took, a file only while it is the version the copy
/// read ([`Version`]), and a directory only once it is empty — each checked and
/// removed under a name of this module's own ([`unlink_unchanged`]). What
/// stays is returned, to report beside a move that otherwise happened.
fn remove_taken(source: &Path, taken: &Taken, cancel: &sk_util::Cancel) -> Result<Left, Problem> {
    let (parent, name) = parent_of(source)?;
    let device = fstat(&parent).map_err(|e| at(e, source, "stat parent"))?.st_dev;
    let mut removal = Removal { cancel, only: Some(taken), left: Left::default() };
    removal.entry(parent.as_fd(), Path::new(name), source, device, 0)?;
    Ok(removal.left)
}

/// One removal's worth of state, carried down the recursion.
struct Removal<'a> {
    cancel: &'a sk_util::Cancel,
    /// For a move, what the copy took; `None` for a delete, which takes all.
    only: Option<&'a Taken>,
    left: Left,
}

impl Removal<'_> {
    /// One entry, whatever kind it is, in a directory on `device`.
    fn entry(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &Path,
        subject: &Path,
        device: u64,
        depth: u32,
    ) -> Result<(), Problem> {
        if self.cancel.is_cancelled() {
            return Err(cancelled(subject));
        }
        if depth > MAX_DEPTH {
            return Err(too_deep(subject));
        }
        let stat =
            statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, subject, "stat"))?;
        if self.only.is_some_and(|taken| !taken.holds(&stat)) {
            self.left.kept(subject);
            return Ok(());
        }
        if is_directory(&stat) {
            return self.directory(parent, name, subject, depth, &stat, device);
        }
        if self.only.is_none() {
            return unlinkat(parent, name, AtFlags::empty()).map_err(|e| at(e, subject, "remove"));
        }
        match unlink_unchanged(parent, name.as_os_str(), &stat, false) {
            Ok(Unlinked::Removed | Unlinked::Gone) => Ok(()),
            Ok(Unlinked::Kept(under)) => {
                self.left.kept(&subject.with_file_name(under));
                Ok(())
            }
            Err(error) => Err(at(error, subject, "remove")),
        }
    }

    /// A directory and what is in it, unless it is another filesystem's.
    ///
    /// **A mount inside the tree is left standing, with everything on it.**
    /// Nothing can remove the mount point while it is mounted, so descending
    /// into it deletes the contents of a bind mount, a FUSE mount or a disk and
    /// then fails anyway. `STATX_ATTR_MOUNT_ROOT` is the test, and it sees a
    /// bind mount of a folder on the same filesystem; `st_dev` is the fallback
    /// only where the kernel does not report the attribute, because a btrfs
    /// subvolume has its own `st_dev` without being mounted — and deleting one
    /// that is not mounted is deleting a folder.
    fn directory(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &Path,
        subject: &Path,
        depth: u32,
        observed: &Stat,
        device: u64,
    ) -> Result<(), Problem> {
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| at(e, subject, "open directory"))?;
        let opened = fstat(&fd).map_err(|e| at(e, subject, "stat directory"))?;
        if !same_file(observed, &opened) {
            return Err(at(Errno::BUSY, subject, "directory changed"));
        }
        if mount_root(&fd).unwrap_or(opened.st_dev != device) {
            self.left.mount(subject);
            return Ok(());
        }
        // **`unlinkat` needs write and search on the directory, not on the file**
        // (unlink(2), `EACCES`), so one `dr-x------` folder from a tarball made
        // every Empty Trash stop at its first file. Being removed for good, it is
        // given them back — through the descriptor just checked, never by name.
        // Where we do not own it this fails, and the `EACCES` below says why.
        let locked = opened.st_mode & 0o300 != 0o300;
        let relock = || {
            if locked {
                let _ = fchmod(&fd, Mode::from_raw_mode(opened.st_mode & 0o7777));
            }
        };
        if locked {
            let _ = fchmod(&fd, Mode::from_raw_mode((opened.st_mode & 0o7777) | 0o700));
        }
        let before = self.left.count;
        // Listed whole first: a move renames entries within this directory while
        // it removes them, and a `getdents64` cursor over that may skip or repeat.
        for child in names(&fd, subject)? {
            let child = Path::new(&child);
            self.entry(fd.as_fd(), child, &subject.join(child), opened.st_dev, depth + 1)?;
        }
        // Something below stays, so this cannot go either — and it stays as it
        // was found rather than with the permissions it was given to be emptied.
        if self.left.count > before {
            relock();
            return Ok(());
        }
        if self.only.is_some() {
            return match unlink_unchanged(parent, name.as_os_str(), &opened, true) {
                Ok(Unlinked::Removed | Unlinked::Gone) => Ok(()),
                Ok(Unlinked::Kept(under)) => {
                    relock();
                    self.left.kept(&subject.with_file_name(under));
                    Ok(())
                }
                Err(error) => {
                    relock();
                    Err(at(error, subject, "remove directory"))
                }
            };
        }
        let current =
            statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, subject, "stat"))?;
        if !same_file(&opened, &current) {
            return Err(at(Errno::BUSY, subject, "directory changed"));
        }
        unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| at(e, subject, "remove directory"))
    }
}

/// What became of an entry that was to be removed only if it was unchanged.
enum Unlinked {
    Removed,
    /// Nothing held the name any more: somebody else removed or moved it.
    Gone,
    /// Not what was copied, so still there — under this name, which is its own
    /// unless something took that while it was being checked.
    Kept(OsString),
}

/// Removes what `name` holds only while it is the entry `seen` described.
///
/// **Through a quarantine name.** A check by name and an unlink by name are two
/// lookups, and a new version renamed over the name between them was the one
/// deleted: 17,282 of 60,000 in the review's race. Renaming whatever holds the
/// name *now* to a fresh name of this module's own is one atomic step, so what
/// is checked next is exactly what is unlinked; if it is not what the copy
/// took, it goes back. The `ctime` in [`Version`] cannot be compared after the
/// rename, which moves it, so the caller has compared it already and this
/// compares the inode, the size and the modification time again.
fn unlink_unchanged(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    seen: &Stat,
    directory: bool,
) -> Result<Unlinked, Errno> {
    let Some(held) = quarantine(dir, name)? else {
        return Ok(Unlinked::Gone);
    };
    let unchanged = statat(dir, &held, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|now| {
        same_file(seen, &now) && (directory || Version::of(seen).untouched(&Version::of(&now)))
    });
    if !unchanged {
        return Ok(put_back(dir, &held, name));
    }
    let flags = if directory { AtFlags::REMOVEDIR } else { AtFlags::empty() };
    match unlinkat(dir, &held, flags) {
        Ok(()) => Ok(Unlinked::Removed),
        // A folder that gained an entry after it was listed.
        Err(Errno::NOTEMPTY | Errno::EXIST) if directory => Ok(put_back(dir, &held, name)),
        Err(error) => {
            put_back(dir, &held, name);
            Err(error)
        }
    }
}

/// Renames whatever `name` holds now to a fresh name in the same directory —
/// `None` where nothing holds it any more.
fn quarantine(dir: BorrowedFd<'_>, name: &OsStr) -> Result<Option<OsString>, Errno> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = rustix::process::getpid().as_raw_nonzero();
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let held = OsString::from(format!(".skerry-moving-{pid}-{n}"));
        match renameat_with(dir, name, dir, &held, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(Some(held)),
            Err(Errno::EXIST) => {}
            Err(Errno::NOENT) => return Ok(None),
            // The flag is unsupported — NFS and CIFS refuse every rename flag.
            // The quarantine name is this module's own, so a plain rename onto
            // one seen a moment ago not to exist risks nothing anybody chose.
            Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
                match statat(dir, &held, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(_) => {}
                    Err(Errno::NOENT) => {
                        return match renameat_with(dir, name, dir, &held, RenameFlags::empty()) {
                            Ok(()) => Ok(Some(held)),
                            Err(Errno::NOENT) => Ok(None),
                            Err(error) => Err(error),
                        };
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Gives a quarantined entry its name back, never over something that has
/// taken the name since — where neither `RENAME_NOREPLACE` nor a hardlink can
/// promise that, it keeps the quarantine name and the report says which.
fn put_back(dir: BorrowedFd<'_>, held: &OsStr, name: &OsStr) -> Unlinked {
    let back = match renameat_with(dir, held, dir, name, RenameFlags::NOREPLACE) {
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
            fs::linkat(dir, held, dir, name, AtFlags::empty())
                .and_then(|()| unlinkat(dir, held, AtFlags::empty()))
        }
        other => other,
    };
    Unlinked::Kept(if back.is_ok() { name } else { held }.to_owned())
}

/// Whether `directory` is the root of a mount, or `None` where the kernel does
/// not say.
///
/// `STATX_ATTR_MOUNT_ROOT` is Linux 5.8, under the 5.10 floor; a filesystem
/// whose `statx` leaves it out of the attribute mask, or a filter that refuses
/// `statx`, gets the `st_dev` comparison instead.
fn mount_root(directory: &impl AsFd) -> Option<bool> {
    let stat = fs::statx(directory, "", AtFlags::EMPTY_PATH, fs::StatxFlags::empty()).ok()?;
    stat.stx_attributes_mask
        .contains(fs::StatxAttributes::MOUNT_ROOT)
        .then(|| stat.stx_attributes.contains(fs::StatxAttributes::MOUNT_ROOT))
}

/// Every source entry a move copied, by inode, with what it was when read.
///
/// **64 bytes an entry at 100,000 entries and 103 at a million** — 6.4 MB and
/// 103 MB held, 9.6 MB and 154 MB at the peak of the map's last growth —
/// measured with a counting allocator around this key and a 24-byte value; the
/// change time made the value 40 bytes, which this has not been measured at.
/// Held only while a move between filesystems runs, and the price of not
/// deleting what the copy never saw.
#[derive(Default)]
struct Taken(std::collections::HashMap<(u64, u64), Option<Version>>);

/// A non-directory as the copy read it.
///
/// **Modification time and size, and the change time where the file has one
/// name.** A write moves `mtime` — unless it is put back, as `touch -r`,
/// `rsync -t` and an SFTP upload all do, which only `ctime` sees. But unlinking
/// one name of a hardlink set moves the inode's `ctime` too, so with more than
/// one name it would leave every second name behind; there it is not compared.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Version {
    mtime: (i64, u64),
    size: i64,
    ctime: Option<(i64, u64)>,
}

impl Version {
    fn of(stat: &Stat) -> Self {
        Self {
            mtime: (stat.st_mtime as _, stat.st_mtime_nsec as _),
            size: stat.st_size,
            ctime: (stat.st_nlink == 1).then_some((stat.st_ctime as _, stat.st_ctime_nsec as _)),
        }
    }

    /// Whether `now` is this version, unwritten: the change time is compared
    /// only where it was recorded, and then the file must still have one name.
    fn matches(&self, now: &Stat) -> bool {
        let now = Self::of(now);
        self.untouched(&now) && self.ctime.is_none_or(|ctime| now.ctime == Some(ctime))
    }

    /// The same, without the change time — for after a rename, which moves it.
    fn untouched(&self, now: &Self) -> bool {
        self.mtime == now.mtime && self.size == now.size
    }
}

impl Taken {
    fn record(&mut self, stat: &Stat) {
        let version = (!is_directory(stat)).then(|| Version::of(stat));
        self.0.insert((stat.st_dev as _, stat.st_ino as _), version);
    }

    /// Whether what `stat` describes is still what the copy took.
    fn holds(&self, stat: &Stat) -> bool {
        match self.0.get(&(stat.st_dev as _, stat.st_ino as _)) {
            Some(Some(version)) => !is_directory(stat) && version.matches(stat),
            Some(None) => is_directory(stat),
            None => false,
        }
    }
}

/// Whether `now` is the file `read` described, unwritten since.
fn unchanged(read: &Stat, now: &Stat) -> bool {
    same_file(read, now) && Version::of(read).matches(now)
}

fn is_directory(stat: &Stat) -> bool {
    fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::Directory
}

/// What a removal left standing on purpose, or a move back found its name taken
/// for.
#[derive(Default)]
struct Left {
    count: usize,
    /// The first few, for the log. Bounded, because a tree can leave a million.
    first: Vec<PathBuf>,
    /// The first mount point, which is what the message names when there is one.
    mount: Option<PathBuf>,
}

impl Left {
    const NAMED: usize = 8;

    fn mount(&mut self, path: &Path) {
        self.mount.get_or_insert_with(|| path.to_path_buf());
        self.kept(path);
    }

    fn kept(&mut self, path: &Path) {
        self.count += 1;
        if self.first.len() < Self::NAMED {
            self.first.push(path.to_path_buf());
        }
    }

    /// The problem to report about what stayed in `root`, if anything did.
    ///
    /// A mount wins the message, because its advice — unmount it — is the one
    /// that helps; then what a move back found its name taken for (`held`), whose
    /// advice is to compare; then the rest, which changed or arrived.
    fn problem(&self, root: &Path, held: &Left) -> Option<Problem> {
        let first = self.first.first()?;
        let named: Vec<String> = self.first.iter().map(|path| path.display().to_string()).collect();
        let detail =
            format!("{} left standing in {}: {}", self.count, root.display(), named.join(", "));
        let count = |left: &Left| u64::try_from(left.count).unwrap_or(u64::MAX);
        Some(match (&self.mount, held.first.first()) {
            (Some(mount), _) => {
                Problem::new(ProblemKind::MountedInside, mount.display().to_string(), detail)
            }
            (None, Some(clash)) => Problem::counted(
                ProblemKind::NameHeld,
                clash.display().to_string(),
                count(held),
                detail,
            ),
            (None, None) => Problem::counted(
                ProblemKind::LeftBehind,
                first.display().to_string(),
                count(self),
                detail,
            ),
        })
    }
}

/// One tree operation's worth of state, carried down the recursion.
struct Walk<'a> {
    options: Options,
    cancel: &'a sk_util::Cancel,
    progress: &'a mut Progress,
    watch: &'a mut dyn FnMut(Progress),
    /// For a move, every source entry as it was when copied — what
    /// [`remove_taken`] may remove afterwards. `None` for a copy.
    taken: Option<&'a mut Taken>,
    /// [`move_to`]'s merge: enter a folder already at the destination, and
    /// leave in the source whatever finds its name taken there, in `held`.
    merge: bool,
    held: Left,
    /// The first folder a merge entered rather than made, which a move back
    /// reports even where nothing clashed in it.
    merged: Option<PathBuf>,
}

impl<'a> Walk<'a> {
    fn copying(
        options: Options,
        cancel: &'a sk_util::Cancel,
        progress: &'a mut Progress,
        watch: &'a mut dyn FnMut(Progress),
    ) -> Self {
        Self {
            options,
            cancel,
            progress,
            watch,
            taken: None,
            merge: false,
            held: Left::default(),
            merged: None,
        }
    }

    /// Copies `source` to `destination`, which must not exist unless the options
    /// say what to do about it.
    fn copy(&mut self, source: &Path, destination: &Path) -> Result<(), Problem> {
        let (from, from_name) = parent_of(source)?;
        let (to, to_name) = parent_of(destination)?;
        self.any(
            At { dir: from.as_fd(), name: from_name, path: source },
            At { dir: to.as_fd(), name: to_name, path: destination },
            0,
        )
    }

    /// One entry, whatever kind it is.
    fn any(&mut self, from: At<'_>, to: At<'_>, depth: u32) -> Result<(), Problem> {
        if self.cancel.is_cancelled() {
            return Err(cancelled(from.path));
        }
        if depth > MAX_DEPTH {
            return Err(too_deep(from.path));
        }
        let stat = statat(from.dir, from.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| at(e, from.path, "stat"))?;
        match fs::FileType::from_raw_mode(stat.st_mode) {
            fs::FileType::RegularFile => self.file(from, to, &stat),
            fs::FileType::Symlink => self.link(from, to, &stat),
            fs::FileType::Directory => self.directory(from, to, depth, &stat),
            // A fifo, a socket or a device. Recreating one needs `mknod` and, for
            // a device, privilege this process does not have — and skipping it
            // silently is how a copy reports success while leaving something out.
            _ => Err(Problem::new(
                ProblemKind::NotCopyable,
                from.path.display().to_string(),
                "not a file, a folder or a link",
            )),
        }
    }

    fn file(&mut self, from: At<'_>, to: At<'_>, stat: &Stat) -> Result<(), Problem> {
        // Already there and the same size: what a stopped copy got to. Counted
        // as done, because it is — the progress line is about the work the
        // queue was given rather than about this attempt at it.
        //
        // **A move takes it only where the bytes are the same.** Size is the
        // copy's test, and a cheap one, because a copy leaves its source where
        // it was. A move then removes what it took, and a source edited to the
        // same length after the stop was deleted for a stale copy of it — so a
        // move reads both, which is the price of removing nothing unverified.
        if self.options.resume && same_size(to, stat.st_size) {
            if self.taken.is_some()
                && let Some(read) = same_bytes(from, to, stat)
            {
                self.took(&read);
            }
            self.finished(stat.st_size);
            return Ok(());
        }
        let base = *self.progress;
        let (watch, cancel) = (&mut *self.watch, self.cancel);
        let copied = copy_regular(from, to, self.options, Some(stat), &mut |bytes| {
            watch(Progress { bytes: base.bytes + bytes, ..base });
            !cancel.is_cancelled()
        });
        let (copied, read) = match copied {
            Err(problem) if self.merge && problem.kind() == ProblemKind::NameTaken => {
                self.held.kept(from.path);
                self.finished(stat.st_size);
                return Ok(());
            }
            copied => copied?,
        };
        self.took(&read);
        self.finished(stat.st_size);
        if !copied.unpreserved.is_empty() || copied.orphan.is_some() {
            return Err(Problem::new(
                ProblemKind::Preservation,
                to.path.display().to_string(),
                format!("not preserved: {:?}; leftover: {:?}", copied.unpreserved, copied.orphan),
            ));
        }
        Ok(())
    }

    /// Recreated rather than followed. A link is 40 bytes and what it points at
    /// may be a terabyte, or may be the directory this walk is inside.
    fn link(&mut self, from: At<'_>, to: At<'_>, stat: &Stat) -> Result<(), Problem> {
        let target = fs::readlinkat(from.dir, from.name, Vec::new())
            .map_err(|error| at(error, from.path, "readlink"))?;
        if self.options.resume && statat(to.dir, to.name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            // Taken, for a move, only where it is the same link.
            if fs::readlinkat(to.dir, to.name, Vec::new()).is_ok_and(|there| there == target) {
                self.took(stat);
            }
            self.finished(0);
            return Ok(());
        }
        match fs::symlinkat(target.as_c_str(), to.dir, to.name) {
            Ok(()) => {}
            Err(Errno::EXIST) if self.merge => {
                self.held.kept(from.path);
                self.finished(0);
                return Ok(());
            }
            Err(error) => return Err(at(error, to.path, "symlink")),
        }
        self.took(stat);
        self.finished(0);
        Ok(())
    }

    fn directory(
        &mut self,
        from: At<'_>,
        to: At<'_>,
        depth: u32,
        stat: &Stat,
    ) -> Result<(), Problem> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let source = openat(from.dir, from.name, flags, Mode::empty())
            .map_err(|error| at(error, from.path, "open"))?;
        if !same_file(stat, &fstat(&source).map_err(|error| at(error, from.path, "stat"))?) {
            return Err(changed(from.path));
        }
        // 0o700 while it is being filled, so a half-copied directory is not
        // readable by anyone who was not already allowed to read the original.
        // The real mode goes on at the end.
        let made = match fs::mkdirat(to.dir, to.name, Mode::from_bits_truncate(0o700)) {
            Ok(()) => true,
            Err(Errno::EXIST) if self.options.replace || self.options.resume || self.merge => false,
            Err(error) => return Err(at(error, to.path, "mkdir")),
        };
        // `O_NOFOLLOW` and `O_DIRECTORY`: what was just made, or what a resumed
        // copy merges into, has to be a directory and not a link to one.
        let into = match openat(to.dir, to.name, flags, Mode::empty()) {
            Ok(into) => into,
            // A file or a link holds the name a folder is coming back to.
            Err(Errno::NOTDIR | Errno::LOOP) if self.merge && !made => {
                self.held.kept(from.path);
                return Ok(());
            }
            Err(error) => return Err(at(error, to.path, "open")),
        };
        if made {
            made_here(&into, to)?;
        } else if self.merge && self.merged.is_none() {
            self.merged = Some(to.path.to_path_buf());
        }
        // Keep the first warning bounded in memory, but finish every child. A
        // preservation warning describes published data; other errors stop the walk.
        let mut warning = None;
        for child in names(&source, from.path)? {
            let result = self.any(
                At { dir: source.as_fd(), name: &child, path: &from.path.join(&child) },
                At { dir: into.as_fd(), name: &child, path: &to.path.join(&child) },
                depth + 1,
            );
            if let Err(problem) = result {
                if problem.kind() != ProblemKind::Preservation {
                    return Err(problem);
                }
                warning.get_or_insert(problem);
            }
        }
        // Mode and times after the contents, for the reason a file's are:
        // writing a child moves the directory's modification time. Not onto a
        // folder a merge entered, which is somebody's own and stays as it was.
        let mut missing = Vec::new();
        if made || !self.merge {
            let theirs = foreign(stat);
            if fchmod(&into, carried_mode(&source, stat, theirs, &into)).is_err() {
                missing.push("mode".to_owned());
            }
            if self.options.xattrs {
                copy_xattrs(&source, &into, &mut missing, !theirs);
            }
            if futimens(&into, &times_of(stat)).is_err() {
                missing.push("times".to_owned());
            }
        }
        if !missing.is_empty() {
            warning.get_or_insert_with(|| {
                Problem::new(
                    ProblemKind::Preservation,
                    to.path.display().to_string(),
                    missing.join(", "),
                )
            });
        }
        self.took(stat);
        self.finished(0);
        warning.map_or(Ok(()), Err)
    }

    fn took(&mut self, stat: &Stat) {
        if let Some(taken) = self.taken.as_deref_mut() {
            taken.record(stat);
        }
    }

    fn finished(&mut self, bytes: i64) {
        self.progress.done += 1;
        self.progress.bytes += bytes.cast_unsigned();
        (self.watch)(*self.progress);
    }
}

/// Whether `to` names a regular file of exactly `bytes`.
///
/// `SYMLINK_NOFOLLOW`, so a symlink where a file should be is not the file: it
/// is something else with the same name, and copying over it is the right
/// answer rather than skipping it.
fn same_size(to: At<'_>, bytes: i64) -> bool {
    statat(to.dir, to.name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
        fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::RegularFile
            && stat.st_size == bytes
    })
}

/// The source's `fstat`, where `to` holds the same bytes as `from` — which is
/// what a resumed move may count as moved already.
fn same_bytes(from: At<'_>, to: At<'_>, seen: &Stat) -> Option<Stat> {
    let (source, read) = open_regular(from, Some(seen)).ok()?;
    let (copy, _) = open_regular(to, None).ok()?;
    let mut ours = vec![0u8; 1 << 20];
    let mut theirs = vec![0u8; 1 << 20];
    loop {
        let length = fill(&source, &mut ours).ok()?;
        if fill(&copy, &mut theirs[..length]).ok()? != length || ours[..length] != theirs[..length]
        {
            return None;
        }
        if length < ours.len() {
            return (fill(&copy, &mut theirs[..1]).ok()? == 0).then_some(read);
        }
    }
}

/// Reads until `buffer` is full or the file ends, and says how much it read.
fn fill(file: &OwnedFd, buffer: &mut [u8]) -> Result<usize, Errno> {
    let mut length = 0;
    while length < buffer.len() {
        match rustix::io::read(file, &mut buffer[length..]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(Errno::INTR) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(length)
}

/// Refuses a folder this walk opened where it had just made one, unless it is
/// the one it made.
///
/// **Between `mkdirat` and `openat` is a window**, and somebody who can write
/// the folder above could rename the new folder away and put one of their own
/// in its place, so that what follows is copied into a folder they hold — the
/// review measured it. A folder made a moment ago is empty, and on a filesystem
/// that keeps real ownership it is ours, so the one opened has to be both.
///
/// **Ownership is asked only where the filesystem answers it.** NFS with
/// `all_squash`, sshfs without `idmap` and CIFS with Unix extensions all give
/// a new folder an owner the server chose, which is neither the copier nor
/// reliably the parent's — so asking there refused ordinary copies onto them.
/// Which filesystem it is comes from `fstatfs` on the destination's parent,
/// against the local ones listed in [`OWNED`]; anywhere else the emptiness
/// test stands alone.
fn made_here(into: &OwnedFd, to: At<'_>) -> Result<(), Problem> {
    let made = fstat(into).map_err(|error| at(error, to.path, "stat"))?;
    let ours = !owners_are_real(to.dir) || made.st_uid == rustix::process::geteuid().as_raw();
    if !ours || !names(into, to.path)?.is_empty() {
        return Err(Problem::new(
            ProblemKind::FolderSwapped,
            to.path.display().to_string(),
            "not the folder just made",
        ));
    }
    Ok(())
}

/// Local filesystems whose owners are the ones the kernel wrote, by `f_type`
/// (`include/uapi/linux/magic.h`): ext2/3/4, xfs, btrfs, tmpfs, f2fs, ramfs,
/// bcachefs, jfs, nilfs2 and ZFS.
const OWNED: [u64; 10] = [
    0xEF53,
    0x5846_5342,
    0x9123_683E,
    0x0102_1994,
    0xF2F5_2010,
    0x8584_58F6,
    0xCA45_1A4E,
    0x3153_464A,
    0x3434,
    0x2FC1_2FC1,
];

/// Whether the filesystem holding `dir` keeps real ownership — see [`OWNED`].
///
/// A refused `fstatfs` answers no, which leaves the emptiness test.
fn owners_are_real(dir: BorrowedFd<'_>) -> bool {
    fs::fstatfs(dir).is_ok_and(|stat| {
        let kind: u64 = stat.f_type as _;
        OWNED.contains(&(kind & 0xFFFF_FFFF))
    })
}

/// Every name in a directory this walk holds open, except `.` and `..`.
///
/// Collected rather than streamed: the walk below creates files in the tree it is
/// reading from when a copy goes into a sibling directory, and a `getdents64`
/// cursor over a directory being written to may or may not see them.
fn names(directory: &OwnedFd, path: &Path) -> Result<Vec<OsString>, Problem> {
    let dir = fs::Dir::read_from(directory).map_err(|error| at(error, path, "read"))?;
    let mut out = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| at(e, path, "read"))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        // Bytes, not `str`: a filename is not required to be UTF-8, and a copy
        // that skips the ones that are not is a copy that loses files.
        out.push(OsStr::from_bytes(name).to_owned());
    }
    Ok(out)
}

/// A name that was one thing when the walk looked and another when it opened it.
///
/// Refused rather than copied: somebody renamed something into the tree while
/// it was being read, and whatever is there now is not what was chosen.
fn changed(path: &Path) -> Problem {
    Problem::new(ProblemKind::Gone, path.display().to_string(), "replaced while being copied")
}

fn cancelled(path: &Path) -> Problem {
    Problem::new(ProblemKind::Gone, path.display().to_string(), "cancelled")
}

fn too_deep(path: &Path) -> Problem {
    Problem::new(
        ProblemKind::BadName,
        path.display().to_string(),
        format!("nested more than {MAX_DEPTH} deep"),
    )
}

/// A name in the destination directory that nothing else will pick.
fn temporary() -> OsString {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    format!(".skerry-copy-{}-{nanos}", rustix::process::getpid().as_raw_nonzero()).into()
}

/// Gives the finished temporary file its final name without ever overwriting
/// something nobody asked to overwrite.
///
/// `true` beside the placement where the temporary name survived it.
fn place(to: At<'_>, temp: &OsStr, replace: bool) -> Result<(Placement, bool), Errno> {
    match place_existing((to.dir, Path::new(temp)), (to.dir, Path::new(to.name)), replace) {
        Ok(Placement::LinkThenUnlink) => {
            // The unlink can fail, and ignoring it leaves a `.skerry-copy-*` name
            // in the destination directory *and* inflates the link count of what
            // is supposed to be a clean copy.
            let orphan = unlinkat(to.dir, temp, AtFlags::empty()).is_err();
            Ok((Placement::LinkThenUnlink, orphan))
        }
        Ok(placement) => Ok((placement, false)),
        Err(error) => Err(error),
    }
}

/// The placement chain, over an entry that already exists.
///
/// Shared by [`move_file`], which renames the source itself, and by [`place`],
/// which renames a temporary file. `LinkThenUnlink` leaves the source linked in
/// two places and it is the caller's job to unlink it — which is what lets
/// `move_file` treat the link count honestly.
fn place_existing(
    (from_dir, from): (BorrowedFd<'_>, &Path),
    (to_dir, to): (BorrowedFd<'_>, &Path),
    replace: bool,
) -> Result<Placement, Errno> {
    if replace {
        renameat_with(from_dir, from, to_dir, to, RenameFlags::empty())?;
        return Ok(Placement::Replace);
    }
    match renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE) {
        Ok(()) => return Ok(Placement::NoReplace),
        // The destination is really there; there is nothing else to try.
        Err(Errno::EXIST) => return Err(Errno::EXIST),
        // Different filesystems, which is the caller's decision to make.
        Err(Errno::XDEV) => return Err(Errno::XDEV),
        // The *flag* is unsupported, which is a different thing entirely and is
        // reported with the same errno by some filesystems.
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {}
        Err(error) => return Err(error),
    }
    match fs::linkat(from_dir, from, to_dir, to, AtFlags::empty()) {
        Ok(()) => return Ok(Placement::LinkThenUnlink),
        Err(Errno::EXIST) => return Err(Errno::EXIST),
        Err(Errno::XDEV) => return Err(Errno::XDEV),
        Err(_) => {}
    }
    // Refuse rather than overwrite an entry created after a userspace check.
    Err(Errno::OPNOTSUPP)
}

/// Each strategy in turn, until one writes the bytes.
fn write_contents(
    from: &impl std::os::fd::AsFd,
    into: &impl std::os::fd::AsFd,
    size: u64,
    options: Options,
    refused: &mut Vec<(Strategy, i32)>,
    checkpoint: &mut dyn FnMut(u64) -> bool,
) -> Result<(Strategy, u64), (Errno, &'static str)> {
    // Not an error when it fails: most filesystems cannot share extents, which is
    // exactly why the strategy is probed and not assumed.
    if options.reflink {
        match fs::ioctl_ficlone(into.as_fd(), from.as_fd()) {
            Ok(()) => return Ok((Strategy::Reflink, 0)),
            Err(error) => refused.push((Strategy::Reflink, error.raw_os_error())),
        }
    }
    if options.kernel_copy {
        match kernel_copy(from, into, size, checkpoint) {
            Ok(bytes) => return Ok((Strategy::KernelCopy, bytes)),
            // Bytes already moved means a real failure rather than a refusal, and
            // starting over would write them twice.
            Err((error, bytes)) if bytes > 0 => return Err((error, "copy_file_range")),
            Err((error, _)) => refused.push((Strategy::KernelCopy, error.raw_os_error())),
        }
    }
    read_write(from, into, size, checkpoint).map(|bytes| (Strategy::ReadWrite, bytes))
}

/// `copy_file_range`, **per data extent**.
///
/// One call over the whole file is what the first version of Spike E did, and it
/// turned a 1 GiB sparse file with 12 KiB of data into 1 GiB of zeros on both ext4
/// and tmpfs: `copy_file_range` does not promise to preserve holes, and where it
/// cannot share extents it materialises them. Seeking to each data extent keeps
/// the kernel-side copy *and* the sparseness, so there is no trade to make.
fn kernel_copy(
    from: &impl std::os::fd::AsFd,
    into: &impl std::os::fd::AsFd,
    size: u64,
    checkpoint: &mut dyn FnMut(u64) -> bool,
) -> Result<u64, (Errno, u64)> {
    let mut total = 0u64;
    let mut cursor = 0u64;
    while cursor < size {
        let (data, hole) = match extent(from, cursor, size) {
            Ok(Some(span)) => span,
            Ok(None) => break,
            Err(error) => return Err((error, total)),
        };
        let (mut read_at, mut write_at) = (data, data);
        while read_at < hole {
            if !checkpoint(read_at) {
                return Err((Errno::CANCELED, total));
            }
            let want = usize::try_from((hole - read_at).min(1 << 20)).unwrap_or(1 << 20);
            match fs::copy_file_range(
                from.as_fd(),
                Some(&mut read_at),
                into.as_fd(),
                Some(&mut write_at),
                want,
            ) {
                Ok(0) => return Err((Errno::IO, total)),
                Ok(moved) => total += moved as u64,
                Err(error) => return Err((error, total)),
            }
        }
        cursor = hole;
    }
    // A file that ends in a hole, or is entirely a hole, still has a length.
    ftruncate(into.as_fd(), size).map_err(|error| (error, total))?;
    Ok(total)
}

/// The floor, and the only path that has to think about holes itself.
fn read_write(
    from: &impl std::os::fd::AsFd,
    into: &impl std::os::fd::AsFd,
    size: u64,
    checkpoint: &mut dyn FnMut(u64) -> bool,
) -> Result<u64, (Errno, &'static str)> {
    let mut buffer = vec![0u8; 1 << 20];
    let mut written = 0u64;
    let mut cursor = 0u64;
    while cursor < size {
        let (data, hole) = match extent(from, cursor, size) {
            Ok(Some(span)) => span,
            Ok(None) => break,
            Err(error) => return Err((error, "seek")),
        };
        let mut at = data;
        while at < hole {
            if !checkpoint(at) {
                return Err((Errno::CANCELED, "cancelled"));
            }
            let want = usize::try_from(hole - at).unwrap_or(buffer.len()).min(buffer.len());
            seek(from.as_fd(), SeekFrom::Start(at)).map_err(|error| (error, "seek"))?;
            let read = rustix::io::read(from.as_fd(), &mut buffer[..want])
                .map_err(|error| (error, "read"))?;
            if read == 0 {
                return Err((Errno::IO, "source shortened"));
            }
            seek(into.as_fd(), SeekFrom::Start(at)).map_err(|error| (error, "seek"))?;
            let mut done = 0;
            while done < read {
                let moved = rustix::io::write(into.as_fd(), &buffer[done..read])
                    .map_err(|error| (error, "write"))?;
                if moved == 0 {
                    return Err((Errno::IO, "write"));
                }
                done += moved;
            }
            at += read as u64;
            written += read as u64;
        }
        cursor = hole;
    }
    // The destination has to be the source's length even when the source ends in
    // a hole, and even when it is entirely a hole.
    ftruncate(into.as_fd(), size).map_err(|error| (error, "truncate"))?;
    Ok(written)
}

/// The next run of data at or after `cursor`, or `None` where the rest is a hole.
///
/// `SEEK_DATA` past the last extent answers `ENXIO`, which is how a caller's loop
/// ends. One function because both strategies walk extents the same way, and two
/// copies of this arithmetic is how one of them stops preserving holes.
fn extent(
    from: &impl std::os::fd::AsFd,
    cursor: u64,
    size: u64,
) -> Result<Option<(u64, u64)>, Errno> {
    let data = match seek(from.as_fd(), SeekFrom::Data(cursor)) {
        Ok(at) => at,
        Err(Errno::NXIO) => return Ok(None),
        Err(error) => return Err(error),
    };
    let hole = seek(from.as_fd(), SeekFrom::Hole(data)).unwrap_or(size).min(size);
    Ok(Some((data, hole)))
}

#[cfg(test)]
std::thread_local! {
    static REFUSE_NEXT_XATTR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Copies extended attributes, reporting each one that will not go.
///
/// `security.*` and `trusted.*` need privilege this process does not have, and a
/// FAT-family filesystem refuses everything. Both are reported and neither aborts
/// the copy, because a copy that fails on a `security.selinux` label is a copy
/// that fails on every file on a Fedora desktop.
///
/// `acls` false leaves the two POSIX ACLs behind, deliberately and unreported:
/// see [`foreign`].
pub(crate) fn copy_xattrs(
    from: &impl std::os::fd::AsFd,
    into: &impl std::os::fd::AsFd,
    out: &mut Vec<String>,
    acls: bool,
) {
    #[cfg(test)]
    if REFUSE_NEXT_XATTR.with(|refuse| refuse.replace(false)) {
        out.push("injected unsupported extended attribute".to_owned());
        return;
    }
    let mut names = vec![0u8; 64 * 1024];
    let length = match fs::flistxattr(from.as_fd(), &mut names[..]) {
        Ok(length) => length,
        // No attributes, or a filesystem that has none. Neither is a failure.
        Err(Errno::NOTSUP | Errno::NODATA) => return,
        Err(_) => {
            out.push("extended attributes".to_owned());
            return;
        }
    };
    let mut value = vec![0u8; 64 * 1024];
    for name in names[..length].split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        if !acls && POSIX_ACLS.contains(&name) {
            continue;
        }
        let name = String::from_utf8_lossy(name).into_owned();
        let Ok(size) = fs::fgetxattr(from.as_fd(), &name[..], &mut value[..]) else {
            out.push(name);
            continue;
        };
        if fs::fsetxattr(into.as_fd(), &name[..], &value[..size], fs::XattrFlags::empty()).is_err()
        {
            out.push(name);
        }
    }
}

/// The access ACL and, on a directory, the one its new entries inherit.
const POSIX_ACLS: [&[u8]; 2] = [b"system.posix_acl_access", b"system.posix_acl_default"];

/// Whether `source` belongs to somebody other than the person copying it.
///
/// **A copy belongs to whoever made it**, because nothing here calls `chown`,
/// so a grant its owner wrote onto the source would arrive as a grant by the
/// copier, who never made it. Two are left behind when the source is somebody
/// else's:
///
/// * Set-user-ID and set-group-ID, which would run the program with the
///   copier's rights. `cp -p` drops these too when it cannot keep the owner.
/// * Both POSIX ACLs. `cp -p` copies those whoever owns the file; this does
///   not, because an entry naming `user:someone:rw-` — a default ACL on a
///   directory most of all, which every file later put inside inherits — would
///   let that person into what is now the copier's file.
fn foreign(source: &Stat) -> bool {
    source.st_uid != rustix::process::geteuid().as_raw()
}

/// The mode a copy of `source` (open as `from`) is given, once it exists as
/// `copy`.
///
/// Set-user-ID and set-group-ID are dropped where `theirs`, the source being
/// [`foreign`], and set-group-ID from one's own file where the copy came out in
/// a different group — `cp -p` keeps each bit only where it kept the owner or
/// the group it is about.
///
/// **Where `theirs` leaves the access ACL behind, the group bits come from its
/// `group::` entry within its mask.** On a file with an ACL the mode's group
/// bits *are* the mask — the union of every named grant — so carrying them
/// gave the copy's owning group what only the named entries had: `group::---`
/// with `user:0:rw-` arrived as `0660`.
///
/// The sticky bit is kept: it grants nothing. On a directory it stops people
/// removing each other's entries, so dropping it would turn a copied `1777`
/// drop box into a `0777` one anybody can empty; on a file Linux ignores it.
fn carried_mode(from: &impl AsFd, source: &Stat, theirs: bool, copy: &impl AsFd) -> Mode {
    let mut mode = Mode::from_bits_truncate(source.st_mode & 0o7777);
    if theirs {
        mode.remove(Mode::SUID | Mode::SGID);
        if let Some(group) = owning_group(from) {
            mode = Mode::from_bits_truncate((mode.bits() & !0o070) | (group << 3));
        }
    } else if mode.contains(Mode::SGID)
        && fstat(copy).map_or(true, |copy| copy.st_gid != source.st_gid)
    {
        mode.remove(Mode::SGID);
    }
    mode
}

/// The owning group's permission bits in `file`'s access ACL, within its mask —
/// what the group may actually do — or `None` where it has no extended ACL.
///
/// The kernel's xattr form (`include/uapi/linux/posix_acl_xattr.h`): a 4-byte
/// version, then 8-byte entries of tag, permission and id, little-endian.
fn owning_group(file: &impl AsFd) -> Option<u32> {
    const GROUP_OBJ: u16 = 0x04;
    const MASK: u16 = 0x10;
    let mut buffer = vec![0u8; 64 * 1024];
    let length = fs::fgetxattr(file, "system.posix_acl_access", &mut buffer[..]).ok()?;
    let entries = buffer.get(4..length)?;
    let (mut group, mut mask) = (None, None);
    for entry in entries.chunks_exact(8) {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        let permission = u32::from(u16::from_le_bytes([entry[2], entry[3]]) & 0o7);
        match tag {
            GROUP_OBJ => group = Some(permission),
            MASK => mask = Some(permission),
            _ => {}
        }
    }
    Some(group? & mask.unwrap_or(0o7))
}

fn times_of(stat: &Stat) -> Timestamps {
    Timestamps {
        // `statx` reports the nanoseconds unsigned and `Timespec` takes them
        // signed. Both hold the same 30 bits, so this is a spelling and not a
        // conversion.
        last_access: Timespec {
            tv_sec: stat.st_atime as _,
            tv_nsec: stat.st_atime_nsec.cast_signed() as _,
        },
        last_modification: Timespec {
            tv_sec: stat.st_mtime as _,
            tv_nsec: stat.st_mtime_nsec.cast_signed() as _,
        },
    }
}

fn same_file(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

/// Removes the temporary file, and says so if even that failed.
fn discard(to: At<'_>, temp: &OsStr, error: Errno, stage: &'static str) -> Problem {
    if unlinkat(to.dir, temp, AtFlags::empty()).is_err() {
        return Problem::new(
            ProblemKind::Unreadable,
            to.path.display().to_string(),
            format!("{stage}: {error}, and {} could not be removed", to.sibling(temp).display()),
        );
    }
    at(error, to.path, stage)
}

/// One errno, as something a person can act on and a stage they can report.
///
/// The same mapping `ops::problem` uses, with `ENOSPC` split out: a destination
/// with no room is the answer a copy gives most often after a name collision, and
/// "a per-user kernel resource is exhausted" is not advice about it.
/// The [`Problem`] this crate raises for a raw errno.
///
/// **Public so there is one table and not two.** A caller that does part of an
/// operation itself — the Spike E matrix copies symlinks with its own code —
/// otherwise keeps a second mapping from errno to kind, and the two drift: that
/// one classified an `EILSEQ` as unreadable while this one called it
/// [`ProblemKind::NameRefused`], and a matrix row went from a limitation to a
/// failure on the strength of the disagreement.
#[must_use]
pub fn problem_for(errno: i32, path: &Path, stage: &str) -> Problem {
    at_stage(Errno::from_raw_os_error(errno), path, stage)
}

fn at(error: Errno, path: &Path, stage: &'static str) -> Problem {
    at_stage(error, path, stage)
}

fn at_stage(error: Errno, path: &Path, stage: &str) -> Problem {
    let kind = match error {
        Errno::NOENT => ProblemKind::Gone,
        Errno::EXIST => ProblemKind::NameTaken,
        Errno::XDEV => ProblemKind::AcrossFilesystems,
        Errno::NOSPC | Errno::DQUOT => ProblemKind::Full,
        Errno::MFILE | Errno::NFILE => ProblemKind::OutOfResources,
        // A destination that will not encode the name's bytes: vfat and NTFS
        // both do this, and it is not the source being unreadable.
        Errno::ILSEQ => ProblemKind::NameRefused,
        // Permission, a read-only mount, and everything left over: the message
        // carries the errno, and the kind decides the advice.
        _ => ProblemKind::Unreadable,
    };
    Problem::new(kind, path.display().to_string(), format!("{stage}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Options, Placement, Strategy, copy_file, move_file, remove};
    use std::os::fd::AsFd as _;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("skerry-copy-{}-{label}", rustix::process::getpid().as_raw_nonzero()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    /// How many 512-byte blocks the file actually occupies, which is the only way
    /// to tell a hole from a run of zeros.
    fn blocks(path: &Path) -> u64 {
        rustix::fs::statat(rustix::fs::CWD, path, rustix::fs::AtFlags::empty())
            .unwrap()
            .st_blocks
            .cast_unsigned()
    }

    /// A file with a 64 MiB hole and 8 KiB of data at each end.
    fn sparse(path: &Path) -> u64 {
        use std::io::{Seek, Write};

        let size = 64 * 1024 * 1024;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&[b'a'; 8192]).unwrap();
        file.seek(std::io::SeekFrom::Start(size - 8192)).unwrap();
        file.write_all(&[b'z'; 8192]).unwrap();
        drop(file);
        size
    }

    #[test]
    fn a_copy_is_byte_for_byte_and_keeps_the_mode_and_the_time() {
        use std::os::unix::fs::MetadataExt as _;

        let root = scratch("plain");
        let source = root.join("source.bin");
        let content: Vec<u8> = (0..70_000u32).map(|byte| byte as u8).collect();
        write(&source, &content);
        std::fs::set_permissions(&source, std::os::unix::fs::PermissionsExt::from_mode(0o640))
            .unwrap();
        let before = std::fs::metadata(&source).unwrap();

        let destination = root.join("copy.bin");
        let copied = copy_file(&source, &destination, Options::default()).expect("copied");
        assert_eq!(std::fs::read(&destination).unwrap(), content);
        assert_eq!(copied.placement, Placement::NoReplace);
        assert!(copied.orphan.is_none());

        let after = std::fs::metadata(&destination).unwrap();
        assert_eq!(after.mode() & 0o7777, before.mode() & 0o7777, "{:?}", copied.unpreserved);
        assert_eq!(after.mtime(), before.mtime(), "the modification time was not carried over");
    }

    /// The defect that turned a 1 GiB sparse file into a gigabyte of zeros. It
    /// passes a byte-for-byte comparison, so the assertion has to be about blocks.
    #[test]
    fn a_sparse_file_stays_sparse() {
        let root = scratch("sparse");
        let source = root.join("sparse.img");
        let size = sparse(&source);
        let occupied = blocks(&source);
        assert!(occupied * 512 < size / 2, "the fixture is not sparse: {occupied} blocks");

        let destination = root.join("sparse-copy.img");
        copy_file(&source, &destination, Options::default()).expect("copied");
        assert_eq!(std::fs::metadata(&destination).unwrap().len(), size, "the length changed");
        assert_eq!(std::fs::read(&source).unwrap(), std::fs::read(&destination).unwrap());
        let after = blocks(&destination);
        assert!(after <= occupied * 2, "the copy occupies {after} blocks against {occupied}");
    }

    /// A file that is nothing but a hole still has a length, and a copy that
    /// forgets the final truncate produces an empty file that compares equal to
    /// nothing.
    #[test]
    fn a_file_that_is_entirely_a_hole_keeps_its_length() {
        let root = scratch("hole");
        let source = root.join("hole.img");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
        drop(file);

        let destination = root.join("hole-copy.img");
        copy_file(&source, &destination, Options::default()).expect("copied");
        assert_eq!(std::fs::metadata(&destination).unwrap().len(), 4 * 1024 * 1024);
        assert!(std::fs::read(&destination).unwrap().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn an_empty_file_copies_to_an_empty_file() {
        let root = scratch("empty");
        let source = root.join("empty");
        write(&source, b"");
        let destination = root.join("empty-copy");
        copy_file(&source, &destination, Options::default()).expect("copied");
        assert_eq!(std::fs::metadata(&destination).unwrap().len(), 0);
    }

    /// Refusing rather than replacing is the whole reason the destination name is
    /// never written to directly.
    #[test]
    fn an_existing_destination_is_refused_and_left_exactly_as_it_was() {
        let root = scratch("exists");
        let source = root.join("source");
        let destination = root.join("destination");
        write(&source, b"new");
        write(&destination, b"old");

        let refused = copy_file(&source, &destination, Options::default()).expect_err("refused");
        assert!(refused.message().contains("destination"), "{}", refused.message());
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");

        let replace = Options { replace: true, ..Options::default() };
        let copied = copy_file(&source, &destination, replace).expect("replaced");
        assert_eq!(copied.placement, Placement::Replace);
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    }

    /// `cp a a` that opens the destination for writing empties the file before it
    /// reads anything. Through a hardlink, where a path comparison sees two names.
    #[test]
    fn copying_a_file_onto_itself_refuses_instead_of_emptying_it() {
        let root = scratch("self");
        let source = root.join("one");
        write(&source, b"precious");
        let same = root.join("two");
        std::fs::hard_link(&source, &same).unwrap();

        let replace = Options { replace: true, ..Options::default() };
        assert!(copy_file(&source, &source, replace).is_err());
        assert!(copy_file(&source, &same, replace).is_err(), "a hardlink is the same file");
        assert_eq!(std::fs::read(&source).unwrap(), b"precious");
    }

    /// The two questions the torture matrix asks that the product never does:
    /// what did the filesystem refuse, and what happens with the floor forced.
    ///
    /// **A matrix row saying `read/write` means nothing on its own.** It is the
    /// same answer whether `FICLONE` was refused with `EOPNOTSUPP` or never
    /// attempted, and `tools/torture` exists to tell those apart on seven
    /// filesystems. Turning a strategy off has to *both* skip it and leave no
    /// refusal behind, or the report says a filesystem cannot do something
    /// nobody asked it to do.
    #[test]
    fn a_forced_floor_skips_the_faster_ways_and_says_it_did_not_ask() {
        let root = scratch("forced");
        let source = root.join("source");
        std::fs::write(&source, b"some bytes to move").unwrap();

        let asked = copy_file(&source, &root.join("asked"), Options::default()).unwrap();
        // Whatever this filesystem does, everything it did *not* choose was
        // tried first and is on the list with an errno.
        assert_eq!(
            asked.refused.len(),
            match asked.strategy {
                Strategy::Reflink => 0,
                Strategy::KernelCopy => 1,
                Strategy::ReadWrite => 2,
            },
            "strategy {:?} against refusals {:?}",
            asked.strategy,
            asked.refused
        );

        let floor = Options { reflink: false, kernel_copy: false, ..Options::default() };
        let forced = copy_file(&source, &root.join("forced"), floor).unwrap();
        assert_eq!(forced.strategy, Strategy::ReadWrite);
        assert!(forced.refused.is_empty(), "refused {:?} while asking for none", forced.refused);
        assert_eq!(std::fs::read(root.join("forced")).unwrap(), b"some bytes to move");
    }

    /// Nothing is left behind when a copy fails, because nothing was ever written
    /// to the destination name. The failure here is the source vanishing mid-flight,
    /// which is the cheapest one to arrange without a filesystem that fails on demand.
    #[test]
    fn a_failed_copy_leaves_no_temporary_file() {
        let root = scratch("fail");
        let missing = root.join("not-there");
        let destination = root.join("destination");
        assert!(copy_file(&missing, &destination, Options::default()).is_err());
        assert!(!destination.exists());
        let leftovers: Vec<_> = std::fs::read_dir(&root) // skerry-allow: no-read-dir — a test reading six names
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".skerry-copy-"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_move_inside_one_filesystem_is_a_rename() {
        let root = scratch("move");
        let source = root.join("here");
        write(&source, b"contents");
        let destination = root.join("there");
        let placement = move_file(&source, &destination, Options::default()).expect("moved");
        assert_eq!(placement, Placement::NoReplace);
        assert!(!source.exists(), "the source survived a move");
        assert_eq!(std::fs::read(&destination).unwrap(), b"contents");
    }

    #[test]
    fn a_move_refuses_an_existing_destination_and_keeps_both_files() {
        let root = scratch("move-exists");
        let source = root.join("here");
        let destination = root.join("there");
        write(&source, b"one");
        write(&destination, b"two");
        assert!(move_file(&source, &destination, Options::default()).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"one", "the source was lost");
        assert_eq!(std::fs::read(&destination).unwrap(), b"two");
    }

    /// The strategy is probed rather than assumed, so which one runs depends on
    /// the filesystem the test happens to be on. What must hold everywhere is
    /// that one of them ran and the bytes arrived.
    #[test]
    fn whichever_strategy_the_filesystem_allows_produces_the_same_bytes() {
        let root = scratch("strategy");
        let source = root.join("source");
        let content: Vec<u8> = (0..3_000_000u32).map(|byte| (byte % 251) as u8).collect();
        write(&source, &content);
        let destination = root.join("destination");
        let copied = copy_file(&source, &destination, Options::default()).expect("copied");
        assert!(matches!(
            copied.strategy,
            Strategy::Reflink | Strategy::KernelCopy | Strategy::ReadWrite
        ));
        assert_eq!(std::fs::read(&destination).unwrap(), content);
    }

    fn tree(root: &Path) -> PathBuf {
        let source = root.join("tree");
        std::fs::create_dir_all(source.join("inner/deeper")).unwrap();
        write(&source.join("top.txt"), b"top");
        write(&source.join("inner/middle.txt"), b"middle");
        write(&source.join("inner/deeper/bottom.txt"), b"bottom");
        std::os::unix::fs::symlink("top.txt", source.join("link")).unwrap();
        source
    }

    fn walk(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(at) = stack.pop() {
            // The rule is about the product's walks over a million entries; this
            // is a test asserting about six names.
            let listing = std::fs::read_dir(&at).unwrap(); // skerry-allow: no-read-dir — see above
            for entry in listing.filter_map(Result::ok) {
                let path = entry.path();
                let shown = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let kind = entry.file_type().unwrap();
                out.push(format!(
                    "{shown} {}",
                    if kind.is_dir() {
                        "dir"
                    } else if kind.is_symlink() {
                        "link"
                    } else {
                        "file"
                    }
                ));
                if kind.is_dir() {
                    stack.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn a_metadata_warning_finishes_the_entire_tree() {
        let root = scratch("tree-warning");
        let source = tree(&root);
        let into = root.join("elsewhere");
        std::fs::create_dir(&into).unwrap();
        super::REFUSE_NEXT_XATTR.with(|refuse| refuse.set(true));
        let mut progress = super::Progress::default();
        let problem = super::copy_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut progress,
            &mut |_| {},
        )
        .expect_err("metadata warning remains visible");
        assert_eq!(problem.kind(), sk_model::ProblemKind::Preservation);
        assert_eq!(walk(&source), walk(&into.join("tree")));
        assert_eq!(progress.done, 7);
        for file in ["top.txt", "inner/middle.txt", "inner/deeper/bottom.txt"] {
            assert_eq!(
                std::fs::read(source.join(file)).unwrap(),
                std::fs::read(into.join("tree").join(file)).unwrap()
            );
        }
    }

    #[test]
    fn a_whole_tree_copies_with_its_shape_and_its_links() {
        let root = scratch("tree");
        let source = tree(&root);
        let into = root.join("elsewhere");
        std::fs::create_dir(&into).unwrap();

        let mut progress = super::Progress::default();
        let landed = super::copy_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut progress,
            &mut |_| {},
        )
        .expect("copied");
        assert_eq!(landed, into.join("tree"));
        assert_eq!(walk(&source), walk(&landed), "the shape changed");
        assert_eq!(std::fs::read(landed.join("inner/deeper/bottom.txt")).unwrap(), b"bottom");
        // Seven: three files, three directories and a link.
        assert_eq!(progress.done, 7, "counted {} entries", progress.done);
        assert!(progress.bytes >= 15, "counted {} bytes", progress.bytes);
    }

    /// A link is recreated, not followed. Following one turns a 40-byte link into
    /// a copy of whatever it points at — and a link into the directory being
    /// copied turns the copy into an unbounded one.
    #[test]
    fn a_symlink_is_recreated_rather_than_followed() {
        let root = scratch("link");
        let source = root.join("from");
        std::fs::create_dir(&source).unwrap();
        write(&source.join("real.txt"), b"contents");
        std::os::unix::fs::symlink("real.txt", source.join("pointer")).unwrap();
        let into = root.join("to");
        std::fs::create_dir(&into).unwrap();

        let landed = super::copy_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect("copied");
        let link = landed.join("pointer");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink(), "it was followed");
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("real.txt"));
    }

    /// The walk would find the copy it is making and copy that, forever.
    #[test]
    fn a_folder_cannot_be_copied_into_itself() {
        let root = scratch("into-itself");
        let source = tree(&root);
        for into in [source.clone(), source.join("inner")] {
            let refused = super::copy_into(
                &source,
                &into,
                Options::default(),
                &sk_util::Cancel::new(),
                &mut super::Progress::default(),
                &mut |_| {},
            )
            .expect_err("refused");
            assert!(refused.message().contains("itself"), "{}", refused.message());
        }
    }

    #[test]
    fn a_tree_moves_and_the_source_is_gone() {
        let root = scratch("move-tree");
        let source = tree(&root);
        let shape = walk(&source);
        let into = root.join("elsewhere");
        std::fs::create_dir(&into).unwrap();
        let (landed, left) = super::move_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect("moved");
        assert!(!source.exists(), "the source survived");
        assert!(left.is_none(), "{left:?}");
        assert_eq!(walk(&landed), shape);
    }

    /// The half of a move that runs between filesystems, run here on one, with
    /// `meanwhile` called once `when` entries have been copied.
    fn across(
        source: &Path,
        destination: &Path,
        when: u64,
        meanwhile: impl FnOnce(),
    ) -> Option<sk_model::Problem> {
        let mut meanwhile = Some(meanwhile);
        super::across(
            source,
            destination,
            Options::default(),
            false,
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |progress: super::Progress| {
                if progress.done == when
                    && let Some(meanwhile) = meanwhile.take()
                {
                    meanwhile();
                }
            },
        )
        .expect("moved")
    }

    /// Removing the whole source after the copy deleted whatever had arrived in
    /// it while the copy ran — files the copy never saw and nothing else held.
    #[test]
    fn a_move_across_filesystems_leaves_what_arrived_while_it_copied() {
        let root = scratch("across-arrived");
        let source = root.join("tree");
        three(&source, "moved");
        let destination = root.join("landed");

        let left = across(&source, &destination, 1, || {
            std::fs::write(source.join("arrived.txt"), b"new").unwrap();
            std::fs::create_dir(source.join("arrived")).unwrap();
        });
        assert_eq!(std::fs::read(source.join("arrived.txt")).unwrap(), b"new");
        assert!(source.join("arrived").is_dir());
        let left = left.expect("what arrived is reported");
        assert_eq!(left.kind(), sk_model::ProblemKind::LeftBehind, "{left}");
        assert!(left.detail().starts_with("2 left standing"), "{left}");
        for name in ["a", "b", "c"] {
            assert!(!source.join(name).exists(), "{name} was copied and should have gone");
            assert_eq!(std::fs::read_to_string(destination.join(name)).unwrap(), "moved");
        }
    }

    /// A file written to after the copy read it: the copy holds the old bytes,
    /// so the source keeps the new ones.
    #[test]
    fn a_file_written_to_after_it_was_copied_stays_where_it_was() {
        let root = scratch("across-written");
        let source = root.join("tree");
        three(&source, "moved");
        let destination = root.join("landed");

        let left = across(&source, &destination, 3, || {
            std::fs::write(source.join("a"), b"written after the copy").unwrap();
        });
        assert_eq!(std::fs::read(source.join("a")).unwrap(), b"written after the copy");
        assert_eq!(std::fs::read_to_string(destination.join("a")).unwrap(), "moved");
        assert!(!source.join("b").exists() && !source.join("c").exists());
        let left = left.expect("the written file is reported");
        assert!(left.message().contains(&source.join("a").display().to_string()), "{left}");
    }

    /// Unlinking one name of a hardlink set changes the inode's `ctime`, which
    /// is why the check is on `mtime`: on `ctime`, the second name stayed.
    #[test]
    fn every_name_of_a_hardlink_set_goes_with_the_move() {
        let root = scratch("across-links");
        let source = root.join("tree");
        three(&source, "moved");
        std::fs::hard_link(source.join("a"), source.join("a-again")).unwrap();

        let left = across(&source, &root.join("landed"), u64::MAX, || {});
        assert!(left.is_none(), "{left:?}");
        assert!(!source.exists());
    }

    /// The review's race, held still: a new version renamed over a copied name
    /// after the removal looked at it. A check and an unlink by name deleted the
    /// new version (17,282 of 60,000 in the race); through the quarantine name,
    /// what is unlinked is what was checked, so the new version goes back.
    #[test]
    fn a_new_version_renamed_over_a_copied_name_is_put_back_not_removed() {
        let root = scratch("quarantine");
        std::fs::write(root.join("f"), b"original").unwrap();
        let dir = std::fs::File::open(&root).unwrap();
        let seen = super::statat(&dir, "f", super::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::write(root.join("new"), b"NEW").unwrap();
        std::fs::rename(root.join("new"), root.join("f")).unwrap();

        let outcome = super::unlink_unchanged(dir.as_fd(), "f".as_ref(), &seen, false).unwrap();
        assert!(matches!(&outcome, super::Unlinked::Kept(name) if name == "f"), "not put back");
        assert_eq!(std::fs::read(root.join("f")).unwrap(), b"NEW");
        assert_eq!(super::names(&dir.into(), &root).unwrap(), vec![std::ffi::OsString::from("f")]);
    }

    /// Rewritten to the same length with the old modification time put back —
    /// `touch -r`, `rsync -t`, an SFTP upload. Size and `mtime` both match; only
    /// the change time says it was written, and the file has one name.
    #[test]
    fn a_same_size_rewrite_with_its_time_put_back_stays_where_it_was() {
        let root = scratch("across-rewritten");
        let source = root.join("tree");
        three(&source, "moved");
        let destination = root.join("landed");

        let left = across(&source, &destination, 3, || {
            let file = source.join("a");
            let before = std::fs::metadata(&file).unwrap();
            std::fs::write(&file, b"MOVED").unwrap();
            let times = std::fs::FileTimes::new()
                .set_modified(before.modified().unwrap())
                .set_accessed(before.accessed().unwrap());
            std::fs::File::options().write(true).open(&file).unwrap().set_times(times).unwrap();
        });
        assert_eq!(std::fs::read(source.join("a")).unwrap(), b"MOVED");
        assert_eq!(std::fs::read(destination.join("a")).unwrap(), b"moved");
        assert!(left.is_some_and(|left| left.message().contains("/tree/a")));
    }

    /// A resumed move skipped a file on its size and then removed the source:
    /// an edit made after the stop, to the same length, was deleted. The bytes
    /// are compared now, and only a file that matches counts as moved.
    #[test]
    fn a_resumed_move_removes_only_what_it_finds_already_moved_byte_for_byte() {
        let root = scratch("across-resumed");
        let source = root.join("tree");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("edited"), b"EDITED-AFTER-STOP").unwrap();
        std::fs::write(source.join("same"), b"unchanged").unwrap();
        let destination = root.join("landed");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("edited"), b"stale-first-pass!").unwrap();
        std::fs::write(destination.join("same"), b"unchanged").unwrap();

        let resume = Options { resume: true, ..Options::default() };
        let left = super::across(
            &source,
            &destination,
            resume,
            false,
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect("moved");
        assert_eq!(std::fs::read(source.join("edited")).unwrap(), b"EDITED-AFTER-STOP");
        assert!(!source.join("same").exists(), "a file already moved was left");
        assert!(left.is_some_and(|left| left.message().contains("/tree/edited")));
    }

    /// The folder opened where one was just made is the one made: empty, and
    /// ours. One with something in it is somebody else's, swapped in.
    #[test]
    fn a_folder_swapped_in_after_it_was_made_is_refused() {
        let root = scratch("made-here");
        let parent = std::fs::File::open(&root).unwrap();
        let at = |name: &'static str, path: &'static Path| super::At {
            dir: parent.as_fd(),
            name: name.as_ref(),
            path,
        };
        std::fs::create_dir(root.join("made")).unwrap();
        let made = std::fs::File::open(root.join("made")).unwrap().into();
        assert!(super::made_here(&made, at("made", Path::new("made"))).is_ok());

        std::fs::create_dir(root.join("theirs")).unwrap();
        std::fs::write(root.join("theirs/ATTACKER"), b"x").unwrap();
        let theirs = std::fs::File::open(root.join("theirs")).unwrap().into();
        let refused = super::made_here(&theirs, at("theirs", Path::new("theirs"))).unwrap_err();
        assert_eq!(refused.kind(), sk_model::ProblemKind::FolderSwapped, "{refused}");
    }

    /// Ownership is asked only of a filesystem that keeps it: tmpfs does, and
    /// procfs — standing in for NFS, sshfs and FAT, whose owners a server or a
    /// mount option chose — does not, so there emptiness is the whole test.
    #[test]
    fn owners_are_asked_only_where_the_filesystem_keeps_them() {
        let open = |path: &str| std::fs::File::open(path).unwrap();
        let tmp = open("/dev/shm");
        assert!(super::owners_are_real(tmp.as_fd()), "tmpfs keeps real owners");
        let proc = open("/proc");
        assert!(!super::owners_are_real(proc.as_fd()), "procfs was asked for an owner");
    }

    /// A move back into a folder that was already there says so, even where
    /// nothing in it clashed: it now holds two things' contents.
    #[test]
    fn a_move_back_into_a_folder_already_there_says_so() {
        let root = scratch("merged-into");
        three(&root.join("tree"), "moved");
        three(&root.join("back"), "there");
        std::fs::rename(root.join("back/a"), root.join("back/kept")).unwrap();
        std::fs::remove_file(root.join("back/b")).unwrap();
        std::fs::remove_file(root.join("back/c")).unwrap();
        let noted = super::across(
            &root.join("tree"),
            &root.join("back"),
            Options::default(),
            true,
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect("moved back")
        .expect("a note");
        assert_eq!(noted.kind(), sk_model::ProblemKind::MergedInto, "{noted}");
        assert_eq!(std::fs::read(root.join("back/kept")).unwrap(), b"there");
        assert_eq!(std::fs::read(root.join("back/a")).unwrap(), b"moved");
        assert!(!root.join("tree").exists());
    }

    #[test]
    fn removing_a_tree_takes_all_of_it() {
        let root = scratch("remove-tree");
        let source = tree(&root);
        super::remove_tree(&source, &sk_util::Cancel::new()).expect("removed");
        assert!(!source.exists());
    }

    /// The failure behind a trash that would not empty: a folder unpacked from
    /// a tarball whose directories are `dr-x------`.
    #[test]
    fn removing_a_tree_takes_read_only_directories_we_own() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("remove-read-only");
        let source = root.join("ez130");
        let locked = source.join("doc");
        std::fs::create_dir_all(locked.join("deeper")).unwrap();
        std::fs::write(locked.join("notes.pdf"), b"%PDF").unwrap();
        std::fs::write(locked.join("deeper/inner.txt"), b"x").unwrap();
        for dir in [locked.join("deeper"), locked.clone()] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        super::remove_tree(&source, &sk_util::Cancel::new()).expect("removed");
        assert!(!source.exists(), "a read-only directory stopped the removal");
    }

    /// A cancelled copy stops and leaves what it wrote, because unwinding it
    /// would be a delete nobody asked for.
    #[test]
    fn a_cancelled_copy_stops_and_says_so() {
        let root = scratch("cancel");
        let source = tree(&root);
        let into = root.join("elsewhere");
        std::fs::create_dir(&into).unwrap();
        let cancel = sk_util::Cancel::new();
        cancel.cancel();
        let stopped = super::copy_into(
            &source,
            &into,
            Options::default(),
            &cancel,
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect_err("stopped");
        assert!(stopped.message().to_lowercase().contains("cancel"), "{}", stopped.message());
    }

    /// A device node or a fifo cannot be recreated without privilege, and a copy
    /// that skips one silently reports success while leaving something out.
    #[test]
    fn a_special_file_is_refused_rather_than_skipped() {
        let root = scratch("fifo");
        let source = root.join("from");
        std::fs::create_dir(&source).unwrap();
        let fifo = source.join("pipe");
        if rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_bits_truncate(0o600),
            0,
        )
        .is_err()
        {
            return;
        }
        let into = root.join("to");
        std::fs::create_dir(&into).unwrap();
        let refused = super::copy_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect_err("refused");
        assert!(refused.message().contains("device or a pipe"), "{}", refused.message());
    }

    #[test]
    fn remove_takes_a_file_and_an_empty_directory_and_refuses_a_full_one() {
        let root = scratch("remove");
        let file = root.join("file");
        write(&file, b"x");
        remove(&file).expect("removed the file");
        assert!(!file.exists());

        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        remove(&empty).expect("removed the directory");
        assert!(!empty.exists());

        let full = root.join("full");
        std::fs::create_dir(&full).unwrap();
        write(&full.join("inside"), b"x");
        assert!(remove(&full).is_err(), "a directory with something in it was removed");
        assert!(full.join("inside").exists());
    }

    #[test]
    fn cancellation_interrupts_one_large_file_and_discards_its_staging() {
        let root = scratch("mid-file-cancel");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, vec![42u8; 4 << 20]).unwrap();
        let options =
            super::Options { reflink: false, kernel_copy: false, ..super::Options::default() };
        let mut largest = 0;
        let parent = std::fs::File::open(&root).unwrap();
        let result = super::copy_regular(
            super::At { dir: parent.as_fd(), name: "source".as_ref(), path: &source },
            super::At { dir: parent.as_fd(), name: "destination".as_ref(), path: &destination },
            options,
            None,
            &mut |bytes| {
                largest = largest.max(bytes);
                bytes < 1 << 20
            },
        );
        assert!(result.is_err());
        assert_eq!(largest, 1 << 20);
        assert!(!destination.exists());
        assert_eq!(super::names(&parent.into(), &root).unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_link_published_move_removes_the_source_name() {
        let root = scratch("link-move");
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, "contents").unwrap();
        std::fs::hard_link(&source, &destination).unwrap();
        super::finish_move(&source, super::Placement::LinkThenUnlink).unwrap();
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "contents");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Three files in a folder, each holding `contents`.
    fn three(folder: &Path, contents: &str) {
        std::fs::create_dir_all(folder).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(folder.join(name), contents).unwrap();
        }
    }

    /// Copies `tree` under `root` into `root/into`, and once the first file has
    /// landed renames `swapped` away and puts a symlink to `target` in its place —
    /// which is what somebody who can write the folder above it can do.
    fn copy_with_a_swap(
        root: &Path,
        swapped: &Path,
        target: &Path,
    ) -> Result<(), sk_model::Problem> {
        let into = root.join("into");
        std::fs::create_dir_all(&into).unwrap();
        let mut armed = true;
        super::copy_into(
            &root.join("tree"),
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |progress| {
                if armed && progress.done == 1 {
                    armed = false;
                    std::fs::rename(swapped, root.join("renamed-away")).unwrap();
                    std::os::unix::fs::symlink(target, swapped).unwrap();
                }
            },
        )
        .map(|_| ())
    }

    /// The finding that made the walk descriptor-based: with paths, two of three
    /// files landed in the symlink's target and the copy called it a warning.
    #[test]
    fn a_destination_folder_swapped_for_a_symlink_mid_copy_redirects_nothing() {
        let root = scratch("swap-destination");
        three(&root.join("tree"), "mine");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();

        let copied = copy_with_a_swap(&root, &root.join("into/tree"), &elsewhere);
        assert!(walk(&elsewhere).is_empty(), "written through the link: {:?}", walk(&elsewhere));
        for name in ["a", "b", "c"] {
            let landed = root.join("renamed-away").join(name);
            assert_eq!(std::fs::read_to_string(landed).unwrap(), "mine");
        }
        copied.expect("copied");
    }

    /// The same swap on the side being read copies another folder's files out.
    #[test]
    fn a_source_folder_swapped_for_a_symlink_mid_copy_is_not_read_through() {
        let root = scratch("swap-source");
        three(&root.join("tree"), "mine");
        let secret = root.join("secret");
        three(&secret, "secret");

        let copied = copy_with_a_swap(&root, &root.join("tree"), &secret);
        for name in ["a", "b", "c"] {
            let landed = root.join("into/tree").join(name);
            assert_eq!(std::fs::read_to_string(landed).unwrap(), "mine", "{name}");
        }
        copied.expect("copied");
    }

    /// A name the walk saw as a file and then found to be a fifo is refused at
    /// once. Opening it without `O_NONBLOCK` waits for a writer forever, and
    /// without the `fstat` the copy would read whatever one wrote.
    #[test]
    fn a_file_replaced_after_it_was_looked_at_is_refused_and_never_waited_on() {
        use std::os::unix::fs::FileTypeExt as _;
        let root = scratch("swap-type");
        let looked_at = root.join("file");
        std::fs::write(&looked_at, b"bytes").unwrap();
        let seen =
            super::statat(super::fs::CWD, &looked_at, super::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::remove_file(&looked_at).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &looked_at,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_bits_truncate(0o600),
            0,
        )
        .unwrap();

        let parent = std::fs::File::open(&root).unwrap();
        let destination = root.join("copy");
        let refused = super::copy_regular(
            super::At { dir: parent.as_fd(), name: "file".as_ref(), path: &looked_at },
            super::At { dir: parent.as_fd(), name: "copy".as_ref(), path: &destination },
            Options::default(),
            Some(&seen),
            &mut |_| true,
        )
        .expect_err("a fifo is not the file that was looked at");
        assert!(refused.detail().contains("replaced"), "{}", refused.detail());
        assert!(!destination.exists());
        assert!(std::fs::symlink_metadata(&looked_at).unwrap().file_type().is_fifo());
    }

    /// A setuid binary out of somebody else's folder, copied, would be a setuid
    /// binary owned by whoever copied it — their rights, for anyone who runs it.
    /// Any setuid program on the machine that this user does not own is the
    /// fixture; there is none to find when the suite runs as root.
    #[test]
    fn somebody_elses_setuid_program_arrives_without_its_set_id_bits() {
        use std::os::unix::fs::MetadataExt as _;
        let me = rustix::process::geteuid().as_raw();
        let Some(program) = ["/usr/bin/passwd", "/usr/bin/su", "/usr/bin/mount", "/bin/su"]
            .into_iter()
            .map(Path::new)
            .find(|path| {
                std::fs::metadata(path).is_ok_and(|meta| {
                    meta.is_file() && meta.mode() & 0o4000 != 0 && meta.uid() != me
                }) && std::fs::File::open(path).is_ok()
            })
        else {
            return;
        };
        let root = scratch("foreign-setuid");
        let copy = root.join("copy");
        copy_file(program, &copy, Options::default()).expect("copied");
        let mode = std::fs::metadata(&copy).unwrap().mode();
        assert_eq!(mode & 0o6000, 0, "{} arrived as {mode:o}", program.display());
        assert_eq!(mode & 0o777, std::fs::metadata(program).unwrap().mode() & 0o777);
    }

    /// The torture matrix's `mode-4755` fixture: one's own setuid file is one's
    /// own grant, and it survives.
    #[test]
    fn ones_own_set_id_bits_survive_a_copy() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let root = scratch("own-setuid");
        let source = root.join("tool");
        std::fs::write(&source, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o6755)).unwrap();
        let copy = root.join("copy");
        copy_file(&source, &copy, Options::default()).expect("copied");
        assert_eq!(std::fs::metadata(&copy).unwrap().mode() & 0o7777, 0o6755);
    }

    /// An ACL with a named user in it, in the kernel's xattr encoding:
    /// `user::rw-`, `user:65534:rw-`, `group::` as `group`, `mask::rw-`,
    /// `other::---`.
    fn granting_acl(group: u16) -> Vec<u8> {
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (0x01u16, 6u16, u32::MAX),
            (0x02, 6, 65534),
            (0x04, group, u32::MAX),
            (0x10, 6, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        acl
    }

    /// The auditor's reproduction: `group::---` beside `user:…:rw-` puts the
    /// mask, `rw-`, in the mode's group bits. With the ACL left behind, the copy
    /// of somebody else's file took `0660` — read and write for a group the
    /// source shut out. The group bits now come from `group::` within the mask.
    #[test]
    fn a_foreign_acls_mask_does_not_become_the_copys_group_bits() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("acl-mask");
        let source = std::fs::File::create(root.join("source")).unwrap();
        std::fs::set_permissions(root.join("source"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let acl = rustix::fs::fsetxattr(
            &source,
            "system.posix_acl_access",
            &granting_acl(0),
            rustix::fs::XattrFlags::empty(),
        );
        if acl.is_err() {
            return;
        }
        let stat = rustix::fs::fstat(&source).unwrap();
        assert_eq!(stat.st_mode & 0o777, 0o660, "the mask shows as the group bits");
        let copy = std::fs::File::create(root.join("copy")).unwrap();
        assert_eq!(super::carried_mode(&source, &stat, true, &copy).bits() & 0o777, 0o600);
        assert_eq!(super::carried_mode(&source, &stat, false, &copy).bits() & 0o777, 0o660);
    }

    /// What a [`super::foreign`] source's ACL would do on the copy: let the
    /// person it names in. Left behind; every other attribute still goes.
    #[test]
    fn a_foreign_files_acl_is_left_behind_and_its_other_attributes_are_not() {
        let root = scratch("foreign-acl");
        let source = std::fs::File::create(root.join("source")).unwrap();
        let acl = rustix::fs::fsetxattr(
            &source,
            "system.posix_acl_access",
            &granting_acl(4),
            rustix::fs::XattrFlags::empty(),
        );
        let user =
            rustix::fs::fsetxattr(&source, "user.origin", b"here", rustix::fs::XattrFlags::empty());
        if acl.is_err() || user.is_err() {
            return;
        }
        let names = |file: &std::fs::File| {
            let mut buffer = vec![0u8; 4096];
            let length = rustix::fs::flistxattr(file, &mut buffer[..]).unwrap();
            String::from_utf8_lossy(&buffer[..length]).into_owned()
        };
        let own = std::fs::File::create(root.join("own")).unwrap();
        let mut missing = Vec::new();
        super::copy_xattrs(&source, &own, &mut missing, true);
        assert!(names(&own).contains("system.posix_acl_access"), "{missing:?}");

        let theirs = std::fs::File::create(root.join("theirs")).unwrap();
        super::copy_xattrs(&source, &theirs, &mut missing, false);
        assert!(!names(&theirs).contains("posix_acl"), "{}", names(&theirs));
        assert!(names(&theirs).contains("user.origin"), "{}", names(&theirs));
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// `copy_file` copies a regular file and nothing that merely leads to one.
    #[test]
    fn copy_file_refuses_a_symlink_rather_than_following_it() {
        let root = scratch("copy-file-link");
        std::fs::write(root.join("target"), b"not this").unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("link")).unwrap();
        assert!(copy_file(&root.join("link"), &root.join("copy"), Options::default()).is_err());
        assert!(!root.join("copy").exists());
    }

    /// Deleting a folder for good used to delete what was mounted inside it.
    ///
    /// Mounting needs privilege, so this re-runs the test binary inside
    /// `unshare -Urm`, where an ordinary user may mount into a namespace of
    /// their own: a bind mount of a folder on the same filesystem, which only
    /// `STATX_ATTR_MOUNT_ROOT` can see, and a tmpfs, which `st_dev` can. It
    /// returns where namespaces are unavailable, as it has nothing to mount.
    #[test]
    fn a_mount_inside_a_deleted_folder_is_left_standing_with_everything_on_it() {
        let can_unshare = std::process::Command::new("unshare")
            .args(["-Urm", "true"])
            .output()
            .is_ok_and(|out| out.status.success());
        if !can_unshare {
            return;
        }
        let root = scratch("mounted-inside");
        std::fs::create_dir_all(root.join("doomed/bound")).unwrap();
        std::fs::create_dir_all(root.join("doomed/tmp")).unwrap();
        std::fs::write(root.join("doomed/plain.txt"), b"goes").unwrap();
        std::fs::create_dir(root.join("precious")).unwrap();
        std::fs::write(root.join("precious/keep"), b"stays").unwrap();
        let script = r#"mount --bind "$1/precious" "$1/doomed/bound" \
            && mount -t tmpfs none "$1/doomed/tmp" && echo on-tmpfs > "$1/doomed/tmp/inside" \
            && exec "$2" --exact copy::tests::inside_a_namespace_a_mount_is_left_standing --ignored"#;
        let out = std::process::Command::new("unshare")
            .args(["-Urm", "sh", "-c", script, "sh"])
            .arg(&root)
            .arg(std::env::current_exe().unwrap())
            .env("SKERRY_MOUNT_TEST_ROOT", &root)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.contains("1 passed"), "{said}");
        assert_eq!(std::fs::read(root.join("precious/keep")).unwrap(), b"stays");
    }

    #[test]
    #[ignore = "run inside a mount namespace by the test above"]
    fn inside_a_namespace_a_mount_is_left_standing() {
        let Some(root) = std::env::var_os("SKERRY_MOUNT_TEST_ROOT").map(PathBuf::from) else {
            return;
        };
        let doomed = root.join("doomed");
        let left = super::remove_tree(&doomed, &sk_util::Cancel::new()).expect_err("left standing");
        assert_eq!(left.kind(), sk_model::ProblemKind::MountedInside, "{left}");
        assert!(left.detail().starts_with("2 left standing"), "{left}");
        assert!(!doomed.join("plain.txt").exists(), "what was not mounted still goes");
        assert_eq!(std::fs::read(doomed.join("bound/keep")).unwrap(), b"stays");
        assert_eq!(std::fs::read(doomed.join("tmp/inside")).unwrap(), b"on-tmpfs\n");
    }

    #[test]
    fn a_directory_swapped_after_stat_cannot_redirect_recursive_deletion() {
        let root = scratch("remove-swap");
        let selected = root.join("selected");
        let outside = root.join("outside");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "valuable").unwrap();
        let parent = std::fs::File::open(&root).unwrap();
        let observed =
            super::statat(&parent, "selected", super::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        std::fs::rename(&selected, root.join("old")).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert!(
            super::Removal {
                cancel: &sk_util::Cancel::new(),
                only: None,
                left: super::Left::default(),
            }
            .directory(
                parent.as_fd(),
                Path::new("selected"),
                &selected,
                0,
                &observed,
                observed.st_dev
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(outside.join("sentinel")).unwrap(), "valuable");
        std::fs::remove_file(&selected).unwrap();
        std::fs::rename(&outside, &selected).unwrap();
        assert!(
            super::Removal {
                cancel: &sk_util::Cancel::new(),
                only: None,
                left: super::Left::default(),
            }
            .directory(
                parent.as_fd(),
                Path::new("selected"),
                &selected,
                0,
                &observed,
                observed.st_dev
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(selected.join("sentinel")).unwrap(), "valuable");
        std::fs::remove_dir_all(root).unwrap();
    }
}
