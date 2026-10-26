// SPDX-License-Identifier: Apache-2.0
//! The error type that reaches a human.
//!
//! Two halves, kept apart on purpose
//! ([docs/04-architecture.md](../../../docs/04-architecture.md#error-policy)):
//! [`Problem::message`] says what happened and what to do, in the user's
//! language; [`Problem::detail`] is the chain that goes to the log. Merging them
//! produces the dialogue every Linux file manager shows — an errno in a sentence
//! nobody can act on.
//!
//! Written by hand rather than with `thiserror`, which would be four crates of
//! derive machinery against a budget of twenty-five for a handful of enums.

use std::fmt;

use crate::i18n;

/// What went wrong, at the granularity a user-facing message needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProblemKind {
    /// No safe, writable trash is available for the source or recovery record.
    TrashUnavailable,
    /// The copy exists, but some metadata or cleanup could not be preserved.
    Preservation,
    /// Permission denied, or a mount that will not answer.
    Unreadable,
    /// The path is not there any more.
    Gone,
    /// No Wayland or X11 display could be reached.
    NoDisplay,
    /// No GPU adapter that can present.
    NoGpu,
    /// No usable font in any of the searched directories.
    NoFont,
    /// A per-user kernel resource is exhausted: inotify instances or watches,
    /// or file descriptors.
    ///
    /// Its own kind because the advice is different from every other kind here
    /// and is the only thing that helps. Spike C measured **99.99% of the watch
    /// budget already consumed** on two of three machines, so this is not a
    /// hypothetical error path — and it used to be reported as `NoDisplay`,
    /// which told the user to set `WAYLAND_DISPLAY`.
    OutOfResources,
    /// A name that cannot be one: empty, or with a separator in it.
    BadName,
    /// Something is already called that.
    ///
    /// Its own kind because it is the one failure a person can fix in the field
    /// they are already typing in, and because every mutation here refuses to
    /// replace rather than replacing — so this is the common answer, not an edge.
    NameTaken,
    /// The source and the destination are on different filesystems.
    ///
    /// A rename cannot cross one and neither can a move to the home trash. The
    /// specification's per-volume `.Trash-$uid` is what fixes it and is owed.
    AcrossFilesystems,
    /// A directory that has to be empty is not.
    NotEmpty,
    /// Another filesystem is mounted inside what was being removed, so that
    /// part was left standing with everything on it.
    ///
    /// Its own kind because the advice — unmount it first — is one no other
    /// kind gives, and because the alternative was deleting the contents of a
    /// bind mount, a FUSE mount or a disk that happened to be mounted below a
    /// folder somebody deleted for good.
    MountedInside,
    /// A move across filesystems left part of its source where it was, because
    /// it changed or appeared after the copy had read what was there.
    ///
    /// Its own kind because the move happened and this is about what it did not
    /// take: the copy holds the source as it was, so removing what came later
    /// would delete the only copy of it. Counted — see [`Problem::counted`].
    LeftBehind,
    /// A move back — the undo of a move between filesystems — found something
    /// already holding a name it was bringing back, and left its own entry where
    /// it was.
    ///
    /// Its own kind because nothing is wrong with either file and there is no
    /// name to choose: the two are versions of one thing, and which to keep is
    /// a person's call. [`Self::NameTaken`]'s advice — choose another name —
    /// was what this said before it existed. Counted.
    NameHeld,
    /// A move back went into a folder that was already there — recreated, or
    /// another of the same name — beside whatever it held, replacing nothing.
    ///
    /// Its own kind because nothing went wrong and nothing was left, but the
    /// folder now holds two things' contents, and that is worth saying.
    MergedInto,
    /// A folder a copy had just made was not the folder it then opened:
    /// somebody replaced it in between.
    ///
    /// Its own kind because [`Self::Gone`] — "no longer there" — was what this
    /// said, about a folder that was there and was the wrong one; the advice is
    /// about who else can write where the copy was going.
    FolderSwapped,
    /// A note a finished step carried, reported as itself.
    ///
    /// What `ops::replay` raises when an undo or redo completed with a note:
    /// the note is already the sentence, and dressing it as
    /// [`Self::Unreadable`] rendered "Cannot read ." with no subject.
    Note,
    /// A mode could not be changed on the descriptor that was checked: the
    /// kernel has no `fchmodat2` and `/proc` is not mounted.
    ///
    /// Its own kind because the file is there and permitted, and the advice is
    /// about the system: [`Self::Gone`], which `ENOENT` from `/proc` used to
    /// raise, sent somebody looking for a file that had not moved.
    ModeUnchangeable,
    /// What an undo or a redo would put back is not what it would be replacing:
    /// somebody changed the file's bytes, or its mode, after this program did.
    ///
    /// **Its own kind because it is the one refusal whose cause is a person.**
    /// It used to arrive as [`Self::NameTaken`], so an edit undone after another
    /// program touched the file said something was already called that — a
    /// sentence about a different problem, with advice that could not help.
    ChangedSince,
    /// More bytes than the operation is willing to hold in memory.
    TooLarge,
    /// Not a file, a folder or a link, so there is nothing to copy.
    ///
    /// Its own kind because a device node and a named pipe are not *broken* and
    /// not *unreadable* — recreating one needs `mknod` and, for a device,
    /// privilege this process does not have. Reported rather than skipped: a copy
    /// that leaves something out and returns success is worse than one that says
    /// what it will not do.
    NotCopyable,
    /// The destination has no room, or the quota is spent.
    ///
    /// Its own kind because the advice is "free some space or choose somewhere
    /// else" and no other kind here says that — and because it is the failure a
    /// copy engine has to handle cleanly rather than the one it hopes not to see:
    /// Spike E measured `ENOSPC` at an exact byte with nothing left behind.
    Full,
    /// The destination filesystem will not store a name with those bytes in it.
    ///
    /// Its own kind because the advice is "rename it for that disk", which no
    /// other kind here says: `Unreadable` sends the person to check permissions
    /// they already have, and [`Self::BadName`] tells them a name cannot contain
    /// a slash when it does not. `EILSEQ` from a copy onto vfat or NTFS is what
    /// raises it, which is what copying to a USB stick is — found by pointing
    /// the Spike E matrix at `sk-fs::copy` instead of at its own fork.
    NameRefused,
    /// The program that reads this archive's compressor is not installed.
    ///
    /// Its own kind because the advice is a package name, which no other kind
    /// here can carry: `Unreadable` sends somebody to check permissions that are
    /// fine, and the subject is the *program* rather than the file — the one
    /// message in this list where that is true.
    NoUnpacker,
    /// The archive is a format this program does not read.
    ///
    /// Its own kind because the advice is the opposite of [`Self::NoUnpacker`]'s:
    /// there is nothing to install that would help, and the answer is another
    /// program. Conflating the two told somebody with 7-Zip installed that 7z
    /// was not installed.
    NotUnpackable,
    /// An archive is not what it says it is, or not all of it arrived.
    ///
    /// Its own kind because it is the only failure whose message has to say what
    /// happened to the output: partial files remain available and undoable.
    Damaged,
    /// An extraction retained output but could not create every entry.
    Incomplete,
    /// A container variant cannot be extracted safely by this reader.
    UnsupportedArchive,
    /// A helper started answering and went silent, and was stopped for it.
    ///
    /// Its own kind because the only other sentence a helper that stopped could
    /// get is [`Self::NoUnpacker`]'s, which tells somebody to install a program
    /// that was installed and running.
    Stalled,
    /// The bytes are not text, so they cannot be treated as text.
    ///
    /// Its own kind because the answer is not "this failed" but "this is not the
    /// thing you asked about": a JPEG has no text to copy, and saying so names
    /// the file rather than blaming the operation.
    NotText,
    /// An undo entry's way back was erased by Empty Trash.
    ///
    /// Its own kind because it is the only failure here that a person *caused on
    /// purpose*, and [`Self::Gone`]'s advice — "it may have been moved or
    /// deleted" — is a guess where this one knows. Without it, `Ctrl+Z` after an
    /// Empty Trash named a path inside `Trash/files` that nobody had ever looked
    /// at, which is [rule 3](../../../docs/03-principles.md#3-everything-is-reversible)'s
    /// failure test arriving as a sentence about the wrong thing.
    TrashEmptied,
    /// The helper that reads the file would not run, because this kernel cannot
    /// confine it and the setting that allows it anyway is off.
    ///
    /// Its own kind because the advice is a setting, which no other kind here
    /// gives: [`Self::NoUnpacker`] would send somebody to install a program they
    /// have, and the file is not damaged.
    Unconfined,
}

impl ProblemKind {
    const fn catalogue_key(self) -> &'static str {
        match self {
            Self::TrashUnavailable => "error.trash_unavailable",
            Self::Preservation => "error.preservation",
            Self::Unreadable => "error.unreadable",
            Self::Gone => "error.gone",
            Self::NoDisplay => "error.no_display",
            Self::NoGpu => "error.no_gpu",
            Self::NoFont => "error.no_font",
            Self::OutOfResources => "error.out_of_resources",
            Self::BadName => "error.bad_name",
            Self::NameTaken => "error.name_taken",
            Self::AcrossFilesystems => "error.across_filesystems",
            Self::NotEmpty => "error.not_empty",
            Self::MountedInside => "error.mounted_inside",
            Self::LeftBehind => "error.left_behind",
            Self::NameHeld => "error.name_held",
            Self::ModeUnchangeable => "error.mode_unchangeable",
            Self::MergedInto => "error.merged_into",
            Self::FolderSwapped => "error.folder_swapped",
            Self::Note => "error.note",
            Self::ChangedSince => "error.changed_since",
            Self::TooLarge => "error.too_large",
            Self::Full => "error.full",
            Self::NotCopyable => "error.not_copyable",
            Self::NameRefused => "error.name_refused",
            Self::NoUnpacker => "error.no_unpacker",
            Self::Damaged => "error.damaged",
            Self::Incomplete => "error.incomplete",
            Self::UnsupportedArchive => "error.unsupported_archive",
            Self::Stalled => "error.stalled",
            Self::NotUnpackable => "error.not_unpackable",
            Self::NotText => "error.not_text",
            Self::TrashEmptied => "error.trash_emptied",
            Self::Unconfined => "error.unconfined",
        }
    }
}

/// Something that went wrong, with both halves of its explanation.
#[derive(Clone, Debug)]
pub struct Problem {
    kind: ProblemKind,
    subject: String,
    detail: String,
    /// How many things this is about, for a kind whose message has plural
    /// forms; `subject` is then the first of them.
    count: Option<u64>,
}

impl Problem {
    /// A problem about `subject`, with `detail` for the log.
    pub fn new(kind: ProblemKind, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { kind, subject: subject.into(), detail: detail.into(), count: None }
    }

    /// A problem about `count` things, `subject` the first of them.
    ///
    /// **The message agrees with the count** through the catalogue's plural
    /// forms ([ADR 0016](../../../docs/adr/0016-plural-forms-are-bracketed-keys-in-the-catalogue.md)),
    /// as `{0}` the count and `{1}` the subject. A sentence that named one of
    /// three hundred things left behind read as though one thing had been.
    pub fn counted(
        kind: ProblemKind,
        subject: impl Into<String>,
        count: u64,
        detail: impl Into<String>,
    ) -> Self {
        Self { count: Some(count), ..Self::new(kind, subject, detail) }
    }

    /// A problem with no particular subject.
    pub fn plain(kind: ProblemKind, detail: impl Into<String>) -> Self {
        Self::new(kind, String::new(), detail)
    }

    /// What went wrong.
    #[must_use]
    pub const fn kind(&self) -> ProblemKind {
        self.kind
    }

    /// What happened and what to do, resolved through the catalogue.
    #[must_use]
    pub fn message(&self) -> String {
        match self.count {
            Some(count) => i18n::plural(self.kind.catalogue_key(), count, &[&count, &self.subject]),
            None => i18n::format(self.kind.catalogue_key(), &[&self.subject]),
        }
    }

    /// The chain for the log. Never shown as the primary message.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for Problem {
    /// The log form: the debug chain, with the user-facing message left to
    /// [`Problem::message`] so a `{}` in a log line cannot leak into a dialogue.
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.subject.is_empty() {
            write!(out, "{:?}: {}", self.kind, self.detail)
        } else {
            write!(out, "{:?} at {}: {}", self.kind, self.subject, self.detail)
        }
    }
}

impl std::error::Error for Problem {}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Problem, ProblemKind};

    #[test]
    fn the_message_names_the_subject_and_says_what_to_do() {
        let problem = Problem::new(ProblemKind::Unreadable, "/proc/1/mem", "EACCES");
        let message = problem.message();
        assert!(message.contains("/proc/1/mem"), "{message}");
        assert!(message.contains("permission"), "{message}");
        assert!(!message.contains("EACCES"), "the errno belongs in the log: {message}");
    }

    #[test]
    fn the_log_form_carries_the_chain_and_the_message_does_not() {
        let problem = Problem::new(ProblemKind::Gone, "/tmp/x", "ENOENT from statx");
        assert!(problem.to_string().contains("ENOENT from statx"));
        assert!(!problem.message().contains("ENOENT"));
    }

    #[test]
    fn every_kind_resolves_to_real_text() {
        for kind in [
            ProblemKind::Unreadable,
            ProblemKind::Gone,
            ProblemKind::NoDisplay,
            ProblemKind::NoGpu,
            ProblemKind::NoFont,
            ProblemKind::Stalled,
        ] {
            let message = Problem::plain(kind, "detail").message();
            assert!(!message.starts_with("error."), "`{kind:?}` has no catalogue entry");
        }
    }

    /// A counted problem agrees with its count, and names the first subject
    /// either way — the plural line has no bare form to fall back on.
    #[test]
    fn a_counted_problem_agrees_with_its_count() {
        for kind in [ProblemKind::LeftBehind, ProblemKind::NameHeld] {
            let one = Problem::counted(kind, "/a/report.pdf", 1, "detail").message();
            let many = Problem::counted(kind, "/a/report.pdf", 300, "detail").message();
            assert!(!one.starts_with("error.") && !many.starts_with("error."), "{kind:?}");
            assert!(one.contains("/a/report.pdf") && many.contains("/a/report.pdf"), "{many}");
            assert!(many.contains("300 items") && !one.contains("items"), "{one} / {many}");
        }
        let mode = Problem::new(ProblemKind::ModeUnchangeable, "/a/f", "detail").message();
        assert!(mode.contains("/proc") && mode.contains("/a/f"), "{mode}");
    }
}
