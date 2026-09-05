// SPDX-License-Identifier: Apache-2.0
//! The persistent filename index: one file per root, rebuilt whole.
//!
//! # Why this exists
//!
//! [ADR 0008](../../../docs/adr/0008-persistent-index-is-required.md) made a
//! persistent index mandatory rather than optional, because the gap between a
//! walk and "milliseconds" is three orders of magnitude and is a missing kernel
//! primitive rather than slow code. [ADR
//! 0012](../../../docs/adr/0012-a-flat-index-file-per-root-rebuilt-whole.md)
//! settles the format, the update mechanism and the freshness policy that ADR
//! 0008 deliberately left open.
//!
//! # The shape of the file
//!
//! A fixed header, then the root's path, then every name concatenated into one
//! arena, then six fixed-width columns. [Spike
//! F](../../../spikes/f-index-and-search/README.md) measured this exact shape at
//! **195 ms to load a million entries cold on a Raspberry Pi 5**, against a cold
//! walk of 3.7 s on the same machine — 19×, from the dullest thing that can
//! work. No mmap, no compression, no ordering: each is complexity spent against
//! a number already an order of magnitude ahead.
//!
//! # It is never the only answer
//!
//! Every failure here returns `None` and the caller walks instead. Absent,
//! truncated, wrong magic, wrong version, failing its checksum, built for
//! another root — all of them are a performance event and none of them is
//! something the user is told about. That is ADR 0008's third constraint and the
//! one this module could most easily break.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustix::fs::{Dir, FileType, Mode, OFlags, StatxFlags};
use sk_util::Cancel;

use crate::find::{Found, Hit, MOST};

/// What every index file starts with.
const MAGIC: [u8; 4] = *b"SKIX";

/// The format this build writes and the only one it reads.
///
/// A mismatch is a silent rebuild rather than a migration, which is only a
/// defensible trade while the file is a cache. The moment anything lives here
/// that the user cannot regenerate, it stops being one.
const VERSION: u32 = 1;

/// Bytes before the body.
const HEADER: usize = 56;

/// Bytes of column per entry: parent, name offset, name length, flags, size,
/// modified.
const PER_ENTRY: usize = 4 + 4 + 2 + 2 + 8 + 8;

/// The parent of an entry that sits directly in the root.
const NO_PARENT: u32 = u32::MAX;

/// A directory, in this file's own flag bits.
///
/// **Deliberately not [`sk_model::Flags`]'s layout.** That type is free to
/// renumber itself for the view's convenience; this one is on disk, where a
/// renumber is a silent misreading of every index a user already has.
const IS_DIR: u16 = 1 << 0;
/// A symbolic link, whatever it points at.
const IS_LINK: u16 = 1 << 1;

/// A filename index for one tree.
///
/// Held as the bytes that were written, because that is also the layout the scan
/// wants: the columns are already contiguous, so loading is a read and a bounds
/// check rather than a parse.
pub struct Index {
    root: PathBuf,
    /// Seconds since the epoch, when the build finished.
    built: i64,
    entries: usize,
    arena_at: usize,
    columns_at: usize,
    bytes: Vec<u8>,
}

/// Where one column starts, `before` bytes per entry into the columns.
const fn column(entries: usize, before: usize) -> usize {
    before * entries
}

impl Index {
    /// How many entries it holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries
    }

    /// Whether it holds nothing at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// When the build finished, in seconds since the epoch.
    #[must_use]
    pub const fn built(&self) -> i64 {
        self.built
    }

    /// The tree it covers.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Bytes it occupies on disk.
    #[must_use]
    pub const fn stored(&self) -> usize {
        HEADER + self.bytes.len()
    }

    /// The name of one entry.
    ///
    /// Written from a `&str`, so the validation cannot fail on a file this build
    /// wrote. Answering an unreadable name with an empty one costs a row rather
    /// than the whole search, which is this module's rule everywhere.
    fn name_of(&self, entry: usize) -> &str {
        let at = self.u32_at(self.columns_at + column(self.entries, 4) + entry * 4) as usize;
        let len = self.u16_at(self.columns_at + column(self.entries, 8) + entry * 2) as usize;
        let from = self.arena_at + at;
        self.bytes
            .get(from..from.saturating_add(len))
            .and_then(|it| std::str::from_utf8(it).ok())
            .unwrap_or("")
    }

    fn parent_of(&self, entry: usize) -> u32 {
        self.u32_at(self.columns_at + column(self.entries, 0) + entry * 4)
    }

    fn u16_at(&self, at: usize) -> u16 {
        self.bytes.get(at..at + 2).map_or(0, |it| u16::from_le_bytes([it[0], it[1]]))
    }

    fn u32_at(&self, at: usize) -> u32 {
        self.bytes.get(at..at + 4).map_or(0, |it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]))
    }

    /// The full path of an entry, walked up the parent chain.
    ///
    /// No path is stored twice — a directory's name is held once however many
    /// files are under it, which is most of why the file is fifty bytes an entry
    /// rather than a few hundred.
    #[must_use]
    pub fn path_of(&self, entry: usize) -> PathBuf {
        let mut parts: Vec<&str> = Vec::new();
        let mut at = entry;
        // Bounded by the entry count, so a corrupt parent chain that points at
        // itself is a wrong path rather than a hang.
        for _ in 0..=self.entries {
            parts.push(self.name_of(at));
            let parent = self.parent_of(at);
            if parent == NO_PARENT {
                break;
            }
            let next = parent as usize;
            if next >= self.entries {
                break;
            }
            at = next;
        }
        let mut path = self.root.clone();
        for part in parts.iter().rev() {
            path.push(part);
        }
        path
    }
}

/// The columns as they are collected, before they become a file.
///
/// A struct rather than nine locals threaded through two functions: the
/// alternative needed a ten-argument constructor and an exemption from the lint
/// that says so.
#[derive(Default)]
struct Rows {
    names: Vec<u8>,
    parent: Vec<u32>,
    name_at: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u16>,
    size: Vec<u64>,
    modified: Vec<i64>,
}

impl Rows {
    fn len(&self) -> usize {
        self.parent.len()
    }

    /// Adds one entry. Whether there was room for it.
    fn push(&mut self, name: &str, owner: u32, flags: u16, size: u64, modified: i64) -> bool {
        let (Ok(at), Ok(len)) = (u32::try_from(self.names.len()), u16::try_from(name.len())) else {
            return false;
        };
        self.names.extend_from_slice(name.as_bytes());
        self.parent.push(owner);
        self.name_at.push(at);
        self.name_len.push(len);
        self.flags.push(flags);
        self.size.push(size);
        self.modified.push(modified);
        true
    }

    /// Lays the columns out as the file holds them.
    fn finish(self, root: PathBuf, built: i64) -> Index {
        let entries = self.parent.len();
        let root_bytes = root.as_os_str().as_encoded_bytes().to_vec();
        let mut bytes =
            Vec::with_capacity(root_bytes.len() + self.names.len() + entries * PER_ENTRY);
        bytes.extend_from_slice(&root_bytes);
        let arena_at = bytes.len();
        bytes.extend_from_slice(&self.names);
        let columns_at = bytes.len();
        for it in &self.parent {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        for it in &self.name_at {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        for it in &self.name_len {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        for it in &self.flags {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        for it in &self.size {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        for it in &self.modified {
            bytes.extend_from_slice(&it.to_le_bytes());
        }
        Index { root, built, entries, arena_at, columns_at, bytes }
    }
}

/// Where an index for `root` is kept.
///
/// `$XDG_CACHE_HOME/skerry/index/`, one file per root named by a hash of its
/// path. A **cache**: the basedir specification's line is whether losing the file
/// loses anything the user cannot regenerate, and this one is a slower copy of
/// the filesystem. Deleting the directory at any moment costs a walk and nothing
/// else.
#[must_use]
pub fn path_for(root: &Path) -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|it| it.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    let key = hash(root.as_os_str().as_encoded_bytes());
    Some(cache.join("skerry").join("index").join(format!("{key:016x}.skix")))
}

/// FNV-1a, 64-bit.
///
/// For detecting a truncated or corrupted file and for naming one, which is all
/// it is for. An index is a cache of names the user can already read, so it is
/// not a security boundary and does not want a cryptographic hash.
fn hash(bytes: &[u8]) -> u64 {
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        sum ^= u64::from(*byte);
        sum = sum.wrapping_mul(0x1000_0000_01b3);
    }
    sum
}

/// Everything found under `root`, ready to be written.
///
/// Runs on a job. `cancel` is checked per directory, and `report` is called with
/// the count so far often enough to fill a progress line and rarely enough that a
/// million files is not a million messages.
///
/// **One filesystem, and symlinks are not walked into** — the two rules
/// [`crate::find`] and [`crate::size`] follow, for the same two reasons: a walk
/// that crosses a mount takes an idle network mount's timeout, and one that
/// follows links finds a cycle.
#[must_use]
pub fn build(root: &Path, cancel: &Cancel, report: &mut impl FnMut(usize)) -> Option<Index> {
    let root = root.to_path_buf();
    let device = device_of(&root)?;
    let mut rows = Rows::default();

    // An explicit queue rather than recursion: a directory tree is
    // attacker-controlled depth, and a deep one must not be a stack overflow.
    // Each item is a directory to read and the entry that names it, which is what
    // lets a child record its parent without storing a path.
    let mut pending: Vec<(PathBuf, u32)> = vec![(root.clone(), NO_PARENT)];
    let mut next_report = 0usize;

    while let Some((directory, owner)) = pending.pop() {
        if cancel.is_cancelled() {
            return None;
        }
        read_into(&directory, owner, device, &mut rows, &mut pending);
        if rows.len() >= next_report {
            next_report = rows.len() + 4096;
            report(rows.len());
        }
    }

    report(rows.len());
    Some(rows.finish(root, now()))
}

/// Reads one directory into `rows`, queueing the subdirectories it holds.
fn read_into(
    directory: &Path,
    owner: u32,
    device: u64,
    rows: &mut Rows,
    pending: &mut Vec<(PathBuf, u32)>,
) {
    let open = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let Ok(fd) = rustix::fs::open(directory, open, Mode::empty()) else { return };
    let Ok(dir) = Dir::read_from(&fd) else { return };
    let wanted = StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::MTIME;

    for entry in dir.flatten() {
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        let Ok(here) = u32::try_from(rows.len()) else { return };
        let name = String::from_utf8_lossy(raw);

        // A symlink is recorded by its own name and never stated through: its
        // target is somewhere else in the tree or outside it altogether.
        let link = entry.file_type() == FileType::Symlink;
        let stat = if link {
            None
        } else {
            let at = rustix::fs::AtFlags::SYMLINK_NOFOLLOW;
            rustix::fs::statx(&fd, name.as_ref(), at, wanted).ok()
        };
        let is_dir =
            stat.as_ref().is_some_and(|it| u32::from(it.stx_mode) & 0o170_000 == 0o040_000);
        let bits = if is_dir { IS_DIR } else { 0 } | if link { IS_LINK } else { 0 };
        let size = stat.as_ref().map_or(0, |it| it.stx_size);
        let modified = stat.as_ref().map_or(0, |it| it.stx_mtime.tv_sec);
        if !rows.push(&name, owner, bits, size, modified) {
            return;
        }

        if let Some(stat) = stat.as_ref().filter(|_| is_dir) {
            // `du -x`: a mount point inside the tree is a different filesystem
            // and a different question.
            let on = u64::from(stat.stx_dev_major) << 32 | u64::from(stat.stx_dev_minor);
            if on == device {
                pending.push((directory.join(name.as_ref()), here));
            }
        }
    }
}

/// How often a count says how far it has got.
///
/// The same 64 ms [`crate::find`] and [`crate::size`] use, and for the same
/// reason: often enough that a bar visibly moves, rarely enough that a tree of a
/// million entries does not send a million messages.
const COUNT_EVERY: std::time::Duration = std::time::Duration::from_millis(64);

/// How many entries [`build`] would store under `root`.
///
/// # Why this exists
///
/// A progress bar needs a denominator, and a walk does not know one until it has
/// finished. This is the same answer the copy engine already gives: a counting
/// pass **beside** the operation rather than before it, whose total the bar waits
/// for. Nothing is estimated from a partial count — a bar that reaches 40% and
/// starts over because the walk found more is worse than no bar at all.
///
/// # Why it is not [`crate::size`]
///
/// That walk answers a different question — how many bytes and files are under
/// here — and it disagrees with this one in four places: it never counts a
/// symlink, never counts a directory on another filesystem, drops an entry whose
/// `statx` failed, and counts the root itself. `build` stores all four. Borrowing
/// its total would put the bar a few percent out on any tree with links in it,
/// which is every tree.
///
/// # Why it is cheap enough to be worth doing twice
///
/// It reads names and does **not** `statx` each entry, which
/// [Spike B](../../../spikes/b-enumeration/README.md) and [Spike
/// F](../../../spikes/f-index-and-search/README.md) both measure as the
/// expensive half — deferring it is 2× on the median and 10× on the p99. The
/// kind comes from `getdents64`'s own `d_type`, and `statx` is asked only where
/// the filesystem answered `Unknown`. One `statx` per *directory* settles the
/// one-filesystem rule, rather than one per entry.
pub fn count(root: &Path, cancel: &Cancel, report: &mut impl FnMut(usize, bool)) {
    let Some(device) = device_of(root) else {
        report(0, true);
        return;
    };
    let mut seen = 0usize;
    let mut pending = vec![root.to_path_buf()];
    let mut next_report = std::time::Instant::now() + COUNT_EVERY;
    while let Some(directory) = pending.pop() {
        if cancel.is_cancelled() {
            return;
        }
        count_one(&directory, device, &mut seen, &mut pending);
        if std::time::Instant::now() >= next_report {
            next_report = std::time::Instant::now() + COUNT_EVERY;
            report(seen, false);
        }
    }
    report(seen, true);
}

/// Counts one directory's entries, queueing the subdirectories under it.
fn count_one(directory: &Path, device: u64, seen: &mut usize, pending: &mut Vec<PathBuf>) {
    let open = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let Ok(fd) = rustix::fs::open(directory, open, Mode::empty()) else { return };
    // **The `du -x` rule, once per directory instead of once per entry.** `build`
    // reaches the same decision from the `statx` it is already doing per entry;
    // this one is not doing those, so it asks here — a mount point is counted as
    // an entry by whoever listed it and nothing inside it is counted by either.
    let on = rustix::fs::statx(&fd, "", rustix::fs::AtFlags::EMPTY_PATH, StatxFlags::TYPE)
        .map(|it| u64::from(it.stx_dev_major) << 32 | u64::from(it.stx_dev_minor));
    if on.is_ok_and(|it| it != device) {
        return;
    }
    let Ok(dir) = Dir::read_from(&fd) else { return };

    for entry in dir.flatten() {
        let raw = entry.file_name().to_bytes();
        if raw == b"." || raw == b".." {
            continue;
        }
        // Every entry, because `build` stores every entry — a link, a socket and
        // a directory it cannot read are all one row in the file.
        *seen += 1;
        let kind = entry.file_type();
        if kind == FileType::Symlink {
            continue;
        }
        let name = String::from_utf8_lossy(raw);
        let directory_here = match kind {
            FileType::Directory => true,
            // Some filesystems do not fill in `d_type`. Only those pay a `statx`.
            FileType::Unknown => {
                let at = rustix::fs::AtFlags::SYMLINK_NOFOLLOW;
                rustix::fs::statx(&fd, name.as_ref(), at, StatxFlags::TYPE)
                    .is_ok_and(|it| u32::from(it.stx_mode) & 0o170_000 == 0o040_000)
            }
            _ => false,
        };
        if directory_here {
            pending.push(directory.join(name.as_ref()));
        }
    }
}

/// Writes the index where [`path_for`] says, whole and atomically.
///
/// Written to a neighbouring name and renamed, which is atomic within a
/// directory, so a reader sees the whole old file or the whole new one and never
/// a torn one. **Not `fsync`ed**, which is where this parts company with [ADR
/// 0011](../../../docs/adr/0011-one-plain-text-state-file-written-atomically.md):
/// a device flush is the expensive half of durability, and losing this file costs
/// a walk.
#[must_use]
pub fn save(index: &Index) -> bool {
    path_for(&index.root).is_some_and(|file| save_to(index, &file))
}

/// As [`save`], to a named file.
///
/// The seam a test uses. `$XDG_CACHE_HOME` is process-global, and a test that
/// moved it would be a test that moves it for every other test running beside it.
#[must_use]
pub fn save_to(index: &Index, file: &Path) -> bool {
    let Some(directory) = file.parent() else { return false };
    if std::fs::create_dir_all(directory).is_err() {
        return false;
    }
    let arena_len = index.columns_at - index.arena_at;
    let mut out = Vec::with_capacity(HEADER + index.bytes.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(index.entries as u64).to_le_bytes());
    out.extend_from_slice(&(arena_len as u64).to_le_bytes());
    out.extend_from_slice(&index.built.to_le_bytes());
    out.extend_from_slice(&device_of(&index.root).unwrap_or_default().to_le_bytes());
    out.extend_from_slice(&(index.arena_at as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&hash(&index.bytes).to_le_bytes());
    out.extend_from_slice(&index.bytes);

    let staging = file.with_extension("new");
    if std::fs::write(&staging, &out).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    if std::fs::rename(&staging, file).is_err() {
        let _ = std::fs::remove_file(&staging);
        return false;
    }
    true
}

/// Reads the index for `root`, or nothing if there is not a usable one.
///
/// Every rejection is a walk, not an error. The checks run in order of cost —
/// length, magic, version, the arithmetic — and only then the checksum, which is
/// the one that reads all fifty megabytes.
#[must_use]
pub fn load(root: &Path) -> Option<Index> {
    load_from(&path_for(root)?, root)
}

/// As [`load`], from a named file.
#[must_use]
pub fn load_from(file: &Path, root: &Path) -> Option<Index> {
    let raw = std::fs::read(file).ok()?;
    if raw.len() < HEADER || raw.get(..4)? != MAGIC {
        return None;
    }
    let word =
        |at: usize| -> u32 { u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) };
    let long = |at: usize| -> [u8; 8] {
        [
            raw[at],
            raw[at + 1],
            raw[at + 2],
            raw[at + 3],
            raw[at + 4],
            raw[at + 5],
            raw[at + 6],
            raw[at + 7],
        ]
    };
    if word(4) != VERSION {
        return None;
    }
    let entries = usize::try_from(u64::from_le_bytes(long(8))).ok()?;
    let arena_len = usize::try_from(u64::from_le_bytes(long(16))).ok()?;
    let built = i64::from_le_bytes(long(24));
    let device = u64::from_le_bytes(long(32));
    let root_len = word(40) as usize;
    let checksum = u64::from_le_bytes(long(48));

    let body = raw.get(HEADER..)?;
    // The arithmetic check that makes every offset below safe: if the three
    // lengths do not add up to the body's own length, nothing in it is trusted.
    let wanted = root_len.checked_add(arena_len)?.checked_add(entries.checked_mul(PER_ENTRY)?)?;
    if body.len() != wanted || hash(body) != checksum {
        return None;
    }
    // Only UTF-8, because these bytes came off a disk somebody else can write and
    // the unchecked conversion is unsafe for exactly that reason. A root whose
    // name is not UTF-8 costs its owner the index and nothing else.
    let stored = PathBuf::from(std::str::from_utf8(body.get(..root_len)?).ok()?);
    if stored != root {
        return None;
    }
    // A remount can put the same path on a different device, and every size and
    // mtime in the file is then somebody else's.
    if device_of(root) != Some(device) {
        return None;
    }
    Some(Index {
        root: stored,
        built,
        entries,
        arena_at: root_len,
        columns_at: root_len + arena_len,
        bytes: body.to_vec(),
    })
}

/// Deletes the index for `root`. Whether there was one to delete.
#[must_use]
pub fn forget(root: &Path) -> bool {
    path_for(root).is_some_and(|file| std::fs::remove_file(file).is_ok())
}

/// Everything in the index whose name contains `needle`.
///
/// The scan is **parallel by specification**, not as an optimisation: [Spike
/// F](../../../spikes/f-index-and-search/README.md) measured a serial scan of a
/// million entries at 47.4 ms worst case on a Raspberry Pi 5 against a 50 ms bar
/// — "inside the bar by 5%, which is not inside it" — and 72.3 ms per keystroke
/// as-you-type, outside it. Four cores buy 3.4–3.7×, well under the core count,
/// because the scan is bound by the memory it moves rather than by arithmetic.
#[must_use]
pub fn search(index: &Arc<Index>, needle: &str, pool: &sk_util::Pool, cancel: &Cancel) -> Found {
    let mut found = Found::default();
    search_into(index, needle, pool, cancel, &mut found);
    // A cancelled scan is a question nobody is asking any more, and the one
    // thing it must not do is claim to have answered it.
    found.complete = !cancel.is_cancelled();
    found
}

/// As [`search`], adding to a list that already holds something.
///
/// **`complete` is the caller's to set**, which is the whole reason this is
/// separate: a scope of several roots answers some of them from an index and
/// walks the rest
/// ([ADR 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)),
/// and a scan that declared the answer complete would make the panel say "done"
/// while a walk of the other half was still running.
pub fn search_into(
    index: &Arc<Index>,
    needle: &str,
    pool: &sk_util::Pool,
    cancel: &Cancel,
    found: &mut Found,
) {
    if needle.is_empty() {
        // Not "everything". An empty query is a question that has not been asked
        // yet, and answering it with the whole tree is how a search box becomes a
        // way to accidentally list a million files.
        return;
    }
    let matched = scan(index, needle, pool, cancel);
    if cancel.is_cancelled() {
        return;
    }
    found.scanned += index.entries as u64;
    // The cap is over the whole scope, so what is left of it is what this scan
    // may add — and reaching it is reported rather than hidden.
    let room = MOST.saturating_sub(found.hits.len());
    found.capped |= matched.len() > room;
    for entry in matched.into_iter().take(room) {
        // **Verified here, which is the freshness policy.** ADR 0012 puts one
        // `statx` between a hit and the screen, so a file deleted since the build
        // never appears; the cap is what keeps that at two thousand calls
        // however large the tree.
        let path = index.path_of(entry);
        let at = rustix::fs::AtFlags::SYMLINK_NOFOLLOW;
        let wanted = StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::MTIME;
        let Ok(stat) = rustix::fs::statx(rustix::fs::CWD, &path, at, wanted) else {
            // Gone since the build. Not an error, and not a result.
            found.partial = true;
            continue;
        };
        found.hits.push(Hit {
            path,
            size: stat.stx_size,
            modified: stat.stx_mtime.tv_sec,
            directory: u32::from(stat.stx_mode) & 0o170_000 == 0o040_000,
        });
    }
}

/// How many entries one shard covers.
///
/// Large enough that claiming a shard costs nothing beside scanning it, small
/// enough that the last shard is not the whole tail of the answer.
const SHARD: usize = 16_384;

/// What the shards are filling in.
struct Shards {
    /// The next shard nobody has taken.
    next: usize,
    /// Each shard's matches, or nothing while it is still outstanding.
    done: Vec<Option<Vec<usize>>>,
}

/// The entries that match, in index order.
///
/// # Why this cannot deadlock
///
/// The pool has a fixed number of workers and does not steal work, so a job that
/// waited on another job could wait forever. This waits on none. Shards are
/// claimed from one counter by the helpers *and* by the calling thread, and once
/// the caller has run out of unclaimed shards it **scans the outstanding ones
/// itself** rather than waiting for them. A helper that was never scheduled
/// therefore costs nothing; a helper that finishes late writes an answer
/// identical to the one already there.
fn scan(index: &Arc<Index>, needle: &str, pool: &sk_util::Pool, cancel: &Cancel) -> Vec<usize> {
    scan_by(index, needle, pool, cancel, SHARD)
}

/// As [`scan`], with the shard size given.
///
/// Exists so a boundary can be tested: at the real sixteen thousand a fixture
/// small enough to be a test is one shard, and every assertion about the shards
/// agreeing would be vacuously true.
fn scan_by(
    index: &Arc<Index>,
    needle: &str,
    pool: &sk_util::Pool,
    cancel: &Cancel,
    size: usize,
) -> Vec<usize> {
    let size = size.max(1);
    let shards = index.entries.div_ceil(size);
    if shards <= 1 {
        return shard_of(index, needle, 0, cancel, size);
    }
    let state = Arc::new(Mutex::new(Shards { next: 0, done: vec![None; shards] }));

    for _ in 0..pool.width().saturating_sub(1).min(shards - 1) {
        let (state, index) = (Arc::clone(&state), Arc::clone(index));
        let needle = needle.to_owned();
        pool.spawn(sk_util::Lane::Parallel, cancel.clone(), move |cancel| {
            claim_and_scan(&index, &needle, &state, cancel, size);
        });
    }
    claim_and_scan(index, needle, &state, cancel, size);

    let mut out = Vec::new();
    for shard in 0..shards {
        let held = state.lock().ok().and_then(|it| it.done.get(shard).cloned().flatten());
        match held {
            Some(hits) => out.extend_from_slice(&hits),
            // Claimed by a helper that has not delivered — possibly one the pool
            // never scheduled. Doing it here is always right and never waits.
            None if !cancel.is_cancelled() => {
                out.extend_from_slice(&shard_of(index, needle, shard, cancel, size));
            }
            None => {}
        }
    }
    out
}

/// Takes shards until there are none left unclaimed.
fn claim_and_scan(
    index: &Index,
    needle: &str,
    state: &Mutex<Shards>,
    cancel: &Cancel,
    size: usize,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let shard = {
            let Ok(mut held) = state.lock() else { return };
            if held.next >= held.done.len() {
                return;
            }
            let shard = held.next;
            held.next += 1;
            shard
        };
        let hits = shard_of(index, needle, shard, cancel, size);
        if let Ok(mut held) = state.lock()
            && let Some(slot) = held.done.get_mut(shard)
        {
            *slot = Some(hits);
        }
    }
}

/// One shard's matches.
fn shard_of(index: &Index, needle: &str, shard: usize, cancel: &Cancel, size: usize) -> Vec<usize> {
    let from = shard * size;
    let to = from.saturating_add(size).min(index.entries);
    let mut out = Vec::new();
    for entry in from..to {
        // Every few thousand rather than every entry: the check is an atomic load
        // and the comparison it guards is a few nanoseconds.
        if entry % 4096 == 0 && cancel.is_cancelled() {
            return out;
        }
        if sk_model::contains_folded(index.name_of(entry), needle) {
            out.push(entry);
            // **One past the cap, not the cap.** Stopping *at* `MOST` leaves the
            // caller unable to tell a shard that ran out of matches from one that
            // ran out of room, so a search with exactly two thousand results
            // reported itself complete and one with a million did too.
            if out.len() > MOST {
                return out;
            }
        }
    }
    out
}

/// The device a path is on, as `statx` reports it.
fn device_of(path: &Path) -> Option<u64> {
    let stat =
        rustix::fs::statx(rustix::fs::CWD, path, rustix::fs::AtFlags::empty(), StatxFlags::TYPE)
            .ok()?;
    Some(u64::from(stat.stx_dev_major) << 32 | u64::from(stat.stx_dev_minor))
}

/// Seconds since the epoch.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| i64::try_from(it.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{HEADER, Index, MOST, build, count, load_from, save_to, scan_by, search, shard_of};
    use sk_util::{Cancel, Pool};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn fixture(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-store-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("reports")).unwrap();
        std::fs::write(root.join("top.txt"), b"x").unwrap();
        std::fs::write(root.join("a/Report.pdf"), vec![0u8; 40]).unwrap();
        std::fs::write(root.join("a/b/notes.md"), b"y").unwrap();
        std::fs::write(root.join("a/b/c/report-2024.pdf"), vec![0u8; 80]).unwrap();
        root
    }

    fn built(root: &Path) -> Arc<Index> {
        Arc::new(build(root, &Cancel::new(), &mut |_| {}).expect("the build found no root"))
    }

    fn names(found: &super::Found) -> Vec<String> {
        let mut out: Vec<String> = found
            .hits
            .iter()
            .map(|hit| hit.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    fn hunt(index: &Arc<Index>, needle: &str) -> super::Found {
        // One worker, so the shard the caller finishes itself is the whole scan.
        search(index, needle, &Pool::with_width(1), &Cancel::new())
    }

    /// The count is the number of entries the build stores. Exactly.
    ///
    /// **This is the whole basis for the progress bar being honest.** The bar's
    /// denominator comes from a second, cheaper walk, and the two agreeing is not
    /// something the type system can arrange — they are separate loops that make
    /// the same decisions for different reasons. The fixture holds one of each
    /// thing they could disagree about: a symlink, which `size` would not have
    /// counted; a directory that cannot be read; and a dangling link.
    #[test]
    fn the_count_is_exactly_what_the_build_stores() {
        let root = fixture("count");
        std::os::unix::fs::symlink(root.join("a"), root.join("a-link")).unwrap();
        std::os::unix::fs::symlink("nowhere-at-all", root.join("dangling")).unwrap();
        let shut = root.join("shut");
        std::fs::create_dir_all(shut.join("hidden-inside")).unwrap();
        std::fs::write(shut.join("unreachable.txt"), b"x").unwrap();
        // Unreadable, so neither walk can see inside it — and both must still
        // count the directory itself.
        let locked = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(&shut, locked).unwrap();

        let mut counted = 0usize;
        let mut finished = false;
        count(&root, &Cancel::new(), &mut |seen, done| {
            counted = seen;
            finished = done;
        });
        let index = built(&root);

        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(finished, "the count never said it had finished");
        assert_eq!(
            counted,
            index.len(),
            "the count and the build disagree, so the bar would end somewhere other than the end"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cancelled count stops rather than finishing for nobody.
    #[test]
    fn a_cancelled_count_never_says_it_finished() {
        let root = fixture("count-cancel");
        let cancel = Cancel::new();
        cancel.cancel();
        let mut reports = 0;
        count(&root, &cancel, &mut |_, _| reports += 1);
        assert_eq!(reports, 0, "a cancelled count reported anyway");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every entry is reachable by the path the parent chain reconstructs.
    #[test]
    fn a_built_index_reconstructs_every_path_it_stored() {
        let root = fixture("paths");
        let index = built(&root);
        assert_eq!(index.len(), 8, "expected eight entries, got {}", index.len());
        let mut seen: Vec<PathBuf> = (0..index.len()).map(|it| index.path_of(it)).collect();
        seen.sort();
        assert!(seen.contains(&root.join("a/b/c/report-2024.pdf")), "{seen:?}");
        assert!(seen.contains(&root.join("reports")), "{seen:?}");
        // Every reconstructed path is a real one: a parent chain that is off by
        // one still produces plausible-looking paths, and this is what tells.
        for path in &seen {
            assert!(path.symlink_metadata().is_ok(), "{} is not on disk", path.display());
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Written and read back, the index answers the same as the one in memory.
    #[test]
    fn a_saved_index_loads_back_with_the_same_entries() {
        let root = fixture("roundtrip");
        let index = built(&root);
        let file = root.join("..").join(format!("{}-roundtrip.skix", std::process::id()));
        assert!(save_to(&index, &file), "the index would not save");
        let back = Arc::new(load_from(&file, &root).expect("the index would not load back"));
        assert_eq!(back.len(), index.len());
        assert_eq!(back.built(), index.built());
        assert_eq!(back.root(), root);
        assert_eq!(names(&hunt(&back, "report")), ["Report.pdf", "report-2024.pdf", "reports"]);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every corruption is a walk, which is ADR 0008's third constraint.
    ///
    /// **The one test this module most needs.** A silent misread is worse than a
    /// refusal here, because the caller has a correct answer available and would
    /// not use it.
    #[test]
    fn a_damaged_file_is_refused_rather_than_misread() {
        let root = fixture("damage");
        let index = built(&root);
        let file = root.join("..").join(format!("{}-damage.skix", std::process::id()));
        assert!(save_to(&index, &file));
        let whole = std::fs::read(&file).unwrap();

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("a header and nothing else", whole[..HEADER].to_vec()),
            ("truncated body", whole[..whole.len() - 8].to_vec()),
            ("wrong magic", {
                let mut it = whole.clone();
                it[0] = b'X';
                it
            }),
            ("wrong version", {
                let mut it = whole.clone();
                it[4] = 99;
                it
            }),
            ("a flipped byte in the arena", {
                let mut it = whole.clone();
                let at = HEADER + whole.len() / 2;
                it[at] ^= 0xff;
                it
            }),
            ("a lie about the entry count", {
                let mut it = whole.clone();
                it[8] = it[8].wrapping_add(1);
                it
            }),
        ];
        for (what, bytes) in cases {
            std::fs::write(&file, &bytes).unwrap();
            assert!(load_from(&file, &root).is_none(), "{what} was accepted");
        }

        // And an index built for one root is not served for another.
        std::fs::write(&file, &whole).unwrap();
        assert!(load_from(&file, Path::new("/")).is_none(), "another root was accepted");
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same folding every other view uses, which is rule 8.
    #[test]
    fn the_match_ignores_ascii_case_the_way_the_one_directory_filter_does() {
        let root = fixture("folding");
        let index = built(&root);
        assert_eq!(names(&hunt(&index, "REPORT")), ["Report.pdf", "report-2024.pdf", "reports"]);
        assert_eq!(names(&hunt(&index, "rEpOrT.")), ["Report.pdf"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty query is a question nobody asked, not a request for everything.
    #[test]
    fn an_empty_query_finds_nothing_rather_than_the_whole_tree() {
        let root = fixture("empty-query");
        let found = hunt(&built(&root), "");
        assert!(found.hits.is_empty());
        assert!(found.complete);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file deleted since the build never reaches the screen.
    ///
    /// ADR 0012's freshness policy is one `statx` between a hit and the result,
    /// and this is the assertion that it happens. Without it the index would
    /// answer with files that are not there, which is the failure mode that makes
    /// people distrust a search.
    #[test]
    fn a_file_deleted_since_the_build_is_not_a_result() {
        let root = fixture("stale");
        let index = built(&root);
        std::fs::remove_file(root.join("a/Report.pdf")).unwrap();
        let found = hunt(&index, "report");
        assert_eq!(names(&found), ["report-2024.pdf", "reports"], "a deleted file was reported");
        assert!(found.partial, "dropping a vanished hit was not reported as partial");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shards agree with one unsharded pass, at every boundary.
    ///
    /// Driven at a shard of one so that every entry is its own shard and the
    /// helpers, the caller's own claims and the caller's fallback all run. At the
    /// real sixteen thousand a fixture this size is a single shard and this would
    /// assert nothing.
    #[test]
    fn a_sharded_scan_finds_what_one_pass_finds_and_in_the_same_order() {
        let root = fixture("shards");
        for extra in 0..40 {
            std::fs::write(root.join(format!("report-{extra:02}.txt")), b"z").unwrap();
        }
        let index = built(&root);
        let whole = shard_of(&index, "report", 0, &Cancel::new(), index.len());
        assert!(whole.len() > 40, "the fixture did not produce enough matches");
        for width in [1usize, 2, 4, 8] {
            let pool = Pool::with_width(width);
            for size in [1usize, 3, 7, 16] {
                let sharded = scan_by(&index, "report", &pool, &Cancel::new(), size);
                assert_eq!(sharded, whole, "width {width}, shard {size} disagreed");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cancelled scan stops rather than finishing for nobody.
    #[test]
    fn a_cancelled_search_reports_nothing() {
        let root = fixture("cancel");
        let index = built(&root);
        let cancel = Cancel::new();
        cancel.cancel();
        let found = search(&index, "report", &Pool::with_width(2), &cancel);
        assert!(found.hits.is_empty());
        assert!(!found.complete, "a cancelled search claimed to be complete");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The cap is the walk's cap, so the two views cannot disagree about it.
    #[test]
    fn the_result_cap_is_the_same_one_the_walk_uses() {
        let root = fixture("cap");
        // One directory, because this is about the cap and not about the walk.
        let many = root.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for at in 0..(MOST + 20) {
            std::fs::write(many.join(format!("hit-{at:05}.txt")), b"q").unwrap();
        }
        let found = hunt(&built(&root), "hit-");
        assert_eq!(found.hits.len(), MOST);
        assert!(found.capped, "the cap was reached and not reported");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A measurement, not a test: build, save and load a real tree and say what it
/// cost.
///
/// Ignored by default because it walks whatever `SKERRY_INDEX_TREE` names and a
/// suite that reads the machine it runs on is a suite with a different answer on
/// every machine. Run it deliberately:
///
/// ```text
/// SKERRY_INDEX_TREE=/usr cargo test -p sk-index --release measure_an_index -- --ignored --nocapture
/// ```
#[cfg(test)]
mod measure {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

    use super::{build, count, load_from, save_to, search, search_into};
    use sk_util::{Cancel, Pool};
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    #[ignore = "walks a real tree; run it deliberately"]
    fn measure_an_index() {
        let Some(tree) = std::env::var_os("SKERRY_INDEX_TREE") else {
            println!("skerry-index set SKERRY_INDEX_TREE to a directory to measure");
            return;
        };
        let root = std::path::PathBuf::from(tree);
        let file = std::env::temp_dir().join("skerry-measure.skix");

        // The counting pass first, on its own, because the question the progress
        // bar turns on is whether it finishes early enough to be worth having.
        let at = Instant::now();
        let mut counted = 0usize;
        count(&root, &Cancel::new(), &mut |seen, _| counted = seen);
        let counting = at.elapsed();

        let at = Instant::now();
        let index = build(&root, &Cancel::new(), &mut |_| {}).expect("the build found no root");
        let building = at.elapsed();
        let entries = index.len();
        assert_eq!(counted, entries, "the count and the build disagree");

        let at = Instant::now();
        assert!(save_to(&index, &file), "the index would not save");
        let saving = at.elapsed();
        let bytes = std::fs::metadata(&file).unwrap().len();

        let at = Instant::now();
        let back = Arc::new(load_from(&file, &root).expect("the index would not load"));
        let loading = at.elapsed();

        let pool = Pool::new();
        let at = Instant::now();
        let found = search(&back, "config", &pool, &Cancel::new());
        let scanning = at.elapsed();

        // The thing it replaces, over the same tree and for the same query: a
        // complete walk. Not the *first* result, which the walk streams and
        // already answers fast — the whole answer, which is what an as-you-type
        // keystroke needs before the next one lands.
        let at = Instant::now();
        let mut walked = crate::find::Found::default();
        crate::find::find(&root, "config", &Cancel::new(), &mut |it| walked = it.clone());
        let walking = at.elapsed();

        // Every line carries the prefix the rest of this program's output does,
        // so a measurement in a build log is greppable and is not mistaken for a
        // log line — which is the whole of what `tools/check-invariants.py`
        // asks for here.
        println!("skerry-index root {}", root.display());
        println!("skerry-index entries {entries}");
        println!("skerry-index build {:.1} ms", building.as_secs_f64() * 1000.0);
        println!(
            "skerry-index count {:.1} ms ({:.0}% of the build)",
            counting.as_secs_f64() * 1000.0,
            counting.as_secs_f64() / building.as_secs_f64() * 100.0
        );
        println!("skerry-index save {:.1} ms", saving.as_secs_f64() * 1000.0);
        println!("skerry-index load-warm {:.1} ms", loading.as_secs_f64() * 1000.0);
        println!(
            "skerry-index scan {:.2} ms, {} hits",
            scanning.as_secs_f64() * 1000.0,
            found.hits.len()
        );
        println!(
            "skerry-index walk-complete {:.1} ms, {} hits",
            walking.as_secs_f64() * 1000.0,
            walked.hits.len()
        );
        println!(
            "skerry-index on-disk {bytes} bytes, {:.1} per entry",
            bytes as f64 / entries.max(1) as f64
        );
        let _ = std::fs::remove_file(&file);
    }

    /// What a search over several roots costs, unindexed: the first result, and
    /// the whole answer.
    ///
    /// The scope [ADR
    /// 0022](../../../docs/adr/0022-a-search-names-a-scope-and-everywhere-is-the-local-mounts.md)
    /// calls *everywhere*, measured over the code that runs it rather than over
    /// one root at a time. Run it deliberately:
    ///
    /// ```text
    /// SKERRY_SEARCH_ROOTS=/:/boot cargo test -p sk-index --release measure_a_search_over_several_roots -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "walks real filesystems; run it deliberately"]
    fn measure_a_search_over_several_roots() {
        let Some(value) = std::env::var_os("SKERRY_SEARCH_ROOTS") else {
            println!("skerry-search set SKERRY_SEARCH_ROOTS to roots to measure, `:` between");
            return;
        };
        let roots: Vec<std::path::PathBuf> = std::env::split_paths(&value).collect();
        let needle = std::env::var("SKERRY_SEARCH_FOR").unwrap_or_else(|_| "config".to_owned());
        // The roots to answer from an index, built here and not timed: what is
        // being measured is the search, and the build is its own row.
        let wanted: Vec<std::path::PathBuf> = std::env::var_os("SKERRY_SEARCH_INDEX")
            .map(|it| std::env::split_paths(&it).collect())
            .unwrap_or_default();
        let pool = Pool::new();
        let held: Vec<Arc<crate::Index>> = wanted
            .iter()
            .filter_map(|root| build(root, &Cancel::new(), &mut |_| {}).map(Arc::new))
            .collect();
        let walks: Vec<std::path::PathBuf> = roots
            .iter()
            .filter(|root| !held.iter().any(|index| index.root() == root.as_path()))
            .cloned()
            .collect();

        // The order the window runs them in: the indexed roots first, because
        // tens of milliseconds of answer belongs on screen before seconds of
        // walking start.
        let at = Instant::now();
        let mut first: Option<std::time::Duration> = None;
        let mut found = crate::find::Found::default();
        for index in &held {
            search_into(index, &needle, &pool, &Cancel::new(), &mut found);
            if first.is_none() && !found.hits.is_empty() {
                first = Some(at.elapsed());
            }
        }
        let scanned = at.elapsed();
        crate::find::across(&walks, &needle, &Cancel::new(), &mut found, &mut |seen| {
            if first.is_none() && !seen.hits.is_empty() {
                first = Some(at.elapsed());
            }
        });
        let whole = at.elapsed();

        println!("skerry-search roots {}", roots.len());
        println!("skerry-search indexed {} walked {}", held.len(), walks.len());
        if !held.is_empty() {
            println!("skerry-search scans {:.2} ms", scanned.as_secs_f64() * 1000.0);
        }
        println!("skerry-search needle {needle}");
        println!(
            "skerry-search first-result {:.2} ms",
            first.unwrap_or_default().as_secs_f64() * 1000.0
        );
        println!("skerry-search complete {:.1} ms", whole.as_secs_f64() * 1000.0);
        println!(
            "skerry-search entries {} hits {} capped {} partial {}",
            found.scanned,
            found.hits.len(),
            found.capped,
            found.partial
        );
    }
}
