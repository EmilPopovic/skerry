//! Copying a *tree*, which is where the decisions live that a single-file copy
//! does not have to make: what a symlink means, and what a second name for an
//! inode means.
//!
//! Both have a wrong answer that looks like success. Following a symlink copies
//! the target's bytes under the link's name, so the destination is bigger than the
//! source and a later edit does not propagate. Copying each name of a hardlink set
//! separately turns one inode into three, so the destination is three times the
//! size and the relationship the user depended on is gone — and nothing reports
//! either.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rustix::fs::{self, AtFlags, Mode, OFlags};

use sk_fs::copy::{self, Options, Placement};
use sk_model::Problem;

#[derive(Default)]
pub struct Tree {
    /// How files were given their final names. A `CheckThenRename` in here means
    /// the destination filesystem forced a non-atomic placement.
    pub placements: BTreeMap<&'static str, usize>,
    pub files: usize,
    pub symlinks: usize,
    pub dirs: usize,
    /// Second and later names for an inode we had already copied, re-linked
    /// rather than copied again.
    pub relinked: usize,
    pub strategies: BTreeMap<&'static str, usize>,
    /// Metadata the destination would not take. No errno beside it any more:
    /// `sk-fs` reports *what* it could not preserve and puts the reason in the
    /// `Problem` chain, and this walker's job is to count, not to diagnose.
    pub unpreserved: Vec<(PathBuf, String)>,
    pub failures: Vec<(PathBuf, Problem)>,
    /// Temporary files that survived a successful copy. Any entry is a defect.
    pub orphans: Vec<PathBuf>,
}

pub fn strategy_name(s: sk_fs::copy::Strategy) -> &'static str {
    match s {
        sk_fs::copy::Strategy::Reflink => "FICLONE",
        sk_fs::copy::Strategy::KernelCopy => "copy_file_range",
        sk_fs::copy::Strategy::ReadWrite => "read/write",
    }
}

fn placement_name(p: Placement) -> &'static str {
    match p {
        Placement::NoReplace => "renameat2(NOREPLACE)",
        Placement::LinkThenUnlink => "link+unlink",
        Placement::CheckThenRename => "stat+rename (not atomic)",
        Placement::Replace => "rename (replacing)",
    }
}

pub fn copy_tree(src: &Path, dst: &Path, o: Options) -> Tree {
    let mut t = Tree::default();
    // (dev, ino) of a source file to the destination path we already wrote for
    // it. Keyed on the device too, because inode numbers are only unique within
    // one filesystem and a tree can span several.
    let mut inodes: BTreeMap<(u64, u64), PathBuf> = BTreeMap::new();
    walk(src, dst, o, &mut t, &mut inodes);
    t
}

fn walk(src: &Path, dst: &Path, o: Options, t: &mut Tree, inodes: &mut BTreeMap<(u64, u64), PathBuf>) {
    if let Err(e) = std::fs::create_dir_all(dst) {
        let errno = e.raw_os_error().unwrap_or(0);
        t.failures.push((dst.to_path_buf(), copy::problem_for(errno, dst, "mkdir")));
        return;
    }
    t.dirs += 1;

    let Ok(entries) = std::fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // Never follow: the type of the *link* is what decides, not the type of
        // whatever it points at. `from_raw_mode` masks with `S_IFMT`; masking by
        // hand with a single type bit does not, and the first version of this
        // walked into a directory as though it were a file.
        let Ok(st) = fs::statat(fs::CWD, &from, AtFlags::SYMLINK_NOFOLLOW) else { continue };
        let kind = fs::FileType::from_raw_mode(st.st_mode);

        if kind == fs::FileType::Symlink {
            match copy_symlink(&from, &to) {
                Ok(()) => t.symlinks += 1,
                // Classified by `sk-fs` rather than here: a symlink this walker
                // could not create fails for the same reasons a file does, and a
                // second table would be the fork all over again in miniature.
                Err(e) => t.failures.push((
                    from.clone(),
                    copy::problem_for(e.raw_os_error(), &from, "symlink"),
                )),
            }
            continue;
        }
        if kind == fs::FileType::Directory {
            walk(&from, &to, o, t, inodes);
            continue;
        }

        let key = (st.st_dev as u64, st.st_ino as u64);
        if st.st_nlink > 1 {
            if let Some(existing) = inodes.get(&key) {
                match fs::linkat(fs::CWD, existing, fs::CWD, &to, AtFlags::empty()) {
                    Ok(()) => {
                        t.relinked += 1;
                        continue;
                    }
                    // A destination filesystem without hardlinks is a documented
                    // degradation, not a failure: the copy falls through and
                    // duplicates the data, which is the only thing it can do.
                    Err(_) => {}
                }
            }
        }

        match copy::copy_file(&from, &to, o) {
            Ok(c) => {
                t.files += 1;
                *t.strategies.entry(strategy_name(c.strategy)).or_insert(0) += 1;
                *t.placements.entry(placement_name(c.placement)).or_insert(0) += 1;
                if let Some(o) = c.orphan {
                    t.orphans.push(o);
                }
                for what in c.unpreserved {
                    t.unpreserved.push((from.clone(), what));
                }
                if st.st_nlink > 1 {
                    inodes.insert(key, to.clone());
                }
            }
            Err(e) => t.failures.push((from.clone(), e)),
        }
    }
}

/// A symlink is copied as a symlink with a byte-identical target. The target is
/// *bytes*, not a path: it may not be valid UTF-8, and it may point nowhere,
/// and neither is this function's business.
pub fn copy_symlink(src: &Path, dst: &Path) -> Result<(), rustix::io::Errno> {
    let target = fs::readlinkat(fs::CWD, src, Vec::new())?;
    match fs::symlinkat(&target, fs::CWD, dst) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::EXIST) => {
            fs::unlinkat(fs::CWD, dst, AtFlags::empty())?;
            fs::symlinkat(&target, fs::CWD, dst)
        }
        Err(e) => Err(e),
    }
}

/// Best-effort recursive removal that does not follow symlinks out of the tree.
pub fn remove_tree(path: &Path) {
    let Ok(st) = fs::statat(fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW) else { return };
    if fs::FileType::from_raw_mode(st.st_mode) == fs::FileType::Directory {
        if let Ok(entries) = std::fs::read_dir(path) {
            for e in entries.flatten() {
                remove_tree(&e.path());
            }
        }
        // A directory the fixture made read-only cannot be emptied until it is
        // writable again.
        let _ = fs::chmodat(fs::CWD, path, Mode::from_bits_truncate(0o755), AtFlags::empty());
        let _ = fs::unlinkat(fs::CWD, path, AtFlags::REMOVEDIR);
    } else {
        let _ = fs::chmodat(fs::CWD, path, Mode::from_bits_truncate(0o644), AtFlags::empty());
        let _ = fs::unlinkat(fs::CWD, path, AtFlags::empty());
    }
    let _ = OFlags::empty();
}
