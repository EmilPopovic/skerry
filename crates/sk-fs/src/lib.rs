// SPDX-License-Identifier: Apache-2.0
//! `getdents64` enumeration, batched `statx`, and the device probe that decides
//! which pool lane a directory reads on.
//!
//! Does not own: deciding *what* to enumerate, or why.
//!
//! Everything here runs on a job and takes a cancellation token. Nothing here is
//! callable from the UI thread, and that is not a convention — it is rule 1, and
//! [Spike B](../../../spikes/b-enumeration/README.md) measured what breaking it
//! costs: the same call that takes 4 ms on the pool took **28,000 ms** on the
//! frame loop with one unresponsive mount.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod copy;
pub mod device;
pub mod facts;
pub mod mounts;
pub mod ops;
pub mod save;
pub mod scan;
pub mod text;
pub mod trash;
pub mod volumes;
pub mod watch;

pub use copy::{Copied, Placement, Strategy, copy_file, move_file};
pub use device::{Device, lane_for};
pub use mounts::{Mount, mounts};
pub use save::atomically;
pub use scan::{META_CHUNK, MetaRow, Names, metadata, names};
pub use text::{TEXT_LIMIT, read_text};
pub use volumes::{Volume, volumes};
pub use watch::{Change, Watcher, can_watch};

/// Open a private lock file without following its final symlink and try to own it.
///
/// The returned file keeps the advisory lock held until dropped. Runs before UI startup.
pub fn lock_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    Ok(held)
}

/// Open a private append-only diagnostic file without following its final symlink.
///
/// Runs before the frame loop starts; old diagnostics are retained.
pub fn diagnostic_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "diagnostic file must be a private regular file owned by this user",
        ));
    }
    Ok(file)
}
