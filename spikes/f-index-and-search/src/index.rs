//! The persistent index, in the dullest format that can work.
//!
//! [ADR 0008](../../../docs/adr/0008-persistent-index-is-required.md) decided
//! the index exists and declined to choose a format without numbers. This is the
//! format whose numbers make that decision easy or hard: a header, the name
//! arena written whole, and the parallel arrays after it. No framing, no
//! compression, no serialization crate — the arrays are already the layout we
//! want in memory, so writing them is a `write` and reading them is a `read`.
//!
//! If this is fast enough, the format ADR is a short document. If it is not, the
//! interesting question becomes which of mmap, compression or a sorted key
//! ordering buys the difference, and that is a better question to have than a
//! guess.

use std::io::{self, Read, Write};
use std::path::Path;

use crate::flat::Flat;

/// `SKIX`, then a format version. A file that does not start with these is not
/// ours, and a file whose version we do not know is not readable — both are
/// performance events per ADR 0008, never errors the user sees.
const MAGIC: [u8; 4] = *b"SKIX";
const VERSION: u32 = 1;

/// Header: magic, version, entry count, arena length, checksum.
const HEADER: usize = 4 + 4 + 8 + 8 + 8;

/// FNV-1a over the arena only. Enough to catch a truncated or corrupted file,
/// which is what a checksum is for here; it is not a security boundary, and the
/// index is written by the same user who reads it.
fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub fn write(flat: &Flat, path: &Path) -> io::Result<u64> {
    let file = std::fs::File::create(path)?;
    let mut out = io::BufWriter::with_capacity(1 << 20, file);

    out.write_all(&MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    out.write_all(&(flat.len() as u64).to_le_bytes())?;
    out.write_all(&(flat.names.len() as u64).to_le_bytes())?;
    out.write_all(&checksum(&flat.names).to_le_bytes())?;

    out.write_all(&flat.names)?;
    out.write_all(as_bytes(&flat.ends))?;
    out.write_all(as_bytes(&flat.size))?;
    out.write_all(as_bytes(&flat.mtime))?;
    out.write_all(as_bytes(&flat.flags))?;
    out.flush()?;

    Ok(std::fs::metadata(path)?.len())
}

pub fn read(path: &Path) -> io::Result<Flat> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; HEADER];
    file.read_exact(&mut header)?;

    if header[..4] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not an index"));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("index version {version}, this build reads {VERSION}"),
        ));
    }
    let entries = u64::from_le_bytes(header[8..16].try_into().expect("8 bytes")) as usize;
    let arena = u64::from_le_bytes(header[16..24].try_into().expect("8 bytes")) as usize;
    let stored = u64::from_le_bytes(header[24..32].try_into().expect("8 bytes"));

    let mut flat = Flat {
        names: vec![0u8; arena],
        ends: vec![0u32; entries],
        size: vec![0u64; entries],
        mtime: vec![0i64; entries],
        flags: vec![0u16; entries],
    };
    file.read_exact(&mut flat.names)?;
    if checksum(&flat.names) != stored {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "checksum mismatch"));
    }
    file.read_exact(as_bytes_mut(&mut flat.ends))?;
    file.read_exact(as_bytes_mut(&mut flat.size))?;
    file.read_exact(as_bytes_mut(&mut flat.mtime))?;
    file.read_exact(as_bytes_mut(&mut flat.flags))?;
    Ok(flat)
}

/// Little-endian on both architectures this project targets, so the arrays go to
/// disk as they sit in memory. An index is a cache of the local filesystem and is
/// never copied between machines; if that ever stops being true, the version in
/// the header is how this gets fixed.
fn as_bytes<T>(slice: &[T]) -> &[u8] {
    // SAFETY: reading any `T` as its own bytes is always defined, the length is
    // derived from the same slice, and the result is only ever written to a file.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

fn as_bytes_mut<T>(slice: &mut [T]) -> &mut [u8] {
    // SAFETY: as above; every bit pattern is valid for the integer types used
    // here, so filling this from a file cannot produce an invalid `T`.
    unsafe {
        std::slice::from_raw_parts_mut(
            slice.as_mut_ptr().cast::<u8>(),
            std::mem::size_of_val(slice),
        )
    }
}
