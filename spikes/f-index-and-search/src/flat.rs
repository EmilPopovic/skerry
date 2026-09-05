// SPDX-License-Identifier: Apache-2.0
//! The flattened view: one arena of names, one parallel array of metadata.
//!
//! This is [04-architecture.md](../../../docs/04-architecture.md#data-layout)'s
//! store with the tree structure thrown away, which is what a flattened view is.
//! Filtering is then a scan over contiguous bytes rather than a walk over
//! pointers, and that property is the whole reason the arena exists — so the
//! matcher below reads `&[u8]` out of it and never allocates a `String`.

/// A directory's worth of entries, produced by one job.
#[derive(Default)]
pub struct Chunk {
    pub names: Vec<u8>,
    /// Byte offset of each name's end. The start is the previous end, so one
    /// `u32` per entry describes the whole layout.
    pub ends: Vec<u32>,
    pub size: Vec<u64>,
    pub mtime: Vec<i64>,
    pub flags: Vec<u16>,
}

impl Chunk {
    pub fn push(&mut self, name: &[u8], flags: u16, size: u64, mtime: i64) {
        self.names.extend_from_slice(name);
        self.ends.push(self.names.len() as u32);
        self.size.push(size);
        self.mtime.push(mtime);
        self.flags.push(flags);
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Spike B measured a 522 MB peak from chunks that reserved for 1024 entries
    /// and held a hundred. Growth from empty plus a shrink on the way into the
    /// store was the fix, and it is the same fix here.
    pub fn shrink(&mut self) {
        self.names.shrink_to_fit();
        self.ends.shrink_to_fit();
        self.size.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.flags.shrink_to_fit();
    }
}

/// Every chunk concatenated into one arena. Built once when the walk finishes,
/// because a filter that has to chase a `Vec<Chunk>` is not measuring the design
/// the architecture document describes.
#[derive(Default)]
pub struct Flat {
    pub names: Vec<u8>,
    pub ends: Vec<u32>,
    pub size: Vec<u64>,
    pub mtime: Vec<i64>,
    pub flags: Vec<u16>,
}

impl Flat {
    pub fn from_chunks(chunks: Vec<Chunk>) -> Self {
        let entries: usize = chunks.iter().map(Chunk::len).sum();
        let bytes: usize = chunks.iter().map(|c| c.names.len()).sum();
        let mut flat = Self {
            names: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(entries),
            size: Vec::with_capacity(entries),
            mtime: Vec::with_capacity(entries),
            flags: Vec::with_capacity(entries),
        };
        for chunk in chunks {
            let base = flat.names.len() as u32;
            flat.names.extend_from_slice(&chunk.names);
            flat.ends.extend(chunk.ends.iter().map(|end| end + base));
            flat.size.extend_from_slice(&chunk.size);
            flat.mtime.extend_from_slice(&chunk.mtime);
            flat.flags.extend_from_slice(&chunk.flags);
        }
        flat
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn name(&self, index: usize) -> &[u8] {
        let end = self.ends[index] as usize;
        let start = if index == 0 { 0 } else { self.ends[index - 1] as usize };
        &self.names[start..end]
    }

    pub fn footprint(&self) -> usize {
        self.names.capacity()
            + self.ends.capacity() * 4
            + self.size.capacity() * 8
            + self.mtime.capacity() * 8
            + self.flags.capacity() * 2
    }
}
