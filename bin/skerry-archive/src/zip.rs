// SPDX-License-Identifier: Apache-2.0
//! A zip, read from its index.
//!
//! A zip's central directory is at the end of the file and names every entry with
//! its size, its method and where its bytes are — so **listing one costs no
//! decompression at all**, and pulling one file out of a ten-gigabyte archive is
//! a seek. That is the property that makes a zip the format worth parsing
//! ourselves: an external tool would give the same answer through a text table
//! and a whole process.
//!
//! Zip64 is read, because an archive over four gigabytes or past 65,535 entries
//! writes its real numbers in a second directory and leaves `0xFFFFFFFF` in the
//! first — and a reader that took the sentinel literally would seek to the wrong
//! place on exactly the archives that are hardest to make again.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use sk_preview::archive::{Entry, Kind, Refusal};

/// The fixed part of a central-directory record.
const CENTRAL: usize = 46;

/// The fixed part of a local header.
const LOCAL: usize = 30;

/// How far back the end-of-directory record is looked for.
///
/// The record is 22 bytes plus a comment of at most 65,535, so this is the whole
/// space it can be in.
const TAIL: u64 = 22 + 65_535;

/// Stored: the bytes are the file.
const STORED: u16 = 0;

/// Deflate, which is every other zip in practice.
const DEFLATE: u16 = 8;

/// One central-directory record.
pub struct Central {
    /// What the listing shows.
    pub entry: Entry,
    /// How the bytes are compressed.
    pub method: u16,
    /// How many bytes of them there are.
    pub compressed: u64,
    /// Where the local header is.
    pub offset: u64,
    /// What the bytes should hash to.
    pub crc: u32,
    /// The unix mode, where the archive was made somewhere that has them.
    pub mode: u32,
}

/// A zip with its index read.
pub struct Zip {
    file: File,
    /// Every entry, in the order the directory lists them.
    pub entries: Vec<Central>,
}

impl Zip {
    /// Reads the central directory of `file`.
    pub fn read(mut file: File) -> Result<Self, Refusal> {
        let length = file.metadata().map_err(|_| Refusal::Unreadable)?.len();
        let from = length.saturating_sub(TAIL);
        let mut tail = vec![0u8; (length - from) as usize];
        file.seek(SeekFrom::Start(from)).map_err(|_| Refusal::Unreadable)?;
        file.read_exact(&mut tail).map_err(|_| Refusal::Unreadable)?;
        let at = rfind(&tail, b"PK\x05\x06").ok_or(Refusal::Undecodable)?;
        let end = &tail[at..];
        if end.len() < 22 {
            return Err(Refusal::Undecodable);
        }
        let mut count = u64::from(word16(end, 10));
        let mut directory = u64::from(word32(end, 16));
        let mut size = u64::from(word32(end, 12));
        if count == 0xFFFF || directory == 0xFFFF_FFFF {
            let (wide_count, wide_size, wide_directory) = zip64(&mut file, &tail, at)?;
            count = wide_count;
            size = wide_size;
            directory = wide_directory;
        }
        // **Where it says it is, and only then where it must be.** Everything a
        // zip records is an offset from the start of the *zip*, and a zip can
        // have anything in front of it — a self-extracting stub, or a download
        // that arrived with a prefix — so those offsets are short by however
        // much. The prefix is what is left after taking the directory's own size
        // off the end record's position.
        //
        // **A fallback rather than a correction**, because the arithmetic rests
        // on a recorded size, and a zip with a *wrong* size in it is a real
        // shape: Go's corpus keeps `test-baddirsz.zip` for it. Applying the shift
        // unconditionally read that one, and `test-badbase.zip`, and both zip64
        // files as empty. So the recorded offset is tried first and this is what
        // happens when there is nothing there.
        let mut entries = walk(&mut file, directory, count, 0)?;
        if entries.is_empty() && count > 0 {
            let ends_at = from + at as u64;
            let shift = ends_at
                .checked_sub(size)
                .and_then(|actual| actual.checked_sub(directory))
                .unwrap_or(0);
            if shift > 0 {
                entries = walk(&mut file, directory + shift, count, shift)?;
            }
        }
        Ok(Self { file, entries })
    }

    /// Writes entry `which`'s bytes into `out`, checking what it hashes to.
    ///
    /// **The check is not optional.** A file that came out of a corrupt archive
    /// looks exactly like a file, and the format carries the answer for free —
    /// so a mismatch stops the unpack with a reason rather than leaving something
    /// plausible on disk.
    pub fn extract(&mut self, which: usize, out: &mut dyn Write) -> Result<u64, Refusal> {
        let entry = self.entries.get(which).ok_or(Refusal::Undecodable)?;
        let (offset, compressed, method, crc) =
            (entry.offset, entry.compressed, entry.method, entry.crc);
        self.file.seek(SeekFrom::Start(offset)).map_err(|_| Refusal::Undecodable)?;
        let mut header = [0u8; LOCAL];
        self.file.read_exact(&mut header).map_err(|_| Refusal::Undecodable)?;
        if &header[0..4] != b"PK\x03\x04" {
            return Err(Refusal::Undecodable);
        }
        let skip = u64::from(word16(&header, 26)) + u64::from(word16(&header, 28));
        self.file.seek(SeekFrom::Current(skip.cast_signed())).map_err(|_| Refusal::Undecodable)?;
        let taken = (&mut self.file).take(compressed);
        let mut hasher = crc32fast::Hasher::new();
        let written = match method {
            STORED => drain(taken, out, &mut hasher)?,
            DEFLATE => drain(flate2::read::DeflateDecoder::new(taken), out, &mut hasher)?,
            // Bzip2, LZMA, zstd and the encrypted methods. Each is a decoder this
            // does not have, and the entry is named rather than written wrong.
            _ => return Err(Refusal::NoUnpacker),
        };
        if hasher.finalize() != crc {
            return Err(Refusal::Undecodable);
        }
        Ok(written)
    }
}

/// Copies a decoded stream out, hashing it on the way past.
fn drain(
    mut from: impl Read,
    out: &mut dyn Write,
    hasher: &mut crc32fast::Hasher,
) -> Result<u64, Refusal> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0;
    loop {
        let read = from.read(&mut buffer).map_err(|_| Refusal::Undecodable)?;
        if read == 0 {
            return Ok(written);
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(|_| Refusal::NoRoom)?;
        written += read as u64;
    }
}

/// The real entry count, directory size and directory offset, from the Zip64
/// records.
fn zip64(file: &mut File, tail: &[u8], end: usize) -> Result<(u64, u64, u64), Refusal> {
    // The locator sits immediately before the end record and points at the Zip64
    // end record, which is where the wide numbers are.
    let at = end.checked_sub(20).ok_or(Refusal::Undecodable)?;
    if &tail[at..at + 4] != b"PK\x06\x07" {
        return Err(Refusal::Undecodable);
    }
    let where_it_is = word64(tail, at + 8);
    let mut record = [0u8; 56];
    file.seek(SeekFrom::Start(where_it_is)).map_err(|_| Refusal::Undecodable)?;
    file.read_exact(&mut record).map_err(|_| Refusal::Undecodable)?;
    if &record[0..4] != b"PK\x06\x06" {
        return Err(Refusal::Undecodable);
    }
    Ok((word64(&record, 32), word64(&record, 40), word64(&record, 48)))
}

/// Reads `count` central-directory records from `at`.
fn walk(file: &mut File, at: u64, count: u64, shift: u64) -> Result<Vec<Central>, Refusal> {
    file.seek(SeekFrom::Start(at)).map_err(|_| Refusal::Undecodable)?;
    let mut reading = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    for _ in 0..count.min(sk_preview::archive::MOST_ENTRIES as u64) {
        let mut record = [0u8; CENTRAL];
        if reading.read_exact(&mut record).is_err() {
            break;
        }
        if &record[0..4] != b"PK\x01\x02" {
            break;
        }
        let (names, extras, comments) =
            (word16(&record, 28), word16(&record, 30), word16(&record, 32));
        let mut rest = vec![0u8; usize::from(names) + usize::from(extras) + usize::from(comments)];
        if reading.read_exact(&mut rest).is_err() {
            break;
        }
        let name = String::from_utf8_lossy(&rest[..usize::from(names)]).into_owned();
        let extra = &rest[usize::from(names)..usize::from(names) + usize::from(extras)];
        let mut size = u64::from(word32(&record, 24));
        let mut compressed = u64::from(word32(&record, 20));
        let mut offset = u64::from(word32(&record, 42));
        wide(extra, &mut size, &mut compressed, &mut offset);
        // The same prefix the directory itself was found past.
        offset += shift;
        // The high sixteen bits of the external attributes are a unix mode, where
        // the archive was made on a system that has one. Zero everywhere else,
        // which is why the execute bit is a bonus rather than the rule.
        let mode = word32(&record, 38) >> 16;
        let kind = if name.ends_with('/') || mode & 0o170_000 == 0o040_000 {
            Kind::Directory
        } else if mode & 0o170_000 == 0o120_000 {
            Kind::Link
        } else {
            Kind::File
        };
        entries.push(Central {
            entry: Entry {
                path: crate::place::tidy(&name),
                size: if kind == Kind::File { size } else { 0 },
                mtime: stamped(extra)
                    .unwrap_or_else(|| dos_time(word16(&record, 12), word16(&record, 14))),
                kind,
            },
            method: word16(&record, 10),
            compressed,
            offset,
            crc: word32(&record, 16),
            mode,
        });
    }
    Ok(entries)
}

/// Replaces whatever the Zip64 extra field carries.
///
/// **Only the fields that were sentinels, and in that order.** The record is a
/// sequence, not a struct: an entry whose size fits and whose offset does not
/// carries one number, and reading it as the first field would give a file its
/// offset as its length.
fn wide(extra: &[u8], size: &mut u64, compressed: &mut u64, offset: &mut u64) {
    let Some(field) = field_of(extra, 0x0001) else { return };
    let mut at = 0;
    for (value, sentinel) in [(size, 0xFFFF_FFFF), (compressed, 0xFFFF_FFFF)] {
        if *value == sentinel && at + 8 <= field.len() {
            *value = word64(field, at);
            at += 8;
        }
    }
    if *offset == 0xFFFF_FFFF && at + 8 <= field.len() {
        *offset = word64(field, at);
    }
}

/// The modification time from the extended timestamp field, where there is one.
///
/// **Preferred over the MS-DOS fields**, which have no timezone at all: a zip
/// made in Tokyo and read in Berlin is eight hours out, and this field is the
/// fix the format grew for it.
fn stamped(extra: &[u8]) -> Option<i64> {
    let field = field_of(extra, 0x5455)?;
    let flags = field.first()?;
    (flags & 1 != 0 && field.len() >= 5).then(|| i64::from(word32(field, 1).cast_signed()))
}

/// One extra field's data, by its id.
fn field_of(extra: &[u8], want: u16) -> Option<&[u8]> {
    let mut at = 0;
    while at + 4 <= extra.len() {
        let id = word16(extra, at);
        let length = usize::from(word16(extra, at + 2));
        let from = at + 4;
        if from + length > extra.len() {
            return None;
        }
        if id == want {
            return Some(&extra[from..from + length]);
        }
        at = from + length;
    }
    None
}

/// An MS-DOS date and time as seconds since the epoch.
///
/// **Read as UTC, because the format says nothing about a zone.** Every reader
/// makes a different choice here and none of them is right; this one is at least
/// the same everywhere, which is what a file manager showing a date needs.
fn dos_time(time: u16, date: u16) -> i64 {
    let (day, month, year) =
        (i64::from(date & 0x1f), i64::from((date >> 5) & 0xf), 1980 + i64::from(date >> 9));
    if day == 0 || month == 0 {
        return 0;
    }
    let (second, minute, hour) =
        (i64::from((time & 0x1f) * 2), i64::from((time >> 5) & 0x3f), i64::from(time >> 11));
    days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
}

/// Days between 1970-01-01 and a calendar date.
///
/// Howard Hinnant's `days_from_civil`, which is the one piece of calendar
/// arithmetic this program needs and is exact for every date a zip can hold.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|window| window == needle)
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

    /// The date the whole world checks a calendar against, plus the epoch itself
    /// and a leap day.
    #[test]
    fn a_dos_date_is_the_date_it_says() {
        // 1980-01-01 00:00:00, which is the earliest a zip can express.
        assert_eq!(dos_time(0, (1 << 5) | 1), 315_532_800);
        // 2024-02-29 12:30:00: a leap day, an hour and a half past noon.
        let date = ((2024 - 1980) << 9) | (2 << 5) | 0x1d;
        assert_eq!(dos_time((12 << 11) | (30 << 5), date as u16), 1_709_209_800);
    }

    /// A day or month of zero is what an archiver writes when it has no date at
    /// all, and running it through the arithmetic yields a date in 1979.
    #[test]
    fn no_date_is_no_date_rather_than_1979() {
        assert_eq!(dos_time(0, 0), 0);
    }

    /// The Zip64 field is a sequence of the numbers that overflowed, so which
    /// eight bytes mean what depends on what was a sentinel.
    #[test]
    fn a_zip64_field_fills_only_the_fields_that_overflowed() {
        // Only the offset overflowed: its value is the *first* eight bytes.
        let mut field = Vec::from([0x01, 0x00, 0x08, 0x00]);
        field.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        let (mut size, mut compressed, mut offset) = (100, 40, 0xFFFF_FFFF);
        wide(&field, &mut size, &mut compressed, &mut offset);
        assert_eq!((size, compressed, offset), (100, 40, 5_000_000_000));

        // Size and offset both did: two numbers, in the order the record defines.
        let mut field = Vec::from([0x01, 0x00, 0x10, 0x00]);
        field.extend_from_slice(&9_000_000_000u64.to_le_bytes());
        field.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        let (mut size, mut compressed, mut offset) = (0xFFFF_FFFF, 40, 0xFFFF_FFFF);
        wide(&field, &mut size, &mut compressed, &mut offset);
        assert_eq!((size, compressed, offset), (9_000_000_000, 40, 5_000_000_000));
    }
}
