//! The sink every arm writes into, so the arms are compared on producing the
//! same thing rather than on doing different amounts of work.
//!
//! Same shape as Spike A's and as
//! [04-architecture.md](../../../docs/04-architecture.md#data-layout): one name
//! arena and parallel arrays indexed by `EntryId`. It is behind a lock here
//! because four arms need to fill it from a varying number of threads; the
//! shipping design writes it from the job pool and hands the UI thread an
//! immutable snapshot, which is Spike A's problem rather than this one's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_LINK: u16 = 1 << 1;
pub const FLAG_STATTED: u16 = 1 << 2;

/// One directory's worth of entries, built by the job that read it.
///
/// The store is a list of these rather than one arena written under a lock. The
/// first version of this file appended into a single global arena while holding
/// a mutex, which serialised roughly 34 MB of name copying across every job and
/// put a hard ceiling on the parallel arm — 0.93 s on a 12-thread machine, worse
/// than the serial arm it was meant to beat. Chunks make the locked region O(1).
///
/// It is also closer to what the product does: enumeration streams in, and the
/// UI thread reads a snapshot made of the chunks that have arrived.
#[derive(Default)]
pub struct Chunk {
    names: String,
    name_ref: Vec<(u32, u32)>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u16>,
}

impl Chunk {
    pub fn with_capacity(entries: usize) -> Self {
        Self {
            names: String::with_capacity(entries * 34),
            name_ref: Vec::with_capacity(entries),
            size: Vec::with_capacity(entries),
            mtime: Vec::with_capacity(entries),
            flags: Vec::with_capacity(entries),
        }
    }

    pub fn push(&mut self, name: &str, flags: u16, size: u64, mtime: i64) {
        let offset = self.names.len() as u32;
        self.names.push_str(name);
        self.name_ref.push((offset, name.len() as u32));
        self.size.push(size);
        self.mtime.push(mtime);
        self.flags.push(flags);
    }

    pub fn len(&self) -> usize {
        self.name_ref.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name_ref.is_empty()
    }

    pub fn clear(&mut self) {
        self.names.clear();
        self.name_ref.clear();
        self.size.clear();
        self.mtime.clear();
        self.flags.clear();
    }

    /// Hands back the slack that growth-by-doubling left, before the chunk is
    /// parked in the store for the rest of the run. One realloc per directory
    /// against 2x the resident footprint of the whole listing.
    pub fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        self.name_ref.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.flags.shrink_to_fit();
    }

    fn footprint(&self) -> usize {
        self.names.capacity()
            + self.name_ref.capacity() * 8
            + self.size.capacity() * 8
            + self.mtime.capacity() * 8
            + self.flags.capacity() * 2
    }
}

#[derive(Default)]
pub struct Store {
    chunks: Mutex<Vec<Chunk>>,
    /// Read without the lock so a progress check never contends with a writer —
    /// this is what the "first 200 rows" measurement polls.
    count: AtomicU64,
    pub statx_calls: AtomicU64,
}

impl Store {
    pub fn with_capacity(entries: usize) -> Self {
        // Chunk count, not entry count: one per directory, plus slack.
        let _ = entries;
        Self {
            chunks: Mutex::new(Vec::with_capacity(32 * 1024)),
            ..Default::default()
        }
    }

    pub fn len(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Hands a finished chunk to the store. The locked region is a `Vec::push`,
    /// so a job's copying happens on the job's own thread and no arm serialises
    /// on the store. Every arm uses this, so it cancels out of the comparison.
    pub fn push_chunk(&self, chunk: Chunk) {
        let entries = chunk.len() as u64;
        if let Ok(mut chunks) = self.chunks.lock() {
            chunks.push(chunk);
        }
        self.count.fetch_add(entries, Ordering::Relaxed);
    }

    pub fn footprint(&self) -> usize {
        match self.chunks.lock() {
            Ok(chunks) => chunks.iter().map(Chunk::footprint).sum(),
            Err(_) => 0,
        }
    }

    pub fn statted(&self) -> u64 {
        self.statx_calls.load(Ordering::Relaxed)
    }
}
