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
//! # What is not in it
//!
//! Settings. The theme, the sidebar, the default view: every one of them is
//! something the window also changes as it runs, and
//! [ADR 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md)'s
//! session file already remembers how it was left — so each needs a precedence
//! rule between the two, which is the successor ADR 0025's revisit trigger names.
//! A chord needs no such rule: nothing in the window rebinds a key while it runs.

use std::path::PathBuf;

use sk_cmd::Registry;

use crate::app::App;

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
            let chords = chords
                .split(',')
                .map(str::trim)
                .filter(|chord| !chord.is_empty())
                .map(str::to_owned)
                .collect();
            keys.push((id.trim().to_owned(), chords));
        }
        Self { keys }
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
pub(crate) fn text(registry: &Registry<App>) -> String {
    let mut out = String::with_capacity(8 * 1024);
    // Two lines and two keys, because the catalogue's values are verbatim: an
    // escape in one would be a literal backslash in the file.
    for key in ["config.header", "config.header_more"] {
        out.push_str("# ");
        out.push_str(sk_model::i18n::lookup(key));
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
pub(crate) fn write(registry: &Registry<App>) -> Result<PathBuf, String> {
    let Some(path) = place() else {
        return Err(sk_model::i18n::lookup("error.no_config_home").to_owned());
    };
    if path.exists() {
        return Err(sk_model::i18n::format("error.config_exists", &[&path.display()]));
    }
    if !sk_fs::save::atomically(&path, &text(registry)) {
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
}
