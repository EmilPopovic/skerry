// SPDX-License-Identifier: Apache-2.0
//! Everything under a directory whose name matches, arriving as it is found.
//!
//! # What this is and what it is not
//!
//! **A walk, not the index.** [ADR
//! 0008](../../../docs/adr/0008-persistent-index-is-required.md) makes a
//! persistent filename index mandatory, and [Spike
//! F](../../../spikes/f-index-and-search/README.md) measured one: 195 ms to load
//! it cold on a Pi against 2.3–3.7 s to walk the same tree. That index is what
//! makes a *complete* answer over the whole filesystem fast — 27.45 ms against
//! 8,559 ms over 6.9 million entries, which is
//! [ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)'s
//! measurement. This is the other half, and it is the half that answers *first*
//! in either scope: the walk streams, so a result is on screen in about a
//! millisecond whether an index exists or not.
//!
//! **One walk, several roots.** ADR 0022 turns "one filesystem" into "the
//! filesystems you asked for": [`across`] takes a set of roots, admits the
//! device of each, and seeds itself with the roots no other root contains — so
//! `/boot` under `/` is reached by descending rather than walked twice, and
//! `/proc` is excluded by not being a root rather than by a list of names to
//! keep up with. [`find`] is `across` over one.
//!
//! # Why it reports as it goes
//!
//! [Rule 7](../../../docs/03-principles.md): the answer arrives in pieces
//! because it *is* in pieces, and a list that fills while you read it is the
//! most honest progress there is. M4's bar is first results within 50 ms of the
//! first keystroke, which a walk that reported only at the end could not meet on
//! any tree worth searching.
//!
//! # The matching rule is not this module's
//!
//! [`sk_model::contains_folded`], which is what the find popup narrows one
//! directory with. [Rule 8](../../../docs/03-principles.md) is that the filter
//! grammar is the same in every view, and two copies of a substring rule is two
//! places for "why did that not match" to have different answers.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{Dir, FileType, Mode, OFlags, StatxFlags};
use sk_util::Cancel;

/// How long between reports.
///
/// The same 64 ms `size` uses, and for the same reason: often enough that the
/// list visibly fills, rarely enough that a tree of a million files does not
/// send a million messages.
const REPORT_EVERY: Duration = Duration::from_millis(64);

/// How many matches are kept before the walk stops collecting them.
///
/// **A cap, and it is reported rather than hidden.** A one-character query over
/// a home directory matches hundreds of thousands of files; keeping them all is
/// a list nobody scrolls and a `Vec` of `PathBuf`s the size of the tree. Two
/// thousand is more than anybody reads and small enough to be nothing.
pub const MOST: usize = 2000;

/// One file the search found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hit {
    /// Where it is.
    pub path: PathBuf,
    /// Bytes, as the file claims. Zero for a directory.
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: i64,
    /// Whether it is a directory.
    pub directory: bool,
}

/// What the walk has found so far.
#[derive(Clone, Debug, Default)]
pub struct Found {
    /// The matches, in the order they were found — which is the order the walk
    /// reached them, and is depth-first rather than alphabetical.
    pub hits: Vec<Hit>,
    /// How many entries have been looked at, matching or not, so the line can
    /// say what the walk has covered rather than only what it has found.
    pub scanned: u64,
    /// Whether the walk reached the end of the tree.
    pub complete: bool,
    /// Whether something was skipped: a directory that could not be opened, or
    /// one on another filesystem.
    pub partial: bool,
    /// Whether [`MOST`] was reached, so the list is a prefix of the answer.
    pub capped: bool,
}

/// Walks `root` and reports everything under it whose name contains `needle`.
///
/// Runs on a job. `cancel` is checked per directory, so a search the caller has
/// stopped caring about stops rather than finishing for nobody.
///
/// **One filesystem, and symlinks are not followed** — the two rules
/// [`crate::size`] follows, for the two reasons it gives: a walk that crosses a
/// mount takes an idle network mount's timeout, and one that follows links finds
/// a cycle.
pub fn find(root: &Path, needle: &str, cancel: &Cancel, report: &mut impl FnMut(&Found)) {
    find_every(root, needle, cancel, REPORT_EVERY, report);
}

/// Walks every root and reports everything under them whose name contains
/// `needle`, adding to what `found` already holds.
///
/// **The scope's roots, and the devices they name.** A directory inside the tree
/// is descended into when its device belongs to one of the roots — so a scope of
/// `/` and `/boot` crosses that mount and a scope of `/` alone does not, which is
/// the same `du -x` rule [`crate::size`] follows, generalised from one device to
/// the set somebody asked about.
///
/// `found` arrives holding whatever the index scans already answered, which is
/// what makes the two halves of one search one list: the cap, the count of
/// entries looked at and the hits are all continued rather than restarted.
pub fn across(
    roots: &[PathBuf],
    needle: &str,
    cancel: &Cancel,
    found: &mut Found,
    report: &mut impl FnMut(&Found),
) {
    across_every(roots, needle, cancel, REPORT_EVERY, found, report);
}

/// As [`find`], with the reporting interval given.
///
/// Exists so the partial-report path can be tested: at 64 ms a fixture small
/// enough to be a test finishes in one report, and every assertion about the
/// list *filling* would be vacuously true.
pub fn find_every(
    root: &Path,
    needle: &str,
    cancel: &Cancel,
    interval: Duration,
    report: &mut impl FnMut(&Found),
) {
    let mut found = Found::default();
    across_every(&[root.to_path_buf()], needle, cancel, interval, &mut found, report);
}

/// As [`across`], with the reporting interval given.
///
/// Exists so the partial-report path can be tested: at 64 ms a fixture small
/// enough to be a test finishes in one report, and every assertion about the
/// list *filling* would be vacuously true.
pub fn across_every(
    roots: &[PathBuf],
    needle: &str,
    cancel: &Cancel,
    interval: Duration,
    found: &mut Found,
    report: &mut impl FnMut(&Found),
) {
    if needle.is_empty() {
        // Not "everything". An empty query is a question that has not been asked
        // yet, and answering it with the whole tree is how a search box becomes a
        // way to accidentally list a million files.
        found.complete = true;
        report(found);
        return;
    }
    let mut devices: Vec<u64> = Vec::with_capacity(roots.len());
    for root in roots {
        match device_of(root) {
            Some(device) if !devices.contains(&device) => devices.push(device),
            Some(_) => {}
            // A root that cannot be stated is a disk that went away between the
            // mount table being read and the question being asked. The rest of
            // the scope is still answerable, and the flag says something was
            // skipped.
            None => found.partial = true,
        }
    }
    if devices.is_empty() {
        found.complete = true;
        found.partial = true;
        report(found);
        return;
    }

    let mut pending: Vec<PathBuf> = seeds(roots);
    let mut next_report = Instant::now() + interval;
    // **The first match goes out at once, whatever the interval says.** M4's bar
    // is first results within 50 ms of the first keystroke, and a walk that only
    // reported every 64 ms could not meet it however fast it was — the number
    // would be the throttle rather than the work. After that the interval takes
    // over, because the second result is not the one anybody is waiting for.
    let mut said_anything = !found.hits.is_empty();
    // Depth-first with an explicit stack rather than recursion: a directory tree
    // is attacker-controlled depth, and a deep one must not be a stack overflow.
    while let Some(directory) = pending.pop() {
        if cancel.is_cancelled() {
            return;
        }
        if !look_in(&directory, needle, &devices, found, &mut pending) {
            found.partial = true;
        }
        let first = !said_anything && !found.hits.is_empty();
        if first || Instant::now() >= next_report {
            said_anything = true;
            next_report = Instant::now() + interval;
            report(found);
        }
    }

    found.complete = true;
    report(found);
}

/// The roots to start walking from: the ones no other root contains.
///
/// A mount point is a path inside its parent filesystem, so a scope of `/` and
/// `/boot` would otherwise walk `/boot` twice — once by descending into it, once
/// as a root of its own. Dropping the contained ones costs nothing, because the
/// device set is what admits them either way.
fn seeds(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|root| !roots.iter().any(|other| other != *root && root.starts_with(other)))
        .cloned()
        .collect()
}

/// Reads one directory, keeping what matches and queueing its subdirectories.
///
/// Returns whether it could be read at all.
fn look_in(
    directory: &Path,
    needle: &str,
    devices: &[u64],
    found: &mut Found,
    pending: &mut Vec<PathBuf>,
) -> bool {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let Ok(fd) = rustix::fs::open(directory, flags, Mode::empty()) else { return false };
    let Ok(dir) = Dir::read_from(&fd) else { return false };

    let wanted = StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::MTIME;
    for entry in dir.flatten() {
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(raw).into_owned();
        found.scanned += 1;
        // A symlink is listed if its own name matches and is never walked into:
        // following one is how a walk finds a cycle, and its target is somewhere
        // else in the tree or outside it altogether.
        if entry.file_type() == FileType::Symlink {
            keep(directory, &name, needle, None, found);
            continue;
        }
        let stat =
            rustix::fs::statx(&fd, name.as_str(), rustix::fs::AtFlags::SYMLINK_NOFOLLOW, wanted);
        let Ok(stat) = stat else {
            found.partial = true;
            continue;
        };
        let directory_here = u32::from(stat.stx_mode) & 0o170_000 == 0o040_000;
        keep(directory, &name, needle, Some(&stat), found);
        if directory_here {
            // `du -x`, over the set: a mount point inside the tree is a
            // different filesystem, and whether it is a different *question*
            // depends on whether the scope named it.
            let device = u64::from(stat.stx_dev_major) << 32 | u64::from(stat.stx_dev_minor);
            if devices.contains(&device) {
                pending.push(directory.join(&name));
            } else {
                found.partial = true;
            }
        }
    }
    true
}

fn keep(
    directory: &Path,
    name: &str,
    needle: &str,
    stat: Option<&rustix::fs::Statx>,
    found: &mut Found,
) {
    if !sk_model::contains_folded(name, needle) {
        return;
    }
    if found.hits.len() >= MOST {
        found.capped = true;
        return;
    }
    let (size, modified, directory_here) = stat.map_or((0, 0, false), |stat| {
        (stat.stx_size, stat.stx_mtime.tv_sec, u32::from(stat.stx_mode) & 0o170_000 == 0o040_000)
    });
    found.hits.push(Hit { path: directory.join(name), size, modified, directory: directory_here });
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

    use super::{Found, find, find_every};
    use sk_util::Cancel;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-find-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("reports")).unwrap();
        std::fs::write(root.join("top.txt"), b"x").unwrap();
        std::fs::write(root.join("a/Report.pdf"), vec![0u8; 40]).unwrap();
        std::fs::write(root.join("a/b/notes.md"), b"y").unwrap();
        std::fs::write(root.join("a/b/c/report-2024.pdf"), vec![0u8; 80]).unwrap();
        root
    }

    fn all(root: &Path, needle: &str) -> Found {
        let mut last = Found::default();
        find(root, needle, &Cancel::new(), &mut |found| last = found.clone());
        last
    }

    fn names(found: &Found) -> Vec<String> {
        let mut out: Vec<String> = found
            .hits
            .iter()
            .map(|hit| hit.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// Every depth, and the folder whose own name matches too.
    #[test]
    fn a_match_at_any_depth_is_found_and_so_is_a_folder() {
        let root = fixture("depth");
        let found = all(&root, "report");
        assert_eq!(names(&found), ["Report.pdf", "report-2024.pdf", "reports"]);
        assert!(found.complete);
        assert!(!found.partial);
        assert!(!found.capped);
        // The size and the kind come with it, so a result row needs no second
        // `statx` on the frame that draws it.
        let folder = found.hits.iter().find(|hit| hit.directory).expect("no folder matched");
        assert_eq!(folder.path, root.join("reports"));
        let file = found.hits.iter().find(|hit| hit.size == 80).expect("no 80-byte file");
        assert!(file.path.ends_with("a/b/c/report-2024.pdf"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same folding the find popup uses, which is rule 8.
    #[test]
    fn the_match_ignores_ascii_case_the_way_the_one_directory_filter_does() {
        let root = fixture("folding");
        assert_eq!(names(&all(&root, "REPORT")), ["Report.pdf", "report-2024.pdf", "reports"]);
        assert_eq!(names(&all(&root, "rEpOrT.")), ["Report.pdf"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty query is a question nobody asked, not a request for everything.
    #[test]
    fn an_empty_query_finds_nothing_rather_than_the_whole_tree() {
        let root = fixture("empty");
        let found = all(&root, "");
        assert!(found.hits.is_empty());
        assert!(found.complete);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link is listed by its own name and never walked into.
    #[test]
    fn a_symlink_is_a_result_and_not_a_way_into_a_cycle() {
        let root = fixture("symlink");
        std::os::unix::fs::symlink(&root, root.join("report-loop")).unwrap();
        let found = all(&root, "report");
        assert!(names(&found).contains(&"report-loop".to_owned()));
        // Three real matches and the link. Following it would have found them
        // again, and again, until the depth ran out.
        assert_eq!(found.hits.len(), 4, "{:?}", names(&found));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The list fills while the walk runs, and only the last report is complete.
    ///
    /// Driven with a zero interval, for the reason `size`'s equivalent is: at the
    /// real 64 ms a fixture small enough to be a test finishes in one report and
    /// every assertion about filling is vacuously true.
    #[test]
    fn results_arrive_before_the_walk_has_finished() {
        let root = fixture("streaming");
        let mut reports: Vec<Found> = Vec::new();
        find_every(&root, "report", &Cancel::new(), Duration::ZERO, &mut |found| {
            reports.push(found.clone());
        });
        assert!(reports.len() > 1, "only {} report(s), so nothing streamed", reports.len());
        assert!(reports.iter().rev().skip(1).all(|it| !it.complete), "an early report said done");
        assert!(reports.last().expect("no reports").complete);
        // Monotone: a result never disappears from a later report.
        for pair in reports.windows(2) {
            assert!(pair[1].hits.len() >= pair[0].hits.len(), "the list went backwards");
            assert!(pair[1].scanned >= pair[0].scanned);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two roots on one filesystem are one list, and nothing is walked twice.
    ///
    /// The contained root is the case that matters: `alpha` is inside the
    /// fixture, so a scope naming both would reach it as a root *and* by
    /// descending — and every entry under it would appear twice. What stops that
    /// is the seed set rather than a visited list, which is
    /// [ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)'s
    /// own argument.
    #[test]
    fn a_scope_of_several_roots_is_one_list_and_walks_nothing_twice() {
        let root = fixture("scope");
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/report-here.txt"), b"z").unwrap();

        let mut both = Found::default();
        let roots = vec![root.clone(), root.join("a")];
        super::across(&roots, "report", &Cancel::new(), &mut both, &mut |_| {});
        let mut whole = Found::default();
        super::across(
            std::slice::from_ref(&root),
            "report",
            &Cancel::new(),
            &mut whole,
            &mut |_| {},
        );
        assert_eq!(names(&both), names(&whole), "a contained root changed the answer");
        assert_eq!(both.scanned, whole.scanned, "something was walked twice");
        assert!(both.complete);

        // And a list that arrives holding something is continued rather than
        // restarted, which is how an index scan and a walk are one search.
        let mut carried = Found { scanned: 40, ..Found::default() };
        carried.hits.push(super::Hit {
            path: root.join("from-an-index.txt"),
            size: 0,
            modified: 0,
            directory: false,
        });
        super::across(
            std::slice::from_ref(&root),
            "report",
            &Cancel::new(),
            &mut carried,
            &mut |_| {},
        );
        assert_eq!(carried.scanned, whole.scanned + 40);
        assert_eq!(carried.hits.len(), whole.hits.len() + 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A root that is not there is a disk that went away, not a failed search.
    #[test]
    fn a_root_that_cannot_be_stated_leaves_the_rest_of_the_scope_answerable() {
        let root = fixture("missing-root");
        let mut found = Found::default();
        let roots = vec![root.clone(), root.join("no-such-mount")];
        super::across(&roots, "report", &Cancel::new(), &mut found, &mut |_| {});
        assert_eq!(names(&found), ["Report.pdf", "report-2024.pdf", "reports"]);
        assert!(found.partial, "something was skipped and the flag did not say so");
        assert!(found.complete);

        // Every root gone is an answer of nothing, said completely.
        let mut none = Found::default();
        super::across(&[root.join("gone")], "report", &Cancel::new(), &mut none, &mut |_| {});
        assert!(none.hits.is_empty());
        assert!(none.complete && none.partial);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cancelled walk stops rather than finishing for nobody.
    #[test]
    fn a_cancelled_walk_reports_nothing_further() {
        let root = fixture("cancel");
        let cancel = Cancel::new();
        cancel.cancel();
        let mut reports = 0;
        find(&root, "report", &cancel, &mut |_| reports += 1);
        assert_eq!(reports, 0, "a cancelled walk reported anyway");
        let _ = std::fs::remove_dir_all(&root);
    }
}
