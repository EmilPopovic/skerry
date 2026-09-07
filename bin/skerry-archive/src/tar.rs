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
        Self { source, data: 0, pad: 0, long_name: None, long_link: None, pax: Pax::default() }
    }

    /// The next entry, or nothing at the end of the archive.
    pub fn next(&mut self) -> Result<Option<Item>, Refusal> {
        self.skip_data()?;
        loop {
            let Some(block) = self.block()? else { return Ok(None) };
            // A block of zeros is the end. The format says two of them; stopping
            // at the first costs nothing, because a tar with a zero block in the
            // middle of it is a tar whose next header is unreadable anyway.
            if block.iter().all(|byte| *byte == 0) {
                return Ok(None);
            }
            let size = octal(&block[124..136]).ok_or(Refusal::Undecodable)?;
            self.data = size;
            self.pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
            match block[156] {
                b'L' => self.long_name = Some(self.extra()?),
                b'K' => self.long_link = Some(self.extra()?),
                b'x' | b'X' => {
                    let records = self.extra()?;
                    self.pax = pax_of(&records);
                }
                // A global header applies to every entry after it, and the two
                // fields it is used for in practice — `mtime` and a comment — are
                // not fields this reads per entry. Skipped rather than merged.
                b'g' => {
                    self.skip_data()?;
                }
                _ => return Ok(Some(self.item(&block, size))),
            }
        }
    }

    /// The entry a header block describes, with the extensions folded in.
    fn item(&mut self, block: &[u8; BLOCK], size: u64) -> Item {
        let flag = block[156];
        let name =
            self.pax.path.take().or_else(|| self.long_name.take()).unwrap_or_else(|| joined(block));
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
        let link = self
            .pax
            .link
            .take()
            .or_else(|| self.long_link.take())
            .unwrap_or_else(|| text(&block[157..257]));
        let item = Item {
            path: crate::place::tidy(&name),
            link,
            size: self.pax.size.take().unwrap_or(size),
            mtime: self
                .pax
                .mtime
                .take()
                .or_else(|| octal(&block[136..148]).map(u64::cast_signed))
                .unwrap_or_default(),
            mode: octal(&block[100..108]).unwrap_or(0o644) as u32,
            kind,
            hard: flag == b'1',
        };
        if item.kind != Kind::File {
            // A directory's or a link's size field is not data in the stream. A
            // reader that trusted it would take the next header for file content.
            self.data = 0;
            self.pad = 0;
        }
        item
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
        while self.data + self.pad > 0 {
            let want = ((self.data + self.pad) as usize).min(buffer.len());
            let read = self.source.read(&mut buffer[..want]).map_err(|_| Refusal::Undecodable)?;
            if read == 0 {
                self.data = 0;
                self.pad = 0;
                return Ok(());
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
                return Ok(None);
            }
            filled += read;
        }
        Ok(Some(block))
    }

    /// The data of an `L`, `K` or `x` block, as text.
    fn extra(&mut self) -> Result<String, Refusal> {
        if self.data > MOST_EXTRA {
            return Err(Refusal::Undecodable);
        }
        let mut bytes = Vec::new();
        self.data(&mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(text(&bytes))
    }
}

/// The `prefix` and `name` fields, joined the way `ustar` defines.
fn joined(block: &[u8; BLOCK]) -> String {
    let name = text(&block[0..100]);
    let prefix = text(&block[345..500]);
    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
}

/// A NUL-padded field as text. Not UTF-8 by specification, and lossy here for the
/// same reason every path in this program is: a name that does not decode still
/// has to be shown.
fn text(field: &[u8]) -> String {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// The four pax records that matter, out of `length key=value\n` repeated.
fn pax_of(records: &str) -> Pax {
    let mut pax = Pax::default();
    for record in records.split('\n') {
        let Some((_, pair)) = record.split_once(' ') else { continue };
        let Some((key, value)) = pair.split_once('=') else { continue };
        match key {
            "path" => pax.path = Some(value.to_owned()),
            "linkpath" => pax.link = Some(value.to_owned()),
            "size" => pax.size = value.parse().ok(),
            // A pax time is decimal seconds with an optional fraction. The
            // fraction is dropped rather than rounded: a listing shows a date.
            "mtime" => {
                pax.mtime = value.split_once('.').map_or(value, |(whole, _)| whole).parse().ok();
            }
            _ => {}
        }
    }
    pax
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The one thing a pax record's own length field must not do is confuse the
    /// key: `30 path=…` has the length before a space, and a key that came out as
    /// `30 path` would silently lose every long name in the archive.
    #[test]
    fn a_pax_record_is_read_past_its_own_length() {
        let pax = pax_of("30 path=deep/name.txt\n20 mtime=1700000000.5\n");
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
        assert_eq!(joined(&block), "a/b/c/name.txt");
        block[345] = 0;
        block[346..350].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(joined(&block), "name.txt");
    }
}
