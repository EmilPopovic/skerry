//! The copy engine: `FICLONE`, `copy_file_range`, and a hole-preserving
//! read/write floor, each probed at runtime rather than chosen by a `#[cfg]`
//! ([ADR 0007](../../docs/adr/0007-runtime-capability-detection.md)).
//!
//! One structural decision here is what makes the bar's second condition
//! provable rather than hoped for: **nothing is ever written to the destination
//! name.** Every copy goes to a temporary file in the destination directory and
//! is renamed into place at the end. If any stage fails — including `ENOSPC` at
//! the last byte — the temporary file is unlinked and the destination never
//! existed. "No partial file left behind" then stops being a promise about error
//! handling and becomes a property of the algorithm.

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{
    self, fchmod, fstat, ftruncate, futimens, openat, renameat_with, seek, statat, unlinkat,
    AtFlags, Mode, OFlags, RenameFlags, SeekFrom, Stat, Timespec, Timestamps,
};
use rustix::io::Errno;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    /// `FICLONE`: the filesystem shares the extents and copies no data.
    Reflink,
    /// `copy_file_range`: the kernel moves the bytes without a round trip
    /// through user space, and preserves holes where the filesystem can.
    CopyFileRange,
    /// The floor. Holes are preserved by seeking over them, which every
    /// filesystem that implements `SEEK_HOLE` supports.
    ReadWrite,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::Reflink => "FICLONE",
            Strategy::CopyFileRange => "copy_file_range",
            Strategy::ReadWrite => "read/write",
        }
    }
}

pub struct Options {
    pub reflink: bool,
    pub kernel_copy: bool,
    /// Whether an existing destination may be replaced. `false` uses
    /// `RENAME_NOREPLACE`, so the refusal is atomic rather than a check followed
    /// by a race.
    pub replace: bool,
    pub preserve_xattrs: bool,
    pub fsync: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reflink: true,
            kernel_copy: true,
            replace: false,
            preserve_xattrs: true,
            fsync: false,
        }
    }
}

/// How the finished temporary file was given its final name. Recorded because one
/// of these three is not atomic, and a caller has to be able to know that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placement {
    /// `renameat2(RENAME_NOREPLACE)` — atomic, and refuses an existing
    /// destination in the same syscall.
    RenameNoReplace,
    /// `link` then `unlink` — also atomic, because `link` refuses an existing
    /// destination, and it needs no flag the filesystem might not have.
    LinkThenUnlink,
    /// `stat` then `rename`. **Not atomic**: a file created between the two calls
    /// is overwritten. Used only where neither of the above works.
    CheckThenRename,
    /// A deliberate replacement, which needs no protection.
    Replace,
}

#[derive(Debug)]
pub struct Copied {
    pub strategy: Strategy,
    /// Which of the three ways the file got its name.
    pub placement: Placement,
    /// Whether the source had holes. Recorded because the answer decides whether
    /// the sparse checks mean anything, and because a strategy that is correct on
    /// a dense file can be wrong on a sparse one.
    pub sparse_source: bool,
    /// Every strategy that was tried and refused, with the errno it refused
    /// with. This is the only way to tell "the filesystem cannot reflink" from
    /// "we never asked".
    pub refused: Vec<(Strategy, Errno)>,
    pub bytes: u64,
    pub elapsed: Duration,
    /// Metadata the destination filesystem would not accept, reported rather
    /// than dropped — [05-platform-integration.md](../../docs/05-platform-integration.md)
    /// requires the report.
    pub unpreserved: Vec<(String, Errno)>,
    /// Set when the placement succeeded and the temporary file survived.
    pub orphan: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Failed {
    pub errno: Errno,
    pub stage: &'static str,
    pub bytes_written: u64,
    /// A path here is a **bug**: it is a partial file we could not clean up.
    pub leftover: Option<PathBuf>,
}

/// A temporary file that could not be removed after a *successful* copy. Ignoring
/// this is what left a `.skerry-copy-*` name on NTFS and made a three-name
/// hardlink set report a link count of four.
#[derive(Debug)]
pub struct Orphan(pub PathBuf);

/// Copy one regular file. Symlinks, directories and hardlink sets are the
/// caller's problem, because the decisions they need are policy and this is
/// mechanism.
pub fn copy_file(src: &Path, dst: &Path, o: &Options) -> Result<Copied, Failed> {
    let started = Instant::now();
    let cwd = rustix::fs::CWD;

    let src_fd = openat(cwd, src, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| fail(e, "open-source", 0, None))?;
    let src_stat = fstat(&src_fd).map_err(|e| fail(e, "stat-source", 0, None))?;

    // The classic truncation bug: `cp a a` that opens the destination for
    // writing empties the file before it reads anything. Comparing (dev, ino)
    // catches it through a symlink, a hardlink and a bind mount alike, which a
    // path comparison does not.
    if let Ok(dst_stat) = statat(cwd, dst, AtFlags::empty()) {
        if same_file(&src_stat, &dst_stat) {
            return Err(fail(Errno::INVAL, "same-file", 0, None));
        }
        if !o.replace {
            return Err(fail(Errno::EXIST, "destination-exists", 0, None));
        }
    }

    let parent = dst.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let temp = parent.join(format!(
        ".skerry-copy-{}-{}",
        rustix::process::getpid().as_raw_nonzero(),
        started.elapsed().subsec_nanos()
    ));

    // O_EXCL: if the name exists we have collided with another operation and must
    // not touch it.
    let dst_fd = openat(
        cwd,
        &temp,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|e| fail(e, "create-temp", 0, None))?;

    let mut refused = Vec::new();
    let size = src_stat.st_size as u64;

    let result = write_contents(&src_fd, &dst_fd, size, o, &mut refused);
    let (strategy, bytes) = match result {
        Ok(v) => v,
        Err((errno, stage, bytes)) => {
            return Err(discard(&temp, errno, stage, bytes));
        }
    };

    let mut unpreserved = Vec::new();
    if o.preserve_xattrs {
        copy_xattrs(&src_fd, &dst_fd, &mut unpreserved);
    }
    if let Err(e) = fchmod(&dst_fd, Mode::from_bits_truncate(src_stat.st_mode & 0o7777)) {
        unpreserved.push(("mode".into(), e));
    }
    if o.fsync {
        if let Err(e) = rustix::fs::fsync(&dst_fd) {
            return Err(discard(&temp, e, "fsync", bytes));
        }
    }
    // Timestamps last. Every write above moves mtime, so setting it earlier sets
    // it to a value the next write overwrites — which is the bug that makes every
    // "is the copy newer than the original" comparison wrong.
    let times = Timestamps {
        last_access: Timespec { tv_sec: src_stat.st_atime as _, tv_nsec: src_stat.st_atime_nsec as _ },
        last_modification: Timespec {
            tv_sec: src_stat.st_mtime as _,
            tv_nsec: src_stat.st_mtime_nsec as _,
        },
    };
    if let Err(e) = futimens(&dst_fd, &times) {
        unpreserved.push(("times".into(), e));
    }
    drop(dst_fd);

    let (placement, orphan) = match place(&temp, dst, o.replace) {
        Ok(p) => p,
        Err(e) => return Err(discard(&temp, e, "rename", bytes)),
    };

    Ok(Copied {
        strategy,
        placement,
        orphan,
        sparse_source: (src_stat.st_blocks as u64) * 512 < size,
        refused,
        bytes,
        elapsed: started.elapsed(),
        unpreserved,
    })
}

/// Give the finished temporary file its final name without ever overwriting
/// something we were not asked to overwrite.
///
/// `RENAME_NOREPLACE` is the right tool and it is **not universally supported**:
/// ntfs-3g returns `EINVAL` for it, and an engine that stops there copies nothing
/// at all to an external drive. That was this spike's first real finding about
/// its own code, and the fallback is the classic one — `link` refuses an existing
/// destination atomically and needs no flag.
fn place(temp: &Path, dst: &Path, replace: bool) -> Result<(Placement, Option<PathBuf>), Errno> {
    let cwd = rustix::fs::CWD;
    if replace {
        renameat_with(cwd, temp, cwd, dst, RenameFlags::empty())?;
        return Ok((Placement::Replace, None));
    }
    match renameat_with(cwd, temp, cwd, dst, RenameFlags::NOREPLACE) {
        Ok(()) => return Ok((Placement::RenameNoReplace, None)),
        // The destination is really there. Nothing else to try.
        Err(Errno::EXIST) => return Err(Errno::EXIST),
        // The *flag* is unsupported, which is a different thing entirely and is
        // reported with the same errno on some filesystems.
        Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::OPNOTSUPP) => {}
        Err(e) => return Err(e),
    }
    match fs::linkat(cwd, temp, cwd, dst, AtFlags::empty()) {
        Ok(()) => {
            // The unlink can fail, and ignoring it leaves a `.skerry-copy-*` name
            // in the destination directory *and* inflates the link count of what
            // is supposed to be a clean copy.
            let orphan = unlinkat(cwd, temp, AtFlags::empty()).err().map(|_| temp.to_path_buf());
            return Ok((Placement::LinkThenUnlink, orphan));
        }
        Err(Errno::EXIST) => return Err(Errno::EXIST),
        Err(_) => {}
    }
    // Neither the flag nor hardlinks. This is the only remaining option and it
    // has a window; the caller is told which path was taken so the window is not
    // a secret.
    if statat(cwd, dst, AtFlags::empty()).is_ok() {
        return Err(Errno::EXIST);
    }
    renameat_with(cwd, temp, cwd, dst, RenameFlags::empty())?;
    Ok((Placement::CheckThenRename, None))
}

/// Try each strategy in turn, recording why the earlier ones were refused.
fn write_contents(
    src: &OwnedFd,
    dst: &OwnedFd,
    size: u64,
    o: &Options,
    refused: &mut Vec<(Strategy, Errno)>,
) -> Result<(Strategy, u64), (Errno, &'static str, u64)> {
    if o.reflink {
        match fs::ioctl_ficlone(dst.as_fd(), src.as_fd()) {
            Ok(()) => return Ok((Strategy::Reflink, size)),
            // Not an error: this filesystem cannot share extents, which is the
            // common case and the reason the strategy is probed and not assumed.
            Err(e) => refused.push((Strategy::Reflink, e)),
        }
    }
    if o.kernel_copy {
        match kernel_copy(src, dst, size) {
            Ok(n) => return Ok((Strategy::CopyFileRange, n)),
            Err((e, n)) if n > 0 => return Err((e, "copy_file_range", n)),
            Err((e, _)) => refused.push((Strategy::CopyFileRange, e)),
        }
    }
    read_write(src, dst, size).map(|n| (Strategy::ReadWrite, n))
}

/// `copy_file_range`, **per data extent**.
///
/// The obvious loop — one `copy_file_range` over the whole file — is what the
/// first version of this did, and it turned a 1 GiB sparse file with 12 KiB of
/// data into 1 GiB of zeros on both ext4 and tmpfs. `copy_file_range` does not
/// promise to preserve holes; where it cannot share extents it materialises them.
///
/// Seeking to each data extent and copying only that keeps the kernel-side copy
/// and the sparseness at once, so there is no speed-versus-correctness trade to
/// make here.
fn kernel_copy(src: &OwnedFd, dst: &OwnedFd, size: u64) -> Result<u64, (Errno, u64)> {
    let mut total = 0u64;
    let mut cursor = 0u64;
    while cursor < size {
        let data = match seek(src.as_fd(), SeekFrom::Data(cursor)) {
            Ok(d) => d,
            // No data at or after the cursor: the rest of the file is a hole.
            Err(Errno::NXIO) => break,
            Err(e) => return Err((e, total)),
        };
        let hole = seek(src.as_fd(), SeekFrom::Hole(data)).unwrap_or(size).min(size);
        let mut off_in = data;
        let mut off_out = data;
        while off_in < hole {
            let want = (hole - off_in).min(1 << 30) as usize;
            match fs::copy_file_range(
                src.as_fd(),
                Some(&mut off_in),
                dst.as_fd(),
                Some(&mut off_out),
                want,
            ) {
                Ok(0) => break,
                Ok(n) => total += n as u64,
                Err(e) => return Err((e, total)),
            }
        }
        cursor = hole;
    }
    // A file that ends in a hole, or is entirely a hole, still has a length.
    ftruncate(dst.as_fd(), size).map_err(|e| (e, total))?;
    Ok(total)
}

/// The floor, and the only path that has to think about holes itself.
/// `SEEK_DATA` past the last extent returns `ENXIO`, which is how the loop ends.
fn read_write(src: &OwnedFd, dst: &OwnedFd, size: u64) -> Result<u64, (Errno, &'static str, u64)> {
    let mut file = std::fs::File::from(src.try_clone().map_err(|e| (errno_of(&e), "dup", 0))?);
    let mut out = std::fs::File::from(dst.try_clone().map_err(|e| (errno_of(&e), "dup", 0))?);
    let mut buf = vec![0u8; 1 << 20];
    let mut written = 0u64;
    let mut cursor = 0u64;

    while cursor < size {
        let data = match seek(src.as_fd(), SeekFrom::Data(cursor)) {
            Ok(d) => d,
            Err(Errno::NXIO) => break,
            Err(e) => return Err((e, "seek-data", written)),
        };
        let hole = seek(src.as_fd(), SeekFrom::Hole(data)).unwrap_or(size).min(size);
        let mut at = data;
        while at < hole {
            let want = ((hole - at) as usize).min(buf.len());
            use std::io::{Seek as _, SeekFrom as StdSeek};
            file.seek(StdSeek::Start(at)).map_err(|e| (errno_of(&e), "seek", written))?;
            let n = file.read(&mut buf[..want]).map_err(|e| (errno_of(&e), "read", written))?;
            if n == 0 {
                break;
            }
            out.seek(StdSeek::Start(at)).map_err(|e| (errno_of(&e), "seek", written))?;
            out.write_all(&buf[..n]).map_err(|e| (errno_of(&e), "write", written))?;
            at += n as u64;
            written += n as u64;
        }
        cursor = hole;
    }
    // The destination must be the source's length even when the source ends in a
    // hole, and even when it is entirely a hole.
    ftruncate(dst.as_fd(), size).map_err(|e| (e, "truncate", written))?;
    // Keep the borrowed descriptors alive rather than closing them here: they are
    // dups, and the caller still owns the originals.
    std::mem::forget(file);
    std::mem::forget(out);
    Ok(written)
}

fn copy_xattrs(src: &OwnedFd, dst: &OwnedFd, unpreserved: &mut Vec<(String, Errno)>) {
    let mut names = vec![0u8; 64 * 1024];
    let len = match fs::flistxattr(src.as_fd(), &mut names[..]) {
        Ok(n) => n,
        // No xattrs, or a filesystem that has none. Neither is a failure.
        Err(Errno::NOTSUP) | Err(Errno::NODATA) => return,
        Err(e) => {
            unpreserved.push(("listxattr".into(), e));
            return;
        }
    };
    let mut value = vec![0u8; 64 * 1024];
    for name in names[..len].split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let name_str = String::from_utf8_lossy(name).to_string();
        let n = match fs::fgetxattr(src.as_fd(), &name_str[..], &mut value[..]) {
            Ok(n) => n,
            Err(e) => {
                unpreserved.push((name_str, e));
                continue;
            }
        };
        if let Err(e) = fs::fsetxattr(dst.as_fd(), &name_str[..], &value[..n], fs::XattrFlags::empty())
        {
            // `security.*` and `trusted.*` need privilege we do not have, and a
            // FAT-like filesystem refuses everything. Both are reported, and
            // neither aborts the copy.
            unpreserved.push((name_str, e));
        }
    }
}

fn same_file(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn fail(errno: Errno, stage: &'static str, bytes: u64, leftover: Option<PathBuf>) -> Failed {
    Failed { errno, stage, bytes_written: bytes, leftover }
}

/// Remove the temporary file, and report it as leftover if even that failed.
fn discard(temp: &Path, errno: Errno, stage: &'static str, bytes: u64) -> Failed {
    let leftover = match unlinkat(rustix::fs::CWD, temp, AtFlags::empty()) {
        Ok(()) => None,
        Err(_) => Some(temp.to_path_buf()),
    };
    Failed { errno, stage, bytes_written: bytes, leftover }
}

fn errno_of(e: &std::io::Error) -> Errno {
    e.raw_os_error().map(Errno::from_raw_os_error).unwrap_or(Errno::IO)
}

/// Move a file. Two entirely different operations behind one word, and the
/// difference is where data loss lives.
///
/// Within a filesystem it is a rename: atomic, instant, and it cannot lose
/// anything. Across filesystems it is a copy followed by an unlink, and the
/// order matters absolutely — **the source is removed only after the destination
/// has been verified byte for byte.** A move that unlinks first, or that trusts
/// the copy's return value, is the classic way a file manager destroys a file.
pub fn move_file(src: &Path, dst: &Path, o: &Options) -> Result<Moved, Failed> {
    let cwd = rustix::fs::CWD;
    let started = Instant::now();

    if let Ok(dst_stat) = statat(cwd, dst, AtFlags::empty()) {
        if let Ok(src_stat) = statat(cwd, src, AtFlags::empty()) {
            if same_file(&src_stat, &dst_stat) {
                return Err(fail(Errno::INVAL, "same-file", 0, None));
            }
        }
        if !o.replace {
            return Err(fail(Errno::EXIST, "destination-exists", 0, None));
        }
    }

    // The fast path. `EXDEV` is the kernel telling us the two paths are on
    // different filesystems, and it is the only reliable way to find that out —
    // comparing `st_dev` misses bind mounts and btrfs subvolumes.
    let flags = if o.replace { RenameFlags::empty() } else { RenameFlags::NOREPLACE };
    match renameat_with(cwd, src, cwd, dst, flags) {
        Ok(()) => {
            return Ok(Moved { renamed: true, verified: false, elapsed: started.elapsed(), bytes: 0 })
        }
        Err(Errno::XDEV) => {}
        Err(Errno::EXIST) => return Err(fail(Errno::EXIST, "destination-exists", 0, None)),
        // A filesystem without the flag says EINVAL; fall through to the copy
        // path rather than guessing, since it is correct either way.
        Err(Errno::INVAL) | Err(Errno::NOSYS) | Err(Errno::OPNOTSUPP) => {}
        Err(e) => return Err(fail(e, "rename", 0, None)),
    }

    let copied = copy_file(src, dst, o)?;

    // Verify before unlinking. Not because the copy is expected to lie, but
    // because this is the one operation where being wrong is unrecoverable.
    match crate::verify::first_difference(src, dst) {
        Ok(None) => {}
        Ok(Some(at)) => {
            let _ = unlinkat(cwd, dst, AtFlags::empty());
            return Err(fail(Errno::IO, "verify", at, None));
        }
        Err(e) => {
            let _ = unlinkat(cwd, dst, AtFlags::empty());
            return Err(fail(errno_of(&e), "verify", 0, None));
        }
    }

    if let Err(e) = unlinkat(cwd, src, AtFlags::empty()) {
        // The copy is good and the original is still there. That is a *duplicate*,
        // not a loss, and it is the right way to fail.
        return Err(Failed {
            errno: e,
            stage: "unlink-source",
            bytes_written: copied.bytes,
            leftover: Some(src.to_path_buf()),
        });
    }
    Ok(Moved {
        renamed: false,
        verified: true,
        elapsed: started.elapsed(),
        bytes: copied.bytes,
    })
}

#[derive(Debug)]
pub struct Moved {
    /// True when the move was a rename, which is the case that cannot lose data.
    pub renamed: bool,
    /// True when the destination was compared against the source before the
    /// source was removed.
    pub verified: bool,
    pub bytes: u64,
    pub elapsed: Duration,
}
