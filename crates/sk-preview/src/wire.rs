//! The bytes that cross between the host and a preview child.
//!
//! **[ADR 0017](../../../docs/adr/0017-one-preview-child-per-kind-answering-one-request-at-a-time.md).**
//! A child is a long-lived filter: length-prefixed requests in on `stdin`,
//! length-prefixed replies out on `stdout`, in order, one in flight at a time.
//! Ordering is the only correlation mechanism, which is why there are no request
//! ids here and why a child that answers out of order has violated the protocol
//! rather than confused the host.
//!
//! **Fixed twenty-byte headers and a payload.** Not a text format: the payload is
//! a quarter of a megabyte of pixels and every byte of framing around it is
//! framing somebody has to parse. Not a serialization crate either — six integers
//! and a byte string is not a dependency's worth of problem, and this crate's
//! whole point is that the host's graph does not grow.
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
const VERSION: u16 = 1;

/// Header bytes before the payload: magic, version, a word, and three lengths.
const HEADER: usize = 20;

/// The most pixels a reply may carry, in bytes.
///
/// Sixty-four megabytes — a 4096×4096 RGBA image. It bounds what a child can make
/// the host allocate by lying about a length, which is the one thing a hostile
/// *child* could do to a host that trusts it. The child is ours, so this is a
/// bound against a bug rather than against an attacker; it is here because a
/// length read off a pipe and handed to `Vec::with_capacity` is how a bug becomes
/// an out-of-memory kill.
const MOST_PIXELS: usize = 64 * 1024 * 1024;

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
            Self::None | Self::NotItsKind => None,
            Self::Unreadable => Some("preview.unreadable"),
            Self::Undecodable => Some("preview.undecodable"),
            Self::NoRenderer => Some("preview.no_renderer"),
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
        let len = word32(&head, 16) as usize;
        if len > 4096 {
            return Err(broken(&format!("a path of {len} bytes")));
        }
        let mut path = vec![0u8; len];
        input.read_exact(&mut path)?;
        Ok(Self { ask, within, path })
    }
}

/// What a child answers with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reply {
    /// Why there are no pixels, or [`Refusal::None`].
    pub refusal: Refusal,
    /// The pixels' dimensions, which are what the child chose inside `within`.
    pub size: (u32, u32),
    /// Straight, non-premultiplied `RGBA8`, row-major, no padding.
    pub pixels: Vec<u8>,
}

impl Reply {
    /// A refusal with no pixels behind it.
    #[must_use]
    pub const fn refused(refusal: Refusal) -> Self {
        Self { refusal, size: (0, 0), pixels: Vec::new() }
    }

    /// Writes this reply to the host, in a child.
    ///
    /// # Errors
    ///
    /// Whatever the pipe says.
    pub fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut header = Vec::with_capacity(HEADER);
        header.extend_from_slice(&MAGIC.to_le_bytes());
        header.extend_from_slice(&VERSION.to_le_bytes());
        header.extend_from_slice(&(self.refusal as u16).to_le_bytes());
        header.extend_from_slice(&self.size.0.to_le_bytes());
        header.extend_from_slice(&self.size.1.to_le_bytes());
        header.extend_from_slice(&u32::try_from(self.pixels.len()).unwrap_or(0).to_le_bytes());
        out.write_all(&header)?;
        out.write_all(&self.pixels)?;
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
        let head = exactly::<HEADER>(input)?;
        let (magic, version) = (word32(&head, 0), word16(&head, 4));
        if magic != MAGIC || version != VERSION {
            return Err(broken(&format!("magic {magic:#x} version {version}")));
        }
        let refusal = Refusal::from_word(word16(&head, 6));
        let size = (word32(&head, 8), word32(&head, 12));
        let len = word32(&head, 16) as usize;
        if len > MOST_PIXELS {
            return Err(broken(&format!("{len} bytes of pixels")));
        }
        let wanted =
            (size.0 as usize).checked_mul(size.1 as usize).and_then(|pixels| pixels.checked_mul(4));
        if wanted != Some(len) {
            return Err(broken(&format!("{len} bytes for {}×{}", size.0, size.1)));
        }
        let mut pixels = vec![0u8; len];
        input.read_exact(&mut pixels)?;
        Ok(Self { refusal, size, pixels })
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

    use super::{Ask, Refusal, Reply, Request};

    #[test]
    fn a_request_round_trips_through_bytes() {
        let asked = Request {
            ask: Ask::Preview,
            within: (800, 600),
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
        let answered =
            Reply { refusal: Refusal::None, size: (2, 1), pixels: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        let mut pipe = Vec::new();
        answered.write_to(&mut pipe).unwrap();
        assert_eq!(Reply::read_from(&mut pipe.as_slice()).unwrap(), answered);

        // A refusal carries no pixels and still round trips.
        let refused = Reply::refused(Refusal::NoRenderer);
        let mut pipe = Vec::new();
        refused.write_to(&mut pipe).unwrap();
        assert_eq!(Reply::read_from(&mut pipe.as_slice()).unwrap(), refused);
    }

    /// **A length is not a promise.** Each of these is a header a buggy or
    /// replaced child could send, and each has to be refused before anything is
    /// allocated from it.
    #[test]
    fn a_reply_whose_length_disagrees_with_its_size_is_refused() {
        let head = |size: (u32, u32), len: u32| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&0x534b_5052u32.to_le_bytes());
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&size.0.to_le_bytes());
            bytes.extend_from_slice(&size.1.to_le_bytes());
            bytes.extend_from_slice(&len.to_le_bytes());
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
        assert!(Request::read_from(&mut b"hello there, twenty bytes".as_slice()).is_err());
        assert!(Reply::read_from(&mut b"hello there, twenty bytes".as_slice()).is_err());
        // A truncated header is an error and not a hang.
        assert!(Request::read_from(&mut b"SKPR".as_slice()).is_err());
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
