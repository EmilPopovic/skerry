//! Where the fonts are, without fontconfig.
//!
//! fontconfig is a C library with an XML configuration language, a cache format
//! and a main loop's worth of global state, and rule 9 keeps it out. What is
//! actually needed is smaller than it looks: the directories are specified by
//! the XDG base-directory and font-directory conventions, and a family is
//! chosen from a preference list.
//!
//! The walk collects **names**, not fonts. Opening and parsing every font on a
//! machine costs tens of milliseconds against an 80 ms cold-start budget, so
//! only the files a preference actually matches are read.

use std::path::{Path, PathBuf};

use rustix::fs::{Dir, FileType, Mode, OFlags};

/// Directories searched, in the order a later one overrides an earlier one.
///
/// User directories come last so a font a user installed wins over the
/// distribution's copy of the same family, which is what a user installing a
/// font is asking for.
fn roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
        // A Flatpak sandbox sees the host's fonts here and nowhere else.
        PathBuf::from("/run/host/fonts"),
        PathBuf::from("/run/host/user-fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".fonts"));
        roots.push(home.join(".local/share/fonts"));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        roots.push(data.join("fonts"));
    }
    roots
}

/// A font file found on disk.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// Where it is.
    pub path: PathBuf,
    /// The file stem, lowercased with every non-alphanumeric byte removed, so
    /// `NotoSans-Regular.ttf`, `noto_sans_regular.otf` and `NotoSansRegular.ttf`
    /// all match the same preference.
    pub token: String,
}

/// Every font file under every font directory.
///
/// Depth-limited: a font directory is two or three levels deep by convention,
/// and a symlink loop under `~/.fonts` must not be able to hang start-up.
#[must_use]
pub fn scan() -> Vec<Candidate> {
    let mut found = Vec::with_capacity(512);
    let mut visited = std::collections::HashSet::with_capacity(64);
    for root in roots() {
        walk(&root, 0, &mut visited, &mut found);
    }
    found
}

const MAX_DEPTH: u32 = 4;

/// Walks `directory`, **following symlinks**, with a visited set for the loops.
///
/// It used to open with `NOFOLLOW` and to recurse only into entries whose
/// `d_type` was `Directory`, which meant a font directory that is itself a
/// symlink was skipped in silence: `open` returned `ELOOP` and the early return
/// was indistinguishable from "not there". That is exactly what home-manager's
/// `fonts.fontconfig` produces for `~/.local/share/fonts` — a link into the Nix
/// store — and with the UI faces missing the binary refuses to start.
///
/// A device-and-inode set bounds the loops that `NOFOLLOW` used to bound.
/// `MAX_DEPTH` stays as the second bound, because fan-out costs opens even when
/// nothing repeats.
fn walk(
    directory: &Path,
    depth: u32,
    visited: &mut std::collections::HashSet<(u64, u64)>,
    found: &mut Vec<Candidate>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    // `getdents64` rather than `read_dir`, per the crate lint: `d_type` answers
    // file-versus-directory without a `statx` per entry, which over a font tree
    // of a few thousand names is the whole cost of this walk.
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let Ok(fd) = rustix::fs::open(directory, flags, Mode::empty()) else { return };
    if let Ok(stat) = rustix::fs::fstat(&fd)
        && !visited.insert((stat.st_dev, stat.st_ino))
    {
        return;
    }
    let Ok(dir) = Dir::read_from(&fd) else { return };
    for entry in dir.flatten() {
        let name = entry.file_name().to_bytes();
        if name.is_empty() || name[0] == b'.' {
            continue;
        }
        let Ok(name) = std::str::from_utf8(name) else { continue };
        let path = directory.join(name);
        match entry.file_type() {
            FileType::Directory => walk(&path, depth + 1, visited, found),
            FileType::RegularFile => {
                if let Some(token) = font_token(name) {
                    found.push(Candidate { path, token });
                }
            }
            // `d_type` says "symlink" and nothing about what it points at. A
            // name that looks like a font is taken as one; anything else is
            // tried as a directory, which costs one `open` that returns
            // `ENOTDIR` when it is not.
            FileType::Symlink => match font_token(name) {
                Some(token) => found.push(Candidate { path, token }),
                None => walk(&path, depth + 1, visited, found),
            },
            _ => {}
        }
    }
}

/// The match token for a font file, or `None` if it is not a font.
fn font_token(name: &str) -> Option<String> {
    let (stem, extension) = name.rsplit_once('.')?;
    if !matches!(extension.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc") {
        return None;
    }
    Some(stem.chars().filter(char::is_ascii_alphanumeric).flat_map(char::to_lowercase).collect())
}

/// Style words that change what a face *is*.
///
/// A substring match alone is not enough: `dejavusansbold` contains
/// `dejavusans`, and `dejavusansboldoblique` contains `dejavusansbold`. Asking
/// for the regular face and getting the bold one, or asking for bold and getting
/// bold oblique, is how a column header ends up in italics — which is exactly
/// what the first capture of the Details view showed.
const STYLE_WORDS: &[&str] = &[
    "italic",
    "oblique",
    "bold",
    "light",
    "thin",
    "black",
    "medium",
    "semi",
    "extra",
    "demi",
    "condensed",
    "narrow",
    "mono",
    "display",
];

/// The best candidate for any of `wanted`, preferring earlier needles.
///
/// Within one needle: an exact token wins, then the shortest token that contains
/// it and introduces no style word the needle did not ask for.
#[must_use]
pub fn pick<'a>(candidates: &'a [Candidate], wanted: &[&str]) -> Option<&'a Candidate> {
    wanted.iter().find_map(|needle| {
        if let Some(exact) = candidates.iter().find(|it| it.token == *needle) {
            return Some(exact);
        }
        candidates
            .iter()
            .filter(|it| it.token.contains(needle))
            .filter(|it| {
                STYLE_WORDS.iter().all(|word| needle.contains(word) || !it.token.contains(word))
            })
            // The shortest remaining token is the closest match: `notosans`
            // beats `notosansdisplay` for a request for `notosans`.
            .min_by_key(|it| it.token.len())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Candidate, font_token, pick, walk};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-fonts-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// home-manager's `fonts.fontconfig` makes `~/.local/share/fonts` a symlink
    /// into the Nix store. With `NOFOLLOW`, `open` returned `ELOOP` and the
    /// early return was indistinguishable from "there is no such directory" —
    /// so the UI faces were missing and the binary refused to start.
    #[test]
    fn a_font_directory_that_is_a_symlink_is_walked() {
        let root = scratch("symlink");
        let real = root.join("store");
        std::fs::create_dir_all(real.join("truetype")).unwrap();
        std::fs::write(real.join("truetype/NotoSans-Regular.ttf"), b"not a font").unwrap();
        let link = root.join("fonts");
        if std::os::unix::fs::symlink(&real, &link).is_err() {
            return;
        }

        let mut found = Vec::new();
        walk(&link, 0, &mut HashSet::new(), &mut found);
        assert_eq!(
            found.iter().map(|it| it.token.as_str()).collect::<Vec<_>>(),
            vec!["notosansregular"],
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// And a subdirectory that is a symlink, which is the other half: `d_type`
    /// says `Symlink` and says nothing about the target.
    #[test]
    fn a_symlinked_subdirectory_is_walked_and_a_loop_does_not_hang() {
        let root = scratch("loop");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/DejaVuSans.ttf"), b"not a font").unwrap();
        if std::os::unix::fs::symlink(root.join("real"), root.join("linked")).is_err() {
            return;
        }
        // Straight back to the top: bounded by the visited set, not by depth.
        if std::os::unix::fs::symlink(&root, root.join("real/again")).is_err() {
            return;
        }

        let mut found = Vec::new();
        walk(&root, 0, &mut HashSet::new(), &mut found);
        assert_eq!(
            found.len(),
            1,
            "found {:?}",
            found.iter().map(|it| &it.path).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_token_ignores_punctuation_and_case() {
        assert_eq!(font_token("NotoSans-Regular.ttf").as_deref(), Some("notosansregular"));
        assert_eq!(font_token("noto_sans_regular.OTF").as_deref(), Some("notosansregular"));
        assert_eq!(font_token("DejaVuSans.ttf").as_deref(), Some("dejavusans"));
    }

    #[test]
    fn only_font_extensions_are_candidates() {
        assert!(font_token("README").is_none());
        assert!(font_token("fonts.dir").is_none());
        assert!(font_token("Inter.pfb").is_none());
        assert!(font_token("NotoSansCJK.ttc").is_some());
    }

    fn candidates(tokens: &[&str]) -> Vec<Candidate> {
        tokens
            .iter()
            .map(|token| Candidate { path: token.into(), token: (*token).to_owned() })
            .collect()
    }

    #[test]
    fn preference_order_beats_disk_order() {
        let found = candidates(&["dejavusans", "interregular"]);
        let chosen = pick(&found, &["interregular", "dejavusans"]).unwrap();
        assert_eq!(chosen.token, "interregular", "the preference list wins, not the walk order");
    }

    /// The defect this function was rewritten for: a substring match handed back
    /// a styled face, and the column headers rendered in italics.
    #[test]
    fn asking_for_bold_does_not_return_bold_oblique() {
        let found = candidates(&["dejavusansboldoblique", "dejavusansbold", "dejavusans"]);
        assert_eq!(pick(&found, &["dejavusansbold"]).unwrap().token, "dejavusansbold");
    }

    #[test]
    fn asking_for_regular_does_not_return_a_styled_face() {
        let found = candidates(&["dejavusansbold", "dejavusansoblique", "dejavusans"]);
        assert_eq!(pick(&found, &["dejavusans"]).unwrap().token, "dejavusans");

        // And with no plain face present, nothing is better than the wrong one.
        let styled = candidates(&["dejavusansbold", "dejavusansoblique"]);
        assert!(pick(&styled, &["dejavusans"]).is_none());
    }

    #[test]
    fn the_closest_token_wins_when_several_contain_the_needle() {
        let found = candidates(&["notosansdisplayregular", "notosansregular"]);
        assert_eq!(pick(&found, &["notosansregular"]).unwrap().token, "notosansregular");
    }
}
