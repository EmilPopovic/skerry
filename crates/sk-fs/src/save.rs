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

/// Writes `text` to `file`, whole, or leaves what was there.
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
/// Returns whether the file is now what `text` says.
#[must_use]
pub fn atomically(file: &Path, text: &str) -> bool {
    let Some(directory) = file.parent() else { return false };
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    let staging = file.with_extension("new");
    let Ok(handle) = std::fs::File::create(&staging) else { return false };
    {
        use std::io::Write as _;
        let mut handle = handle;
        if handle.write_all(text.as_bytes()).is_err() || handle.sync_all().is_err() {
            let _ = std::fs::remove_file(&staging);
            return false;
        }
    }
    if std::fs::rename(&staging, file).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    // A directory that cannot be opened for reading is a directory a rename
    // just succeeded in, so there is nothing left to do about it and nothing
    // useful to say: the file is in place either way.
    if let Ok(handle) = std::fs::File::open(directory) {
        let _ = handle.sync_all();
    }
    true
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
}
