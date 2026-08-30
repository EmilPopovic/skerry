//! The persistent index, as this window holds it.
//!
//! **The window owns one at a time.** [ADR
//! 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)
//! stores one file per root, and a search is always over one root, so holding a
//! map of them would be fifty megabytes each of speculation. The one that is
//! loaded is the one the last search wanted.
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
    /// A build finished, and this is what it produced.
    Built(Option<Arc<sk_index::Index>>),
    /// A load finished, and this is what was on disk.
    Loaded(Option<Arc<sk_index::Index>>),
}

/// The index this window has, and any build in flight.
#[derive(Default)]
pub(crate) struct Indexes {
    /// The one that is loaded.
    held: Option<Arc<sk_index::Index>>,
    /// The root a build is running for, and how many entries it has reached.
    building: Option<(PathBuf, usize)>,
    /// The token that stops a build in flight.
    cancel: Option<sk_util::Cancel>,
    /// The root a load or a build was asked about.
    ///
    /// A reply names no root, so this is what tells a reply for the folder being
    /// looked at from one for the folder that was being looked at two navigations
    /// ago — which would otherwise replace a good index with an absent one.
    asked: Option<PathBuf>,
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
        self.held.as_ref().filter(|it| it.root() == root)
    }

    /// Whether a load or a build for `root` has already been asked for.
    pub(crate) fn asked_about(&self, root: &Path) -> bool {
        self.asked.as_deref() == Some(root)
    }

    /// Records that one has been.
    pub(crate) fn asking(&mut self, root: &Path) {
        self.asked = Some(root.to_path_buf());
    }

    /// The root a build is running for, and how far it has got.
    pub(crate) fn building(&self) -> Option<(&Path, usize)> {
        self.building.as_ref().map(|(root, count)| (root.as_path(), *count))
    }

    /// Starts a build, replacing any that was running.
    pub(crate) fn begins(&mut self, root: &Path, cancel: sk_util::Cancel) {
        self.stop();
        self.asking(root);
        self.building = Some((root.to_path_buf(), 0));
        self.cancel = Some(cancel);
    }

    /// Stops a build in flight, if there is one.
    pub(crate) fn stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.building = None;
    }

    /// Forgets the loaded index for `root`.
    pub(crate) fn drop_for(&mut self, root: &Path) {
        if self.held.as_ref().is_some_and(|it| it.root() == root) {
            self.held = None;
        }
        if self.asked.as_deref() == Some(root) {
            self.asked = None;
        }
    }

    /// Takes a reply. Whether anything changed on screen.
    pub(crate) fn arrived(&mut self, note: Note) -> bool {
        match note {
            Note::Counting(count) => {
                let Some((_, at)) = self.building.as_mut() else { return false };
                *at = count;
                true
            }
            Note::Built(index) => {
                self.building = None;
                self.cancel = None;
                if index.is_some() {
                    self.held = index;
                }
                true
            }
            // **A load that found nothing must not clear one that is held.** The
            // reply says only what was on disk for the root it was asked about;
            // an index built a moment ago and not yet saved is still the best
            // answer this window has.
            Note::Loaded(index) => {
                if index.is_some() {
                    self.held = index;
                    return true;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
