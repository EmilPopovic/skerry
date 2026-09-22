// SPDX-License-Identifier: Apache-2.0
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
    /// Validated original location for a direct trash entry.
    pub original: Option<PathBuf>,
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
        original: crate::trash::contains(path)
            .then(|| crate::trash::read(path).ok().map(|entry| entry.original))
            .flatten(),
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

/// What the name service calls one path's owner and group.
///
/// Both are optional because a number with no account behind it is ordinary: a
/// file written inside a container, or an account deleted after its files.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Accounts {
    /// The identifiers these names were asked about, so an answer that arrives
    /// after the panel moved on can be recognised as stale.
    pub uid: u32,
    /// The group these names were asked about.
    pub gid: u32,
    /// What the owner is called.
    pub owner: Option<String>,
    /// What the group is called.
    pub group: Option<String>,
}

/// Asks the name service what a `uid` and a `gid` are called.
///
/// **Its own call, and its own job** — which is what [`Facts`]'s numbers say in
/// their own comment. `getpwuid_r` consults every source `/etc/nsswitch.conf`
/// names, and on a machine joined to a directory one of them is a network: the
/// answer can take as long as that server takes, and a caller that wanted a mode
/// and a size should not wait for it.
///
/// **Runs on a job.**
#[must_use]
pub fn accounts(uid: u32, gid: u32) -> Accounts {
    if sk_util::disabled("names") {
        // **The injection [ADR
        // 0037](../../../docs/adr/0037-runtime-degradation-is-visible-and-injectable.md)
        // asks for, in the shape this facility really fails in.** A directory
        // server that has gone away does not answer *no such account*; it does
        // not answer, and the caller waits — which is `tools/hung-mount` with a
        // socket in place of a mount. Ten minutes rather than forever, so a
        // worker parked by a test is given back to the pool.
        // A pool worker, not the UI thread, and only when `SKERRY_DISABLE` names
        // it: this is the hang being simulated, and the one-lookup-at-a-time bound
        // in `sk-app` keeps it to one worker.
        std::thread::sleep(std::time::Duration::from_secs(600)); // skerry-allow: no-blocking-ui
        return Accounts { uid, gid, owner: None, group: None };
    }
    Accounts { uid, gid, owner: owner_name(uid), group: group_name(gid) }
}

/// How much room a name lookup is given before it is treated as unanswerable.
///
/// `getpwuid_r` asks for a bigger buffer with `ERANGE` rather than truncating.
/// A megabyte is past any real passwd record and stops a broken NSS module from
/// being answered with an unbounded allocation.
const NAME_BUFFER_LIMIT: usize = 1 << 20;

fn owner_name(uid: u32) -> Option<String> {
    with_buffer(|buffer, buffer_len| {
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `getpwuid_r` writes `record` and the buffer it is handed, and
        // sets `found` to `record` or to null. Nothing else is read.
        let code =
            unsafe { libc::getpwuid_r(uid, &raw mut record, buffer, buffer_len, &raw mut found) };
        (code, (!found.is_null()).then_some(record.pw_name))
    })
}

fn group_name(gid: u32) -> Option<String> {
    with_buffer(|buffer, buffer_len| {
        let mut record: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as above, for the group file's record.
        let code =
            unsafe { libc::getgrgid_r(gid, &raw mut record, buffer, buffer_len, &raw mut found) };
        (code, (!found.is_null()).then_some(record.gr_name))
    })
}

/// Runs one of the `_r` lookups, growing its buffer while it asks for more.
///
/// The name is copied out before the buffer it points into is dropped, which is
/// the whole reason this is a function rather than two copies of a loop.
fn with_buffer(
    mut ask: impl FnMut(*mut libc::c_char, usize) -> (libc::c_int, Option<*mut libc::c_char>),
) -> Option<String> {
    let mut buffer = vec![0u8; 1024];
    loop {
        let (code, name) = ask(buffer.as_mut_ptr().cast(), buffer.len());
        if code == libc::ERANGE && buffer.len() < NAME_BUFFER_LIMIT {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return None;
        }
        let name = name?;
        // SAFETY: a record the lookup filled in points its name at the buffer
        // above, which is still alive here and outlives the copy.
        let name = unsafe { std::ffi::CStr::from_ptr(name) };
        return name.to_str().ok().map(str::to_owned);
    }
}

/// Whether this process can change `uid`'s file without being refused.
///
/// `chmod` is the owner's or root's, and nobody else's — so a control that would
/// always fail with `EPERM` is drawn as a statement rather than as a switch.
#[must_use]
pub fn mine(uid: u32) -> bool {
    let me = rustix::process::geteuid();
    me.is_root() || me.as_raw() == uid
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

    use super::{Kind, accounts, describe, mine, permissions};
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

    /// Root is the one account every Unix has, so it is the one name that can be
    /// asserted — and only if this machine has a passwd source at all, because a
    /// container with an empty one is a machine, not a failure.
    #[test]
    fn a_name_is_looked_up_and_an_identifier_nothing_owns_has_none() {
        let root = accounts(0, 0);
        assert_eq!(root.uid, 0);
        if let Some(name) = root.owner.as_deref() {
            assert_eq!(name, "root", "uid 0 resolved to something other than root");
        }
        // Past `nobody` and past any real account: a lookup that answered this
        // would be reading past the record it was given.
        let nobody = accounts(0xFFFF_FFFE, 0xFFFF_FFFE);
        assert_eq!(nobody.owner, None);
        assert_eq!(nobody.group, None);
    }

    /// A file this process just wrote is a file this process owns, which is the
    /// only way to know an account here without asking who is running.
    #[test]
    fn a_file_this_process_does_not_own_is_not_this_processs_to_change() {
        let root = fixture("owner");
        let path = root.join("ours.txt");
        std::fs::write(&path, b"contents").unwrap();
        let facts = describe(&path).expect("described");
        assert!(mine(facts.uid), "a file we just wrote was not ours to change");
        assert_eq!(
            mine(0xFFFF_FFFE),
            facts.uid == 0,
            "only root may change another account's file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
