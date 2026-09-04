// SPDX-License-Identifier: MIT OR Apache-2.0
//! `.desktop` files: the parts a launcher needs.
//!
//! The desktop-entry specification is a large document about a small format. What
//! is read here is the `[Desktop Entry]` group, the localized `Name`, `Exec`,
//! `Terminal`, `TryExec`, `NoDisplay` and `MimeType` — and nothing else, because
//! everything else in it is about menus, which are M7's.

use std::path::{Path, PathBuf};

/// One application, as its `.desktop` file describes it.
#[derive(Clone, Debug)]
pub struct DesktopEntry {
    /// The file's basename, which is the id `mimeapps.list` refers to.
    pub id: String,
    /// Where it was found.
    pub path: PathBuf,
    /// What to show a person.
    pub name: String,
    /// The command line, unexpanded.
    pub exec: String,
    /// Whether it wants a terminal to run in.
    pub terminal: bool,
    /// Types it says it opens.
    pub mime_types: Vec<String>,
    /// Whether it asked not to be shown in menus.
    pub no_display: bool,
    /// A binary that must exist for the entry to be usable.
    pub try_exec: Option<String>,
}

impl DesktopEntry {
    /// Parses a `.desktop` file.
    ///
    /// Returns nothing for a file that is not an `Application`, has no `Exec`, or
    /// is `Hidden` — all three mean "not something to launch", and treating them
    /// as launchable is how a file manager offers to open a PDF with a
    /// screen-reader configuration fragment.
    #[must_use]
    pub fn parse(path: &Path, text: &str) -> Option<Self> {
        let mut in_group = false;
        let mut fields: Vec<(&str, &str)> = Vec::new();

        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                // Only the first group counts. Everything after it is an action,
                // which has its own `Exec` and is not the default one.
                if in_group {
                    break;
                }
                in_group = line.eq_ignore_ascii_case("[desktop entry]");
                continue;
            }
            if !in_group || line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                fields.push((key.trim(), value.trim()));
            }
        }

        let find =
            |wanted: &str| fields.iter().find(|(key, _)| *key == wanted).map(|(_, value)| *value);
        if !matches!(find("Type"), Some("Application")) {
            return None;
        }
        if matches!(find("Hidden"), Some("true")) {
            return None;
        }
        let exec = find("Exec")?;
        if exec.is_empty() {
            return None;
        }

        let id = path.file_name()?.to_string_lossy().into_owned();
        Some(Self {
            // `Name=` with nothing after it is present and empty, which is not
            // a name — the notice read "Opening " with a space and a full stop.
            name: localized(&fields, "Name")
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(&id)
                .to_owned(),
            id,
            path: path.to_path_buf(),
            exec: exec.to_owned(),
            terminal: matches!(find("Terminal"), Some("true")),
            mime_types: find("MimeType")
                .unwrap_or("")
                .split(';')
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            no_display: matches!(find("NoDisplay"), Some("true")),
            try_exec: find("TryExec").map(str::to_owned),
        })
    }

    /// Reads and parses one file.
    #[must_use]
    pub fn open(path: &Path) -> Option<Self> {
        Self::parse(path, &crate::read(path)?)
    }

    /// Whether the binary it names exists.
    ///
    /// `TryExec` is the specification's own "is this actually installed" field,
    /// and entries left behind by an uninstalled package are common enough that
    /// checking it is the difference between an open that works and one that
    /// silently does nothing.
    #[must_use]
    pub fn usable(&self) -> bool {
        let Some(binary) = self.try_exec.as_deref().filter(|value| !value.is_empty()) else {
            return true;
        };
        crate::launch::find_on_path(binary).is_some()
    }
}

/// The value for the caller's locale, falling back to the unlocalized key.
///
/// `Name[de_DE]`, then `Name[de]`, then `Name`. Enough of the specification's
/// matching rules to be right for the case that occurs; the modifier and encoding
/// parts of a locale name are ignored, and no locale on any machine here uses
/// them.
fn localized<'a>(fields: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    let locale =
        std::env::var("LC_MESSAGES").or_else(|_| std::env::var("LANG")).unwrap_or_default();
    let locale = locale.split('.').next().unwrap_or("").to_owned();
    let language = locale.split('_').next().unwrap_or("").to_owned();

    for candidate in [format!("{key}[{locale}]"), format!("{key}[{language}]")] {
        if candidate.ends_with("[]") {
            continue;
        }
        if let Some((_, value)) = fields.iter().find(|(name, _)| *name == candidate) {
            return Some(value);
        }
    }
    fields.iter().find(|(name, _)| *name == key).map(|(_, value)| *value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::DesktopEntry;
    use std::path::Path;

    const FIREFOX: &str = "\
[Desktop Entry]
Version=1.0
Name=Firefox
Name[de]=Feuerfuchs
GenericName=Web Browser
Exec=firefox %u
Terminal=false
Type=Application
MimeType=text/html;application/xhtml+xml;x-scheme-handler/http;
Categories=Network;WebBrowser;

[Desktop Action new-window]
Name=New Window
Exec=firefox --new-window
";

    #[test]
    fn the_fields_a_launcher_needs_are_read() {
        let entry = DesktopEntry::parse(Path::new("/x/firefox.desktop"), FIREFOX).expect("parsed");
        assert_eq!(entry.id, "firefox.desktop");
        assert_eq!(entry.exec, "firefox %u");
        assert!(!entry.terminal);
        assert_eq!(entry.mime_types.len(), 3);
        assert!(entry.mime_types.contains(&"text/html".to_owned()));
    }

    /// A desktop file's actions have their own `Exec`, and taking one of them as
    /// the entry's command opens every HTML file in a new window forever.
    #[test]
    fn an_action_group_is_not_the_entry() {
        let entry = DesktopEntry::parse(Path::new("/x/firefox.desktop"), FIREFOX).expect("parsed");
        assert_eq!(entry.exec, "firefox %u");
        assert_ne!(entry.name, "New Window");
    }

    #[test]
    fn something_that_is_not_an_application_is_not_an_entry() {
        for text in [
            "[Desktop Entry]\nType=Link\nName=x\nURL=http://x\n",
            "[Desktop Entry]\nType=Application\nName=x\n",
            "[Desktop Entry]\nType=Application\nName=x\nExec=\n",
            "[Desktop Entry]\nType=Application\nName=x\nExec=x\nHidden=true\n",
            "not a desktop file at all",
        ] {
            assert!(DesktopEntry::parse(Path::new("/x/a.desktop"), text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn a_missing_try_exec_binary_makes_the_entry_unusable() {
        let text = "[Desktop Entry]\nType=Application\nName=x\nExec=x %f\nTryExec=/nope/nope\n";
        let entry = DesktopEntry::parse(Path::new("/x/a.desktop"), text).expect("parsed");
        assert!(!entry.usable());

        let text = "[Desktop Entry]\nType=Application\nName=x\nExec=x %f\n";
        let entry = DesktopEntry::parse(Path::new("/x/a.desktop"), text).expect("parsed");
        assert!(entry.usable(), "no TryExec means nothing to check");
    }

    #[test]
    fn the_name_falls_back_when_there_is_no_translation_for_the_locale() {
        // Whatever the machine's locale is, one of these two is the answer and
        // neither is empty.
        let entry = DesktopEntry::parse(Path::new("/x/a.desktop"), FIREFOX).expect("parsed");
        assert!(entry.name == "Firefox" || entry.name == "Feuerfuchs", "{}", entry.name);
    }
}
