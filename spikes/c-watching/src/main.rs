//! Spike C — watching at scale. Question, bar and method are in README.md.

#![allow(dead_code)]

mod stats;
mod watch;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stats::Samples;
use watch::Watcher;

const DEFAULT_TREE: &str = "/var/tmp/skerry-spike-f/realistic";

const USAGE: &str = "\
spike-c-watching — inotify at scale

  --tree PATH      directory tree to watch (default /var/tmp/skerry-spike-f/realistic)
  --scratch PATH   a writable directory for the mutation tests (default under the tree)
  --repeats N      repeats per latency measurement (default 20)
  --limit-probe    keep adding watches until the kernel refuses, to find the budget";

struct Options {
    tree: PathBuf,
    scratch: Option<PathBuf>,
    repeats: usize,
    limit_probe: bool,
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        tree: PathBuf::from(DEFAULT_TREE),
        scratch: None,
        repeats: 20,
        limit_probe: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--tree" => options.tree = PathBuf::from(value()?),
            "--scratch" => options.scratch = Some(PathBuf::from(value()?)),
            "--repeats" => options.repeats = value()?.parse().map_err(|_| "bad --repeats")?,
            "--limit-probe" => options.limit_probe = true,
            "--help" | "-h" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(options)
}

fn sysctl(name: &str) -> String {
    std::fs::read_to_string(format!("/proc/sys/fs/inotify/{name}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn run() -> Result<(), String> {
    let options = parse()?;
    if !options.tree.is_dir() {
        return Err(format!("no tree at {}", options.tree.display()));
    }

    let max_watches: usize = sysctl("max_user_watches").parse().unwrap_or(0);
    let max_queued: usize = sysctl("max_queued_events").parse().unwrap_or(0);

    println!("Spike C — watching at scale");
    println!("  tree         {}", options.tree.display());
    println!(
        "  kernel       max_user_watches {max_watches}, max_queued_events {max_queued}, \
max_user_instances {}",
        sysctl("max_user_instances")
    );
    println!("  load         {}", load_average());
    println!();

    // ── enumerate the directories, which is the thing to be watched ───────────
    let started = Instant::now();
    let directories = collect_directories(&options.tree);
    println!(
        "  {} directories found in {:.2} s",
        directories.len(),
        started.elapsed().as_secs_f64()
    );
    println!(
        "  that is {:.1}% of this machine's entire watch budget for one tree",
        100.0 * directories.len() as f64 / max_watches.max(1) as f64
    );

    // ── establish watches ─────────────────────────────────────────────────────
    let rss_before = stats::current_rss().unwrap_or(0);
    let started = Instant::now();
    let mut watcher = Watcher::new().map_err(|e| format!("inotify_init: {e}"))?;
    for directory in &directories {
        watcher.watch(directory);
    }
    let elapsed = started.elapsed();
    let rss_after = stats::current_rss().unwrap_or(0);

    println!(
        "\n  {} watches established in {:.3} s ({:.0}/s), {} refused{}",
        watcher.added,
        elapsed.as_secs_f64(),
        watcher.added as f64 / elapsed.as_secs_f64(),
        watcher.refused,
        match watcher.stopped_by {
            Some(errno) => format!(" — first refusal {errno:?}"),
            None => String::new(),
        }
    );
    println!(
        "  process RSS grew {:.2} MB, so {:.0} bytes per watch on our side alone",
        (rss_after.saturating_sub(rss_before)) as f64 / 1e6,
        (rss_after.saturating_sub(rss_before)) as f64 / watcher.added.max(1) as f64
    );

    let scratch = options
        .scratch
        .clone()
        .unwrap_or_else(|| options.tree.join("bucket-100/dir-00000/d0/d0"));
    if !scratch.is_dir() {
        return Err(format!("scratch {} is not a directory", scratch.display()));
    }
    // Drain anything the walk itself provoked before timing begins.
    watcher.drain(usize::MAX, Duration::from_millis(50));

    single_change(&watcher, &scratch, &options)?;
    bulk_delete(&watcher, &scratch, &options)?;
    bulk_rename(&watcher, &scratch, &options)?;
    overflow(&watcher, &scratch, max_queued)?;

    scattered_overflow(&watcher, &directories, max_queued, &options)?;

    if options.limit_probe {
        limit_probe(max_watches)?;
    }
    Ok(())
}

/// Latency for one file appearing. The floor, and the number every other case is
/// measured against.
fn single_change(watcher: &Watcher, scratch: &Path, options: &Options) -> Result<(), String> {
    let mut samples = Samples::new("one create");
    for i in 0..options.repeats {
        let path = scratch.join(format!(".spike-c-probe-{i}"));
        let started = Instant::now();
        std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let drained = watcher.drain(1, Duration::from_millis(500));
        match drained.first {
            Some(at) => samples.push((at - started).as_secs_f32() * 1000.0),
            None => return Err("no event for a create — is the scratch inside the tree?".into()),
        }
        let _ = std::fs::remove_file(&path);
        watcher.drain(usize::MAX, Duration::from_millis(20));
    }
    println!("\n  {}", samples.row());
    println!("  bar: an external change reflected within 100 ms");
    Ok(())
}

/// The bar's own case: `rm -rf` of a populated subtree.
fn bulk_delete(watcher: &Watcher, scratch: &Path, options: &Options) -> Result<(), String> {
    let mut first = Samples::new("rm -rf 1st");
    let mut complete = Samples::new("rm -rf all");
    let count = 2_000;

    for round in 0..options.repeats.min(5) {
        let victim = scratch.join(format!(".spike-c-victim-{round}"));
        std::fs::create_dir_all(&victim).map_err(|e| format!("{e}"))?;
        for i in 0..count {
            std::fs::File::create(victim.join(format!("f{i}"))).map_err(|e| format!("{e}"))?;
        }
        // The new directory is not watched, so watch it as the product would on
        // seeing it created.
        let mut inner = Watcher::new().map_err(|e| format!("{e}"))?;
        inner.watch(&victim);
        inner.drain(usize::MAX, Duration::from_millis(50));

        let started = Instant::now();
        std::fs::remove_dir_all(&victim).map_err(|e| format!("{e}"))?;
        let drained = inner.drain(count, Duration::from_millis(2000));
        if let Some(at) = drained.first {
            first.push((at - started).as_secs_f32() * 1000.0);
        }
        if let Some(at) = drained.last {
            complete.push((at - started).as_secs_f32() * 1000.0);
        }
        let _ = watcher.drain(usize::MAX, Duration::from_millis(20));
    }
    println!("\n  {}", first.row());
    println!("  {}   ({count} files)", complete.row());
    Ok(())
}

/// A `git checkout` in the shape that matters: many renames at once, where each
/// one is a `MOVED_FROM`/`MOVED_TO` pair the listing has to re-pair.
fn bulk_rename(watcher: &Watcher, scratch: &Path, options: &Options) -> Result<(), String> {
    let mut complete = Samples::new("rename 500");
    let count = 500;

    for round in 0..options.repeats.min(5) {
        let home = scratch.join(format!(".spike-c-rename-{round}"));
        std::fs::create_dir_all(&home).map_err(|e| format!("{e}"))?;
        for i in 0..count {
            std::fs::File::create(home.join(format!("before-{i}"))).map_err(|e| format!("{e}"))?;
        }
        let mut inner = Watcher::new().map_err(|e| format!("{e}"))?;
        inner.watch(&home);
        inner.drain(usize::MAX, Duration::from_millis(50));

        let started = Instant::now();
        for i in 0..count {
            std::fs::rename(home.join(format!("before-{i}")), home.join(format!("after-{i}")))
                .map_err(|e| format!("{e}"))?;
        }
        // Two events per rename.
        let drained = inner.drain(count * 2, Duration::from_millis(2000));
        if let Some(at) = drained.last {
            complete.push((at - started).as_secs_f32() * 1000.0);
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = watcher.drain(usize::MAX, Duration::from_millis(20));
    }
    println!("  {}   ({count} renames, {} events)", complete.row(), count * 2);
    Ok(())
}

/// Provoke `IN_Q_OVERFLOW` deliberately, then find out what it costs to recover.
fn overflow(watcher: &Watcher, scratch: &Path, max_queued: usize) -> Result<(), String> {
    println!("\n  forced overflow — max_queued_events is {max_queued}");

    let home = scratch.join(".spike-c-overflow");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).map_err(|e| format!("{e}"))?;

    let mut inner = Watcher::new().map_err(|e| format!("{e}"))?;
    inner.watch(&home);
    inner.drain(usize::MAX, Duration::from_millis(50));

    // Well past the queue, and deliberately without draining while it happens:
    // this is the burst a `cargo build` or `npm install` produces while the UI
    // thread is busy painting.
    let burst = max_queued * 3;
    let started = Instant::now();
    for i in 0..burst {
        std::fs::File::create(home.join(format!("b{i}"))).map_err(|e| format!("{e}"))?;
    }
    let write_time = started.elapsed();

    let drained = inner.drain(usize::MAX, Duration::from_millis(500));
    let on_disk = std::fs::read_dir(&home).map(|d| d.count()).unwrap_or(0);

    println!(
        "  created {burst} files in {:.2} s; queue delivered {} events and {} overflow",
        write_time.as_secs_f64(),
        drained.events,
        if drained.overflowed { "SIGNALLED" } else { "did NOT signal" }
    );
    println!(
        "  events lost: {} of {burst} ({:.1}%) — the kernel does not say which",
        burst.saturating_sub(drained.events),
        100.0 * burst.saturating_sub(drained.events) as f64 / burst as f64
    );

    // The only correct recovery: re-read the directory and compare.
    let started = Instant::now();
    let rescanned = std::fs::read_dir(&home).map(|d| d.count()).unwrap_or(0);
    let rescan = started.elapsed();
    println!(
        "  targeted rescan of the one affected directory: {} entries in {:.1} ms — \
recovers {} ({})",
        rescanned,
        rescan.as_secs_f64() * 1000.0,
        if rescanned == on_disk { "everything" } else { "NOT everything" },
        if drained.overflowed && rescanned == on_disk { "PASS" } else { "see above" }
    );

    let _ = std::fs::remove_dir_all(&home);
    let _ = watcher.drain(usize::MAX, Duration::from_millis(50));
    Ok(())
}

/// Add watches to fresh directories until the kernel refuses, so the budget is a
/// measured number rather than a sysctl read.
fn limit_probe(max_watches: usize) -> Result<(), String> {
    println!("\n  watch-limit probe");
    let home = std::env::temp_dir().join("skerry-spike-c-limit");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).map_err(|e| format!("{e}"))?;

    let mut watcher = Watcher::new().map_err(|e| format!("{e}"))?;
    let mut created = 0usize;
    let target = max_watches + 1_000;
    let rss_before = stats::current_rss().unwrap_or(0);

    while watcher.stopped_by.is_none() && created < target {
        let directory = home.join(format!("d{created}"));
        if std::fs::create_dir(&directory).is_err() {
            break;
        }
        created += 1;
        watcher.watch(&directory);
    }
    let rss_after = stats::current_rss().unwrap_or(0);

    println!(
        "  added {} watches before {:?}; sysctl says {max_watches}",
        watcher.added,
        watcher.stopped_by
    );
    println!(
        "  our own RSS for those: {:.2} MB ({:.0} bytes each)",
        (rss_after.saturating_sub(rss_before)) as f64 / 1e6,
        (rss_after.saturating_sub(rss_before)) as f64 / watcher.added.max(1) as f64
    );
    drop(watcher);
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

/// The overflow case that actually hurts: a burst spread across thousands of
/// watched directories, which is what `git checkout`, `cargo build` and
/// `npm install` all produce.
///
/// The single-directory test above is the easy version — one watch descriptor is
/// affected, so a "targeted" rescan is one `read_dir`. Here the kernel signals
/// one overflow and says nothing about which of 21,338 directories it dropped
/// events for. **The `touched` list is not the answer**: it holds the watches
/// whose events *survived* the queue, which is precisely not the set that needs
/// rescanning.
fn scattered_overflow(
    watcher: &Watcher,
    directories: &[PathBuf],
    max_queued: usize,
    _options: &Options,
) -> Result<(), String> {
    println!("\n  scattered overflow — a burst across many watched directories");

    // Enough directories that no single one dominates, and enough files that the
    // queue cannot hold the result.
    let spread = directories.len().min(4_000);
    let per = (max_queued * 3) / spread.max(1) + 1;
    let victims = &directories[..spread];

    let started = Instant::now();
    let mut created = 0usize;
    for (n, directory) in victims.iter().enumerate() {
        for i in 0..per {
            if std::fs::File::create(directory.join(format!(".spike-c-s{n}-{i}"))).is_ok() {
                created += 1;
            }
        }
    }
    let write_time = started.elapsed();

    let drained = watcher.drain(usize::MAX, Duration::from_millis(1000));
    let distinct: std::collections::HashSet<i32> = drained.touched.iter().copied().collect();

    println!(
        "  created {created} files across {spread} directories in {:.2} s",
        write_time.as_secs_f64()
    );
    println!(
        "  delivered {} events from {} distinct watches, overflow {}",
        drained.events,
        distinct.len(),
        if drained.overflowed { "SIGNALLED" } else { "not signalled" }
    );
    println!(
        "  {} events lost across an unknown subset of {spread} directories",
        created.saturating_sub(drained.events)
    );

    // Price both recoveries.
    let started = Instant::now();
    let mut seen = 0usize;
    for directory in victims {
        // skerry-allow: no-read-dir — a spike pricing recovery, not measuring read_dir
        if let Ok(entries) = std::fs::read_dir(directory) {
            seen += entries.count();
        }
    }
    let targeted = started.elapsed();

    let started = Instant::now();
    let mut whole = 0usize;
    for directory in directories {
        // skerry-allow: no-read-dir — as above
        if let Ok(entries) = std::fs::read_dir(directory) {
            whole += entries.count();
        }
    }
    let full = started.elapsed();

    println!(
        "  rescanning the {spread} directories we happened to touch: {:.0} ms, {seen} entries",
        targeted.as_secs_f64() * 1000.0
    );
    println!(
        "  rescanning all {} directories, which is what correctness requires: {:.0} ms, {whole} entries",
        directories.len(),
        full.as_secs_f64() * 1000.0
    );
    println!(
        "  bar: no lost entries after a forced overflow — met only by the full rescan, at {:.0}x \
the 100 ms latency budget",
        full.as_secs_f64() * 1000.0 / 100.0
    );

    // Clean up.
    for (n, directory) in victims.iter().enumerate() {
        for i in 0..per {
            let _ = std::fs::remove_file(directory.join(format!(".spike-c-s{n}-{i}")));
        }
    }
    watcher.drain(usize::MAX, Duration::from_millis(200));
    Ok(())
}

fn collect_directories(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        // skerry-allow: no-read-dir — a spike enumerating for setup, not measuring
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        found.push(directory);
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
            }
        }
    }
    found
}

fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}
