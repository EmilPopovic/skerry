// SPDX-License-Identifier: MIT OR Apache-2.0
//! The bytes that cross between the host and a preview child.
//!
//! **[ADR 0019](../../../docs/adr/0019-a-preview-request-names-a-page-and-the-child-stays-stateless.md).**
//! A child is a long-lived filter: length-prefixed requests in on `stdin`,
//! length-prefixed replies out on `stdout`, in order, one in flight at a time.
//! Ordering is the only correlation mechanism, which is why there are no request
//! ids here and why a child that answers out of order has violated the protocol
//! rather than confused the host.
//!
//! **Fixed twenty-four-byte headers and a payload.** Not a text format: the
//! payload is a quarter of a megabyte of pixels and every byte of framing around
//! it is framing somebody has to parse. Not a serialization crate either — seven
//! integers and a byte string is not a dependency's worth of problem, and this
//! crate's whole point is that the host's graph does not grow.
//!
//! Every field is little-endian. The two ends are the same build on the same
//! machine — a helper is spawned from the binary's own directory — so this is a
//! pipe between two processes rather than a format anything else reads.

use std::io::{Read, Write};

/// `SKPR`, so a child started by hand and fed something else says so at once.
const MAGIC: u32 = 0x534b_5052;

/// The only version this build speaks.
///
/// A mismatch is a fatal protocol error rather than a negotiation: the host
/// spawns the helper out of its own directory, so two versions in one
/// conversation means somebody has a stale binary and the useful thing to do is
/// say which.
///
/// **Four, since a page reply grew its words.** Both ends are the same build
/// spawned from the same directory, so this is a recompile and not a migration —
/// the number moves so that a stale helper says which side is old instead of
/// misparsing a header.
const VERSION: u16 = 4;

/// A request's header: magic, version, the ask, a box, a page and a length.
const HEADER: usize = 24;

/// A reply's, which is eight bytes longer: the frame delay beside the page
/// count, and the length of the text layer that follows the pixels.
///
/// **Two constants rather than one**, because the two headers stopped being the
/// same size the moment one of them grew: a shared `HEADER` made a request's
/// reader ask for four bytes that were the *path's* first four, which is a parse
/// that succeeds and is wrong.
const REPLY_HEADER: usize = 32;

/// The most pixels a reply may carry, in bytes.
///
/// Sixty-four megabytes — a 4096×4096 RGBA image. It bounds what a child can make
/// the host allocate by lying about a length, which is the one thing a hostile
/// *child* could do to a host that trusts it. The child is ours, so this is a
/// bound against a bug rather than against an attacker; it is here because a
/// length read off a pipe and handed to `Vec::with_capacity` is how a bug becomes
/// an out-of-memory kill.
const MOST_PIXELS: usize = 64 * 1024 * 1024;

/// The most a page's text layer may carry, in bytes.
///
/// Four megabytes. A word record is twenty-two bytes and its text, so this is
/// something like a hundred thousand words on one page — two orders of magnitude
/// past the densest page measured, and the same kind of bound as
/// [`MOST_PIXELS`]: a length read off a pipe is not a promise.
const MOST_WORDS: usize = 4 * 1024 * 1024;

/// What the host is asking for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Ask {
    /// A thumbnail at the cache's size, for the atlas.
    ///
    /// The child may answer from the XDG cache without decoding the original,
    /// and the host neither knows nor cares which happened.
    Thumbnail = 1,
    /// One image as large as the panel that will draw it, not cached.
    Preview = 2,
}

impl Ask {
    /// The ask a wire word names, if it names one.
    #[must_use]
    pub const fn from_word(word: u16) -> Option<Self> {
        match word {
            1 => Some(Self::Thumbnail),
            2 => Some(Self::Preview),
            _ => None,
        }
    }
}

/// Why a child could not answer with pixels.
///
/// **A status rather than a dead child.** Each of these is an ordinary outcome the
/// host turns into a message or a shrug; only a crash, a hang or a protocol
/// violation costs the child itself — ADR 0017.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum Refusal {
    /// Nothing is wrong: pixels follow.
    None = 0,
    /// The file is not something this child decodes.
    NotItsKind = 1,
    /// The file could not be opened or read.
    Unreadable = 2,
    /// It is that kind and it is malformed.
    Undecodable = 3,
    /// The renderer this child drives is not installed.
    NoRenderer = 4,
    /// It would need more memory than the child is allowed.
    TooLarge = 5,
    /// The helper stopped, or the pipe broke, before it answered.
    ///
    /// **Not the file's fault and not silence either.** A child that dies
    /// mid-request used to leave the host with nothing at all — no pixels, no
    /// reason — and a panel that had already recorded what it asked for would
    /// never ask again: blank, with a title and a page number, until the window
    /// was resized. Rule 7 says what is in force, and "the helper did not answer"
    /// is a thing that is in force.
    Interrupted = 7,
    /// The document has no such page.
    ///
    /// **Not a message**, because the only way to ask for one is to press `↓` on
    /// the last page, and a panel that answered that with a sentence would be
    /// scolding somebody for reaching the end. The host keeps the page it has.
    NoSuchPage = 6,
}

impl Refusal {
    /// The refusal a wire word names. An unknown word is `Undecodable`, because a
    /// child that reports a reason this build does not know still failed.
    #[must_use]
    pub const fn from_word(word: u16) -> Self {
        match word {
            0 => Self::None,
            1 => Self::NotItsKind,
            2 => Self::Unreadable,
            4 => Self::NoRenderer,
            5 => Self::TooLarge,
            6 => Self::NoSuchPage,
            7 => Self::Interrupted,
            _ => Self::Undecodable,
        }
    }

    /// The catalogue key that says this to a person, or nothing where there is
    /// nothing to say.
    #[must_use]
    pub const fn message(self) -> Option<&'static str> {
        match self {
            // Not a message: a file that is not an image simply has no preview,
            // and a panel saying so about every text file would be furniture.
            Self::None | Self::NotItsKind | Self::NoSuchPage => None,
            Self::Unreadable => Some("preview.unreadable"),
            Self::Undecodable => Some("preview.undecodable"),
            Self::NoRenderer => Some("preview.no_renderer"),
            Self::Interrupted => Some("preview.interrupted"),
            Self::TooLarge => Some("preview.too_large"),
        }
    }
}

/// One question for a child.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    /// Which of the two shapes of answer is wanted.
    pub ask: Ask,
    /// The largest the answer may be, in device pixels. The child fits the image
    /// inside this box and keeps its aspect ratio.
    pub within: (u32, u32),
    /// Which page, one-based, or zero for "whatever the first one is".
    ///
    /// **Zero rather than one as the don't-care value** — ADR 0019 — so every
    /// caller that has no pages to think about, which is every image and every
    /// thumbnail, sends a default and never learns the field exists.
    pub page: u32,
    /// The file, as raw bytes, because a path is not text.
    pub path: Vec<u8>,
}

impl Request {
    /// Writes this request to a child's `stdin`.
    ///
    /// # Errors
    ///
    /// Whatever the pipe says. A closed pipe is a child that has died, which the
    /// caller answers by respawning rather than by reporting.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut header = Vec::with_capacity(HEADER + self.path.len());
        header.extend_from_slice(&MAGIC.to_le_bytes());
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&(self.ask as u16).to_le_bytes());
        header.extend_from_slice(&self.within.0.to_le_bytes());
        header.extend_from_slice(&self.within.1.to_le_bytes());
        header.extend_from_slice(&self.page.to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.path.len()).unwrap_or(0).to_le_bytes());
        header.extend_from_slice(&self.path);
        out.write_all(&header)?;
        out.flush()
    }

    /// Reads a request from the host, in a child.
    ///
    /// # Errors
    ///
    /// A closed pipe means the host has gone and the child should end. Anything
    /// else is a protocol violation, which is also an ending.
    pub fn read_from(input: &mut impl Read) -> std::io::Result<Self> {
        let head = exactly::<HEADER>(input)?;
        let (magic, version) = (word32(&head, 0), word16(&head, 4));
        if magic != MAGIC || version != VERSION {
            return Err(broken(&format!("magic {magic:#x} version {version}")));
        }
        let ask = Ask::from_word(word16(&head, 6))
            .ok_or_else(|| broken(&format!("ask {}", word16(&head, 6))))?;
        let within = (word32(&head, 8), word32(&head, 12));
        let page = word32(&head, 16);
        let len = word32(&head, 20) as usize;
        if len > 4096 {
            return Err(broken(&format!("a path of {len} bytes")));
        }
        let mut path = vec![0u8; len];
        input.read_exact(&mut path)?;
        Ok(Self { ask, within, page, path })
    }
}

/// What a child answers with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    /// Why there are no pixels, or [`Refusal::None`].
    pub refusal: Refusal,
    /// The pixels' dimensions, which are what the child chose inside `within`.
    pub size: (u32, u32),
    /// How many pages the document has, or zero for "unknown".
    ///
    /// **Unknown, not none** — ADR 0019. A machine with a renderer but not the
    /// tool that counts pages still previews the document; it cannot say how long
    /// it is, and the panel shows the page number without a total rather than
    /// inventing one. Anything that is not a document answers zero.
    pub pages: u32,
    /// How long this page is shown for, in milliseconds, or zero for "not timed".
    ///
    /// **A GIF's frame, and nothing else so far.** A document's page is shown
    /// until somebody turns it and answers zero; an animation's frame carries its
    /// own delay, because a GIF's frames are not evenly spaced and one rate for
    /// the whole file would be a number nothing behind it knows — rule 7.
    pub delay_ms: u32,
    /// Straight, non-premultiplied `RGBA8`, row-major, no padding.
    pub pixels: Vec<u8>,
    /// Where this page's words are, or empty where there are none.
    ///
    /// **[ADR 0024](../../../docs/adr/0024-a-page-arrives-with-its-words.md).**
    /// A page reply carries the renderer's own text layer beside its pixels, as
    /// the flat records [`words`] describes — so the panel can select and copy a
    /// line out of a document, which the same three commands already do for a
    /// text file. Empty for every image, every thumbnail, and every page whose
    /// document has no text in it.
    pub words: Vec<u8>,
}

impl Reply {
    /// A refusal with no pixels behind it.
    #[must_use]
    pub const fn refused(refusal: Refusal) -> Self {
        Self { refusal, size: (0, 0), pages: 0, delay_ms: 0, pixels: Vec::new(), words: Vec::new() }
    }

    /// Writes this reply to the host, in a child.
    ///
    /// # Errors
    ///
    /// Whatever the pipe says.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut header = Vec::with_capacity(REPLY_HEADER);
        header.extend_from_slice(&MAGIC.to_le_bytes());
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&(self.refusal as u16).to_le_bytes());
        header.extend_from_slice(&self.size.0.to_le_bytes());
        header.extend_from_slice(&self.size.1.to_le_bytes());
        header.extend_from_slice(&self.pages.to_le_bytes());
        header.extend_from_slice(&self.delay_ms.to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.pixels.len()).unwrap_or(0).to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.words.len()).unwrap_or(0).to_le_bytes());
        out.write_all(&header)?;
        out.write_all(&self.pixels)?;
        out.write_all(&self.words)?;
        out.flush()
    }

    /// Reads a reply from a child, in the host.
    ///
    /// # Errors
    ///
    /// A closed pipe is a child that died mid-request. A length past
    /// [`MOST_PIXELS`], or one that disagrees with the dimensions beside it, is a
    /// protocol violation — both are refused before anything is allocated, because
    /// a length read off a pipe and handed to `Vec::with_capacity` is how a bug in
    /// one process becomes an out-of-memory kill in another.
    pub fn read_from(input: &mut impl Read) -> std::io::Result<Self> {
        let head = exactly::<REPLY_HEADER>(input)?;
        let (magic, version) = (word32(&head, 0), word16(&head, 4));
        if magic != MAGIC || version != VERSION {
            return Err(broken(&format!("magic {magic:#x} version {version}")));
        }
        let refusal = Refusal::from_word(word16(&head, 6));
        let size = (word32(&head, 8), word32(&head, 12));
        let pages = word32(&head, 16);
        let delay_ms = word32(&head, 20);
        let len = word32(&head, 24) as usize;
        let said = word32(&head, 28) as usize;
        if len > MOST_PIXELS {
            return Err(broken(&format!("{len} bytes of pixels")));
        }
        // **The layer is bounded too, and by a much smaller number.** It is
        // records of a few dozen bytes each and a dense page has a few thousand
        // of them; four megabytes is a hundred times the largest page measured
        // and still a length a bug cannot turn into an allocation that matters.
        if said > MOST_WORDS {
            return Err(broken(&format!("{said} bytes of words")));
        }
        let wanted =
            (size.0 as usize).checked_mul(size.1 as usize).and_then(|pixels| pixels.checked_mul(4));
        if wanted != Some(len) {
            return Err(broken(&format!("{len} bytes for {}×{}", size.0, size.1)));
        }
        let mut pixels = vec![0u8; len];
        input.read_exact(&mut pixels)?;
        let mut words = vec![0u8; said];
        input.read_exact(&mut words)?;
        Ok(Self { refusal, size, pages, delay_ms, pixels, words })
    }
}

/// One word of a page, and where it is on it.
///
/// **In PDF points, with the origin at the page's top left**, which is what both
/// `pdftotext -bbox-layout` and `mutool` answer in — the host maps them onto the
/// drawn page with the one transform that already places it.
#[derive(Clone, PartialEq, Debug)]
pub struct Word {
    /// Its box: left, top, width, height, in points.
    pub at: (f32, f32, f32, f32),
    /// Which line of the page it belongs to, counted in the order the tool gave.
    ///
    /// **The tool's reading order, not ours** — ADR 0024. It is what turns a
    /// selection into text with newlines in it, and deciding it ourselves would
    /// be a page segmentation algorithm.
    pub line: u32,
    /// The word itself.
    pub text: String,
}

/// A page's text layer: the page's own size, and its words.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Words {
    /// The page's size in points, which is what the boxes are relative to.
    pub page: (f32, f32),
    /// Every word the tool found, in the order it found them.
    pub words: Vec<Word>,
}

impl Words {
    /// Packs a layer into the bytes a reply carries.
    ///
    /// **Flat records, not a serialization format.** Two floats, a count, and
    /// then per word four floats, a line number, a length and its UTF-8 — which
    /// is a `for` loop at each end and no dependency in the middle, the same
    /// bargain the rest of this file makes.
    #[must_use]
    pub fn pack(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 * self.words.len() + 12);
        out.extend_from_slice(&self.page.0.to_le_bytes());
        out.extend_from_slice(&self.page.1.to_le_bytes());
        out.extend_from_slice(&u32::try_from(self.words.len()).unwrap_or(0).to_le_bytes());
        for word in &self.words {
            for value in [word.at.0, word.at.1, word.at.2, word.at.3] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&word.line.to_le_bytes());
            let bytes = word.text.as_bytes();
            out.extend_from_slice(&u16::try_from(bytes.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(&bytes[..usize::from(u16::try_from(bytes.len()).unwrap_or(0))]);
        }
        out
    }

    /// Unpacks one, or nothing where the bytes do not describe a layer.
    ///
    /// **`None` rather than a partial answer.** A layer that stopped halfway
    /// would be a page whose last paragraph cannot be selected and nothing
    /// saying why; the whole of it is small enough that the honest answer to a
    /// short read is no layer at all.
    #[must_use]
    pub fn unpack(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        let mut at = 0usize;
        let mut take = |count: usize| -> Option<&[u8]> {
            let slice = bytes.get(at..at.checked_add(count)?)?;
            at += count;
            Some(slice)
        };
        let float = |slice: &[u8]| f32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
        let page = (float(take(4)?), float(take(4)?));
        let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let mut words = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            let box_ = (float(take(4)?), float(take(4)?), float(take(4)?), float(take(4)?));
            let line = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let len = usize::from(u16::from_le_bytes(take(2)?.try_into().ok()?));
            let text = String::from_utf8(take(len)?.to_vec()).ok()?;
            words.push(Word { at: box_, line, text });
        }
        Some(Self { page, words })
    }
}

/// Reads exactly `N` bytes, or says why it could not.
fn exactly<const N: usize>(input: &mut impl Read) -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn word16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn word32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn broken(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("preview protocol: {what}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Ask, Refusal, Reply, Request, Word, Words};

    #[test]
    fn a_request_round_trips_through_bytes() {
        let asked = Request {
            ask: Ask::Preview,
            within: (800, 600),
            page: 7,
            // Not UTF-8: a path is bytes, and a file called `\xff` is a file.
            path: vec![b'/', 0xff, b'x'],
        };
        let mut pipe = Vec::new();
        asked.write_to(&mut pipe).unwrap();
        let back = Request::read_from(&mut pipe.as_slice()).unwrap();
        assert_eq!(back, asked);
    }

    #[test]
    fn a_reply_round_trips_with_its_pixels() {
        let answered = Reply {
            refusal: Refusal::None,
            size: (2, 1),
            pages: 14,
            delay_ms: 40,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
            words: Words {
                page: (420.0, 595.0),
                words: vec![Word { at: (40.0, 82.0, 12.0, 16.0), line: 0, text: "A".into() }],
            }
            .pack(),
        };
        let mut pipe = Vec::new();
        answered.write_to(&mut pipe).unwrap();
        assert_eq!(Reply::read_from(&mut pipe.as_slice()).unwrap(), answered);

        // A refusal carries no pixels and still round trips.
        let refused = Reply::refused(Refusal::NoRenderer);
        let mut pipe = Vec::new();
        refused.write_to(&mut pipe).unwrap();
        assert_eq!(Reply::read_from(&mut pipe.as_slice()).unwrap(), refused);

        // **And a page count travels with a refusal too**, which is the whole of
        // how `↓` at the last page learns how long the document is: `NoSuchPage`
        // carries the count and no pixels — ADR 0019.
        let ended = Reply {
            refusal: Refusal::NoSuchPage,
            size: (0, 0),
            pages: 14,
            delay_ms: 0,
            pixels: Vec::new(),
            words: Vec::new(),
        };
        let mut pipe = Vec::new();
        ended.write_to(&mut pipe).unwrap();
        assert_eq!(Reply::read_from(&mut pipe.as_slice()).unwrap(), ended);
    }

    /// The layer round trips, and a short or invalid one is no layer rather than
    /// half of one.
    #[test]
    fn a_text_layer_round_trips_and_refuses_to_be_half_read() {
        let layer = Words {
            page: (420.0, 595.5),
            words: vec![
                Word { at: (40.0, 82.076, 12.006, 16.65), line: 0, text: "A".into() },
                Word { at: (57.01, 82.076, 45.036, 16.65), line: 0, text: "page,".into() },
                // A word whose bytes are not its characters, because a layer
                // carries UTF-8 and a length in bytes.
                Word { at: (40.0, 253.5, 68.464, 14.8), line: 1, text: "日本語".into() },
            ],
        };
        let packed = layer.pack();
        assert_eq!(Words::unpack(&packed).as_ref(), Some(&layer));

        // Every truncation is nothing rather than a partial page.
        for cut in 0..packed.len() {
            assert_eq!(Words::unpack(&packed[..cut]), None, "{cut} bytes read as a layer");
        }
        // And an empty layer is a page with no text in it, which is a scan.
        assert_eq!(Words::unpack(&[]), None);
    }

    /// **A length is not a promise.** Each of these is a header a buggy or
    /// replaced child could send, and each has to be refused before anything is
    /// allocated from it.
    #[test]
    fn a_reply_whose_length_disagrees_with_its_size_is_refused() {
        // **Assembled by hand rather than through `write_to`**, because what is
        // being tested is a header this build would never write: the point is what
        // happens when the bytes on the pipe are wrong.
        let head = |size: (u32, u32), len: u32| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&0x534b_5052u32.to_le_bytes());
            bytes.extend_from_slice(&super::VERSION.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&size.0.to_le_bytes());
            bytes.extend_from_slice(&size.1.to_le_bytes());
            // The page count and the frame delay, neither of which this is about.
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&len.to_le_bytes());
            // And no text layer, which is every reply this test is about.
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes
        };
        // Two pixels claimed, one pixel's worth of bytes.
        assert!(Reply::read_from(&mut head((2, 1), 4).as_slice()).is_err());
        // A gigabyte claimed for a 1×1 image.
        assert!(Reply::read_from(&mut head((1, 1), 1 << 30).as_slice()).is_err());
        // A size that overflows on multiplication rather than wrapping to a
        // plausible length.
        assert!(Reply::read_from(&mut head((u32::MAX, u32::MAX), 4).as_slice()).is_err());
        // And the honest one still reads.
        let mut ok = head((1, 1), 4);
        ok.extend_from_slice(&[9, 9, 9, 9]);
        assert!(Reply::read_from(&mut ok.as_slice()).is_ok());
    }

    #[test]
    fn a_stranger_on_the_pipe_is_refused_rather_than_parsed() {
        assert!(Request::read_from(&mut b"hello there, twenty-four+".as_slice()).is_err());
        assert!(Reply::read_from(&mut b"hello there, twenty-four+".as_slice()).is_err());
        // A truncated header is an error and not a hang.
        assert!(Request::read_from(&mut b"SKPR".as_slice()).is_err());
    }

    /// **A stale helper says which side is old.** The header grew a page in
    /// ADR 0019 and `VERSION` moved with it; a build that reads the old layout as
    /// the new one would take a path length out of a page number.
    #[test]
    fn a_header_from_the_previous_version_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x534b_5052u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&(Ask::Preview as u16).to_le_bytes());
        bytes.extend_from_slice(&800u32.to_le_bytes());
        bytes.extend_from_slice(&600u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert!(Request::read_from(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn every_wire_word_maps_back_to_what_it_names() {
        assert_eq!(Ask::from_word(Ask::Thumbnail as u16), Some(Ask::Thumbnail));
        assert_eq!(Ask::from_word(Ask::Preview as u16), Some(Ask::Preview));
        assert_eq!(Ask::from_word(99), None);
        for refusal in [
            Refusal::None,
            Refusal::NotItsKind,
            Refusal::Unreadable,
            Refusal::Undecodable,
            Refusal::NoRenderer,
            Refusal::TooLarge,
        ] {
            assert_eq!(Refusal::from_word(refusal as u16), refusal);
        }
        // A reason this build does not know still failed.
        assert_eq!(Refusal::from_word(4242), Refusal::Undecodable);
        assert!(Refusal::None.message().is_none());
        assert!(Refusal::Undecodable.message().is_some());
    }
}
