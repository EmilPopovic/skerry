// SPDX-License-Identifier: Apache-2.0
//! What a person can change about this window, and where each value came from.
//!
//! **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md).**
//! A setting is a line in `$XDG_CONFIG_HOME/skerry/config`, beside the chords
//! [ADR 0025](../../../docs/adr/0025-the-config-file-is-the-keymap-written-out.md)
//! put there. The file says what a *fresh* window does; the session file says
//! what *this* window was last left doing, and it wins — but only for a setting
//! the window itself changed. So editing the file changes a machine that has not
//! been told otherwise, and never argues with somebody who set it from inside.
//!
//! # One table, three readers
//!
//! [`ALL`] is walked by the config's parser, by the session's writer and by the
//! panel. That is the consequence ADR 0027 accepted in exchange for the
//! precedence rule: a seventh setting is one row here and a compile error in
//! each of the three, rather than three lists that drift.
//!
//! # What is not a setting
//!
//! The panel's last two rows do something rather than hold a value — make this
//! the desktop's file manager, and write the keymap out. They are in the panel
//! because that is where a person looks, and they carry no precedence because
//! there is no value to have two answers for.

use crate::views::Layout;

/// Every setting, in the order the panel lists them.
///
/// **Seven.** Each is one the window already changes as it runs, which is what
/// makes the precedence rule testable: a setting the window cannot change has no
/// second opinion to lose to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Setting {
    /// Dark or light.
    Theme,
    /// Whether dotfiles are listed.
    Hidden,
    /// Whether the list of places is open.
    Sidebar,
    /// How wide it is, in logical pixels.
    SidebarWidth,
    /// What a folder with no opinion of its own is laid out as.
    View,
    /// Whether Move to Trash asks first.
    ConfirmTrash,
    /// Where a file's mark comes from — [ADR
    /// 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).
    Icons,
}

/// Every setting, in panel order. See [`Setting`].
pub(crate) const ALL: [Setting; 7] = [
    Setting::Theme,
    Setting::Hidden,
    Setting::Sidebar,
    Setting::SidebarWidth,
    Setting::View,
    Setting::ConfirmTrash,
    Setting::Icons,
];

/// How wide the sidebar moves per press of a step.
///
/// Twenty logical pixels: coarse enough that a keyboard reaches either end of
/// the 150–480 range in a few presses, fine enough to land where a drag would.
pub(crate) const WIDTH_STEP: f32 = 20.0;

/// What kind of control a row carries.
pub(crate) enum Kind {
    /// Two positions with a word each — a theme, a layout, an on and an off.
    ///
    /// **Two words rather than a tick.** "Theme ☑" says nothing; *Dark | Light*
    /// says both what it is and what the other one would be, which is the
    /// discovery half of [rule 4](../../../docs/03-principles.md).
    Either {
        /// The word for the true position.
        yes: &'static str,
        /// The word for the false position.
        no: &'static str,
    },
    /// A number in logical pixels, with a step either way.
    Number,
    /// One of a list, stepped either way.
    ///
    /// **Not three buttons.** One of [`Icons`]' values is the name of a theme
    /// somebody wrote in the file, so the list is not fixed and cannot be drawn
    /// as a row of positions — it is the word in force with a step on each side,
    /// which is [`Kind::Number`]'s shape over something that is not a number.
    Choice,
    /// Something the row does, rather than a value it holds.
    Action,
}

impl Setting {
    /// The name this is written under in the config file.
    ///
    /// Stable once released, like a command id, because it is what somebody's
    /// file says.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Hidden => "hidden",
            Self::Sidebar => "sidebar",
            Self::SidebarWidth => "sidebar_width",
            Self::View => "view",
            Self::ConfirmTrash => "confirm_trash",
            Self::Icons => "icons",
        }
    }

    /// The catalogue key for its label in the panel.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Theme => "settings.theme",
            Self::Hidden => "settings.hidden",
            Self::Sidebar => "settings.sidebar",
            Self::SidebarWidth => "settings.sidebar_width",
            Self::View => "settings.view",
            Self::ConfirmTrash => "settings.confirm_trash",
            Self::Icons => "settings.icons",
        }
    }

    /// The control it carries.
    pub(crate) const fn kind(self) -> Kind {
        match self {
            Self::Theme => Kind::Either { yes: "settings.dark", no: "settings.light" },
            Self::Hidden | Self::ConfirmTrash => {
                Kind::Either { yes: "settings.on", no: "settings.off" }
            }
            Self::Sidebar => Kind::Either { yes: "settings.shown", no: "settings.hidden_word" },
            Self::SidebarWidth => Kind::Number,
            Self::View => Kind::Either { yes: "settings.grid", no: "settings.details" },
            Self::Icons => Kind::Choice,
        }
    }

    /// The setting a name in the file refers to, if it refers to one.
    pub(crate) fn named(name: &str) -> Option<Self> {
        ALL.into_iter().find(|setting| setting.name() == name)
    }
}

/// The source of file icons. Bundled vectors are the default; desktop and named
/// themes remain explicit choices. See ADR 0045.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Icons {
    /// Skerry's bundled vectors, the default. Nothing parses an `index.theme` and the
    /// lookup is never called.
    #[default]
    Marks,
    /// Whatever the desktop is using, when explicitly selected.
    Desktop,
    /// One theme, whatever the desktop does.
    Named(String),
}

impl Icons {
    /// The word this is written as in the config file.
    #[must_use]
    pub(crate) fn token(&self) -> &str {
        match self {
            Self::Marks => "marks",
            Self::Desktop => "desktop",
            Self::Named(name) => name,
        }
    }

    /// The word the panel shows, which is a theme's own name where there is one.
    #[must_use]
    pub(crate) fn shown(&self) -> &str {
        match self {
            Self::Marks => sk_model::i18n::lookup("settings.icons_marks"),
            Self::Desktop => sk_model::i18n::lookup("settings.icons_desktop"),
            Self::Named(name) => name,
        }
    }

    /// What the word after the `=` means, or nothing where it means nothing.
    ///
    /// **Any other word is a theme name**, because a theme name is whatever
    /// somebody installed and this build cannot have a list of them. A name
    /// nothing declares is a load that fails and says so, which is a different
    /// state from this returning nothing.
    fn of(word: &str) -> Option<Self> {
        match word {
            "" => None,
            "marks" => Some(Self::Marks),
            "desktop" => Some(Self::Desktop),
            other if other.split_whitespace().count() == 1 => Some(Self::Named(other.to_owned())),
            _ => None,
        }
    }
}

/// What all seven are set to.
///
/// **A struct rather than the window's own fields.** The config's values and the
/// live ones are two different things that have to be compared — that comparison
/// *is* the precedence rule — and a rule written against two copies of the same
/// type is a rule that can be unit tested without a window.
///
/// **`Clone` rather than `Copy` since ADR 0042.** `icons` may be a theme's name,
/// so one of the seven owns a `String`; every caller that used to take a copy
/// takes a reference, which is what keeps a panel open for a frame from
/// allocating one.
#[derive(Clone, PartialEq, Debug)]
pub struct Values {
    /// Whether the dark palette is in use.
    pub dark: bool,
    /// Whether dotfiles are listed.
    pub hidden: bool,
    /// Whether the list of places is open.
    pub sidebar: bool,
    /// How wide it is, in logical pixels.
    pub aside: f32,
    /// What a folder with no record of its own is laid out as.
    pub layout: Layout,
    /// Whether Move to Trash asks first.
    pub confirm_trash: bool,
    /// Where a file's mark comes from.
    pub icons: Icons,
}

impl Default for Values {
    fn default() -> Self {
        Self {
            dark: true,
            hidden: false,
            sidebar: true,
            aside: crate::sidebar::DEFAULT_WIDTH,
            layout: Layout::Details,
            confirm_trash: false,
            icons: Icons::default(),
        }
    }
}

impl Values {
    /// The true-or-false position of one setting, for the two-word controls.
    ///
    /// [`Setting::SidebarWidth`] has no such position and answers `false`, which
    /// no caller reads: the panel branches on [`Kind`] before it asks.
    #[must_use]
    pub(crate) fn yes(&self, setting: Setting) -> bool {
        match setting {
            Setting::Theme => self.dark,
            Setting::Hidden => self.hidden,
            Setting::Sidebar => self.sidebar,
            Setting::View => self.layout == Layout::Grid,
            Setting::ConfirmTrash => self.confirm_trash,
            Setting::SidebarWidth | Setting::Icons => false,
        }
    }

    /// Puts one setting into a position.
    pub(crate) const fn set(&mut self, setting: Setting, yes: bool) {
        match setting {
            Setting::Theme => self.dark = yes,
            Setting::Hidden => self.hidden = yes,
            Setting::Sidebar => self.sidebar = yes,
            Setting::View => self.layout = if yes { Layout::Grid } else { Layout::Details },
            Setting::ConfirmTrash => self.confirm_trash = yes,
            Setting::SidebarWidth | Setting::Icons => {}
        }
    }

    /// Moves a choice one position, wrapping at both ends.
    ///
    /// **`named` is the theme the ring keeps a place for**, so a person who
    /// stepped away from `Papirus` to look at the marks can step back to it
    /// without editing the file. The window remembers it, because a value that
    /// has been stepped past is not in the value any more — see
    /// [`crate::app::App::step_icons`].
    pub(crate) fn step(&mut self, delta: isize, named: Option<&str>) {
        let mut ring = vec![Icons::Marks, Icons::Desktop];
        if let Some(name) = named.map(str::to_owned).or_else(|| match &self.icons {
            Icons::Named(name) => Some(name.clone()),
            Icons::Marks | Icons::Desktop => None,
        }) {
            ring.push(Icons::Named(name));
        }
        let at = ring.iter().position(|it| *it == self.icons).unwrap_or(0);
        let Ok(span) = isize::try_from(ring.len()) else { return };
        let Ok(from) = isize::try_from(at) else { return };
        let to = usize::try_from(from.wrapping_add(delta).rem_euclid(span)).unwrap_or(0);
        if let Some(chosen) = ring.get(to) {
            self.icons = chosen.clone();
        }
    }

    /// Reads one setting from the text after the `=` in a config line.
    ///
    /// Whether the word was one this setting understands. A word it does not is
    /// a problem printed at start-up, which is what a bad chord already is.
    pub(crate) fn take(&mut self, setting: Setting, word: &str) -> bool {
        let word = word.trim();
        match setting.kind() {
            Kind::Either { .. } => {
                let Some(yes) = truth(setting, word) else { return false };
                self.set(setting, yes);
                true
            }
            Kind::Number => {
                let Ok(width) = word.parse::<f32>() else { return false };
                self.aside = width.clamp(crate::sidebar::NARROWEST, crate::sidebar::WIDEST);
                true
            }
            Kind::Choice => {
                let Some(icons) = Icons::of(word) else { return false };
                self.icons = icons;
                true
            }
            Kind::Action => false,
        }
    }

    /// The word this setting is written as in a file.
    pub(crate) fn word(&self, setting: Setting) -> String {
        match setting {
            Setting::Theme => (if self.dark { "dark" } else { "light" }).to_owned(),
            Setting::View => self.layout.token().to_owned(),
            Setting::SidebarWidth => format!("{}", self.aside.round()),
            Setting::Icons => self.icons.token().to_owned(),
            other => (if self.yes(other) { "on" } else { "off" }).to_owned(),
        }
    }
}

/// The word a setting's file value may be, as a truth.
///
/// **`on`/`off` everywhere, and the domain word beside it.** `theme = on` is
/// nonsense, but a person who writes `hidden = true` after years of other
/// programs' files should not have their line refused over a synonym.
fn truth(setting: Setting, word: &str) -> Option<bool> {
    let word = word.to_ascii_lowercase();
    match (setting, word.as_str()) {
        (Setting::Theme, "dark") | (Setting::View, "grid") => Some(true),
        (Setting::Theme, "light") | (Setting::View, "details") => Some(false),
        (Setting::Theme | Setting::View, _) => None,
        (_, "on" | "yes" | "true") => Some(true),
        (_, "off" | "no" | "false") => Some(false),
        _ => None,
    }
}

/// Where the value a row shows came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Source {
    /// Nobody has said, so this is what the program starts as.
    Default,
    /// A line in the config file says so.
    File,
    /// This window changed it, and the session file remembers.
    Window,
}

impl Source {
    /// The catalogue key for the word shown beside the value.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Default => "settings.from_default",
            Self::File => "settings.from_file",
            Self::Window => "settings.from_window",
        }
    }
}

/// Which settings the file named and which this window has changed.
///
/// **Two bit sets and nothing else.** The values themselves live where they are
/// used — the theme in the palette, `hidden` on every pane — because a second
/// copy of a live value is a second copy to keep in step. What cannot live there
/// is *provenance*, which is what the precedence rule and the panel's third
/// column are both about.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Marks {
    named: [bool; ALL.len()],
    touched: [bool; ALL.len()],
}

impl Marks {
    /// Records that the config file named this setting.
    pub(crate) const fn name(&mut self, setting: Setting) {
        self.named[at(setting)] = true;
    }

    /// Records that this window changed it.
    pub(crate) const fn touch(&mut self, setting: Setting) {
        self.touched[at(setting)] = true;
    }

    /// Whether the config file named it.
    #[must_use]
    pub(crate) const fn is_named(&self, setting: Setting) -> bool {
        self.named[at(setting)]
    }

    /// Takes `other`'s touched marks, keeping our own names.
    ///
    /// **Two files, one set of marks.** The names come from the config and the
    /// touches from the session, and each is read once at start-up from the file
    /// that knows it.
    pub(crate) const fn absorb(&mut self, other: &Self) {
        self.touched = other.touched;
    }

    /// Whether this window changed it, which is what lets the session override
    /// the file.
    #[must_use]
    pub(crate) const fn touched(&self, setting: Setting) -> bool {
        self.touched[at(setting)]
    }

    /// Where the value now in force came from.
    #[must_use]
    pub(crate) const fn source(&self, setting: Setting) -> Source {
        if self.touched[at(setting)] {
            Source::Window
        } else if self.named[at(setting)] {
            Source::File
        } else {
            Source::Default
        }
    }

    /// The names this window changed, in table order, for the session's
    /// `touched` line.
    pub(crate) fn names(&self) -> Vec<&'static str> {
        ALL.into_iter().filter(|it| self.touched(*it)).map(Setting::name).collect()
    }

    /// Marks every setting a `touched` line names.
    pub(crate) fn read(&mut self, line: &str) {
        for word in line.split_whitespace() {
            if let Some(setting) = Setting::named(word) {
                self.touch(setting);
            }
        }
    }
}

const fn at(setting: Setting) -> usize {
    match setting {
        Setting::Theme => 0,
        Setting::Hidden => 1,
        Setting::Sidebar => 2,
        Setting::SidebarWidth => 3,
        Setting::View => 4,
        Setting::ConfirmTrash => 5,
        Setting::Icons => 6,
    }
}

/// The panel's rows: the seven settings, then the two things it can do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Row {
    /// One of the seven.
    Of(Setting),
    /// Make this the desktop's handler for folders.
    Handler,
    /// Write the keymap out.
    Keymap,
}

/// Every row, in the order the panel draws them.
pub(crate) fn rows() -> Vec<Row> {
    let mut out: Vec<Row> = ALL.into_iter().map(Row::Of).collect();
    out.push(Row::Handler);
    out.push(Row::Keymap);
    out
}

impl Row {
    /// The catalogue key for its label.
    pub(crate) const fn label_key(self) -> &'static str {
        match self {
            Self::Of(setting) => setting.label_key(),
            Self::Handler => "settings.handler",
            Self::Keymap => "settings.keymap",
        }
    }

    /// The control it carries.
    pub(crate) const fn kind(self) -> Kind {
        match self {
            Self::Of(setting) => setting.kind(),
            Self::Handler => Kind::Either { yes: "settings.yes", no: "settings.no" },
            Self::Keymap => Kind::Action,
        }
    }
}

/// The panel, while it is open.
pub(crate) struct Panel {
    /// Which row the keyboard is on.
    pub(crate) active: usize,
    /// Whether this machine's `mimeapps.list` already names us for folders.
    ///
    /// **Read on a job**, because it is an `openat` and a read — so the row
    /// starts as `None` and says nothing rather than saying *No* and being wrong
    /// for a frame.
    pub(crate) handler: Option<bool>,
    /// What the last thing the panel did says about itself: a path written, or
    /// why it was not.
    pub(crate) said: Option<String>,
}

impl Panel {
    pub(crate) const fn new() -> Self {
        Self { active: 0, handler: None, said: None }
    }

    /// Moves the highlight, wrapping at both ends the way every other list in
    /// this program does.
    pub(crate) fn step(&mut self, delta: isize) {
        let count = rows().len();
        let Ok(span) = isize::try_from(count) else { return };
        let Ok(from) = isize::try_from(self.active) else { return };
        self.active = usize::try_from(from.wrapping_add(delta).rem_euclid(span)).unwrap_or(0);
    }

    /// The row the keyboard is on.
    pub(crate) fn row(&self) -> Row {
        rows().get(self.active).copied().unwrap_or(Row::Keymap)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{ALL, Icons, Kind, Marks, Panel, Row, Setting, Source, Values, rows};
    use crate::views::Layout;

    /// The round trip every one of the seven has to survive, because the file is
    /// written by us and read by us and a setting that cannot do this is a
    /// setting that silently resets.
    #[test]
    fn every_setting_reads_back_the_word_it_writes() {
        for setting in ALL {
            for start in [true, false] {
                let mut wrote = Values::default();
                wrote.set(setting, start);
                wrote.aside = 300.0;
                wrote.icons = Icons::Named("breeze-dark".to_owned());
                let word = wrote.word(setting);
                let mut read = Values::default();
                assert!(read.take(setting, &word), "{} refused its own {word:?}", setting.name());
                assert_eq!(
                    read.yes(setting),
                    wrote.yes(setting),
                    "{} did not survive {word:?}",
                    setting.name()
                );
                // The two settings with no true-or-false position, checked as
                // what they are — without this the loop above asserts `false ==
                // false` about them and says nothing at all.
                if setting == Setting::SidebarWidth {
                    assert!(
                        (read.aside - wrote.aside).abs() < f32::EPSILON,
                        "the width came back as {} from {word:?}",
                        read.aside
                    );
                }
                if setting == Setting::Icons {
                    assert_eq!(read.icons, wrote.icons, "the icons came back from {word:?}");
                }
            }
        }
    }

    /// All three values of the first setting that is neither a truth nor a
    /// number, through the file and back.
    #[test]
    fn the_three_icon_sources_survive_the_file() {
        for icons in [Icons::Marks, Icons::Desktop, Icons::Named("Papirus-Dark".to_owned())] {
            let wrote = Values { icons: icons.clone(), ..Values::default() };
            let word = wrote.word(Setting::Icons);
            let mut read = Values::default();
            assert!(read.take(Setting::Icons, &word), "icons refused its own {word:?}");
            assert_eq!(read.icons, icons, "{word:?} did not survive");
        }
        assert_eq!(Values::default().icons, Icons::Marks, "a fresh install uses bundled icons");
        assert!(matches!(Setting::Icons.kind(), Kind::Choice));
    }

    /// A theme name is whatever somebody installed, so almost any word is one —
    /// but a line with nothing after the `=`, or a sentence, is a problem printed
    /// at start-up rather than a theme called *Adwaita Dark*.
    #[test]
    fn an_icons_line_with_no_word_or_two_words_is_refused() {
        for word in ["", "   ", "Adwaita Dark", "\ttwo words\t"] {
            let mut values = Values::default();
            assert!(!values.take(Setting::Icons, word), "{word:?} was taken as a theme");
            assert_eq!(values.icons, Icons::Marks, "a refused word still moved the setting");
        }
    }

    /// **The theme the file named stays in the ring**, so stepping away from it
    /// to look at the marks is a step that can be taken back without editing the
    /// file — and stepping is a cycle, like every other list in this window.
    #[test]
    fn stepping_the_icons_row_cycles_and_keeps_the_theme_that_was_named() {
        let mut values = Values { icons: Icons::Named("Yaru".to_owned()), ..Values::default() };
        let kept = Some("Yaru");
        values.step(1, kept);
        assert_eq!(values.icons, Icons::Marks);
        values.step(1, kept);
        assert_eq!(values.icons, Icons::Desktop);
        values.step(1, kept);
        assert_eq!(values.icons, Icons::Named("Yaru".to_owned()), "the named theme left the ring");
        values.step(-1, kept);
        assert_eq!(values.icons, Icons::Desktop);

        // With no theme named there are two positions and no third to reach.
        let mut two = Values::default();
        two.step(1, None);
        assert_eq!(two.icons, Icons::Desktop);
        two.step(1, None);
        assert_eq!(two.icons, Icons::Marks);
    }

    #[test]
    fn a_name_the_table_does_not_hold_is_not_a_setting() {
        assert_eq!(Setting::named("theme"), Some(Setting::Theme));
        assert_eq!(Setting::named("sidebar_width"), Some(Setting::SidebarWidth));
        assert_eq!(Setting::named("go.back"), None);
        assert_eq!(Setting::named(""), None);
    }

    /// A word the setting does not understand is refused rather than taken as
    /// its opposite, which is what makes a typo a problem at start-up.
    #[test]
    fn a_word_a_setting_does_not_know_is_refused() {
        let mut values = Values::default();
        assert!(!values.take(Setting::Theme, "on"), "theme took an on/off word");
        assert!(!values.take(Setting::View, "list"), "view took a word it does not draw");
        assert!(!values.take(Setting::Hidden, "sometimes"));
        assert!(!values.take(Setting::SidebarWidth, "wide"));
        assert_eq!(values, Values::default(), "a refused word still changed something");
    }

    /// The synonyms other programs' files use, which a person will write.
    #[test]
    fn on_yes_and_true_all_mean_the_same_thing() {
        for word in ["on", "yes", "true", "TRUE"] {
            let mut values = Values::default();
            assert!(values.take(Setting::Hidden, word));
            assert!(values.hidden, "{word} did not read as true");
        }
        for word in ["off", "no", "false"] {
            let mut values = Values { hidden: true, ..Values::default() };
            assert!(values.take(Setting::Hidden, word));
            assert!(!values.hidden, "{word} did not read as false");
        }
    }

    /// A width outside the range the divider can be dragged to is clamped, not
    /// refused: the line said something meaningful and the window has bounds.
    #[test]
    fn a_width_is_clamped_to_what_a_drag_could_have_produced() {
        let mut values = Values::default();
        assert!(values.take(Setting::SidebarWidth, "4000"));
        assert!((values.aside - crate::sidebar::WIDEST).abs() < f32::EPSILON);
        assert!(values.take(Setting::SidebarWidth, "1"));
        assert!((values.aside - crate::sidebar::NARROWEST).abs() < f32::EPSILON);
    }

    /// The three sources, which is the panel's third column and the whole point
    /// of keeping the marks separate from the values.
    #[test]
    fn a_value_says_whether_it_came_from_the_file_or_the_window() {
        let mut marks = Marks::default();
        assert_eq!(marks.source(Setting::Theme), Source::Default);
        marks.name(Setting::Theme);
        assert_eq!(marks.source(Setting::Theme), Source::File);
        marks.touch(Setting::Theme);
        assert_eq!(marks.source(Setting::Theme), Source::Window);
        assert_eq!(marks.source(Setting::Hidden), Source::Default, "a mark leaked sideways");
    }

    /// The session line, both ways. An old file has no line at all, which reads
    /// as nothing touched — so it restores the config's values, which is what a
    /// first upgrade should do.
    #[test]
    fn the_touched_line_round_trips_and_an_absent_one_touches_nothing() {
        let mut marks = Marks::default();
        marks.touch(Setting::Hidden);
        marks.touch(Setting::ConfirmTrash);
        assert_eq!(marks.names(), vec!["hidden", "confirm_trash"]);
        let mut read = Marks::default();
        read.read(&marks.names().join(" "));
        for setting in ALL {
            assert_eq!(read.touched(setting), marks.touched(setting), "{}", setting.name());
        }
        let mut none = Marks::default();
        none.read("");
        none.read("something_else_entirely");
        assert!(none.names().is_empty());
    }

    #[test]
    fn the_panel_wraps_at_both_ends_and_ends_on_the_two_actions() {
        let all = rows();
        assert_eq!(all.len(), ALL.len() + 2);
        assert_eq!(all[ALL.len()], Row::Handler);
        let mut panel = Panel::new();
        assert_eq!(panel.row(), Row::Of(Setting::Theme));
        panel.step(-1);
        assert_eq!(panel.row(), Row::Keymap);
        panel.step(1);
        assert_eq!(panel.row(), Row::Of(Setting::Theme));
    }

    #[test]
    fn the_view_setting_is_the_layout_a_folder_with_no_record_gets() {
        let mut values = Values::default();
        assert_eq!(values.layout, Layout::Details);
        assert!(values.take(Setting::View, "grid"));
        assert_eq!(values.layout, Layout::Grid);
        assert_eq!(values.word(Setting::View), "grid");
    }
}
