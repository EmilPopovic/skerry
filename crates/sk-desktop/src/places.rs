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

/// The bookmarks every other file manager on this machine reads and writes.
///
/// A line is a URI, optionally followed by a space and a label. A bookmark whose
/// directory is gone is dropped rather than shown broken — the file outlives the
/// directories in it, and this is a list of places you can go.
///
/// **Runs on a job.**
#[must_use]
pub fn bookmarks() -> Vec<Place> {
    let text = crate::config_dirs()
        .into_iter()
        .map(|dir| dir.join("gtk-3.0/bookmarks"))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let mut places = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let (uri, named) = line.split_once(' ').map_or((line, None), |(uri, rest)| {
            (uri, Some(rest.trim()).filter(|it| !it.is_empty()))
        });
        // Only local files. A `sftp://` bookmark is a GVfs mount, and reaching it
        // needs the thing rule 9 forbids.
        let Some(path) = uri.strip_prefix("file://").map(percent_decode).map(PathBuf::from) else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        let label = named.map_or_else(
            || {
                path.file_name().map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                )
            },
            str::to_owned,
        );
        places.push(Place { path, label, kind: Kind::Other });
    }
    places
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

    use super::{Kind, assignment, expand, percent_decode, user_dirs};
    use std::path::Path;

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
}
