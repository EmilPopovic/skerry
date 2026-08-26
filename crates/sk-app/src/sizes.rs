//! Scheduling folder totals, and putting them on the rows.
//!
//! The cache is `sk_index::Sizes` and is keyed by path; this is the part that
//! decides *when* to walk, which the crate table gives to this level. Two rules,
//! and the first one is the defect this module exists to fix:
//!
//! * **A total belongs to a path, not to a row.** The first version kept it in
//!   the `Listing` and tracked in-flight walks by `EntryId`, so every new listing
//!   — a navigation, or any change at all in the watched directory — threw all of
//!   it away and re-walked from nothing. Going to the parent folder meant
//!   watching a home directory rediscover, over several seconds, something that
//!   had not changed.
//! * **Only what is on screen is measured, and only a few at a time.** A
//!   directory of three hundred folders must not start three hundred recursive
//!   walks. The cap defers rather than refuses: the next frame asks again, and
//!   the next frame arrives because a running walk reported.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use sk_index::{Progress, Sizes};
use sk_model::Flags;
use sk_util::{Cancel, Lane, WakeHandle};

use crate::pane::{Jobs, Pane};

/// Recursive walks running at once.
///
/// Eight against a pool of `ncpu`: enough that a screenful of folders resolves in
/// a few waves, few enough that enumeration and `statx` for the directory a
/// person is actually looking at are never queued behind them.
const CONCURRENT: usize = 8;

/// The folder totals, and the jobs that produce them.
pub(crate) struct Measurer {
    sizes: Sizes,
    sender: Sender<(PathBuf, Progress)>,
    reports: Receiver<(PathBuf, Progress)>,
    wake: WakeHandle,
}

impl Measurer {
    pub fn new(wake: WakeHandle) -> Self {
        let (sender, reports) = mpsc::channel();
        Self { sizes: Sizes::new(CONCURRENT), sender, reports, wake }
    }

    /// Puts known totals on the visible rows, and starts walks for the rest.
    ///
    /// Called once per frame. Applying every frame rather than on arrival is what
    /// makes a new listing show what is already known immediately: the cache is
    /// the truth and the listing is a view of it.
    pub fn update(&mut self, pane: &mut Pane, rows: Range<usize>, jobs: &Jobs) {
        // What you are looking at wins. Walks started in a directory you have
        // left keep running — their results still fill the cache — but they give
        // up their slots, or arriving anywhere while a source checkout was being
        // walked would show a dash against every folder until it finished.
        self.sizes.retain_under(&pane.path);

        for row in rows {
            let Some(id) = pane.listing.at(row) else { continue };
            if !pane.listing.flags(id).has(Flags::DIR) {
                continue;
            }
            let path = pane.path.join(pane.listing.name(id));
            if let Some(progress) = self.sizes.get(&path) {
                pane.listing.set_recursive_size(
                    id,
                    progress.bytes,
                    progress.complete,
                    progress.partial,
                );
            }
            if let Some(cancel) = self.sizes.claim(&path) {
                self.spawn(path, cancel, pane.lane(), jobs);
            }
        }
    }

    fn spawn(&self, path: PathBuf, cancel: Cancel, lane: Lane, jobs: &Jobs) {
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        // The pane's lane, not always the parallel one: on a network mount or a
        // rotational disk, eight concurrent recursive walks is the worst thing
        // this program could ask a device to do.
        jobs.pool.spawn(lane, cancel, move |cancel| {
            sk_index::measure(&path, cancel, &mut |progress| {
                if sender.send((path.clone(), progress)).is_err() {
                    cancel.cancel();
                    return;
                }
                wake.wake();
            });
        });
    }

    /// Takes delivery of every report waiting. Returns whether anything arrived.
    pub fn drain(&mut self) -> bool {
        let mut any = false;
        while let Ok((path, progress)) = self.reports.try_recv() {
            self.sizes.record(&path, progress);
            any = true;
        }
        any
    }

    /// Forgets every total, so the next frame measures again.
    ///
    /// What `F5` does. A watch on a directory says nothing about its
    /// grandchildren, so a subtree that changed underneath keeps its old number
    /// until somebody asks — and this is the asking.
    pub fn forget_all(&mut self) {
        self.sizes.forget_all();
    }

    /// Folders known, and walks running.
    pub fn stats(&self) -> (usize, usize) {
        self.sizes.stats()
    }

    /// Stops every walk. Called before the pool is dropped.
    ///
    /// Dropping the pool joins its workers, and a worker three minutes into a
    /// directory tree would hold the process open for all of it.
    pub fn stop(&mut self) {
        self.sizes.forget_all();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Measurer;
    use crate::pane::{Jobs, Message, Pane};
    use sk_util::{Pool, Waker};
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Harness {
        jobs: Jobs,
        updates: mpsc::Receiver<Message>,
        _waker: Waker,
    }

    impl Harness {
        fn new() -> (Self, Measurer) {
            let waker = Waker::new().expect("eventfd");
            let (sender, updates) = mpsc::channel();
            let measurer = Measurer::new(waker.handle());
            let jobs = Jobs { pool: Pool::with_width(4), wake: waker.handle(), sender };
            (Self { jobs, updates, _waker: waker }, measurer)
        }

        fn settle(&self, pane: &mut Pane) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.updates.recv_timeout(Duration::from_millis(20)) {
                    Ok(message) => {
                        pane.apply(message, &self.jobs);
                    }
                    Err(_) if !pane.loading => break,
                    Err(_) => {}
                }
            }
            while let Ok(message) = self.updates.try_recv() {
                pane.apply(message, &self.jobs);
            }
        }
    }

    /// Runs `update` and `drain` until every visible folder has a complete total.
    fn measure_all(measurer: &mut Measurer, pane: &mut Pane, jobs: &Jobs) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            measurer.update(pane, 0..pane.listing.rows(), jobs);
            if (0..pane.listing.rows())
                .filter_map(|row| pane.listing.at(row))
                .filter(|id| pane.listing.flags(*id).has(sk_model::Flags::DIR))
                .all(|id| pane.listing.recursive_size(id).is_some_and(|(_, done)| done))
            {
                return;
            }
            measurer.drain();
            std::thread::yield_now();
        }
    }

    fn total_of(pane: &Pane, name: &str) -> Option<(u64, bool)> {
        (0..pane.listing.rows())
            .filter_map(|row| pane.listing.at(row))
            .find(|id| pane.listing.name(*id) == name)
            .and_then(|id| pane.listing.recursive_size(id))
    }

    fn fixture(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-sizes-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("sub/a.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(root.join("sub/deeper/b.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(root.join("other/c.bin"), vec![0u8; 400]).unwrap();
        root
    }

    #[test]
    fn a_visible_folder_gets_a_total() {
        let root = fixture("basic");
        let (harness, mut measurer) = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);

        measure_all(&mut measurer, &mut pane, &harness.jobs);
        assert_eq!(total_of(&pane, "sub"), Some((300, true)));
        assert_eq!(total_of(&pane, "other"), Some((400, true)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reported defect: a change in the directory threw every total away.
    #[test]
    fn a_refresh_keeps_the_totals_it_already_had() {
        let root = fixture("refresh");
        let (harness, mut measurer) = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        measure_all(&mut measurer, &mut pane, &harness.jobs);
        assert_eq!(total_of(&pane, "sub"), Some((300, true)));

        // Something appears in the directory, which is what the watcher reports.
        std::fs::write(root.join("new-arrival.txt"), b"x").unwrap();
        pane.refresh(&harness.jobs);
        harness.settle(&mut pane);
        let rows = 0..pane.listing.rows();
        measurer.update(&mut pane, rows, &harness.jobs);

        assert_eq!(
            total_of(&pane, "sub"),
            Some((300, true)),
            "a refresh threw the folder total away"
        );
        let (known, running) = measurer.stats();
        assert_eq!((known, running), (2, 0), "and re-walked it");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the same defect: leaving and coming back.
    #[test]
    fn coming_back_to_a_folder_is_instant() {
        let root = fixture("return");
        let (harness, mut measurer) = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        measure_all(&mut measurer, &mut pane, &harness.jobs);
        let (known_before, _) = measurer.stats();

        pane.open(root.join("sub"), &harness.jobs);
        harness.settle(&mut pane);
        pane.go_up(&harness.jobs);
        harness.settle(&mut pane);

        // One pass, no waiting: the totals are already known.
        let rows = 0..pane.listing.rows();
        measurer.update(&mut pane, rows, &harness.jobs);
        assert_eq!(total_of(&pane, "sub"), Some((300, true)));
        assert_eq!(total_of(&pane, "other"), Some((400, true)));
        let (known_after, _) = measurer.stats();
        assert!(known_after >= known_before, "{known_after} < {known_before}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn forgetting_measures_again() {
        let root = fixture("forget");
        let (harness, mut measurer) = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        measure_all(&mut measurer, &mut pane, &harness.jobs);
        assert!(measurer.stats().0 > 0);

        measurer.forget_all();
        assert_eq!(measurer.stats(), (0, 0));
        measure_all(&mut measurer, &mut pane, &harness.jobs);
        assert_eq!(total_of(&pane, "sub"), Some((300, true)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reported defect, at the level a person meets it: arriving in a
    /// directory while the previous one's walks are still running must not leave
    /// every folder showing a dash.
    #[test]
    fn arriving_somewhere_new_measures_it_even_if_the_old_walks_are_running() {
        let old = fixture("old");
        let new = fixture("new");
        let (harness, mut measurer) = Harness::new();

        // Fill every slot with walks belonging to the old directory.
        for index in 0..16 {
            std::fs::create_dir_all(old.join(format!("filler-{index}"))).unwrap();
        }
        let mut pane = Pane::new(old.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        let rows = 0..pane.listing.rows();
        measurer.update(&mut pane, rows, &harness.jobs);

        // Navigate before they finish.
        pane.open(new.clone(), &harness.jobs);
        harness.settle(&mut pane);
        measure_all(&mut measurer, &mut pane, &harness.jobs);

        assert_eq!(total_of(&pane, "sub"), Some((300, true)));
        assert_eq!(total_of(&pane, "other"), Some((400, true)));
        let _ = std::fs::remove_dir_all(&old);
        let _ = std::fs::remove_dir_all(&new);
    }

    #[test]
    fn a_file_is_never_measured() {
        let root = fixture("files");
        std::fs::write(root.join("loose.bin"), vec![0u8; 7]).unwrap();
        let (harness, mut measurer) = Harness::new();
        let mut pane = Pane::new(root.clone());
        pane.reload(&harness.jobs);
        harness.settle(&mut pane);
        measure_all(&mut measurer, &mut pane, &harness.jobs);

        assert_eq!(total_of(&pane, "loose.bin"), None, "a file was walked as a folder");
        assert_eq!(measurer.stats().0, 2, "only the two folders");
        let _ = std::fs::remove_dir_all(&root);
    }
}
