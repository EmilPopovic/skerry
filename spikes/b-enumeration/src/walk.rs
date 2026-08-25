//! The four arms.
//!
//! All four produce the same columnar store from the same tree, so the
//! difference between their numbers is the difference between the syscalls they
//! make and the allocations they make on the way.
//!
//! What 04-architecture claims and this file exists to price: `read_dir`
//! allocates a `PathBuf` and a `DirEntry` per entry and hides a `stat` behind
//! `metadata()`, while `getdents64` returns a packed buffer of names with
//! `d_type` already in it, and `statx` can then be deferred to a background pass
//! that fills columns in as they arrive.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxFlags};

use crate::pool::{Cancel, Pool};
use crate::store::{Chunk, Store, FLAG_DIR, FLAG_LINK, FLAG_STATTED};

/// Whether a chunk is shrunk to fit before it is parked in the store. A global
/// because the walkers are reached through a `'static` job boundary, and
/// threading one flag through all of them would be more code than the flag is
/// worth in a spike. Set once before a walk, read many times.
static SHRINK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_shrink(on: bool) {
    SHRINK.store(on, Ordering::Relaxed);
}

fn shrink_enabled() -> bool {
    SHRINK.load(Ordering::Relaxed)
}

/// 32 KiB. Large enough that a 100,000-entry directory takes tens of
/// `getdents64` calls rather than thousands, small enough to stay in L2.
const DIRENT_BUFFER: usize = 32 * 1024;

/// Entries handed to `statx` per job. Small enough that cancelling is prompt,
/// large enough that the queueing cost is amortised.
const STATX_BATCH: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    ReadDir,
    ReadDirMeta,
    Getdents,
    GetdentsStatx,
    DirfdStatx,
    Parallel,
}

impl Arm {
    pub fn label(self) -> &'static str {
        match self {
            Arm::ReadDir => "read_dir",
            Arm::ReadDirMeta => "read_dir+meta",
            Arm::Getdents => "getdents64",
            Arm::GetdentsStatx => "getdents64+statx",
            Arm::DirfdStatx => "dirfd-statx",
            Arm::Parallel => "parallel",
        }
    }

    pub fn all() -> [Arm; 6] {
        [
            Arm::ReadDir,
            Arm::ReadDirMeta,
            Arm::Getdents,
            Arm::GetdentsStatx,
            Arm::DirfdStatx,
            Arm::Parallel,
        ]
    }

    /// Whether the arm fills the size and mtime columns. Only the two that do
    /// are comparable to each other; the other two are the floors they sit on.
    pub fn stats(self) -> bool {
        matches!(
            self,
            Arm::ReadDirMeta | Arm::GetdentsStatx | Arm::DirfdStatx | Arm::Parallel
        )
    }
}

pub fn walk(arm: Arm, root: &Path, store: &Arc<Store>, pool: &Pool, cancel: &Cancel) -> u64 {
    match arm {
        Arm::ReadDir => walk_read_dir(root, store, cancel, false),
        Arm::ReadDirMeta => walk_read_dir(root, store, cancel, true),
        Arm::Getdents => walk_getdents(root, store, cancel, None),
        Arm::GetdentsStatx => walk_getdents(root, store, cancel, Some((pool, store))),
        Arm::DirfdStatx => walk_dirfd(root, store, cancel, pool, false),
        Arm::Parallel => walk_dirfd(root, store, cancel, pool, true),
    }
}

/// Arms 1 and 2. The `metadata()` call is the whole difference between them, and
/// it is a `statx` per entry issued from the walking thread — which is both the
/// ergonomic path and the one rule 1 forbids on the UI thread.
fn walk_read_dir(root: &Path, store: &Arc<Store>, cancel: &Cancel, with_meta: bool) -> u64 {
    let mut directories = vec![root.to_path_buf()];
    let mut visited = 0u64;
    let mut chunk = Chunk::with_capacity(1024);

    while let Some(directory) = directories.pop() {
        if cancel.is_cancelled() {
            break;
        }
        // skerry-allow: no-read-dir — this arm exists to price read_dir
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        visited += 1;
        chunk.clear();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut flags = 0;
            let (mut size, mut mtime) = (0u64, 0i64);

            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    flags |= FLAG_DIR;
                    directories.push(entry.path());
                }
                Ok(kind) if kind.is_symlink() => flags |= FLAG_LINK,
                _ => {}
            }

            if with_meta {
                if let Ok(meta) = entry.metadata() {
                    size = meta.len();
                    mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs() as i64);
                    flags |= FLAG_STATTED;
                    store.statx_calls.fetch_add(1, Ordering::Relaxed);
                }
            }
            chunk.push(&name, flags, size, mtime);
        }
        store.push_chunk(std::mem::take(&mut chunk));
    }
    visited
}

/// Arms 3 and 4. One `openat` and a handful of `getdents64` per directory, with
/// `d_type` giving file-versus-directory for free. When `statx` is wanted it is
/// batched onto the pool rather than issued inline, so the walk never waits for
/// it — which is the property the hung-mount test depends on.
fn walk_getdents(
    root: &Path,
    store: &Arc<Store>,
    cancel: &Cancel,
    statx_to: Option<(&Pool, &Arc<Store>)>,
) -> u64 {
    let mut directories = vec![root.to_path_buf()];
    let mut visited = 0u64;
    let mut buffer = vec![MaybeUninit::<u8>::uninit(); DIRENT_BUFFER];
    let mut chunk = Chunk::with_capacity(1024);
    let mut pending: Vec<PathBuf> = Vec::with_capacity(STATX_BATCH);

    while let Some(directory) = directories.pop() {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(fd) = open_directory(&directory) else {
            continue;
        };
        visited += 1;
        chunk.clear();

        let mut iter = RawDir::new(&fd, buffer.as_mut_slice());
        while let Some(entry) = iter.next() {
            let Ok(entry) = entry else { break };
            let raw = entry.file_name().to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            let name = String::from_utf8_lossy(raw).into_owned();
            let mut flags = 0;
            match entry.file_type() {
                FileType::Directory => {
                    flags |= FLAG_DIR;
                    directories.push(directory.join(&name));
                }
                FileType::Symlink => flags |= FLAG_LINK,
                _ => {}
            }

            if statx_to.is_some() && flags & FLAG_DIR == 0 {
                pending.push(directory.join(&name));
                if pending.len() >= STATX_BATCH {
                    submit_statx(&mut pending, statx_to, cancel);
                }
            }
            chunk.push(&name, flags, 0, 0);
        }
        store.push_chunk(std::mem::take(&mut chunk));
    }

    if !pending.is_empty() {
        submit_statx(&mut pending, statx_to, cancel);
    }
    visited
}

/// Hands a batch of paths to the pool. The results are counted rather than
/// written back into the store: a real implementation fills the columns it
/// already reserved, and doing that here would need the row indices threaded
/// through, which measures bookkeeping rather than syscalls.
fn submit_statx(
    pending: &mut Vec<PathBuf>,
    statx_to: Option<(&Pool, &Arc<Store>)>,
    cancel: &Cancel,
) {
    let Some((pool, store)) = statx_to else {
        pending.clear();
        return;
    };
    let batch = std::mem::take(pending);
    let store = store.clone();
    let cancel = cancel.clone();
    pool.submit(move || {
        let mut done = 0u64;
        for path in &batch {
            if cancel.is_cancelled() {
                break;
            }
            if statx_one(path).is_some() {
                done += 1;
            }
        }
        store.statx_calls.fetch_add(done, Ordering::Relaxed);
    });
}

fn statx_one(path: &Path) -> Option<(u64, i64)> {
    let stat = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::SIZE | StatxFlags::MTIME,
    )
    .ok()?;
    Some((stat.stx_size, stat.stx_mtime.tv_sec))
}

fn open_directory(path: &Path) -> rustix::io::Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Reads one directory only, stopping as soon as `target` names are in the
/// store. This is the "first 200 rows" measurement, and it stops early on
/// purpose: a user who has seen the top of the list does not care that the
/// bottom is still arriving.
pub fn first_rows(arm: Arm, directory: &Path, store: &Arc<Store>, target: u64) -> u64 {
    let mut chunk = Chunk::with_capacity(target as usize);

    match arm {
        Arm::ReadDir | Arm::ReadDirMeta => {
            // skerry-allow: no-read-dir — this arm exists to price read_dir
            let Ok(entries) = std::fs::read_dir(directory) else {
                return 0;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let (mut size, mut mtime, mut flags) = (0u64, 0i64, 0u16);
                if arm == Arm::ReadDirMeta {
                    if let Ok(meta) = entry.metadata() {
                        size = meta.len();
                        mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_secs() as i64);
                        flags |= FLAG_STATTED;
                    }
                }
                chunk.push(&name, flags, size, mtime);
                if chunk.len() as u64 >= target {
                    break;
                }
            }
        }
        Arm::Getdents | Arm::GetdentsStatx | Arm::DirfdStatx | Arm::Parallel => {
            let Ok(fd) = open_directory(directory) else {
                return 0;
            };
            let mut buffer = vec![MaybeUninit::<u8>::uninit(); DIRENT_BUFFER];
            let mut iter = RawDir::new(fd.as_fd(), buffer.as_mut_slice());
            'outer: while let Some(entry) = iter.next() {
                let Ok(entry) = entry else { break };
                let raw = entry.file_name().to_bytes();
                if raw == b"." || raw == b".." {
                    continue;
                }
                let flags = match entry.file_type() {
                    FileType::Directory => FLAG_DIR,
                    FileType::Symlink => FLAG_LINK,
                    _ => 0,
                };
                chunk.push(&String::from_utf8_lossy(raw), flags, 0, 0);
                if chunk.len() as u64 >= target {
                    break 'outer;
                }
            }
        }
    }

    let produced = chunk.len() as u64;
    store.push_chunk(chunk);
    produced
}

// ── the two arms that came out of asking whether this could be faster ─────────

/// Arm 5. Same as `getdents64+statx`, with two changes that are invisible in the
/// syscall count and large in the time:
///
/// - `statx` is issued **relative to the directory's file descriptor** rather
///   than against a path from the root. `statx(CWD, "/var/tmp/.../d3/d1/name")`
///   makes the kernel walk five path components and check permissions on each,
///   once per file, a million times. `statx(dirfd, "name")` walks one.
/// - No `PathBuf` per entry. The name goes to `statx` as the `CStr` that
///   `getdents64` already returned, so the million allocations disappear with
///   the million path walks.
fn walk_dirfd(
    root: &Path,
    store: &Arc<Store>,
    cancel: &Cancel,
    pool: &Pool,
    parallel: bool,
) -> u64 {
    let visited = Arc::new(std::sync::atomic::AtomicU64::new(0));
    match (parallel, pool.submitter()) {
        // Arm 6: directories are jobs, and a job that finds a subdirectory
        // submits it. A tree walk is embarrassingly parallel and this one was
        // not parallel at all, which is why it was the floor under every other
        // number in this spike.
        (true, Some(submitter)) => {
            read_directory_job(
                root.to_path_buf(),
                store.clone(),
                cancel.clone(),
                Some(submitter),
                visited.clone(),
            );
            // The walk is done when nothing is queued and nothing is running.
            // Yielding rather than spinning: a busy wait here holds a whole core
            // against the pool, which costs nothing on a 24-thread machine and
            // measurably distorts a 12-thread one — the first version of this
            // arm spun, and was slower than the arm it replaced on the smaller
            // machine because of it.
            while pool.outstanding() > 0 && !cancel.is_cancelled() {
                std::thread::yield_now();
            }
        }
        _ => read_directory_job(
            root.to_path_buf(),
            store.clone(),
            cancel.clone(),
            None,
            visited.clone(),
        ),
    }
    visited.load(Ordering::Relaxed)
}

/// Reads one directory. With a submitter, each subdirectory becomes another job;
/// without one, subdirectories are drained on this thread.
fn read_directory_job(
    directory: PathBuf,
    store: Arc<Store>,
    cancel: Cancel,
    submitter: Option<crate::pool::Submitter>,
    visited: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut stack = vec![directory];
    let mut buffer = vec![MaybeUninit::<u8>::uninit(); DIRENT_BUFFER];
    // Deliberately *not* pre-reserved. In the parallel arm one job is one
    // directory, so a `with_capacity(1024)` here reserves ~61 KB for every one
    // of 21,338 chunks while most hold a hundred entries — measured at 521 MB
    // peak RSS against 83 MB for the other arms, over Spike A's 300 MB bar.
    // Growing from empty costs a few reallocations and bounds the waste at 2x.
    let mut chunk = Chunk::default();

    while let Some(directory) = stack.pop() {
        if cancel.is_cancelled() {
            return;
        }
        let Ok(fd) = open_directory(&directory) else {
            continue;
        };
        visited.fetch_add(1, Ordering::Relaxed);
        chunk.clear();

        let mut iter = RawDir::new(&fd, buffer.as_mut_slice());
        while let Some(entry) = iter.next() {
            let Ok(entry) = entry else { break };
            let raw_name = entry.file_name();
            let raw = raw_name.to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }

            let mut flags = 0;
            let (mut size, mut mtime) = (0u64, 0i64);
            match entry.file_type() {
                FileType::Directory => {
                    flags |= FLAG_DIR;
                    let child = directory.join(String::from_utf8_lossy(raw).as_ref());
                    match &submitter {
                        Some(submitter) => {
                            let (store, cancel, visited) =
                                (store.clone(), cancel.clone(), visited.clone());
                            let onward = submitter.clone();
                            submitter.submit(move || {
                                read_directory_job(child, store, cancel, Some(onward), visited);
                            });
                        }
                        None => stack.push(child),
                    }
                }
                FileType::Symlink => flags |= FLAG_LINK,
                _ => {
                    // Relative to this directory's fd, using the name getdents64
                    // already gave us. No path walk, no allocation.
                    if let Ok(stat) = rustix::fs::statx(
                        &fd,
                        raw_name,
                        AtFlags::SYMLINK_NOFOLLOW,
                        StatxFlags::SIZE | StatxFlags::MTIME,
                    ) {
                        size = stat.stx_size;
                        mtime = stat.stx_mtime.tv_sec;
                        flags |= FLAG_STATTED;
                        store.statx_calls.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            chunk.push(&String::from_utf8_lossy(raw), flags, size, mtime);
        }
        if shrink_enabled() {
            chunk.shrink_to_fit();
        }
        store.push_chunk(std::mem::take(&mut chunk));
    }
}
