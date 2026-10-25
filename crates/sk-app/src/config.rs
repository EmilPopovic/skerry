// SPDX-License-Identifier: Apache-2.0
//! The keymap, as a file a person owns.
//!
//! **[ADR 0025](../../../docs/adr/0025-the-config-file-is-the-keymap-written-out.md).**
//! `$XDG_CONFIG_HOME/skerry/config` holds one line per command —
//! `command.id = ctrl+shift+f3, f5` — read at startup and written only when
//! somebody asks. The program never rewrites it, so a comment a person adds
//! stays and the file is a thing they edit rather than a thing that is edited
//! under them.
//!
//! # Why there is no format crate here
//!
//! `key = value` with `#` comments is the third file this program parses by hand,
//! after GTK's `bookmarks` and its own session — and each of them is under a
//! hundred lines for the same reason: rule 10 asks whether a dependency earns its
//! row, and a `HashMap<String, String>` does not.
//!
//! # Settings, beside the keys
//!
//! **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)**
//! answered the precedence question ADR 0025 declined to invent, so the same
//! grammar now carries both: `theme = dark` sits beside
//! `view.theme = ctrl+shift+d`. **A name with no dot is a setting**, because
//! every command id has one — which is checked by a test rather than assumed —
//! and a name that is neither is a problem printed at start-up, exactly as a bad
//! chord already is.

use std::path::PathBuf;

use sk_cmd::Registry;

use crate::app::App;
use crate::settings::{ALL, Marks, Setting, Values};

/// Where the file is, if the machine says where configuration goes.
///
/// `$XDG_CONFIG_HOME`, else `~/.config`, which is the specification's own
/// fallback and the same one [`sk_desktop::bookmarks`] follows for GTK's file.
#[must_use]
pub(crate) fn place() -> Option<PathBuf> {
    let home =
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|it| !it.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(home.join("skerry").join("config"))
}

/// What one file said: a command id, and the chords it should answer to.
///
/// **Order kept**, because the problems a bad line produces are reported in the
/// order the file has them and a person reads them against the file.
pub(crate) struct Config {
    keys: Vec<(String, Vec<String>)>,
    /// What the settings lines said, over the defaults.
    pub(crate) values: Values,
    /// Which settings the file named, which is the panel's third column.
    pub(crate) marks: Marks,
    /// The settings lines this build could not read, as the words they used.
    ///
    /// **Kept rather than reported here**, because the window prints these
    /// beside the registry's own refusals and both have to be in file order.
    pub(crate) refused: Vec<String>,
}

impl Config {
    /// Reads the file, or nothing where there is none.
    ///
    /// **A missing file is the common case and not an error.** No file, no lines,
    /// no problems: the defaults are the keymap, and the way to get one is the
    /// command that writes it.
    pub(crate) fn read() -> Self {
        let text = place().and_then(|path| std::fs::read_to_string(path).ok());
        Self::of(text.as_deref().unwrap_or_default())
    }

    /// Parses the text of one.
    pub(crate) fn of(text: &str) -> Self {
        let mut keys = Vec::new();
        let mut values = Values::default();
        let mut marks = Marks::default();
        let mut refused = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // **A `#` after whitespace ends the line.** The file this program
            // writes puts each command's display name after its chords — `#
            // Show hidden files` — and a parser that took that as a chord
            // refused every line of its own output, which is what the scenario
            // caught. A `#` with no space before it is a chord: `ctrl+#` is a key
            // somebody may have.
            let line = line.split(" #").next().unwrap_or(line).trim_end();
            let Some((id, chords)) = line.split_once('=') else {
                // A line with no `=` is a line somebody meant something by, and
                // the id is what a problem has to name — so it is kept and
                // refused by the rebind rather than dropped in silence.
                keys.push((line.to_owned(), Vec::new()));
                continue;
            };
            // **A setting is a name with no dot**, which every command id has —
            // `a_command_id_is_never_mistaken_for_a_setting` is what keeps that
            // true as commands are added.
            if let Some(setting) = Setting::named(id.trim()) {
                if values.take(setting, chords) {
                    marks.name(setting);
                } else {
                    refused.push(sk_model::i18n::format(
                        "error.setting_value",
                        &[&setting.name(), &chords.trim()],
                    ));
                }
                continue;
            }
            let chords = chords
                .split(',')
                .map(str::trim)
                .filter(|chord| !chord.is_empty())
                .map(str::to_owned)
                .collect();
            keys.push((id.trim().to_owned(), chords));
        }
        Self { keys, values, marks, refused }
    }

    /// How many lines named a command.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    /// Applies every line to the registry.
    ///
    /// Each line replaces that command's chords; a line with nothing after the
    /// `=` unbinds it. Whatever the registry refuses lands in its own problems,
    /// which the window prints at startup beside the ones the defaults produce.
    pub(crate) fn apply(&self, registry: &mut Registry<App>) {
        for (id, chords) in &self.keys {
            registry.rebind(id, chords);
        }
    }
}

/// The whole keymap as a file, commented.
///
/// **Every command, not only the ones somebody changed.** A file holding three
/// lines teaches a person three commands; a file holding all of them is the
/// answer to "what can I bind", which is the discovery half of rule 4 and the
/// reason this is written at all.
#[must_use]
pub(crate) fn text(registry: &Registry<App>, values: &Values) -> String {
    let mut out = String::with_capacity(8 * 1024);
    // Two lines and two keys, because the catalogue's values are verbatim: an
    // escape in one would be a literal backslash in the file.
    for key in ["config.header", "config.header_more"] {
        out.push_str("# ");
        out.push_str(sk_model::i18n::lookup(key));
        out.push('\n');
    }
    // **The settings first, at what the window is set to now.** The file is the
    // answer to "what can I change", and six lines a person can see are worth
    // more than six they have to be told about — the same argument that writes
    // every command rather than only the rebound ones.
    out.push('\n');
    out.push_str("# ");
    out.push_str(sk_model::i18n::lookup("settings.title"));
    out.push('\n');
    let widest = ALL.iter().map(|it| it.name().len()).max().unwrap_or(0);
    for setting in ALL {
        out.push_str(setting.name());
        for _ in setting.name().len()..widest {
            out.push(' ');
        }
        let word = values.word(setting);
        out.push_str(" = ");
        out.push_str(&word);
        for _ in word.len()..28 {
            out.push(' ');
        }
        out.push_str(" # ");
        out.push_str(sk_model::i18n::lookup(setting.label_key()));
        out.push('\n');
    }
    let mut group = None;
    let mut rows: Vec<_> = registry.iter().collect();
    // The widest id decides the column, so the names line up down the whole file
    // rather than per group — a table, which is what makes it readable cold.
    let widest = rows.iter().map(|row| row.id.len()).max().unwrap_or(0);
    // By group and then by id, so the file has the shape of the keyboard overlay
    // rather than of the order the registry happened to be built in.
    rows.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| a.id.cmp(b.id)));
    for row in rows {
        if group != Some(row.group) {
            group = Some(row.group);
            out.push('\n');
            out.push_str("# ");
            out.push_str(sk_model::i18n::lookup(row.group.label_key()));
            out.push('\n');
        }
        let Some(id) = registry.find(row.id) else { continue };
        let chords = registry.chords_of(id).join(", ");
        // The display name as a comment on the same line, because `file.trash`
        // does not say *Move to Trash* to anybody reading the file cold.
        // The display name after the chords, because `file.trash` does not say
        // *Move to Trash* to anybody reading the file cold. A `#` with a space
        // before it is where a line ends, which is what the parser reads back.
        out.push_str(row.id);
        for _ in row.id.len()..widest {
            out.push(' ');
        }
        out.push_str(" = ");
        out.push_str(&chords);
        // Padded so the names line up down the file, which is what makes it
        // readable as a table rather than as a list of assignments.
        for _ in chords.len()..28 {
            out.push(' ');
        }
        out.push_str(" # ");
        out.push_str(row.name);
        out.push('\n');
    }
    out
}

/// Writes the keymap out. The path, or why not.
///
/// **Refuses to overwrite**, which is ADR 0025's rule about who owns the file: a
/// person who has edited it gets told the file is there rather than losing what
/// they wrote. Deleting it is how they ask for a fresh one.
pub(crate) fn write(registry: &Registry<App>, values: &Values) -> Result<PathBuf, String> {
    let Some(path) = place() else {
        return Err(sk_model::i18n::lookup("error.no_config_home").to_owned());
    };
    if path.exists() {
        return Err(sk_model::i18n::format("error.config_exists", &[&path.display()]));
    }
    if !sk_fs::save::atomically(&path, &text(registry, values)) {
        return Err(sk_model::i18n::format("error.config_refused", &[&path.display()]));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::Config;

    /// Comments, blanks, several chords, and an empty right-hand side.
    #[test]
    fn a_file_is_lines_of_a_command_and_its_chords() {
        let config = Config::of(
            "# a comment\n\
             \n\
             go.back = alt+left, backspace\n\
             view.theme =\n\
             \x20  go.home   =   super+h  \n\
             # another\n",
        );
        assert_eq!(config.len(), 3);
        assert_eq!(
            config.keys[0],
            ("go.back".to_owned(), vec!["alt+left".to_owned(), "backspace".to_owned()])
        );
        // **An empty right-hand side is an unbinding**, which is the question a
        // rebindable keymap exists to answer: how do I get rid of this chord.
        assert_eq!(config.keys[1], ("view.theme".to_owned(), Vec::new()));
        // Whitespace is not part of an id or a chord.
        assert_eq!(config.keys[2], ("go.home".to_owned(), vec!["super+h".to_owned()]));
    }

    /// An empty file is no lines, which is what a machine with no config has.
    #[test]
    fn nothing_at_all_is_no_lines_and_no_problems() {
        assert_eq!(Config::of("").len(), 0);
        assert_eq!(Config::of("# only a comment\n\n\n").len(), 0);
    }

    /// A line with no `=` is kept, so the problem it produces can name it.
    #[test]
    fn a_line_that_is_not_a_binding_is_still_a_line() {
        let config = Config::of("go.back\n");
        assert_eq!(config.len(), 1);
        assert_eq!(config.keys[0].1, Vec::<String>::new());
    }

    /// **The rule the whole split rests on.** A setting is a name with no dot,
    /// and every command id has one — so a command can never be read as a
    /// setting, whatever gets added later.
    #[test]
    fn a_command_id_is_never_mistaken_for_a_setting() {
        let registry = crate::commands::registry();
        for row in registry.iter() {
            assert!(row.id.contains('.'), "{} has no dot and would parse as a setting", row.id);
            assert!(
                crate::settings::Setting::named(row.id).is_none(),
                "{} collides with a setting name",
                row.id
            );
        }
    }

    /// Settings and chords in one file, each going where it belongs.
    #[test]
    fn a_setting_and_a_chord_share_one_grammar() {
        let config = Config::of(
            "theme = light\n\
             go.back = alt+left\n\
             sidebar_width = 300\n\
             hidden = on   # with a trailing comment\n",
        );
        assert_eq!(config.len(), 1, "a setting was taken for a command");
        assert_eq!(config.keys[0].0, "go.back");
        assert!(!config.values.dark);
        assert!(config.values.hidden);
        assert!((config.values.aside - 300.0).abs() < f32::EPSILON);
        assert!(config.refused.is_empty(), "{:?}", config.refused);
        assert_eq!(
            config.marks.names(),
            Vec::<&str>::new(),
            "reading the file is not the window changing it"
        );
    }

    /// A value this build does not understand is a problem naming the line,
    /// not a setting silently left at its default.
    #[test]
    fn a_setting_with_a_value_we_cannot_read_is_a_problem() {
        let config = Config::of("theme = purple\n");
        assert_eq!(config.refused.len(), 1);
        assert!(config.refused[0].contains("purple"), "{:?}", config.refused);
        assert!(config.values.dark, "a refused line moved the value anyway");
    }

    /// The file this program writes has to be the file it reads.
    #[test]
    fn the_settings_it_writes_are_the_settings_it_reads_back() {
        let registry = crate::commands::registry();
        let wrote = crate::settings::Values {
            icons: crate::settings::Icons::Named("Adwaita".to_owned()),
            dark: false,
            hidden: true,
            sidebar: false,
            aside: 260.0,
            layout: crate::views::Layout::Grid,
            confirm_trash: true,
            decode_without_landlock: true,
        };
        let read = Config::of(&super::text(&registry, &wrote));
        assert_eq!(read.values, wrote);
        assert!(read.refused.is_empty(), "{:?}", read.refused);
        for setting in crate::settings::ALL {
            assert_eq!(
                read.marks.source(setting),
                crate::settings::Source::File,
                "{} was not named by the file it was written into",
                setting.name()
            );
        }
    }
}
