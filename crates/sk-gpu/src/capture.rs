// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading a frame back, and writing it out as a PNG.
//!
//! This is the other half of the golden-frame harness
//! [docs/04-architecture.md](../../../docs/04-architecture.md#testing-strategy-by-layer)
//! asks for: render offscreen, compare against a reference, at 100/125/150/200%,
//! on llvmpipe so CI needs no GPU. It also makes the renderer inspectable by a
//! person on a machine whose screen is not available.
//!
//! The PNG writer is sixty lines rather than a dependency because it only has to
//! emit one kind of file: 8-bit RGBA, one `IDAT`, and **stored** deflate blocks.
//! A stored block is a length, its complement, and the bytes — no compression at
//! all, which is the right trade for a file that exists to be looked at once and
//! diffed by a machine.

use std::io::Write;
use std::path::Path;

/// Writes 8-bit RGBA pixels as a PNG.
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> std::io::Result<()> {
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() < expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} bytes for a {width}x{height} image, need {expected}", rgba.len()),
        ));
    }

    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    out.write_all(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])?;

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, truecolour with alpha
    chunk(&mut out, *b"IHDR", &header)?;

    // Scanlines, each prefixed by filter type 0.
    let mut raw = Vec::with_capacity(expected + height as usize);
    for row in 0..height as usize {
        raw.push(0);
        let start = row * width as usize * 4;
        raw.extend_from_slice(&rgba[start..start + width as usize * 4]);
    }
    chunk(&mut out, *b"IDAT", &zlib_stored(&raw))?;
    chunk(&mut out, *b"IEND", &[])?;
    out.flush()
}

fn chunk(out: &mut impl Write, kind: [u8; 4], data: &[u8]) -> std::io::Result<()> {
    out.write_all(&(data.len() as u32).to_be_bytes())?;
    out.write_all(&kind)?;
    out.write_all(data)?;
    let mut crc = Crc::new();
    crc.update(&kind);
    crc.update(data);
    out.write_all(&crc.finish().to_be_bytes())
}

/// A zlib stream of stored deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // 0x78 0x01: deflate, 32 KiB window, no preset dictionary, fastest level.
    let mut out = vec![0x78, 0x01];
    let mut adler = Adler::new();
    adler.update(data);
    let mut chunks = data.chunks(0xffff).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(block) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler.finish().to_be_bytes());
    out
}

struct Crc(u32);

impl Crc {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                // The CRC-32 polynomial from the PNG specification, reversed.
                let mask = 0u32.wrapping_sub(self.0 & 1);
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}

struct Adler(u32, u32);

impl Adler {
    fn new() -> Self {
        Self(1, 0)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 + u32::from(*byte)) % 65521;
            self.1 = (self.1 + self.0) % 65521;
        }
    }

    fn finish(self) -> u32 {
        (self.1 << 16) | self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Adler, Crc, write_png, zlib_stored};

    #[test]
    fn the_crc_matches_the_png_specifications_worked_example() {
        // `IEND` with no data has a fixed CRC that every PNG file in the world
        // ends with, which makes it a free conformance test.
        let mut crc = Crc::new();
        crc.update(b"IEND");
        assert_eq!(crc.finish(), 0xae42_6082);
    }

    #[test]
    fn the_adler_sum_matches_the_rfc_1950_definition() {
        let mut adler = Adler::new();
        adler.update(b"Wikipedia");
        assert_eq!(adler.finish(), 0x11E6_0398);
    }

    #[test]
    fn a_stored_stream_carries_the_bytes_and_marks_its_last_block() {
        let data = vec![7u8; 0xffff + 10];
        let stream = zlib_stored(&data);
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        // Two blocks: the first not final, the second final.
        assert_eq!(stream[2], 0x00);
        assert_eq!(stream[2 + 5 + 0xffff], 0x01);
        assert_eq!(stream.len(), 2 + 5 + 0xffff + 5 + 10 + 4);
    }

    #[test]
    fn a_written_file_starts_with_the_signature_and_ends_with_iend() {
        let path = std::env::temp_dir().join(format!("skerry-png-{}.png", std::process::id()));
        write_png(&path, 2, 2, &[0xff; 16]).expect("write");
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&bytes[bytes.len() - 8..bytes.len() - 4], b"IEND");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_read_past() {
        let path = std::env::temp_dir().join("skerry-png-short.png");
        assert!(write_png(&path, 4, 4, &[0; 8]).is_err());
    }
}
