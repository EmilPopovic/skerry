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
}

impl ProblemKind {
    const fn catalogue_key(self) -> &'static str {
        match self {
            Self::Unreadable => "error.unreadable",
            Self::Gone => "error.gone",
            Self::NoDisplay => "error.no_display",
            Self::NoGpu => "error.no_gpu",
            Self::NoFont => "error.no_font",
            Self::OutOfResources => "error.out_of_resources",
        }
    }
}

/// Something that went wrong, with both halves of its explanation.
#[derive(Clone, Debug)]
pub struct Problem {
    kind: ProblemKind,
    subject: String,
    detail: String,
}

impl Problem {
    /// A problem about `subject`, with `detail` for the log.
    pub fn new(kind: ProblemKind, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { kind, subject: subject.into(), detail: detail.into() }
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
        i18n::format(self.kind.catalogue_key(), &[&self.subject])
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
        ] {
            let message = Problem::plain(kind, "detail").message();
            assert!(!message.starts_with("error."), "`{kind:?}` has no catalogue entry");
        }
    }
}
