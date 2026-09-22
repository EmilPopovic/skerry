// SPDX-License-Identifier: Apache-2.0
//! Where this window's marks come from, and the theme behind them.
//!
//! **[ADR 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).**
//! The `icons` setting says `marks`, `desktop` or a theme's name; this is what
//! that resolves to, and the join between the three level-2 crates that cannot
//! see each other. `sk-desktop` parses `index.theme` and answers with a path,
//! `sk-preview` hosts the child that turns a path into pixels, and neither may
//! depend on the other — so the caller is this crate, which is the same shape
//! [kind detection](crate::previews) already has.
//!
//! # Asked for no theme, and no theme found
//!
//! [`Source::Ours`] and [`Source::Failed`] are different states reached from
//! different code, which is the thing ADR 0042 asks for by name: under `Ours`
//! nothing parses an `index.theme`, nothing is held, and [`sk_desktop::icons`] is
//! never called at all. Collapsing them into one absent theme would leave a
//! machine with a broken `index.theme` indistinguishable from one whose owner
//! chose the marks, and rule 7's honest sentence could not be written.
//!
//! [`Source::Loading`] is the fourth and it is not one of the three: it is the
//! window between the job starting and its answer, and nothing is resolved yet.

use sk_desktop::icons::{Refused, Theme};

use crate::settings::Icons;

/// Where the marks in this window come from, now.
#[derive(Debug)]
pub(crate) enum Source {
    /// The setting says `marks`. Nothing is parsed, nothing is held, and the
    /// lookup is never called.
    Ours,
    /// A theme is being loaded on a job and has not come back.
    Loading,
    /// A theme is loaded. A name it does not hold is a *miss* — the mark is drawn
    /// for that one file and the next name may still hit.
    Ready(Theme),
    /// A theme was asked for and could not be had.
    Failed {
        /// What was asked for, which the sentence names.
        theme: String,
        /// Which of the two ways it failed.
        why: Refused,
    },
}

impl Source {
    /// One sentence saying where a mark comes from, for the settings panel.
    ///
    /// **Every state says something different**, which is the assertion ADR 0042
    /// asks for: a panel that said the same thing under `marks` and under a theme
    /// that failed to load would be a panel that cannot explain either.
    pub(crate) fn says(&self) -> String {
        use sk_model::i18n;
        match self {
            Self::Ours => i18n::lookup("icons.ours").to_owned(),
            Self::Loading => i18n::lookup("icons.loading").to_owned(),
            Self::Ready(theme) => i18n::format("icons.ready", &[&theme.name()]),
            Self::Failed { theme, why: Refused::Unknown } => {
                i18n::format("icons.unknown", &[theme])
            }
            Self::Failed { theme, why: Refused::Empty } => i18n::format("icons.empty", &[theme]),
        }
    }

    /// What the diagnostic line calls this, which a scenario asserts against.
    pub(crate) fn token(&self) -> String {
        match self {
            Self::Ours => "ours".to_owned(),
            Self::Loading => "loading".to_owned(),
            Self::Ready(theme) => format!("ready:{}:{}", theme.name(), theme.holds()),
            Self::Failed { theme, why } => {
                let why = match why {
                    Refused::Unknown => "unknown",
                    Refused::Empty => "empty",
                };
                format!("failed:{theme}:{why}")
            }
        }
    }
}

/// What a load job has to say: what was asked for, and what came back.
pub(crate) struct Loaded {
    /// The theme's name, which the failure sentence needs and the theme itself
    /// cannot carry when there is no theme.
    pub(crate) theme: String,
    /// The theme, or why there is none.
    pub(crate) outcome: Result<Theme, Refused>,
}

/// Loads whatever the setting asks for. **Runs on the job pool.**
///
/// `from_the_portal` is what the desktop answered over D-Bus, asked once at
/// start-up by [`crate::service`] on the connection that already exists — the
/// call is level 4's because the bus is, and this is the file half of the same
/// question.
pub(crate) fn load(icons: &Icons, from_the_portal: Option<String>) -> Option<Loaded> {
    let theme = match icons {
        Icons::Marks => return None,
        Icons::Named(name) => name.clone(),
        // The desktop's own answer, then the files it writes, then the theme the
        // specification requires every machine to have.
        Icons::Desktop => from_the_portal
            .or_else(sk_desktop::icons::from_the_desktop_files)
            .unwrap_or_else(|| sk_desktop::icons::FALLBACK.to_owned()),
    };
    let outcome = Theme::load(&theme);
    Some(Loaded { theme, outcome })
}

impl Loaded {
    /// The state this answer puts the window in.
    pub(crate) fn settle(self) -> Source {
        match self.outcome {
            Ok(theme) => Source::Ready(theme),
            Err(why) => Source::Failed { theme: self.theme, why },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Loaded, Source};
    use crate::settings::Icons;
    use sk_desktop::icons::Refused;

    /// **The assertion ADR 0042 asks for by name.** A person who chose the marks
    /// and a machine whose `index.theme` could not be read are two different
    /// states, and a panel that says the same thing about both cannot explain
    /// either of them.
    #[test]
    fn every_source_says_something_different() {
        let said = [
            Source::Ours,
            Source::Loading,
            Source::Failed { theme: "Adwaita".to_owned(), why: Refused::Unknown },
            Source::Failed { theme: "Adwaita".to_owned(), why: Refused::Empty },
        ]
        .map(|source| source.says());
        for (at, one) in said.iter().enumerate() {
            assert!(!one.is_empty(), "{at} said nothing");
            assert!(!one.contains("icons."), "{one} is a key rather than a sentence");
            for other in said.iter().skip(at + 1) {
                assert_ne!(one, other, "two sources say the same thing");
            }
        }
        // The two failures both name the theme, which is what makes the sentence
        // actionable rather than a shrug.
        assert!(said[2].contains("Adwaita") && said[3].contains("Adwaita"), "{said:?}");
    }

    /// **`marks` asks for no load at all.** Not a load that is skipped later — the
    /// job is never started, so nothing parses an `index.theme` and nothing walks
    /// an icon directory.
    #[test]
    fn under_marks_nothing_is_loaded() {
        assert!(super::load(&Icons::Marks, None).is_none());
        assert!(super::load(&Icons::Marks, Some("Adwaita".to_owned())).is_none());
    }

    /// A theme nothing declares fails under its own name, so the panel can say
    /// which one — and the name the *portal* gave is the one that appears, not
    /// the word `desktop`.
    #[test]
    fn a_theme_that_cannot_be_had_fails_under_the_name_that_was_asked_for() {
        let asked = "no-theme-is-called-this-anywhere";
        let loaded = super::load(&Icons::Named(asked.to_owned()), None).unwrap();
        assert_eq!(loaded.theme, asked);
        assert_eq!(loaded.outcome.as_ref().unwrap_err(), &Refused::Unknown);
        let source = loaded.settle();
        assert!(source.says().contains(asked), "{}", source.says());
        assert!(matches!(source, Source::Failed { .. }), "a failed load is not a loaded theme");
        assert_eq!(source.token(), format!("failed:{asked}:unknown"));

        let from_portal = super::load(&Icons::Desktop, Some(asked.to_owned())).unwrap();
        assert_eq!(from_portal.theme, asked, "the portal's answer was not the one asked for");
    }

    /// What the diagnostic line says about each state, which is how a scenario
    /// tells them apart without a screenshot.
    #[test]
    fn every_state_has_its_own_word_in_the_diagnostic() {
        assert_eq!(Source::Ours.token(), "ours");
        assert_eq!(Source::Loading.token(), "loading");
        let failed = Loaded { theme: "x".to_owned(), outcome: Err(Refused::Empty) }.settle();
        assert_eq!(failed.token(), "failed:x:empty");
    }
}
