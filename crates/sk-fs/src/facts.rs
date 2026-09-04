// SPDX-License-Identifier: MIT OR Apache-2.0
//! Everything `statx` knows about one path, for the Properties panel.
//!
//! Separate from [`crate::scan`], which reads a directory's worth of entries and
//! asks for the four fields a *row* needs. This asks for all of them about one
//! path, and it is the only place that reads an owner, a mode or a link target —
//! a listing that fetched those would pay for a `getpwuid` per row.
//!
//! **Runs on a job.** One `statx`, one `readlink`, and a `statfs` for the
//! filesystem's name.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, StatxFlags};

/// What one path is.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// The path itself.
    pub path: PathBuf,
    /// Bytes, as the file claims.
    pub size: u64,
    /// Bytes it actually occupies. Smaller than `size` for a sparse file or a
    /// compressed filesystem, and larger for a small file in a big block.
    pub on_disk: u64,
    /// Permission bits, as `0o644`.
    pub mode: u16,
    /// Seconds since the epoch. Zero where the filesystem does not keep one.
    pub modified: i64,
    /// Seconds since the epoch, of the last read.
    pub accessed: i64,
    /// The birth time, which most filesystems have and some do not.
    pub created: Option<i64>,
    /// The owner and the group, as numbers. Names are a `getpwuid`, which is a
    /// lookup that can hit the network on a machine with LDAP configured, so it
    /// is left to the caller to decide whether it wants that.
    pub uid: u32,
    /// The group that owns it.
    pub gid: u32,
    /// How many names this inode has.
    pub links: u64,
    /// What it is.
    pub kind: Kind,
    /// Where a symlink points, unresolved.
    pub target: Option<PathBuf>,
    /// The filesystem type, as a name where one is known.
    pub filesystem: Option<&'static str>,
}

/// What a path names.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(missing_docs, reason = "each variant is the kind of that name")]
pub enum Kind {
    #[default]
    File,
    Directory,
    Symlink,
    /// A socket, a fifo, or a device node. One variant, because what a file
    /// manager does with each of them is the same: name it and refuse to open
    /// it.
    Special,
}

impl Kind {
    /// The catalogue key naming this kind.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::File => "kind.file",
            Self::Directory => "kind.folder",
            Self::Symlink => "kind.link",
            Self::Special => "kind.special",
        }
    }
}

/// Reads everything about `path`.
///
/// **Does not follow the link.** A symlink's own facts are what a person asked
/// for when they selected the symlink; where it points is `target`, and the
/// panel shows both.
///
/// **Runs on a job.**
#[must_use]
pub fn describe(path: &Path) -> Option<Facts> {
    let wanted = StatxFlags::TYPE
        | StatxFlags::MODE
        | StatxFlags::SIZE
        | StatxFlags::BLOCKS
        | StatxFlags::MTIME
        | StatxFlags::ATIME
        | StatxFlags::BTIME
        | StatxFlags::UID
        | StatxFlags::GID
        | StatxFlags::NLINK;
    let stat = rustix::fs::statx(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW, wanted).ok()?;

    let mode = stat.stx_mode;
    let kind = match u32::from(mode) & libc::S_IFMT {
        libc::S_IFDIR => Kind::Directory,
        libc::S_IFLNK => Kind::Symlink,
        libc::S_IFREG => Kind::File,
        _ => Kind::Special,
    };
    // `stx_btime` is only meaningful when the filesystem said it filled it in:
    // ext4 has it, and every other field is populated regardless, so an
    // unchecked read is a birth time of the epoch on filesystems that have none.
    let created = (stat.stx_mask & StatxFlags::BTIME.bits() != 0)
        .then_some(stat.stx_btime.tv_sec)
        .filter(|seconds| *seconds > 0);

    Some(Facts {
        path: path.to_path_buf(),
        size: stat.stx_size,
        // 512-byte blocks, which is what `statx` reports whatever the
        // filesystem's own block size is.
        on_disk: stat.stx_blocks * 512,
        mode: mode & 0o7777,
        modified: stat.stx_mtime.tv_sec,
        accessed: stat.stx_atime.tv_sec,
        created,
        uid: stat.stx_uid,
        gid: stat.stx_gid,
        links: u64::from(stat.stx_nlink),
        kind,
        target: (kind == Kind::Symlink).then(|| read_link(path)).flatten(),
        filesystem: crate::device::filesystem_name(path),
    })
}

/// Where a symlink points, as it is written.
fn read_link(path: &Path) -> Option<PathBuf> {
    let raw = rustix::fs::readlinkat(rustix::fs::CWD, path, Vec::new()).ok()?;
    Some(PathBuf::from(OsString::from(raw.into_string().ok()?)))
}

/// The mode as `rwxr-xr-x`, which is the form every Unix tool prints.
#[must_use]
pub fn permissions(mode: u16) -> String {
    let triple = |shift: u32| {
        let bits = (mode >> shift) & 0o7;
        [
            if bits & 0o4 != 0 { 'r' } else { '-' },
            if bits & 0o2 != 0 { 'w' } else { '-' },
            if bits & 0o1 != 0 { 'x' } else { '-' },
        ]
    };
    triple(6).into_iter().chain(triple(3)).chain(triple(0)).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Kind, describe, permissions};
    use std::path::PathBuf;

    fn fixture(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-facts-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_file_reports_its_size_its_mode_and_its_kind() {
        let root = fixture("file");
        let path = root.join("thing.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let facts = describe(&path).expect("described");
        assert_eq!(facts.size, 10);
        assert_eq!(facts.kind, Kind::File);
        assert_eq!(facts.links, 1);
        assert!(facts.modified > 0);
        assert!(facts.on_disk >= 512, "ten bytes still occupy a block");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The link's own facts, not the target's — which is what makes a broken
    /// link describable at all.
    #[test]
    fn a_symlink_describes_itself_and_says_where_it_points() {
        let root = fixture("link");
        let link = root.join("points-nowhere");
        std::os::unix::fs::symlink("/nonexistent-skerry-target", &link).unwrap();
        let facts = describe(&link).expect("described");
        assert_eq!(facts.kind, Kind::Symlink);
        assert_eq!(
            facts.target.as_deref(),
            Some(std::path::Path::new("/nonexistent-skerry-target"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_is_a_directory_and_a_missing_path_is_nothing() {
        let root = fixture("dir");
        assert_eq!(describe(&root).expect("described").kind, Kind::Directory);
        assert!(describe(&root.join("not-there")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_mode_prints_the_way_every_unix_tool_prints_it() {
        assert_eq!(permissions(0o644), "rw-r--r--");
        assert_eq!(permissions(0o755), "rwxr-xr-x");
        assert_eq!(permissions(0o000), "---------");
        assert_eq!(permissions(0o777), "rwxrwxrwx");
        // The setuid and sticky bits are not in the nine, and must not shift the
        // others: `04755` is still `rwxr-xr-x` here.
        assert_eq!(permissions(0o4755), "rwxr-xr-x");
    }
}
