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
        // First wins at equal weight, and a repeat only replaces a lighter one —
        // the same rule the two maps above use. `globs2` is read once per XDG
        // data directory, most specific first, so without this the list grew by
        // one entry per directory per glob and `/usr/share` overrode
        // `$XDG_DATA_HOME`.
        if let Some(existing) = self.patterns.iter_mut().find(|it| it.glob == glob) {
            if weight > existing.weight {
                *existing = Pattern { weight, glob: glob.to_owned(), mime: mime.to_owned() };
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
        // Not `max_by_key`: that returns the **last** maximum, so two patterns of
        // equal weight resolved to whichever was read last — which is
        // `/usr/share`, the opposite of the precedence every other lookup here
        // honours.
        let mut best: Option<&Pattern> = None;
        for pattern in &self.patterns {
            if !matches(&pattern.glob, name) {
                continue;
            }
            if best.is_none_or(|current| pattern.weight > current.weight) {
                best = Some(pattern);
            }
        }
        best.map(|pattern| pattern.mime.as_str())
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
/// taken from a crate because it is thirty lines and the alternative brings a
/// regex engine for four hundred patterns that are almost all `*.ext`.
///
/// **Iterative, with one backtrack point.** The obvious recursive form — every
/// `*` trying every split — is exponential: measured, `*a*a…*b` against a string
/// of twenty `a`s took 442 calls with two stars, 49,595 with five and 665,879
/// with eight, and twelve stars against a sixty-character name did not finish in
/// ten minutes. Both halves of that are attacker-supplied: a `globs2` line is a
/// file anyone can drop into `~/.local/share/mime/`, and the name is a filename.
/// The greedy form below is O(pattern × name) and needs no stack.
fn matches(glob: &str, name: &str) -> bool {
    let (pattern, text) = (glob.as_bytes(), name.as_bytes());
    let (mut at, mut index) = (0usize, 0usize);
    // Where to resume from if the greedy star took too much.
    let mut star: Option<(usize, usize)> = None;
    while index < text.len() {
        if let Some(next) = element(pattern, at, text[index]) {
            at = next;
            index += 1;
        } else if pattern.get(at) == Some(&b'*') {
            star = Some((at, index));
            at += 1;
        } else if let Some((from, taken)) = star {
            at = from + 1;
            index = taken + 1;
            star = Some((from, taken + 1));
        } else {
            return false;
        }
    }
    while pattern.get(at) == Some(&b'*') {
        at += 1;
    }
    at == pattern.len()
}

/// Matches one pattern element against one byte.
///
/// Returns where the pattern continues, or `None` if it did not match — which
/// `*` always is, because a star is not an element and is handled by the caller.
fn element(pattern: &[u8], at: usize, byte: u8) -> Option<usize> {
    match pattern.get(at)? {
        b'?' => Some(at + 1),
        b'*' => None,
        b'[' => {
            let close = at + pattern[at..].iter().position(|it| *it == b']')?;
            let set = &pattern[at + 1..close];
            let (negated, set) = match set.first() {
                Some(b'^' | b'!') => (true, &set[1..]),
                _ => (false, set),
            };
            let mut hit = false;
            let mut cursor = 0;
            while cursor < set.len() {
                if cursor + 2 < set.len() && set[cursor + 1] == b'-' {
                    hit |= (set[cursor]..=set[cursor + 2]).contains(&byte);
                    cursor += 3;
                } else {
                    hit |= set[cursor] == byte;
                    cursor += 1;
                }
            }
            (hit != negated).then_some(close + 1)
        }
        other => (*other == byte).then_some(at + 1),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{DIRECTORY, MimeDatabase, TEXT, matches};
    use std::path::Path;

    /// A `globs2` line is a file anyone can write, and a filename is what it is
    /// matched against. The recursive matcher took 665,879 calls for eight stars
    /// against twenty characters and did not finish at all for twelve against
    /// sixty; this asks for twenty against a hundred and twenty.
    #[test]
    fn a_pathological_glob_does_not_take_exponential_time() {
        let glob = format!("{}b", "*a".repeat(20));
        let name = "a".repeat(120);
        let started = std::time::Instant::now();
        assert!(!matches(&glob, &name), "there is no b in it");
        assert!(started.elapsed().as_millis() < 50, "twenty stars took {:?}", started.elapsed());
    }

    /// The greedy matcher has to agree with the recursive one it replaced on
    /// everything the recursive one got right.
    #[test]
    fn stars_classes_and_question_marks_still_mean_what_they_meant() {
        for (glob, name, wanted) in [
            ("*.c", "main.c", true),
            ("*.c", "main.cpp", false),
            ("*", "anything", true),
            ("", "", true),
            ("", "x", false),
            ("*", "", true),
            ("a*b*c", "abc", true),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("?????", "abcde", true),
            ("?????", "abcd", false),
            ("[abc]x", "bx", true),
            ("[abc]x", "dx", false),
            ("[a-z]x", "qx", true),
            ("[^a-z]x", "Qx", true),
            ("[^a-z]x", "qx", false),
            ("*[0-9].log", "server7.log", true),
            ("*[0-9].log", "server.log", false),
            ("*.tar.*", "archive.tar.gz", true),
            ("[unclosed", "u", false),
        ] {
            assert_eq!(matches(glob, name), wanted, "{glob:?} against {name:?}");
        }
    }

    /// Two data directories offering the same pattern at the same weight: the
    /// one read first wins, which is `$XDG_DATA_HOME`. `max_by_key` returns the
    /// last maximum and so gave `/usr/share`, silently inverting the precedence
    /// the extension and literal paths honour.
    #[test]
    fn the_first_data_directory_wins_a_tie_for_patterns_too() {
        let mut database = MimeDatabase::default();
        database.absorb("50:text/from-xdg-data-home:*.[ch]x\n");
        database.absorb("50:text/from-usr-share:*.[ch]x\n");
        assert_eq!(database.by_name("a.cx"), Some("text/from-xdg-data-home"));

        // And a heavier one still overrides, whichever order it arrives in.
        database.absorb("80:text/heavier:*.[ch]x\n");
        assert_eq!(database.by_name("a.cx"), Some("text/heavier"));
    }

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
