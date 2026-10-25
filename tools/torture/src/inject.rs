// SPDX-License-Identifier: Apache-2.0
//! A filesystem that fails on purpose, at an exact offset.
//!
//! The bar says `ENOSPC` handled cleanly mid-copy with no partial file left
//! behind. A real full disk is a poor instrument for that: filling one is slow,
//! and the byte at which it gives up depends on the allocator, so the test is not
//! repeatable and a regression would not be visible.
//!
//! This is an in-memory FUSE filesystem with injectable faults, so the failure
//! happens at byte *n* every time:
//!
//! ```text
//! inject <mountpoint> enospc=65536 eio-read=4096 short-write=512 no-xattr no-fallocate
//! ```
//!
//! `/dev/fuse` is world-accessible here and `fusermount3` is present, so none of
//! it needs root. It is an instrument and not a shipping path — nothing in
//! `sk-fs` will ever mount a filesystem.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Filesystem, Generation, INodeNo, LockOwner,
    MountOption, OpenFlags, Request, TimeOrNow, WriteFlags,
};

const TTL: Duration = Duration::from_secs(0);
const ROOT: u64 = 1;
/// The file the read-side faults are served from, so `eio-read` has something to
/// fail on.
const SOURCE: &str = "source.bin";

#[derive(Default, Clone, Copy)]
struct Faults {
    /// Accept exactly this many bytes across the whole filesystem, then refuse.
    enospc_after: Option<u64>,
    /// Serve reads below this offset and fail above it.
    eio_read_after: Option<u64>,
    /// Never accept more than this many bytes in one `write`.
    short_write: Option<usize>,
    no_xattr: bool,
    no_fallocate: bool,
}

struct Node {
    kind: FileType,
    perm: u16,
    data: Vec<u8>,
    xattrs: BTreeMap<OsString, Vec<u8>>,
    children: BTreeMap<OsString, u64>,
    mtime: SystemTime,
    atime: SystemTime,
}

impl Node {
    fn dir() -> Self {
        Self {
            kind: FileType::Directory,
            perm: 0o755,
            data: Vec::new(),
            xattrs: BTreeMap::new(),
            children: BTreeMap::new(),
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
        }
    }
    fn file(perm: u16) -> Self {
        Self {
            kind: FileType::RegularFile,
            perm,
            data: Vec::new(),
            xattrs: BTreeMap::new(),
            children: BTreeMap::new(),
            mtime: SystemTime::UNIX_EPOCH,
            atime: SystemTime::UNIX_EPOCH,
        }
    }
    fn attr(&self, ino: u64) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: self.data.len() as u64,
            // Deliberately the true allocated size: the sparse checks read this.
            blocks: (self.data.len() as u64).div_ceil(512),
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.mtime,
            crtime: self.mtime,
            kind: self.kind,
            perm: self.perm,
            nlink: if self.kind == FileType::Directory { 2 } else { 1 },
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

struct Inject {
    nodes: Mutex<BTreeMap<u64, Node>>,
    next_ino: AtomicU64,
    accepted: AtomicU64,
    faults: Faults,
}

impl Inject {
    fn new(faults: Faults, source_size: usize) -> Self {
        let mut nodes = BTreeMap::new();
        let mut root = Node::dir();
        let mut source = Node::file(0o644);
        source.data = (0..source_size).map(|i| ((i % 251) as u8) | 1).collect();
        root.children.insert(OsString::from(SOURCE), 2);
        nodes.insert(ROOT, root);
        nodes.insert(2, source);
        Self {
            nodes: Mutex::new(nodes),
            next_ino: AtomicU64::new(3),
            accepted: AtomicU64::new(0),
            faults,
        }
    }
}

impl Filesystem for Inject {
    fn lookup(&self, _r: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEntry) {
        let nodes = self.nodes.lock().unwrap();
        let Some(p) = nodes.get(&parent.0) else { return reply.error(Errno::ENOENT) };
        match p.children.get(name).and_then(|ino| nodes.get(ino).map(|n| (ino, n))) {
            Some((ino, n)) => reply.entry(&TTL, &n.attr(*ino), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _r: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: fuser::ReplyAttr) {
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(&ino.0) {
            Some(n) => reply.attr(&TTL, &n.attr(ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _r: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: fuser::ReplyAttr,
    ) {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(n) = nodes.get_mut(&ino.0) else { return reply.error(Errno::ENOENT) };
        if let Some(m) = mode {
            n.perm = (m & 0o7777) as u16;
        }
        if let Some(s) = size {
            // ftruncate. Growing produces zeros, which is what a hole reads as —
            // this filesystem has no holes, and the sparse checks skip it.
            n.data.resize(s as usize, 0);
        }
        if let Some(t) = mtime {
            n.mtime = resolve(t);
        }
        if let Some(t) = atime {
            n.atime = resolve(t);
        }
        reply.attr(&TTL, &n.attr(ino.0));
    }

    fn create(
        &self,
        _r: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.get(&parent.0).is_some_and(|p| p.children.contains_key(name)) {
            return reply.error(Errno::EEXIST);
        }
        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        nodes.insert(ino, Node::file((mode & 0o7777) as u16));
        if let Some(p) = nodes.get_mut(&parent.0) {
            p.children.insert(name.to_owned(), ino);
        }
        let attr = nodes[&ino].attr(ino);
        reply.created(&TTL, &attr, Generation(0), FileHandle(0), FopenFlags::empty());
    }

    fn mkdir(
        &self,
        _r: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: fuser::ReplyEntry,
    ) {
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.get(&parent.0).is_some_and(|p| p.children.contains_key(name)) {
            return reply.error(Errno::EEXIST);
        }
        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        let mut d = Node::dir();
        d.perm = (mode & 0o7777) as u16;
        nodes.insert(ino, d);
        if let Some(p) = nodes.get_mut(&parent.0) {
            p.children.insert(name.to_owned(), ino);
        }
        let attr = nodes[&ino].attr(ino);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn rmdir(&self, _r: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(&ino) = nodes.get(&parent.0).and_then(|p| p.children.get(name)) else {
            return reply.error(Errno::ENOENT);
        };
        if nodes.get(&ino).is_some_and(|d| !d.children.is_empty()) {
            return reply.error(Errno::ENOTEMPTY);
        }
        nodes.remove(&ino);
        if let Some(p) = nodes.get_mut(&parent.0) {
            p.children.remove(name);
        }
        reply.ok()
    }

    /// Symlinks, so a tree copy has somewhere to put them. The target is stored
    /// as the node's data, which is what a symlink is.
    fn symlink(
        &self,
        _r: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: fuser::ReplyEntry,
    ) {
        let mut nodes = self.nodes.lock().unwrap();
        if nodes.get(&parent.0).is_some_and(|p| p.children.contains_key(link_name)) {
            return reply.error(Errno::EEXIST);
        }
        let ino = self.next_ino.fetch_add(1, Ordering::SeqCst);
        let mut n = Node::file(0o777);
        n.kind = FileType::Symlink;
        n.data = target.as_os_str().as_encoded_bytes().to_vec();
        nodes.insert(ino, n);
        if let Some(p) = nodes.get_mut(&parent.0) {
            p.children.insert(link_name.to_owned(), ino);
        }
        let attr = nodes[&ino].attr(ino);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn readlink(&self, _r: &Request, ino: INodeNo, reply: fuser::ReplyData) {
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(&ino.0) {
            Some(n) if n.kind == FileType::Symlink => reply.data(&n.data),
            Some(_) => reply.error(Errno::EINVAL),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _r: &Request, _ino: INodeNo, _flags: OpenFlags, reply: fuser::ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _r: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: fuser::ReplyData,
    ) {
        // The read-side fault: a disk that has started to fail. Everything below
        // the threshold reads fine, which is what makes it dangerous.
        if let Some(limit) = self.faults.eio_read_after {
            if offset >= limit {
                return reply.error(Errno::EIO);
            }
        }
        let nodes = self.nodes.lock().unwrap();
        let Some(n) = nodes.get(&ino.0) else { return reply.error(Errno::ENOENT) };
        let start = (offset as usize).min(n.data.len());
        let mut end = (start + size as usize).min(n.data.len());
        if let Some(limit) = self.faults.eio_read_after {
            end = end.min(limit as usize);
        }
        reply.data(&n.data[start..end]);
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _r: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: fuser::ReplyWrite,
    ) {
        let mut accept = match self.faults.short_write {
            Some(n) => data.len().min(n),
            None => data.len(),
        };
        // ENOSPC counted across the whole filesystem, so the failure lands at the
        // same byte on every run regardless of how the caller chunks its writes.
        if let Some(limit) = self.faults.enospc_after {
            let already = self.accepted.load(Ordering::SeqCst);
            let room = limit.saturating_sub(already);
            if room == 0 {
                return reply.error(Errno::ENOSPC);
            }
            accept = accept.min(room as usize);
        }
        let mut nodes = self.nodes.lock().unwrap();
        let Some(n) = nodes.get_mut(&ino.0) else { return reply.error(Errno::ENOENT) };
        let end = offset as usize + accept;
        if n.data.len() < end {
            n.data.resize(end, 0);
        }
        n.data[offset as usize..end].copy_from_slice(&data[..accept]);
        self.accepted.fetch_add(accept as u64, Ordering::SeqCst);
        reply.written(accept as u32);
    }

    fn unlink(&self, _r: &Request, parent: INodeNo, name: &OsStr, reply: fuser::ReplyEmpty) {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(p) = nodes.get_mut(&parent.0) else { return reply.error(Errno::ENOENT) };
        match p.children.remove(name) {
            Some(ino) => {
                if let Some(n) = nodes.remove(&ino) {
                    // Space is reclaimed, which is what makes a retry after
                    // ENOSPC behave like a real filesystem.
                    self.accepted.fetch_sub(n.data.len() as u64, Ordering::SeqCst);
                }
                reply.ok()
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn rename(
        &self,
        _r: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: fuser::ReplyEmpty,
    ) {
        // RENAME_NOREPLACE has to be honoured, because the engine relies on it to
        // refuse an existing destination atomically. A filesystem that ignores the
        // flag turns that refusal into a silent overwrite.
        const NOREPLACE: u32 = 1;
        const EXCHANGE: u32 = 2;
        let raw = flags.bits();
        if raw & EXCHANGE != 0 {
            return reply.error(Errno::EINVAL);
        }
        let mut nodes = self.nodes.lock().unwrap();
        let exists = nodes.get(&newparent.0).is_some_and(|p| p.children.contains_key(newname));
        if exists && raw & NOREPLACE != 0 {
            return reply.error(Errno::EEXIST);
        }
        let Some(ino) = nodes.get_mut(&parent.0).and_then(|p| p.children.remove(name)) else {
            return reply.error(Errno::ENOENT);
        };
        if let Some(p) = nodes.get_mut(&newparent.0) {
            if let Some(old) = p.children.insert(newname.to_owned(), ino) {
                nodes.remove(&old);
            }
        }
        reply.ok()
    }

    fn readdir(
        &self,
        _r: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let nodes = self.nodes.lock().unwrap();
        let Some(d) = nodes.get(&ino.0) else { return reply.error(Errno::ENOENT) };
        let mut entries: Vec<(u64, FileType, OsString)> = vec![
            (ino.0, FileType::Directory, OsString::from(".")),
            (ROOT, FileType::Directory, OsString::from("..")),
        ];
        for (name, child) in &d.children {
            let kind = nodes.get(child).map(|n| n.kind).unwrap_or(FileType::RegularFile);
            entries.push((*child, kind, name.clone()));
        }
        for (i, (child, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn setxattr(
        &self,
        _r: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: fuser::ReplyEmpty,
    ) {
        if self.faults.no_xattr {
            return reply.error(Errno::ENOTSUP);
        }
        let mut nodes = self.nodes.lock().unwrap();
        match nodes.get_mut(&ino.0) {
            Some(n) => {
                n.xattrs.insert(name.to_owned(), value.to_vec());
                reply.ok()
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getxattr(&self, _r: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: fuser::ReplyXattr) {
        if self.faults.no_xattr {
            return reply.error(Errno::ENOTSUP);
        }
        let nodes = self.nodes.lock().unwrap();
        let Some(v) = nodes.get(&ino.0).and_then(|n| n.xattrs.get(name)) else {
            return reply.error(Errno::ENODATA);
        };
        if size == 0 {
            reply.size(v.len() as u32)
        } else {
            reply.data(v)
        }
    }

    fn listxattr(&self, _r: &Request, ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        if self.faults.no_xattr {
            return reply.error(Errno::ENOTSUP);
        }
        let nodes = self.nodes.lock().unwrap();
        let Some(n) = nodes.get(&ino.0) else { return reply.error(Errno::ENOENT) };
        let mut buf = Vec::new();
        for name in n.xattrs.keys() {
            buf.extend_from_slice(name.as_encoded_bytes());
            buf.push(0);
        }
        if size == 0 {
            reply.size(buf.len() as u32)
        } else {
            reply.data(&buf)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fallocate(
        &self,
        _r: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        _mode: i32,
        reply: fuser::ReplyEmpty,
    ) {
        if self.faults.no_fallocate {
            return reply.error(Errno::EOPNOTSUPP);
        }
        let mut nodes = self.nodes.lock().unwrap();
        match nodes.get_mut(&ino.0) {
            Some(n) => {
                let end = (offset + length) as usize;
                if n.data.len() < end {
                    n.data.resize(end, 0);
                }
                reply.ok()
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn statfs(&self, _r: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // A small filesystem, so that anything asking "will this fit" gets an
        // answer consistent with the injected ENOSPC.
        let total = self.faults.enospc_after.unwrap_or(64 << 20);
        let used = self.accepted.load(Ordering::SeqCst);
        let free = total.saturating_sub(used);
        reply.statfs(total / 4096, free / 4096, free / 4096, 1 << 16, 1 << 15, 4096, 255, 4096);
    }

    fn fsync(&self, _r: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: fuser::ReplyEmpty) {
        reply.ok()
    }

    fn flush(
        &self,
        _r: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock: LockOwner,
        reply: fuser::ReplyEmpty,
    ) {
        reply.ok()
    }
}

fn resolve(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(s) => s,
        TimeOrNow::Now => SystemTime::now(),
    }
}

pub fn main(args: &[String]) {
    let Some(mountpoint) = args.first() else {
        eprintln!("inject <mountpoint> [enospc=N] [eio-read=N] [short-write=N] [no-xattr] [no-fallocate] [source=N]");
        std::process::exit(2);
    };
    let mut faults = Faults::default();
    let mut source_size = 1 << 20;
    for a in &args[1..] {
        let (k, v) = a.split_once('=').unwrap_or((a.as_str(), ""));
        let n = || v.parse::<u64>().unwrap_or(0);
        match k {
            "enospc" => faults.enospc_after = Some(n()),
            "eio-read" => faults.eio_read_after = Some(n()),
            "short-write" => faults.short_write = Some(n() as usize),
            "no-xattr" => faults.no_xattr = true,
            "no-fallocate" => faults.no_fallocate = true,
            "source" => source_size = n() as usize,
            other => {
                eprintln!("unknown fault {other:?}");
                std::process::exit(2);
            }
        }
    }
    std::fs::create_dir_all(mountpoint).ok();
    println!(
        "injecting at {mountpoint}: enospc {:?} eio-read {:?} short-write {:?} no-xattr {} \
         no-fallocate {}; {SOURCE} is {source_size} bytes",
        faults.enospc_after, faults.eio_read_after, faults.short_write, faults.no_xattr,
        faults.no_fallocate
    );
    let mut config = fuser::Config::default();
    // `AutoUnmount` is refused with the default owner-only ACL, and widening the
    // ACL to get it would let any user on the machine read the mount. The caller
    // unmounts with `fusermount3 -u` instead.
    config.mount_options = vec![MountOption::FSName("skerry-inject".into())];
    if let Err(e) = fuser::mount(Inject::new(faults, source_size), mountpoint, &config) {
        eprintln!("mount failed: {e}");
        std::process::exit(3);
    }
}
