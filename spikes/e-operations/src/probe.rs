//! What each filesystem can actually do, asked at runtime.
//!
//! [ADR 0007](../../docs/adr/0007-runtime-capability-detection.md) says
//! capabilities are probed and never compiled in. That rule is usually about the
//! desktop; it matters more here. "ext4 does not support reflink" is true of the
//! ext4 in this kernel and is not a property of the name — and the filesystem
//! under a user's `~/Documents` is not knowable at build time in any case.

use std::path::{Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};
use rustix::io::Errno;

#[derive(Debug)]
pub struct Capabilities {
    pub fstype: String,
    pub reflink: Result<(), Errno>,
    pub copy_file_range: Result<(), Errno>,
    pub user_xattr: Result<(), Errno>,
    pub fallocate: Result<(), Errno>,
    pub sparse: bool,
    /// Whether POSIX mode bits survive a round trip. ntfs-3g reports 0o777 for
    /// everything unless mounted with `permissions`, so "the mode was not
    /// preserved" is a fact about the filesystem and not about the copy.
    pub mode_bits: bool,
    pub hardlinks: Result<(), Errno>,
    pub symlinks: Result<(), Errno>,
    /// The rounding error this filesystem applied to a known timestamp, in
    /// nanoseconds — measured, not assumed. ext4 returns 0, NTFS returns 89 for
    /// this probe because its resolution is 100 ns, and FAT would return most of
    /// a second. It is an *observed error bound* rather than the grain itself,
    /// which is the useful quantity: a copy whose error is no larger than the
    /// probe's has preserved everything the filesystem can hold.
    pub time_residue_ns: u64,
}

pub struct Target {
    pub label: String,
    pub dir: PathBuf,
    pub caps: Capabilities,
}

/// The filesystem type for a path, from `/proc/self/mountinfo` — the longest
/// mount point that is a prefix of it.
pub fn fstype_of(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return "unknown".into();
    };
    let mut best: Option<(usize, String)> = None;
    for line in text.lines() {
        // mountinfo: id parent major:minor root mount-point options... - fstype source ...
        let mut parts = line.split(" - ");
        let (left, right) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let Some(mount_point) = left.split_whitespace().nth(4) else { continue };
        let mut rf = right.split_whitespace();
        let Some(fstype) = rf.next() else { continue };
        // `fuse` and `fuseblk` say nothing about what the filesystem actually is.
        // The source field does: `ntfs-3g`, `/dev/loop0`, `skerry-inject`.
        let fstype = if fstype.starts_with("fuse") {
            match rf.next() {
                Some(src) if !src.starts_with('/') => format!("{fstype}.{src}"),
                _ => fstype.to_string(),
            }
        } else {
            fstype.to_string()
        };
        let mp = unescape(mount_point);
        if canon.starts_with(&mp) && best.as_ref().map_or(true, |(n, _)| mp.len() > *n) {
            best = Some((mp.len(), fstype));
        }
    }
    best.map(|(_, t)| t).unwrap_or_else(|| "unknown".into())
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub fn capabilities(dir: &Path) -> std::io::Result<Capabilities> {
    let fstype = fstype_of(dir);
    let a = dir.join(".skerry-probe-a");
    let b = dir.join(".skerry-probe-b");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    crate::fixture::write_pattern(&a, 65536)?;

    let fd_a = fs::openat(fs::CWD, &a, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())?;
    let fd_b = fs::openat(
        fs::CWD,
        &b,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?;

    let reflink = fs::ioctl_ficlone(&fd_b, &fd_a).map_err(|e| e);
    // A reflink leaves the destination populated, so the next probe needs a clean
    // one — otherwise copy_file_range is measured against a file it has already
    // filled.
    let _ = fs::ftruncate(&fd_b, 0);
    let mut off_in = 0u64;
    let mut off_out = 0u64;
    let copy_file_range =
        fs::copy_file_range(&fd_a, Some(&mut off_in), &fd_b, Some(&mut off_out), 4096).map(|_| ());

    let user_xattr = fs::fsetxattr(&fd_b, "user.skerry.probe", b"x", fs::XattrFlags::empty());
    let fallocate = fs::fallocate(&fd_b, fs::FallocateFlags::empty(), 0, 4096);

    // Sparse support: write one byte a megabyte in and see whether the
    // filesystem reports a hole before it.
    let sparse = {
        let _ = fs::ftruncate(&fd_b, 0);
        let _ = fs::seek(&fd_b, fs::SeekFrom::Start(1 << 20));
        let _ = rustix::io::write(&fd_b, b"x");
        fs::seek(&fd_b, fs::SeekFrom::Hole(0)).map(|h| h == 0).unwrap_or(false)
    };

    let link = dir.join(".skerry-probe-link");
    let _ = std::fs::remove_file(&link);
    let hardlinks = fs::linkat(fs::CWD, &a, fs::CWD, &link, fs::AtFlags::empty());
    let _ = std::fs::remove_file(&link);
    let symlinks = fs::symlinkat("target", fs::CWD, &link);
    let _ = std::fs::remove_file(&link);

    let mode_bits = {
        let want = Mode::from_bits_truncate(0o4755);
        fs::fchmod(&fd_b, want).is_ok()
            && fs::fstat(&fd_b).map(|st| st.st_mode & 0o7777 == 0o4755).unwrap_or(false)
    };
    let time_residue_ns = time_residue(&fd_b);

    drop(fd_a);
    drop(fd_b);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);

    Ok(Capabilities {
        fstype,
        reflink,
        copy_file_range,
        user_xattr,
        fallocate,
        sparse,
        mode_bits,
        hardlinks,
        symlinks,
        time_residue_ns,
    })
}

/// Set an odd nanosecond timestamp and read back what survived. The difference is
/// the only honest input to "was the timestamp preserved".
fn time_residue(fd: &rustix::fd::OwnedFd) -> u64 {
    let want = fs::Timespec { tv_sec: 1_600_000_000, tv_nsec: 123_456_789 };
    if fs::futimens(fd, &fs::Timestamps { last_access: want, last_modification: want }).is_err() {
        return u64::MAX;
    }
    let Ok(st) = fs::fstat(fd) else { return u64::MAX };
    let got_ns = st.st_mtime_nsec as i64;
    let got_s = st.st_mtime as i64;
    if got_s != 1_600_000_000 {
        // Rounded to something coarser than a second.
        return ((got_s - 1_600_000_000).unsigned_abs()).max(1) * 1_000_000_000;
    }
    (123_456_789 - got_ns).unsigned_abs()
}

/// A directory that was asked for and could not be used, with the reason.
pub struct Skipped {
    pub dir: PathBuf,
    pub reason: String,
    /// True when the caller named this directory explicitly. A built-in candidate
    /// that is absent is uninteresting; one the caller asked for is not.
    pub named: bool,
}

/// Directories worth testing on this machine, one per distinct filesystem, plus
/// anything the caller names. Discovery rather than a list: the point of this
/// spike is to be re-runnable on a machine with filesystems this one does not
/// have.
///
/// Everything it *cannot* use is returned too. The first version of this silently
/// skipped a directory it could not create, and a run under `sudo` — where `$HOME`
/// still points at root's home and the loopback mounts are owned by root — lost
/// ext4, exFAT and vfat without printing a word. A target that was asked for and
/// quietly dropped is the exact failure `12-measurement.md` calls a test that can
/// only pass.
pub fn discover(extra: &[String]) -> (Vec<Target>, Vec<Skipped>) {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let builtin: Vec<PathBuf> = [
        format!("{home}/skerry-spike-e"),
        "/tmp/skerry-spike-e".into(),
        "/dev/shm/skerry-spike-e".into(),
    ]
    .iter()
    .map(PathBuf::from)
    .collect();

    let candidates: Vec<(PathBuf, bool)> = builtin
        .into_iter()
        .map(|p| (p, false))
        .chain(extra.iter().map(|e| (PathBuf::from(e), true)))
        .collect();

    for (dir, named) in candidates {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            skipped.push(Skipped { dir, reason: format!("cannot create the directory: {e}"), named });
            continue;
        }
        // Creatable is not the same as writable: a directory owned by root under a
        // loopback mount is both.
        let probe_file = dir.join(".skerry-writable");
        if let Err(e) = std::fs::write(&probe_file, b"x") {
            skipped.push(Skipped { dir, reason: format!("not writable: {e}"), named });
            continue;
        }
        let _ = std::fs::remove_file(&probe_file);

        let fstype = fstype_of(&dir);
        if !named && seen.contains(&fstype) {
            let _ = std::fs::remove_dir(&dir);
            continue;
        }
        seen.push(fstype.clone());
        match capabilities(&dir) {
            Ok(caps) => out.push(Target { label: format!("{fstype} at {}", dir.display()), dir, caps }),
            Err(e) => {
                skipped.push(Skipped { dir, reason: format!("capability probe failed: {e}"), named })
            }
        }
    }
    (out, skipped)
}

pub fn describe(r: &Result<(), Errno>) -> String {
    match r {
        Ok(()) => "yes".into(),
        Err(e) => format!("no ({e:?})"),
    }
}
