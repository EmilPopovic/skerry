// SPDX-License-Identifier: Apache-2.0
//! Where the recent list is kept, and the jobs that read and change it.
//!
//! What the list *is* — the shared file's grammar, the URI spelling, the merge —
//! is [`sk_desktop::recent`], and
//! [ADR 0043](../../../docs/adr/0043-recents-are-two-stores-read-as-one-list-and-ours-is-the-one-rule-6-counts.md)
//! is why there are two stores. This is the part with I/O in it, and every
//! function here runs on a job.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sk_desktop::recent::{self, Shared, Store};

/// One file on the list, with what a listing shows about it.
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) size: u64,
    /// Seconds since 1970, which is what a listing's modified column holds.
    pub(crate) modified: i64,
}

/// Where our list lives: beside the session, in `$XDG_STATE_HOME/skerry`.
fn ours() -> Option<PathBuf> {
    crate::session::path().map(|session| session.with_file_name("recents"))
}

/// Where the desktop's list lives.
///
/// `$XDG_DATA_HOME`, or `~/.local/share` where it is unset — and never the first
/// of `$XDG_DATA_DIRS`, which with neither variable set is `/usr/local/share`.
fn shared() -> Option<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
    })?;
    Some(home.join("recently-used.xbel"))
}

/// The desktop's list, or `None` where it could not be read.
///
/// **`None` is not an empty list.** An empty list is one whose tombstones can be
/// pruned; a list that did not read might still hold every entry they hide, and
/// pruning against it would bring back each one the next time it read.
fn read_shared() -> Option<Vec<Shared>> {
    let path = shared()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    match recent::read_shared(&text) {
        Ok(entries) => Some(entries),
        Err(why) => {
            eprintln!("skerry: recents: {} was not read: {}", path.display(), why.0);
            None
        }
    }
}

fn read_ours(path: &Path) -> Store {
    Store::parse(&std::fs::read_to_string(path).unwrap_or_default())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX))
}

/// Both lists as one, newest first, keeping only what is still a file.
///
/// Two reads and a `statx` per entry, any of which can be on a mount that has
/// stopped answering — which is why this is a job and the list is its answer.
pub(crate) fn load() -> Vec<Found> {
    let store = ours().map(|path| read_ours(&path)).unwrap_or_default();
    let shared = read_shared().unwrap_or_default();
    let (list, _) = recent::merge(&store, &shared);
    list.into_iter()
        .filter_map(|row| {
            let meta = std::fs::symlink_metadata(&row.path).ok()?;
            (!meta.is_dir()).then(|| Found {
                size: meta.len(),
                modified: meta.mtime(),
                path: row.path,
            })
        })
        .collect()
}

/// Changes our list and writes it back.
///
/// **On `Lane::Control`, and only there**, so two changes cannot interleave a
/// read and a write. That is the lost update `GLib` has between applications, and
/// it is not one to have between two jobs of this one. The shared file is read
/// for pruning alone: a tombstone it spent, or one for an entry it no longer
/// holds, has nothing left to hide.
fn change(apply: impl FnOnce(&mut Store, i64)) {
    let Some(path) = ours() else { return };
    let mut store = read_ours(&path);
    apply(&mut store, now());
    if let Some(shared) = read_shared() {
        let (_, spent) = recent::merge(&store, &shared);
        store.prune(&shared, &spent);
    }
    let _ = sk_fs::atomically(&path, &store.render());
}

/// Records that `path` was opened.
pub(crate) fn opened(path: &Path) {
    change(|store, now| store.opened(recent::uri_of(path), now));
}

/// Forgets each of `paths` from now on.
pub(crate) fn forget(paths: &[PathBuf]) {
    change(|store, now| {
        for path in paths {
            store.forget(&recent::uri_of(path), now);
        }
    });
}
