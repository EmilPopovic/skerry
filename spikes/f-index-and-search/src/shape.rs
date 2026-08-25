//! The tree shapes this spike measures.
//!
//! Spike B's tree is realistic — mostly small directories, a few large ones —
//! and a realistic tree is not where enumeration breaks. These are the shapes
//! that break it, and every one of them is a directory a real user opens:
//! `node_modules` caches, Maildir stores, object directories, `/nix/store`,
//! dataset dumps, and anything written by a naive downloader.

use std::io;
use std::path::{Path, PathBuf};

use crate::data::Rng;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One directory, one million entries. A parallel walk has one unit of work
    /// here, because the unit of work is a directory.
    Flat1M,
    /// The same, an order of magnitude smaller, so the cost is a curve.
    Flat100K,
    /// Spike B's tree, regenerated here so both spikes measure the same bytes.
    Realistic,
}

impl Shape {
    pub fn label(self) -> &'static str {
        match self {
            Shape::Flat1M => "flat-1m",
            Shape::Flat100K => "flat-100k",
            Shape::Realistic => "realistic",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [Shape::Flat1M, Shape::Flat100K, Shape::Realistic]
            .into_iter()
            .find(|s| s.label() == name)
    }

    pub fn files(self) -> usize {
        match self {
            Shape::Flat1M => 1_000_000,
            Shape::Flat100K => 100_000,
            Shape::Realistic => 1_000_000,
        }
    }

    pub fn root(self, base: &Path) -> PathBuf {
        base.join(self.label())
    }
}

/// Spike B's shape, kept identical so the two spikes' numbers are comparable.
const REALISTIC: &[(usize, usize)] = &[(1, 100_000), (10, 10_000), (100, 1_000), (7_000, 100)];

pub fn generate(shape: Shape, base: &Path, seed: u64) -> io::Result<usize> {
    let root = shape.root(base);
    let mut rng = Rng::new(seed);
    let mut name = String::with_capacity(256);
    let mut written = 0usize;

    match shape {
        Shape::Flat1M | Shape::Flat100K => {
            std::fs::create_dir_all(&root)?;
            for _ in 0..shape.files() {
                name.clear();
                crate::data::compose_unique_name(&mut rng, written, &mut name);
                std::fs::File::create(root.join(&name))?;
                written += 1;
            }
        }
        Shape::Realistic => {
            for (count, each) in REALISTIC {
                for index in 0..*count {
                    let directory = root
                        .join(format!("bucket-{each}"))
                        .join(format!("dir-{index:05}"))
                        .join(format!("d{}", index % 7))
                        .join(format!("d{}", index % 5));
                    std::fs::create_dir_all(&directory)?;
                    for _ in 0..*each {
                        name.clear();
                        crate::data::compose_unique_name(&mut rng, written, &mut name);
                        std::fs::File::create(directory.join(&name))?;
                        written += 1;
                    }
                }
            }
        }
    }
    std::fs::write(root.join(".generated"), written.to_string())?;
    Ok(written)
}

pub fn present(shape: Shape, base: &Path) -> Option<usize> {
    std::fs::read_to_string(shape.root(base).join(".generated"))
        .ok()?
        .trim()
        .parse()
        .ok()
}
