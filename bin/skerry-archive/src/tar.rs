// SPDX-License-Identifier: Apache-2.0
//! A tar, read forwards.
//!
//! Five hundred and twelve bytes of header, the file's bytes padded to the next
//! block, and again until two of them are zero. There is no index: what is inside
//! a tar is only knowable by reading to the end of it, which is why a compressed
//! one costs a whole decompression to list — [ADR
//! 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md).
//!
//! Three extensions are read because every tar in the wild has them: `ustar`'s
//! 155-byte prefix, GNU's `L` block for a name that does not fit, and pax's `x`
//! block, which is the only one of the three that can carry a name with a
//! newline in it.

use std::io::Read;

use sk_preview::archive::{Kind, Refusal};

use crate::detect::octal;

/// One block, which is also the padding unit.
const BLOCK: usize = 512;

/// The most a long-name or pax block may carry.
///
/// A megabyte. A path is at most 4 KiB on Linux and a pax block holds several
/// records; this is a bound against a length field rather than against a filename.
const MOST_EXTRA: u64 = 1024 * 1024;

/// One entry, with everything the writer and the listing both need.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    /// The path inside the archive.
    pub path: String,
    /// What a symbolic or hard link points at.
    pub link: String,
    /// The unpacked size in bytes.
    pub size: u64,
    /// The modification time in seconds since the epoch.
    pub mtime: i64,
    /// The mode the archive recorded. Only the execute bit is honoured.
    pub mode: u32,
    /// What it is.
    pub kind: Kind,
    /// Whether it is a hard link, which [`Kind::Other`] cannot say on its own.
    ///
    /// **The distinction the wire does not carry.** A listing shows a hard link
    /// and a device node the same way, because to a person they are both "not a
    /// file"; the writer has to tell them apart, because one of them is content
    /// that would otherwise be dropped and the other is a thing it must not make.
    pub hard: bool,
}

/// A tar being walked.
pub struct Tar<R: Read> {
    source: R,
    /// Bytes of the current entry's data not yet read.
    data: u64,
    /// Padding after it, to the next block boundary.
    pad: u64,
    long_name: Option<String>,
    long_link: Option<String>,
    pax: Pax,
    global: Pax,
}

/// What a pax header said about the entry after it.
#[derive(Default)]
struct Pax {
    path: Option<String>,
    link: Option<String>,
    size: Option<u64>,
    mtime: Option<i64>,
}

impl<R: Read> Tar<R> {
    /// A walk over `source`, which is the container with any filter removed.
    pub fn over(source: R) -> Self {
        Self {
            source,
            data: 0,
            pad: 0,
            long_name: None,
            long_link: None,
            pax: Pax::default(),
            global: Pax::default(),
        }
    }

    /// The next entry, or nothing at the end of the archive.
    pub fn next(&mut self) -> Result<Option<Item>, Refusal> {
        self.skip_data()?;
        loop {
            let Some(block) = self.block()? else { return self.finished() };
            if block.iter().all(|byte| *byte == 0) {
                // Drain padding so compressed trailers and filter exit status are
                // validated even when the last member was already complete.
                let mut tail = vec![0; 64 * 1024];
                loop {
                    let read = self.source.read(&mut tail).map_err(|_| Refusal::Undecodable)?;
                    if read == 0 {
                        return self.finished();
                    }
                    if tail[..read].iter().any(|byte| *byte != 0) {
                        return Err(Refusal::Undecodable);
                    }
                }
            }
            if !crate::detect::valid_tar_checksum(&block) {
                return Err(Refusal::Undecodable);
            }
            let size = octal(&block[124..136]).ok_or(Refusal::Undecodable)?;
            self.data = size;
            self.pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
            match block[156] {
                b'L' => self.long_name = Some(self.extra(true)?),
                b'K' => self.long_link = Some(self.extra(true)?),
                b'x' | b'X' => {
                    let records = self.extra(false)?;
                    self.pax.merge(pax_of(&records)?);
                }
                // Global defaults persist; a per-entry header overrides them.
                b'g' => {
                    let records = self.extra(false)?;
                    self.global.merge(pax_of(&records)?);
                }
                b'S' => return Err(Refusal::Unsupported),
                _ => return self.item(&block, size).map(Some),
            }
        }
    }

    fn finished(&self) -> Result<Option<Item>, Refusal> {
        if self.long_name.is_some() || self.long_link.is_some() || self.pax.present() {
            Err(Refusal::Undecodable)
        } else {
            Ok(None)
        }
    }

    /// The entry a header block describes, with the extensions folded in.
    fn item(&mut self, block: &[u8; BLOCK], size: u64) -> Result<Item, Refusal> {
        let flag = block[156];
        let long_name = self.long_name.take();
        let name = self
            .pax
            .path
            .take()
            .or(long_name)
            .or_else(|| self.global.path.clone())
            .map_or_else(|| joined(block), Ok)?;
        let kind = match flag {
            b'5' => Kind::Directory,
            b'2' => Kind::Link,
            b'0' | b'\0' | b'7' => {
                // Some writers mark a directory with a trailing separator and the
                // ordinary file flag, which is the one case where the name is a
                // better witness than the field.
                if name.ends_with('/') { Kind::Directory } else { Kind::File }
            }
            _ => Kind::Other,
        };
        let long_link = self.long_link.take();
        let link = self
            .pax
            .link
            .take()
            .or(long_link)
            .or_else(|| self.global.link.clone())
            .map_or_else(|| text(&block[157..257]), Ok)?;
        let item = Item {
            path: crate::place::tidy(&name),
            link,
            size: self.pax.size.take().or(self.global.size).unwrap_or(size),
            mtime: self
                .pax
                .mtime
                .take()
                .or(self.global.mtime)
                .or_else(|| octal(&block[136..148]).map(u64::cast_signed))
                .unwrap_or_default(),
            mode: octal(&block[100..108]).unwrap_or(0o644) as u32,
            kind,
            hard: flag == b'1',
        };
        self.data = item.size;
        self.pad = (BLOCK as u64 - item.size % BLOCK as u64) % BLOCK as u64;
        if item.kind != Kind::File {
            // A directory's or a link's size field is not data in the stream. A
            // reader that trusted it would take the next header for file content.
            if item.size != 0 {
                return Err(Refusal::Undecodable);
            }
            self.data = 0;
            self.pad = 0;
        }
        Ok(item)
    }

    /// Reads this entry's data, handing it out in chunks.
    pub fn data(
        &mut self,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), Refusal>,
    ) -> Result<u64, Refusal> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut written = 0;
        while self.data > 0 {
            let want = (self.data as usize).min(buffer.len());
            let read = self.source.read(&mut buffer[..want]).map_err(|_| Refusal::Undecodable)?;
            if read == 0 {
                // The archive ended inside a file. What is on disk is short and
                // the caller is told, rather than a file that looks complete.
                self.data = 0;
                return Err(Refusal::Undecodable);
            }
            sink(&buffer[..read])?;
            self.data -= read as u64;
            written += read as u64;
        }
        self.skip_data()?;
        Ok(written)
    }

    /// Drops whatever is left of this entry's data and its padding.
    fn skip_data(&mut self) -> Result<(), Refusal> {
        let mut buffer = vec![0u8; 64 * 1024];
        while self.data > 0 || self.pad > 0 {
            let want = self.data.saturating_add(self.pad).min(buffer.len() as u64) as usize;
            let read = self.source.read(&mut buffer[..want]).map_err(|_| Refusal::Undecodable)?;
            if read == 0 {
                return Err(Refusal::Undecodable);
            }
            let from_data = (read as u64).min(self.data);
            self.data -= from_data;
            self.pad -= read as u64 - from_data;
        }
        Ok(())
    }

    /// One whole block, or nothing at the end of the file.
    fn block(&mut self) -> Result<Option<[u8; BLOCK]>, Refusal> {
        let mut block = [0u8; BLOCK];
        let mut filled = 0;
        while filled < BLOCK {
            let read = self.source.read(&mut block[filled..]).map_err(|_| Refusal::Undecodable)?;
            if read == 0 {
                // A short final block is a truncated archive, not an entry.
                return if filled == 0 { Ok(None) } else { Err(Refusal::Undecodable) };
            }
            filled += read;
        }
        Ok(Some(block))
    }

    /// The data of an `L`, `K` or `x` block, as text.
    fn extra(&mut self, nul_terminated: bool) -> Result<String, Refusal> {
        if self.data > MOST_EXTRA {
            return Err(Refusal::Undecodable);
        }
        let mut bytes = Vec::new();
        self.data(&mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        if end < bytes.len() && (!nul_terminated || bytes[end..].iter().any(|byte| *byte != 0)) {
            return Err(Refusal::Undecodable);
        }
        String::from_utf8(bytes[..end].to_vec()).map_err(|_| Refusal::Undecodable)
    }
}

/// The `prefix` and `name` fields, joined the way `ustar` defines.
fn joined(block: &[u8; BLOCK]) -> Result<String, Refusal> {
    let name = text(&block[0..100])?;
    let prefix =
        if &block[257..263] == b"ustar\0" { text(&block[345..500])? } else { String::new() };
    Ok(if prefix.is_empty() { name } else { format!("{prefix}/{name}") })
}

/// Refuse names the string-based archive protocol cannot represent losslessly.
fn text(field: &[u8]) -> Result<String, Refusal> {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| Refusal::Unsupported)
}

/// The four pax records that matter, out of `length key=value\n` repeated.
fn pax_of(records: &str) -> Result<Pax, Refusal> {
    let mut pax = Pax::default();
    let mut bytes = records.as_bytes();
    while !bytes.is_empty() {
        let space = bytes.iter().position(|byte| *byte == b' ').ok_or(Refusal::Undecodable)?;
        if !bytes[..space].iter().all(u8::is_ascii_digit) {
            return Err(Refusal::Undecodable);
        }
        let length: usize = std::str::from_utf8(&bytes[..space])
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or(Refusal::Undecodable)?;
        if length <= space + 2 || length > bytes.len() || bytes[length - 1] != b'\n' {
            return Err(Refusal::Undecodable);
        }
        let pair =
            std::str::from_utf8(&bytes[space + 1..length - 1]).map_err(|_| Refusal::Undecodable)?;
        let (key, value) = pair.split_once('=').ok_or(Refusal::Undecodable)?;
        if key.starts_with("GNU.sparse.")
            || key == "SCHILY.realsize"
            || (matches!(key, "path" | "linkpath" | "size" | "mtime") && value.is_empty())
            || (key == "hdrcharset" && value != "ISO-IR 10646 2000 UTF-8")
        {
            return Err(Refusal::Unsupported);
        }
        match key {
            "path" => pax.path = Some(value.to_owned()),
            "linkpath" => pax.link = Some(value.to_owned()),
            "size" => pax.size = Some(value.parse().map_err(|_| Refusal::Undecodable)?),
            "mtime" => {
                pax.mtime = Some(pax_seconds(value)?);
            }
            _ => {}
        }
        bytes = &bytes[length..];
    }
    Ok(pax)
}

// The protocol carries whole Unix seconds, rounded down like filesystem times.
fn pax_seconds(value: &str) -> Result<i64, Refusal> {
    let (whole, fraction) = value.split_once('.').map_or((value, None), |(a, b)| (a, Some(b)));
    if fraction
        .is_some_and(|digits| digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(Refusal::Undecodable);
    }
    let seconds: i64 = whole.parse().map_err(|_| Refusal::Undecodable)?;
    if whole.starts_with('-') && fraction.is_some_and(|digits| digits.bytes().any(|b| b != b'0')) {
        seconds.checked_sub(1).ok_or(Refusal::Undecodable)
    } else {
        Ok(seconds)
    }
}

impl Pax {
    fn present(&self) -> bool {
        self.path.is_some() || self.link.is_some() || self.size.is_some() || self.mtime.is_some()
    }

    fn merge(&mut self, next: Self) {
        if next.path.is_some() {
            self.path = next.path;
        }
        if next.link.is_some() {
            self.link = next.link;
        }
        if next.size.is_some() {
            self.size = next.size;
        }
        if next.mtime.is_some() {
            self.mtime = next.mtime;
        }
    }
}

#[test]
fn parser_campaign_tar() {
    crate::fuzz_support::campaign(
        "tar",
        &[include_bytes!("../../../tests/fuzz/valid.tar"), &[0; 1024]],
        |bytes| {
            let mut tar = Tar::over(bytes);
            for _ in 0..1024 {
                match tar.next() {
                    Ok(Some(_)) => {
                        let _ = tar.data(&mut |_| Ok(()));
                    }
                    _ => break,
                }
            }
        },
    );
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The one thing a pax record's own length field must not do is confuse the
    /// key: `30 path=…` has the length before a space, and a key that came out as
    /// `30 path` would silently lose every long name in the archive.
    #[test]
    fn pax_fractional_seconds_are_validated_and_floor_negative_times() {
        assert_eq!(pax_seconds("-0.5"), Ok(-1));
        assert_eq!(pax_seconds("-1.25"), Ok(-2));
        assert_eq!(pax_seconds("1.25"), Ok(1));
        assert_eq!(pax_seconds("-1.000"), Ok(-1));
        for bad in ["1.", "1.no", "1.2.3", "-9223372036854775808.1"] {
            assert!(pax_seconds(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_pax_record_is_read_past_its_own_length() {
        let pax =
            pax_of(&(record("path", "deep/name.txt") + &record("mtime", "1700000000.5"))).unwrap();
        assert_eq!(pax.path.as_deref(), Some("deep/name.txt"));
        assert_eq!(pax.mtime, Some(1_700_000_000));
    }

    /// `ustar`'s prefix is a directory and the name is the rest of it, joined with
    /// a separator that is in neither field.
    #[test]
    fn a_prefix_and_a_name_are_one_path() {
        let mut block = [0u8; BLOCK];
        block[0..8].copy_from_slice(b"name.txt");
        block[345..350].copy_from_slice(b"a/b/c");
        block[257..263].copy_from_slice(b"ustar\0");
        assert_eq!(joined(&block).unwrap(), "a/b/c/name.txt");
        block[345] = 0;
        block[346..350].copy_from_slice(&[0, 0, 0, 0]);
        block[257..263].copy_from_slice(b"ustar\0");
        assert_eq!(joined(&block).unwrap(), "name.txt");
    }
    fn header(name: &str, size: u64, kind: u8) -> [u8; BLOCK] {
        let mut header = [0; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        header
    }

    fn record(key: &str, value: &str) -> String {
        let mut length = key.len() + value.len() + 4;
        loop {
            let record = format!("{length} {key}={value}\n");
            if record.len() == length {
                return record;
            }
            length = record.len();
        }
    }

    pub(crate) fn member(out: &mut Vec<u8>, name: &str, kind: u8, data: &[u8]) {
        out.extend_from_slice(&header(name, data.len() as u64, kind));
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(BLOCK), 0);
    }

    #[test]
    fn every_header_checksum_is_validated() {
        let mut data = Vec::new();
        member(&mut data, "first", b'0', b"first");
        let mut bad = header("second", 0, b'0');
        bad[0] ^= 1;
        data.extend_from_slice(&bad);
        let mut tar = Tar::over(data.as_slice());
        assert!(tar.next().unwrap().is_some());
        assert!(tar.next().is_err());
    }

    #[test]
    fn truncated_headers_payloads_and_padding_are_errors() {
        assert!(Tar::over(&[1u8; 23][..]).next().is_err());
        for payload in [0, 10, 100] {
            let mut data = header("file", 100, b'0').to_vec();
            data.extend(vec![1; payload]);
            let mut tar = Tar::over(data.as_slice());
            assert!(tar.next().unwrap().is_some());
            assert!(tar.next().is_err(), "truncation with {payload} payload bytes was accepted");
        }
    }

    #[test]
    fn pax_size_controls_payload_and_padding_and_names_are_byte_framed() {
        let mut data = Vec::new();
        let pax = record("path", "café\nnotes") + &record("size", "513");
        member(&mut data, "PaxHeader", b'x', pax.as_bytes());
        data.extend_from_slice(&header("placeholder", 1, b'0'));
        data.extend(vec![42; 513]);
        data.resize(data.len().next_multiple_of(BLOCK), 0);
        member(&mut data, "following", b'0', b"next");
        data.extend([0; BLOCK * 2]);
        let mut tar = Tar::over(data.as_slice());
        let item = tar.next().unwrap().unwrap();
        assert_eq!(item.path, "café\nnotes");
        assert_eq!(item.size, 513);
        let mut bytes = Vec::new();
        assert_eq!(
            tar.data(&mut |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap(),
            513
        );
        assert_eq!(bytes, vec![42; 513]);
        assert_eq!(tar.next().unwrap().unwrap().path, "following");
        assert!(tar.next().unwrap().is_none());
    }

    #[test]
    fn malformed_pax_and_unsupported_sparse_data_are_refused() {
        for pax in [
            "999 path=missing\n",
            "0 path=bad\n",
            "abc path=bad\n",
            "12 path=bad",
            "18 GNU.sparse.x=1\n",
        ] {
            let mut bytes = Vec::new();
            member(&mut bytes, "PaxHeader", b'x', pax.as_bytes());
            member(&mut bytes, "file", b'0', b"x");
            assert!(Tar::over(bytes.as_slice()).next().is_err(), "{pax:?}");
        }
    }
}
