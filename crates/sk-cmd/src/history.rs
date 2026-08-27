//! One undo stack, and the redo branch behind it.
//!
//! [Rule 3](../../../docs/03-principles.md) is that every mutation is undoable
//! and that there is **one** stack for all of them — not one per kind, not one
//! per pane. So this is generic over what a change is, for the same reason
//! [`crate::Registry`] is generic over what a command runs against: the stack is
//! bookkeeping, and the crate that knows how to invert a change is a different
//! one.
//!
//! The invert-and-record is a single call taking a closure, because the two-step
//! shape — pop, invert, push to the redo side — has a way to be half-done, and a
//! half-done undo stack tells a person their file came back when it did not.

/// A bounded stack of changes, with the ones that were undone kept behind it.
pub struct History<T> {
    done: Vec<T>,
    undone: Vec<T>,
    cap: usize,
}

impl<T> History<T> {
    /// A stack holding at most `cap` changes.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self { done: Vec::new(), undone: Vec::new(), cap: cap.max(1) }
    }

    /// Records something that just happened.
    ///
    /// **Clears the redo branch**, which is what every editor does and what a
    /// person expects: once you do something new, the future you undid is gone.
    pub fn record(&mut self, entry: T) {
        self.undone.clear();
        if self.done.len() >= self.cap {
            self.done.remove(0);
        }
        self.done.push(entry);
    }

    /// Undoes the most recent change, if `revert` succeeds.
    ///
    /// The entry moves to the redo branch **only** when `revert` returns true.
    /// A refused undo — a directory somebody has since put files in — leaves the
    /// stack exactly as it was, so pressing the key again tries the same thing
    /// rather than skipping past it.
    #[must_use = "a refused undo is a thing to tell somebody about"]
    pub fn undo(&mut self, revert: impl FnOnce(&T) -> bool) -> bool {
        let Some(entry) = self.done.pop() else { return false };
        if revert(&entry) {
            self.undone.push(entry);
            true
        } else {
            self.done.push(entry);
            false
        }
    }

    /// Does the most recently undone change again, if `apply` succeeds.
    ///
    /// `apply` returns a **fresh** entry rather than a bare success, because
    /// doing a change again does not always reproduce the same change: trashing
    /// the same file twice claims two different slots in the trash, and the
    /// entry that can undo the second one is not the entry that undid the first.
    #[must_use = "a refused redo is a thing to tell somebody about"]
    pub fn redo(&mut self, apply: impl FnOnce(&T) -> Option<T>) -> bool {
        let Some(entry) = self.undone.pop() else { return false };
        let Some(fresh) = apply(&entry) else {
            self.undone.push(entry);
            return false;
        };
        if self.done.len() >= self.cap {
            self.done.remove(0);
        }
        self.done.push(fresh);
        true
    }

    /// The change an undo would revert, without touching the stack.
    ///
    /// For a caller whose revert is a **job**: it reads the entry here, hands a
    /// copy to the pool, and calls [`History::undo`] when the answer comes back.
    /// What makes that safe rather than the half-done shape this API exists to
    /// avoid is that the caller does not start a second operation while one is in
    /// flight — so the entry `undo` pops is the entry `peek_undo` returned.
    #[must_use]
    pub fn peek_undo(&self) -> Option<&T> {
        self.done.last()
    }

    /// The change a redo would apply again, without touching the stack.
    #[must_use]
    pub fn peek_redo(&self) -> Option<&T> {
        self.undone.last()
    }

    /// Whether there is anything to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    /// Whether there is anything to redo.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Everything that has been done, oldest first.
    ///
    /// For the visible history rule 3 asks for. Not yet drawn anywhere, which is
    /// why it is the one method here with no caller — and why deleting it would
    /// be deleting the reason the stack keeps its entries at all.
    #[must_use]
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &T> + '_ {
        self.done.iter()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::History;

    #[test]
    fn undo_and_redo_walk_the_same_ground() {
        let mut history = History::new(8);
        assert!(!history.can_undo() && !history.can_redo());
        history.record("one");
        history.record("two");

        let mut seen = Vec::new();
        assert!(history.undo(|entry| {
            seen.push(*entry);
            true
        }));
        assert_eq!(seen, vec!["two"]);
        assert!(history.can_redo());
        assert!(history.redo(|entry| {
            seen.push(*entry);
            Some(*entry)
        }));
        assert_eq!(seen, vec!["two", "two"]);
        assert!(!history.can_redo() && history.can_undo());
    }

    /// A refused undo must leave the stack alone. Pressing the key again should
    /// try the same thing, not skip past it to the change before.
    #[test]
    fn a_refused_undo_stays_at_the_top_of_the_stack() {
        let mut history = History::new(8);
        history.record("first");
        history.record("second");

        assert!(!history.undo(|_| false));
        assert!(history.can_undo() && !history.can_redo());

        let mut seen = Vec::new();
        assert!(history.undo(|entry| {
            seen.push(*entry);
            true
        }));
        assert_eq!(seen, vec!["second"], "the refusal let it skip an entry");
    }

    /// Doing something again does not always reproduce the same change: trashing
    /// one file twice claims two slots, and the entry that undoes the second is
    /// not the one that undid the first.
    #[test]
    fn a_redo_records_what_actually_happened_the_second_time() {
        let mut history = History::new(8);
        history.record("trashed as notes.txt");
        assert!(history.undo(|_| true));
        assert!(history.redo(|_| Some("trashed as notes.txt.2")));

        let entries: Vec<_> = history.entries().copied().collect();
        assert_eq!(entries, vec!["trashed as notes.txt.2"]);
    }

    #[test]
    fn doing_something_new_throws_away_the_future() {
        let mut history = History::new(8);
        history.record("one");
        assert!(history.undo(|_| true));
        assert!(history.can_redo());

        history.record("something else");
        assert!(!history.can_redo(), "the undone change came back from the dead");
    }

    /// A session that runs for days must not grow a stack for days.
    #[test]
    fn the_oldest_change_falls_off_the_bottom() {
        let mut history = History::new(3);
        for index in 0..10 {
            history.record(index);
        }
        let entries: Vec<_> = history.entries().copied().collect();
        assert_eq!(entries, vec![7, 8, 9]);
        // And a redo cannot push it past the cap either.
        assert!(history.undo(|_| true));
        assert!(history.redo(|entry| Some(*entry)));
        assert_eq!(history.entries().count(), 3);
    }

    /// What a caller whose revert is a job does: read, hand a copy to the pool,
    /// and settle the stack when the answer arrives.
    #[test]
    fn peeking_does_not_move_anything() {
        let mut history = History::new(4);
        history.record("one");
        history.record("two");
        assert_eq!(history.peek_undo(), Some(&"two"));
        assert_eq!(history.peek_undo(), Some(&"two"), "peeking twice moved it");
        assert_eq!(history.peek_redo(), None);

        assert!(history.undo(|entry| *entry == "two"));
        assert_eq!(history.peek_undo(), Some(&"one"));
        assert_eq!(history.peek_redo(), Some(&"two"));
    }

    #[test]
    fn an_empty_stack_refuses_both_ways_without_calling_anything() {
        let mut history: History<u8> = History::new(4);
        assert!(!history.undo(|_| panic!("nothing to undo")));
        assert!(!history.redo(|_| panic!("nothing to redo")));
    }
}
