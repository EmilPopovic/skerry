// SPDX-License-Identifier: Apache-2.0
//! A filesystem that stops responding, for rule 1's canonical test.
//!
//! [03-principles.md](../../../docs/03-principles.md) states the failure as "any
//! frame exceeds 8 ms while an sshfs mount is unplugged mid-listing". An sshfs
//! mount to a host that has stopped answering is a network dependency, a second
//! machine and a race; a FUSE filesystem whose `readdir` sleeps is the same
//! syscall behaviour, mounted unprivileged, reproducible to the millisecond.
//!
//! It is a stand-in and the README says so. What it reproduces faithfully is the
//! part that matters: a `getdents64` that never returns, on a real mount, in
//! another process.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, INodeNo, MountOption, ReplyAttr,
    ReplyDirectory, ReplyEntry, Request,
};

/// How long a request hangs. Longer than the pool arm's measurement window, so
/// the mount is effectively dead rather than merely slow — and short enough that
/// the control arm, which blocks for the whole of it on purpose, still finishes.
pub const HANG: Duration = Duration::from_secs(10);

const TTL: Duration = Duration::from_secs(0);

pub struct HungFs {
    /// Incremented before sleeping, so the harness can prove a request actually
    /// reached the filesystem rather than failing earlier.
    pub entered: Arc<AtomicU64>,
}

impl HungFs {
    /// One event-loop thread serves this filesystem, so the first request to
    /// hang stops every later one too. That is what a dead mount does, and
    /// answering some requests while hanging on others would be a gentler thing
    /// than the one rule 1 names.
    fn stall(&self) {
        self.entered.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(HANG);
    }
}

impl Filesystem for HungFs {
    fn lookup(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEntry) {
        self.stall();
        reply.error(Errno::EIO);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        // The root answers, or the mount cannot be traversed at all and the test
        // would measure a failed mount instead of a hung one.
        if ino == INodeNo::ROOT {
            reply.attr(&TTL, &root_attr());
            return;
        }
        self.stall();
        reply.error(Errno::EIO);
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        reply: ReplyDirectory,
    ) {
        self.stall();
        reply.error(Errno::EIO);
    }
}

fn root_attr() -> FileAttr {
    let epoch = std::time::UNIX_EPOCH;
    FileAttr {
        ino: INodeNo::ROOT,
        size: 0,
        blocks: 0,
        atime: epoch,
        mtime: epoch,
        ctime: epoch,
        crtime: epoch,
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

/// Mounts on a background thread; dropping the guard unmounts.
pub struct Mount {
    _session: fuser::BackgroundSession,
}

pub fn mount(at: &Path, entered: Arc<AtomicU64>) -> std::io::Result<Mount> {
    std::fs::create_dir_all(at)?;
    // Config is non-exhaustive, so it is built by mutation rather than by
    // struct expression. No AutoUnmount: it requires AllowOther or AllowRoot,
    // which need user_allow_other in /etc/fuse.conf. Dropping the session
    // unmounts instead.
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("skerry-hung".into()), MountOption::RO];
    let session = fuser::spawn_mount(HungFs { entered }, at, &config)?;
    Ok(Mount { _session: session })
}
