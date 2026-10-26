// SPDX-License-Identifier: Apache-2.0
//! Writing a small file whole, so that a reader sees all of it or none of it.
//!
//! Three files in this program are a person's own state — the session, the
//! per-folder view records, and the pinned places —
//! and [rule 6](../../../docs/03-principles.md#6-state-survives-everything) says
//! all three are "written durably enough to survive a hard power cut". That is
//! four steps rather than one write, and it was written out twice before this
//! module existed, with the second copy missing the `fsync`.
//!
//! **Runs on a job.** Rule 1 has no exemption for a small file.

use std::path::Path;

/// Publishes `text` to `file` atomically and synchronizes the file and directory.
///
/// Four steps, and each one is a different way the file could have been lost:
///
/// * a neighbouring name, so a failure part way through damages nothing;
/// * `fsync` on the handle, so the bytes are on the device rather than in the
///   page cache;
/// * `rename`, which POSIX makes atomic within a directory — a reader sees the
///   whole old file or the whole new one and never a torn one;
/// * `fsync` on the *directory*, because the rename itself is a change to the
///   directory and is not on the device until the directory is synced. Without
///   it a power cut can leave the new bytes safely written under a name nothing
///   points at, which is the failure that looks like the write never happened.
///
/// Returns whether publication and synchronization both succeeded. A directory
/// sync error can follow a successful publication.
#[must_use]
pub fn atomically(file: &Path, text: &str) -> bool {
    let Some(directory) = file.parent() else { return false };
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    write(file, text).is_ok()
}

/// Publishes text using an exclusively owned temporary file.
///
/// Existing regular files retain ownership, permissions, and extended attributes.
/// Symlinks and hardlinks are refused because replacement would change their meaning.
/// An error after publication can reflect directory sync; it does not imply no change.
pub fn write(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = file.parent().ok_or_else(|| std::io::Error::other("no parent"))?;
    let parent = std::fs::File::open(directory)?;
    let original = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)
    {
        Ok(handle) => {
            let meta = handle.metadata()?;
            if !meta.is_file() || meta.nlink() != 1 {
                return Err(std::io::Error::other("editing requires a regular file with one link"));
            }
            Some((handle, meta))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (staging, mut handle) = loop {
        let name = directory.join(format!(
            ".skerry-save-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&name) {
            Ok(handle) => break (name, handle),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        handle.write_all(text.as_bytes())?;
        if let Some((source, meta)) = &original {
            use std::os::fd::AsRawFd as _;
            // SAFETY: the descriptor remains owned by `handle` throughout the call.
            if unsafe { libc::fchown(handle.as_raw_fd(), meta.uid(), meta.gid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            handle.set_permissions(meta.permissions())?;
            let mut missing = Vec::new();
            // ACLs included: this is the same file rewritten, and the `fchown`
            // above has already refused unless its owner can own the new one.
            crate::copy::copy_xattrs(source, &handle, &mut missing, true);
            if !missing.is_empty() {
                return Err(std::io::Error::other(format!(
                    "metadata could not be preserved: {}",
                    missing.join(", ")
                )));
            }
            let current = std::fs::symlink_metadata(file)?;
            if current.dev() != meta.dev()
                || current.ino() != meta.ino()
                || current.ctime() != meta.ctime()
                || current.ctime_nsec() != meta.ctime_nsec()
                || current.mtime() != meta.mtime()
                || current.mtime_nsec() != meta.mtime_nsec()
                || current.len() != meta.len()
            {
                return Err(std::io::Error::other("file changed while saving"));
            }
        }
        handle.sync_all()?;
        if original.is_some() {
            std::fs::rename(&staging, file)?;
        } else {
            rustix::fs::renameat_with(
                rustix::fs::CWD,
                &staging,
                rustix::fs::CWD,
                file,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
        }
        parent.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::PathBuf;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-save-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    /// The directory is made if it is not there, which is the first run on a
    /// fresh account.
    #[test]
    fn a_write_into_a_directory_that_does_not_exist_makes_it() {
        let root = scratch("fresh");
        let file = root.join("deeper").join("state.txt");
        assert!(super::atomically(&file, "one\n"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A second write replaces the first and leaves no staging file behind — the
    /// name the rename came from must not be a file somebody finds later.
    #[test]
    fn a_second_write_replaces_the_first_and_leaves_nothing_beside_it() {
        let root = scratch("twice");
        let file = root.join("state.txt");
        assert!(super::atomically(&file, "one\n"));
        assert!(super::atomically(&file, "two\n"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\n");
        // The name the rename came from must not be a file somebody finds later.
        assert!(!root.join("state.new").exists(), "the staging file was left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A write that cannot happen says so rather than reporting success: the
    /// callers all decide what to tell the person from this `bool`.
    #[test]
    fn a_write_that_cannot_happen_is_reported_and_not_swallowed() {
        let root = scratch("refused");
        std::fs::create_dir_all(&root).unwrap();
        let blocked = root.join("occupied");
        std::fs::write(&blocked, b"a file, not a directory").unwrap();
        assert!(!super::atomically(&blocked.join("state.txt"), "one\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_preserves_neighbours_metadata_and_refuses_links() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = scratch("ownership");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("notes.txt");
        let victim = root.join("victim");
        std::fs::write(&file, "old").unwrap();
        std::fs::write(&victim, "unrelated").unwrap();
        symlink(&victim, root.join("notes.new")).unwrap();
        for mode in [0o600, 0o751] {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            super::write(&file, "new").unwrap();
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, mode);
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unrelated");
            assert!(
                std::fs::symlink_metadata(root.join("notes.new")).unwrap().file_type().is_symlink()
            );
        }
        let linked = root.join("linked");
        symlink(&file, &linked).unwrap();
        assert!(super::write(&linked, "bad").is_err());
        std::fs::hard_link(&file, root.join("hard")).unwrap();
        assert!(super::write(&file, "bad").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        std::fs::remove_dir_all(root).unwrap();
    }
}
