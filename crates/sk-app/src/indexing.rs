//! The persistent index, as this window holds it.
//!
//! **The window owns one per root of the search in flight.** [ADR
//! 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)
//! stores one file per root; a search over one folder wants one of them and a
//! search over the whole machine wants one per disk
//! ([ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)).
//! Nothing else is held: fifty megabytes of index for a folder nobody is
//! searching is fifty megabytes of speculation, and the bytes go when the panel
//! does.
//!
//! **What survives the bytes is the knowledge that the file exists.** A path and
//! a build time each, which is what *Forget the index* is enabled by and what the
//! status line reports — 24 bytes against the 326 MB an index of this machine's
//! root filesystem occupies.
//!
//! Nothing here reads a file. Loading and building are jobs, because
//! [rule 1](../../../docs/03-principles.md) is that the UI thread issues no
//! filesystem syscalls and a fifty-megabyte read is the clearest possible
//! violation of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a job working on an index has to say.
pub(crate) enum Note {
    /// Entries walked so far, while a build runs.
    Counting(usize),
    /// How many entries there turn out to be, and whether that is the final word.
    ///
    /// From the counting pass beside the build, which is where the progress bar's
    /// denominator comes from.
    Total(usize, bool),
    /// A build finished, and this is what it produced.
    Built(Option<Arc<sk_index::Index>>),
    /// A load finished, and this is what was on disk.
    Loaded(Option<Arc<sk_index::Index>>),
}

/// The indexes this window has, and any build in flight.
#[derive(Default)]
pub(crate) struct Indexes {
    /// The ones that are loaded, one per root at most.
    held: Vec<Arc<sk_index::Index>>,
    /// Every root this window has seen an index for, and when it was built.
    ///
    /// Kept when [`Indexes::drop_the_bytes`] throws the indexes away, because
    /// "there is an index for this folder" is a different question from "it is
    /// loaded" — and the row that deletes one has to be enabled by the first.
    known: Vec<(PathBuf, i64)>,
    /// The root a build is running for, and how many entries it has reached.
    building: Option<(PathBuf, usize)>,
    /// How many entries the counting pass has found, and whether it finished.
    ///
    /// **Nothing is estimated from a partial count**, which is the rule the copy
    /// engine's `Total` states and the reason it carries the same flag: a bar that
    /// climbs to 40% and starts over because the walk found more is worse than no
    /// bar at all. So the fraction waits for the `bool`.
    total: (usize, bool),
    /// The token that stops a build in flight.
    cancel: Option<sk_util::Cancel>,
    /// The roots still owed a build, oldest first.
    ///
    /// **What "index every filesystem" is**: one build is one walk of one
    /// filesystem, so indexing several is the same build repeated — and the
    /// strip's line says which of how many while it works through them.
    owed: Vec<PathBuf>,
    /// The roots a load or a build has been asked about.
    ///
    /// A reply names no root, so this is what stops a second load being asked
    /// for on every keystroke — the loaded index is what every search of that
    /// folder then scans, and re-reading it per character would be the walk this
    /// feature exists to replace, with extra steps.
    asked: Vec<PathBuf>,
}

impl Indexes {
    /// The index covering `root`, if this window has one.
    ///
    /// Exactly `root` and not an ancestor of it. An index of a parent does cover
    /// this tree, and using it would mean filtering every hit by prefix and
    /// explaining why a search of one folder reports entries from another — so
    /// the containment check ADR 0012 declined at the storage layer is declined
    /// here too.
    pub(crate) fn covering(&self, root: &Path) -> Option<&Arc<sk_index::Index>> {
        self.held.iter().find(|it| it.root() == root)
    }

    /// Whether this window knows of an index for `root`, loaded or not.
    pub(crate) fn knows_of(&self, root: &Path) -> bool {
        self.known.iter().any(|(path, _)| path == root)
    }

    /// Whether a load or a build for `root` has already been asked for.
    pub(crate) fn asked_about(&self, root: &Path) -> bool {
        self.asked.iter().any(|path| path == root)
    }

    /// Records that one has been.
    pub(crate) fn asking(&mut self, root: &Path) {
        if !self.asked_about(root) {
            self.asked.push(root.to_path_buf());
        }
    }

    /// Throws away the loaded indexes, keeping what is known about them.
    ///
    /// What the panel closing does. The `asked` list goes with the bytes: the
    /// next search has to read the file again, which is 413.6 ms for a whole
    /// filesystem on a job and is the price of the peak not being a floor.
    pub(crate) fn drop_the_bytes(&mut self) {
        self.held.clear();
        self.asked.clear();
    }

    /// The root a build is running for, and how far it has got.
    pub(crate) fn building(&self) -> Option<(&Path, usize)> {
        self.building.as_ref().map(|(root, count)| (root.as_path(), *count))
    }

    /// The roots still owed a build, after the one running.
    pub(crate) fn owed(&self) -> &[std::path::PathBuf] {
        &self.owed
    }

    /// Puts `roots` in the queue, in order, behind whatever is running.
    ///
    /// **A queue rather than a batch.** One build is one walk of one filesystem
    /// and the strip reports one — so indexing every root is the same build run
    /// as many times as there are roots, and the line says which of how many.
    /// The alternative is a walk of several devices at once, which is [ADR
    /// 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)'s
    /// question for a *search* and the wrong shape for a build: the file is one
    /// per root, so the work is too.
    pub(crate) fn owe(&mut self, roots: Vec<std::path::PathBuf>) {
        self.owed = roots;
    }

    /// The next root owed, if any, taken out of the queue.
    pub(crate) fn next_owed(&mut self) -> Option<std::path::PathBuf> {
        if self.owed.is_empty() {
            return None;
        }
        Some(self.owed.remove(0))
    }

    /// Starts a build, replacing any that was running.
    pub(crate) fn begins(&mut self, root: &Path, cancel: sk_util::Cancel) {
        // **The queue survives this**, because `begins` is how each root in it
        // starts: a stop is what empties it, and a stop is somebody saying so.
        let owed = std::mem::take(&mut self.owed);
        self.stop();
        self.owed = owed;
        self.asking(root);
        self.building = Some((root.to_path_buf(), 0));
        self.total = (0, false);
        self.cancel = Some(cancel);
    }

    /// How far the build has got, once there is a total to measure it against.
    ///
    /// `None` until the counting pass has finished, which is the same bargain
    /// the copy makes: no bar at all rather than one whose denominator is still
    /// moving. Clamped, because the tree can grow between the count and the
    /// build reaching the end of it.
    pub(crate) fn fraction(&self) -> Option<f32> {
        let (_, done) = self.building.as_ref()?;
        let (total, complete) = self.total;
        (complete && total > 0).then(|| (*done as f32 / total as f32).clamp(0.0, 1.0))
    }

    /// The total the counting pass found, once it is final.
    pub(crate) const fn counted(&self) -> Option<usize> {
        match self.total {
            (total, true) => Some(total),
            _ => None,
        }
    }

    /// Stops a build in flight, if there is one, and forgets what was owed.
    ///
    /// **Both, because a stop is one gesture.** A queue that outlived the stop
    /// would start the next root a moment after somebody asked for nothing to be
    /// running — which is the shape of a progress bar that will not go away.
    pub(crate) fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.building = None;
        self.owed.clear();
    }

    /// Forgets the index for `root` — the bytes, and that there was one.
    pub(crate) fn drop_for(&mut self, root: &Path) {
        self.held.retain(|it| it.root() != root);
        self.known.retain(|(path, _)| path != root);
        self.asked.retain(|path| path != root);
    }

    /// Takes delivery of one index, replacing any held for the same root.
    fn take(&mut self, index: Arc<sk_index::Index>) {
        let root = index.root().to_path_buf();
        let built = index.built();
        self.held.retain(|it| it.root() != root);
        self.known.retain(|(path, _)| *path != root);
        self.known.push((root, built));
        self.held.push(index);
    }

    /// Takes a reply. Whether anything changed on screen.
    pub(crate) fn arrived(&mut self, note: Note) -> bool {
        match note {
            Note::Counting(count) => {
                let Some((_, at)) = self.building.as_mut() else { return false };
                *at = count;
                true
            }
            Note::Total(total, complete) => {
                self.total = (total, complete);
                self.building.is_some()
            }
            Note::Built(index) => {
                self.building = None;
                self.cancel = None;
                self.total = (0, false);
                if let Some(index) = index {
                    self.take(index);
                }
                true
            }
            // **A load that found nothing must not clear one that is held.** The
            // reply says only what was on disk for the root it was asked about;
            // an index built a moment ago and not yet saved is still the best
            // answer this window has.
            Note::Loaded(index) => {
                if let Some(index) = index {
                    self.take(index);
                    return true;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Indexes, Note};
    use std::path::Path;

    /// A reply for a folder nobody is looking at any more must not land.
    ///
    /// The same rule the search's own replies follow, for the same reason: a
    /// build takes seconds, and a person navigates in the middle of one.
    #[test]
    fn an_index_is_only_used_for_the_root_it_was_built_for() {
        let mut indexes = Indexes::default();
        assert!(indexes.covering(Path::new("/tmp")).is_none());
        indexes.asking(Path::new("/tmp"));
        assert!(indexes.asked_about(Path::new("/tmp")));
        assert!(!indexes.asked_about(Path::new("/var")));
    }

    /// A load that came back empty leaves what is held alone.
    #[test]
    fn an_empty_load_does_not_throw_away_the_index_in_hand() {
        let mut indexes = Indexes::default();
        assert!(!indexes.arrived(Note::Loaded(None)), "an empty load claimed to change something");
        assert!(indexes.covering(Path::new("/tmp")).is_none());
    }

    /// No bar until the count has finished, and then a real one.
    ///
    /// **The rule the copy engine states and this one inherits**: nothing is
    /// estimated from a partial total, because a bar that climbs to 40% and
    /// starts over when the walk finds more is worse than no bar at all.
    #[test]
    fn the_bar_waits_for_a_total_that_has_stopped_moving() {
        let mut indexes = Indexes::default();
        assert!(indexes.fraction().is_none(), "a bar with no build behind it");
        indexes.begins(Path::new("/tmp"), sk_util::Cancel::new());
        assert!(indexes.fraction().is_none(), "a bar before anything was counted");

        indexes.arrived(Note::Counting(50));
        indexes.arrived(Note::Total(200, false));
        assert!(indexes.fraction().is_none(), "a bar drawn from a count still moving");
        assert!(indexes.counted().is_none());

        indexes.arrived(Note::Total(400, true));
        assert_eq!(indexes.counted(), Some(400));
        assert_eq!(indexes.fraction(), Some(0.125));

        // The tree can grow between the count and the build reaching the end of
        // it, and a bar past its own end is a bar that has stopped meaning
        // anything.
        indexes.arrived(Note::Counting(900));
        assert_eq!(indexes.fraction(), Some(1.0));

        // And it goes away with the build rather than lingering over the strip.
        indexes.arrived(Note::Built(None));
        assert!(indexes.fraction().is_none());
    }

    /// Three roots, one at a time, and a stop that means all of them.
    #[test]
    fn a_queue_of_roots_is_worked_through_and_a_stop_empties_it() {
        let mut indexes = Indexes::default();
        let roots: Vec<std::path::PathBuf> =
            ["/", "/home", "/mnt/backup"].iter().map(std::path::PathBuf::from).collect();
        let mut queue = roots.clone();
        let first = queue.remove(0);
        indexes.owe(queue);
        indexes.begins(&first, sk_util::Cancel::new());
        // **The queue survives the start**, which is the thing `begins` had to be
        // taught: it stops whatever was running, and a stop empties the queue.
        assert_eq!(indexes.owed().len(), 2);
        assert_eq!(indexes.building().map(|(root, _)| root), Some(roots[0].as_path()));

        let next = indexes.next_owed().expect("a second root");
        assert_eq!(next, roots[1]);
        indexes.begins(&next, sk_util::Cancel::new());
        assert_eq!(indexes.owed().len(), 1);

        // A stop is one gesture: nothing running and nothing owed, or the next
        // root starts a moment after somebody asked for nothing to be.
        indexes.stop();
        assert!(indexes.building().is_none());
        assert!(indexes.owed().is_empty());
        assert!(indexes.next_owed().is_none());
    }

    /// A build that is cancelled stops being a build in flight.
    #[test]
    fn stopping_a_build_clears_the_line_that_reports_it() {
        let mut indexes = Indexes::default();
        let cancel = sk_util::Cancel::new();
        indexes.begins(Path::new("/tmp"), cancel.clone());
        assert_eq!(indexes.building().map(|(root, _)| root), Some(Path::new("/tmp")));
        assert!(indexes.arrived(Note::Counting(40)));
        assert_eq!(indexes.building().map(|(_, count)| count), Some(40));
        indexes.stop();
        assert!(indexes.building().is_none());
        assert!(cancel.is_cancelled(), "stopping did not cancel the job");
    }
}
