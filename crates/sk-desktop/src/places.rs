// SPDX-License-Identifier: MIT OR Apache-2.0
//! The directories a person keeps things in, and the ones they have bookmarked.
//!
//! Two files, both plain text, both written by other programs and read by every
//! file manager on the machine:
//!
//! * **`user-dirs.dirs`**, from the XDG user-directories specification. Written
//!   by `xdg-user-dirs-update`, and the reason Downloads is called
//!   `Descargas` on a Spanish desktop and finding it by that name is not our
//!   business.
//! * **`bookmarks`**, GTK's. Not a specification and not GTK the library — a
//!   list of URIs, one per line, which Nautilus, Thunar, Nemo, Caja and `PCManFM`
//!   all read and write. Interoperating through the file is the whole of
//!   [rule 9](../../../docs/03-principles.md): the format is the contract, and
//!   nothing here links against anything of GTK's.
//!
//! **This one is written as well as read**, which is what pinning a folder is —
//! [ADR 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md).
//! It is somebody else's file, so every line this program cannot interpret comes
//! back out of it unchanged and in the same position, and the write goes through
//! [`sk_fs::atomically`] so a crash leaves the old list whole.

use std::path::{Path, PathBuf};

/// One entry in the sidebar's list of places.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// Where it goes.
    pub path: PathBuf,
    /// What to call it. From the file where the file names it, and from the
    /// last path component where it does not.
    pub label: String,
    /// Which of the well-known user directories this is, if any.
    pub kind: Kind,
}

/// Which well-known directory a place is.
///
/// Named rather than inferred from the path, because the path is localised and
/// the meaning is not: a sidebar wants Downloads above Templates whatever the
/// directory is called.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(missing_docs, reason = "each variant is the directory of that name")]
pub enum Kind {
    Home,
    Desktop,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    /// A bookmark, or anything else with no well-known meaning.
    Other,
}

/// The keys `user-dirs.dirs` uses, in the order a sidebar shows them.
///
/// Downloads before Documents, because that is the one people open twenty times
/// a day. Templates and Public are deliberately absent: they exist on a fresh
/// account, nobody puts anything in them, and a sidebar is a list of the places
/// you go.
const WANTED: &[(&str, Kind)] = &[
    ("XDG_DESKTOP_DIR", Kind::Desktop),
    ("XDG_DOWNLOAD_DIR", Kind::Downloads),
    ("XDG_DOCUMENTS_DIR", Kind::Documents),
    ("XDG_MUSIC_DIR", Kind::Music),
    ("XDG_PICTURES_DIR", Kind::Pictures),
    ("XDG_VIDEOS_DIR", Kind::Videos),
];

/// Home, then the well-known directories that exist.
///
/// **Only the ones that exist.** A fresh account has every key in the file and
/// half the directories missing, and a sidebar entry that reports "no such
/// directory" when clicked is worse than no entry.
///
/// **Runs on a job**: it reads a file and stats each directory.
#[must_use]
pub fn user_dirs() -> Vec<Place> {
    let Some(home) = home() else { return Vec::new() };
    // Home's label is empty on purpose: it is the one entry whose name is not
    // the directory's — that is the user's login name — and the word for it
    // belongs to whoever has the string catalogue, which is not this crate.
    let mut places = vec![Place { label: String::new(), path: home.clone(), kind: Kind::Home }];
    let text = crate::config_dirs()
        .into_iter()
        .map(|dir| dir.join("user-dirs.dirs"))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    for (key, kind) in WANTED {
        // The specification's own fallback where the file says nothing: the
        // English name under `$HOME`.
        let path = assignment(&text, key)
            .map_or_else(|| home.join(default_name(*kind)), |value| expand(&value, &home));
        if !path.is_dir() || path == home {
            continue;
        }
        places.push(Place {
            label: path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path,
            kind: *kind,
        });
    }
    places
}

/// Where the list lives. `gtk-3.0` and not `gtk-4.0`: GTK 4 kept the path.
const BOOKMARKS: &str = "gtk-3.0/bookmarks";

/// The bookmarks every other file manager on this machine reads and writes.
///
/// A line is a URI, optionally followed by a space and a label. A bookmark whose
/// directory is gone is dropped rather than shown broken — the file outlives the
/// directories in it, and this is a list of places you can go. The line stays in
/// the file: a removable disk is unplugged far more often than a bookmark is
/// meant to be deleted.
///
/// **Runs on a job.**
#[must_use]
pub fn bookmarks() -> Vec<Place> {
    lines()
        .iter()
        .filter_map(|line| {
            let (path, named) = interpret(line)?;
            path.is_dir().then(|| Place { label: name_for(&path, named), path, kind: Kind::Other })
        })
        .collect()
}

/// Adds `path` to the list. Whether the list now holds it.
///
/// Idempotent: a directory already on the list is a request that has already
/// been granted, and reporting that as a failure would make a second press look
/// broken.
///
/// **Runs on a job.**
#[must_use]
pub fn pin(path: &Path) -> bool {
    added(&lines(), path).is_none_or(|held| write(&held))
}

/// Removes `path` from the list. Whether the list now lacks it.
///
/// **Runs on a job.**
#[must_use]
pub fn unpin(path: &Path) -> bool {
    removed(&lines(), path).is_none_or(|held| write(&held))
}

/// Puts the lines naming `order`'s paths into `order`'s sequence.
///
/// **Runs on a job.**
#[must_use]
pub fn reorder(order: &[PathBuf]) -> bool {
    permuted(&lines(), order).is_some_and(|held| write(&held))
}

/// The lines with one for `path` at the end, or nothing if one is there already.
fn added(held: &[String], path: &Path) -> Option<Vec<String>> {
    if held.iter().any(|line| names(line, path)) {
        return None;
    }
    let mut out = held.to_vec();
    out.push(format!("file://{}", percent_encode(path)));
    Some(out)
}

/// The lines without the ones naming `path`, or nothing if none did.
fn removed(held: &[String], path: &Path) -> Option<Vec<String>> {
    let kept: Vec<String> = held.iter().filter(|line| !names(line, path)).cloned().collect();
    (kept.len() != held.len()).then_some(kept)
}

/// The lines with the ones naming `order`'s paths put into `order`'s sequence.
///
/// **Every other line keeps the position it had**, which is the whole difficulty:
/// a `sftp://` bookmark between two local ones is a line this program cannot show
/// and must not move or lose, so this is a permutation of the lines it recognises
/// into the slots those lines already occupied. A line's own text travels with
/// it, so a label somebody set survives being dragged.
///
/// Nothing where `order` does not describe the file — a path the file does not
/// hold, or one path twice. A list assembled from a request that does not match
/// is how a reorder loses a row.
fn permuted(held: &[String], order: &[PathBuf]) -> Option<Vec<String>> {
    let slots: Vec<usize> = held
        .iter()
        .enumerate()
        .filter(|(_, line)| order.iter().any(|path| names(line, path)))
        .map(|(at, _)| at)
        .collect();
    if slots.len() != order.len() {
        return None;
    }
    let mut moved: Vec<String> = Vec::with_capacity(order.len());
    for path in order {
        let found = slots.iter().map(|at| &held[*at]).find(|line| names(line, path))?;
        moved.push(found.clone());
    }
    let mut out = held.to_vec();
    for (at, line) in slots.into_iter().zip(moved) {
        out[at] = line;
    }
    Some(out)
}

/// Whether a line is the one for `path`.
fn names(line: &str, path: &Path) -> bool {
    interpret(line).is_some_and(|(it, _)| it == path)
}

/// The file's lines, from the first place that has one.
fn lines() -> Vec<String> {
    crate::config_dirs().into_iter().find_map(|dir| read_in(&dir)).unwrap_or_default()
}

/// The lines of the bookmarks file in one directory, if it has one.
///
/// Split on `\n` rather than by [`str::lines`], which strips a `\r` — a line
/// this program does not interpret has to come back out of the file as it went
/// in, and that includes a line ending somebody else's editor left.
fn read_in(dir: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(dir.join(BOOKMARKS)).ok()?;
    let mut held: Vec<String> = text.split('\n').map(str::to_owned).collect();
    // A file ending in a newline splits into a last empty element that is not a
    // line. One that does not, ends with a line — and gains a newline when it is
    // written back, which is the one normalisation this makes.
    if held.last().is_some_and(String::is_empty) {
        held.pop();
    }
    Some(held)
}

/// Writes the lines to *this user's* config directory.
///
/// Read from the first directory that has a copy and written to the user's own,
/// so a system-wide list is inherited on the first pin rather than shadowed by a
/// file holding one line.
fn write(held: &[String]) -> bool {
    crate::config_dirs().first().is_some_and(|home| write_in(home, held))
}

/// Writes the lines into one directory.
fn write_in(dir: &Path, held: &[String]) -> bool {
    let text = if held.is_empty() { String::new() } else { format!("{}\n", held.join("\n")) };
    sk_fs::atomically(&dir.join(BOOKMARKS), &text)
}

/// The local directory a line names and the label it carries, if it names one.
///
/// Only `file://`. A `sftp://` bookmark is a `GVfs` mount, and reaching it needs
/// the thing rule 9 forbids — so it is a line to preserve rather than a place to
/// show.
fn interpret(line: &str) -> Option<(PathBuf, Option<&str>)> {
    let line = line.trim();
    let (uri, named) = line
        .split_once(' ')
        .map_or((line, None), |(uri, rest)| (uri, Some(rest.trim()).filter(|it| !it.is_empty())));
    let path = PathBuf::from(percent_decode(uri.strip_prefix("file://")?));
    // A relative path is not something `file://` can express, and joining one to
    // the process's working directory would make the same file mean two places.
    path.is_absolute().then_some((path, named))
}

/// What to call a place: the label the file gave, else the directory's own name.
fn name_for(path: &Path, named: Option<&str>) -> String {
    named.map_or_else(
        || {
            path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            )
        },
        str::to_owned,
    )
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|path| !path.as_os_str().is_empty())
}

/// The English directory name the specification falls back to.
const fn default_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Desktop => "Desktop",
        Kind::Downloads => "Downloads",
        Kind::Documents => "Documents",
        Kind::Music => "Music",
        Kind::Pictures => "Pictures",
        Kind::Videos => "Videos",
        Kind::Home | Kind::Other => "",
    }
}

/// The value of `KEY="value"` in `user-dirs.dirs`.
///
/// Not a shell parser: the file's own header says the format is
/// `XDG_xxx_DIR="$HOME/yyy"` or an absolute path and that no other format is
/// supported, so the quotes and the one variable are the whole grammar.
fn assignment(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim() == key)
        .map(|(_, value)| value.trim().trim_matches('"').to_owned())
}

/// `$HOME/Downloads` and `${HOME}/Downloads` to a path.
fn expand(value: &str, home: &Path) -> PathBuf {
    for prefix in ["$HOME/", "${HOME}/"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return home.join(rest);
        }
    }
    if value == "$HOME" || value == "${HOME}" {
        return home.to_path_buf();
    }
    PathBuf::from(value)
}

/// A path as a `file://` URI, which is how every desktop protocol names one.
///
/// Public because the bus speaks in URIs — `FileManager1` takes an array of them
/// — and a second encoder would be a second set of escaping rules.
#[must_use]
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", percent_encode(path))
}

/// A path as the file's URI syntax wants it.
///
/// Everything outside RFC 3986's unreserved set is escaped, with `/` left alone
/// because it is the path's own structure. That is more than `g_filename_to_uri`
/// escapes — it leaves the sub-delimiters literal — and a URI that escapes more
/// than it must still decodes to the same path, which is the property that
/// matters when the reader is somebody else's program.
fn percent_encode(path: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(*byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// `%20` and friends, because a bookmark is a URI.
fn percent_decode(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&uri[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{
        Kind, added, assignment, expand, percent_decode, percent_encode, permuted, read_in,
        removed, user_dirs, write_in,
    };
    use std::path::{Path, PathBuf};

    const FILE: &str = "# This file is written by xdg-user-dirs-update\n\
                        # Format is XDG_xxx_DIR=\"$HOME/yyy\"\n\
                        XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\n\
                        XDG_DOWNLOAD_DIR=\"$HOME/Descargas\"\n\
                        XDG_MUSIC_DIR=\"/mnt/media/music\"\n\
                        #XDG_VIDEOS_DIR=\"$HOME/Commented\"\n";

    /// The directory is localised and the meaning is not: `Descargas` is
    /// Downloads, and finding it by the English name is not our business.
    #[test]
    fn a_localised_directory_is_read_from_the_file_and_not_guessed() {
        assert_eq!(assignment(FILE, "XDG_DOWNLOAD_DIR").as_deref(), Some("$HOME/Descargas"));
        assert_eq!(assignment(FILE, "XDG_MUSIC_DIR").as_deref(), Some("/mnt/media/music"));
        assert_eq!(assignment(FILE, "XDG_VIDEOS_DIR"), None, "a commented line is not a value");
        assert_eq!(assignment(FILE, "XDG_PICTURES_DIR"), None);
    }

    #[test]
    fn the_one_variable_the_format_has_is_expanded() {
        let home = Path::new("/home/emil");
        assert_eq!(expand("$HOME/Descargas", home), Path::new("/home/emil/Descargas"));
        assert_eq!(expand("${HOME}/Descargas", home), Path::new("/home/emil/Descargas"));
        assert_eq!(expand("$HOME", home), home);
        assert_eq!(expand("/mnt/media", home), Path::new("/mnt/media"));
    }

    #[test]
    fn a_bookmark_is_a_uri_and_arrives_percent_encoded() {
        assert_eq!(percent_decode("/home/emil/My%20Files"), "/home/emil/My Files");
        assert_eq!(percent_decode("/plain"), "/plain");
        assert_eq!(percent_decode("/trailing%"), "/trailing%");
        assert_eq!(percent_decode("/caf%C3%A9"), "/café");
    }

    /// Home is always first and always there; the rest are whatever this
    /// machine actually has, and every one of them is a directory.
    #[test]
    fn every_place_is_a_directory_that_exists_and_home_leads() {
        let places = user_dirs();
        assert!(places.first().is_some_and(|place| place.kind == Kind::Home));
        for place in &places {
            assert!(place.path.is_dir(), "{place:?}");
            // Every label but Home's, which the caller names.
            assert!(place.kind == Kind::Home || !place.label.is_empty(), "{place:?}");
        }
        let mut paths: Vec<_> = places.iter().map(|place| &place.path).collect();
        let count = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), count, "the same directory twice: {places:?}");
    }

    /// Every form a real machine's file holds, in one fixture.
    ///
    /// Three of these five lines are ones this program cannot show: a `GVfs` mount
    /// it must not open, a `trash://` URI, and a blank line. [ADR
    /// 0021](../../../docs/adr/0021-a-pin-is-a-line-in-gtks-bookmarks-file.md)
    /// promises all three come back out unchanged and in the same position, and
    /// this is what makes that a promise rather than an intention.
    const WILD: &[&str] = &[
        "file:///home/emil/repos",
        "sftp://box.example/srv",
        "",
        "file:///home/emil/My%20Files Work",
        "trash:///",
    ];

    fn wild() -> Vec<String> {
        WILD.iter().map(|it| (*it).to_owned()).collect()
    }

    /// A pin appends one line and touches nothing else.
    #[test]
    fn pinning_adds_a_line_and_leaves_every_other_line_alone() {
        let held = added(&wild(), Path::new("/mnt/photos")).expect("nothing was added");
        assert_eq!(held[..WILD.len()], wild()[..], "an existing line moved or changed");
        assert_eq!(held.last().map(String::as_str), Some("file:///mnt/photos"));
    }

    /// A directory already on the list is a request already granted.
    #[test]
    fn pinning_what_is_already_pinned_writes_nothing() {
        assert!(added(&wild(), Path::new("/home/emil/repos")).is_none());
        // Including the one whose line carries a label, which is the same line
        // and must not be duplicated because the text differs.
        assert!(added(&wild(), Path::new("/home/emil/My Files")).is_none());
    }

    /// An unpin removes its line and nothing near it.
    #[test]
    fn unpinning_removes_one_line_and_keeps_the_ones_it_cannot_read() {
        let held = removed(&wild(), Path::new("/home/emil/My Files")).expect("nothing removed");
        assert_eq!(held, ["file:///home/emil/repos", "sftp://box.example/srv", "", "trash:///"]);
        assert!(removed(&held, Path::new("/nowhere")).is_none(), "a path not on the list");
    }

    /// A reorder is a permutation of the lines it recognises into the slots they
    /// already occupied — so the `sftp://` line between them stays between them,
    /// and the label travels with the row that carries it.
    #[test]
    fn a_reorder_moves_the_rows_it_knows_and_nothing_else() {
        let order = vec![PathBuf::from("/home/emil/My Files"), PathBuf::from("/home/emil/repos")];
        let held = permuted(&wild(), &order).expect("the request did not describe the file");
        assert_eq!(
            held,
            [
                "file:///home/emil/My%20Files Work",
                "sftp://box.example/srv",
                "",
                "file:///home/emil/repos",
                "trash:///",
            ]
        );
    }

    /// A request that cannot be a permutation is refused rather than applied.
    ///
    /// The list on screen and the file on disk are two things: another program
    /// can edit the file while a drag is in flight, and a bookmark whose
    /// directory has gone is on the list and not on the screen. So the rule is
    /// narrow — the named lines are permuted among their own slots, and a
    /// request naming one of them twice, or naming a line that is not there,
    /// would write one line where two were. That is how a reorder loses the row
    /// nobody dragged.
    #[test]
    fn a_reorder_that_cannot_be_a_permutation_is_refused() {
        let twice = vec![PathBuf::from("/home/emil/repos"), PathBuf::from("/home/emil/repos")];
        assert!(permuted(&wild(), &twice).is_none(), "the same row twice was accepted");
        let absent = vec![PathBuf::from("/home/emil/repos"), PathBuf::from("/nowhere")];
        assert!(permuted(&wild(), &absent).is_none(), "a path not in the file was accepted");
        // A request naming fewer rows than the file holds is not stale, it is a
        // permutation of those rows — which is a permutation of one row, and
        // changes nothing.
        let one = vec![PathBuf::from("/home/emil/repos")];
        assert_eq!(permuted(&wild(), &one).expect("a subset was refused"), wild());
    }

    /// The whole file survives a trip through the writer and the reader.
    #[test]
    fn a_file_written_and_read_again_is_the_same_file() {
        let dir = std::env::temp_dir().join(format!("skerry-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_in(&dir).is_none(), "a directory with no file reported lines");
        assert!(write_in(&dir, &wild()));
        assert_eq!(read_in(&dir).expect("nothing was read"), wild());
        // A file with no trailing newline is a file a person's editor wrote, and
        // its last line is a line.
        std::fs::write(dir.join(super::BOOKMARKS), b"file:///one\nfile:///two").unwrap();
        assert_eq!(read_in(&dir).expect("nothing was read"), ["file:///one", "file:///two"]);
        // And an empty list is an empty file rather than a blank line.
        assert!(write_in(&dir, &[]));
        assert_eq!(std::fs::read_to_string(dir.join(super::BOOKMARKS)).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What we write is what our own reader takes back, for the names that make
    /// a URI awkward.
    #[test]
    fn a_name_that_needs_escaping_survives_the_round_trip() {
        for name in ["/a b", "/caf\u{e9}", "/100%", "/a#b?c", "/plus+one", "/\u{1f4c1}"] {
            let encoded = percent_encode(Path::new(name));
            assert!(!encoded.contains(' '), "{encoded} has a space in it");
            assert_eq!(percent_decode(&encoded), name, "{encoded}");
        }
        assert_eq!(percent_encode(Path::new("/plain/path")), "/plain/path");
    }
}
