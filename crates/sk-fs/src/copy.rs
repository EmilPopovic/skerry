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
/// decisions they need are policy and this is mechanism.
///
/// **Runs on a job.**
pub fn copy_file(source: &Path, destination: &Path, options: Options) -> Result<Copied, Problem> {
    copy_file_progress(source, destination, options, &mut |_| true)
}

fn copy_file_progress(
    source: &Path,
    destination: &Path,
    options: Options,
    checkpoint: &mut dyn FnMut(u64) -> bool,
) -> Result<Copied, Problem> {
    let cwd = fs::CWD;
    let from = openat(cwd, source, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|error| at(error, source, "open"))?;
    let stat = fstat(&from).map_err(|error| at(error, source, "stat"))?;

    // The classic truncation bug: `cp a a` that opens the destination for writing
    // empties the file before it reads anything. Comparing device and inode
    // catches it through a symlink, a hardlink and a bind mount alike, where a
    // path comparison does not.
    if let Ok(there) = statat(cwd, destination, AtFlags::empty()) {
        if same_file(&stat, &there) {
            return Err(at(Errno::INVAL, destination, "the same file"));
        }
        if !options.replace {
            return Err(at(Errno::EXIST, destination, "place"));
        }
    }

    let temp = temporary(destination);
    // `O_EXCL`: a name that already exists means a collision with another
    // operation, and that file must not be touched.
    let into = openat(
        cwd,
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| at(error, destination, "create"))?;

    let size = stat.st_size.cast_unsigned();
    let mut refused = Vec::new();
    let (strategy, bytes) =
        match write_contents(&from, &into, size, options, &mut refused, checkpoint) {
            Ok(written) => written,
            Err((error, stage)) => return Err(discard(&temp, error, destination, stage)),
        };

    let mut unpreserved = Vec::new();
    if fchmod(&into, Mode::from_bits_truncate(stat.st_mode & 0o7777)).is_err() {
        unpreserved.push("mode".to_owned());
    }
    if options.xattrs {
        copy_xattrs(&from, &into, &mut unpreserved);
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

    let (placement, orphan) = place(&temp, destination, options.replace)
        .map_err(|error| discard(&temp, error, destination, "place"))?;
    Ok(Copied { strategy, placement, bytes, unpreserved, orphan, refused })
}

/// Moves one file, which is two entirely different operations behind one word.
///
/// Within a filesystem it is a rename: atomic, instant, and it cannot lose
/// anything. Across filesystems it is a copy followed by an unlink, and **the
/// source is removed only after the copy has been placed**. A move that unlinks
/// first, or that trusts a return value it did not check, is the classic way a
/// file manager destroys a file.
///
/// **Runs on a job.**
pub fn move_file(
    source: &Path,
    destination: &Path,
    options: Options,
) -> Result<Placement, Problem> {
    let cwd = fs::CWD;
    match place_existing(source, destination, options.replace) {
        Ok(placement) => {
            finish_move(source, placement)?;
            return Ok(placement);
        }
        // Different filesystems. Every other error is the caller's answer.
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, destination, "rename")),
    }
    let copied = copy_file(source, destination, options)?;
    if !copied.unpreserved.is_empty() || copied.orphan.is_some() {
        return Err(Problem::new(
            ProblemKind::Preservation,
            destination.display().to_string(),
            format!("{:?}", copied.unpreserved),
        ));
    }
    unlinkat(cwd, source, AtFlags::empty()).map_err(|error| at(error, source, "unlink"))?;
    Ok(copied.placement)
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
    copy_any(source, &destination, options, cancel, progress, watch, 0)?;
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
    copy_any(source, destination, options, cancel, &mut progress, watch, 0)
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
/// **Runs on a job.**
pub fn move_into(
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
    match place_existing(source, &destination, options.replace) {
        Ok(placement) => {
            finish_move(source, placement)?;
            progress.done += 1;
            return Ok(destination);
        }
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, &destination, "rename")),
    }
    copy_any(source, &destination, options, cancel, progress, watch, 0)?;
    // Only now, and only because the copy above returned `Ok`.
    remove_tree(source, cancel)?;
    Ok(destination)
}

/// Moves a file or directory to the exact recorded destination.
pub fn move_to(
    source: &Path,
    destination: &Path,
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(Progress),
) -> Result<(), Problem> {
    match place_existing(source, destination, false) {
        Ok(placement) => return finish_move(source, placement),
        Err(Errno::XDEV) => {}
        Err(error) => return Err(at(error, destination, "rename")),
    }
    copy_to(source, destination, cancel, watch)?;
    remove_tree(source, cancel)
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
    let parent = path.parent().ok_or_else(|| at(Errno::INVAL, path, "no parent"))?;
    let name = path.file_name().ok_or_else(|| at(Errno::INVAL, path, "no name"))?;
    let fd = openat(
        fs::CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| at(e, parent, "open parent"))?;
    remove_at(&fd, Path::new(name), path, cancel, 0)
}

fn remove_at(
    parent: &impl std::os::fd::AsFd,
    name: &Path,
    subject: &Path,
    cancel: &sk_util::Cancel,
    depth: u32,
) -> Result<(), Problem> {
    if cancel.is_cancelled() {
        return Err(cancelled(subject));
    }
    if depth > MAX_DEPTH {
        return Err(too_deep(subject));
    }
    let stat =
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, subject, "stat"))?;
    if fs::FileType::from_raw_mode(stat.st_mode) != fs::FileType::Directory {
        return unlinkat(parent, name, AtFlags::empty()).map_err(|e| at(e, subject, "remove"));
    }
    remove_directory(parent, name, subject, cancel, depth, &stat)
}

fn remove_directory(
    parent: &impl std::os::fd::AsFd,
    name: &Path,
    subject: &Path,
    cancel: &sk_util::Cancel,
    depth: u32,
    observed: &Stat,
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
    let dir = fs::Dir::read_from(&fd).map_err(|e| at(e, subject, "read directory"))?;
    for entry in dir {
        let entry = entry.map_err(|e| at(e, subject, "read directory"))?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let child = Path::new(std::ffi::OsStr::from_bytes(bytes));
        remove_at(&fd, child, &subject.join(child), cancel, depth + 1)?;
    }
    let current =
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, subject, "stat"))?;
    if !same_file(&opened, &current) {
        return Err(at(Errno::BUSY, subject, "directory changed"));
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(|e| at(e, subject, "remove directory"))
}

/// One entry, whatever kind it is, at a destination path that must not exist.
fn copy_any(
    source: &Path,
    destination: &Path,
    options: Options,
    cancel: &sk_util::Cancel,
    progress: &mut Progress,
    watch: &mut dyn FnMut(Progress),
    depth: u32,
) -> Result<(), Problem> {
    if cancel.is_cancelled() {
        return Err(cancelled(source));
    }
    if depth > MAX_DEPTH {
        return Err(too_deep(source));
    }
    let stat =
        statat(fs::CWD, source, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| at(e, source, "stat"))?;
    match fs::FileType::from_raw_mode(stat.st_mode) {
        fs::FileType::RegularFile => {
            // Already there and the same size: what a stopped copy got to.
            // Counted as done, because it is — the progress line is about the
            // work the queue was given rather than about this attempt at it.
            if options.resume && same_size(destination, stat.st_size) {
                progress.done += 1;
                progress.bytes += stat.st_size.cast_unsigned();
                watch(*progress);
                return Ok(());
            }
            let base = *progress;
            let copied = copy_file_progress(source, destination, options, &mut |bytes| {
                watch(Progress { bytes: base.bytes + bytes, ..base });
                !cancel.is_cancelled()
            })?;
            progress.done += 1;
            progress.bytes += stat.st_size.cast_unsigned();
            watch(*progress);
            if !copied.unpreserved.is_empty() || copied.orphan.is_some() {
                return Err(Problem::new(
                    ProblemKind::Preservation,
                    destination.display().to_string(),
                    format!(
                        "not preserved: {:?}; leftover: {:?}",
                        copied.unpreserved, copied.orphan
                    ),
                ));
            }
            Ok(())
        }
        // Recreated rather than followed. A link is 40 bytes and what it points at
        // may be a terabyte, or may be the directory this walk is inside.
        fs::FileType::Symlink => {
            if options.resume && statat(fs::CWD, destination, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
                progress.done += 1;
                watch(*progress);
                return Ok(());
            }
            let target = fs::readlinkat(fs::CWD, source, Vec::new())
                .map_err(|error| at(error, source, "readlink"))?;
            fs::symlinkat(target.as_c_str(), fs::CWD, destination)
                .map_err(|error| at(error, destination, "symlink"))?;
            progress.done += 1;
            watch(*progress);
            Ok(())
        }
        fs::FileType::Directory => {
            copy_directory(source, destination, options, cancel, progress, watch, depth, &stat)
        }
        // A fifo, a socket or a device. Recreating one needs `mknod` and, for a
        // device, privilege this process does not have — and skipping it silently
        // is how a copy reports success while leaving something out.
        _ => Err(Problem::new(
            ProblemKind::NotCopyable,
            source.display().to_string(),
            "not a file, a folder or a link",
        )),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one walk's worth of state, and \
    bundling it into a struct would put a lifetime on every recursive call"
)]
fn copy_directory(
    source: &Path,
    destination: &Path,
    options: Options,
    cancel: &sk_util::Cancel,
    progress: &mut Progress,
    watch: &mut dyn FnMut(Progress),
    depth: u32,
    stat: &Stat,
) -> Result<(), Problem> {
    // 0o700 while it is being filled, so a half-copied directory is not readable
    // by anyone who was not already allowed to read the original. The real mode
    // goes on at the end.
    match fs::mkdir(destination, Mode::from_bits_truncate(0o700)) {
        Ok(()) => {}
        Err(Errno::EXIST) if options.replace || options.resume => {}
        Err(error) => return Err(at(error, destination, "mkdir")),
    }
    // Keep the first warning bounded in memory, but finish every child. A
    // preservation warning describes published data; other errors stop the walk.
    let mut warning = None;
    for child in children(source)? {
        let result = copy_any(
            &source.join(&child),
            &destination.join(&child),
            options,
            cancel,
            progress,
            watch,
            depth + 1,
        );
        if let Err(problem) = result {
            if problem.kind() != ProblemKind::Preservation {
                return Err(problem);
            }
            warning.get_or_insert(problem);
        }
    }
    // Mode and times after the contents, for the reason a file's are: writing a
    // child moves the directory's modification time.
    let mode = Mode::from_bits_truncate(stat.st_mode & 0o7777);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let from = openat(fs::CWD, source, flags, Mode::empty())
        .map_err(|error| at(error, source, "metadata"))?;
    let into = openat(fs::CWD, destination, flags, Mode::empty())
        .map_err(|error| at(error, destination, "metadata"))?;
    let mut missing = Vec::new();
    if fchmod(&into, mode).is_err() {
        missing.push("mode".to_owned());
    }
    if options.xattrs {
        copy_xattrs(&from, &into, &mut missing);
    }
    if futimens(&into, &times_of(stat)).is_err() {
        missing.push("times".to_owned());
    }
    if !missing.is_empty() {
        warning.get_or_insert_with(|| {
            Problem::new(
                ProblemKind::Preservation,
                destination.display().to_string(),
                missing.join(", "),
            )
        });
    }
    progress.done += 1;
    watch(*progress);
    warning.map_or(Ok(()), Err)
}

/// Whether `path` is a regular file of exactly `bytes`.
///
/// `SYMLINK_NOFOLLOW`, so a symlink where a file should be is not the file: it
/// is something else with the same name, and copying over it is the right
/// answer rather than skipping it.
fn same_size(path: &Path, bytes: i64) -> bool {
    statat(fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|stat| {
        fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::RegularFile
            && stat.st_size == bytes
    })
}

/// Every name in a directory except `.` and `..`.
///
/// Collected rather than streamed: the walk below creates files in the tree it is
/// reading from when a copy goes into a sibling directory, and a `getdents64`
/// cursor over a directory being written to may or may not see them.
fn children(directory: &Path) -> Result<Vec<PathBuf>, Problem> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = openat(fs::CWD, directory, flags, Mode::empty())
        .map_err(|error| at(error, directory, "open"))?;
    let dir = fs::Dir::read_from(&fd).map_err(|error| at(error, directory, "read"))?;
    let mut out = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|e| at(e, directory, "read"))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        // Bytes, not `str`: a filename is not required to be UTF-8, and a copy
        // that skips the ones that are not is a copy that loses files.
        let name = <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(name.to_vec());
        out.push(PathBuf::from(name));
    }
    Ok(out)
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
fn temporary(destination: &Path) -> PathBuf {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    parent.join(format!(".skerry-copy-{}-{nanos}", rustix::process::getpid().as_raw_nonzero()))
}

/// Gives the finished temporary file its final name without ever overwriting
/// something nobody asked to overwrite.
fn place(
    temp: &Path,
    destination: &Path,
    replace: bool,
) -> Result<(Placement, Option<PathBuf>), Errno> {
    let cwd = fs::CWD;
    match place_existing(temp, destination, replace) {
        Ok(Placement::LinkThenUnlink) => {
            // The unlink can fail, and ignoring it leaves a `.skerry-copy-*` name
            // in the destination directory *and* inflates the link count of what
            // is supposed to be a clean copy.
            let orphan = unlinkat(cwd, temp, AtFlags::empty()).err().map(|_| temp.to_path_buf());
            Ok((Placement::LinkThenUnlink, orphan))
        }
        Ok(placement) => Ok((placement, None)),
        Err(error) => Err(error),
    }
}

/// The placement chain, over a path that already exists.
///
/// Shared by [`move_file`], which renames the source itself, and by [`place`],
/// which renames a temporary file. `LinkThenUnlink` leaves the source linked in
/// two places and it is the caller's job to unlink it — which is what lets
/// `move_file` treat the link count honestly.
fn place_existing(from: &Path, to: &Path, replace: bool) -> Result<Placement, Errno> {
    let cwd = fs::CWD;
    if replace {
        renameat_with(cwd, from, cwd, to, RenameFlags::empty())?;
        return Ok(Placement::Replace);
    }
    match renameat_with(cwd, from, cwd, to, RenameFlags::NOREPLACE) {
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
    match fs::linkat(cwd, from, cwd, to, AtFlags::empty()) {
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
pub(crate) fn copy_xattrs(
    from: &impl std::os::fd::AsFd,
    into: &impl std::os::fd::AsFd,
    out: &mut Vec<String>,
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
fn discard(temp: &Path, error: Errno, subject: &Path, stage: &'static str) -> Problem {
    if unlinkat(fs::CWD, temp, AtFlags::empty()).is_err() {
        return Problem::new(
            ProblemKind::Unreadable,
            subject.display().to_string(),
            format!("{stage}: {error}, and {} could not be removed", temp.display()),
        );
    }
    at(error, subject, stage)
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
        let landed = super::move_into(
            &source,
            &into,
            Options::default(),
            &sk_util::Cancel::new(),
            &mut super::Progress::default(),
            &mut |_| {},
        )
        .expect("moved");
        assert!(!source.exists(), "the source survived");
        assert_eq!(walk(&landed), shape);
    }

    #[test]
    fn removing_a_tree_takes_all_of_it() {
        let root = scratch("remove-tree");
        let source = tree(&root);
        super::remove_tree(&source, &sk_util::Cancel::new()).expect("removed");
        assert!(!source.exists());
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
        let result = super::copy_file_progress(&source, &destination, options, &mut |bytes| {
            largest = largest.max(bytes);
            bytes < 1 << 20
        });
        assert!(result.is_err());
        assert_eq!(largest, 1 << 20);
        assert!(!destination.exists());
        assert_eq!(super::children(&root).unwrap().len(), 1);
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
            super::remove_directory(
                &parent,
                Path::new("selected"),
                &selected,
                &sk_util::Cancel::new(),
                0,
                &observed
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(outside.join("sentinel")).unwrap(), "valuable");
        std::fs::remove_file(&selected).unwrap();
        std::fs::rename(&outside, &selected).unwrap();
        assert!(
            super::remove_directory(
                &parent,
                Path::new("selected"),
                &selected,
                &sk_util::Cancel::new(),
                0,
                &observed
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(selected.join("sentinel")).unwrap(), "valuable");
        std::fs::remove_dir_all(root).unwrap();
    }
}
