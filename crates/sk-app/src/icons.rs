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
    /// A theme is being loaded on a job and has not come back. The name is what
    /// the setting asked for, or nothing where it said `desktop` and the job has
    /// not yet asked the desktop.
    Loading(Option<String>),
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
            Self::Loading(None) => i18n::lookup("icons.loading").to_owned(),
            Self::Loading(Some(theme)) => i18n::format("icons.loading_named", &[theme]),
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
            Self::Ours | Self::Loading(_) | Self::Failed { .. } => None,
        }
    }

    /// What the diagnostic line calls this, which a scenario asserts against.
    pub(crate) fn token(&self) -> String {
        match self {
            Self::Ours => "ours".to_owned(),
            Self::Loading(None) => "loading".to_owned(),
            Self::Loading(Some(theme)) => format!("loading:{theme}"),
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
pub(crate) fn load(
    icons: &Icons,
    from_the_portal: Option<String>,
    cancel: &sk_util::Cancel,
) -> Option<Loaded> {
    let theme = match icons {
        Icons::Marks => return None,
        Icons::Named(name) => name.clone(),
        // The desktop's own answer, then the files it writes, then the theme the
        // specification requires every machine to have.
        Icons::Desktop => from_the_portal
            .or_else(sk_desktop::icons::from_the_desktop_files)
            .unwrap_or_else(|| sk_desktop::icons::FALLBACK.to_owned()),
    };
    let outcome = Theme::load_cancellably(&theme, &sk_desktop::icons::icon_dirs(), cancel);
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
            Source::Loading(None),
            Source::Loading(Some("Adwaita".to_owned())),
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
        // Everything that knows a theme's name says it, which is what makes the
        // sentence actionable rather than a shrug.
        assert!(said[2..].iter().all(|it| it.contains("Adwaita")), "{said:?}");
    }

    /// **`marks` asks for no load at all.** Not a load that is skipped later — the
    /// job is never started, so nothing parses an `index.theme` and nothing walks
    /// an icon directory.
    #[test]
    fn under_marks_nothing_is_loaded() {
        let none = sk_util::Cancel::new();
        assert!(super::load(&Icons::Marks, None, &none).is_none());
        assert!(super::load(&Icons::Marks, Some("Adwaita".to_owned()), &none).is_none());
    }

    /// A theme nothing declares fails under its own name, so the panel can say
    /// which one — and the name the *portal* gave is the one that appears, not
    /// the word `desktop`.
    #[test]
    fn a_theme_that_cannot_be_had_fails_under_the_name_that_was_asked_for() {
        let asked = "no-theme-is-called-this-anywhere";
        let none = sk_util::Cancel::new();
        let loaded = super::load(&Icons::Named(asked.to_owned()), None, &none).unwrap();
        assert_eq!(loaded.theme, asked);
        assert_eq!(loaded.outcome.as_ref().unwrap_err(), &Refused::Unknown);
        let source = loaded.settle();
        assert!(source.says().contains(asked), "{}", source.says());
        assert!(matches!(source, Source::Failed { .. }), "a failed load is not a loaded theme");
        assert_eq!(source.token(), format!("failed:{asked}:unknown"));

        let from_portal = super::load(&Icons::Desktop, Some(asked.to_owned()), &none).unwrap();
        assert_eq!(from_portal.theme, asked, "the portal's answer was not the one asked for");
    }

    /// The shared-mime-info convention: the type's own name, then the generic the
    /// database names — read, not built — and the one every theme has.
    #[test]
    fn a_file_answers_to_its_type_then_to_the_generic_the_database_names() {
        let mut types = sk_desktop::MimeDatabase::default();
        types.absorb_generic_icons(
            "application/pdf:x-office-document\napplication/x-compressed-tar:package-x-generic\n",
        );
        let names = |directory, mime| super::names_for(directory, mime, &types);
        assert_eq!(names(true, Some("inode/directory")), vec!["folder"]);
        assert_eq!(names(true, None), vec!["folder"]);
        assert_eq!(names(false, Some("text/plain")), vec!["text-plain", "text-x-generic"]);
        // **The case this test used to assert the other way.** A PDF asked for a
        // built `application-x-generic`, which almost no theme ships, and fell to
        // the text icon with `x-office-document` sitting in the theme.
        assert_eq!(
            names(false, Some("application/pdf")),
            vec!["application-pdf", "x-office-document", "text-x-generic"]
        );
        assert_eq!(
            names(false, Some("application/x-compressed-tar"))[1],
            "package-x-generic",
            "an archive does not ask for the archive icon"
        );
        // A type the database names no generic for is the one place the built
        // name is right.
        assert_eq!(
            names(false, Some("application/x-unknown-thing")),
            vec!["application-x-unknown-thing", "application-x-generic", "text-x-generic"]
        );
        // The `+` stays: a theme's file really is called `image-svg+xml.png`.
        assert_eq!(names(false, Some("image/svg+xml"))[0], "image-svg+xml");
        // A file the databases do not name still draws something.
        assert_eq!(names(false, None), vec!["text-x-generic"]);
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
        // **Never urgent.** A press has to reach the child before a mark does,
        // and the queue's only word for that is `ahead`.
        assert!(!want.is_urgent(), "an icon would take the batch a `Space` needs");
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

    /// **Only what the atlas has stopped holding is forgotten.** `folder` and
    /// `text-x-generic` are the first two names minted in any session and the two
    /// most drawn, so dropping by mint order would drop exactly them.
    #[test]
    fn a_name_the_atlas_still_holds_is_kept_however_old_it_is() {
        let mut names = super::Names::default();
        let (_, oldest) = names.name(std::path::Path::new("/icons/a/folder.png"), 16);
        for at in 0..3000 {
            names.name(std::path::Path::new(&format!("/icons/a/{at}.png")), 16);
        }
        names.forget_the_oldest(|image| image == oldest);
        let (_, again) = names.name(std::path::Path::new("/icons/a/folder.png"), 16);
        assert_eq!(again, oldest, "the most-drawn name was re-minted");
        assert_eq!(names.held(), 1, "a name nothing holds was kept");
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
        assert_eq!(Source::Loading(None).token(), "loading");
        assert_eq!(Source::Loading(Some("Yaru".to_owned())).token(), "loading:Yaru");
        let failed = Loaded { theme: "x".to_owned(), outcome: Err(Refused::Empty) }.settle();
        assert_eq!(failed.token(), "failed:x:empty");
    }
}

/// What a mark `side` logical pixels across asks the theme for at `scale`: the
/// nominal size, the step, and the atlas band the picture lands in.
///
/// **One answer for the window and the capture**, because the two used to derive
/// it separately and a fix to one was a frame the other disagreed with.
///
/// **The step is rounded up, and it is not a rounding preference.** The
/// specification's scale is an integer and no theme ships an `@1.25x`
/// directory, so 125% asks for a `@2x` icon and the GPU minifies it to the twenty
/// device pixels the mark is. Rounding *down* asks for 16-pixel art, and
/// `image_quad` never enlarges — so the mark drew at 16 device pixels in a
/// 20-pixel box, visibly smaller than every other mark in the window. Captured at
/// 125% and looked at, which is the only way that one is visible.
pub(crate) fn sizing(side: f32, scale: f32) -> (u32, u32, u32) {
    let nominal = side.round().max(1.0) as u32;
    let step = scale.ceil().max(1.0) as u32;
    let band = sk_gpu::icon_band((side * scale).round().max(1.0) as u32);
    (nominal, step, band)
}

/// The icon names a file answers to, most specific first.
///
/// **The shared-mime-info convention, which is what every theme is built
/// against**: the type's own icon, then the generic the MIME database names for
/// it — [`sk_desktop::MimeDatabase::icon_names`] — and last the generic every
/// theme has. A directory is `folder`, which is the one name no theme is
/// without.
///
/// A name the theme does not hold is a *miss* and the next is tried; when all of
/// them miss, the geometric mark is what draws, which is
/// [`Source::Ours`] arrived at for one file rather than for the window.
pub(crate) fn names_for(
    directory: bool,
    mime: Option<&str>,
    types: &sk_desktop::MimeDatabase,
) -> Vec<String> {
    /// The name every theme has, which is what a file the databases cannot type
    /// falls through to.
    const LAST: &str = "text-x-generic";
    if directory {
        return vec!["folder".to_owned()];
    }
    let Some(mime) = mime.filter(|it| it.contains('/')) else {
        return vec![LAST.to_owned()];
    };
    let mut names = types.icon_names(mime).to_vec();
    names.dedup();
    if !names.iter().any(|name| name == LAST) {
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
    /// Icons that will never reach the atlas, which are never asked for again.
    ///
    /// **Rule 1's idle promise, and it is not a nicety.** A file the atlas never
    /// holds is one `settle_icons` would queue on every frame, and the reply
    /// marks the window dirty — so one of these is a window that redraws and
    /// re-decodes for as long as it is open.
    ///
    /// Two things land here. A picture the child could not decode: a theme really
    /// does ship `.xpm`, because the extension is in the specification, and
    /// `skerry-thumbnailer` decodes PNG, JPEG, SVG and GIF. And a band with every
    /// slot drawn this frame, which has nowhere to evict — ADR 0042's own revisit
    /// trigger, degrading to an icon that does not arrive rather than to a window
    /// that spins.
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

    /// How many names are held, for the test below.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.named.len()
    }

    /// Records that this one will never reach the atlas.
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

    /// Forgets the names the atlas no longer holds, once there are more than the
    /// four bands could hold.
    ///
    /// **Bounded by what the four bands hold**, for the reason the thumbnail
    /// names are bounded by the one grid: this map would otherwise grow for the
    /// life of a session, one entry per kind ever scrolled past at every size the
    /// window has been.
    ///
    /// **And dropped by what the atlas has, not by what was minted first.** The
    /// thumbnail map sorts on mint order because for a thumbnail that is scroll
    /// order; the first names minted here are `folder` and `text-x-generic`, which
    /// are the two most drawn in any session, so the same sort would drop exactly
    /// those and the marks would flicker back to geometric while they were
    /// re-minted.
    pub(crate) fn forget_the_oldest(&mut self, holds: impl Fn(ImageKey) -> bool) {
        const KEEP: usize = 1_360 * 2;
        if self.named.len() <= KEEP {
            return;
        }
        self.named.retain(|_, image| holds(*image));
        self.refused.retain(|key| !self.named.contains_key(key));
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
///
/// **`ahead`, because nobody is waiting for this one.** An icon is a preview in
/// the child's terms and not in the queue's: `Space` is a press somebody made and
/// an icon is a mark that will arrive. Without this the eight icons a new
/// directory asks for take the urgent batch and the quick look waits a round trip
/// behind them, which is what the scenario saw as a preview that never opened.
pub(crate) fn wanted(path: PathBuf, band: u32) -> Want {
    Want {
        kind: sk_preview::Kind::Image,
        ask: Ask::Preview,
        path,
        mtime: 0,
        within: (band, band),
        page: 0,
        ahead: true,
    }
}
