//! Spike B — enumeration throughput. The question, the bar and the method are in
//! README.md next to this file.
//!
//! Throwaway code, kept for its measurements. It is careful about one thing: no
//! number is printed that the harness did not measure, and the arms are held to
//! producing the same columns so the difference between them is the syscalls.

// `data.rs` and `stats.rs` are copies of Spike A's, kept whole rather than
// trimmed to what this spike uses: the two spikes generate the same filenames,
// and a divergence between the copies would make that quietly untrue. The one
// intentional difference is `compose_unique_name`, which `tree.rs` needs because
// these names become real files.
#![allow(dead_code)]

mod data;
#[cfg(feature = "hang")]
mod hang;
mod pool;
mod stats;
mod store;
mod tree;
mod walk;

use std::path::{Path, PathBuf};
#[cfg(feature = "hang")]
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
#[cfg(feature = "hang")]
use std::time::Duration;
use std::time::Instant;

use pool::{Cancel, Pool};
use stats::Samples;
use store::Store;
use walk::Arm;

const DEFAULT_ROOT: &str = "/var/tmp/skerry-spike-b";
const FIRST_ROWS: u64 = 200;

struct Options {
    root: PathBuf,
    generate: bool,
    repeats: usize,
    skip_hang: bool,
    only: Option<String>,
    hang_at: Option<PathBuf>,
    /// Pool width, for measuring how the batching win scales down to machines
    /// with fewer cores than this one.
    workers: Option<usize>,
    /// Skip the warming walk, so a run after `drop_caches` measures a cold tree
    /// in the table rather than in the discarded warm-up line.
    cold: bool,
}

const USAGE: &str = "\
spike-b-enumeration — Spike B, getdents64 + statx against read_dir

  --root PATH     where the tree lives (default /var/tmp/skerry-spike-b)
  --generate      create the tree, then exit
  --repeats N     runs per arm (default 3)
  --skip-hang     leave out the hung-mount condition
  --only ARM      one arm only: read_dir, read_dir+meta, getdents64, getdents64+statx
  --hang-at PATH  where to mount the hung filesystem (default under ~/.cache)
  --workers N     job pool width (default ncpu - 2)
  --cold          skip the warming walk — for a run straight after drop_caches";

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        root: PathBuf::from(DEFAULT_ROOT),
        generate: false,
        repeats: 3,
        skip_hang: false,
        only: None,
        hang_at: None,
        workers: None,
        cold: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--root" => options.root = PathBuf::from(value()?),
            "--generate" => options.generate = true,
            "--repeats" => options.repeats = value()?.parse().map_err(|_| "bad --repeats")?,
            "--skip-hang" => options.skip_hang = true,
            "--only" => options.only = Some(value()?),
            "--hang-at" => options.hang_at = Some(PathBuf::from(value()?)),
            "--workers" => options.workers = Some(value()?.parse().map_err(|_| "bad --workers")?),
            "--cold" => options.cold = true,
            "--help" | "-h" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn run() -> Result<(), String> {
    let options = parse()?;

    if options.generate {
        return generate(&options.root);
    }

    let expected = tree::total_files();
    let present = tree::looks_complete(&options.root).ok_or_else(|| {
        format!(
            "no tree at {} — run with --generate first (it writes {expected} files)",
            options.root.display()
        )
    })?;
    if present != expected {
        return Err(format!(
            "tree at {} holds {present} files, expected {expected} — regenerate it",
            options.root.display()
        ));
    }

    println!("Spike B — enumeration throughput");
    println!("  tree         {} files under {}", present, options.root.display());
    println!(
        "  pool         {} workers{}",
        pool_width(&options),
        match options.workers {
            Some(_) => " (--workers)",
            None => " (ncpu - 2)",
        }
    );
    println!("  load         {}", load_average());
    println!();

    let arms: Vec<Arm> = match &options.only {
        Some(name) => Arm::all()
            .into_iter()
            .filter(|a| a.label() == name)
            .collect(),
        None => Arm::all().to_vec(),
    };
    if arms.is_empty() {
        return Err(format!("no arm matches `{}`", options.only.unwrap_or_default()));
    }

    if options.cold {
        println!("  warming      SKIPPED — --cold, so the table below is a cold-cache run");
    } else {
        warm_the_cache(&options.root);
    }
    full_walk(&options, &arms);
    first_rows(&options, &arms);
    #[cfg(feature = "hang")]
    if !options.skip_hang {
        hung_mount(&options)?;
    } else {
        println!("\nhung mount");
        println!("  SKIPPED — --skip-hang");
    }
    #[cfg(not(feature = "hang"))]
    {
        let _ = options.skip_hang;
        println!("\nhung mount");
        println!("  NOT BUILT — this binary was compiled without the `hang` feature");
    }
    Ok(())
}

fn generate(root: &Path) -> Result<(), String> {
    let expected = tree::total_files();
    println!("generating {expected} files under {}", root.display());
    println!("this writes {expected} inodes and takes a few minutes; ^C is safe");
    let started = Instant::now();
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let written = tree::generate(root, 0xB00_5EED).map_err(|e| format!("generate: {e}"))?;
    tree::mark_complete(root, written).map_err(|e| format!("marker: {e}"))?;
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "  wrote {written} files in {elapsed:.1} s ({:.0} files/s)",
        written as f64 / elapsed
    );
    Ok(())
}

/// The bar says warm cache, so the cache is warmed deliberately and the warming
/// pass is discarded. A first run that measured a cold tree and called it warm
/// would be the easiest possible way for this spike to lie.
fn warm_the_cache(root: &Path) {
    let started = Instant::now();
    let store = Arc::new(Store::with_capacity(0));
    let cancel = Cancel::new();
    let mut pool = Pool::with_width(1, cancel.clone());
    let entries = walk::walk(Arm::Getdents, root, &store, &pool, &cancel);
    pool.drain();
    println!(
        "  warming      {} directories, {} entries in {:.2} s — discarded",
        entries,
        store.len(),
        started.elapsed().as_secs_f64()
    );
}

fn full_walk(options: &Options, arms: &[Arm]) {
    println!("full recursive walk, warm cache, seconds");
    println!("  {:<17} {:>7} {:>7} {:>7}   {:>12}   {}", "arm", "min", "med", "max", "files/s", "stat calls");

    for arm in arms {
        let mut samples = Samples::new(arm.label());
        let mut entries = 0u64;
        let mut statted = 0u64;
        let mut footprint = 0usize;

        for _ in 0..options.repeats {
            let cancel = Cancel::new();
            let mut pool = Pool::with_width(pool_width(options), cancel.clone());
            let store = Arc::new(Store::with_capacity(tree::total_files()));

            let started = Instant::now();
            walk::walk(*arm, &options.root, &store, &pool, &cancel);
            pool.drain();
            samples.push(started.elapsed().as_secs_f32() * 1000.0);

            entries = store.len();
            statted = store.statted();
            footprint = store.footprint();
        }

        let median = samples.percentile(50.0) / 1000.0;
        println!(
            "  {:<17} {:>7.3} {:>7.3} {:>7.3}   {:>12.0}   {}",
            arm.label(),
            samples.percentile(0.0) / 1000.0,
            median,
            samples.max() / 1000.0,
            entries as f64 / median as f64,
            if arm.stats() {
                format!("{statted}")
            } else {
                "none".into()
            }
        );
        let _ = footprint;
    }

    println!("\n  bar: warm-cache full walk under 1 s");
    match (arms.len(), stats::peak_rss()) {
        // VmHWM only ever rises, so with several arms in one process the first
        // arm's peak is every arm's peak and the figure means nothing.
        (1, Some(peak)) => println!(
            "  peak RSS {:.1} MB for this arm alone",
            peak as f32 / 1.0e6
        ),
        (1, None) => println!("  peak RSS UNMEASURED — /proc/self/status unreadable"),
        _ => println!(
            "  peak RSS omitted: VmHWM only rises, so in one process the first arm's peak\n\
             is every arm's peak. Run --only <arm> for a figure that means something."
        ),
    }
}

fn first_rows(options: &Options, arms: &[Arm]) {
    println!("\ntime to first {FIRST_ROWS} rows, milliseconds");
    let probes = tree::probe_directories(&options.root);
    print!("  {:<17}", "arm");
    for (entries, _) in &probes {
        print!(" {:>13}", format!("{entries}-entry"));
    }
    println!();

    for arm in arms {
        print!("  {:<17}", arm.label());
        for (entries, directory) in &probes {
            // A directory smaller than the target cannot produce the target. Ask
            // for everything it has and report that, rather than an infinity.
            let target = FIRST_ROWS.min(*entries as u64);
            let mut best = f32::INFINITY;
            let mut got_any = 0u64;
            for _ in 0..options.repeats {
                let store = Arc::new(Store::with_capacity(target as usize));
                let started = Instant::now();
                let got = walk::first_rows(*arm, directory, &store, target);
                let elapsed = started.elapsed().as_secs_f32() * 1000.0;
                if got >= target {
                    best = best.min(elapsed);
                    got_any = got;
                }
            }
            let cell = if got_any == 0 {
                "unreadable".to_string()
            } else if target < FIRST_ROWS {
                format!("{best:.3} ({target})")
            } else {
                format!("{best:.3}")
            };
            print!(" {cell:>13}");
        }
        println!();
    }
    println!("\n  bar: first 200 visible rows within 30 ms of opening any directory");
}

#[cfg(feature = "hang")]
/// Rule 1's canonical test. A frame loop runs while a walk of a dead mount is in
/// flight on the pool; the question is only ever whether the frame loop notices.
fn hung_mount(options: &Options) -> Result<(), String> {
    println!("\nhung mount — rule 1's canonical test");

    // Beside the tree, never inside it: a hung mount under the walk root would
    // make every other arm recurse into it and block.
    // Not beside the tree: `mount(2)` returns EPERM for an unprivileged FUSE
    // mount under /var/tmp on this machine, while the user's cache directory is
    // allowed. Recorded in the README as an environment constraint rather than
    // worked around silently.
    let mount_at = options.hang_at.clone().unwrap_or_else(default_hang_mount);
    let entered = Arc::new(AtomicU64::new(0));
    let mount = match hang::mount(&mount_at, entered.clone()) {
        Ok(mount) => mount,
        Err(e) => {
            println!("  UNMEASURED — could not mount FUSE at {}: {e}", mount_at.display());
            return Ok(());
        }
    };

    let cancel = Cancel::new();
    let pool = Pool::new(cancel.clone());
    let store = Arc::new(Store::with_capacity(1024));

    // The walk is submitted to the pool, never run here. That is the entire
    // design under test: the UI thread has no path to a filesystem syscall.
    {
        let store = store.clone();
        let cancel = cancel.clone();
        let root = mount_at.clone();
        let inner = Pool::with_width(1, cancel.clone());
        pool.submit(move || {
            walk::walk(Arm::GetdentsStatx, &root, &store, &inner, &cancel);
        });
    }

    let mut frames = Samples::new("on the pool");
    let started = Instant::now();
    let mut last = Instant::now();
    // Two seconds of frames at a 4 ms cadence: long enough that a blocked
    // syscall would have to show up, short enough to stay a test.
    while started.elapsed() < Duration::from_secs(2) {
        spin(Duration::from_micros(4000));
        let now = Instant::now();
        frames.push((now - last).as_secs_f32() * 1000.0);
        last = now;
    }

    let reached = entered.load(std::sync::atomic::Ordering::Relaxed);
    cancel.cancel();

    // The control. Without it this test can only pass: a frame loop with no path
    // to a filesystem syscall cannot be blocked by one, so "no frame exceeded
    // 8 ms" would be a restatement of the design rather than a measurement of
    // it. Here the frame loop makes the call itself, which is what every file
    // manager that blocks on a dead mount is doing.
    let mut control = Samples::new("on the UI thread");
    let control_started = Instant::now();
    let blocked = {
        let began = Instant::now();
        let listed = std::fs::read_dir(&mount_at).map(|entries| entries.count());
        let elapsed = began.elapsed();
        control.push(elapsed.as_secs_f32() * 1000.0);
        let _ = listed;
        elapsed
    };
    let _ = control_started;

    println!("  {}", frames.row());
    println!("  {}", control.row());
    println!(
        "  requests that reached the hung filesystem: {reached} — {}",
        if reached > 0 {
            "the mount was genuinely entered and did not answer"
        } else {
            "NOTHING REACHED IT, so this measured an unmounted path rather than a hung one"
        }
    );
    println!(
        "  bar: no frame exceeds 8 ms while a mount is hung — p99 {:.3} ms, max {:.3} ms ({})",
        frames.percentile(99.0),
        frames.max(),
        if frames.max() < 8.0 && reached > 0 { "PASS" } else { "MISS" }
    );
    let total_reached = entered.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "  control: the same listing from the frame loop blocked it for {:.1} s — {:.0}x the\n\
           budget, which is what the pool is buying",
        blocked.as_secs_f64(),
        blocked.as_secs_f64() * 1000.0 / 8.0
    );
    println!(
        "           {total_reached} requests stalled in total at {} s each: a dead mount\n\
           serialises, so the control also waited behind what the pool arm left outstanding",
        hang::HANG.as_secs()
    );

    // Left mounted until here on purpose: unmounting earlier would let the walk
    // fail fast and turn a hung mount into a missing one.
    drop(mount);
    Ok(())
}

#[cfg(feature = "hang")]
/// A busy wait rather than a sleep. Rule 1 says the UI thread sleeps on the
/// event queue and never on a timer, so a `sleep` here would both violate the
/// rule being tested and hide a stall inside the sleep.
fn spin(duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

fn pool_width(options: &Options) -> usize {
    options.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2).max(1))
            .unwrap_or(2)
    })
}

#[cfg(feature = "hang")]
fn default_hang_mount() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.map_or_else(
        || PathBuf::from("/tmp/skerry-spike-b-hung"),
        |home| home.join(".cache").join("skerry-spike-b-hung"),
    )
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
