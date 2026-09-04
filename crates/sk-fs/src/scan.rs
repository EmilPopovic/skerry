// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading one directory: names first, metadata second, and the metadata across
//! the whole pool.
//!
//! The split is the measurement, not a style. [Spike
//! B](../../../spikes/b-enumeration/README.md) took a warm million-file walk from
//! 1.195 s to 0.530 s, and **0.614 s of that 0.665 s is deferring `statx`** —
//! `getdents64` itself is worth 0.051 s. So the batching is the decision and the
//! syscall is the trim.
//!
//! The fan-out is the second half of the same finding. Spike B measured a single
//! directory holding a million entries at **3.4× faster** with `statx` spread
//! across the pool, and its p99 dropped from 11.9 s to 1.2 s. This module
//! therefore does the names in one pass and hands the metadata back in slices
//! that the caller can dispatch in parallel — which is why `metadata` takes a
//! range rather than reading the whole directory itself.
//!
//! What the split buys the user is more direct than either number: names arrive
//! in one pass and the list is on screen and scrollable while `statx` is still
//! running. A row with no size yet is a row a user can already select.

use std::path::Path;
use std::sync::Arc;

use rustix::fs::{Dir, FileType, Mode, OFlags, StatxFlags};
use sk_model::{EntryId, Flags, Listing, Problem, ProblemKind};
use sk_util::Cancel;

/// Names per metadata job.
///
/// Small enough that a directory of ten thousand entries still fans out across
/// the pool, large enough that the per-job cost — one `openat` and a channel
/// send — stays under a percent of the work.
pub const META_CHUNK: usize = 2048;

/// What `statx` returned for one entry.
#[derive(Clone, Copy, Debug)]
pub struct MetaRow {
    /// Which entry.
    pub id: EntryId,
    /// Size in bytes.
    pub size: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
    /// Flags only `statx` can know: executable, broken link, the real type.
    pub extra: Flags,
}

/// Every name in a directory, and the listing built from them.
#[derive(Debug)]
pub struct Names {
    /// The entries, with metadata still pending.
    pub listing: Listing,
    /// The same names in entry order, for the metadata pass.
    ///
    /// Shared rather than copied per job: a directory of a million entries is
    /// about 24 MB of names, and handing each of five hundred jobs its own slice
    /// of an `Arc` costs nothing.
    pub names: Arc<Vec<String>>,
    /// Microseconds the name pass took.
    pub elapsed_us: u64,
}

/// Reads every name in `directory`.
///
/// Runs on a job. `cancel` is checked per `getdents64` buffer, never once at the
/// start: a directory with a million entries has to stop when the user navigates
/// away, not when it finishes.
pub fn names(directory: &Path, cancel: &Cancel) -> Result<Names, Problem> {
    let started = std::time::Instant::now();
    let fd = open_dir(directory)?;
    let dir = Dir::read_from(&fd).map_err(|error| problem_for(error, directory))?;

    let mut listing = Listing::with_capacity(256);
    let mut names = Vec::with_capacity(256);
    for entry in dir {
        if cancel.is_cancelled() {
            return Err(Problem::plain(ProblemKind::Gone, "cancelled"));
        }
        let Ok(entry) = entry else { continue };
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        // A filename is bytes, not text. An invalid sequence is shown rather than
        // hidden — a file you cannot see is a file you cannot rename.
        let name = String::from_utf8_lossy(raw).into_owned();
        listing.push(&name, flags_of(entry.file_type()));
        names.push(name);
    }

    Ok(Names { listing, names: Arc::new(names), elapsed_us: started.elapsed().as_micros() as u64 })
}

/// Runs `statx` over `names[range]`, whose first entry is `first`.
///
/// One `openat` for the whole slice, then a name-relative `statx` per entry, so
/// the kernel resolves the directory once instead of once per file.
#[must_use]
pub fn metadata(
    directory: &Path,
    names: &[String],
    first: EntryId,
    cancel: &Cancel,
) -> Vec<MetaRow> {
    let Ok(fd) = open_dir(directory) else { return Vec::new() };
    let mut rows = Vec::with_capacity(names.len());
    for (offset, name) in names.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        rows.push(meta_of(&fd, name, EntryId(first.0 + offset as u32)));
    }
    rows
}

fn open_dir(directory: &Path) -> Result<rustix::fd::OwnedFd, Problem> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    rustix::fs::open(directory, flags, Mode::empty()).map_err(|error| problem_for(error, directory))
}

/// What `getdents64` already knows, for free, from `d_type`.
fn flags_of(kind: FileType) -> Flags {
    match kind {
        FileType::Directory => Flags::DIR,
        FileType::Symlink => Flags::SYMLINK,
        // `d_type` is `DT_UNKNOWN` on some filesystems — XFS without `ftype=1`,
        // and every 9p mount — so it is indistinguishable from a plain file
        // here, and `statx` resolves both in the second pass.
        FileType::RegularFile | FileType::Unknown => Flags::NONE,
        _ => Flags::SPECIAL,
    }
}

fn meta_of(dirfd: impl std::os::fd::AsFd, name: &str, id: EntryId) -> MetaRow {
    let wanted = StatxFlags::TYPE | StatxFlags::MODE | StatxFlags::SIZE | StatxFlags::MTIME;
    // Following the link, because a listing shows what a symlink *is*: a link to
    // a directory opens on a double click and sorts with the directories.
    let followed =
        rustix::fs::statx(dirfd.as_fd(), name, rustix::fs::AtFlags::empty(), wanted).ok();
    let Some(stat) = followed else {
        // A symlink whose target is gone still has a name, a size and an mtime of
        // its own, and a listing that omits it cannot be used to delete it.
        let own =
            rustix::fs::statx(dirfd.as_fd(), name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW, wanted);
        return match own {
            Ok(stat) => MetaRow {
                id,
                size: stat.stx_size,
                mtime: stat.stx_mtime.tv_sec,
                extra: Flags::BROKEN,
            },
            Err(_) => MetaRow { id, size: 0, mtime: 0, extra: Flags::BROKEN },
        };
    };

    let mode = u32::from(stat.stx_mode);
    let mut extra = Flags::NONE;
    if mode & mode_bits::TYPE == mode_bits::DIRECTORY {
        extra = extra.with(Flags::DIR);
    }
    if mode & 0o111 != 0 {
        extra = extra.with(Flags::EXEC);
    }
    MetaRow { id, size: stat.stx_size, mtime: stat.stx_mtime.tv_sec, extra }
}

/// `S_IFMT` and `S_IFDIR` from `sys/stat.h`, which are ABI and not arithmetic we
/// get to choose. Two constants rather than a `libc` dependency here.
mod mode_bits {
    pub const TYPE: u32 = 0o170_000;
    pub const DIRECTORY: u32 = 0o040_000;
}

fn problem_for(error: rustix::io::Errno, path: &Path) -> Problem {
    let subject = path.display().to_string();
    let kind = match error {
        rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR => ProblemKind::Gone,
        _ => ProblemKind::Unreadable,
    };
    Problem::new(kind, subject, format!("open: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{META_CHUNK, metadata, names};
    use sk_model::{EntryId, Flags, ProblemKind};
    use sk_util::Cancel;
    use std::path::{Path, PathBuf};

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-scan-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join(".dotfile"), b"").unwrap();
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("gone"), root.join("broken")).unwrap();
        root
    }

    #[test]
    fn names_arrive_with_no_metadata_at_all() {
        let root = fixture("names");
        let found = names(&root, &Cancel::new()).expect("names");
        assert_eq!(found.listing.len(), 5, "sub, a.txt, .dotfile, link, broken");
        assert_eq!(found.listing.pending(), 5, "nothing has metadata yet");
        assert_eq!(found.names.len(), 5);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlink_to_a_file_gets_the_targets_size_and_a_broken_one_is_flagged() {
        let root = fixture("links");
        let found = names(&root, &Cancel::new()).expect("names");
        let mut listing = found.listing;
        for row in metadata(&root, &found.names, EntryId(0), &Cancel::new()) {
            listing.set_meta(row.id, row.size, row.mtime, row.extra);
        }
        let by_name = |wanted: &str| {
            (0..listing.len())
                .map(|index| EntryId(index as u32))
                .find(|id| listing.name(*id) == wanted)
                .expect(wanted)
        };
        assert_eq!(listing.size(by_name("link")), 5, "the link reports its target's size");
        assert!(listing.flags(by_name("broken")).has(Flags::BROKEN));
        assert!(listing.flags(by_name("sub")).has(Flags::DIR));
        assert!(listing.flags(by_name(".dotfile")).has(Flags::HIDDEN));
        assert_eq!(listing.pending(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The property the fan-out depends on: a slice's rows carry the entry ids
    /// of that slice and no others. Getting this wrong writes one file's size
    /// onto another's row, which no test of the whole directory would notice.
    #[test]
    fn a_slice_of_names_reports_the_ids_of_that_slice() {
        let root = fixture("slice");
        let found = names(&root, &Cancel::new()).expect("names");
        let rows = metadata(&root, &found.names[2..4], EntryId(2), &Cancel::new());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, EntryId(2));
        assert_eq!(rows[1].id, EntryId(3));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_directory_produces_a_message_and_not_a_panic() {
        let problem =
            names(Path::new("/nonexistent-skerry-scan"), &Cancel::new()).expect_err("a problem");
        assert_eq!(problem.kind(), ProblemKind::Gone);
        assert!(problem.message().contains("nonexistent-skerry-scan"));
    }

    #[test]
    fn a_cancelled_read_stops_rather_than_finishing() {
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(names(Path::new("/usr/lib"), &cancel).is_err());
        let found = names(Path::new("/usr/lib"), &Cancel::new()).expect("names");
        assert!(metadata(Path::new("/usr/lib"), &found.names, EntryId(0), &cancel).is_empty());
    }

    #[test]
    fn a_real_directory_reads_and_every_name_is_non_empty() {
        let found = names(Path::new("/usr/lib"), &Cancel::new()).expect("names");
        assert!(found.listing.len() > 10, "{} entries in /usr/lib", found.listing.len());
        for index in 0..found.listing.len() {
            let id = EntryId(index as u32);
            assert!(!found.listing.name(id).is_empty(), "entry {index} has no name");
        }
    }

    /// 10,000 entries is the budget row in docs/09; it has to become more than
    /// one job or the fan-out is decoration.
    #[test]
    fn the_chunk_size_fans_a_real_directory_out_across_a_pool() {
        let jobs = 10_000_usize.div_ceil(META_CHUNK);
        assert!(jobs >= 4, "a 10,000-entry directory becomes {jobs} jobs");
    }
}
