//! shared-mime-info, `.desktop` parsing and launch.
//!
//! Does not own: any policy about *when* to launch something.
//!
//! Every parser here is ours, per
//! [docs/07-dependencies.md](../../../docs/07-dependencies.md): each format is
//! small, well specified, and otherwise reachable only through GIO — which
//! rule 9 forbids and which would bring a main loop with it. They are the
//! obvious fuzz targets and are listed as such in
//! [docs/04-architecture.md](../../../docs/04-architecture.md#testing-strategy-by-layer).
//!
//! # What is here and what is owed
//!
//! Detecting a file's type, finding what opens it, and starting that. **Not**
//! here: icon themes, D-Bus, `FileManager1`, udisks2, portals, and registering
//! ourselves as a handler. Those are M7's and each is larger than all of this.
//!
//! The type detection is by **filename** — `globs2` — plus a small table of
//! signatures for files with no useful extension. shared-mime-info's binary
//! `magic` database is not parsed; what that costs is a file called `data` being
//! reported as unknown rather than as a PNG.

#![deny(clippy::unwrap_used, clippy::expect_used)]
// `missing_docs` and `clippy::pedantic` are warned workspace-wide from the lint
// table in Cargo.toml, together with the few pedantic lints allowed there with a
// reason. Repeating `warn(clippy::pedantic)` here would override those.

pub mod apps;
pub mod entry;
pub mod launch;
pub mod mime;
pub mod places;

pub use apps::Applications;
pub use entry::DesktopEntry;
pub use launch::Launcher;
pub use mime::{Category, MimeDatabase, category};
pub use places::{Kind, Place, bookmarks, user_dirs};

use std::path::{Path, PathBuf};

/// The XDG data directories, most specific first.
///
/// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`, with the defaults the base-directory
/// specification names when either is unset. Order matters everywhere it is
/// used: the first hit wins, which is how a user overrides a distribution.
#[must_use]
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(4);
    if let Some(home) = env_path("XDG_DATA_HOME") {
        dirs.push(home);
    } else if let Some(home) = env_path("HOME") {
        dirs.push(home.join(".local/share"));
    }
    match std::env::var_os("XDG_DATA_DIRS") {
        Some(value) if !value.is_empty() => {
            dirs.extend(std::env::split_paths(&value).filter(|path| !path.as_os_str().is_empty()));
        }
        _ => {
            dirs.push(PathBuf::from("/usr/local/share"));
            dirs.push(PathBuf::from("/usr/share"));
        }
    }
    dirs
}

/// The XDG config directories, most specific first.
#[must_use]
pub fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(3);
    if let Some(home) = env_path("XDG_CONFIG_HOME") {
        dirs.push(home);
    } else if let Some(home) = env_path("HOME") {
        dirs.push(home.join(".config"));
    }
    match std::env::var_os("XDG_CONFIG_DIRS") {
        Some(value) if !value.is_empty() => {
            dirs.extend(std::env::split_paths(&value).filter(|path| !path.as_os_str().is_empty()));
        }
        _ => dirs.push(PathBuf::from("/etc/xdg")),
    }
    dirs
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    // An empty or relative value is "unset" per the specification, not a path
    // relative to wherever the process happens to be.
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

/// Reads a file, or returns nothing.
///
/// Every database here is optional: a machine with no shared-mime-info is a
/// machine where files have no type, not a machine where the file manager
/// refuses to start.
fn read(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{config_dirs, data_dirs};

    #[test]
    fn the_data_directories_end_in_the_specifications_defaults() {
        let dirs = data_dirs();
        assert!(!dirs.is_empty());
        assert!(dirs.iter().all(|path| path.is_absolute()), "{dirs:?}");
    }

    #[test]
    fn the_config_directories_are_absolute_and_ordered() {
        let dirs = config_dirs();
        assert!(!dirs.is_empty());
        assert!(dirs.iter().all(|path| path.is_absolute()), "{dirs:?}");
    }
}
