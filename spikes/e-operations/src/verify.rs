// SPDX-License-Identifier: MIT OR Apache-2.0
//! Verification is always a comparison against the source. Nothing here asks the
//! copy engine what it did; a copy that reports success and lost a hole, an
//! xattr or a nanosecond has to be caught by looking at both files.

use std::io::Read;
use std::path::Path;

use rustix::fs::{self, fstat, openat, seek, AtFlags, Mode, OFlags, SeekFrom, Stat};
use rustix::io::Errno;

/// One data extent, as the filesystem reports it. Two files with identical bytes
/// and different extent maps are not the same file to a file manager: one of them
/// occupies a thousand times the space.
#[derive(PartialEq, Eq, Debug)]
pub struct Extents {
    pub runs: Vec<(u64, u64)>,
    pub size: u64,
    /// 512-byte blocks actually allocated, from `st_blocks`.
    pub blocks: u64,
}

pub fn extents(path: &Path) -> Result<Extents, Errno> {
    let fd = openat(fs::CWD, path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
    let st = fstat(&fd)?;
    let size = st.st_size as u64;
    let mut runs = Vec::new();
    let mut cursor = 0u64;
    while cursor < size {
        let data = match seek(&fd, SeekFrom::Data(cursor)) {
            Ok(d) => d,
            Err(Errno::NXIO) => break,
            Err(e) => return Err(e),
        };
        let hole = seek(&fd, SeekFrom::Hole(data)).unwrap_or(size).min(size);
        runs.push((data, hole - data));
        cursor = hole;
    }
    Ok(Extents { runs, size, blocks: st.st_blocks as u64 })
}

/// Streaming comparison. A hash would be shorter and would also tell us less:
/// the first differing offset is the thing worth knowing.
pub fn first_difference(a: &Path, b: &Path) -> Result<Option<u64>, std::io::Error> {
    let mut fa = std::fs::File::open(a)?;
    let mut fb = std::fs::File::open(b)?;
    let mut ba = vec![0u8; 1 << 20];
    let mut bb = vec![0u8; 1 << 20];
    let mut at = 0u64;
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb {
            return Ok(Some(at + na.min(nb) as u64));
        }
        if na == 0 {
            return Ok(None);
        }
        if let Some(i) = ba[..na].iter().zip(&bb[..nb]).position(|(x, y)| x != y) {
            return Ok(Some(at + i as u64));
        }
        at += na as u64;
    }
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

pub struct Meta {
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, i64),
    pub links: u64,
    pub ino: u64,
    pub dev: u64,
}

pub fn meta(path: &Path) -> Result<Meta, Errno> {
    of(fs::statat(fs::CWD, path, AtFlags::empty())?)
}

pub fn meta_nofollow(path: &Path) -> Result<Meta, Errno> {
    of(fs::statat(fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW)?)
}

fn of(st: Stat) -> Result<Meta, Errno> {
    Ok(Meta {
        mode: st.st_mode & 0o7777,
        size: st.st_size as u64,
        mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
        links: st.st_nlink as u64,
        ino: st.st_ino as u64,
        dev: st.st_dev as u64,
    })
}

/// Every difference, named. An empty list is the only pass.
pub fn meta_differences(a: &Meta, b: &Meta) -> Vec<String> {
    let mut out = Vec::new();
    if a.mode != b.mode {
        out.push(format!("mode {:o} vs {:o}", a.mode, b.mode));
    }
    if a.size != b.size {
        out.push(format!("size {} vs {}", a.size, b.size));
    }
    if a.mtime != b.mtime {
        // Reported as the difference rather than as two timestamps, because the
        // failure that matters is rounding to the second.
        let ns = (b.mtime.0 - a.mtime.0) * 1_000_000_000 + (b.mtime.1 - a.mtime.1);
        out.push(format!("mtime differs by {ns} ns"));
    }
    out
}

pub fn xattrs(path: &Path) -> Result<Vec<(String, Vec<u8>)>, Errno> {
    let mut names = vec![0u8; 64 * 1024];
    let len = match fs::listxattr(path, &mut names[..]) {
        Ok(n) => n,
        Err(Errno::NOTSUP) | Err(Errno::NODATA) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    let mut value = vec![0u8; 64 * 1024];
    for name in names[..len].split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let name = String::from_utf8_lossy(name).to_string();
        match fs::getxattr(path, &name[..], &mut value[..]) {
            Ok(n) => out.push((name, value[..n].to_vec())),
            Err(_) => out.push((name, Vec::new())),
        }
    }
    out.sort();
    Ok(out)
}

pub fn symlink_target(path: &Path) -> Result<Vec<u8>, Errno> {
    fs::readlinkat(fs::CWD, path, Vec::new()).map(|s| s.into_bytes())
}
