//! Spike B's winning arm, carried over: directories are jobs, `statx` goes
//! through the directory's own file descriptor, and each job builds its own
//! chunk so the store's locked region is a `Vec::push`.
//!
//! It is reproduced rather than shared because the two spikes must stay
//! independently buildable, and because this one drops the tree structure on the
//! floor — a flattened view has no directories in it.

use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, StatxFlags};

use crate::flat::{Chunk, Flat};
use crate::pool::{Cancel, Pool, Submitter};

const DIRENT_BUFFER: usize = 32 * 1024;

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_LINK: u16 = 1 << 1;

#[derive(Default)]
pub struct Sink {
    chunks: Mutex<Vec<Chunk>>,
    entries: AtomicU64,
}

impl Sink {
    pub fn len(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    fn push(&self, chunk: Chunk) {
        let n = chunk.len() as u64;
        if let Ok(mut chunks) = self.chunks.lock() {
            chunks.push(chunk);
        }
        self.entries.fetch_add(n, Ordering::Relaxed);
    }

    pub fn into_flat(self) -> Flat {
        Flat::from_chunks(self.chunks.into_inner().unwrap_or_default())
    }
}

/// Walks `root` into a flat store. Returns the number of directories visited,
/// which is the number that explains the flat-directory result.
pub fn walk(root: &Path, sink: &Arc<Sink>, pool: &Pool, cancel: &Cancel) -> u64 {
    let visited = Arc::new(AtomicU64::new(0));
    match pool.submitter() {
        Some(submitter) => {
            read_directory(root.to_path_buf(), sink.clone(), cancel.clone(), Some(submitter), visited.clone());
            while pool.outstanding() > 0 && !cancel.is_cancelled() {
                std::thread::yield_now();
            }
        }
        None => read_directory(root.to_path_buf(), sink.clone(), cancel.clone(), None, visited.clone()),
    }
    visited.load(Ordering::Relaxed)
}

fn read_directory(
    directory: PathBuf,
    sink: Arc<Sink>,
    cancel: Cancel,
    submitter: Option<Submitter>,
    visited: Arc<AtomicU64>,
) {
    let mut stack = vec![directory];
    let mut buffer = vec![MaybeUninit::<u8>::uninit(); DIRENT_BUFFER];
    let mut chunk = Chunk::default();

    while let Some(directory) = stack.pop() {
        if cancel.is_cancelled() {
            return;
        }
        let Ok(fd) = rustix::fs::open(
            &directory,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            continue;
        };
        visited.fetch_add(1, Ordering::Relaxed);

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
                            let (sink, cancel, visited) =
                                (sink.clone(), cancel.clone(), visited.clone());
                            let onward = submitter.clone();
                            submitter.submit(move || {
                                read_directory(child, sink, cancel, Some(onward), visited);
                            });
                        }
                        None => stack.push(child),
                    }
                }
                FileType::Symlink => flags |= FLAG_LINK,
                _ => {
                    if let Ok(stat) = rustix::fs::statx(
                        &fd,
                        raw_name,
                        AtFlags::SYMLINK_NOFOLLOW,
                        StatxFlags::SIZE | StatxFlags::MTIME,
                    ) {
                        size = stat.stx_size;
                        mtime = stat.stx_mtime.tv_sec;
                    }
                }
            }
            chunk.push(raw, flags, size, mtime);
        }
        chunk.shrink();
        sink.push(std::mem::take(&mut chunk));
    }
}
