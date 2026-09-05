// SPDX-License-Identifier: Apache-2.0
//! Generates the tree the spike walks.
//!
//! Seeded, so the workload is the same between runs and between machines. The
//! shape is the point: a real tree is mostly small directories with a few
//! enormous ones, and the 30 ms condition is decided entirely by the enormous
//! ones. A million files spread evenly over a thousand directories would be a
//! measurement of a tree nobody has.

use std::io;
use std::path::{Path, PathBuf};

use crate::data::Rng;

/// (directory count, entries each). Sums to 1,000,000.
pub const SHAPE: &[(usize, usize)] = &[(1, 100_000), (10, 10_000), (100, 1_000), (7_000, 100)];

pub fn total_files() -> usize {
    SHAPE.iter().map(|(dirs, each)| dirs * each).sum()
}

/// Where the files for one bucket entry actually live. Nested so the walk
/// recurses rather than reading one flat level, and shared by the generator and
/// the probe so the two cannot disagree about it.
fn leaf(root: &Path, each: usize, index: usize) -> PathBuf {
    root.join(format!("bucket-{each}"))
        .join(format!("dir-{index:05}"))
        .join(format!("d{}", index % 7))
        .join(format!("d{}", index % 5))
}

/// One directory per bucket for the "first 200 rows" measurement, paired with
/// how many entries it holds.
pub fn probe_directories(root: &Path) -> Vec<(usize, PathBuf)> {
    SHAPE.iter().map(|(_, each)| (*each, leaf(root, *each, 0))).collect()
}

pub fn generate(root: &Path, seed: u64) -> io::Result<usize> {
    let mut rng = Rng::new(seed);
    let mut name = String::with_capacity(256);
    let mut written = 0usize;

    for (count, each) in SHAPE {
        for index in 0..*count {
            let directory = leaf(root, *each, index);
            std::fs::create_dir_all(&directory)?;
            for _ in 0..*each {
                name.clear();
                // The index is woven into the name, so a collision cannot
                // silently leave the tree short of a million files.
                crate::data::compose_unique_name(&mut rng, written, &mut name);
                // An empty file: the measurement is metadata throughput, and
                // contents would only add page-cache noise.
                std::fs::File::create(directory.join(&name))?;
                written += 1;
            }
        }
    }
    Ok(written)
}

/// The file count of a previously generated tree, so a run cannot silently
/// measure half a tree.
pub fn looks_complete(root: &Path) -> Option<usize> {
    std::fs::read_to_string(root.join(".generated")).ok()?.trim().parse().ok()
}

pub fn mark_complete(root: &Path, files: usize) -> io::Result<()> {
    std::fs::write(root.join(".generated"), files.to_string())
}
