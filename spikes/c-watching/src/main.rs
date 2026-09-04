// SPDX-License-Identifier: MIT OR Apache-2.0
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
    pruned_rescan(&watcher, &directories, max_queued)?;

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

/// Overflow recovery by pruning on directory mtime instead of re-reading
/// everything.
///
/// A full rescan is 1,234 ms on a Pi because it reads all 21,338 directories. But
/// a directory's mtime moves whenever an entry is created, deleted or renamed in
/// it — so `statx` on each directory is enough to tell which ones can be skipped,
/// and `statx` is far cheaper than `getdents64`.
///
/// **What this is sound for, and what it is not.** Directory mtime tracks
/// *structural* change: entries appearing, disappearing, being renamed. It does
/// not move when a file already in the directory is written to or has its
/// permissions changed, so a lost `MODIFY` or `ATTRIB` event is invisible to this
/// scheme. It recovers the listing's shape, not every column in it — and the
/// columns are recoverable separately, by re-`statx`ing the entries of the
/// handful of directories actually on screen.
fn pruned_rescan(
    watcher: &Watcher,
    directories: &[PathBuf],
    max_queued: usize,
) -> Result<(), String> {
    println!("\n  overflow recovery by mtime pruning");

    // Baseline: one statx per directory, which is what a listing already has.
    let started = Instant::now();
    let mut baseline: Vec<i64> = Vec::with_capacity(directories.len());
    for directory in directories {
        baseline.push(directory_mtime(directory));
    }
    let baseline_cost = started.elapsed();
    println!(
        "  baseline: {} directory mtimes in {:.0} ms",
        baseline.len(),
        baseline_cost.as_secs_f64() * 1000.0
    );

    // The same burst that overflowed the queue before.
    let spread = directories.len().min(4_000);
    let per = (max_queued * 3) / spread.max(1) + 1;
    let victims = &directories[..spread];
    let mut created = 0usize;
    for (n, directory) in victims.iter().enumerate() {
        for i in 0..per {
            if std::fs::File::create(directory.join(format!(".spike-c-p{n}-{i}"))).is_ok() {
                created += 1;
            }
        }
    }
    let drained = watcher.drain(usize::MAX, Duration::from_millis(500));

    // Recovery: stat everything, read only what moved.
    let started = Instant::now();
    let mut changed = Vec::new();
    for (index, directory) in directories.iter().enumerate() {
        if directory_mtime(directory) != baseline[index] {
            changed.push(directory);
        }
    }
    let stat_pass = started.elapsed();

    let started = Instant::now();
    let mut entries = 0usize;
    for directory in &changed {
        // skerry-allow: no-read-dir — a spike pricing recovery
        if let Ok(read) = std::fs::read_dir(directory) {
            entries += read.count();
        }
    }
    let read_pass = started.elapsed();
    let total = stat_pass + read_pass;

    println!(
        "  burst of {created} files across {spread} directories; {} events delivered, overflow {}",
        drained.events,
        if drained.overflowed { "signalled" } else { "not signalled" }
    );
    println!(
        "  detected {} changed directories of {} ({:.1}%) — {} false negatives",
        changed.len(),
        directories.len(),
        100.0 * changed.len() as f64 / directories.len() as f64,
        spread.saturating_sub(changed.len())
    );
    println!(
        "  stat pass {:.0} ms + read of {} directories {:.0} ms = {:.0} ms total, {entries} entries",
        stat_pass.as_secs_f64() * 1000.0,
        changed.len(),
        read_pass.as_secs_f64() * 1000.0,
        total.as_secs_f64() * 1000.0
    );

    // The same recovery, done the only other correct way, for comparison.
    let started = Instant::now();
    let mut whole = 0usize;
    for directory in directories {
        // skerry-allow: no-read-dir — as above
        if let Ok(read) = std::fs::read_dir(directory) {
            whole += read.count();
        }
    }
    let full = started.elapsed();
    println!(
        "  against a full rescan: {:.0} ms for {whole} entries — pruning is {:.1}x faster",
        full.as_secs_f64() * 1000.0,
        full.as_secs_f64() / total.as_secs_f64().max(0.000_001)
    );

    for (n, directory) in victims.iter().enumerate() {
        for i in 0..per {
            let _ = std::fs::remove_file(directory.join(format!(".spike-c-p{n}-{i}")));
        }
    }
    watcher.drain(usize::MAX, Duration::from_millis(200));
    Ok(())
}

fn directory_mtime(directory: &Path) -> i64 {
    rustix::fs::statx(
        rustix::fs::CWD,
        directory,
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::MTIME,
    )
    .map(|s| s.stx_mtime.tv_sec * 1_000_000_000 + i64::from(s.stx_mtime.tv_nsec))
    .unwrap_or(0)
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
