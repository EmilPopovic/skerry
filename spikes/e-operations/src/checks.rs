//! The checks, in the order a run executes them. Every one compares the
//! destination against the source; none of them asks the engine what it did.

use std::path::Path;

use rustix::fs::{self, AtFlags};
use rustix::io::Errno;

use crate::copy::{self, Options, Strategy};
use crate::fixture::{self, Fixture};
use crate::probe::Target;
use crate::report::{Outcome, Report};
use crate::{tree, verify};

/// Build the source tree and report what the filesystem under it would not
/// accept. Returns `None` only if the directory itself could not be made.
pub fn build_source(report: &mut Report, label: &str, dir: &Path) -> Option<Fixture> {
    tree::remove_tree(dir);
    let fx = match Fixture::build(dir) {
        Ok(f) => f,
        Err(e) => {
            report.fail("fixture", format!("{label}: could not create the tree: {e}"));
            return None;
        }
    };
    let evidence = format!(
        "{} files and {} refusals: {}",
        fx.names.len(),
        fx.refused.len(),
        if fx.refused.is_empty() {
            "the filesystem accepted every fixture".to_string()
        } else {
            fx.refused
                .iter()
                .map(|(what, e)| format!("{what} ({})", errno_word(e)))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    // A refusal is a property of the filesystem, not a failure of the fixture —
    // and it is one of the more useful things this spike produces.
    report.record(
        format!("fixture/{label}"),
        if fx.refused.is_empty() { Outcome::Pass } else { Outcome::Limitation },
        evidence,
    );
    Some(fx)
}

fn errno_word(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(84) => "EILSEQ — the name cannot be encoded".into(),
        Some(95) => "EOPNOTSUPP".into(),
        Some(1) => "EPERM".into(),
        Some(c) => format!("errno {c}"),
        None => e.to_string(),
    }
}

/// One (source, destination) pair. Splitting it this way is what makes a foreign
/// destination testable: the interesting case for NTFS is not NTFS to itself, it
/// is ext4 to NTFS — which is what copying to a USB drive actually is.
pub fn run_pair(report: &mut Report, fx: &Fixture, target: &Target, pair: &str) {
    let dst = target.dir.join("dst");
    tree::remove_tree(&dst);

    let opts = Options { replace: false, fsync: false, ..Default::default() };
    let t = tree::copy_tree(&fx.root, &dst, &opts);

    let strategies: Vec<String> = t.strategies.iter().map(|(k, v)| format!("{v}x {k}")).collect();
    let placements: Vec<String> = t.placements.iter().map(|(k, v)| format!("{v}x {k}")).collect();
    let evidence = format!(
        "{} files, {} symlinks, {} director{}, {} names re-linked instead of copied; \
         strategies {}; placement {}; {} failures",
        t.files,
        t.symlinks,
        t.dirs,
        if t.dirs == 1 { "y" } else { "ies" },
        t.relinked,
        strategies.join(", "),
        placements.join(", "),
        t.failures.len()
    );
    if t.failures.is_empty() {
        report.pass(format!("tree/{pair}"), evidence);
    } else {
        let detail: Vec<String> = t
            .failures
            .iter()
            .take(4)
            .map(|(p, f)| format!("{}: {:?} at {}", name_of(p), f.errno, f.stage))
            .collect();
        // A destination that cannot represent a name refuses it, and that is a
        // limitation of the destination rather than a defect in the copy — as
        // long as it is reported, which is what `detail` is.
        let encoding_only = t.failures.iter().all(|(_, f)| f.errno == Errno::ILSEQ);
        report.record(
            format!("tree/{pair}"),
            if encoding_only { Outcome::Limitation } else { Outcome::Fail },
            format!("{evidence} — {}", detail.join("; ")),
        );
    }
    // Nothing of ours may survive a copy, whether it succeeded or failed. The
    // tree is walked for the temporary-file prefix as well as trusting the
    // engine's own accounting, because the engine is what is under test.
    let leftovers: Vec<String> = t
        .failures
        .iter()
        .filter_map(|(_, f)| f.leftover.as_ref())
        .chain(t.orphans.iter())
        .map(|p| name_of(p))
        .collect();
    let stray = stray_temporaries(&dst);
    let evidence = format!(
        "{} reported by the engine, {} found by walking the destination",
        leftovers.len(),
        stray.len()
    );
    if leftovers.is_empty() && stray.is_empty() {
        report.pass(format!("no-temp-files/{pair}"), evidence);
    } else {
        report.fail(
            format!("no-temp-files/{pair}"),
            format!("{evidence}: {:?} {:?}", leftovers, stray),
        );
    }

    bytes(report, fx, &fx.root, &dst, pair);
    sparse(report, fx, &dst, target, pair);
    metadata(report, &fx.root, &dst, target, pair);
    xattrs(report, &fx.root, &dst, target, &t, pair);
    hardlinks(report, &fx.root, &dst, target, pair);
    symlinks(report, fx, &dst, pair);
    names(report, &fx.root, &dst, pair);
    self_copy(report, &fx.root, pair);
    clobber(report, &fx.root, &dst, pair);
    reflink(report, target, &fx.root, pair);
    moves(report, &fx.root, target, pair);

    tree::remove_tree(&dst);
}

fn bytes(report: &mut Report, fx: &Fixture, src: &Path, dst: &Path, pair: &str) {
    let mut checked = 0usize;
    let mut differ = Vec::new();
    let mut unrepresentable = 0usize;
    for p in &fx.names {
        let Ok(rel) = p.strip_prefix(src) else { continue };
        let d = dst.join(rel);
        if !d.exists() {
            // A name the destination could not encode is accounted for by the
            // tree check; counting it as a byte difference would say the copy
            // corrupted something.
            if std::str::from_utf8(rel.as_os_str().as_encoded_bytes()).is_err() {
                unrepresentable += 1;
            } else {
                differ.push(format!("{}: missing", name_of(p)));
            }
            continue;
        }
        match verify::first_difference(p, &d) {
            Ok(None) => checked += 1,
            Ok(Some(at)) => differ.push(format!("{}: differs at byte {at}", name_of(p))),
            Err(e) => differ.push(format!("{}: {e}", name_of(p))),
        }
    }
    let evidence = format!(
        "{checked} files compared byte for byte{}",
        if unrepresentable == 0 {
            String::new()
        } else {
            format!("; {unrepresentable} skipped because the destination cannot encode the name")
        }
    );
    if differ.is_empty() {
        report.pass(format!("bytes/{pair}"), evidence);
    } else {
        report.fail(format!("bytes/{pair}"), format!("{evidence}; {}", differ.join("; ")));
    }
}

/// A hole that becomes zeros passes a byte comparison and fails the user. The
/// extent map and `st_blocks` are the only things that can tell the difference.
fn sparse(report: &mut Report, fx: &Fixture, dst: &Path, target: &Target, pair: &str) {
    let src = &fx.root;
    let a = src.join("sparse.bin");
    let b = dst.join("sparse.bin");
    if fx.sparse_size.is_none() || !a.exists() {
        report.limitation(
            format!("sparse/{pair}"),
            "the source filesystem could not hold a sparse file, so there is nothing to preserve",
        );
        return;
    }
    if !target.caps.sparse {
        report.limitation(
            format!("sparse/{pair}"),
            "the destination reports no holes, so a sparse file necessarily becomes a dense one",
        );
        return;
    }
    if !b.exists() {
        report.fail(format!("sparse/{pair}"), "the sparse file is missing from the copy");
        return;
    }
    let (Ok(ea), Ok(eb)) = (verify::extents(&a), verify::extents(&b)) else {
        report.fail(format!("sparse/{pair}"), "could not read an extent map");
        return;
    };
    let evidence = format!(
        "{} apparent bytes in {} extents; source occupies {} KiB, destination {} KiB",
        ea.size,
        ea.runs.len(),
        ea.blocks / 2,
        eb.blocks / 2
    );
    // The extent boundaries themselves may legitimately differ — a filesystem is
    // allowed to merge adjacent extents — so the test is the *allocated size*,
    // with the apparent size unchanged.
    let ok = ea.size == eb.size && eb.blocks <= ea.blocks.max(16) * 4;
    report.record(format!("sparse/{pair}"), if ok { Outcome::Pass } else { Outcome::Fail }, evidence);
}

fn metadata(report: &mut Report, src: &Path, dst: &Path, target: &Target, pair: &str) {
    let a = src.join("mode-4755.bin");
    let b = dst.join("mode-4755.bin");
    if !a.exists() {
        report.limitation(
            format!("metadata/{pair}"),
            "the source filesystem refused a setuid file with a nanosecond mtime, so there is \
             nothing to compare",
        );
        return;
    }
    let (Ok(ma), Ok(mb)) = (verify::meta(&a), verify::meta(&b)) else {
        report.fail(format!("metadata/{pair}"), format!("{} exists and its copy does not", name_of(&a)));
        return;
    };
    let allowed_ns = target.caps.time_residue_ns;
    let mtime_off_ns =
        (mb.mtime.0 - ma.mtime.0) * 1_000_000_000 + (mb.mtime.1 - ma.mtime.1);
    let evidence = format!(
        "mode {:o} to {:o} (the filesystem {} POSIX mode bits); mtime off by {mtime_off_ns} ns, \
         against {} measured on a probe file",
        ma.mode,
        mb.mode,
        if target.caps.mode_bits { "keeps" } else { "does not keep" },
        residue(allowed_ns)
    );
    // Each attribute is judged against what the destination can actually hold.
    // "Not preserved" and "not preservable" are different results and only the
    // first is a defect.
    let mode = attribute(ma.mode == mb.mode, !target.caps.mode_bits);
    // `<=`, not `<`: an error equal to the one the probe observed is the
    // filesystem rounding exactly as it always does.
    let time = attribute(
        mtime_off_ns == 0,
        allowed_ns == u64::MAX || mtime_off_ns.unsigned_abs() <= allowed_ns,
    );
    report.record(format!("metadata/{pair}"), worse_of(mode, time), evidence);
}

/// Preserved, not preservable, or lost. The middle one is a property of the
/// destination and the last one is a defect, and collapsing them is how a copy
/// engine comes to claim it preserved something it rounded off.
/// Human-readable form of a measured timestamp error.
pub fn residue(ns: u64) -> String {
    match ns {
        u64::MAX => "timestamps not settable".into(),
        0 => "exact to the nanosecond".into(),
        n if n >= 1_000_000_000 => format!("{} s of rounding", n / 1_000_000_000),
        n => format!("{n} ns of rounding"),
    }
}

fn attribute(preserved: bool, excused: bool) -> Outcome {
    match (preserved, excused) {
        (true, _) => Outcome::Pass,
        (false, true) => Outcome::Limitation,
        (false, false) => Outcome::Fail,
    }
}

fn worse_of(a: Outcome, b: Outcome) -> Outcome {
    let rank = |o: Outcome| match o {
        Outcome::Pass => 0,
        Outcome::Owed => 1,
        Outcome::Limitation => 2,
        Outcome::Fail => 3,
    };
    if rank(a) >= rank(b) { a } else { b }
}

fn xattrs(report: &mut Report, src: &Path, dst: &Path, target: &Target, t: &tree::Tree, pair: &str) {
    let a = src.join("with-xattrs.bin");
    let b = dst.join("with-xattrs.bin");
    if !a.exists() {
        report.limitation(
            format!("xattrs/{pair}"),
            "the source filesystem would not create the attribute-bearing file",
        );
        return;
    }
    let (Ok(xa), Ok(xb)) = (verify::xattrs(&a), verify::xattrs(&b)) else {
        report.fail(format!("xattrs/{pair}"), "could not list attributes on both files");
        return;
    };
    let user_a: Vec<_> = xa.iter().filter(|(n, _)| n.starts_with("user.")).collect();
    let user_b: Vec<_> = xb.iter().filter(|(n, _)| n.starts_with("user.")).collect();
    let reported: Vec<String> = t
        .unpreserved
        .iter()
        .map(|(p, what, e)| format!("{} {what} ({e:?})", name_of(p)))
        .collect();
    let evidence = format!(
        "{} user attributes on the source, {} on the copy; {} reported as unpreservable{}",
        user_a.len(),
        user_b.len(),
        reported.len(),
        if reported.is_empty() { String::new() } else { format!(": {}", reported.join(", ")) }
    );
    if user_a.is_empty() {
        // Nothing to preserve: the source filesystem refused the attributes too.
        // Asserting that the destination lost nothing would be asserting nothing.
        report.limitation(
            format!("xattrs/{pair}"),
            format!("{evidence}; the source filesystem holds no user attributes either"),
        );
    } else if target.caps.user_xattr.is_err() {
        // The important half: it must be *reported*, not silently dropped.
        let outcome = if reported.is_empty() { Outcome::Fail } else { Outcome::Limitation };
        report.record(format!("xattrs/{pair}"), outcome, format!("{evidence}; the destination refuses user attributes"));
    } else if user_a == user_b {
        report.pass(format!("xattrs/{pair}"), evidence);
    } else {
        report.fail(format!("xattrs/{pair}"), evidence);
    }
}

/// Three names for one inode must be one inode afterwards. Anything else silently
/// triples the space and loses the relationship.
/// Three names for one inode must be one inode afterwards.
///
/// The test counts *names in the copied tree that share an inode*, not the
/// filesystem's link count. ntfs-3g reports one more link than there are names,
/// and testing its bookkeeping instead of the user-visible property turns a
/// working copy into a failure.
fn hardlinks(report: &mut Report, src: &Path, dst: &Path, target: &Target, pair: &str) {
    let names = ["hardlink-a", "hardlink-b", "hardlink-c"];
    let in_source = names.iter().filter(|n| src.join(n).exists()).count();
    if in_source < names.len() {
        report.limitation(
            format!("hardlinks/{pair}"),
            format!(
                "the source filesystem accepted {in_source} of {} names for one inode, so there \
                 is no set to preserve",
                names.len()
            ),
        );
        return;
    }
    let mut found = Vec::new();
    for n in names {
        match verify::meta(&dst.join(n)) {
            Ok(m) => found.push((n, m.dev, m.ino, m.links)),
            Err(e) => {
                let outcome = if target.caps.hardlinks.is_err() {
                    // A destination without hardlinks duplicates the data, which
                    // is the only thing it can do.
                    Outcome::Limitation
                } else {
                    Outcome::Fail
                };
                report.record(
                    format!("hardlinks/{pair}"),
                    outcome,
                    format!("{n} missing from the copy ({e:?})"),
                );
                return;
            }
        }
    }
    let mut ids: Vec<(u64, u64)> = found.iter().map(|(_, d, i, _)| (*d, *i)).collect();
    ids.sort_unstable();
    ids.dedup();
    let first = (found[0].1, found[0].2);
    let shared = found.iter().filter(|(_, d, i, _)| (*d, *i) == first).count();
    let evidence = format!(
        "3 names share {} inode(s); {shared} of them resolve to the first one; the filesystem \
         reports a link count of {}",
        ids.len(),
        found[0].3
    );
    let ok = ids.len() == 1;
    let outcome = if ok {
        Outcome::Pass
    } else if target.caps.hardlinks.is_err() {
        Outcome::Limitation
    } else {
        Outcome::Fail
    };
    report.record(format!("hardlinks/{pair}"), outcome, evidence);
}

fn symlinks(report: &mut Report, fx: &Fixture, dst: &Path, pair: &str) {
    let src = &fx.root;
    let all = ["link-relative", "link-absolute", "link-dangling", "link-invalid-target"];
    // Only the ones the source filesystem allowed. FAT allows none, and faulting
    // a copy for not reproducing a symlink that never existed is asserting nothing.
    let names: Vec<&str> = all.iter().copied().filter(|n| fx.has_symlink(n)).collect();
    if names.is_empty() {
        report.limitation(
            format!("symlinks/{pair}"),
            "the source filesystem accepts no symlinks, so there are none to copy",
        );
        return;
    }
    let mut wrong = Vec::new();
    let mut ok = 0;
    for n in names.iter().copied() {
        let (a, b) = (src.join(n), dst.join(n));
        let ma = verify::meta_nofollow(&b);
        match (verify::symlink_target(&a), verify::symlink_target(&b), ma) {
            (Ok(ta), Ok(tb), Ok(_)) if ta == tb => ok += 1,
            (Ok(ta), Ok(tb), _) => wrong.push(format!("{n}: target {ta:?} became {tb:?}")),
            // A copy that followed the link wrote a regular file, so readlink on
            // the destination fails. That is the failure worth naming.
            (Ok(_), Err(e), _) => wrong.push(format!("{n}: not a symlink in the copy ({e:?})")),
            (Err(e), _, _) => wrong.push(format!("{n}: unreadable in the source ({e:?})")),
        }
    }
    let evidence = format!(
        "{ok} of {} symlinks copied as symlinks with identical targets ({} of {} exist in the \
         source)",
        names.len(),
        names.len(),
        all.len()
    );
    if wrong.is_empty() {
        report.pass(format!("symlinks/{pair}"), evidence);
    } else {
        // A target that is not valid UTF-8 cannot exist on a filesystem whose
        // names are UTF-16. That is the destination's limit, and the only failure
        // would be losing one it could have held.
        let unencodable: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| {
                verify::symlink_target(&src.join(n))
                    .map(|t| std::str::from_utf8(&t).is_err())
                    .unwrap_or(false)
            })
            .collect();
        let all_excused = wrong.iter().all(|w| unencodable.iter().any(|n| w.starts_with(n)));
        report.record(
            format!("symlinks/{pair}"),
            if all_excused { Outcome::Limitation } else { Outcome::Fail },
            format!("{evidence}; {}", wrong.join("; ")),
        );
    }
}

fn names(report: &mut Report, src: &Path, dst: &Path, pair: &str) {
    let all = fixture::adversarial_names();
    // Only the names the *source* filesystem accepted. exFAT refuses a newline and
    // invalid UTF-8 outright, so a copy cannot be faulted for their absence.
    let want: Vec<_> = all.iter().filter(|n| src.join(n).exists()).cloned().collect();
    if want.is_empty() {
        report.limitation(
            format!("names/{pair}"),
            format!("the source filesystem accepted none of the {} adversarial names", all.len()),
        );
        return;
    }
    let missing: Vec<String> = want
        .iter()
        .filter(|n| !dst.join(n).exists())
        .map(|n| n.to_string_lossy().chars().take(20).collect())
        .collect();
    let evidence = format!(
        "{} of {} names the source could hold round-tripped, out of {} tried: 255 bytes, an \
         embedded newline, invalid UTF-8, and a leading dash",
        want.len() - missing.len(),
        want.len(),
        all.len()
    );
    if missing.is_empty() {
        report.pass(format!("names/{pair}"), evidence);
    } else {
        // A name the destination cannot encode is the destination's limit. A name
        // it could encode and lost is ours.
        let encodable_missing = want
            .iter()
            .filter(|n| !dst.join(n).exists())
            .any(|n| std::str::from_utf8(n.as_encoded_bytes()).is_ok());
        report.record(
            format!("names/{pair}"),
            if encodable_missing { Outcome::Fail } else { Outcome::Limitation },
            format!("{evidence}; missing {}", missing.join(", ")),
        );
    }
}

/// `cp a a` that opens the destination for writing empties the file before it
/// reads a byte. This is the check that the engine refuses instead.
fn self_copy(report: &mut Report, src: &Path, pair: &str) {
    let path = src.join("plain.bin");
    let before = verify::meta(&path).map(|m| m.size).unwrap_or(0);
    let result = copy::copy_file(&path, &path, &Options { replace: true, ..Default::default() });
    let after = verify::meta(&path).map(|m| m.size).unwrap_or(0);
    let evidence = match &result {
        Ok(_) => format!("the copy was allowed; size {before} became {after}"),
        Err(f) => format!("refused at {} with {:?}; size still {after} of {before}", f.stage, f.errno),
    };
    let ok = result.is_err() && before == after && before > 0;
    report.record(format!("self-copy/{pair}"), if ok { Outcome::Pass } else { Outcome::Fail }, evidence);
}

/// An existing destination is never silently overwritten, and the refusal is
/// atomic — `RENAME_NOREPLACE` rather than a check followed by a race.
fn clobber(report: &mut Report, src: &Path, dst: &Path, pair: &str) {
    let from = src.join("plain.bin");
    let onto = dst.join("mode-4755.bin");
    if !from.exists() || !onto.exists() {
        report.limitation(
            format!("clobber/{pair}"),
            "the source or the destination file does not exist, so there is nothing to refuse to \
             overwrite",
        );
        return;
    }
    let before = verify::meta(&onto).map(|m| (m.size, m.mtime)).ok();
    let result = copy::copy_file(&from, &onto, &Options { replace: false, ..Default::default() });
    let after = verify::meta(&onto).map(|m| (m.size, m.mtime)).ok();
    let evidence = match &result {
        Ok(_) => "the copy was allowed over an existing file".to_string(),
        Err(f) => format!("refused at {} with {:?}", f.stage, f.errno),
    };
    let ok = result.is_err() && before == after && before.is_some();
    report.record(
        format!("clobber/{pair}"),
        if ok { Outcome::Pass } else { Outcome::Fail },
        format!("{evidence}; the destination is unchanged: {}", before == after),
    );
}

/// A move within a filesystem, and a move across one. The second is a copy
/// followed by an unlink, and the only thing that keeps it safe is the order:
/// verify, then remove. This check runs both and confirms the source is gone
/// exactly when the destination is good.
fn moves(report: &mut Report, src: &Path, target: &Target, pair: &str) {
    // Same filesystem: a rename, which cannot lose anything.
    let a = src.join("move-within.bin");
    let b = src.join("move-within-moved.bin");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    if fixture::write_pattern(&a, 4096).is_err() {
        report.owed(format!("move/{pair}"), "could not write a source file");
        return;
    }
    let within = copy::move_file(&a, &b, &Options::default());
    let within_ok = matches!(&within, Ok(m) if m.renamed) && !a.exists() && b.exists();

    // Across filesystems: the destination is on the target, the source is not.
    let across_src = std::env::temp_dir().join(format!("skerry-spike-e-move-{}", std::process::id()));
    let across_dst = target.dir.join("moved-across.bin");
    let _ = std::fs::remove_file(&across_src);
    let _ = std::fs::remove_file(&across_dst);
    let across = if fixture::write_pattern(&across_src, 65536).is_ok() {
        Some(copy::move_file(&across_src, &across_dst, &Options::default()))
    } else {
        None
    };
    let across_ok = match &across {
        // A rename here means the two paths were on the same filesystem after
        // all, which is a valid outcome and not what this half is testing.
        Some(Ok(m)) => (m.verified || m.renamed) && !across_src.exists() && across_dst.exists(),
        // No source: nothing to check, and nothing was lost.
        None => true,
        Some(Err(_)) => across_src.exists() && !across_dst.exists(),
    };
    let evidence = format!(
        "within a filesystem: {}; across: {}; the source survives exactly when the destination \
         does not: {}",
        match &within {
            Ok(m) if m.renamed => format!("renamed in {:.3} ms", m.elapsed.as_secs_f64() * 1000.0),
            Ok(m) => format!("copied {} bytes and verified {}", m.bytes, m.verified),
            Err(f) => format!("failed at {} with {:?}", f.stage, f.errno),
        },
        match &across {
            Some(Ok(m)) if m.renamed => "renamed — the paths were on one filesystem".to_string(),
            Some(Ok(m)) => format!("copied {} bytes, verified before unlinking: {}", m.bytes, m.verified),
            Some(Err(f)) => format!("failed at {} with {:?}", f.stage, f.errno),
            None => "not attempted".to_string(),
        },
        across_ok
    );
    let _ = std::fs::remove_file(&b);
    let _ = std::fs::remove_file(&across_src);
    let _ = std::fs::remove_file(&across_dst);
    report.record(
        format!("move/{pair}"),
        if within_ok && across_ok { Outcome::Pass } else { Outcome::Fail },
        evidence,
    );
}

/// The bar's third condition: **reflink measurably instant on btrfs**.
///
/// The first version of this compared `FICLONE` against a copy with `reflink:
/// false` and called anything under 4x a failure — and on btrfs it measured 0x,
/// because `copy_file_range` **already reflinks** on btrfs and xfs. The comparison
/// was a reflink against a reflink. That is a finding about the strategy ladder,
/// not just about the check: the explicit `FICLONE` path buys nothing over
/// `copy_file_range` on the two filesystems that support either.
///
/// So the control is a *true* byte copy — `kernel_copy: false` as well — and the
/// primary signal is **allocated blocks**, not time. A reflink shares extents, so
/// the filesystem's free space does not move; a byte copy consumes the file's size.
/// That is what "instant" means here, and unlike a duration it does not depend on
/// what happens to be in page cache.
fn reflink(report: &mut Report, target: &Target, src: &Path, pair: &str) {
    if let Err(e) = &target.caps.reflink {
        report.owed(
            format!("reflink/{pair}"),
            format!(
                "FICLONE refused with {e:?} on {}. The bar's reflink condition needs btrfs or \
                 xfs; `sudo ./with-root.sh` creates both",
                target.caps.fstype
            ),
        );
        return;
    }

    // Large enough that a byte copy has to move real data, and small enough to
    // fit a 2 GiB loopback image twice over.
    const SIZE: usize = 192 * 1024 * 1024;
    let big = src.join("reflink-source.bin");
    let cloned = src.join("reflink-cloned.bin");
    let copied = src.join("reflink-copied.bin");
    for p in [&big, &cloned, &copied] {
        let _ = std::fs::remove_file(p);
    }
    if fixture::write_pattern(&big, SIZE).is_err() {
        report.owed(format!("reflink/{pair}"), "not enough room for the reflink source file");
        return;
    }
    // btrfs delays allocation, so free space does not move until the data is
    // flushed. Without this the control copy of 192 MiB appeared to consume zero
    // bytes — and the check passed anyway, because `0 * 8 < 1`.
    let settle = |p: &Path| {
        if let Ok(f) = std::fs::File::open(p) {
            let _ = rustix::fs::fsync(&f);
            let _ = rustix::fs::syncfs(&f);
        }
    };
    settle(&big);
    let free_before = free_bytes(src);

    let clone =
        copy::copy_file(&big, &cloned, &Options { reflink: true, fsync: true, ..Default::default() });
    settle(&cloned);
    let free_after_clone = free_bytes(src);

    let plain = copy::copy_file(
        &big,
        &copied,
        // Both kernel-side paths off: on btrfs and xfs `copy_file_range` is itself
        // a clone, so leaving it on measures nothing.
        &Options { reflink: false, kernel_copy: false, fsync: true, ..Default::default() },
    );
    settle(&copied);
    let free_after_copy = free_bytes(src);

    let (Ok(c), Ok(p)) = (&clone, &plain) else {
        report.fail(format!("reflink/{pair}"), format!("a copy failed: {clone:?} / {plain:?}"));
        for f in [&big, &cloned, &copied] {
            let _ = std::fs::remove_file(f);
        }
        return;
    };
    let clone_cost = free_before.saturating_sub(free_after_clone);
    let copy_cost = free_after_clone.saturating_sub(free_after_copy);
    let evidence = format!(
        "{} MiB: FICLONE consumed {} KiB of free space in {:.1} ms; a true byte copy of the same \
         file consumed {} KiB in {:.1} ms. copy_file_range on this filesystem is itself a clone, \
         so it is excluded from the control; the time is similar because both fit in page cache, \
         and the space is the measurement",
        SIZE / (1 << 20),
        clone_cost / 1024,
        c.elapsed.as_secs_f64() * 1000.0,
        copy_cost / 1024,
        p.elapsed.as_secs_f64() * 1000.0,
    );
    // Both halves have to hold, and the second one is what keeps the first
    // honest: a clone that costs nothing proves nothing unless the control
    // actually costs something. Before this, btrfs passed on `0 * 8 < 1` because
    // delayed allocation had not been flushed and *neither* copy appeared to
    // consume space.
    let clone_free = clone_cost < SIZE as u64 / 16;
    let control_paid = copy_cost > SIZE as u64 / 2;
    let outcome = match (c.strategy == Strategy::Reflink && clone_free, control_paid) {
        (true, true) => Outcome::Pass,
        // The control did not consume space either, so nothing was compared.
        (_, false) => Outcome::Fail,
        (false, _) => Outcome::Fail,
    };
    let evidence = if control_paid {
        evidence
    } else {
        format!(
            "{evidence}. The control consumed no space either, so this measured nothing — the \
             filesystem has not accounted the allocation yet"
        )
    };
    report.record(format!("reflink/{pair}"), outcome, evidence);
    for f in [&big, &cloned, &copied] {
        let _ = std::fs::remove_file(f);
    }
}

/// Any surviving `.skerry-copy-*` name. A whole-tree scan rather than a question
/// to the engine, because the engine is the thing being tested.
fn stray_temporaries(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(".skerry-copy-") {
            out.push(name);
        }
        if e.path().is_dir() {
            out.extend(stray_temporaries(&e.path()));
        }
    }
    out
}

/// Free space on the filesystem holding `path`, in bytes.
fn free_bytes(path: &Path) -> u64 {
    rustix::fs::statvfs(path).map(|s| s.f_bavail * s.f_frsize).unwrap_or(0)
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().chars().take(24).collect()).unwrap_or_default()
}

/// Faults, on the filesystem that produces them on demand. Separate from `run`
/// because the injected filesystem is not a general-purpose destination: it has
/// no holes and no hardlinks, and running the whole matrix on it would report
/// limitations that say nothing.
pub fn fault_enospc(report: &mut Report, mount: &Path, source_bytes: u64, limit: u64) {
    let source = mount.join("source.bin");
    let dest = mount.join("destination.bin");

    // ENOSPC, the bar's second condition. The engine writes to a temporary file
    // and renames, so a failure at any byte must leave the destination name
    // absent — not empty, absent.
    let before: Vec<_> = entries(mount);
    let result = copy::copy_file(&source, &dest, &Options { fsync: true, ..Default::default() });
    let after: Vec<_> = entries(mount);
    let leftover: Vec<&String> = after.iter().filter(|n| !before.contains(n)).collect();
    let evidence = match &result {
        Ok(c) => format!("the copy succeeded, {} bytes — the fault did not fire", c.bytes),
        Err(f) => format!(
            "failed at {} with {:?} after {} of {source_bytes} bytes, {} short of the file; \
             the destination filesystem accepted {limit} bytes",
            f.stage,
            f.errno,
            f.bytes_written,
            source_bytes - f.bytes_written.min(source_bytes)
        ),
    };
    let clean = leftover.is_empty() && !dest.exists();
    let failed_with_enospc = matches!(&result, Err(f) if f.errno == Errno::NOSPC);
    report.record(
        "enospc",
        if failed_with_enospc && clean { Outcome::Pass } else { Outcome::Fail },
        format!(
            "{evidence}; the destination name exists afterwards: {}; files left behind: {:?}",
            dest.exists(),
            leftover
        ),
    );
}

/// A read error partway through. The same shape as `ENOSPC` from the other side:
/// a disk that has begun to fail reads fine below the threshold, which is what
/// makes it dangerous — a naive copy writes a truncated file and reports success.
pub fn fault_eio(report: &mut Report, mount: &Path, dest_dir: &Path, limit: u64) {
    let source = mount.join("source.bin");
    let dest = dest_dir.join("from-failing-disk.bin");
    let _ = std::fs::remove_file(&dest);
    let result = copy::copy_file(&source, &dest, &Options::default());
    let evidence = match &result {
        Ok(c) => format!("the copy reported success with {} bytes — a truncated file", c.bytes),
        Err(f) => format!(
            "failed at {} with {:?} after {} bytes, with the fault set at {limit}",
            f.stage, f.errno, f.bytes_written
        ),
    };
    let ok = matches!(&result, Err(f) if f.errno == Errno::IO) && !dest.exists();
    report.record(
        "eio",
        if ok { Outcome::Pass } else { Outcome::Fail },
        format!("{evidence}; the destination name exists afterwards: {}", dest.exists()),
    );
}

/// A destination that refuses extended attributes. The requirement is not that
/// the copy succeeds — it is that what could not be preserved is *named*.
pub fn fault_no_xattr(report: &mut Report, mount: &Path, src_dir: &Path) {
    let source = src_dir.join("attrs-for-injection.bin");
    let _ = std::fs::remove_file(&source);
    if fixture::write_pattern(&source, 4096).is_err() {
        report.owed("no-xattr", "could not write a source file");
        return;
    }
    fs::setxattr(&source, "user.skerry.one", b"first", fs::XattrFlags::empty()).ok();
    fs::setxattr(&source, "user.skerry.two", b"second", fs::XattrFlags::empty()).ok();
    let dest = mount.join("with-attrs.bin");
    let result = copy::copy_file(&source, &dest, &Options::default());
    let evidence = match &result {
        Ok(c) => format!(
            "the copy completed with {} bytes and reported {} attributes as unpreservable: {}",
            c.bytes,
            c.unpreserved.len(),
            c.unpreserved
                .iter()
                .map(|(n, e)| format!("{n} ({e:?})"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(f) => format!("the copy failed at {} with {:?}", f.stage, f.errno),
    };
    let ok = matches!(&result, Ok(c) if c.unpreserved.iter().any(|(n, _)| n.starts_with("user.")));
    report.record("no-xattr", if ok { Outcome::Pass } else { Outcome::Fail }, evidence);
    let _ = std::fs::remove_file(&source);
}

/// A destination that never accepts a whole buffer. Nothing about this should be
/// visible in the result: a write loop that treats a short write as an error, or
/// as a success, produces a corrupt file either way.
pub fn fault_short_write(report: &mut Report, mount: &Path, src_dir: &Path, chunk: usize) {
    let source = src_dir.join("short-write-source.bin");
    let _ = std::fs::remove_file(&source);
    if fixture::write_pattern(&source, 256 * 1024).is_err() {
        report.owed("short-write", "could not write a source file");
        return;
    }
    let dest = mount.join("short-write.bin");
    let result = copy::copy_file(&source, &dest, &Options::default());
    let evidence = match &result {
        Ok(c) => {
            let diff = verify::first_difference(&source, &dest);
            format!(
                "{} bytes written {} bytes at a time via {}; first difference from the source: {:?}",
                c.bytes,
                chunk,
                c.strategy.name(),
                diff
            )
        }
        Err(f) => format!("failed at {} with {:?} after {} bytes", f.stage, f.errno, f.bytes_written),
    };
    let ok = matches!(&result, Ok(_))
        && matches!(verify::first_difference(&source, &dest), Ok(None));
    report.record("short-write", if ok { Outcome::Pass } else { Outcome::Fail }, evidence);
    let _ = std::fs::remove_file(&source);
}

/// A destination that runs out of space partway through a *tree*, not a file. The
/// per-file guarantee is that no partial file is left; the per-tree guarantee is
/// weaker and worth stating: every file that exists is complete, and the ones
/// that do not exist were never started.
pub fn fault_tree_enospc(report: &mut Report, mount: &Path, src: &Path, limit: u64) {
    let dst = mount.join("tree");
    let t = crate::tree::copy_tree(src, &dst, &Options::default());
    // Whatever arrived must be byte-exact. A short file would mean the failure
    // was reported for one file while another was quietly truncated.
    let mut incomplete = Vec::new();
    let mut complete = 0usize;
    for p in walk_files(&dst) {
        let Ok(rel) = p.strip_prefix(&dst) else { continue };
        let origin = src.join(rel);
        match verify::first_difference(&origin, &p) {
            Ok(None) => complete += 1,
            Ok(Some(at)) => incomplete.push(format!("{}: differs at {at}", name_of(&p))),
            Err(e) => incomplete.push(format!("{}: {e}", name_of(&p))),
        }
    }
    let stray = stray_temporaries(&dst);
    let evidence = format!(
        "the destination accepted {limit} bytes; {} files copied and {} failed; {complete} of the \
         files that exist are byte-exact; {} incomplete; {} temporary files left",
        t.files,
        t.failures.len(),
        incomplete.len(),
        stray.len()
    );
    // Something must have succeeded and something must have failed, or the check
    // has not exercised the boundary it exists for.
    let ok = incomplete.is_empty() && stray.is_empty() && !t.failures.is_empty() && t.files > 0;
    report.record(
        "enospc-tree",
        if ok { Outcome::Pass } else { Outcome::Fail },
        if incomplete.is_empty() { evidence } else { format!("{evidence}: {}", incomplete.join("; ")) },
    );
}

fn walk_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.is_dir() {
            out.extend(walk_files(&p));
        } else if md.is_file() {
            out.push(p);
        }
    }
    out
}

fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default()
}

/// `AtFlags` is used by the walk; kept referenced so the import stays honest.
const _: Option<AtFlags> = None;
const _: Option<fs::Mode> = None;
