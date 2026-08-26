//! What kind of file this is.
//!
//! By name, from shared-mime-info's `globs2`, plus a short table of signatures
//! for the files whose names say nothing. That covers what a file manager needs:
//! a name is what a listing already has, and reading the first bytes of every
//! file in a directory to type it is exactly the per-entry I/O rule 1 exists to
//! avoid.

use std::collections::HashMap;
use std::path::Path;

/// The type of a directory, which shared-mime-info defines and no glob matches.
pub const DIRECTORY: &str = "inode/directory";
/// What an unrecognised file is called.
pub const UNKNOWN: &str = "application/octet-stream";
/// What an unrecognised *text* file is called.
pub const TEXT: &str = "text/plain";

/// Signatures for files whose names do not say what they are.
///
/// A short table rather than shared-mime-info's binary `magic` database, which is
/// a tree of offsets and masks and a parser of its own. These are the ones that
/// actually turn up without an extension in a downloads folder. What the missing
/// database costs is listed in the crate documentation.
const SIGNATURES: &[(&[u8], &str)] = &[
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"%PDF-", "application/pdf"),
    (b"\x7fELF", "application/x-executable"),
    (b"PK\x03\x04", "application/zip"),
    (b"\x1f\x8b", "application/gzip"),
    (b"BZh", "application/x-bzip"),
    (b"\xfd7zXZ\x00", "application/x-xz"),
    (b"\x28\xb5\x2f\xfd", "application/zstd"),
    (b"7z\xbc\xaf\x27\x1c", "application/x-7z-compressed"),
    (b"Rar!\x1a\x07", "application/vnd.rar"),
    (b"OggS", "audio/ogg"),
    (b"\x00\x00\x01\x00", "image/vnd.microsoft.icon"),
    (b"#!", "application/x-shellscript"),
    (b"<?xml", "application/xml"),
    (b"RIFF", "audio/x-wav"),
];

/// One `globs2` entry that is neither a plain extension nor a literal name.
struct Pattern {
    weight: u32,
    glob: String,
    mime: String,
}

/// The filename half of shared-mime-info.
#[derive(Default)]
pub struct MimeDatabase {
    /// Lowercased extension without the dot, to (weight, type).
    extensions: HashMap<String, (u32, String)>,
    /// Whole filenames, like `Makefile`.
    literals: HashMap<String, (u32, String)>,
    /// Everything else, matched in order of weight.
    patterns: Vec<Pattern>,
}

impl MimeDatabase {
    /// Reads `globs2` from every data directory, most specific first.
    ///
    /// A later file never overrides an earlier one at the same weight, which is
    /// how `$XDG_DATA_HOME` beats `/usr/share` — the specification's rule, and
    /// the reason weight is compared rather than ignored.
    #[must_use]
    pub fn load() -> Self {
        let mut database = Self::default();
        for dir in crate::data_dirs() {
            if let Some(text) = crate::read(&dir.join("mime/globs2")) {
                database.absorb(&text);
            }
        }
        database
    }

    /// Parses one `globs2` file: `weight:type:glob[:flags]` per line.
    pub fn absorb(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.splitn(3, ':');
            let (Some(weight), Some(mime), Some(glob)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            // The optional fourth field is flags such as `cs` for case
            // sensitivity, and it is separated from the glob by another colon.
            // A glob may not contain one, so splitting again is safe.
            let glob = glob.split(':').next().unwrap_or(glob);
            let Ok(weight) = weight.parse::<u32>() else { continue };
            if glob.is_empty() || mime.is_empty() {
                continue;
            }
            self.insert(weight, mime, glob);
        }
    }

    fn insert(&mut self, weight: u32, mime: &str, glob: &str) {
        // `*.ext` is the overwhelming majority and becomes a hash lookup.
        if let Some(extension) =
            glob.strip_prefix("*.").filter(|rest| !rest.contains(['*', '?', '[']))
        {
            let key = extension.to_ascii_lowercase();
            let entry = self.extensions.entry(key).or_insert((0, String::new()));
            if entry.1.is_empty() || weight > entry.0 {
                *entry = (weight, mime.to_owned());
            }
            return;
        }
        if !glob.contains(['*', '?', '[']) {
            let entry = self.literals.entry(glob.to_owned()).or_insert((0, String::new()));
            if entry.1.is_empty() || weight > entry.0 {
                *entry = (weight, mime.to_owned());
            }
            return;
        }
        self.patterns.push(Pattern { weight, glob: glob.to_owned(), mime: mime.to_owned() });
    }

    /// The type of a file with this name, by name alone.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&str> {
        if let Some((_, mime)) = self.literals.get(name) {
            return Some(mime);
        }
        // Longest extension first, so `.tar.gz` beats `.gz`.
        let lower = name.to_ascii_lowercase();
        let mut best: Option<(u32, usize, &str)> = None;
        for (index, _) in lower.match_indices('.') {
            let extension = &lower[index + 1..];
            if let Some((weight, mime)) = self.extensions.get(extension) {
                let candidate = (*weight, extension.len(), mime.as_str());
                if best.is_none_or(|current| candidate > current) {
                    best = Some(candidate);
                }
            }
        }
        if let Some((_, _, mime)) = best {
            return Some(mime);
        }
        self.patterns
            .iter()
            .filter(|pattern| matches(&pattern.glob, name))
            .max_by_key(|pattern| pattern.weight)
            .map(|pattern| pattern.mime.as_str())
    }

    /// The type of `path`, using its name and, if that fails, its first bytes.
    ///
    /// Reads at most 32 bytes and only when the name says nothing, so a listing
    /// of ten thousand named files performs no I/O at all. **Runs on a job**:
    /// it opens a file.
    #[must_use]
    pub fn of(&self, path: &Path, is_directory: bool) -> String {
        if is_directory {
            return DIRECTORY.to_owned();
        }
        if let Some(mime) =
            path.file_name().and_then(|name| name.to_str()).and_then(|name| self.by_name(name))
        {
            return mime.to_owned();
        }
        sniff(path).unwrap_or_else(|| UNKNOWN.to_owned())
    }
}

/// The type of a file from its first bytes.
fn sniff(path: &Path) -> Option<String> {
    let mut head = [0u8; 32];
    let file = std::fs::File::open(path).ok()?;
    let read = std::io::Read::read(&mut &file, &mut head).ok()?;
    let head = &head[..read];
    for (signature, mime) in SIGNATURES {
        if head.starts_with(signature) {
            return Some((*mime).to_owned());
        }
    }
    // Anything with no control bytes in its first line is text, which is the one
    // guess worth making: it is the difference between a README with no
    // extension opening in an editor and not opening at all.
    (!head.is_empty()
        && head.iter().all(|byte| *byte >= 0x20 || matches!(byte, b'\t' | b'\n' | b'\r')))
    .then(|| TEXT.to_owned())
}

/// Matches a shared-mime-info glob against a name.
///
/// `*`, `?` and `[...]`, which is all the format uses. Written out rather than
/// taken from a crate because it is twenty lines and the alternative brings a
/// regex engine for four hundred patterns that are almost all `*.ext`.
fn matches(glob: &str, name: &str) -> bool {
    fn walk(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some(b'*') => (0..=text.len()).any(|skip| walk(&pattern[1..], &text[skip..])),
            Some(b'?') => !text.is_empty() && walk(&pattern[1..], &text[1..]),
            Some(b'[') => {
                let Some(close) = pattern.iter().position(|byte| *byte == b']') else {
                    return false;
                };
                let Some(first) = text.first() else { return false };
                let set = &pattern[1..close];
                let (negated, set) = match set.first() {
                    Some(b'^' | b'!') => (true, &set[1..]),
                    _ => (false, set),
                };
                let mut hit = false;
                let mut index = 0;
                while index < set.len() {
                    if index + 2 < set.len() && set[index + 1] == b'-' {
                        hit |= (set[index]..=set[index + 2]).contains(first);
                        index += 3;
                    } else {
                        hit |= set[index] == *first;
                        index += 1;
                    }
                }
                hit != negated && walk(&pattern[close + 1..], &text[1..])
            }
            Some(byte) => text.first() == Some(byte) && walk(&pattern[1..], &text[1..]),
        }
    }
    walk(glob.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{DIRECTORY, MimeDatabase, TEXT, matches};
    use std::path::Path;

    fn database() -> MimeDatabase {
        let mut database = MimeDatabase::default();
        database.absorb(
            "# a comment\n\
             50:text/plain:*.txt\n\
             50:application/gzip:*.gz\n\
             50:application/x-compressed-tar:*.tar.gz\n\
             50:text/x-makefile:Makefile\n\
             50:application/x-trash:*~\n\
             50:text/x-csrc:*.c\n\
             40:text/x-generic:*.c\n\
             50:application/x-font-pcf:*.pcf.Z\n\
             50:text/x-log:*.log:cs\n",
        );
        database
    }

    #[test]
    fn a_plain_extension_resolves() {
        let database = database();
        assert_eq!(database.by_name("notes.txt"), Some("text/plain"));
        assert_eq!(database.by_name("NOTES.TXT"), Some("text/plain"), "and is case-insensitive");
    }

    /// `.tar.gz` is a compressed tar and not a gzip, and the only thing that
    /// decides it is preferring the longer extension.
    #[test]
    fn the_longer_extension_wins() {
        let database = database();
        assert_eq!(database.by_name("backup.tar.gz"), Some("application/x-compressed-tar"));
        assert_eq!(database.by_name("backup.gz"), Some("application/gzip"));
    }

    #[test]
    fn a_literal_name_beats_having_no_extension_at_all() {
        let database = database();
        assert_eq!(database.by_name("Makefile"), Some("text/x-makefile"));
        assert_eq!(database.by_name("makefile"), None, "the literal is case-sensitive");
    }

    #[test]
    fn the_heavier_glob_wins_when_two_match() {
        let database = database();
        assert_eq!(database.by_name("main.c"), Some("text/x-csrc"));
    }

    #[test]
    fn a_glob_that_is_not_an_extension_still_matches() {
        let database = database();
        assert_eq!(database.by_name("notes.txt~"), Some("application/x-trash"));
    }

    #[test]
    fn a_malformed_line_is_skipped_rather_than_taken() {
        let mut database = MimeDatabase::default();
        database.absorb("nonsense\n:::\nnotanumber:text/plain:*.txt\n50::*.x\n50:text/plain:\n");
        assert_eq!(database.by_name("a.txt"), None);
        assert_eq!(database.by_name("a.x"), None);
    }

    #[test]
    fn glob_matching_handles_the_three_metacharacters() {
        assert!(matches("*.txt", "a.txt"));
        assert!(!matches("*.txt", "a.txtx"));
        assert!(matches("*.[ch]", "main.c"));
        assert!(matches("*.[ch]", "main.h"));
        assert!(!matches("*.[ch]", "main.o"));
        assert!(matches("file?.log", "file1.log"));
        assert!(!matches("file?.log", "file12.log"));
        assert!(matches("*", "anything"));
        assert!(matches("*.pcf.Z", "cursor.pcf.Z"));
        assert!(!matches("[a-c]x", "dx"));
        assert!(matches("[a-c]x", "bx"));
        assert!(matches("[^a-c]x", "dx"));
    }

    #[test]
    fn a_directory_is_typed_without_being_opened() {
        let database = database();
        assert_eq!(database.of(Path::new("/nonexistent"), true), DIRECTORY);
    }

    #[test]
    fn a_file_with_no_useful_name_is_sniffed() {
        let root = std::env::temp_dir().join(format!("skerry-mime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("png"), b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();
        std::fs::write(root.join("readme"), b"Hello, this is text.\n").unwrap();
        std::fs::write(root.join("binary"), [0u8, 1, 2, 3, 4]).unwrap();

        let database = database();
        assert_eq!(database.of(&root.join("png"), false), "image/png");
        assert_eq!(database.of(&root.join("readme"), false), TEXT);
        assert_eq!(database.of(&root.join("binary"), false), super::UNKNOWN);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The real database on this machine, if there is one. Not an assertion
    /// about its contents — an assertion that parsing it produces something.
    #[test]
    fn the_installed_database_parses_and_types_the_obvious_things() {
        let database = MimeDatabase::load();
        if database.by_name("a.txt").is_none() {
            return;
        }
        assert_eq!(database.by_name("a.txt"), Some("text/plain"));
        assert_eq!(database.by_name("a.png"), Some("image/png"));
        assert_eq!(database.by_name("a.pdf"), Some("application/pdf"));
    }
}
