// SPDX-License-Identifier: Apache-2.0
//! What kind of archive a file is, from its first bytes and never its name.
//!
//! A file manager that trusted `.zip` would be wrong about every file somebody
//! renamed, and the mark beside the name already comes from the MIME database —
//! so what decides *how to read* it is the only thing that has the bytes.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use sk_preview::archive::{Container, Filter, Format, Refusal};

/// How much is read before anything is decided.
///
/// **512, because that is a tar header.** The magic that says "tar" is 257 bytes
/// in, and the checksum that says so when the magic is absent covers the whole
/// block.
const BLOCK: usize = 512;

/// The memory a threaded `xz` is allowed, and the reason it is told at all.
///
/// **332 MB of RSS at this figure and 1.5 GB without it** — ADR 0032 — and the
/// unbounded case does not run under the sandbox's address-space cap. `-T0` picks
/// the thread count that fits inside it, which is eleven times a single thread on
/// the reference machine.
const XZ_MEMORY: &str = "-M256MiB";

/// What was opened, and what to read it with.
pub enum Opened {
    /// A zip. Read by seeking, because a zip has an index at the end.
    Zip(File),
    /// A tar, behind nothing or behind a filter.
    Tar {
        /// What to call it.
        format: Format,
        /// The container's bytes, with the filter already applied.
        bytes: Bytes,
    },
    /// One compressed file that is not a tar: `notes.txt.gz`.
    ///
    /// **An archive of one entry**, which is what a person means by it: the panel
    /// shows the one name it would unpack to, and the size is exact because
    /// counting it is reading it, which is what listing already does.
    Single {
        /// What to call it.
        format: Format,
        /// The name the one entry gets, which is this file's without the suffix.
        name: String,
        /// Its bytes.
        bytes: Bytes,
    },
    /// An archive a person would recognise, named by its format.
    Elsewhere(&'static str),
    /// Nothing this knows about.
    Nothing,
}

/// Opens `path` and says what it holds.
pub fn open(path: &Path) -> Result<Opened, Refusal> {
    let mut file = File::open(path).map_err(|_| Refusal::Unreadable)?;
    let mut head = vec![0u8; BLOCK];
    let mut filled = 0;
    while filled < BLOCK {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(_) => return Err(Refusal::Unreadable),
        }
    }
    head.truncate(filled);

    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Ok(Opened::Zip(file));
    }
    if let Some(format) = elsewhere(&head) {
        return Ok(Opened::Elsewhere(format));
    }
    if looks_like_tar(&head) {
        let format = Format { container: Container::Tar, filter: Filter::Store };
        return Ok(Opened::Tar { format, bytes: Bytes::whole(head, file) });
    }
    let Some(filter) = filter_of(&head) else {
        // **Last, after everything else has been ruled out.** A zip is defined
        // from its *end*, so one with anything in front of it — a self-extracting
        // stub, or a download that arrived with a prefix — has no `PK\x03\x04` at
        // offset zero and is still a zip. Go's own `archive/zip` corpus keeps one
        // of these, and it was the only file in those thirty this refused.
        return Ok(if index_at_the_end(&mut file) { Opened::Zip(file) } else { Opened::Nothing });
    };
    // From the top: the filter reads the file itself, and the bytes already read
    // are the file's rather than the container's.
    let mut bytes = through(path, filter)?;
    let inside = bytes.peek(BLOCK).map_err(|_| Refusal::Undecodable)?;
    if looks_like_tar(inside) {
        let format = Format { container: Container::Tar, filter };
        return Ok(Opened::Tar { format, bytes });
    }
    let format = Format { container: Container::One, filter };
    Ok(Opened::Single { format, name: unsuffixed(path), bytes })
}

/// The archive formats that are somebody else's, by magic.
///
/// **Named rather than ignored** — ADR 0032. A 7z that reported "not an archive"
/// would be this program calling a file something it plainly is not; what the
/// panel says instead names the format, so the sentence is true whether or not
/// the machine has a program for it.
fn elsewhere(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some("7z");
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return Some("rar");
    }
    head.starts_with(b"MSCF").then_some("cab")
}

/// Whether a zip's own index is at the end of this file.
///
/// The end-of-directory record is 22 bytes plus a comment of at most 65,535, so
/// the whole space it can be in is the tail this reads.
fn index_at_the_end(file: &mut File) -> bool {
    use std::io::{Seek, SeekFrom};
    const TAIL: u64 = 22 + 65_535;
    let Ok(length) = file.metadata().map(|facts| facts.len()) else { return false };
    let from = length.saturating_sub(TAIL);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    if std::io::Read::read_to_end(file, &mut tail).is_err() {
        return false;
    }
    tail.windows(4).any(|window| window == b"PK\x05\x06")
}

/// The filter a file's first bytes name, if they name one.
fn filter_of(head: &[u8]) -> Option<Filter> {
    if head.starts_with(b"\x1f\x8b") {
        return Some(Filter::Gzip);
    }
    if head.starts_with(b"\xfd7zXZ\x00") {
        return Some(Filter::Xz);
    }
    if head.starts_with(b"\x28\xb5\x2f\xfd") {
        return Some(Filter::Zstd);
    }
    if head.starts_with(b"BZh") {
        return Some(Filter::Bzip2);
    }
    // Raw LZMA has no magic: a properties byte, then a four-byte dictionary size.
    // `0x5d` is what every encoder in practice writes, and the dictionary size is
    // a power of two under 2 GiB — which is three bytes and a zero. It is the
    // weakest test here, and it is last for that reason.
    (head.starts_with(b"\x5d") && head.get(4) == Some(&0)).then_some(Filter::Lzma)
}

/// Whether a 512-byte block is a tar header.
///
/// **The magic, or the checksum.** `ustar` covers POSIX and GNU tars, which is
/// everything made this century; the checksum is what recognises the v7 tars that
/// still come out of embedded toolchains, and it is the field the format defines
/// for exactly this purpose.
fn looks_like_tar(block: &[u8]) -> bool {
    if block.len() < BLOCK {
        return false;
    }
    if &block[257..262] == b"ustar" {
        return true;
    }
    let Some(claimed) = octal(&block[148..156]) else { return false };
    // The checksum field is counted as eight spaces, which is the one rule that
    // makes the sum computable at all.
    let sum: u32 = block
        .iter()
        .enumerate()
        .map(|(at, byte)| if (148..156).contains(&at) { 32 } else { u32::from(*byte) })
        .sum();
    claimed != 0 && claimed == u64::from(sum)
}

/// An octal field, which ends at the first byte that is not a digit.
pub fn octal(field: &[u8]) -> Option<u64> {
    // GNU's base-256 encoding for a number too big for the field: the high bit of
    // the first byte is set and the rest is big-endian.
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Some(field[1..].iter().fold(0u64, |value, byte| (value << 8) | u64::from(*byte)));
    }
    let digits = field.iter().take_while(|byte| (b'0'..=b'7').contains(byte));
    let mut value = 0u64;
    let mut any = false;
    for digit in digits {
        any = true;
        value = value.checked_mul(8)?.checked_add(u64::from(digit - b'0'))?;
    }
    // An empty field is zero rather than nothing: tar writes one for a directory's
    // size, and refusing it would refuse every directory.
    Some(if any { value } else { 0 })
}

/// The name a single compressed file unpacks to.
///
/// `notes.txt.gz` is `notes.txt`; a file with no suffix to drop gets `.out`
/// appended, because a file that unpacked onto its own name would refuse itself.
fn unsuffixed(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for suffix in [".gz", ".xz", ".zst", ".bz2", ".lzma", ".tgz", ".txz", ".tzst", ".tbz2"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            return stem.to_owned();
        }
    }
    format!("{name}.out")
}

/// Starts the program that decodes `filter`, reading `path`.
fn through(path: &Path, filter: Filter) -> Result<Bytes, Refusal> {
    let file = File::open(path).map_err(|_| Refusal::Unreadable)?;
    let Some(program) = filter.program() else {
        return Ok(Bytes::inflated(file, filter));
    };
    // **`xz` reads `.lzma` too**, so the program is the same and only the message
    // that names it would differ.
    let program = if filter == Filter::Lzma { "xz" } else { program };
    let mut command = Command::new(program);
    command.arg("-dc");
    if program == "xz" {
        command.args(["-T0", XZ_MEMORY]);
    }
    let mut child = command
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| Refusal::NoUnpacker)?;
    let out = child.stdout.take().ok_or(Refusal::NoUnpacker)?;
    Ok(Bytes { head: Vec::new(), at: 0, rest: Rest::Piped(Box::new(child), out) })
}

/// The container's bytes, with whatever was peeked at the front.
pub struct Bytes {
    head: Vec<u8>,
    at: usize,
    rest: Rest,
}

enum Rest {
    Plain(File),
    Gzip(Box<flate2::read::MultiGzDecoder<File>>),
    Piped(Box<Child>, ChildStdout),
}

impl Bytes {
    /// The file itself, with the block already read put back in front of it.
    fn whole(head: Vec<u8>, file: File) -> Self {
        Self { head, at: 0, rest: Rest::Plain(file) }
    }

    /// A gzip stream, decoded here rather than by a program.
    ///
    /// **Multi-member**, because concatenating two `.gz` files is a valid gzip
    /// and `cat a.gz b.gz > both.gz` is how some tools write one. A single-member
    /// decoder stops silently at the first end of stream, which is a listing that
    /// is short and says nothing.
    fn inflated(file: File, filter: Filter) -> Self {
        debug_assert!(matches!(filter, Filter::Gzip));
        Self {
            head: Vec::new(),
            at: 0,
            rest: Rest::Gzip(Box::new(flate2::read::MultiGzDecoder::new(file))),
        }
    }

    /// Reads ahead to `want` bytes without consuming them.
    pub fn peek(&mut self, want: usize) -> std::io::Result<&[u8]> {
        while self.head.len() - self.at < want {
            let mut chunk = [0u8; BLOCK];
            let read = self.source(&mut chunk)?;
            if read == 0 {
                break;
            }
            self.head.extend_from_slice(&chunk[..read]);
        }
        Ok(&self.head[self.at..])
    }

    fn source(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.rest {
            Rest::Plain(file) => file.read(into),
            Rest::Gzip(gzip) => gzip.read(into),
            Rest::Piped(_, out) => out.read(into),
        }
    }
}

impl Read for Bytes {
    fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
        let waiting = self.head.len() - self.at;
        if waiting > 0 {
            let take = waiting.min(into.len());
            into[..take].copy_from_slice(&self.head[self.at..self.at + take]);
            self.at += take;
            if self.at == self.head.len() {
                self.head.clear();
                self.at = 0;
            }
            return Ok(take);
        }
        self.source(into)
    }
}

impl Drop for Bytes {
    /// **A filter that is still running goes with the request that started it.**
    /// A listing stops reading the moment it has [`MOST_ENTRIES`], and the `xz`
    /// behind it would otherwise decompress the rest of a four-gigabyte archive
    /// into a pipe nobody drains — for as long as the child lives, which is
    /// exactly as long as it takes to notice.
    ///
    /// [`MOST_ENTRIES`]: sk_preview::archive::MOST_ENTRIES
    fn drop(&mut self) {
        if let Rest::Piped(child, _) = &mut self.rest {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
