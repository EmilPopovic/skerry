// SPDX-License-Identifier: Apache-2.0
//! Spike F — the index, flattened search, and the shapes that break a walk.
//! The question, the bar and the method are in README.md next to this file.

#![allow(dead_code)]

mod data;
mod flat;
mod index;
mod matcher;
mod pool;
mod probe;
mod shape;
mod sort;
mod stats;
mod walk;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use flat::Flat;
use matcher::Query;
use pool::{Cancel, Pool};
use shape::Shape;
use stats::Samples;
use walk::Sink;

const DEFAULT_BASE: &str = "/var/tmp/skerry-spike-f";

/// The queries, chosen to span what the matcher actually sees. A fuzzy matcher's
/// cost is dominated by how early it can reject, so the interesting axis is the
/// match *rate*, not the query length.
const QUERIES: &[(&str, &str)] = &[
    ("", "everything — the floor, no matching at all"),
    ("zzqx", "almost nothing — the rejection path"),
    ("snapshot-render-70", "a handful — the realistic case"),
    ("e", "a large fraction — where the result set is the cost"),
    ("rpt", "scattered subsequence — no contiguous run to find"),
];

const USAGE: &str = "\
spike-f-index-and-search — flattened search and the persistent index

  --base PATH      where the trees live (default /var/tmp/skerry-spike-f)
  --generate       create every tree shape, then exit
  --shape NAME     flat-1m, flat-100k or realistic (default: all present)
  --workers N      job pool width (default ncpu, per Spike B)
  --repeats N      repeats per measurement (default 5)";

struct Options {
    base: PathBuf,
    generate: bool,
    shape: Option<Shape>,
    workers: Option<usize>,
    repeats: usize,
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        base: PathBuf::from(DEFAULT_BASE),
        generate: false,
        shape: None,
        workers: None,
        repeats: 5,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--base" => options.base = PathBuf::from(value()?),
            "--generate" => options.generate = true,
            "--shape" => {
                let name = value()?;
                options.shape =
                    Some(Shape::parse(&name).ok_or_else(|| format!("unknown shape `{name}`"))?);
            }
            "--workers" => options.workers = Some(value()?.parse().map_err(|_| "bad --workers")?),
            "--repeats" => options.repeats = value()?.parse().map_err(|_| "bad --repeats")?,
            "--help" | "-h" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn pool_width(options: &Options) -> usize {
    // Spike B measured the optimum at or above ncpu on all four machines, and
    // ncpu - 2 as clearly wrong on small ones.
    options.workers.unwrap_or_else(|| {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    })
}

fn run() -> Result<(), String> {
    let options = parse()?;
    std::fs::create_dir_all(&options.base).map_err(|e| format!("{}: {e}", options.base.display()))?;

    if options.generate {
        for shape in [Shape::Flat100K, Shape::Flat1M, Shape::Realistic] {
            if let Some(n) = shape::present(shape, &options.base) {
                println!("  {:<10} already present, {n} files", shape.label());
                continue;
            }
            let started = Instant::now();
            let n = shape::generate(shape, &options.base, 0xF1E_5EED)
                .map_err(|e| format!("{}: {e}", shape.label()))?;
            println!(
                "  {:<10} {n} files in {:.1} s",
                shape.label(),
                started.elapsed().as_secs_f64()
            );
        }
        return Ok(());
    }

    let shapes: Vec<Shape> = match options.shape {
        Some(shape) => vec![shape],
        None => [Shape::Flat100K, Shape::Flat1M, Shape::Realistic]
            .into_iter()
            .filter(|s| shape::present(*s, &options.base).is_some())
            .collect(),
    };
    if shapes.is_empty() {
        return Err(format!(
            "no trees under {} — run --generate first",
            options.base.display()
        ));
    }

    println!("Spike F — index and flattened search");
    println!("  pool         {} workers", pool_width(&options));
    println!("  load         {}", load_average());
    println!();

    for shape in shapes {
        measure(shape, &options)?;
    }
    Ok(())
}

fn measure(shape: Shape, options: &Options) -> Result<(), String> {
    let root = shape.root(&options.base);
    let expected = shape::present(shape, &options.base).unwrap_or(0);
    println!("═══ {} — {expected} files ═══", shape.label());

    // ── walk and flatten ──────────────────────────────────────────────────────
    let mut walk_ms = Samples::new("walk");
    let mut flatten_ms = Samples::new("flatten");
    let mut directories = 0u64;
    let mut flat = Flat::default();

    for _ in 0..options.repeats {
        let cancel = Cancel::new();
        let mut pool = Pool::with_width(pool_width(options), cancel.clone());
        let sink = Arc::new(Sink::default());

        let started = Instant::now();
        directories = walk::walk(&root, &sink, &pool, &cancel);
        walk_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        pool.drain();

        let sink = Arc::try_unwrap(sink).map_err(|_| "sink still shared")?;
        let started = Instant::now();
        flat = sink.into_flat();
        flatten_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }

    // The flat-directory fix: getdents stays sequential, statx goes to the pool.
    let mut deferred_ms = Samples::new("walk deferred");
    for _ in 0..options.repeats {
        let cancel = Cancel::new();
        let mut pool = Pool::with_width(pool_width(options), cancel.clone());
        let sink = Arc::new(Sink::default());
        let started = Instant::now();
        walk::walk_deferred_statx(&root, &sink, &pool, &cancel, 256);
        deferred_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        pool.drain();
    }

    println!("  {} directories, {} entries, arena {:.1} MB, store {:.1} MB",
        directories, flat.len(), flat.names.len() as f64 / 1e6, flat.footprint() as f64 / 1e6);
    println!("  {}", walk_ms.row());
    println!("  {}", flatten_ms.row());
    println!("  {}", deferred_ms.row());
    println!("  peak RSS {:.1} MB", stats::peak_rss().unwrap_or(0) as f64 / 1e6);

    // ── search ────────────────────────────────────────────────────────────────
    println!("\n  flattened search, milliseconds — bar: first result within 50 ms");
    println!("  {:<22} {:>10} {:>10} {:>12}  {}", "query", "first", "all", "matches", "what it is");
    for (text, description) in QUERIES {
        let query = Query::new(text);
        let mut first = Samples::new("first");
        let mut all = Samples::new("all");
        let mut matches = 0usize;

        for _ in 0..options.repeats {
            let started = Instant::now();
            let mut found = None;
            for index in 0..flat.len() {
                if query.score(flat.name(index)).is_some() {
                    found = Some(index);
                    break;
                }
            }
            first.push(started.elapsed().as_secs_f32() * 1000.0);
            let _ = found;

            let started = Instant::now();
            let mut count = 0usize;
            let mut best = i32::MIN;
            for index in 0..flat.len() {
                if let Some(score) = query.score(flat.name(index)) {
                    count += 1;
                    best = best.max(score);
                }
            }
            all.push(started.elapsed().as_secs_f32() * 1000.0);
            matches = count;
            let _ = best;
        }

        let shown = if text.is_empty() { "(empty)" } else { text };
        println!(
            "  {:<22} {:>10.3} {:>10.3} {:>12}  {}",
            shown,
            first.percentile(50.0),
            all.percentile(50.0),
            matches,
            description
        );
    }

    let threads = pool_width(options);

    // ── what the machine can do ───────────────────────────────────────────────
    // So that "memory-bandwidth bound" is a comparison rather than an assertion.
    let bandwidth = probe::measure(&flat.names, threads);
    println!("\n  what this machine offers, over the same {:.1} MB arena", flat.names.len() as f64 / 1e6);
    println!(
        "  sequential {:.2} GB/s · random 8-byte reads {:.1} M/s across {threads} threads · \
dependent {:.1} M/s on one",
        bandwidth.sequential / 1e9,
        bandwidth.random / 1e6,
        bandwidth.dependent / 1e6
    );

    // ── index ─────────────────────────────────────────────────────────────────
    let path = options.base.join(format!("{}.skix", shape.label()));
    let mut build = Samples::new("build");
    let mut load = Samples::new("load");
    let mut bytes = 0u64;

    for _ in 0..options.repeats {
        let started = Instant::now();
        bytes = index::write(&flat, &path).map_err(|e| format!("index write: {e}"))?;
        build.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    for _ in 0..options.repeats {
        let started = Instant::now();
        let loaded = index::read(&path).map_err(|e| format!("index read: {e}"))?;
        load.push(started.elapsed().as_secs_f32() * 1000.0);
        if loaded.len() != flat.len() {
            return Err(format!(
                "index round-trip lost entries: {} in, {} out",
                flat.len(),
                loaded.len()
            ));
        }
    }

    // The comparison ADR 0008 turns on: a *cold* index load. fadvise evicts the
    // file from the page cache without root, so this is a real device read.
    let mut cold = Samples::new("load cold");
    for _ in 0..options.repeats {
        index::evict(&path).map_err(|e| format!("evict: {e}"))?;
        let started = Instant::now();
        let loaded = index::read(&path).map_err(|e| format!("index read: {e}"))?;
        cold.push(started.elapsed().as_secs_f32() * 1000.0);
        if loaded.len() != flat.len() {
            return Err("cold round-trip lost entries".into());
        }
    }

    println!("\n  persistent index");
    println!("  {}", build.row());
    println!("  {}", load.row());
    println!("  {}", cold.row());
    println!(
        "  on disk {:.1} MB, {:.0} bytes per entry",
        bytes as f64 / 1e6,
        bytes as f64 / flat.len().max(1) as f64
    );
    println!(
        "  cold load is {:.2}x the warm walk, and {:.0}x faster than a 2.3 s cold walk",
        cold.percentile(50.0) / walk_ms.percentile(50.0).max(0.001),
        2300.0 / cold.percentile(50.0).max(0.001)
    );

    // ── sorting ───────────────────────────────────────────────────────────────
    println!("\n  sorting {} entries, milliseconds — bar: 120 ms, hard fail 300 ms", flat.len());
    let mut keys_ms = Samples::new("build keys");
    let mut keys = Vec::new();
    for _ in 0..options.repeats {
        let started = Instant::now();
        keys = sort::keys(&flat);
        keys_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    let base: Vec<u32> = (0..flat.len() as u32).collect();
    let mut runs: Vec<(&str, Samples)> = vec![
        ("by size", Samples::new("by size")),
        ("by mtime", Samples::new("by mtime")),
        ("by name, arena", Samples::new("by name, arena")),
        ("by name, keyed", Samples::new("by name, keyed")),
    ];
    for _ in 0..options.repeats {
        for (which, samples) in runs.iter_mut().enumerate() {
            let mut order = base.clone();
            let started = Instant::now();
            match which {
                0 => sort::by_size(&flat, &mut order),
                1 => sort::by_mtime(&flat, &mut order),
                2 => sort::by_name_arena(&flat, &mut order),
                _ => sort::by_precomputed(&flat, &keys, &mut order),
            }
            samples.1.push(started.elapsed().as_secs_f32() * 1000.0);
        }
    }
    println!("  {}", keys_ms.row());
    for (_, samples) in &runs {
        println!("  {}", samples.row());
    }

    let mut radix_ms = Samples::new("radix, keys");
    let mut radix_ties_ms = Samples::new("radix + ties");
    let mut scratch = Vec::new();
    let mut repaired = 0usize;
    for _ in 0..options.repeats {
        let mut order = base.clone();
        let started = Instant::now();
        sort::radix(&keys, &mut order, &mut scratch);
        radix_ms.push(started.elapsed().as_secs_f32() * 1000.0);

        let mut order = base.clone();
        let started = Instant::now();
        repaired = sort::radix_with_ties(&flat, &keys, &mut order, &mut scratch);
        radix_ties_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", radix_ms.row());
    println!("  {}", radix_ties_ms.row());
    println!(
        "  {} of {} entries ({:.0}%) share an 8-byte key prefix and need the arena to separate",
        repaired,
        flat.len(),
        100.0 * repaired as f64 / flat.len().max(1) as f64
    );

    let mut packed_ms = Samples::new("radix packed");
    let mut packed_ties_ms = Samples::new("packed + ties");
    for _ in 0..options.repeats {
        let mut order = Vec::new();
        let started = Instant::now();
        sort::radix_packed(&keys, &mut order);
        packed_ms.push(started.elapsed().as_secs_f32() * 1000.0);

        let mut order = Vec::new();
        let started = Instant::now();
        sort::radix_packed_with_ties(&flat, &keys, &mut order);
        packed_ties_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", packed_ms.row());
    println!("  {}", packed_ties_ms.row());

    let mut recursive_ms = Samples::new("radix recursive");
    for _ in 0..options.repeats {
        let mut order = base.clone();
        let started = Instant::now();
        sort::radix_recursive(&flat, &mut order, 0, &mut scratch);
        recursive_ms.push(started.elapsed().as_secs_f32() * 1000.0);
        // A sort that is fast and wrong is not a result.
        debug_assert!(order.windows(2).all(|w| {
            flat.name(w[0] as usize) <= flat.name(w[1] as usize)
        }));
    }
    println!("  {}", recursive_ms.row());
    {
        let mut order = base.clone();
        sort::radix_recursive(&flat, &mut order, 0, &mut scratch);
        let sorted = order
            .windows(2)
            .all(|w| flat.name(w[0] as usize) <= flat.name(w[1] as usize));
        println!("  recursive radix produces a correctly ordered result: {sorted}");
    }

    let mut parallel_sort_ms = Samples::new("radix parallel");
    for _ in 0..options.repeats {
        let mut order = base.clone();
        let started = Instant::now();
        sort::radix_parallel(&flat, &mut order, threads);
        parallel_sort_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", parallel_sort_ms.row());
    {
        let mut order = base.clone();
        sort::radix_parallel(&flat, &mut order, threads);
        let sorted = order
            .windows(2)
            .all(|w| flat.name(w[0] as usize) <= flat.name(w[1] as usize));
        println!("  parallel radix produces a correctly ordered result: {sorted}");
    }

    let mut permuted_ms = Samples::new("radix permuted");
    for _ in 0..options.repeats {
        let mut order = Vec::new();
        let started = Instant::now();
        sort::radix_permuted(&flat, &keys, &mut order);
        permuted_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", permuted_ms.row());
    {
        let mut order = Vec::new();
        sort::radix_permuted(&flat, &keys, &mut order);
        let sorted = order
            .windows(2)
            .all(|w| flat.name(w[0] as usize) <= flat.name(w[1] as usize));
        println!("  permuted-arena radix produces a correctly ordered result: {sorted}");
    }

    let mut msd_ms = Samples::new("radix msd par");
    for _ in 0..options.repeats {
        let mut order = base.clone();
        let started = Instant::now();
        sort::radix_msd_parallel(&flat, &mut order, threads);
        msd_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", msd_ms.row());
    {
        let mut order = base.clone();
        sort::radix_msd_parallel(&flat, &mut order, threads);
        let sorted = order
            .windows(2)
            .all(|w| flat.name(w[0] as usize) <= flat.name(w[1] as usize));
        println!("  msd parallel radix produces a correctly ordered result: {sorted}");
    }

    let mut full_ms = Samples::new("radix msd full");
    for _ in 0..options.repeats {
        let mut order = base.clone();
        let started = Instant::now();
        sort::radix_msd_fully_parallel(&flat, &mut order, threads);
        full_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", full_ms.row());
    {
        let mut order = base.clone();
        sort::radix_msd_fully_parallel(&flat, &mut order, threads);
        let sorted = order
            .windows(2)
            .all(|w| flat.name(w[0] as usize) <= flat.name(w[1] as usize));
        println!("  fully parallel msd radix produces a correctly ordered result: {sorted}");
    }

    {
        // The sort's own throughput, against what the probe says is available.
        // A recursive radix touches each name roughly once per level of
        // recursion; with 94% of names sharing eight bytes, two levels is the
        // floor and three is typical.
        let touches = flat.len() as f64 * 2.5;
        let achieved = touches / (full_ms.percentile(50.0) as f64 / 1000.0);
        println!(
            "  the sort makes about {:.1} M random arena reads and achieves {:.1} M/s, \
which is {:.0}% of the {:.1} M/s this machine offers",
            touches / 1e6,
            achieved / 1e6,
            100.0 * achieved / bandwidth.random.max(1.0),
            bandwidth.random / 1e6
        );
    }

    {
        // Why the parallel sort stops improving: if one first-byte bucket holds a
        // large share of the entries, it is a straggler no number of threads can
        // divide. Data-dependent, so it is measured rather than assumed.
        let mut buckets = [0usize; 257];
        for index in 0..flat.len() {
            let name = flat.name(index);
            buckets[name.first().map_or(0, |b| *b as usize + 1)] += 1;
        }
        let mut ranked: Vec<(usize, usize)> = buckets
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(b, n)| (*n, b))
            .collect();
        ranked.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
        let biggest = ranked.first().copied().unwrap_or((0, 0));
        let top4: usize = ranked.iter().take(4).map(|(n, _)| n).sum();
        println!(
            "  first-byte buckets: {} non-empty, largest {:.1}% ('{}'), top 4 = {:.1}% of entries",
            ranked.len(),
            100.0 * biggest.0 as f64 / flat.len() as f64,
            if biggest.1 > 0 { (biggest.1 as u8 - 1) as char } else { '?' },
            100.0 * top4 as f64 / flat.len() as f64
        );
    }

    let mut keys16_ms = Samples::new("build keys16");
    let mut radix16_ms = Samples::new("radix16 + ties");
    let mut pair;
    let mut repaired16 = 0usize;
    for _ in 0..options.repeats {
        let started = Instant::now();
        pair = sort::keys16(&flat);
        keys16_ms.push(started.elapsed().as_secs_f32() * 1000.0);

        let mut order = base.clone();
        let started = Instant::now();
        repaired16 = sort::radix16_with_ties(&flat, &pair.0, &pair.1, &mut order, &mut scratch);
        radix16_ms.push(started.elapsed().as_secs_f32() * 1000.0);
    }
    println!("  {}", keys16_ms.row());
    println!("  {}", radix16_ms.row());
    println!(
        "  {} of {} entries ({:.0}%) still tied at 16 bytes",
        repaired16,
        flat.len(),
        100.0 * repaired16 as f64 / flat.len().max(1) as f64
    );

    // ── as you type ───────────────────────────────────────────────────────────
    // Typing `report` is six searches, not one, and each is over the whole set
    // because a fuzzy match is not a prefix match — a later keystroke cannot
    // narrow the previous result set without dropping matches it should keep.
    // Parallel scan: the only lever that does not change what the search means.
    println!("\n  parallel scan across {threads} threads, milliseconds");
    println!("  {:<22} {:>12} {:>12} {:>10}", "query", "1 thread", "parallel", "speedup");
    for (text, _) in QUERIES {
        let query = Query::new(text);
        let (mut one, mut many) = (Samples::new("1"), Samples::new("n"));
        for _ in 0..options.repeats {
            let started = Instant::now();
            let a = matcher::count_parallel(&flat, &query, 1);
            one.push(started.elapsed().as_secs_f32() * 1000.0);
            let started = Instant::now();
            let b = matcher::count_parallel(&flat, &query, threads);
            many.push(started.elapsed().as_secs_f32() * 1000.0);
            assert_eq!(a, b, "parallel scan disagreed with serial on `{text}`");
        }
        let (s1, sn) = (one.percentile(50.0), many.percentile(50.0));
        println!(
            "  {:<22} {:>12.3} {:>12.3} {:>9.1}x",
            if text.is_empty() { "(empty)" } else { text },
            s1,
            sn,
            s1 / sn.max(0.001)
        );
    }

    // The bar's own measurement: first result, with every thread stopping as
    // soon as any of them finds one.
    let worst = QUERIES
        .iter()
        .map(|(text, _)| {
            let query = Query::new(text);
            let mut samples = Samples::new("first");
            for _ in 0..options.repeats {
                let started = Instant::now();
                let hit = matcher::first_parallel(&flat, &query, threads);
                samples.push(started.elapsed().as_secs_f32() * 1000.0);
                let _ = hit;
            }
            (*text, samples.percentile(50.0))
        })
        .fold(("", 0.0f32), |acc, next| if next.1 > acc.1 { next } else { acc });
    println!(
        "  worst first-result across every query, parallel: {:.3} ms on `{}` ({})",
        worst.1,
        if worst.0.is_empty() { "(empty)" } else { worst.0 },
        if worst.1 < 50.0 { "inside the 50 ms bar" } else { "OVER the 50 ms bar" }
    );

    println!("\n  as-you-type, one full scan per keystroke, milliseconds");
    let word = "report";
    let mut total = 0.0f32;
    print!("  ");
    for length in 1..=word.len() {
        let query = Query::new(&word[..length]);
        let mut samples = Samples::new("k");
        for _ in 0..options.repeats {
            let started = Instant::now();
            let mut count = 0usize;
            for index in 0..flat.len() {
                if query.score(flat.name(index)).is_some() {
                    count += 1;
                }
            }
            samples.push(started.elapsed().as_secs_f32() * 1000.0);
            let _ = count;
        }
        let ms = samples.percentile(50.0);
        total += ms;
        print!("{:?}={:.1} ", &word[..length], ms);
    }
    println!("\n  worst single keystroke {:.1} ms, whole word {:.1} ms",
        total / word.len() as f32, total);

    println!();
    Ok(())
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
