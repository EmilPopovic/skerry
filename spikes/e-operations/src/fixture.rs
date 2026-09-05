// SPDX-License-Identifier: Apache-2.0
//! The tree that gets copied. Every property the bar cares about is a file in
//! here rather than a separate test, because a copy engine that handles them one
//! at a time and not together is not a copy engine — the interesting failures are
//! a sparse file inside a hardlink set, or an xattr on a name that is not valid
//! UTF-8.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{self, Mode, OFlags};

/// 1 GiB apparent, three extents, about 12 KiB on disk — on a filesystem that has
/// holes. A copy that expands them turns this into a gigabyte, which is the kind
/// of bug only noticed when the disk fills.
///
/// On a filesystem *without* holes it is written in full, and on a 1 GiB FAT image
/// that fills the disk and every later fixture fails with `ENOSPC`. So the size is
/// chosen from what the filesystem can do rather than fixed: see `sparse_plan`.
pub const SPARSE_SIZE: u64 = 1 << 30;

/// What size of sparse file this filesystem can hold, and why.
fn sparse_plan(root: &Path) -> Result<u64, String> {
    let st = rustix::fs::statvfs(root).map_err(|e| format!("statvfs failed: {e:?}"))?;
    let free = st.f_bavail * st.f_frsize;
    let holes = {
        // Ask directly rather than trusting the filesystem's name.
        let probe = root.join(".skerry-sparse-probe");
        let ok = std::fs::File::create(&probe)
            .and_then(|f| {
                f.set_len(1 << 20)?;
                Ok(rustix::fs::seek(&f, rustix::fs::SeekFrom::Hole(0)).map(|h| h == 0).unwrap_or(false))
            })
            .unwrap_or(false);
        let _ = std::fs::remove_file(&probe);
        ok
    };
    if !holes {
        return Err(format!(
            "the filesystem has no holes, so a {} MiB sparse file would be written in full and \
             fill a small volume",
            SPARSE_SIZE / (1 << 20)
        ));
    }
    // Room for the source *and* a copy of it, with margin. A hole costs nothing,
    // but a filesystem that reports holes and then materialises them on copy is
    // exactly the bug being looked for, so the space has to actually be there.
    if free / 4 >= SPARSE_SIZE {
        Ok(SPARSE_SIZE)
    } else if free / 4 >= 16 << 20 {
        Ok(free / 4)
    } else {
        Err(format!("only {} MiB free", free / (1 << 20)))
    }
}

pub struct Fixture {
    pub root: PathBuf,
    pub names: Vec<PathBuf>,
    /// What this filesystem would not let us create, and why. NTFS refuses a name
    /// that is not valid UTF-8 with `EILSEQ`, because its names are UTF-16 — so a
    /// fixture that insists on creating everything cannot be built on half the
    /// filesystems a user owns. What it refused is a *result*, not an obstacle.
    pub refused: Vec<(String, std::io::Error)>,
    /// The apparent size of the sparse fixture, if the filesystem could hold one.
    pub sparse_size: Option<u64>,
}

/// Every step is tolerant. A fixture that aborts on the first refusal tells us
/// only which refusal came first.
macro_rules! attempt {
    ($f:expr, $what:expr, $body:expr) => {
        match $body {
            Ok(v) => Some(v),
            Err(e) => {
                $f.refused.push(($what.to_string(), e));
                None
            }
        }
    };
}

impl Fixture {
    pub fn build(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let mut f = Self {
            root: root.to_path_buf(),
            names: Vec::new(),
            refused: Vec::new(),
            sparse_size: None,
        };

        // The floor.
        let plain = root.join("plain.bin");
        if attempt!(f, "plain file", write_pattern(&plain, 3 * 1024 * 1024 + 7)).is_some() {
            f.names.push(plain.clone());
        }

        // The sparse fixture is sized from the filesystem, not fixed. A 1 GiB
        // apparent file on a filesystem with no holes is 1 GiB of real data, and
        // on a small FAT volume that fills it and makes every later refusal a
        // cascade rather than a result.
        match sparse_plan(root) {
            Ok(size) => {
                let sparse = root.join("sparse.bin");
                if attempt!(f, "sparse file", make_sparse(&sparse, size)).is_some() {
                    f.sparse_size = Some(size);
                    f.names.push(sparse);
                }
            }
            Err(why) => f.refused.push((format!("sparse file — {why}"), std::io::Error::other(""))),
        }

        // Three names for one inode. A copy that breaks the set silently triples
        // the space and unlinks the relationship the user relied on.
        let link_a = root.join("hardlink-a");
        if attempt!(f, "hardlink source", write_pattern(&link_a, 65536)).is_some() {
            attempt!(f, "hardlink b", std::fs::hard_link(&link_a, root.join("hardlink-b")));
            attempt!(f, "hardlink c", std::fs::hard_link(&link_a, root.join("hardlink-c")));
            f.names.push(link_a);
        }

        // Symlinks: relative, absolute, dangling, and one whose target is not
        // valid UTF-8 — a target is bytes.
        attempt!(f, "relative symlink", std::os::unix::fs::symlink("plain.bin", root.join("link-relative")));
        attempt!(f, "absolute symlink", std::os::unix::fs::symlink(&plain, root.join("link-absolute")));
        attempt!(f, "dangling symlink", std::os::unix::fs::symlink("nowhere-at-all", root.join("link-dangling")));
        attempt!(f, "symlink to an invalid name", symlink_bytes(b"\xff\xfe-not-utf8", &root.join("link-invalid-target")));

        // Extended attributes, and a POSIX ACL — which is an xattr, so getting
        // xattrs right gets ACLs right and getting them wrong loses permissions
        // without saying so.
        let attrs = root.join("with-xattrs.bin");
        if attempt!(f, "xattr file", write_pattern(&attrs, 4096)).is_some() {
            let one = fs::setxattr(&attrs, "user.skerry.one", b"first", fs::XattrFlags::empty());
            let two = fs::setxattr(&attrs, "user.skerry.two", &[0u8, 1, 2, 255], fs::XattrFlags::empty());
            for (name, r) in [("user.skerry.one", one), ("user.skerry.two", two)] {
                if let Err(e) = r {
                    f.refused.push((name.to_string(), std::io::Error::from(e)));
                }
            }
            f.names.push(attrs);
        }

        // Mode with setuid set, and an mtime at an exact nanosecond that no
        // rounding can survive.
        let moded = root.join("mode-4755.bin");
        if attempt!(f, "mode file", write_pattern(&moded, 1024)).is_some() {
            attempt!(f, "setuid mode", std::fs::set_permissions(&moded, std::os::unix::fs::PermissionsExt::from_mode(0o4755)));
            attempt!(f, "nanosecond mtime", set_mtime(&moded, 1_600_000_000, 123_456_789));
            f.names.push(moded);
        }

        // Names that have broken real file managers.
        for name in adversarial_names() {
            let p = root.join(&name);
            let label = format!("name {:?}", name.to_string_lossy().chars().take(16).collect::<String>());
            if attempt!(f, label, write_pattern(&p, 512)).is_some() {
                f.names.push(p);
            }
        }

        // Depth, so the walk recurses rather than reading one directory. The
        // nested file is also a second hardlink set spanning two directories,
        // which is where an inode map keyed only on the inode number breaks.
        let nested = root.join("deeper/and-deeper");
        if attempt!(f, "nested directories", std::fs::create_dir_all(&nested)).is_some() {
            let inner = nested.join("nested.bin");
            if attempt!(f, "nested file", write_pattern(&inner, 32768)).is_some() {
                attempt!(f, "hardlink across directories", std::fs::hard_link(&inner, root.join("deeper/linked-across-directories")));
                f.names.push(inner);
            }
        }

        // A read-only file. The failure has to be reported, not skipped.
        let ro = root.join("read-only.bin");
        if attempt!(f, "read-only file", write_pattern(&ro, 2048)).is_some() {
            attempt!(f, "read-only mode", std::fs::set_permissions(&ro, std::os::unix::fs::PermissionsExt::from_mode(0o444)));
            f.names.push(ro);
        }

        Ok(f)
    }

    pub fn has(&self, name: &str) -> bool {
        self.root.join(name).exists()
    }

    /// Whether the source has a symlink at this name. A destination cannot be
    /// faulted for lacking what the source never had, and on a FAT filesystem the
    /// source never has any.
    pub fn has_symlink(&self, name: &str) -> bool {
        std::fs::symlink_metadata(self.root.join(name))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    }
}

/// Four names, each of which has broken something shipped. The 255-byte one is
/// the filesystem's limit; the newline breaks anything that parses `ls`; the
/// invalid UTF-8 breaks anything that assumes a path is a `String`; the leading
/// dash breaks anything that builds a command line.
pub fn adversarial_names() -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let mut v = Vec::new();
    v.push(OsString::from("a".repeat(255)));
    v.push(OsString::from("has\na\nnewline"));
    v.push(std::ffi::OsString::from_vec(b"invalid-\xff\xfe-utf8".to_vec()));
    v.push(OsString::from("-rf"));
    v
}

trait FromVec {
    fn from_vec(v: Vec<u8>) -> Self;
}
impl FromVec for std::ffi::OsString {
    fn from_vec(v: Vec<u8>) -> Self {
        std::os::unix::ffi::OsStringExt::from_vec(v)
    }
}

/// A pattern rather than zeros: zeros are indistinguishable from a hole, so a
/// copy that turned data into a hole would pass a byte comparison.
pub fn write_pattern(path: &Path, len: usize) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let mut buf = vec![0u8; 65536];
    let mut written = 0;
    while written < len {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((written + i) % 251) as u8 | 1;
        }
        let n = buf.len().min(len - written);
        f.write_all(&buf[..n])?;
        written += n;
    }
    Ok(())
}

/// Three extents: one at the start, one a megabyte in, one at the very end. The
/// last one matters — a copy that gets the length from the last write rather than
/// from the source's size truncates a file that ends in a hole.
fn make_sparse(path: &Path, size: u64) -> std::io::Result<()> {
    use std::io::Seek;
    let mut f = std::fs::File::create(path)?;
    for offset in [0, 1 << 20, size - 4096] {
        f.seek(std::io::SeekFrom::Start(offset))?;
        let mut chunk = vec![0u8; 4096];
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (i % 251) as u8 | 1;
        }
        f.write_all(&chunk)?;
    }
    f.set_len(size)?;
    Ok(())
}

fn symlink_bytes(target: &[u8], path: &Path) -> std::io::Result<()> {
    let t = std::ffi::OsStr::from_bytes(target);
    std::os::unix::fs::symlink(t, path)
}

fn set_mtime(path: &Path, secs: i64, nanos: i64) -> std::io::Result<()> {
    let fd = fs::openat(fs::CWD, path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())?;
    let ts = fs::Timespec { tv_sec: secs as _, tv_nsec: nanos as _ };
    fs::futimens(&fd, &fs::Timestamps { last_access: ts, last_modification: ts })?;
    Ok(())
}
