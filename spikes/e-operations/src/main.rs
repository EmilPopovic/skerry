//! Spike E — operation engine correctness.
//!
//! Question, bar and method are in [README.md](README.md). Unlike the other five
//! spikes this code is not thrown away: `08-roadmap.md` says it becomes the
//! permanent torture matrix, and M3 is gated on it being green.
//!
//!     probe                     what each filesystem here can do
//!     run [dir...]              the matrix, on every filesystem found
//!     run --strict [dir...]     the same, and fail if anything was left unmeasured
//!     inject <mnt> <mode> <n>   a filesystem that fails at an exact offset

use std::time::Duration;

mod checks;
mod fixture;
mod copy;
mod probe;
mod report;
mod tree;
mod verify;

#[cfg(feature = "inject")]
mod inject;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `get(1..)` and not `&args[1..]`: with no arguments at all the slice is
    // empty and the index panics, and no arguments at all is the documented way
    // to run this — one binary that discovers which filesystems to test.
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str).unwrap_or("run") {
        "probe" => probe_mode(rest),
        "run" => run_mode(rest),
        #[cfg(feature = "inject")]
        "inject" => inject::main(rest),
        other => {
            eprintln!("modes: probe | run [dir...] | inject <mnt> <mode> <n>");
            eprintln!("unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn header(title: &str) {
    println!("Spike E — {title}");
    println!("  kernel       {}", kernel());
    println!("  load         {:.2}", report::loadavg());
    println!();
}

/// A real foreign filesystem, created and mounted without root: `mkntfs` writes
/// the image and `ntfs-3g` is FUSE.
///
/// It is not in the roadmap's list, and it earns its place because it is the only
/// filesystem here that *refuses* things — an external drive is usually formatted
/// this way, and "could not preserve" is a path that needs exercising.
fn with_ntfs(report: &mut report::Report, source: Option<&(String, std::path::PathBuf)>) {
    let (Some(mkntfs), Some(ntfs3g)) = (which("mkntfs"), which("ntfs-3g")) else {
        report.owed("ntfs", "mkntfs or ntfs-3g is not installed");
        return;
    };
    let image = std::path::PathBuf::from(format!("/tmp/skerry-spike-e-ntfs-{}.img", std::process::id()));
    let mount = std::path::PathBuf::from(format!("/tmp/skerry-spike-e-ntfs-{}", std::process::id()));
    // 512 MiB: enough for the 1 GiB sparse file's real 12 KiB, and small enough
    // to format in about a second.
    if std::fs::File::create(&image).and_then(|f| f.set_len(512 << 20)).is_err() {
        report.owed("ntfs", "could not create the image file");
        return;
    }
    let formatted = std::process::Command::new(mkntfs)
        .args(["-Q", "-F", "-L", "skerry"])
        .arg(&image)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(formatted, Ok(s) if s.success()) {
        report.owed("ntfs", "mkntfs refused the image");
        let _ = std::fs::remove_file(&image);
        return;
    }
    let _ = std::fs::create_dir_all(&mount);
    let mounted = std::process::Command::new(ntfs3g)
        .arg(&image)
        .arg(&mount)
        // `streams_interface=windows` is what exposes alternate data streams as
        // xattrs; without it ntfs-3g refuses every user attribute, which is
        // itself worth measuring.
        .args(["-o", "streams_interface=windows"])
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(mounted, Ok(s) if s.success()) {
        report.owed("ntfs", "ntfs-3g would not mount the image");
        let _ = std::fs::remove_file(&image);
        let _ = std::fs::remove_dir(&mount);
        return;
    }
    let dir = mount.join("matrix-root");
    let _ = std::fs::create_dir_all(&dir);
    match probe::capabilities(&dir) {
        Ok(caps) => {
            let t = probe::Target { label: format!("ntfs (ntfs-3g) at {}", dir.display()), dir, caps };
            println!("═══ {} ═══", t.label);
            match source {
                // The cross-filesystem pair: a POSIX source, a foreign
                // destination. Reusing the source tree also means the two pairs
                // are copying exactly the same bytes.
                Some((label, root)) => {
                    checks::run_pair(report, &reopen(root), &t, &format!("{label}→ntfs"))
                }
                None => {
                    let src = t.dir.join("src");
                    if let Some(fx) = checks::build_source(report, "ntfs", &src) {
                        checks::run_pair(report, &fx, &t, "ntfs→ntfs");
                    }
                }
            }
            println!();
        }
        Err(e) => report.owed("ntfs", format!("could not probe the mount: {e}")),
    }
    unmount(&mount);
    let _ = std::fs::remove_dir(&mount);
    let _ = std::fs::remove_file(&image);
}

/// A fixture handle for a tree that already exists on disk, so the same source
/// can be copied to several destinations without being rebuilt.
fn reopen(root: &std::path::Path) -> fixture::Fixture {
    let mut names = Vec::new();
    collect_files(root, &mut names);
    // The sparse fixture's size is read back from the file rather than assumed,
    // because a reopened tree does not know how it was built.
    let sparse_size = std::fs::metadata(root.join("sparse.bin")).ok().map(|m| m.len());
    fixture::Fixture { root: root.to_path_buf(), names, refused: Vec::new(), sparse_size }
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.is_dir() {
            collect_files(&p, out);
        } else if md.is_file() {
            out.push(p);
        }
    }
}

/// A short label for a pair name: `ext4`, `tmpfs`, `ntfs`.
fn short_name(fstype: &str, dir: &std::path::Path) -> String {
    let _ = dir;
    fstype.split('.').next_back().unwrap_or(fstype).to_string()
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    for dir in ["/usr/bin", "/usr/sbin", "/bin", "/sbin"] {
        let p = std::path::Path::new(dir).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// `fusermount3 -u` fails with `EBUSY` while the kernel still holds a reference,
/// which happens routinely a few milliseconds after the last close. Retry, then
/// fall back to a lazy unmount so a run never leaves a mount point behind.
fn unmount(mount: &std::path::Path) {
    let path = mount.display().to_string();
    for _ in 0..20 {
        if matches!(
            std::process::Command::new("fusermount3")
                .args(["-u", &path])
                .stderr(std::process::Stdio::null())
                .status(),
            Ok(s) if s.success()
        ) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::process::Command::new("fusermount3")
        .args(["-uz", &path])
        .stderr(std::process::Stdio::null())
        .status();
}

/// The fault arm. Each case needs its own mount, because the faults are set when
/// the filesystem starts — which is the point: a filesystem whose behaviour
/// changes underneath a running copy is a different experiment.
///
/// Done here rather than in `checks` so that a machine without `/dev/fuse`
/// records the cases as owed and the rest of the matrix still runs.
fn faults(report: &mut report::Report) {
    #[cfg(not(feature = "inject"))]
    {
        report.owed("faults", "built without the inject feature");
    }
    #[cfg(feature = "inject")]
    {
        const SOURCE: u64 = 1 << 20;
        const LIMIT: u64 = 300 * 1024;
        const EIO_AT: u64 = 64 * 1024;
        const CHUNK: usize = 512;
        let scratch = std::path::PathBuf::from("/tmp/skerry-spike-e-scratch");
        let _ = std::fs::create_dir_all(&scratch);

        with_injector(report, &[format!("enospc={LIMIT}"), format!("source={SOURCE}")], |r, m| {
            checks::fault_enospc(r, m, SOURCE, LIMIT);
        });
        with_injector(report, &[format!("eio-read={EIO_AT}"), format!("source={SOURCE}")], |r, m| {
            checks::fault_eio(r, m, &scratch, EIO_AT);
        });
        with_injector(report, &["no-xattr".into()], |r, m| {
            checks::fault_no_xattr(r, m, &scratch);
        });
        with_injector(report, &[format!("short-write={CHUNK}")], |r, m| {
            checks::fault_short_write(r, m, &scratch, CHUNK);
        });
        // A tree that does not fit, rather than a file that does not fit. The
        // per-file guarantee is "no partial file"; the per-tree guarantee is
        // "everything that exists is complete", and they are not the same claim.
        // Twenty equal files, and room for eight of them. Equal sizes are the
        // point: with a mixed tree the cut lands wherever `read_dir` happens to
        // order things, and "everything that exists is complete" is only a claim
        // worth testing when something does exist and something does not.
        const FILE: u64 = 256 * 1024;
        const COUNT: u64 = 20;
        const TREE_LIMIT: u64 = 8 * FILE;
        if let Some(root) = uniform_source(FILE as usize, COUNT as usize) {
            with_injector(report, &[format!("enospc={TREE_LIMIT}")], |r, m| {
                checks::fault_tree_enospc(r, m, &root, TREE_LIMIT);
            });
            tree::remove_tree(&root);
        }
        tree::remove_tree(&scratch);
    }
}

/// `count` files of exactly `size` bytes each, so the point at which a destination
/// runs out of space is known in advance.
#[cfg(feature = "inject")]
fn uniform_source(size: usize, count: usize) -> Option<std::path::PathBuf> {
    let root = std::path::PathBuf::from(format!("/tmp/skerry-spike-e-fault-src-{}", std::process::id()));
    std::fs::create_dir_all(&root).ok()?;
    for i in 0..count {
        fixture::write_pattern(&root.join(format!("file-{i:03}.bin")), size).ok()?;
    }
    Some(root)
}

/// Mount the injecting filesystem with one fault configuration, run one check,
/// unmount. The mount point carries the pid so two runs cannot collide.
#[cfg(feature = "inject")]
fn with_injector(
    report: &mut report::Report,
    faults: &[String],
    body: impl FnOnce(&mut report::Report, &std::path::Path),
) {
    let mount = std::path::PathBuf::from(format!(
        "/tmp/skerry-spike-e-inject-{}-{}",
        std::process::id(),
        faults.first().map(|f| f.split('=').next().unwrap_or("f").to_string()).unwrap_or_default()
    ));
    let exe = std::env::current_exe().expect("current_exe");
    let mut args: Vec<String> = vec!["inject".into(), mount.display().to_string()];
    args.extend(faults.iter().cloned());
    let child = std::process::Command::new(exe)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        report.owed("faults", format!("could not start the injecting filesystem for {faults:?}"));
        return;
    };
    // Wait for the mount rather than sleeping a guessed interval.
    let marker = mount.join("source.bin");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !marker.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !marker.exists() {
        let _ = child.kill();
        let _ = child.wait();
        report.owed(
            "faults",
            format!("the injecting filesystem never appeared for {faults:?} — is /dev/fuse readable?"),
        );
        return;
    }
    println!("═══ injected: {} ═══", faults.join(" "));
    body(report, &mount);
    println!();
    unmount(&mount);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir(&mount);
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn probe_mode(extra: &[String]) {
    header("filesystem capabilities");
    let (targets, skipped) = probe::discover(extra);
    for s in &skipped {
        println!("  {} — SKIPPED: {}", s.dir.display(), s.reason);
    }
    if !skipped.is_empty() {
        println!();
    }
    for t in targets {
        let c = &t.caps;
        println!("  {}", t.label);
        println!("    reflink            {}", probe::describe(&c.reflink));
        println!("    copy_file_range    {}", probe::describe(&c.copy_file_range));
        println!("    user.* xattrs      {}", probe::describe(&c.user_xattr));
        println!("    fallocate          {}", probe::describe(&c.fallocate));
        println!("    hardlinks          {}", probe::describe(&c.hardlinks));
        println!("    symlinks           {}", probe::describe(&c.symlinks));
        println!("    sparse files       {}", if c.sparse { "yes" } else { "no" });
        println!("    POSIX mode bits    {}", if c.mode_bits { "yes" } else { "no" });
        println!("    timestamp error    {}", crate::checks::residue(c.time_residue_ns));
        println!();
    }
}

fn run_mode(args: &[String]) {
    // `--strict` is for a CI runner that is supposed to have every filesystem: it
    // turns "owed" into a failure, so a filesystem quietly disappearing from the
    // image is caught instead of being reported as a pass.
    let strict = args.iter().any(|a| a == "--strict");
    let extra: Vec<String> = args.iter().filter(|a| !a.starts_with("--")).cloned().collect();
    header(if strict { "the torture matrix (strict)" } else { "the torture matrix" });
    let (targets, skipped) = probe::discover(&extra);
    let mut report = report::Report::default();
    // A directory that was named and cannot be used is a failure of the run, not
    // a footnote: somebody asked for that filesystem to be tested.
    for s in &skipped {
        let evidence = format!("{}: {}", s.dir.display(), s.reason);
        if s.named {
            report.fail("target", evidence);
        } else {
            report.owed("target", evidence);
        }
    }
    if targets.is_empty() {
        eprintln!("no writable filesystem found");
        println!("  {}", report.summary());
        std::process::exit(1);
    }
    // One fixture per source filesystem, then one pair per destination. The
    // same-filesystem pairs come first because a cross-filesystem failure is only
    // interesting once the simple case works.
    let mut first_source: Option<(String, std::path::PathBuf)> = None;
    for t in &targets {
        let short = short_name(&t.caps.fstype, &t.dir);
        println!("═══ {} ═══", t.label);
        let src = t.dir.join("src");
        if let Some(fx) = checks::build_source(&mut report, &short, &src) {
            checks::run_pair(&mut report, &fx, t, &format!("{short}→{short}"));
            if first_source.is_none() {
                first_source = Some((short.clone(), fx.root.clone()));
            }
        }
        println!();
    }
    // A matrix whose filesystems all say yes has not been tested. NTFS says no to
    // several things, which is the point of including it — and the pair that
    // matters is a POSIX filesystem *to* NTFS, because that is what copying to a
    // USB drive is.
    with_ntfs(&mut report, first_source.as_ref());
    faults(&mut report);
    for t in &targets {
        tree::remove_tree(&t.dir);
    }
    println!("  {}", report.summary());
    // One machine-readable line, so a CI job can assert on the numbers rather
    // than grepping prose.
    println!(
        "  counts pass={} fail={} limitation={} owed={} total={}",
        report.count(report::Outcome::Pass),
        report.count(report::Outcome::Fail),
        report.count(report::Outcome::Limitation),
        report.count(report::Outcome::Owed),
        report.count(report::Outcome::Pass)
            + report.count(report::Outcome::Fail)
            + report.count(report::Outcome::Limitation)
            + report.count(report::Outcome::Owed),
    );
    if strict && report.owed_any() {
        eprintln!(
            "  --strict: {} checks were left unmeasured, which on a runner that is supposed to \
             have every filesystem is the regression worth catching",
            report.count(report::Outcome::Owed)
        );
    }
    std::process::exit(i32::from(report.failed() || (strict && report.owed_any())));
}
