//! How big a directory is, counting everything under it.
//!
//! # What the number means
//!
//! **Apparent size, one filesystem, no hardlink deduplication.** Each of those is
//! a choice and each one is visible to somebody:
//!
//! * **Apparent size** — the sum of `st_size` — is what a person means by "how
//!   big is this folder": it is what copying the tree elsewhere would produce.
//!   `du` reports allocated blocks instead, so the two disagree on a sparse file
//!   by as much as the file is sparse. [Spike
//!   E](../../../spikes/e-operations/README.md) built a 1 GiB file with 12 KiB in
//!   it; this reports 1 GiB for it and `du` reports 12 KiB. Both are true
//!   answers to different questions.
//! * **One filesystem**, like `du -x`. Without it, measuring `/` walks `/proc`
//!   and `/sys`, which are not sizes and are not finite in any useful sense, and
//!   an idle network mount would make the number take its own timeout to appear.
//! * **No hardlink deduplication**, unlike `du`. Deduplicating needs a set of
//!   every inode seen, which for a large tree is tens of megabytes of the kind of
//!   random-access memory traffic [docs/12-measurement.md](../../../docs/12-measurement.md)
//!   says ports worst. A tree of hardlinks therefore reads larger here than it
//!   occupies. This is the one of the three that is a limitation rather than a
//!   choice, and it is the one to revisit first.
//!
//! # Why it reports as it goes
//!
//! Rule 7: honest progress, no indeterminate spinners. A number that climbs is
//! the most honest progress indicator there is — it is the answer, partially
//! known — and it costs one channel message every few frames instead of a
//! spinner that means nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{Dir, FileType, Mode, OFlags, StatxFlags};
use sk_util::Cancel;

/// How long between partial reports.
///
/// About four frames at 60 Hz. Often enough that the number visibly climbs,
/// rarely enough that a directory of a million files does not send a million
/// messages.
const REPORT_EVERY: Duration = Duration::from_millis(64);

/// How much of a directory tree is known so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    /// Bytes counted so far.
    pub bytes: u64,
    /// Files counted so far.
    pub files: u64,
    /// Directories counted so far, including the root.
    pub directories: u64,
    /// Whether the walk finished. `false` means the number is a lower bound.
    pub complete: bool,
    /// Whether anything was skipped: a subdirectory that could not be opened, or
    /// one on another filesystem.
    ///
    /// A number with something missing from it should not look like a complete
    /// one, and a folder containing an unreadable subdirectory is common enough
    /// on a shared machine to be worth saying.
    pub partial: bool,
}

/// Walks `root` and reports its total, as it goes and once at the end.
///
/// Runs on a job. `cancel` is checked per directory, so navigating away from a
/// listing stops every measurement it started rather than finishing them for
/// nobody.
pub fn measure(root: &Path, cancel: &Cancel, report: &mut impl FnMut(Progress)) {
    let Some(device) = device_of(root) else {
        report(Progress { complete: true, partial: true, ..Progress::default() });
        return;
    };

    let mut progress = Progress::default();
    let mut pending = vec![root.to_path_buf()];
    let mut next_report = Instant::now() + REPORT_EVERY;

    // Depth-first with an explicit stack rather than recursion: a directory tree
    // is attacker-controlled depth, and a deep one must not be a stack overflow.
    while let Some(directory) = pending.pop() {
        if cancel.is_cancelled() {
            return;
        }
        progress.directories += 1;
        if !walk_one(&directory, device, &mut progress, &mut pending) {
            progress.partial = true;
        }
        if Instant::now() >= next_report {
            next_report = Instant::now() + REPORT_EVERY;
            report(progress);
        }
    }

    progress.complete = true;
    report(progress);
}

/// Reads one directory, adding its files and queueing its subdirectories.
///
/// Returns whether it could be read at all.
fn walk_one(
    directory: &Path,
    device: u64,
    progress: &mut Progress,
    pending: &mut Vec<PathBuf>,
) -> bool {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let Ok(fd) = rustix::fs::open(directory, flags, Mode::empty()) else { return false };
    let Ok(dir) = Dir::read_from(&fd) else { return false };

    let wanted = StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::INO;
    for entry in dir.flatten() {
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(raw).into_owned();

        // A symlink is never followed and never counted: following one is how a
        // walk finds a cycle, and counting the target is how it counts the same
        // bytes twice.
        if entry.file_type() == FileType::Symlink {
            continue;
        }
        let stat =
            rustix::fs::statx(&fd, name.as_str(), rustix::fs::AtFlags::SYMLINK_NOFOLLOW, wanted);
        let Ok(stat) = stat else {
            progress.partial = true;
            continue;
        };
        let mode = u32::from(stat.stx_mode);
        if mode & 0o170_000 == 0o040_000 {
            // `du -x`: a mount point inside the tree is a different filesystem
            // and a different question.
            if u64::from(stat.stx_dev_major) << 32 | u64::from(stat.stx_dev_minor) == device {
                pending.push(directory.join(&name));
            } else {
                progress.partial = true;
            }
        } else {
            progress.files += 1;
            progress.bytes = progress.bytes.saturating_add(stat.stx_size);
        }
    }
    true
}

/// The device a path is on, as `statx` reports it.
fn device_of(path: &Path) -> Option<u64> {
    let stat =
        rustix::fs::statx(rustix::fs::CWD, path, rustix::fs::AtFlags::empty(), StatxFlags::TYPE)
            .ok()?;
    Some(u64::from(stat.stx_dev_major) << 32 | u64::from(stat.stx_dev_minor))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Progress, measure};
    use sk_util::Cancel;
    use std::path::{Path, PathBuf};

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-size-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("top.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(root.join("a/one.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(root.join("a/b/two.bin"), vec![0u8; 300]).unwrap();
        std::fs::write(root.join("a/b/c/three.bin"), vec![0u8; 400]).unwrap();
        root
    }

    fn total(path: &Path) -> Progress {
        let mut last = Progress::default();
        measure(path, &Cancel::new(), &mut |progress| last = progress);
        last
    }

    #[test]
    fn every_file_at_every_depth_is_counted_once() {
        let root = fixture("depth");
        let found = total(&root);
        assert_eq!(found.bytes, 1000);
        assert_eq!(found.files, 4);
        assert_eq!(found.directories, 4, "root, a, a/b, a/b/c");
        assert!(found.complete);
        assert!(!found.partial);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlink_is_not_followed_and_not_counted() {
        let root = fixture("symlink");
        // A link to a sibling subtree would double the total, and a link to the
        // root would never finish.
        std::os::unix::fs::symlink(root.join("a"), root.join("loop")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("self")).unwrap();
        let found = total(&root);
        assert_eq!(found.bytes, 1000, "the links added bytes");
        assert_eq!(found.directories, 4, "the links were walked");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_number_climbs_and_the_last_report_is_the_complete_one() {
        let root = fixture("climb");
        let mut reports = Vec::new();
        measure(&root, &Cancel::new(), &mut |progress| reports.push(progress));
        assert!(!reports.is_empty());
        let last = reports[reports.len() - 1];
        assert!(last.complete, "the final report is the complete one");
        assert!(
            reports[..reports.len() - 1].iter().all(|progress| !progress.complete),
            "only the last report is complete"
        );
        assert!(
            reports.windows(2).all(|pair| pair[0].bytes <= pair[1].bytes),
            "the total went down: {reports:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cancelled_walk_reports_nothing_further() {
        let root = fixture("cancel");
        let cancel = Cancel::new();
        cancel.cancel();
        let mut reports = Vec::new();
        measure(&root, &cancel, &mut |progress| reports.push(progress));
        assert!(reports.is_empty(), "{reports:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_that_is_not_there_completes_and_says_something_is_missing() {
        let found = total(Path::new("/nonexistent-skerry-size"));
        assert!(found.complete);
        assert!(found.partial);
        assert_eq!(found.bytes, 0);
    }

    /// `/proc` is a different filesystem mounted inside `/`, and walking it is
    /// neither finite nor meaningful. `du -x` skips it and so does this.
    ///
    /// Tested on the device numbers rather than by walking `/`: the walk is the
    /// correct end-to-end check and it takes 23 seconds, which is not a
    /// per-commit test. What the walk would prove is that these two differ.
    #[test]
    fn a_mount_point_inside_a_tree_is_a_different_device() {
        let root = super::device_of(Path::new("/")).expect("/");
        let proc = super::device_of(Path::new("/proc")).expect("/proc");
        assert_ne!(root, proc, "the boundary the walk stops at is invisible");
    }

    #[test]
    fn a_subdirectory_that_cannot_be_read_marks_the_total_partial() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = fixture("unreadable");
        let closed = root.join("a/b");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();

        let found = total(&root);
        // Running as root reads it anyway, and then there is nothing to assert.
        if rustix::process::geteuid().is_root() {
            let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert!(found.partial, "an unreadable subdirectory was not reported");
        assert!(found.complete, "the walk still finished");
        assert_eq!(found.bytes, 300, "top.bin and a/one.bin, and nothing under a/b");

        let _ = std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_directory_is_zero_and_complete() {
        let root = std::env::temp_dir().join(format!("skerry-size-{}-empty", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let found = total(&root);
        assert_eq!((found.bytes, found.files, found.directories), (0, 0, 1));
        assert!(found.complete && !found.partial);
        let _ = std::fs::remove_dir_all(&root);
    }
}
