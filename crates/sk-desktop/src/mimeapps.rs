// SPDX-License-Identifier: Apache-2.0
//! Which application the *user* has chosen for a type, and how to change it.
//!
//! [`Applications`](crate::Applications) reads every association database on the
//! machine and answers "who can open this". This module is about one file —
//! `$XDG_CONFIG_HOME/mimeapps.list` — and one question: is this application the
//! default for this type, and make it so or stop being so.
//!
//! **[ADR 0027](../../../docs/adr/0027-a-setting-is-a-line-in-the-config-and-the-session-remembers-what-the-window-changed.md)**
//! puts that behind a row in the settings panel rather than a command in the
//! palette, and requires the shape [`crate::places`] already has for GTK's
//! `bookmarks`: this is a file other programs write, so what we did not write is
//! preserved byte for byte, and a file whose syntax we do not understand is left
//! alone rather than rewritten. The failure mode of getting that wrong is
//! somebody's default browser.
//!
//! The grammar is the desktop-entry specification's own key-value form: `#`
//! comments, `[Section]` headers, and `key=value` under them. Nothing else is
//! legal, and anything else is why [`Refused::Unreadable`] exists.

use std::path::PathBuf;

/// The section a default association lives under.
const DEFAULTS: &str = "Default Applications";

/// Why the file was not written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// There is no `$XDG_CONFIG_HOME` and no `$HOME`, so there is nowhere to put
    /// it.
    NoConfigHome,
    /// A line is neither a comment, a blank, a section header nor `key=value`.
    ///
    /// **Refusing is the whole point.** Rewriting a file we cannot re-emit
    /// faithfully loses whatever that line meant, and the line belongs to
    /// somebody else's program.
    Unreadable(usize),
    /// The write itself failed — a full disk, a read-only home.
    NotWritten,
}

/// Where the user's own associations live.
///
/// The **first** config directory only. `$XDG_CONFIG_DIRS` entries are the
/// distribution's and are not ours to edit.
#[must_use]
pub fn place() -> Option<PathBuf> {
    crate::config_dirs().into_iter().next().map(|dir| dir.join("mimeapps.list"))
}

/// Whether `id` is the user's default for `mime`.
///
/// **The user's file alone**, which is the file this module can change. An
/// application that is the default because the *system* file says so reads as
/// `false` here, and setting it writes a line that says the same thing — which
/// is correct, because the system file can change under us and the user's cannot.
#[must_use]
pub fn is_default(mime: &str, id: &str) -> bool {
    let Some(path) = place() else { return false };
    let Some(text) = crate::read(&path) else { return false };
    match parse(&text) {
        Ok(lines) => first_default(&lines, mime).is_some_and(|it| it == id),
        Err(_) => false,
    }
}

/// Makes `id` the user's default for `mime`. The file, or why not.
///
/// **Prepended, not replaced.** The specification reads the list left to right
/// and takes the first entry that is installed, so putting ours first states the
/// preference without discarding what the person had before it — which is what
/// [`clear_default`] restores them to.
pub fn set_default(mime: &str, id: &str) -> Result<PathBuf, Refused> {
    edit(mime, |ids| {
        let mut out = vec![id.to_owned()];
        out.extend(ids.iter().filter(|it| *it != id).cloned());
        out
    })
}

/// Stops `id` being the user's default for `mime`. The file, or why not.
pub fn clear_default(mime: &str, id: &str) -> Result<PathBuf, Refused> {
    edit(mime, |ids| ids.iter().filter(|it| *it != id).cloned().collect())
}

/// Reads the file, applies `change` to the ids under `mime`, and writes it back.
fn edit(mime: &str, change: impl Fn(&[String]) -> Vec<String>) -> Result<PathBuf, Refused> {
    let path = place().ok_or(Refused::NoConfigHome)?;
    let text = crate::read(&path).unwrap_or_default();
    let lines = parse(&text)?;
    let ids = change(&entry(&lines, mime));
    let out = render(&rewritten(lines, mime, &ids));
    if sk_fs::atomically(&path, &out) { Ok(path) } else { Err(Refused::NotWritten) }
}

/// One line of the file, kept in the shape that lets it be re-emitted verbatim.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Line {
    /// A comment or a blank, kept exactly as it arrived.
    Kept(String),
    /// `[Default Applications]`, with the name as written.
    Section(String),
    /// `key=value` under whichever section was last opened.
    Pair { section: String, key: String, value: String },
}

/// The file as lines, or the number of the first one that is not this grammar.
fn parse(text: &str) -> Result<Vec<Line>, Refused> {
    let mut section = String::new();
    let mut out = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push(Line::Kept(line.to_owned()));
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|it| it.strip_suffix(']')) {
            name.clone_into(&mut section);
            out.push(Line::Section(name.to_owned()));
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(Refused::Unreadable(number + 1));
        };
        out.push(Line::Pair {
            section: section.clone(),
            key: key.trim().to_owned(),
            value: value.trim().to_owned(),
        });
    }
    Ok(out)
}

/// The ids listed for `mime` under `[Default Applications]`.
fn entry(lines: &[Line], mime: &str) -> Vec<String> {
    lines
        .iter()
        .find_map(|line| match line {
            Line::Pair { section, key, value }
                if section.eq_ignore_ascii_case(DEFAULTS) && key == mime =>
            {
                Some(split(value))
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The first id listed for `mime`, which is the one the specification picks.
fn first_default(lines: &[Line], mime: &str) -> Option<String> {
    entry(lines, mime).into_iter().next()
}

fn split(value: &str) -> Vec<String> {
    value.split(';').map(str::trim).filter(|it| !it.is_empty()).map(str::to_owned).collect()
}

/// The lines with `mime`'s defaults set to `ids`.
///
/// **Three cases, and the order of the checks is the whole function.** The line
/// exists — replace its value in place, so it keeps its position among a person's
/// own ordering. The section exists and the line does not — put the line at the
/// end of that section, not at the end of the file, or it lands under whichever
/// section happens to be last. Neither exists — a new section at the end.
///
/// An empty `ids` removes the line rather than writing `inode/directory=`, which
/// some readers take as "the default is the empty string".
fn rewritten(lines: Vec<Line>, mime: &str, ids: &[String]) -> Vec<Line> {
    let value = ids.join(";");
    let ours = |line: &Line| {
        matches!(line, Line::Pair { section, key, .. }
            if section.eq_ignore_ascii_case(DEFAULTS) && key == mime)
    };
    if lines.iter().any(&ours) {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            if !ours(&line) {
                out.push(line);
            } else if let (false, Line::Pair { section, key, .. }) = (ids.is_empty(), line) {
                out.push(Line::Pair { section, key, value: value.clone() });
            }
        }
        return out;
    }
    if ids.is_empty() {
        return lines;
    }
    let pair =
        Line::Pair { section: DEFAULTS.to_owned(), key: mime.to_owned(), value: value.clone() };
    let last = lines.iter().rposition(|line| {
        matches!(line, Line::Section(name) | Line::Pair { section: name, .. }
            if name.eq_ignore_ascii_case(DEFAULTS))
    });
    let mut out = lines;
    if let Some(at) = last {
        out.insert(at + 1, pair);
        return out;
    }
    if out.last().is_some_and(|line| line != &Line::Kept(String::new())) {
        out.push(Line::Kept(String::new()));
    }
    out.push(Line::Section(DEFAULTS.to_owned()));
    out.push(pair);
    out
}

/// The lines back as a file, with a trailing newline.
fn render(lines: &[Line]) -> String {
    let mut out = String::with_capacity(256);
    for line in lines {
        match line {
            Line::Kept(text) => out.push_str(text),
            Line::Section(name) => {
                out.push('[');
                out.push_str(name);
                out.push(']');
            }
            Line::Pair { key, value, .. } => {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{DEFAULTS, Refused, entry, first_default, parse, render, rewritten};

    fn round(text: &str, mime: &str, ids: &[&str]) -> String {
        let lines = parse(text).expect("a file this grammar reads");
        let ids: Vec<String> = ids.iter().map(|it| (*it).to_owned()).collect();
        render(&rewritten(lines, mime, &ids))
    }

    /// The property that matters most: everything we did not write comes back.
    #[test]
    fn every_line_we_did_not_write_survives_a_rewrite() {
        let before = "# somebody's own note\n\
                      [Added Associations]\n\
                      text/html=firefox.desktop;\n\
                      \n\
                      [Default Applications]\n\
                      text/html=firefox.desktop\n\
                      application/pdf=okular.desktop\n";
        let after = round(before, "inode/directory", &["dev.skerry.Skerry.desktop"]);
        for line in before.lines() {
            assert!(after.contains(line), "the rewrite lost {line:?}");
        }
        assert!(after.contains("inode/directory=dev.skerry.Skerry.desktop"));
    }

    /// A line that already exists keeps its position, so a person's own ordering
    /// of the section is not reshuffled by a change to one entry.
    #[test]
    fn an_existing_line_is_replaced_where_it_stands() {
        let before = "[Default Applications]\n\
                      a/one=first.desktop\n\
                      inode/directory=nautilus.desktop\n\
                      z/two=last.desktop\n";
        let after = round(before, "inode/directory", &["dev.skerry.Skerry.desktop", "x.desktop"]);
        assert_eq!(
            after,
            "[Default Applications]\n\
             a/one=first.desktop\n\
             inode/directory=dev.skerry.Skerry.desktop;x.desktop\n\
             z/two=last.desktop\n"
        );
    }

    /// The section exists and the line does not: the line goes at the end of
    /// *that* section, not at the end of the file.
    #[test]
    fn a_new_line_lands_inside_its_own_section() {
        let before = "[Default Applications]\n\
                      a/one=first.desktop\n\
                      \n\
                      [Added Associations]\n\
                      b/two=other.desktop\n";
        let after = round(before, "inode/directory", &["skerry.desktop"]);
        let lines: Vec<&str> = after.lines().collect();
        let mine = lines.iter().position(|it| it.starts_with("inode/directory")).expect("written");
        let added = lines.iter().position(|it| *it == "[Added Associations]").expect("kept");
        assert!(mine < added, "the new line landed under the wrong section:\n{after}");
    }

    /// A file with no such section at all, and an empty file, both get one.
    #[test]
    fn a_missing_section_is_added_at_the_end() {
        let after = round(
            "[Added Associations]\nb/two=other.desktop\n",
            "inode/directory",
            &["skerry.desktop"],
        );
        assert!(after.ends_with("[Default Applications]\ninode/directory=skerry.desktop\n"));
        let fresh = round("", "inode/directory", &["skerry.desktop"]);
        assert_eq!(fresh, "[Default Applications]\ninode/directory=skerry.desktop\n");
    }

    /// Clearing the last id removes the line rather than writing an empty value,
    /// which some readers take as "the default is the empty string".
    #[test]
    fn clearing_the_last_id_removes_the_line() {
        let after = round(
            "[Default Applications]\ninode/directory=skerry.desktop\nkeep/this=other.desktop\n",
            "inode/directory",
            &[],
        );
        assert_eq!(after, "[Default Applications]\nkeep/this=other.desktop\n");
    }

    /// A line this grammar cannot re-emit stops the write, with its number.
    #[test]
    fn a_line_we_cannot_reemit_refuses_the_whole_file() {
        assert_eq!(
            parse("[Default Applications]\nthis is not a pair\n"),
            Err(Refused::Unreadable(2))
        );
    }

    /// Section names are case-insensitive in the specification, and a file
    /// written by another program may not match our capitalisation.
    #[test]
    fn a_section_is_found_whatever_its_case() {
        let lines = parse("[default applications]\ninode/directory=one.desktop;two.desktop\n")
            .expect("read");
        assert_eq!(first_default(&lines, "inode/directory"), Some("one.desktop".to_owned()));
        assert_eq!(entry(&lines, "inode/directory").len(), 2);
        let after = round(
            "[default applications]\ninode/directory=one.desktop\n",
            "inode/directory",
            &["skerry.desktop"],
        );
        assert_eq!(after.lines().filter(|it| it.starts_with('[')).count(), 1, "a second section");
        assert_eq!(DEFAULTS, "Default Applications");
    }
}
