// SPDX-License-Identifier: Apache-2.0
//! What is inside an archive, and unpacking one into a directory.
//!
//! **[ADR 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).**
//! Both answers come from `skerry-archive`, spawned once per request, because a
//! zip out of a download directory is the most literal case of the sentence [ADR
//! 0006](../../../docs/adr/0006-out-of-process-decoders.md) is built on. What is
//! here is the framing and the two calls that drive it; the parsing and the
//! writing are the child's.
//!
//! # Why this is not the preview wire
//!
//! [`crate::wire`] is one request and one reply with pixels in it. An unpack
//! reports as it goes and could not be one reply, and a listing is a table
//! rather than an image — so growing a third payload kind onto a header built
//! for pictures would make every reader of that header ask which ask it came
//! from. This is the same little-endian shape and none of the same fields.
//!
//! # Why the child is not long-lived
//!
//! [ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md)
//! keeps a preview child alive because a grid of forty thumbnails is forty
//! requests. An archive is one request per `Space` press and one per extraction,
//! so a process per request costs a `fork` nobody can measure and buys a child
//! whose confinement is chosen for *this* destination.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;

/// `SKAR`, so a child started by hand and fed something else says so at once.
const MAGIC: u32 = 0x534b_4152;

/// The only version this build speaks. Both ends are the same build spawned from
/// the same directory, so a mismatch is a stale binary rather than a negotiation.
const VERSION: u16 = 1;

/// A request's header: magic, version, the ask, and three lengths.
const HEADER: usize = 20;

/// A listing reply's header.
const LISTING_HEADER: usize = 36;

/// One record of an unpack's progress.
const RECORD: usize = 32;

/// The most entries a listing puts on the wire.
///
/// **A hundred thousand, and the counts stay honest past it** — ADR 0032. A tar
/// of a million files is a real thing and a panel draws forty rows; what a
/// bounded table costs is the ability to scroll to the end, and what an
/// unbounded one costs is a reply nobody can allocate.
pub const MOST_ENTRIES: usize = 100_000;

/// The most a listing's table may carry, in bytes.
///
/// Thirty-two megabytes, which is [`MOST_ENTRIES`] at 300 bytes a path. The same
/// kind of bound as the preview wire's pixel ceiling: a length read off a pipe
/// and handed to `Vec::with_capacity` is how a bug becomes an out-of-memory kill.
///
/// **Public because the child has to stop below it.** A bound the host enforces
/// and the child does not know about is a bound that turns a legitimate archive
/// — a hundred thousand entries with long paths — into a listing the host
/// refuses whole. The count and the total stay honest past it, the same way they
/// do past [`MOST_ENTRIES`].
pub const MOST_TABLE: usize = 32 * 1024 * 1024;

/// The most a note may carry. A note is a program's name or a path.
const MOST_NOTE: usize = 4096;

/// What the host is asking for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Ask {
    /// What is inside, as a table of entries.
    List = 1,
    /// Write it into a directory, reporting as it goes.
    Unpack = 2,
}

impl Ask {
    /// The ask a wire word names, if it names one.
    #[must_use]
    pub const fn from_word(word: u16) -> Option<Self> {
        match word {
            1 => Some(Self::List),
            2 => Some(Self::Unpack),
            _ => None,
        }
    }
}

/// The shape of the archive itself, once the magic bytes have been read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Container {
    /// A zip: an index at the end, so a listing costs no decompression.
    Zip = 1,
    /// A tar: 512-byte headers all the way to the end, and no index anywhere.
    Tar = 2,
    /// One file and nothing else: `notes.txt.gz`.
    ///
    /// **A container with no container in it.** A compressed file is what most
    /// people mean by half the archives they meet, and calling it a tar — which
    /// an earlier version of this did — puts `tar.gz` under a name that has no
    /// tar in it.
    One = 3,
}

/// What the container is wrapped in.
///
/// **A compressor is the machine's** — ADR 0032. Gzip is the exception because
/// `flate2` is already in this workspace's helper trees through `png`, so it
/// costs no crate and no process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Filter {
    /// Nothing: the container is the file.
    Store = 0,
    /// Gzip, decoded in the child.
    Gzip = 1,
    /// xz, through `xz -dc`.
    Xz = 2,
    /// Zstandard, through `zstd -dc`.
    Zstd = 3,
    /// bzip2, through `bzip2 -dc`.
    Bzip2 = 4,
    /// Raw LZMA, through `lzma -dc`.
    Lzma = 5,
}

impl Filter {
    /// The program that decodes this, or nothing where the child does it itself.
    #[must_use]
    pub const fn program(self) -> Option<&'static str> {
        match self {
            Self::Store | Self::Gzip => None,
            Self::Xz => Some("xz"),
            Self::Zstd => Some("zstd"),
            Self::Bzip2 => Some("bzip2"),
            Self::Lzma => Some("lzma"),
        }
    }

    /// The filter a wire word names.
    #[must_use]
    pub const fn from_word(word: u16) -> Self {
        match word {
            1 => Self::Gzip,
            2 => Self::Xz,
            3 => Self::Zstd,
            4 => Self::Bzip2,
            5 => Self::Lzma,
            _ => Self::Store,
        }
    }
}

/// What kind of archive this is, for the line that says so.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Format {
    /// Zip or tar.
    pub container: Container,
    /// What it is wrapped in, if anything.
    pub filter: Filter,
}

impl Format {
    /// What it is called, which is the extension people already write.
    ///
    /// **Not a catalogue key.** `tar.xz` is an identifier the way `image/png` is
    /// one: it is the same six characters in every language, and putting it in
    /// the catalogue would be inviting a translation of a file extension.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match (self.container, self.filter) {
            (Container::Zip, _) => "zip",
            (Container::One, Filter::Store) => "file",
            (Container::One, Filter::Gzip) => "gz",
            (Container::One, Filter::Xz) => "xz",
            (Container::One, Filter::Zstd) => "zst",
            (Container::One, Filter::Bzip2) => "bz2",
            (Container::One, Filter::Lzma) => "lzma",
            (Container::Tar, Filter::Store) => "tar",
            (Container::Tar, Filter::Gzip) => "tar.gz",
            (Container::Tar, Filter::Xz) => "tar.xz",
            (Container::Tar, Filter::Zstd) => "tar.zst",
            (Container::Tar, Filter::Bzip2) => "tar.bz2",
            (Container::Tar, Filter::Lzma) => "tar.lzma",
        }
    }
}

/// What one entry in an archive is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// An ordinary file.
    File = 1,
    /// A directory.
    Directory = 2,
    /// A symbolic link.
    Link = 3,
    /// Something else: a device node, a fifo, a socket, a hard link.
    ///
    /// **Listed and never created.** An archive is not a way to make a device
    /// node, and it is certainly not a way to make one this process could not
    /// make itself.
    Other = 4,
}

impl Kind {
    /// The kind a wire byte names.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Self::File,
            2 => Self::Directory,
            3 => Self::Link,
            _ => Self::Other,
        }
    }
}

/// One row of what is inside.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The path inside the archive, `/`-separated, with no leading separator.
    pub path: String,
    /// The unpacked size in bytes. Zero for anything that is not a file.
    pub size: u64,
    /// The modification time the archive recorded, in seconds since the epoch.
    pub mtime: i64,
    /// What it is.
    pub kind: Kind,
}

/// Why there is no answer, or no complete one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Refusal {
    /// Nothing is wrong.
    None = 0,
    /// The magic bytes name nothing this reads.
    NotAnArchive = 1,
    /// It could not be opened.
    Unreadable = 2,
    /// It is that kind and it is malformed.
    Undecodable = 3,
    /// The program that decodes this compressor is not installed.
    NoUnpacker = 4,
    /// The destination filesystem is nearly full.
    NoRoom = 5,
    /// Somebody stopped it.
    Stopped = 6,
    /// The child stopped, or the pipe broke, before it answered.
    Interrupted = 7,
    /// The format is a real archive format that nothing here reads.
    ///
    /// **Not the same as [`Self::NoUnpacker`], and the first version of this
    /// conflated them.** A missing `xz` is a package somebody can install; a 7z
    /// is a format this program does not read, and saying "7z is not installed"
    /// on a machine with 7z on it is a message that sends somebody to install
    /// what they already have.
    Elsewhere = 8,
    /// Some entries could not be created; completed output is retained.
    Partial = 9,
    /// A supported container uses a variant this reader cannot represent safely.
    Unsupported = 10,
}

impl Refusal {
    /// The refusal a wire word names. An unknown word is `Undecodable`, because a
    /// child that reports a reason this build does not know still failed.
    #[must_use]
    pub const fn from_word(word: u16) -> Self {
        match word {
            0 => Self::None,
            1 => Self::NotAnArchive,
            2 => Self::Unreadable,
            4 => Self::NoUnpacker,
            5 => Self::NoRoom,
            6 => Self::Stopped,
            7 => Self::Interrupted,
            8 => Self::Elsewhere,
            9 => Self::Partial,
            10 => Self::Unsupported,
            _ => Self::Undecodable,
        }
    }

    /// The catalogue key that says this to a person, or nothing where there is
    /// nothing to say.
    #[must_use]
    pub const fn message(self) -> Option<&'static str> {
        match self {
            // A file that is not an archive has no listing, the way a text file
            // has no picture: the panel simply shows what it is instead.
            Self::None | Self::NotAnArchive => None,
            Self::Unreadable => Some("archive.unreadable"),
            Self::Undecodable => Some("archive.undecodable"),
            Self::NoUnpacker => Some("archive.no_unpacker"),
            Self::Elsewhere => Some("archive.elsewhere"),
            Self::Partial => Some("archive.partial"),
            Self::Unsupported => Some("archive.unsupported"),
            Self::NoRoom => Some("archive.no_room"),
            Self::Stopped => Some("archive.stopped"),
            Self::Interrupted => Some("archive.interrupted"),
        }
    }
}

/// One question for a child.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// Which of the two.
    pub ask: Ask,
    /// The archive.
    pub archive: PathBuf,
    /// Where an unpack writes. Empty for a listing.
    pub into: PathBuf,
    /// Which entries to unpack, or empty for all of them.
    pub only: Vec<String>,
}

impl Request {
    /// Writes this request to a child's `stdin`.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let archive = self.archive.as_os_str().as_encoded_bytes();
        let into = self.into.as_os_str().as_encoded_bytes();
        let only = self.only.join("\0").into_bytes();
        let mut buffer = Vec::with_capacity(HEADER + archive.len() + into.len() + only.len());
        buffer.extend_from_slice(&MAGIC.to_le_bytes());
        buffer.extend_from_slice(&VERSION.to_le_bytes());
        buffer.extend_from_slice(&(self.ask as u16).to_le_bytes());
        buffer.extend_from_slice(&(archive.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&(into.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&(only.len() as u32).to_le_bytes());
        buffer.extend_from_slice(archive);
        buffer.extend_from_slice(into);
        buffer.extend_from_slice(&only);
        out.write_all(&buffer)?;
        out.flush()
    }

    /// Reads one request, or nothing where the host has closed the pipe.
    pub fn read_from(input: &mut impl Read) -> std::io::Result<Option<Self>> {
        let mut header = [0u8; HEADER];
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if word32(&header, 0) != MAGIC || word16(&header, 4) != VERSION {
            return Err(broken("not a request this build speaks"));
        }
        let Some(ask) = Ask::from_word(word16(&header, 6)) else {
            return Err(broken("no such ask"));
        };
        let archive = take(input, word32(&header, 8) as usize, MOST_NOTE)?;
        let into = take(input, word32(&header, 12) as usize, MOST_NOTE)?;
        let only = take(input, word32(&header, 16) as usize, MOST_TABLE)?;
        Ok(Some(Self {
            ask,
            archive: path_of(archive),
            into: path_of(into),
            only: if only.is_empty() {
                Vec::new()
            } else {
                String::from_utf8_lossy(&only).split('\0').map(str::to_owned).collect()
            },
        }))
    }
}

/// What is inside an archive, as far as the child could say.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Listing {
    /// What kind of archive it is, once it was recognised.
    pub format: Option<Format>,
    /// The first [`MOST_ENTRIES`] of them.
    pub entries: Vec<Entry>,
    /// How many there are, which is not always how many were sent.
    pub count: u64,
    /// What they come to unpacked, in bytes. Complete even where the table is not.
    pub bytes: u64,
    /// Why the answer is missing or short.
    pub refusal: Refusal,
    /// The one detail a message needs: a program's name, usually.
    pub note: String,
}

impl Listing {
    /// A listing that is only a refusal.
    #[must_use]
    pub fn refused(refusal: Refusal, note: String) -> Self {
        Self { format: None, entries: Vec::new(), count: 0, bytes: 0, refusal, note }
    }

    /// Whether the table is shorter than the archive.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.count > self.entries.len() as u64
    }

    /// Writes this listing to `stdout`.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut table = Vec::new();
        for entry in &self.entries {
            table.push(entry.kind as u8);
            table.extend_from_slice(&entry.size.to_le_bytes());
            table.extend_from_slice(&entry.mtime.to_le_bytes());
            table.extend_from_slice(&(entry.path.len() as u32).to_le_bytes());
            table.extend_from_slice(entry.path.as_bytes());
        }
        let note = self.note.as_bytes();
        let (container, filter) =
            self.format.map_or((0, 0), |format| (format.container as u16, format.filter as u16));
        let mut header = Vec::with_capacity(LISTING_HEADER + table.len() + note.len());
        header.extend_from_slice(&MAGIC.to_le_bytes());
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&(self.refusal as u16).to_le_bytes());
        header.extend_from_slice(&container.to_le_bytes());
        header.extend_from_slice(&filter.to_le_bytes());
        header.extend_from_slice(&self.count.to_le_bytes());
        header.extend_from_slice(&self.bytes.to_le_bytes());
        header.extend_from_slice(&(table.len() as u32).to_le_bytes());
        header.extend_from_slice(&(note.len() as u32).to_le_bytes());
        header.extend_from_slice(&table);
        header.extend_from_slice(note);
        out.write_all(&header)?;
        out.flush()
    }

    /// Reads one listing from a child's `stdout`.
    pub fn read_from(input: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; LISTING_HEADER];
        input.read_exact(&mut header)?;
        if word32(&header, 0) != MAGIC {
            return Err(broken("not a listing"));
        }
        let refusal = Refusal::from_word(word16(&header, 6));
        let filter = Filter::from_word(word16(&header, 10));
        let format = match word16(&header, 8) {
            1 => Some(Format { container: Container::Zip, filter }),
            2 => Some(Format { container: Container::Tar, filter }),
            3 => Some(Format { container: Container::One, filter }),
            _ => None,
        };
        let count = word64(&header, 12);
        let bytes = word64(&header, 20);
        let table = take(input, word32(&header, 28) as usize, MOST_TABLE)?;
        let note = take(input, word32(&header, 32) as usize, MOST_NOTE)?;
        Ok(Self {
            format,
            entries: entries_of(&table),
            count,
            bytes,
            refusal,
            note: String::from_utf8_lossy(&note).into_owned(),
        })
    }
}

/// Cuts a table back into entries, stopping at the first one that does not fit.
///
/// **A short read is the end rather than an error.** The table came from our own
/// child over a pipe that either delivered it or did not; a trailing partial
/// record means the child died mid-write, and what arrived is still worth showing.
fn entries_of(table: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 21 <= table.len() {
        let kind = Kind::from_byte(table[at]);
        let size = word64(table, at + 1);
        let mtime = word64(table, at + 9).cast_signed();
        let length = word32(table, at + 17) as usize;
        let from = at + 21;
        if from + length > table.len() {
            break;
        }
        entries.push(Entry {
            path: String::from_utf8_lossy(&table[from..from + length]).into_owned(),
            size,
            mtime,
            kind,
        });
        at = from + length;
    }
    entries
}

/// How far an unpack has got.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    /// Entries written.
    pub entries: u64,
    /// Bytes written.
    pub bytes: u64,
}

/// What an unpack did, once it stopped doing it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Unpacked {
    /// Why it stopped, where it did not finish.
    pub refusal: Refusal,
    /// The one detail the message needs.
    pub note: String,
    /// How much it wrote.
    pub progress: Progress,
}

/// One line from a child in the middle of an unpack.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Record {
    /// Still going, this far in.
    Went(Progress),
    /// Finished, for better or worse.
    Done(Box<Unpacked>),
}

impl Record {
    /// Writes one record to `stdout`.
    pub fn write_to(&self, out: &mut dyn Write) -> std::io::Result<()> {
        let (kind, refusal, progress, note) = match self {
            Self::Went(progress) => (1u16, Refusal::None, *progress, ""),
            Self::Done(done) => (2u16, done.refusal, done.progress, done.note.as_str()),
        };
        let mut buffer = Vec::with_capacity(RECORD + note.len());
        buffer.extend_from_slice(&MAGIC.to_le_bytes());
        buffer.extend_from_slice(&VERSION.to_le_bytes());
        buffer.extend_from_slice(&kind.to_le_bytes());
        buffer.extend_from_slice(&(refusal as u16).to_le_bytes());
        buffer.extend_from_slice(&0u16.to_le_bytes());
        buffer.extend_from_slice(&progress.entries.to_le_bytes());
        buffer.extend_from_slice(&progress.bytes.to_le_bytes());
        buffer.extend_from_slice(&(note.len() as u32).to_le_bytes());
        buffer.extend_from_slice(note.as_bytes());
        out.write_all(&buffer)?;
        out.flush()
    }

    /// Reads one record, or nothing where the child has closed the pipe.
    pub fn read_from(input: &mut impl Read) -> std::io::Result<Option<Self>> {
        let mut header = [0u8; RECORD];
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if word32(&header, 0) != MAGIC {
            return Err(broken("not a record"));
        }
        let refusal = Refusal::from_word(word16(&header, 8));
        let progress = Progress { entries: word64(&header, 12), bytes: word64(&header, 20) };
        let note = take(input, word32(&header, 28) as usize, MOST_NOTE)?;
        Ok(Some(match word16(&header, 6) {
            1 => Self::Went(progress),
            _ => Self::Done(Box::new(Unpacked {
                refusal,
                note: String::from_utf8_lossy(&note).into_owned(),
                progress,
            })),
        }))
    }
}

/// What is inside `archive`.
///
/// **Runs on a job**, and takes as long as the machine's decompressor takes: a
/// compressed tar has no index, so listing one is reading all of it. Nothing here
/// has a deadline, because the thing waiting is a panel somebody can close —
/// ADR 0032.
#[must_use]
pub fn list(archive: &Path, cancel: &sk_util::Cancel) -> Listing {
    let Some(mut child) = spawn() else {
        return Listing::refused(Refusal::Interrupted, "skerry-archive".to_owned());
    };
    let request = Request {
        ask: Ask::List,
        archive: archive.to_path_buf(),
        into: PathBuf::new(),
        only: Vec::new(),
    };
    let mut stopped = false;
    let answer = ask(&mut child, &request, |output| {
        if waited(output.get_ref(), cancel) {
            return Listing::read_from(output);
        }
        stopped = true;
        Ok(Listing::refused(Refusal::Stopped, String::new()))
    });
    // **A cancelled listing takes its child with it.** A panel closed over a
    // four-gigabyte `tar.xz` would otherwise leave an `xz` decompressing the
    // rest of it into a pipe nobody will ever read.
    if stopped || cancel.is_cancelled() {
        let _ = child.kill();
    }
    let _ = child.wait();
    answer.unwrap_or_else(|| Listing::refused(Refusal::Interrupted, String::new()))
}

/// Whether there is a record to read, waiting for one unless the caller has
/// stopped caring.
///
/// **The buffer first.** A `BufReader` fills 8 KiB at a time, so several records
/// arrive in one read and `poll` would report nothing readable while two of them
/// were already in hand — a hundred milliseconds of waiting per record, on a
/// pipe that had nothing left to deliver.
fn ready(
    output: &mut std::io::BufReader<std::process::ChildStdout>,
    cancel: &sk_util::Cancel,
) -> bool {
    !output.buffer().is_empty() || waited(output.get_ref(), cancel)
}

/// Waits until the child has something to say, or until the caller stops caring.
///
/// **A tenth of a second at a time.** The one read that can take seconds is the
/// first — a compressed tar has no index, so the child says nothing until it has
/// walked the whole archive — and everything after it is already in the pipe. So
/// this is the only place a cancel can land, and polling it costs ten wake-ups a
/// second on a thread that would otherwise be blocked.
fn waited(output: &std::process::ChildStdout, cancel: &sk_util::Cancel) -> bool {
    use std::os::fd::AsFd;
    let pipe = output.as_fd();
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        let mut waiting = [rustix::event::PollFd::new(&pipe, rustix::event::PollFlags::IN)];
        let tenth = rustix::event::Timespec { tv_sec: 0, tv_nsec: 100_000_000 };
        // Anything but a timeout means go and read: ready, a closed pipe — which
        // the read reports as the interruption it is — or a `poll` that failed,
        // where blocking on the read is a better answer than giving up here.
        if rustix::event::poll(&mut waiting, Some(&tenth)) != Ok(0) {
            return true;
        }
    }
}

/// Unpacks `archive` into `into`, reporting as it goes.
///
/// `only` names the entries to write, or is empty for every one of them.
///
/// **Runs on a job.** `watch` is called from this thread with each record the
/// child sends, which is what makes the pause latch and the status line work the
/// way they do for a copy: a `watch` that blocks blocks the read, and a child
/// whose pipe is full stops writing.
#[must_use]
pub fn unpack(
    archive: &Path,
    into: &Path,
    only: &[String],
    cancel: &sk_util::Cancel,
    watch: &mut dyn FnMut(Progress),
) -> Unpacked {
    let interrupted = |note: &str| Unpacked {
        refusal: Refusal::Interrupted,
        note: note.to_owned(),
        progress: Progress::default(),
    };
    let Some(mut child) = spawn() else {
        return interrupted("skerry-archive");
    };
    let request = Request {
        ask: Ask::Unpack,
        archive: archive.to_path_buf(),
        into: into.to_path_buf(),
        only: only.to_vec(),
    };
    let answer = ask(&mut child, &request, |output| {
        let mut last = Progress::default();
        loop {
            // **Waited for rather than blocked on**, so a Stop lands inside the
            // hundred milliseconds rather than when the child next has something
            // to say. A single four-gigabyte entry is one record's worth of
            // silence, and without this the Cancel button would do nothing
            // visible until it had been written.
            if !ready(output, cancel) {
                return Ok(Unpacked {
                    refusal: Refusal::Stopped,
                    note: String::new(),
                    progress: last,
                });
            }
            match Record::read_from(output) {
                Ok(Some(Record::Went(progress))) => {
                    last = progress;
                    watch(progress);
                    if cancel.is_cancelled() {
                        return Ok(Unpacked {
                            refusal: Refusal::Stopped,
                            note: String::new(),
                            progress,
                        });
                    }
                }
                Ok(Some(Record::Done(done))) => return Ok(*done),
                // The child died without a final record. What it wrote is still
                // on disk, so the count comes back with the reason: an unpack
                // that reported nothing would leave files nothing describes.
                Ok(None) | Err(_) => {
                    return Ok(Unpacked {
                        refusal: Refusal::Interrupted,
                        note: String::new(),
                        progress: last,
                    });
                }
            }
        }
    });
    // A cancelled unpack is a child still writing files. It goes now rather than
    // when it finishes the archive.
    if cancel.is_cancelled() {
        let _ = child.kill();
    }
    let _ = child.wait();
    answer.unwrap_or_else(|| interrupted(""))
}

/// Writes a request, then reads the answer with `read`.
fn ask<T>(
    child: &mut Child,
    request: &Request,
    read: impl FnOnce(&mut std::io::BufReader<std::process::ChildStdout>) -> std::io::Result<T>,
) -> Option<T> {
    let mut input = child.stdin.take()?;
    let output = child.stdout.take()?;
    request.write_to(&mut input).ok()?;
    // **Closed before the answer is read.** The child reads until end of file to
    // know the request is whole, so a host still holding the pipe is a host and
    // a child both waiting for the other one.
    drop(input);
    let mut reading = std::io::BufReader::new(output);
    read(&mut reading).ok()
}

/// Starts the helper, from beside this binary and then from `PATH`.
///
/// **Beside first**, for the reason [`crate::host`] gives: a development build
/// and an installed one are different binaries, and a version skew is a protocol
/// error rather than something to survive.
fn spawn() -> Option<Child> {
    crate::host::helper("skerry-archive", beside_us().as_deref())
}

/// The directory this binary is in, if the kernel will say.
fn beside_us() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Reads `length` bytes, refusing a length no honest child would send.
fn take(input: &mut impl Read, length: usize, most: usize) -> std::io::Result<Vec<u8>> {
    if length > most {
        return Err(broken("a length past what this build allows"));
    }
    let mut bytes = vec![0u8; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// A path from raw bytes, because a path is not text.
fn path_of(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

fn broken(why: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why)
}

fn word16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn word32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn word64(bytes: &[u8], at: usize) -> u64 {
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(eight)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Every field, out and back, because a framing bug is a header read at the
    /// wrong offset and every field after it is silently something else.
    #[test]
    fn a_request_survives_the_wire() {
        let request = Request {
            ask: Ask::Unpack,
            archive: PathBuf::from("/home/somebody/a file.tar.zst"),
            into: PathBuf::from("/home/somebody/a file"),
            only: vec!["one/two.txt".to_owned(), "three".to_owned()],
        };
        let mut bytes = Vec::new();
        request.write_to(&mut bytes).unwrap();
        let read = Request::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(read, Some(request));
    }

    /// A request with no `only` comes back with no `only`, rather than with one
    /// empty string in it — which would be an unpack that matched nothing.
    #[test]
    fn everything_is_not_one_entry_called_nothing() {
        let request = Request {
            ask: Ask::List,
            archive: PathBuf::from("/x.zip"),
            into: PathBuf::new(),
            only: Vec::new(),
        };
        let mut bytes = Vec::new();
        request.write_to(&mut bytes).unwrap();
        let read = Request::read_from(&mut bytes.as_slice()).unwrap().unwrap();
        assert!(read.only.is_empty(), "{:?}", read.only);
    }

    /// A listing with a table, a count past it, and a format.
    #[test]
    fn a_listing_survives_the_wire() {
        let listing = Listing {
            format: Some(Format { container: Container::Tar, filter: Filter::Xz }),
            entries: vec![
                Entry {
                    path: "a/b.txt".to_owned(),
                    size: 12,
                    mtime: 1_700_000_000,
                    kind: Kind::File,
                },
                Entry { path: "a".to_owned(), size: 0, mtime: -1, kind: Kind::Directory },
            ],
            count: 9_000,
            bytes: 123_456_789,
            refusal: Refusal::None,
            note: String::new(),
        };
        let mut bytes = Vec::new();
        listing.write_to(&mut bytes).unwrap();
        let read = Listing::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(read, listing);
        assert!(read.truncated(), "nine thousand entries in a table of two is truncated");
        assert_eq!(read.format.map(Format::name), Some("tar.xz"));
    }

    /// A refusal carries the one word its message needs.
    #[test]
    fn a_refusal_carries_the_program_that_would_have_answered() {
        let listing = Listing::refused(Refusal::NoUnpacker, "7z".to_owned());
        let mut bytes = Vec::new();
        listing.write_to(&mut bytes).unwrap();
        let read = Listing::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(read.refusal, Refusal::NoUnpacker);
        assert_eq!(read.note, "7z");
        assert!(read.format.is_none());
    }

    /// Progress and the final word share a header and are told apart by one field.
    #[test]
    fn a_record_says_which_of_the_two_it_is() {
        let went = Record::Went(Progress { entries: 40, bytes: 900 });
        let mut bytes = Vec::new();
        went.write_to(&mut bytes).unwrap();
        Record::Done(Box::new(Unpacked {
            refusal: Refusal::NoRoom,
            note: "big/file.iso".to_owned(),
            progress: Progress { entries: 41, bytes: 1_000 },
        }))
        .write_to(&mut bytes)
        .unwrap();

        let mut reading = bytes.as_slice();
        assert_eq!(Record::read_from(&mut reading).unwrap(), Some(went));
        let Some(Record::Done(done)) = Record::read_from(&mut reading).unwrap() else {
            panic!("the second record was not the last word");
        };
        assert_eq!(done.refusal, Refusal::NoRoom);
        assert_eq!(done.note, "big/file.iso");
        assert_eq!(done.progress.entries, 41);
        assert_eq!(Record::read_from(&mut reading).unwrap(), None);
    }

    /// A length no honest child would send is refused rather than allocated.
    #[test]
    fn a_length_past_the_ceiling_is_not_allocated() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(Ask::List as u16).to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(Request::read_from(&mut bytes.as_slice()).is_err());
    }
}
