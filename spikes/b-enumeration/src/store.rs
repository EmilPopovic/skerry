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

#[derive(Default)]
struct Columns {
    names: String,
    name_ref: Vec<(u32, u32)>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u16>,
}

#[derive(Default)]
pub struct Store {
    columns: Mutex<Columns>,
    /// Read without the lock so a progress check never contends with a writer —
    /// this is what the "first 200 rows" measurement polls.
    count: AtomicU64,
    pub statx_calls: AtomicU64,
}

impl Store {
    pub fn with_capacity(entries: usize) -> Self {
        Self {
            columns: Mutex::new(Columns {
                names: String::with_capacity(entries * 34),
                name_ref: Vec::with_capacity(entries),
                size: Vec::with_capacity(entries),
                mtime: Vec::with_capacity(entries),
                flags: Vec::with_capacity(entries),
            }),
            ..Default::default()
        }
    }

    pub fn len(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Appends a whole directory's worth under one lock acquisition. A per-entry
    /// lock is a measurable tax of its own at a million entries, and every arm
    /// pays this one equally, so it cancels out of the comparison.
    pub fn extend(&self, batch: &[(String, u16, u64, i64)]) {
        if let Ok(mut columns) = self.columns.lock() {
            for (name, flags, size, mtime) in batch {
                let offset = columns.names.len() as u32;
                columns.names.push_str(name);
                columns.name_ref.push((offset, name.len() as u32));
                columns.size.push(*size);
                columns.mtime.push(*mtime);
                columns.flags.push(*flags);
            }
        }
        self.count.fetch_add(batch.len() as u64, Ordering::Relaxed);
    }

    pub fn footprint(&self) -> usize {
        match self.columns.lock() {
            Ok(columns) => {
                columns.names.capacity()
                    + columns.name_ref.capacity() * 8
                    + columns.size.capacity() * 8
                    + columns.mtime.capacity() * 8
                    + columns.flags.capacity() * 2
            }
            Err(_) => 0,
        }
    }

    pub fn statted(&self) -> u64 {
        self.statx_calls.load(Ordering::Relaxed)
    }
}
