// SPDX-License-Identifier: Apache-2.0
//! What the machine can actually do, so "memory-bandwidth bound" stops being an
//! assertion.
//!
//! This spike has claimed several times that the sort and the search are limited
//! by memory rather than by cores, on the evidence that adding threads stops
//! helping. That is consistent with the claim and does not establish it — a lock,
//! a scheduler or a straggler would look the same from the outside.
//!
//! So: measure the two access patterns the workload actually uses, on the same
//! arena, and compare what the sort achieves against what the machine offers.

use std::time::Instant;

use crate::data::Rng;

pub struct Bandwidth {
    /// Sequential bytes per second, reading the arena end to end.
    pub sequential: f64,
    /// Independent random 8-byte reads per second, scattered across the arena.
    /// This is the sort's pattern: many outstanding, none dependent.
    pub random: f64,
    /// Dependent random reads per second — each address comes from the previous
    /// value, so the memory system cannot overlap them. This is the latency
    /// floor, and the gap between it and `random` is what the hardware's
    /// parallelism is worth.
    pub dependent: f64,
}

pub fn measure(arena: &[u8], threads: usize) -> Bandwidth {
    Bandwidth {
        sequential: sequential(arena),
        random: random(arena, threads),
        dependent: dependent(arena),
    }
}

fn sequential(arena: &[u8]) -> f64 {
    let started = Instant::now();
    let mut total = 0u64;
    // Four accumulators so the additions pipeline rather than serialising on one
    // dependency chain, which would measure the adder instead of the memory.
    let mut sums = [0u64; 4];
    for chunk in arena.chunks_exact(4) {
        sums[0] += u64::from(chunk[0]);
        sums[1] += u64::from(chunk[1]);
        sums[2] += u64::from(chunk[2]);
        sums[3] += u64::from(chunk[3]);
    }
    total += sums.iter().sum::<u64>();
    let elapsed = started.elapsed().as_secs_f64();
    std::hint::black_box(total);
    arena.len() as f64 / elapsed
}

/// Independent random reads, spread across every thread the sort would use.
fn random(arena: &[u8], threads: usize) -> f64 {
    const READS: usize = 4_000_000;
    if arena.len() < 8 {
        return 0.0;
    }
    let span = arena.len() - 8;
    let per = READS / threads.max(1);

    let started = Instant::now();
    let total: u64 = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|slot| {
                scope.spawn(move || {
                    let mut rng = Rng::new(0xB0B_1EED ^ slot as u64);
                    let mut sum = 0u64;
                    for _ in 0..per {
                        let at = (rng.next() as usize) % span;
                        sum = sum.wrapping_add(u64::from_le_bytes(
                            arena[at..at + 8].try_into().expect("8 bytes"),
                        ));
                    }
                    sum
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
    });
    let elapsed = started.elapsed().as_secs_f64();
    std::hint::black_box(total);
    (per * threads.max(1)) as f64 / elapsed
}

/// Each address depends on the previous read, so nothing overlaps.
fn dependent(arena: &[u8]) -> f64 {
    const READS: usize = 2_000_000;
    if arena.len() < 8 {
        return 0.0;
    }
    let span = arena.len() - 8;
    let started = Instant::now();
    let mut at = 0usize;
    let mut sum = 0u64;
    for _ in 0..READS {
        let value = u64::from_le_bytes(arena[at..at + 8].try_into().expect("8 bytes"));
        sum = sum.wrapping_add(value);
        // Mixed, so the address is unpredictable but derived from what was read.
        at = ((value ^ (value >> 29)).wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize) % span;
    }
    let elapsed = started.elapsed().as_secs_f64();
    std::hint::black_box(sum);
    READS as f64 / elapsed
}
