// SPDX-License-Identifier: Apache-2.0
//! Recents: the files this program opened, and the ones the desktop says were.
//!
//! **[ADR 0043](../../../docs/adr/0043-recents-are-two-stores-read-as-one-list-and-ours-is-the-one-rule-6-counts.md)**
//! — two stores read as one list. Ours is a line per file in
//! `$XDG_STATE_HOME/skerry/recents` and is the one rule 6 counts; the desktop's
//! is `$XDG_DATA_HOME/recently-used.xbel`, which this reads and never writes.
//! Everything here is without I/O: the reader for the shared file, the URI
//! spelling `GLib` matches on, our file's format, and the merge.
//!
//! # The reader refuses what `GLib` refuses
//!
//! `GBookmarkFile` rejects a whole document over a duplicate `href`, a
//! `version` other than `1.0`, an XBEL `folder`, an unknown element or
//! attribute, an entity it does not know or a timestamp it cannot read —
//! and the next GTK application to write then replaces the file with one entry
//! of its own. So a document holding any of those is one no desktop on this
//! machine is reading either, and this refuses it whole rather than guessing at
//! the part it understood: a half-read list is indistinguishable from a short
//! one.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// How many of the files this program opened it remembers.
///
/// GTK's own cap on the shared file, so neither list outlives the other by
/// much; and a bound, because a file appended to on every open is a file that
/// grows without one.
pub const KEPT: usize = 1000;

/// What the desktop's recent-files list says about one file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Shared {
    /// The URI exactly as the file spells it, which is what an entry is keyed by.
    pub uri: String,
    /// When it was last opened, in microseconds since 1970.
    pub visited: i64,
    /// Its type, where the entry names one.
    pub mime: Option<String>,
    /// Whether the application that recorded it asked for it not to be shown.
    pub private: bool,
}

/// Why the desktop's file was not read: a sentence for the log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unreadable(pub &'static str);

/// `path` as the `file://` URI `g_filename_to_uri` writes for it, byte for byte.
///
/// **Not [`crate::places::file_uri`]**, which escapes every sub-delimiter and is
/// right for `bookmarks`. A recent is matched on the exact URI string, so
/// `Q&A.txt` spelled with `%26` would be a second entry beside `GLib`'s `&` —
/// shown twice by every GTK application, with neither count ever growing.
#[must_use]
pub fn uri_of(path: &Path) -> String {
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The path a `file://` URI names, as the bytes it names.
///
/// **Bytes and not a string**: a name that is not UTF-8 is spelled `%FF` and has
/// to come back as the byte it was, or it names a file that does not exist.
/// `None` for another scheme, a host other than this one, or a `%00`.
#[must_use]
pub fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let raw = rest.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut at = 0;
    while let Some(&byte) = raw.get(at) {
        if byte == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            let value = u8::from_str_radix(hex, 16).ok()?;
            if value == 0 {
                return None;
            }
            bytes.push(value);
            at += 3;
        } else {
            bytes.push(byte);
            at += 1;
        }
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Reads the desktop's `recently-used.xbel`.
///
/// # Errors
///
/// [`Unreadable`] for a document `GLib` would refuse, or one that ends before its
/// root element closes.
pub fn read_shared(text: &str) -> Result<Vec<Shared>, Unreadable> {
    let mut reader = Reader::default();
    let mut scan = Scan { text, at: 0 };
    while let Some(token) = scan.next()? {
        reader.take(token)?;
    }
    if !reader.closed {
        return Err(Unreadable("the document ends before its root element does"));
    }
    Ok(reader.found)
}

/// Where the shared file's grammar has got to.
#[derive(Default)]
struct Reader {
    /// The elements open, innermost last.
    open: Vec<Element>,
    /// Whether `xmlns:bookmark` and `xmlns:mime` were declared on the root.
    bookmark_ns: bool,
    mime_ns: bool,
    /// The entry being read, while inside its `bookmark`.
    current: Option<Shared>,
    found: Vec<Shared>,
    seen: HashSet<String>,
    /// Whether the root has closed, which is the only way a document is whole.
    closed: bool,
}

/// An element this grammar has, by its local name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Element {
    Xbel,
    Title,
    Desc,
    Bookmark,
    Info,
    /// `metadata` owned by the freedesktop specification, which is ours to read.
    Ours,
    /// `metadata` owned by anybody else: `GLib` accepts one only while it is empty.
    Foreign,
    MimeType,
    Applications,
    Application,
    Groups,
    Group,
    Private,
    Icon,
}

impl Reader {
    fn take(&mut self, token: Token<'_>) -> Result<(), Unreadable> {
        match token {
            Token::Open { name, attributes, empty } => {
                let element = self.opens(name, &attributes)?;
                if empty {
                    self.closes(element);
                } else {
                    self.open.push(element);
                }
            }
            Token::Close(name) => {
                let element =
                    self.open.pop().ok_or(Unreadable("a closing tag with nothing open"))?;
                if local(name) != local_name(element) {
                    return Err(Unreadable("a closing tag that does not match what is open"));
                }
                self.closes(element);
            }
            Token::Text => {}
        }
        Ok(())
    }

    fn opens(&mut self, name: &str, attributes: &[(&str, String)]) -> Result<Element, Unreadable> {
        let (prefix, local) = split_name(name);
        match prefix {
            None => {}
            Some("bookmark") if self.bookmark_ns => {}
            Some("mime") if self.mime_ns => {}
            Some(_) => {
                return Err(Unreadable("an element in a namespace the root did not declare"));
            }
        }
        let parent = self.open.last().copied();
        if self.closed {
            return Err(Unreadable("an element after the root has closed"));
        }
        let element = match (parent, local) {
            (None, "xbel") => {
                if attribute(attributes, "version") != Some("1.0") {
                    return Err(Unreadable("an XBEL version other than 1.0"));
                }
                self.bookmark_ns = attribute(attributes, "xmlns:bookmark").is_some();
                self.mime_ns = attribute(attributes, "xmlns:mime").is_some();
                Element::Xbel
            }
            (None, _) => return Err(Unreadable("a root element that is not xbel")),
            (Some(Element::Xbel), "folder") => {
                return Err(Unreadable("an XBEL folder, which GLib refuses"));
            }
            (Some(Element::Xbel | Element::Bookmark), "title") => Element::Title,
            (Some(Element::Xbel | Element::Bookmark), "desc") => Element::Desc,
            (Some(Element::Xbel), "bookmark") => {
                self.current = Some(bookmark(attributes, &mut self.seen)?);
                Element::Bookmark
            }
            (Some(Element::Bookmark), "info") => Element::Info,
            (Some(Element::Info), "metadata") => {
                let owner = attribute(attributes, "owner").unwrap_or_default();
                if owner.trim_end_matches('/') == "http://freedesktop.org" {
                    Element::Ours
                } else {
                    Element::Foreign
                }
            }
            (Some(Element::Foreign), _) => {
                return Err(Unreadable("a foreign metadata block with something in it"));
            }
            (Some(Element::Ours), "mime-type") => {
                if let Some(entry) = self.current.as_mut() {
                    entry.mime = attribute(attributes, "type").map(str::to_owned);
                }
                Element::MimeType
            }
            (Some(Element::Ours), "applications") => Element::Applications,
            (Some(Element::Applications), "application") => {
                if let Some(stamp) = attribute(attributes, "modified") {
                    instant(stamp)
                        .ok_or(Unreadable("an application timestamp GLib cannot read"))?;
                }
                Element::Application
            }
            (Some(Element::Ours), "groups") => Element::Groups,
            (Some(Element::Groups), "group") => Element::Group,
            (Some(Element::Ours), "private") => {
                if let Some(entry) = self.current.as_mut() {
                    entry.private = true;
                }
                Element::Private
            }
            (Some(Element::Ours), "icon") => Element::Icon,
            _ => return Err(Unreadable("an element this grammar does not have")),
        };
        Ok(element)
    }

    fn closes(&mut self, element: Element) {
        match element {
            Element::Bookmark => {
                if let Some(entry) = self.current.take() {
                    self.found.push(entry);
                }
            }
            Element::Xbel => self.closed = true,
            _ => {}
        }
    }
}

/// A `bookmark` element's attributes, as the entry they begin.
///
/// **Four attributes and no others**, because `GLib` refuses the document over any
/// fifth — and every timestamp that is there has to be one `GLib` can read, for
/// the same reason.
fn bookmark(
    attributes: &[(&str, String)],
    seen: &mut HashSet<String>,
) -> Result<Shared, Unreadable> {
    if attributes
        .iter()
        .any(|(name, _)| !matches!(*name, "href" | "added" | "modified" | "visited"))
    {
        return Err(Unreadable("an attribute on a bookmark that GLib refuses"));
    }
    let uri = attribute(attributes, "href").ok_or(Unreadable("a bookmark with no href"))?;
    if !seen.insert(uri.to_owned()) {
        return Err(Unreadable("two bookmarks with the same href"));
    }
    let when = |name| match attribute(attributes, name) {
        Some(stamp) => instant(stamp).map(Some).ok_or(Unreadable("a timestamp GLib cannot read")),
        None => Ok(None),
    };
    let (visited, modified, added) = (when("visited")?, when("modified")?, when("added")?);
    Ok(Shared {
        uri: uri.to_owned(),
        // The specification says `visited` should be set, and `GLib` does not
        // insist: the latest of the three that are there is the honest answer.
        visited: visited.or(modified).or(added).unwrap_or(0),
        mime: None,
        private: false,
    })
}

fn attribute<'a>(attributes: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    attributes.iter().find(|(key, _)| *key == name).map(|(_, value)| value.as_str())
}

fn split_name(name: &str) -> (Option<&str>, &str) {
    match name.split_once(':') {
        Some((prefix, local)) => (Some(prefix), local),
        None => (None, name),
    }
}

fn local(name: &str) -> &str {
    split_name(name).1
}

const fn local_name(element: Element) -> &'static str {
    match element {
        Element::Xbel => "xbel",
        Element::Title => "title",
        Element::Desc => "desc",
        Element::Bookmark => "bookmark",
        Element::Info => "info",
        Element::Ours | Element::Foreign => "metadata",
        Element::MimeType => "mime-type",
        Element::Applications => "applications",
        Element::Application => "application",
        Element::Groups => "groups",
        Element::Group => "group",
        Element::Private => "private",
        Element::Icon => "icon",
    }
}

/// One piece of the document.
enum Token<'a> {
    /// A start tag, and whether it was also its own end.
    Open { name: &'a str, attributes: Vec<(&'a str, String)>, empty: bool },
    /// An end tag.
    Close(&'a str),
    /// Characters, a comment, a declaration or a CDATA section — nothing this
    /// grammar reads, though its entities are still checked.
    Text,
}

/// A single pass over the document.
struct Scan<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Scan<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>, Unreadable> {
        let rest = &self.text[self.at..];
        if rest.is_empty() {
            return Ok(None);
        }
        if !rest.starts_with('<') {
            let end = rest.find('<').unwrap_or(rest.len());
            decode(&rest[..end])?;
            self.at += end;
            return Ok(Some(Token::Text));
        }
        for (open, close) in [("<?", "?>"), ("<!--", "-->"), ("<![CDATA[", "]]>")] {
            if rest.starts_with(open) {
                let end = rest.find(close).ok_or(Unreadable("an unterminated declaration"))?;
                self.at += end + close.len();
                return Ok(Some(Token::Text));
            }
        }
        if rest.starts_with("<!") {
            // A document type declaration, skipped whole — internal subset and
            // all, as GMarkup skips it. Measured against `GLib` 2.88: a subset that
            // declares an entity is accepted, and it is the entity's *use* that
            // is refused, which `decode` refuses in the same place.
            let first = rest.find('>').ok_or(Unreadable("an unterminated declaration"))?;
            let end = match rest[..first].find('[') {
                Some(open) => {
                    let close =
                        rest[open..].find(']').ok_or(Unreadable("an unterminated subset"))?;
                    let after = open + close;
                    after + rest[after..].find('>').ok_or(Unreadable("an unterminated subset"))?
                }
                None => first,
            };
            self.at += end + 1;
            return Ok(Some(Token::Text));
        }
        let end = tag_end(rest).ok_or(Unreadable("an unterminated tag"))?;
        let inner = &rest[1..end];
        self.at += end + 1;
        if let Some(name) = inner.strip_prefix('/') {
            return Ok(Some(Token::Close(name.trim())));
        }
        let (inner, empty) = match inner.strip_suffix('/') {
            Some(inner) => (inner, true),
            None => (inner, false),
        };
        let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
        let name = &inner[..name_end];
        if name.is_empty() {
            return Err(Unreadable("a tag with no name"));
        }
        Ok(Some(Token::Open { name, attributes: attributes(&inner[name_end..])?, empty }))
    }
}

/// The `>` that ends the tag `rest` starts with, outside any quoted value.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (at, character) in rest.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), close) if open == close => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// `name="value"` pairs, with their values' entities undone.
fn attributes(mut text: &str) -> Result<Vec<(&str, String)>, Unreadable> {
    let mut out: Vec<(&str, String)> = Vec::new();
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return Ok(out);
        }
        let equals = text.find('=').ok_or(Unreadable("an attribute with no value"))?;
        let name = text[..equals].trim();
        let after = text[equals + 1..].trim_start();
        let quote = after.chars().next().filter(|it| matches!(it, '"' | '\''));
        let quote = quote.ok_or(Unreadable("an attribute value that is not quoted"))?;
        let body = &after[1..];
        let close = body.find(quote).ok_or(Unreadable("an unterminated attribute value"))?;
        if out.iter().any(|(seen, _)| *seen == name) {
            return Err(Unreadable("the same attribute twice on one element"));
        }
        out.push((name, decode(&body[..close])?));
        text = &body[close + 1..];
    }
}

/// `text` with the five XML entities and numeric references undone.
///
/// **Undone before the percent-decoding and never after** — the order `GLib`
/// wrote them in reversed. `&amp;` inside an `href` is an `&` in the URI, which
/// is a literal `&` in the file's name.
fn decode(text: &str) -> Result<String, Unreadable> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let semicolon = after.find(';').ok_or(Unreadable("an entity with no semicolon"))?;
        let name = &after[..semicolon];
        let character = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => name.strip_prefix('#').and_then(|it| it.parse().ok()),
                };
                number.and_then(char::from_u32).ok_or(Unreadable("an entity GLib does not know"))?
            }
        };
        out.push(character);
        rest = &after[semicolon + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// An ISO 8601 date and time as microseconds since 1970.
///
/// The form `GLib` writes, `2026-08-27T16:01:47.097173Z`, and the older one with
/// an offset, `2026-08-27T18:01:47+02:00`. Nothing else: `GLib` refuses a
/// document whose timestamp it cannot read, and so does this.
#[must_use]
pub fn instant(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let zone_at = time.find(['Z', '+', '-'])?;
    let (clock, zone) = time.split_at(zone_at);
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock = whole.splitn(3, ':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..=60).contains(&second)
        || !fraction.bytes().all(|it| it.is_ascii_digit())
    {
        return None;
    }
    let micros: i64 = format!("{fraction:0<6}").get(..6)?.parse().ok()?;
    let offset = if zone == "Z" {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
        let (hours, minutes) = (digits.get(..2)?, digits.get(2..4).unwrap_or("00"));
        sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    Some((seconds - offset) * 1_000_000 + micros)
}

/// Days since 1970-01-01 of a proleptic Gregorian date — Howard Hinnant's
/// `days_from_civil`, which has no table and no loop.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let of_year = (153 * shifted + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + of_cycle - 719_468
}

/// Our own list: every file this program opened, and every one it was told to
/// forget.
///
/// **Keyed by the URI in `GLib`'s spelling**, so an entry here and an entry in the
/// shared file are the same entry when they are the same string.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Store {
    /// When each file was last opened here.
    opened: HashMap<String, i64>,
    /// When each file was forgotten — the tombstone a union over two stores
    /// needs, or the next read would bring back every removal.
    forgotten: HashMap<String, i64>,
}

impl Store {
    /// Our file, read leniently: a line this does not understand is skipped and
    /// the others are kept, because the whole file is ours and losing it over one
    /// bad line would lose the list rule 6 is about.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut store = Self::default();
        for line in text.lines() {
            let (forgotten, line) = match line.strip_prefix("- ") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let Some((when, uri)) = line.split_once(' ') else { continue };
            let (Ok(when), true) = (when.parse::<i64>(), uri.starts_with("file://")) else {
                continue;
            };
            let into = if forgotten { &mut store.forgotten } else { &mut store.opened };
            into.insert(uri.to_owned(), when);
        }
        store
    }

    /// Our file, most recent first, so a person reading it reads it in order.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# The files Skerry opened, newest first: microseconds since 1970, then the\n\
             # URI. A line starting `-` is one it was told to forget.\n",
        );
        for (prefix, lines) in [("", &self.opened), ("- ", &self.forgotten)] {
            let mut sorted: Vec<(&String, &i64)> = lines.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            for (uri, when) in sorted {
                let _ = writeln!(out, "{prefix}{when} {uri}");
            }
        }
        out
    }

    /// Records that `uri` was opened at `at`, which spends any tombstone for it:
    /// opening a file again is the one thing that should bring it back.
    pub fn opened(&mut self, uri: String, at: i64) {
        self.forgotten.remove(&uri);
        let when = self.opened.entry(uri).or_insert(at);
        *when = (*when).max(at);
        self.cap();
    }

    /// Forgets `uri` from now on.
    pub fn forget(&mut self, uri: &str, at: i64) {
        self.opened.remove(uri);
        self.forgotten.insert(uri.to_owned(), at);
    }

    /// Drops the tombstones `spent` names, and those for URIs the shared file no
    /// longer has — which could not bring the entry back, so hold nothing.
    pub fn prune(&mut self, shared: &[Shared], spent: &[String]) {
        let present: HashSet<&str> = shared.iter().map(|entry| entry.uri.as_str()).collect();
        self.forgotten.retain(|uri, _| present.contains(uri.as_str()) && !spent.contains(uri));
    }

    /// Whether `uri` was forgotten.
    #[must_use]
    pub fn is_forgotten(&self, uri: &str) -> bool {
        self.forgotten.contains_key(uri)
    }

    fn cap(&mut self) {
        if self.opened.len() <= KEPT {
            return;
        }
        let mut when: Vec<i64> = self.opened.values().copied().collect();
        when.sort_unstable_by(|a, b| b.cmp(a));
        let oldest_kept = when[KEPT - 1];
        self.opened.retain(|_, at| *at >= oldest_kept);
    }
}

/// One row of the recent list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Recent {
    /// The URI both stores key it by.
    pub uri: String,
    /// The file.
    pub path: PathBuf,
    /// When it was last opened, by either store, in microseconds since 1970.
    pub visited: i64,
}

/// The two stores as one list, most recent first, and the tombstones the
/// shared file spent.
///
/// **Filtered the way `recent:///` filters**: not private, a `file:` URI, and
/// not a folder. Whether the file still exists is a `statx`, so it is the
/// caller's, on a job. A tombstone hides a shared entry opened before it and is
/// spent by one opened after, because that means somebody opened the file
/// again after it was forgotten.
#[must_use]
pub fn merge(ours: &Store, shared: &[Shared]) -> (Vec<Recent>, Vec<String>) {
    let mut rows: HashMap<&str, i64> = HashMap::new();
    let mut spent = Vec::new();
    for entry in shared {
        if entry.private || entry.mime.as_deref() == Some("inode/directory") {
            continue;
        }
        if let Some(&forgotten) = ours.forgotten.get(&entry.uri) {
            if entry.visited <= forgotten {
                continue;
            }
            spent.push(entry.uri.clone());
        }
        rows.insert(&entry.uri, entry.visited);
    }
    for (uri, &when) in &ours.opened {
        let at = rows.entry(uri.as_str()).or_insert(when);
        *at = (*at).max(when);
    }
    let mut list: Vec<Recent> = rows
        .into_iter()
        .filter_map(|(uri, visited)| {
            path_of(uri).map(|path| Recent { uri: uri.to_owned(), path, visited })
        })
        .collect();
    list.sort_by(|a, b| b.visited.cmp(&a.visited).then_with(|| a.uri.cmp(&b.uri)));
    (list, spent)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    use super::{Shared, Store, instant, merge, path_of, read_shared, uri_of};

    const GLIB: &str = include_str!("../fixtures/recently-used.glib-2.88.xbel");

    fn shared(uri: &str, visited: i64) -> Shared {
        Shared { uri: uri.to_owned(), visited, mime: Some("text/plain".to_owned()), private: false }
    }

    /// The file `GLib` 2.88 wrote, read back entry for entry — the fixture is its
    /// output and not a document written to agree with this reader.
    #[test]
    fn what_glib_wrote_is_read_back() {
        let entries = read_shared(GLIB).expect("GLib's own output");
        assert_eq!(entries.len(), 5);
        let uris: Vec<&str> = entries.iter().map(|it| it.uri.as_str()).collect();
        assert_eq!(uris[1], "file:///home/tester/Q&A%20'notes'%20%3Cdraft%3E.txt");
        assert_eq!(entries[1].mime.as_deref(), Some("text/plain"));
        assert!(entries[3].private, "`<bookmark:private/>` was not read");
        assert_eq!(entries[4].mime.as_deref(), Some("inode/directory"));
        assert!(entries.iter().all(|it| it.visited > 0), "a visited time was not read");
    }

    /// The spelling `GLib` writes, byte for byte, taken from the fixture: `&` and
    /// `'` survive percent-encoding, and a space, `<` and anything past ASCII do
    /// not.
    #[test]
    fn a_path_is_spelled_the_way_glib_spells_it() {
        assert_eq!(
            uri_of(Path::new("/home/tester/Q&A 'notes' <draft>.txt")),
            "file:///home/tester/Q&A%20'notes'%20%3Cdraft%3E.txt"
        );
        assert_eq!(
            uri_of(Path::new("/home/tester/été 2024/photo.jpg")),
            "file:///home/tester/%C3%A9t%C3%A9%202024/photo.jpg"
        );
        for entry in read_shared(GLIB).unwrap() {
            let path = path_of(&entry.uri).expect("a file URI");
            assert_eq!(uri_of(&path), entry.uri, "{path:?} does not round-trip");
        }
    }

    /// A name that is not UTF-8 comes back as the bytes it was, not as
    /// replacement characters naming a file that does not exist.
    #[test]
    fn a_name_that_is_not_utf8_round_trips_as_bytes() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/caf\xe9".to_vec()));
        let uri = uri_of(&path);
        assert_eq!(uri, "file:///tmp/caf%E9");
        assert_eq!(path_of(&uri), Some(path));
        assert_eq!(path_of("file://localhost/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert_eq!(path_of("file://elsewhere/etc/hosts"), None);
        assert_eq!(path_of("https://example.org/"), None);
        assert_eq!(path_of("file:///tmp/a%00b"), None);
    }

    /// Every document `GLib` refuses, refused whole — never half-read.
    #[test]
    fn a_document_glib_refuses_is_refused_whole() {
        let refused = [
            GLIB.replace("<xbel version=\"1.0\"", "<xbel version=\"1.1\""),
            GLIB.replace("</xbel>", "<folder></folder></xbel>"),
            GLIB.replace(
                "file:///home/tester/secret.txt",
                "file:///home/tester/Documents/report.pdf",
            ),
            GLIB.replace("<bookmark href=", "<bookmark colour=\"red\" href="),
            GLIB.replace("<info>", "<info><metadata owner=\"http://example.org\"><x/></metadata>"),
            GLIB.replace(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<!DOCTYPE x [<!ENTITY h \"h\">]>",
            )
            .replacen("&apos;", "&h;", 1),
            GLIB.replacen("added=\"2026", "added=\"yesterday", 1),
            GLIB.replacen("<mime:mime-type", "<mime:colour/><mime:mime-type", 1),
            GLIB.replacen("&apos;", "&nope;", 1),
            GLIB.replace("</xbel>", ""),
        ];
        for document in &refused {
            assert!(read_shared(document).is_err(), "accepted what GLib refuses:\n{document}");
        }
    }

    /// A declared entity nothing uses is a document `GLib` reads, so it is read:
    /// refusing it would throw away a list every GTK application sees. The *use*
    /// is what both refuse.
    #[test]
    fn an_internal_subset_is_skipped_the_way_glib_skips_it() {
        let declared = GLIB.replace(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
            "<!DOCTYPE x [<!ENTITY h \"h\">]>",
        );
        assert_eq!(read_shared(&declared).expect("GLib reads this").len(), 5);
    }

    /// Both timestamp forms, and the instants they name.
    #[test]
    fn a_timestamp_is_read_to_the_microsecond() {
        assert_eq!(instant("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(instant("2000-03-01T00:00:00Z"), Some(951_868_800_000_000));
        assert_eq!(instant("2026-08-27T16:01:47.097173Z"), Some(1_787_846_507_097_173));
        assert_eq!(instant("2026-08-27T18:01:47+02:00"), instant("2026-08-27T16:01:47Z"));
        assert_eq!(instant("2026-08-27T16:01:47.5Z"), Some(1_787_846_507_500_000));
        assert_eq!(instant("yesterday"), None);
        assert_eq!(instant("2026-13-01T00:00:00Z"), None);
    }

    /// Our file comes back as it was written, and a line it does not understand
    /// costs that line and not the list.
    #[test]
    fn our_store_round_trips_and_survives_a_bad_line() {
        let mut store = Store::default();
        store.opened("file:///a".to_owned(), 20);
        store.opened("file:///b".to_owned(), 10);
        store.forget("file:///c", 30);
        let text = store.render();
        assert_eq!(Store::parse(&text), store);
        let damaged = format!("{text}not a line\n40\nfile:///no-time\n");
        assert_eq!(Store::parse(&damaged), store);
    }

    /// Opening a file again is what brings a forgotten one back.
    #[test]
    fn opening_a_forgotten_file_spends_its_tombstone() {
        let mut store = Store::default();
        store.opened("file:///a".to_owned(), 10);
        store.forget("file:///a", 20);
        assert!(store.is_forgotten("file:///a"));
        store.opened("file:///a".to_owned(), 30);
        assert!(!store.is_forgotten("file:///a"));
        assert_eq!(merge(&store, &[]).0.len(), 1);
    }

    /// The merge: the later time wins, a tombstone hides what was opened before
    /// it and is spent by what was opened after, and the filters are `recent:///`'s.
    #[test]
    fn two_stores_are_one_list_and_a_tombstone_holds_until_the_file_is_opened_again() {
        let mut ours = Store::default();
        ours.opened("file:///both".to_owned(), 50);
        ours.opened("file:///ours".to_owned(), 5);
        ours.forget("file:///forgotten", 100);
        ours.forget("file:///reopened", 100);
        let mut private = shared("file:///private", 70);
        private.private = true;
        let mut folder = shared("file:///folder", 70);
        folder.mime = Some("inode/directory".to_owned());
        let theirs = [
            shared("file:///both", 80),
            shared("file:///forgotten", 90),
            shared("file:///reopened", 110),
            shared("https://example.org/", 120),
            private,
            folder,
        ];

        let (list, spent) = merge(&ours, &theirs);
        let seen: Vec<(&str, i64)> = list.iter().map(|it| (it.uri.as_str(), it.visited)).collect();
        assert_eq!(seen, [("file:///reopened", 110), ("file:///both", 80), ("file:///ours", 5)]);
        assert_eq!(spent, ["file:///reopened"]);

        ours.prune(&theirs, &spent);
        assert!(ours.is_forgotten("file:///forgotten"), "a tombstone still holding was pruned");
        assert!(!ours.is_forgotten("file:///reopened"), "a spent tombstone was kept");
        ours.prune(&[], &[]);
        assert!(!ours.is_forgotten("file:///forgotten"), "a tombstone for nothing was kept");
    }

    /// The bound: the oldest go once there are more than it keeps.
    #[test]
    fn our_list_is_bounded() {
        let mut store = Store::default();
        for at in 0..=super::KEPT {
            store.opened(format!("file:///{at}"), i64::try_from(at).unwrap());
        }
        let (list, _) = merge(&store, &[]);
        assert_eq!(list.len(), super::KEPT);
        assert!(list.iter().all(|it| it.uri != "file:///0"), "the oldest was kept");
    }
}
