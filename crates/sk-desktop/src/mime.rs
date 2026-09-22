// SPDX-License-Identifier: Apache-2.0
//! What kind of file this is.
//!
//! By name, from shared-mime-info's `globs2`, plus a short table of signatures
//! for the files whose names say nothing. That covers what a file manager needs:
//! a name is what a listing already has, and reading the first bytes of every
//! file in a directory to type it is exactly the per-entry I/O rule 1 exists to
//! avoid.

use std::collections::HashMap;
use std::path::Path;

use sk_model::{EntryId, Flags, Listing};

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

/// The broad kind of a file, for anything that wants one mark per kind.
///
/// Nine of them, because that is how many a person can tell apart at thirteen
/// pixels without reading the name — and because the alternative, an icon per
/// MIME type, is an icon theme, which is M7 and is four hundred lines of
/// `index.theme` before the first pixel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Category {
    /// A directory.
    Folder,
    /// A picture.
    Image,
    /// Sound.
    Audio,
    /// Moving pictures.
    Video,
    /// Something holding other files.
    Archive,
    /// Source, markup, or a script.
    Code,
    /// Prose: text, PDF, office formats.
    Document,
    /// Something the system can run.
    Executable,
    /// Anything else, including a file whose type nothing claims.
    #[default]
    Unknown,
}

impl Category {
    /// The nine, in the order a grouped pane lists them.
    ///
    /// Folders first because a folder is not a file and everything else is
    /// alphabetical-by-accident; `Unknown` last because "anything else" is where
    /// a heading nobody recognises belongs.
    pub const ALL: [Self; 9] = [
        Self::Folder,
        Self::Image,
        Self::Audio,
        Self::Video,
        Self::Archive,
        Self::Code,
        Self::Document,
        Self::Executable,
        Self::Unknown,
    ];

    /// Its position in [`Self::ALL`], which is the byte a sort bands on.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Self::Folder => 0,
            Self::Image => 1,
            Self::Audio => 2,
            Self::Video => 3,
            Self::Archive => 4,
            Self::Code => 5,
            Self::Document => 6,
            Self::Executable => 7,
            Self::Unknown => 8,
        }
    }
}

/// What kind one entry of a listing is.
///
/// **The one place the taxonomy is decided.** A row's mark, a cell's mark and a
/// grouped pane's headings all read this, so a file cannot be an image to the
/// colour and a document to the heading above it.
///
/// `mime` is optional because the databases are read on a job: for the first few
/// milliseconds of a session there is no answer from the name, and every entry
/// that is not a folder or a program is `Unknown`.
#[must_use]
pub fn kind_of(listing: &Listing, id: EntryId, mime: Option<&MimeDatabase>) -> Category {
    kind_and_type_of(listing, id, mime).0
}

/// [`kind_of`], and the type the name answered to on the way.
///
/// **One lookup for both**, for a caller that wants the type as well as the
/// kind — a mark that tells a spreadsheet from a letter — and would otherwise
/// ask the database twice for every visible row of every frame.
#[must_use]
pub fn kind_and_type_of<'m>(
    listing: &Listing,
    id: EntryId,
    mime: Option<&'m MimeDatabase>,
) -> (Category, Option<&'m str>) {
    let flags = listing.flags(id);
    if flags.has(Flags::DIR) {
        return (Category::Folder, None);
    }
    let found = mime.and_then(|mime| mime.by_name(listing.name(id)));
    let by_name = found.map_or(Category::Unknown, category);
    // The mode bit after the name: a program is usually called something with no
    // extension at all — `runme`, `configure`, half of `/usr/bin` — so a
    // name-only answer calls it unknown, and what it is is the one thing about it
    // worth marking.
    if by_name == Category::Unknown && flags.has(Flags::EXEC) {
        (Category::Executable, found)
    } else {
        (by_name, found)
    }
}

/// One band byte per entry, indexed by [`EntryId`], for [`Banding::Kind`].
///
/// **Every entry, not the visible ones.** A sort groups the whole listing, so
/// this is the one place in the program that asks `by_name` a hundred thousand
/// times — which is why it runs on the job that computes the order and not on the
/// frame loop. Measured cost is in
/// [09-performance-budgets.md](../../../docs/09-performance-budgets.md).
///
/// [`Banding::Kind`]: sk_model::Banding::Kind
#[must_use]
pub fn kinds(listing: &Listing, mime: Option<&MimeDatabase>) -> Vec<u8> {
    (0..listing.len() as u32).map(|index| kind_of(listing, EntryId(index), mime).index()).collect()
}

/// Whether a type's *bytes* are text.
///
/// **Not [`Category`], which answers a different question.** `Document` holds
/// `text/plain` and `application/pdf` together because they are the same thing
/// to a person reading a listing, and they are not the same thing to a clipboard.
///
/// `text/*` is the specification's own answer and most of this one. The list
/// below exists because shared-mime-info spells a dozen text formats
/// `application/*` for historical reasons — JSON, XML and every shell script
/// among them — and a person copying the text out of a `.json` is not interested
/// in why it is not called `text/json`.
///
/// The answer is a *guess from the name*, which is the only kind available
/// without reading the file: it decides whether to offer the action, and the read
/// that follows decides whether the bytes really are text.
#[must_use]
pub fn is_text(mime: &str) -> bool {
    mime.starts_with("text/") || TEXT_TYPES.contains(&mime)
}

/// Text that shared-mime-info does not call `text/`.
const TEXT_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/xhtml+xml",
    "application/rss+xml",
    "application/atom+xml",
    "application/javascript",
    "application/ecmascript",
    "application/sql",
    "application/toml",
    "application/x-yaml",
    "application/x-shellscript",
    "application/x-csh",
    "application/x-perl",
    "application/x-php",
    "application/x-python",
    "application/x-ruby",
    "application/x-awk",
    "application/x-m4",
    "application/x-tex",
    "application/x-desktop",
    "application/x-subrip",
    // XML with a picture in it. The mark on the row says image, and a person
    // asking for the text of an `.svg` knows exactly what they are asking for.
    "image/svg+xml",
];

/// Which kind a MIME type belongs to.
///
/// By prefix where shared-mime-info has one — `image/`, `audio/`, `video/` are
/// three quarters of the answer — and by a short list where it does not. The
/// list is short on purpose: a type that is not on it is `Unknown`, which draws
/// the plain mark, and a plain mark is the right answer for a file nothing
/// recognises.
#[must_use]
pub fn category(mime: &str) -> Category {
    let (top, rest) = mime.split_once('/').unwrap_or((mime, ""));
    match top {
        "inode" => Category::Folder,
        "image" => Category::Image,
        "audio" => Category::Audio,
        "video" => Category::Video,
        "font" => Category::Document,
        _ => other_category(mime, rest),
    }
}

fn other_category(mime: &str, rest: &str) -> Category {
    const ARCHIVES: &[&str] = &[
        "zip",
        "gzip",
        "x-tar",
        "x-compressed-tar",
        "x-xz",
        "x-xz-compressed-tar",
        "x-7z-compressed",
        "vnd.rar",
        "x-bzip",
        "x-bzip2",
        "x-bzip-compressed-tar",
        "zstd",
        "x-zstd-compressed-tar",
        "x-lzma",
        "x-cpio",
        "x-archive",
        "vnd.debian.binary-package",
        "x-rpm",
        "x-iso9660-image",
    ];
    // **`x-shellscript` is deliberately absent.** A shell script is text you
    // open in an editor, and it is the one runnable thing whose name a person
    // reads rather than launches. `Executable` here means a binary.
    const RUNNABLE: &[&str] =
        &["x-executable", "x-sharedlib", "x-pie-executable", "x-ms-dos-executable"];
    if ARCHIVES.contains(&rest) {
        return Category::Archive;
    }
    if RUNNABLE.contains(&rest) {
        return Category::Executable;
    }
    // `text/x-csrc`, `text/x-python`, `application/javascript`, `text/x-makefile`
    // — source is spelled several ways and none of them is a prefix.
    if rest.ends_with("src")
        || rest.ends_with("hdr")
        || rest.ends_with("script")
        || matches!(
            mime,
            "application/javascript"
                | "application/json"
                | "application/xml"
                | "application/x-yaml"
                | "text/x-makefile"
                | "text/x-cmake"
                | "text/x-python"
                | "text/x-ruby"
                | "text/x-java"
                | "text/x-go"
                | "text/x-rust"
                | "text/html"
                | "text/css"
                | "text/x-diff"
                | "text/x-patch"
        )
    {
        return Category::Code;
    }
    if mime.starts_with("text/") || DOCUMENTS.contains(&rest) {
        return Category::Document;
    }
    Category::Unknown
}

const DOCUMENTS: &[&str] = &[
    "pdf",
    "epub+zip",
    "rtf",
    "msword",
    "vnd.oasis.opendocument.text",
    "vnd.oasis.opendocument.spreadsheet",
    "vnd.oasis.opendocument.presentation",
    "vnd.openxmlformats-officedocument.wordprocessingml.document",
    "vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "vnd.openxmlformats-officedocument.presentationml.presentation",
];

/// The filename half of shared-mime-info.
#[derive(Default)]
pub struct MimeDatabase {
    /// Lowercased extension without the dot, to (weight, type).
    extensions: HashMap<String, (u32, String)>,
    /// Extensions whose `globs2` line carried the `cs` flag, kept in the case
    /// they were written in and consulted first.
    ///
    /// **`*.C` is C++ and `*.c` is C**, and both are weight 50. Folding them into
    /// one key made the first line read win, and `/usr/share/mime/globs2` has
    /// `*.C` at line 1118 and `*.c` at 1352 — so every C source file on the
    /// machine was typed as C++ and opened with whatever handles C++.
    cased: HashMap<String, (u32, String)>,
    /// Whole filenames, like `makefile`, **lowercased**.
    ///
    /// Folded for the same reason the extensions are: `globs2` spells it
    /// `makefile` with no `cs` flag, and a specification-conformant reader
    /// matches a flagless glob without regard to case. Keyed as written, a file
    /// called `Makefile` — which is what every one of them is called — had no
    /// type at all.
    literals: HashMap<String, (u32, String)>,
    /// Whole filenames whose glob carried `cs`, kept as written.
    cased_literals: HashMap<String, (u32, String)>,
    /// Everything else, matched in order of weight.
    patterns: Vec<Pattern>,
    /// The icon a type names in `mime/icons`, where it names one other than its
    /// own spelling.
    icons: HashMap<String, String>,
    /// The generic icon a type names in `mime/generic-icons` — `package-x-generic`
    /// for a compressed tarball, `x-office-document` for a PDF.
    generic_icons: HashMap<String, String>,
}

/// Takes one `type:icon` file into `into`, keeping what an earlier data
/// directory already said — which is how `$XDG_DATA_HOME` beats `/usr/share`.
fn absorb_icons(into: &mut HashMap<String, String>, text: &str) {
    for line in text.lines() {
        let Some((mime, icon)) = line.trim().split_once(':') else { continue };
        if !mime.is_empty() && !icon.is_empty() && !mime.starts_with('#') {
            into.entry(mime.to_owned()).or_insert_with(|| icon.to_owned());
        }
    }
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
            if let Some(text) = crate::read(&dir.join("mime/icons")) {
                absorb_icons(&mut database.icons, &text);
            }
            if let Some(text) = crate::read(&dir.join("mime/generic-icons")) {
                absorb_icons(&mut database.generic_icons, &text);
            }
        }
        database
    }

    /// Takes one `mime/generic-icons` file: `type:icon` per line.
    pub fn absorb_generic_icons(&mut self, text: &str) {
        absorb_icons(&mut self.generic_icons, text);
    }

    /// The two icon names a type answers to, the specific one first.
    ///
    /// **The [shared-mime-info
    /// specification](https://specifications.freedesktop.org/shared-mime-info-spec/latest/)'s
    /// own two**: the `icon` the database names, or the type with its `/` made a
    /// `-`; then the `generic-icon` it names, or `{media}-x-generic` where it
    /// names none. The generic is *read*, not built — built, a PDF asked every
    /// theme for `application-x-generic`, which almost none ships, and fell all
    /// the way to the text icon where `x-office-document` was waiting.
    #[must_use]
    pub fn icon_names(&self, mime: &str) -> [String; 2] {
        let own = self.icons.get(mime).cloned().unwrap_or_else(|| mime.replace('/', "-"));
        let generic = self.generic_icons.get(mime).cloned().unwrap_or_else(|| {
            let media = mime.split_once('/').map_or(mime, |(media, _)| media);
            format!("{media}-x-generic")
        });
        [own, generic]
    }

    /// The `generic-icon` the database names for a type, and only where it names
    /// one — borrowed, because a row's mark asks this every frame.
    #[must_use]
    pub fn generic_icon(&self, mime: &str) -> Option<&str> {
        self.generic_icons.get(mime).map(String::as_str)
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
            // The optional fourth field is flags, separated from the glob by
            // another colon. A glob may not contain one, so splitting again is
            // safe. `cs` means the glob is case-sensitive, which is the only
            // flag the specification defines that changes a lookup.
            let (glob, flags) = glob.split_once(':').unwrap_or((glob, ""));
            let cased = flags.split(',').any(|flag| flag == "cs");
            let Ok(weight) = weight.parse::<u32>() else { continue };
            if glob.is_empty() || mime.is_empty() {
                continue;
            }
            self.insert(weight, mime, glob, cased);
        }
    }

    fn insert(&mut self, weight: u32, mime: &str, glob: &str, cased: bool) {
        // `*.ext` is the overwhelming majority and becomes a hash lookup.
        if let Some(extension) =
            glob.strip_prefix("*.").filter(|rest| !rest.contains(['*', '?', '[']))
        {
            let (map, key) = if cased {
                (&mut self.cased, extension.to_owned())
            } else {
                (&mut self.extensions, extension.to_ascii_lowercase())
            };
            let entry = map.entry(key).or_insert((0, String::new()));
            if entry.1.is_empty() || weight > entry.0 {
                *entry = (weight, mime.to_owned());
            }
            return;
        }
        if !glob.contains(['*', '?', '[']) {
            let (map, key) = if cased {
                (&mut self.cased_literals, glob.to_owned())
            } else {
                (&mut self.literals, glob.to_ascii_lowercase())
            };
            let entry = map.entry(key).or_insert((0, String::new()));
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
        if let Some((_, mime)) = self.cased_literals.get(name) {
            return Some(mime);
        }
        // Borrowed when the name is already lowercase, which is most of them.
        // This runs once per visible row per frame, and an allocation there is
        // forty a frame for nothing.
        let lowered = if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(name.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(name)
        };
        if let Some((_, mime)) = self.literals.get(lowered.as_ref()) {
            return Some(mime);
        }
        // Longest extension first, so `.tar.gz` beats `.gz`. The case-sensitive
        // map is consulted with the name as written and wins outright, because a
        // `cs` glob is the specification saying the case *is* the distinction.
        let lower = lowered.as_ref();
        let mut best: Option<(u32, usize, &str)> = None;
        for (index, _) in name.match_indices('.') {
            let extension = &name[index + 1..];
            if let Some((weight, mime)) = self.cased.get(extension) {
                let candidate = (*weight, extension.len(), mime.as_str());
                if best.is_none_or(|current| candidate > current) {
                    best = Some(candidate);
                }
            }
        }
        if let Some((_, _, mime)) = best {
            return Some(mime);
        }
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
    // **ASCII on both sides is the case that runs**, and it runs once per visible
    // row per frame for every glob that is not `*.ext`. Collecting two `Vec<char>`
    // there cost 0.05 ms of a 0.19 ms frame, measured — so the bytes are used
    // directly when they are the characters, and the allocation happens only for
    // the name with an accent in it.
    if glob.is_ascii() && name.is_ascii() {
        return walk(glob.as_bytes(), name.as_bytes());
    }
    let pattern: Vec<char> = glob.chars().collect();
    let text: Vec<char> = name.chars().collect();
    walk(&pattern, &text)
}

/// The greedy matcher, over bytes or over characters.
///
/// Iterative, with one backtrack point. The obvious recursive form — every `*`
/// trying every split — is exponential: measured, `*a*a…*b` against a string of
/// twenty `a`s took 442 calls with two stars, 49,595 with five and 665,879 with
/// eight, and twelve stars against a sixty-character name did not finish in ten
/// minutes. Both halves of that are attacker-supplied: a `globs2` line is a file
/// anyone can drop into `~/.local/share/mime/`, and the name is a filename. This
/// form is O(pattern × name) and needs no stack.
fn walk<T: Glyph>(pattern: &[T], text: &[T]) -> bool {
    let (mut at, mut index) = (0usize, 0usize);
    // Where to resume from if the greedy star took too much.
    let mut star: Option<(usize, usize)> = None;
    while index < text.len() {
        if let Some(next) = element(pattern, at, text[index]) {
            at = next;
            index += 1;
        } else if pattern.get(at).is_some_and(|it| it.is(b'*')) {
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
    while pattern.get(at).is_some_and(|it| it.is(b'*')) {
        at += 1;
    }
    at == pattern.len()
}

/// One element of a pattern: a byte when both sides are ASCII, a `char` when
/// they are not.
trait Glyph: Copy + PartialEq + PartialOrd {
    /// Whether this is the ASCII character `mark`.
    fn is(self, mark: u8) -> bool;
}

impl Glyph for u8 {
    fn is(self, mark: u8) -> bool {
        self == mark
    }
}

impl Glyph for char {
    fn is(self, mark: u8) -> bool {
        self == char::from(mark)
    }
}

/// Matches one pattern element against one glyph.
///
/// Returns where the pattern continues, or `None` if it did not match — which
/// `*` always is, because a star is not an element and is handled by the caller.
///
/// The bracket rules are `fnmatch`'s, checked against it: an unterminated `[` is
/// a literal `[`, and a `]` immediately after the opening bracket (or after its
/// negation) is a member of the set rather than its end. Both were wrong here,
/// and both are reachable from a `globs2` line.
fn element<T: Glyph>(pattern: &[T], at: usize, glyph: T) -> Option<usize> {
    let here = *pattern.get(at)?;
    if here.is(b'?') {
        return Some(at + 1);
    }
    if here.is(b'*') {
        return None;
    }
    if here.is(b'[') {
        return match class_end(pattern, at) {
            Some(close) => in_class(&pattern[at + 1..close], glyph).then_some(close + 1),
            // No closing bracket anywhere: a literal `[`, which is what
            // `fnmatch` does and what a filename containing one needs.
            None => here.is(b'[').then_some(at + 1).filter(|_| glyph.is(b'[')),
        };
    }
    (here == glyph).then_some(at + 1)
}

/// Where the bracket opened at `at` closes, if it does.
///
/// The first `]` cannot end it if it is the first member: `[]]` is a set holding
/// a right bracket, and `[!]]` is everything except one.
fn class_end<T: Glyph>(pattern: &[T], at: usize) -> Option<usize> {
    let mut cursor = at + 1;
    if pattern.get(cursor).is_some_and(|it| it.is(b'^') || it.is(b'!')) {
        cursor += 1;
    }
    if pattern.get(cursor).is_some_and(|it| it.is(b']')) {
        cursor += 1;
    }
    (cursor..pattern.len()).find(|index| pattern[*index].is(b']'))
}

/// Whether `glyph` is in the set between the brackets.
fn in_class<T: Glyph>(set: &[T], glyph: T) -> bool {
    let (negated, set) = match set.first() {
        Some(first) if first.is(b'^') || first.is(b'!') => (true, &set[1..]),
        _ => (false, set),
    };
    let mut hit = false;
    let mut cursor = 0;
    while cursor < set.len() {
        if cursor + 2 < set.len() && set[cursor + 1].is(b'-') {
            hit |= set[cursor] <= glyph && glyph <= set[cursor + 2];
            cursor += 3;
        } else {
            hit |= set[cursor] == glyph;
            cursor += 1;
        }
    }
    hit != negated
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Category, DIRECTORY, MimeDatabase, TEXT, kind_of, kinds, matches};
    use sk_model::{EntryId, Flags, Listing};
    use std::path::Path;

    /// Every category's `index` is its position in `ALL`, because the band bytes
    /// a sort groups by are indices into it and the labels are read in that order.
    ///
    /// **Two lists that have to agree and no compiler check that they do.** A
    /// category inserted in the middle of `ALL` without its `index` moving files
    /// every archive under "Code".
    #[test]
    fn every_category_index_is_its_place_in_the_list() {
        for (at, category) in Category::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(category.index()), at, "{category:?}");
        }
        assert_eq!(Category::ALL.len(), 9);
        assert_eq!(Category::default(), Category::Unknown, "the default is not the last band");
    }

    /// What a listing's entries are, by the three things that decide it: the
    /// directory bit, the name, and the executable bit for a name that says
    /// nothing.
    #[test]
    fn a_folder_is_a_folder_and_a_nameless_program_is_a_program() {
        let mut listing = Listing::with_capacity(4);
        // A directory whose name looks like a picture is still a directory.
        listing.push("holiday.png", Flags::DIR);
        listing.push("shot.png", Flags::NONE);
        listing.push("configure", Flags::EXEC);
        listing.push("notes", Flags::NONE);
        let mime = MimeDatabase::load();

        assert_eq!(kind_of(&listing, EntryId(0), Some(&mime)), Category::Folder);
        assert_eq!(kind_of(&listing, EntryId(1), Some(&mime)), Category::Image);
        // The mode bit only where the name said nothing: `configure` matches no
        // glob, and half of `/usr/bin` is named like that.
        assert_eq!(kind_of(&listing, EntryId(2), Some(&mime)), Category::Executable);
        assert_eq!(kind_of(&listing, EntryId(3), Some(&mime)), Category::Unknown);

        // Before the databases land there is no answer from the name, and the two
        // that come from flags alone still have one.
        assert_eq!(kind_of(&listing, EntryId(1), None), Category::Unknown);
        assert_eq!(kind_of(&listing, EntryId(0), None), Category::Folder);
        assert_eq!(kind_of(&listing, EntryId(2), None), Category::Executable);

        // And the band bytes are one per entry, in `EntryId` order.
        let bytes = kinds(&listing, Some(&mime));
        assert_eq!(bytes.len(), 4);
        assert_eq!(bytes[0], Category::Folder.index());
        assert_eq!(bytes[1], Category::Image.index());
    }

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
            // `fnmatch`'s bracket rules, checked against `fnmatch` itself: an
            // unterminated `[` is a literal one, and a `]` in the first position
            // is a member of the set rather than its end.
            ("[unclosed", "u", false),
            ("[unclosed", "[unclosed", true),
            ("a[bc", "a[bc", true),
            ("[]]", "]", true),
            ("[]a]", "a", true),
            ("[]a]", "]", true),
            ("[!]]", "x", true),
            ("[!]]", "]", false),
            // A character is a character, not a byte: `?` matched one byte of a
            // multi-byte one, so a name with an accent in it was unreachable.
            ("?.txt", "é.txt", true),
            ("?.txt", "ab.txt", false),
            ("[à-ÿ]x", "éx", true),
            ("[à-ÿ]x", "ax", false),
            ("*é*", "café.txt", true),
        ] {
            assert_eq!(matches(glob, name), wanted, "{glob:?} against {name:?}");
        }
    }

    /// `*.C:cs` is C++ and `*.c:cs` is C, both at weight 50. Folded into one
    /// key, the first line read won — and `/usr/share/mime/globs2` has `*.C` at
    /// line 1118 and `*.c` at 1352, so every C source file on the machine was
    /// typed as C++ and opened with whatever handles C++.
    #[test]
    fn a_case_sensitive_glob_distinguishes_the_case_it_was_written_in() {
        let mut database = MimeDatabase::default();
        database.absorb("50:text/x-c++src:*.C:cs\n50:text/x-csrc:*.c:cs\n");
        assert_eq!(database.by_name("main.c"), Some("text/x-csrc"));
        assert_eq!(database.by_name("main.C"), Some("text/x-c++src"));

        // A glob with no `cs` still folds, which is every other line in the file.
        database.absorb("50:image/png:*.PNG\n");
        assert_eq!(database.by_name("photo.png"), Some("image/png"));
        assert_eq!(database.by_name("photo.PNG"), Some("image/png"));

        // And a case-sensitive glob that does not match falls through to the
        // folded map rather than stopping the search.
        database.absorb("50:text/plain:*.txt\n");
        assert_eq!(database.by_name("notes.TXT"), Some("text/plain"));

        // A whole-filename glob folds too, which is how `Makefile` gets a type:
        // `globs2` spells it `makefile` and does not flag it.
        database.absorb("50:text/x-makefile:makefile\n50:text/x-lisp:MAKEFILE.LISP:cs\n");
        assert_eq!(database.by_name("Makefile"), Some("text/x-makefile"));
        assert_eq!(database.by_name("makefile"), Some("text/x-makefile"));
        assert_eq!(database.by_name("MAKEFILE.LISP"), Some("text/x-lisp"));
        assert_eq!(database.by_name("makefile.lisp"), None, "a cs glob is not folded");
    }

    /// A mark per kind is only useful if the kinds are the ones a person would
    /// draw the same line between. Checked against the types the installed
    /// database actually produces for these names.
    #[test]
    fn a_type_lands_in_the_kind_a_person_would_put_it_in() {
        use super::{Category, category};
        for (mime, wanted) in [
            ("inode/directory", Category::Folder),
            ("image/png", Category::Image),
            ("image/svg+xml", Category::Image),
            ("audio/flac", Category::Audio),
            ("video/x-matroska", Category::Video),
            ("application/zip", Category::Archive),
            ("application/x-compressed-tar", Category::Archive),
            ("application/vnd.debian.binary-package", Category::Archive),
            ("text/x-csrc", Category::Code),
            ("text/x-chdr", Category::Code),
            ("text/x-shellscript", Category::Code),
            ("application/json", Category::Code),
            ("text/html", Category::Code),
            ("text/x-makefile", Category::Code),
            ("text/plain", Category::Document),
            ("application/pdf", Category::Document),
            ("application/vnd.oasis.opendocument.text", Category::Document),
            ("application/x-executable", Category::Executable),
            ("application/x-sharedlib", Category::Executable),
            ("application/octet-stream", Category::Unknown),
            ("application/x-sqlite3", Category::Unknown),
            ("", Category::Unknown),
        ] {
            assert_eq!(category(mime), wanted, "{mime}");
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
        // **Case-insensitive**, and this assertion used to say the opposite.
        // shared-mime-info matches a glob without the `cs` flag without regard
        // to case; the installed `globs2` spells this one `makefile`, so keying
        // it as written gave a file called `Makefile` — which is what every one
        // of them is called — no type at all.
        assert_eq!(database.by_name("makefile"), Some("text/x-makefile"));
        assert_eq!(database.by_name("MAKEFILE"), Some("text/x-makefile"));
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

    /// The question a clipboard asks, which is not the question a row's mark
    /// asks: `text/plain` and `application/pdf` are one `Category` and two
    /// different answers here.
    #[test]
    fn text_is_the_bytes_and_not_the_category() {
        for mime in [
            "text/plain",
            "text/x-rust",
            "text/x-csrc",
            "text/markdown",
            "text/html",
            "application/json",
            "application/x-shellscript",
            "application/xml",
            "application/x-desktop",
            "image/svg+xml",
        ] {
            assert!(super::is_text(mime), "{mime}");
        }
        for mime in [
            "application/pdf",
            "image/png",
            "image/jpeg",
            "audio/flac",
            "video/mp4",
            "application/zip",
            "inode/directory",
            "application/octet-stream",
            "application/x-executable",
            "application/vnd.oasis.opendocument.text",
        ] {
            assert!(!super::is_text(mime), "{mime}");
        }
        // And the two that are one category and two answers.
        assert_eq!(super::category("text/plain"), super::category("application/pdf"));
        assert!(super::is_text("text/plain") && !super::is_text("application/pdf"));
    }
}
