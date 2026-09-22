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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sk_desktop::icons::{Refused, Theme};
use sk_gpu::ImageKey;
use sk_preview::{Ask, Want};

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

    /// The theme to look a name up in, where one is loaded.
    pub(crate) const fn theme(&self) -> Option<&Theme> {
        match self {
            Self::Ready(theme) => Some(theme),
            Self::Ours | Self::Loading | Self::Failed { .. } => None,
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

    /// The freedesktop convention, which is what every theme is built against.
    #[test]
    fn a_file_answers_to_its_type_then_to_its_familys_generic() {
        assert_eq!(super::names_for(true, Some("inode/directory")), vec!["folder"]);
        assert_eq!(super::names_for(true, None), vec!["folder"]);
        assert_eq!(
            super::names_for(false, Some("text/plain")),
            vec!["text-plain", "text-x-generic"]
        );
        assert_eq!(
            super::names_for(false, Some("application/pdf")),
            vec!["application-pdf", "application-x-generic", "text-x-generic"]
        );
        // The `+` stays: a theme's file really is called `image-svg+xml.png`.
        assert_eq!(super::names_for(false, Some("image/svg+xml"))[0], "image-svg+xml");
        // A file the databases do not name still draws something.
        assert_eq!(super::names_for(false, None), vec!["text-x-generic"]);
    }

    /// **An icon asks for a preview and never for a thumbnail**, which is what
    /// keeps ADR 0034's grant unwidened: a thumbnail is forced to 128 pixels and
    /// writes the shared XDG cache, and an icon wants neither.
    #[test]
    fn an_icon_is_a_preview_request_at_its_own_band() {
        let want = super::wanted(std::path::PathBuf::from("/usr/share/icons/a/folder.png"), 32);
        assert_eq!(want.ask, sk_preview::Ask::Preview);
        assert_eq!(want.within, (32, 32));
        assert_eq!(want.page, 0);
        assert!(!want.ahead, "an icon is wanted, not guessed");
        // And it is the key a reply is matched against.
        let mut names = super::Names::default();
        let (key, image) = names.name(&want.path, 32);
        assert_eq!(key, want.key());
        assert_eq!(names.look_up(&key), Some(image));
        // The same file at another band is another picture, because 16-pixel art
        // is not 48-pixel art downscaled.
        let (_, bigger) = names.name(&want.path, 64);
        assert_ne!(image, bigger);
    }

    /// **An icon the child cannot draw is asked for once.** Without this a single
    /// `.xpm` in a theme is a window that queues a decode on every frame and is
    /// woken by the refusal — which is rule 1's idle promise broken by a picture
    /// nobody can see.
    #[test]
    fn an_icon_the_child_refused_is_never_asked_for_again() {
        let mut names = super::Names::default();
        let (key, _) = names.name(std::path::Path::new("/icons/a/old.xpm"), 16);
        assert!(!names.was_refused(&key));
        names.refuse(&key);
        assert!(names.was_refused(&key));
        // And the same file at another band is a different question.
        let (bigger, _) = names.name(std::path::Path::new("/icons/a/old.xpm"), 32);
        assert!(!names.was_refused(&bigger));

        // The list is bounded by a clear, and what was refused most recently is
        // still refused after one — which is the half that keeps the guard
        // working while the other half keeps it from growing without end.
        let mut last = key;
        for at in 0..600 {
            let (key, _) = names.name(std::path::Path::new(&format!("/icons/a/{at}.xpm")), 16);
            names.refuse(&key);
            last = key;
        }
        assert!(names.was_refused(&last), "the newest refusal was forgotten");
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

/// The icon names a file answers to, most specific first.
///
/// **The freedesktop convention, which is what every theme is built against.** A
/// MIME type `text/plain` is `text-plain`, and a theme that does not ship that
/// one ships `text-x-generic` — so the list is the specific name, its family's
/// generic, and the generic every theme has. A directory is `folder`, which is
/// the one name no theme is without.
///
/// A name the theme does not hold is a *miss* and the next is tried; when all of
/// them miss, the geometric mark is what draws, which is
/// [`Source::Ours`] arrived at for one file rather than for the window.
pub(crate) fn names_for(directory: bool, mime: Option<&str>) -> Vec<String> {
    /// The name every theme has, which is what a file the databases cannot type
    /// falls through to.
    const LAST: &str = "text-x-generic";
    if directory {
        return vec!["folder".to_owned()];
    }
    let Some((top, sub)) = mime.and_then(|it| it.split_once('/')) else {
        return vec![LAST.to_owned()];
    };
    let mut names = vec![format!("{top}-{sub}"), format!("{top}-x-generic")];
    if top != "text" {
        names.push(LAST.to_owned());
    }
    names
}

/// The numbers the icon atlas knows icons by, and what they stand for.
///
/// **Keyed by the file and the box, which is ADR 0042's key by a shorter road.**
/// That record names the theme generation, the icon's name, its nominal size and
/// its scale; the resolved path already carries the first two — a theme's name is
/// a directory component of it — and the box carries the other two. Two themes
/// that both fall through to the same `hicolor` file are drawing the same
/// picture, and sharing one slot for it is right rather than a collision.
///
/// The cost of that is a theme edited in place under a running window, which
/// keeps the picture it had. A thumbnail answers this with a modification time in
/// its key; an icon cannot, because reading one would be a `statx` on the UI
/// thread and rule 1 does not allow that for a picture nobody has changed.
#[derive(Default)]
pub(crate) struct Names {
    named: HashMap<sk_preview::Key, ImageKey>,
    /// Icons the child could not decode, which are never asked for again.
    ///
    /// **Rule 1's idle promise, and it is not a nicety.** A file the atlas never
    /// holds is one `settle_icons` would queue on every frame, and the reply
    /// marks the window dirty — so one `.xpm` in a theme is a window that redraws
    /// and re-decodes for as long as it is open. A theme really does ship those:
    /// the extension is in the specification and `skerry-thumbnailer` decodes
    /// PNG, JPEG, SVG and GIF.
    refused: Vec<sk_preview::Key>,
    next: u64,
}

impl Names {
    /// The atlas key for a file at a box, minting one if it is new.
    pub(crate) fn name(&mut self, path: &Path, band: u32) -> (sk_preview::Key, ImageKey) {
        let key = key_for(path, band);
        if let Some(image) = self.named.get(&key) {
            return (key, *image);
        }
        self.next += 1;
        let image = ImageKey(self.next);
        self.named.insert(key.clone(), image);
        (key, image)
    }

    /// The atlas key an answer belongs to, if anything still wants it.
    pub(crate) fn look_up(&self, key: &sk_preview::Key) -> Option<ImageKey> {
        self.named.get(key).copied()
    }

    /// Records that the child could not draw this one.
    ///
    /// **Bounded, and cleared whole when it fills.** A theme has a few hundred
    /// names at four bands; a list that grew without end would be a leak, and
    /// asking again after a clear is one round trip rather than one a frame.
    pub(crate) fn refuse(&mut self, key: &sk_preview::Key) {
        const MOST: usize = 512;
        if self.refused.len() >= MOST {
            self.refused.clear();
        }
        if !self.refused.contains(key) {
            self.refused.push(key.clone());
        }
    }

    /// Whether the child has already said it cannot draw this one.
    pub(crate) fn was_refused(&self, key: &sk_preview::Key) -> bool {
        self.refused.contains(key)
    }

    /// Forgets the names minted longest ago, once there are more than the atlas
    /// could hold.
    ///
    /// **Bounded by what the four bands hold**, for the reason the thumbnail
    /// names are bounded by the one grid: this map would otherwise grow for the
    /// life of a session, one entry per kind ever scrolled past at every size the
    /// window has been.
    pub(crate) fn forget_the_oldest(&mut self) {
        const KEEP: usize = 1_360 * 2;
        if self.named.len() <= KEEP {
            return;
        }
        let mut held: Vec<(ImageKey, sk_preview::Key)> =
            self.named.iter().map(|(key, image)| (*image, key.clone())).collect();
        held.sort_unstable_by_key(|(image, _)| *image);
        for (_, key) in held.iter().take(self.named.len() - KEEP) {
            self.named.remove(key);
        }
    }
}

/// What an icon's answer is filed under.
fn key_for(path: &Path, band: u32) -> sk_preview::Key {
    // **No modification time.** See [`Names`]: reading one would be a syscall on
    // the UI thread for a file that does not change while a window is open.
    sk_preview::Key { path: path.to_path_buf(), mtime: 0, within: (band, band), page: 0 }
}

/// A want for one icon file, at the band it will be drawn from.
///
/// **`Ask::Preview`, not `Ask::Thumbnail`** — ADR 0042. A thumbnail is forced to
/// the cache's 128 pixels and writes the shared XDG cache, which are exactly the
/// two things an icon must not do; `Ask::Preview` already names a box in device
/// pixels and already returns straight `RGBA8`. **The wire does not change and
/// the protocol version stays 4.**
pub(crate) fn wanted(path: PathBuf, band: u32) -> Want {
    Want {
        kind: sk_preview::Kind::Image,
        ask: Ask::Preview,
        path,
        mtime: 0,
        within: (band, band),
        page: 0,
        ahead: false,
    }
}
