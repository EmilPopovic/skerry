// SPDX-License-Identifier: Apache-2.0
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
//!   E](../../../tools/torture/README.md) built a 1 GiB file with 12 KiB in
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

use std::collections::HashMap;
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
/// Runs on a job. `cancel` is checked per directory, so a walk the caller has
/// stopped caring about stops rather than finishing for nobody.
pub fn measure(root: &Path, cancel: &Cancel, report: &mut impl FnMut(Progress)) {
    measure_every(root, cancel, REPORT_EVERY, report);
}

/// As [`measure`], with the reporting interval given.
///
/// Exists so the partial-report path can be tested. With the interval fixed at
/// 64 ms, a test fixture small enough to be a test finishes in one report and the
/// assertion about *climbing* is vacuously true — which is a test that can only
/// pass, and [docs/12-measurement.md](../../../docs/12-measurement.md) has a rule
/// about those.
pub fn measure_every(
    root: &Path,
    cancel: &Cancel,
    interval: Duration,
    report: &mut impl FnMut(Progress),
) {
    let Some(device) = device_of(root) else {
        report(Progress { complete: true, partial: true, ..Progress::default() });
        return;
    };

    let mut progress = Progress::default();
    let mut pending = vec![root.to_path_buf()];
    let mut next_report = Instant::now() + interval;

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
            next_report = Instant::now() + interval;
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

    use super::{Progress, measure, measure_every};
    use sk_util::Cancel;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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

    /// The number climbs while the walk is running, and only the last report is
    /// the complete one.
    ///
    /// Driven with a zero interval so a fixture small enough to be a test still
    /// produces partial reports. At the real 64 ms this fixture finishes in one
    /// report, `reports[..0]` is empty, and every assertion about climbing is
    /// vacuously true — which is a test that can only pass, and the whole point
    /// of the mechanism under test is the reports before the last one.
    #[test]
    fn the_number_climbs_and_the_last_report_is_the_complete_one() {
        let root = fixture("climb");
        let mut reports = Vec::new();
        measure_every(&root, &Cancel::new(), Duration::ZERO, &mut |progress| {
            reports.push(progress);
        });
        assert!(reports.len() >= 4, "four directories produced {} reports", reports.len());

        let last = reports[reports.len() - 1];
        assert!(last.complete, "the final report is the complete one");
        assert_eq!(last.bytes, 1000);
        assert!(
            reports[..reports.len() - 1].iter().all(|progress| !progress.complete),
            "only the last report is complete"
        );
        assert!(
            reports.windows(2).all(|pair| pair[0].bytes <= pair[1].bytes),
            "the total went down: {reports:?}"
        );
        assert!(
            reports[..reports.len() - 1].iter().any(|progress| progress.bytes > 0),
            "no partial report ever carried a number"
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

/// Folder totals, remembered by path.
///
/// **Keyed by path and owned above the listing**, which is the whole point. A
/// total belongs to a directory on the disk, not to a row: the first version of
/// this stored it in the `Listing` and keyed the in-flight set by `EntryId`, so
/// every new listing threw all of it away. Going to the parent folder, or any
/// change at all in the current one, re-walked every subtree from nothing — and
/// on a home directory that is minutes of work to rediscover something that had
/// not changed.
///
/// Two consequences worth stating, because both are visible:
///
/// * **Coming back to a folder is instant.** The totals are already known.
/// * **A total can be stale.** inotify on a directory says nothing about its
///   grandchildren, so a subtree that changed underneath keeps its old number
///   until something asks again. `F5` is that something — it forgets and
///   re-measures — and the alternative is a recursive watch on every visible
///   folder, which [Spike C](https://github.com/EmilPopovic/skerry/blob/spikes-final/spikes/c-watching/README.md) priced at 34%
///   of the per-user watch budget for one `node_modules`.
pub struct Sizes {
    known: HashMap<PathBuf, Progress>,
    /// Walks in flight, each with the token that stops it.
    running: HashMap<PathBuf, Cancel>,
    concurrent: usize,
}

impl Sizes {
    /// A cache that runs at most `concurrent` walks at once.
    #[must_use]
    pub fn new(concurrent: usize) -> Self {
        Self { known: HashMap::new(), running: HashMap::new(), concurrent: concurrent.max(1) }
    }

    /// What is known about a folder, complete or not.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<Progress> {
        self.known.get(path).copied()
    }

    /// Claims `path` for a walk, returning the token that stops it.
    ///
    /// The cap is why this can say no to something it has never seen: scrolling
    /// through a directory of three hundred folders must not start three hundred
    /// recursive walks. A refusal is not final — the caller asks again on the
    /// next frame, and the next frame arrives because something else reported.
    pub fn claim(&mut self, path: &Path) -> Option<Cancel> {
        if self.running.contains_key(path) || self.running.len() >= self.concurrent {
            return None;
        }
        if self.known.get(path).is_some_and(|progress| progress.complete) {
            return None;
        }
        let cancel = Cancel::new();
        self.running.insert(path.to_path_buf(), cancel.clone());
        Some(cancel)
    }

    /// Stops every walk that is not a child of `parent`.
    ///
    /// The rule is **what you are looking at wins**. Walks are deliberately not
    /// cancelled when you navigate — one that finishes after you have left still
    /// fills the cache, and coming back is then instant — but they must not go on
    /// holding the slots the new directory needs. Without this, arriving anywhere
    /// while a large tree was still being walked showed a dash against every
    /// folder until that tree finished, which for a source checkout is minutes.
    ///
    /// A sibling scrolled off screen is left alone: it is in the directory being
    /// looked at, and scrolling back should not restart it.
    pub fn retain_under(&mut self, parent: &Path) {
        self.running.retain(|path, cancel| {
            let keep = path.parent() == Some(parent);
            if !keep {
                cancel.cancel();
            }
            keep
        });
    }

    /// Records what a walk found. A complete report releases its slot.
    pub fn record(&mut self, path: &Path, progress: Progress) {
        if progress.complete {
            self.running.remove(path);
        }
        let entry = self.known.entry(path.to_path_buf()).or_insert(progress);
        // A partial report that arrives after the complete one must not undo it,
        // and a total must never go backwards while a person is reading it.
        if progress.complete || progress.bytes >= entry.bytes {
            *entry = progress;
        }
    }

    /// Forgets everything, so the next look measures again.
    pub fn forget_all(&mut self) {
        self.known.clear();
        for (_, cancel) in self.running.drain() {
            cancel.cancel();
        }
    }

    /// Folders known, and walks running.
    #[must_use]
    pub fn stats(&self) -> (usize, usize) {
        (self.known.len(), self.running.len())
    }
}

#[cfg(test)]
mod cache_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Progress, Sizes};
    use std::path::Path;

    fn total(bytes: u64, complete: bool) -> Progress {
        Progress { bytes, complete, ..Progress::default() }
    }

    #[test]
    fn a_total_is_remembered_by_path_and_survives_everything_else() {
        let mut sizes = Sizes::new(4);
        assert!(sizes.claim(Path::new("/a/one")).is_some());
        sizes.record(Path::new("/a/one"), total(100, true));
        assert_eq!(sizes.get(Path::new("/a/one")).map(|it| it.bytes), Some(100));
        assert_eq!(sizes.get(Path::new("/a/two")), None);
    }

    #[test]
    fn a_folder_already_measured_is_not_measured_again() {
        let mut sizes = Sizes::new(4);
        assert!(sizes.claim(Path::new("/a/one")).is_some());
        sizes.record(Path::new("/a/one"), total(100, true));
        assert!(sizes.claim(Path::new("/a/one")).is_none(), "a complete total was re-walked");
    }

    #[test]
    fn an_incomplete_total_keeps_its_slot_and_may_be_resumed_after_it_is_released() {
        let mut sizes = Sizes::new(4);
        assert!(sizes.claim(Path::new("/a/one")).is_some());
        sizes.record(Path::new("/a/one"), total(50, false));
        assert!(sizes.claim(Path::new("/a/one")).is_none(), "already running");
        assert_eq!(sizes.stats(), (1, 1));

        sizes.record(Path::new("/a/one"), total(100, true));
        assert_eq!(sizes.stats(), (1, 0));
    }

    #[test]
    fn the_cap_defers_rather_than_refusing_forever() {
        let mut sizes = Sizes::new(2);
        assert!(sizes.claim(Path::new("/a/one")).is_some());
        assert!(sizes.claim(Path::new("/a/two")).is_some());
        assert!(sizes.claim(Path::new("/a/three")).is_none(), "the cap held");

        sizes.record(Path::new("/a/one"), total(1, true));
        assert!(sizes.claim(Path::new("/a/three")).is_some(), "a finished walk freed a slot");
    }

    /// A number a person is reading must not go backwards.
    #[test]
    fn a_late_partial_report_does_not_undo_a_larger_one() {
        let mut sizes = Sizes::new(4);
        let _ = sizes.claim(Path::new("/a/one"));
        sizes.record(Path::new("/a/one"), total(900, false));
        sizes.record(Path::new("/a/one"), total(100, false));
        assert_eq!(sizes.get(Path::new("/a/one")).map(|it| it.bytes), Some(900));

        // A complete report is authoritative even if it is smaller: the folder
        // may genuinely have shrunk.
        sizes.record(Path::new("/a/one"), total(50, true));
        assert_eq!(sizes.get(Path::new("/a/one")).map(|it| it.bytes), Some(50));
    }

    /// The reported defect. Walks are no longer cancelled when you navigate, so
    /// that a result finishing after you have left still fills the cache — but
    /// they must not go on holding the slots that the folder you are *looking at*
    /// needs. Every folder in the new directory showed a dash until the walks
    /// from the old one finished, which for a large tree is minutes.
    #[test]
    fn walks_left_behind_do_not_hold_the_slots_of_the_folder_being_looked_at() {
        let mut sizes = Sizes::new(2);
        assert!(sizes.claim(Path::new("/old/one")).is_some());
        assert!(sizes.claim(Path::new("/old/two")).is_some());

        // The user navigates. Nothing in the new directory can start.
        assert!(sizes.claim(Path::new("/new/one")).is_none(), "the cap was already full");

        sizes.retain_under(Path::new("/new"));
        assert!(sizes.claim(Path::new("/new/one")).is_some(), "the slots were never released");
        assert!(sizes.claim(Path::new("/new/two")).is_some());
    }

    #[test]
    fn a_folder_scrolled_out_of_view_in_the_same_directory_keeps_walking() {
        let mut sizes = Sizes::new(4);
        assert!(sizes.claim(Path::new("/here/one")).is_some());
        sizes.retain_under(Path::new("/here"));
        assert_eq!(sizes.stats().1, 1, "a sibling was cancelled for scrolling past it");
    }

    #[test]
    fn forgetting_makes_everything_measurable_again() {
        let mut sizes = Sizes::new(4);
        let _ = sizes.claim(Path::new("/a/one"));
        sizes.record(Path::new("/a/one"), total(100, true));
        sizes.forget_all();
        assert_eq!(sizes.get(Path::new("/a/one")), None);
        assert!(sizes.claim(Path::new("/a/one")).is_some());
    }
}
