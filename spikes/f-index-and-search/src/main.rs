//! Spike F — the index, flattened search, and the shapes that break a walk.
//! The question, the bar and the method are in README.md next to this file.

#![allow(dead_code)]

mod data;
mod flat;
mod index;
mod matcher;
mod pool;
mod shape;
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

    println!("  {} directories, {} entries, arena {:.1} MB, store {:.1} MB",
        directories, flat.len(), flat.names.len() as f64 / 1e6, flat.footprint() as f64 / 1e6);
    println!("  {}", walk_ms.row());
    println!("  {}", flatten_ms.row());
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

    println!("\n  persistent index");
    println!("  {}", build.row());
    println!("  {}", load.row());
    println!(
        "  on disk {:.1} MB, {:.0} bytes per entry",
        bytes as f64 / 1e6,
        bytes as f64 / flat.len().max(1) as f64
    );
    println!(
        "  load vs walk: {:.2}x the warm walk on this machine",
        load.percentile(50.0) / walk_ms.percentile(50.0).max(0.001)
    );
    println!();
    Ok(())
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
