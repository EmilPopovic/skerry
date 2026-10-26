// SPDX-License-Identifier: Apache-2.0
//! Filesystem-local XDG trash and bounded, reversible recovery records (ADR 0035).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use sk_model::{Problem, ProblemKind};

/// A validated recovery record, also retained by undo after restoration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Original absolute destination, never inferred from the stored filename.
    pub original: PathBuf,
    /// File currently in trash, or its reserved location after restoring.
    pub stored: PathBuf,
    /// Associated information file.
    pub info: PathBuf,
    /// Original metadata, retained so undo can restore it exactly.
    pub record: String,
    /// A cleanup or durability problem after the move completed.
    pub warning: Option<String>,
}

struct Storage {
    root: PathBuf,
    files: OwnedFd,
    info: OwnedFd,
}

fn failure(path: &Path, detail: impl std::fmt::Display) -> Problem {
    Problem::new(ProblemKind::TrashUnavailable, path.display().to_string(), detail.to_string())
}

fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Where this user's home trash is, decided once and handed down.
///
/// **A value rather than a lookup per call.** The home trash is the one location
/// the environment chooses, and [`Trash::from_environment`] reads it when the
/// program starts; everything that puts into a trash, reads one or refuses to
/// act inside one is given the same value. A test gives one inside its own
/// scratch directory instead of writing `$XDG_DATA_HOME` — which, with C in the
/// same process reading the environment (`localtime_r` reads `TZ`), is a data
/// race, and is `unsafe` in edition 2024 for that reason.
///
/// A volume's own trash is not part of it: that is `.Trash-$uid` at the top of
/// whichever mount a file is on, which the file decides and nothing configures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trash {
    /// The home trash — the directory holding `files` and `info` — or nothing
    /// where neither variable names an absolute directory.
    home: Option<PathBuf>,
}

impl Trash {
    /// `$XDG_DATA_HOME/Trash`, else `$HOME/.local/share/Trash`, per the
    /// specification. No filesystem access.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_variables(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
    }

    /// [`Trash::from_environment`] over the two values it reads.
    #[must_use]
    pub fn from_variables(data_home: Option<OsString>, home: Option<OsString>) -> Self {
        let data = data_home.map(PathBuf::from).filter(|p| p.is_absolute());
        let home = data.map(|data| data.join("Trash")).or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".local/share/Trash"))
        });
        Self { home }
    }

    /// A home trash at `home`, which is the directory `files` and `info` go in.
    ///
    /// A relative path is no trash at all, the same answer a relative
    /// `$XDG_DATA_HOME` gets: every record written in it would be read back
    /// against whatever the working directory had become.
    #[must_use]
    pub fn at(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self { home: home.is_absolute().then_some(home) }
    }

    /// The home trash's location. No filesystem access.
    pub fn home(&self) -> Result<&Path, Problem> {
        self.home
            .as_deref()
            .ok_or_else(|| failure(Path::new("Trash"), "no absolute home directory configured"))
    }
}

fn private(
    parent: impl std::os::fd::AsFd,
    name: &OsStr,
    create: bool,
) -> Result<OwnedFd, rustix::io::Errno> {
    if create {
        match rustix::fs::mkdirat(&parent, name, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error),
        }
    }
    let directory = rustix::fs::openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&directory)?;
    // Never trust another user's trash or a directory they can replace entries in.
    // FAT may emulate read/execute bits; creation still requests private mode.
    if stat.st_uid != uid() || stat.st_mode & 0o022 != 0 {
        return Err(rustix::io::Errno::ACCESS);
    }
    Ok(directory)
}

impl Storage {
    fn open(root: &Path, create: bool) -> Result<Self, Problem> {
        let directory =
            private(rustix::fs::CWD, root.as_os_str(), create).map_err(|e| failure(root, e))?;
        let files =
            private(&directory, OsStr::new("files"), create).map_err(|e| failure(root, e))?;
        let info = private(&directory, OsStr::new("info"), create).map_err(|e| failure(root, e))?;
        Ok(Self { root: root.to_path_buf(), files, info })
    }

    fn reserve(&self, name: &OsStr, record: &str) -> Result<PathBuf, Problem> {
        let fd = rustix::fs::openat(
            &self.info,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| crate::ops::problem(e, &self.root.join("info").join(name)))?;
        let mut file = File::from(fd);
        if let Err(error) = file.write_all(record.as_bytes()).and_then(|()| file.sync_all()) {
            let _ = rustix::fs::unlinkat(&self.info, name, AtFlags::empty());
            return Err(failure(&self.root, error));
        }
        if let Err(error) = rustix::fs::fsync(&self.info) {
            let _ = rustix::fs::unlinkat(&self.info, name, AtFlags::empty());
            return Err(failure(&self.root, error));
        }
        Ok(self.root.join("info").join(name))
    }
}

fn absolute(path: &Path) -> Result<PathBuf, Problem> {
    let name = path.file_name().ok_or_else(|| failure(path, "no filename"))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent).map_err(|e| failure(path, e))?;
    Ok(parent.join(name))
}

fn mount_paths() -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read("/proc/self/mountinfo") else { return Vec::new() };
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| line.split(|b| *b == b' ').nth(4))
        .map(|field| {
            let mut decoded = Vec::new();
            let mut i = 0;
            while i < field.len() {
                if field[i] == b'\\'
                    && i + 3 < field.len()
                    && field[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b))
                {
                    decoded.push(
                        (field[i + 1] - b'0') * 64 + (field[i + 2] - b'0') * 8 + field[i + 3]
                            - b'0',
                    );
                    i += 4;
                } else {
                    decoded.push(field[i]);
                    i += 1;
                }
            }
            PathBuf::from(OsString::from_vec(decoded))
        })
        .collect()
}

fn mount_of<'a>(path: &Path, mounts: &'a [PathBuf]) -> Option<&'a Path> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(mount))
        .max_by_key(|mount| mount.as_os_str().len())
        .map(PathBuf::as_path)
}

fn shared(top: &Path) -> Option<PathBuf> {
    let root = top.join(".Trash");
    let directory = rustix::fs::open(
        &root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&directory).ok()?;
    (stat.st_mode & 0o1000 != 0 && (stat.st_uid == 0 || stat.st_uid == uid()))
        .then(|| root.join(uid().to_string()))
}

fn volume(top: &Path) -> Result<Storage, Problem> {
    if let Some(root) = shared(top)
        && let Ok(storage) = Storage::open(&root, true)
    {
        return Ok(storage);
    }
    Storage::open(&top.join(format!(".Trash-{}", uid())), true)
}

impl Trash {
    /// Trash on the source mount; no copy/delete fallback. Runs on a filesystem job.
    pub fn put(&self, path: &Path) -> Result<Entry, Problem> {
        put(self.home()?, path)
    }

    /// Prepare the home Trash place. Runs on a filesystem job.
    pub fn home_files(&self) -> Result<PathBuf, Problem> {
        let home = self.home()?;
        if let Some(parent) = home.parent() {
            std::fs::create_dir_all(parent).map_err(|e| failure(home, e))?;
        }
        Storage::open(home, true).map(|s| s.root.join("files"))
    }
}

fn put(configured: &Path, path: &Path) -> Result<Entry, Problem> {
    let original = absolute(path)?;
    let (Some(parent), Some(own)) = (configured.parent(), configured.file_name()) else {
        return Err(failure(path, "no trash parent"));
    };
    std::fs::create_dir_all(parent).map_err(|e| failure(path, e))?;
    let home = std::fs::canonicalize(parent).map_err(|e| failure(path, e))?.join(own);
    let mounts = mount_paths();
    let top =
        mount_of(&original, &mounts).ok_or_else(|| failure(path, "source mount unavailable"))?;
    let local = mount_of(&home, &mounts) == Some(top);
    let storage = if local { Storage::open(&home, true) } else { volume(top) }
        .map_err(|e| failure(path, e.detail()))?;
    if original.starts_with(&storage.root) {
        return Err(failure(path, "already inside trash; restore it instead"));
    }
    let recorded = if local {
        original.as_path()
    } else {
        original.strip_prefix(top).map_err(|e| failure(path, e))?
    };
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        crate::ops::encode_path_bytes(recorded.as_os_str().as_encoded_bytes()),
        crate::ops::deletion_date()
    );
    let name = original.file_name().ok_or_else(|| failure(path, "no filename"))?;
    for attempt in 0..1000 {
        let mut candidate = name.to_owned();
        if attempt != 0 {
            candidate.push(format!(".{attempt}"));
        }
        let mut info_name = candidate.clone();
        info_name.push(".trashinfo");
        let info = match storage.reserve(&info_name, &record) {
            Ok(info) => info,
            Err(p) if p.kind() == ProblemKind::NameTaken => continue,
            Err(p) => return Err(failure(path, p.detail())),
        };
        match rustix::fs::renameat_with(
            rustix::fs::CWD,
            &original,
            &storage.files,
            &candidate,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                return Ok(Entry {
                    original,
                    stored: storage.root.join("files").join(candidate),
                    info,
                    record,
                    warning: rustix::fs::fsync(&storage.files).err().map(|_| {
                        sk_model::i18n::format("notice.trash_metadata", &[&path.display()])
                    }),
                });
            }
            Err(error) => {
                let _ = rustix::fs::unlinkat(&storage.info, &info_name, AtFlags::empty());
                if error != rustix::io::Errno::EXIST {
                    return Err(failure(path, error));
                }
            }
        }
    }
    Err(failure(path, "no free name in trash"))
}

/// Existing, validated trash listings. Each supplied mount can be probed on its own job.
#[must_use]
pub fn locations(top: &Path) -> Vec<PathBuf> {
    let mut roots = vec![top.join(format!(".Trash-{}", uid()))];
    if let Some(root) = shared(top) {
        roots.push(root);
    }
    roots
        .into_iter()
        .filter(|root| Storage::open(root, false).is_ok())
        .map(|root| root.join("files"))
        .collect()
}

fn decode_path(value: &str) -> Option<PathBuf> {
    let mut out = Vec::new();
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        let decoded = if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        };
        if decoded == 0 {
            return None;
        }
        out.push(decoded);
    }
    let path = PathBuf::from(OsString::from_vec(out));
    (!path.as_os_str().is_empty() && !path.components().any(|p| matches!(p, Component::ParentDir)))
        .then_some(path)
}

fn origin(home: &Path, record: &str, root: &Path) -> Result<PathBuf, Problem> {
    let mut lines = record.lines();
    if lines.next() != Some("[Trash Info]") {
        return Err(failure(root, "invalid trash record header"));
    }
    let value = lines
        .find_map(|line| line.strip_prefix("Path="))
        .and_then(decode_path)
        .ok_or_else(|| failure(root, "invalid original path in trash record"))?;
    let home = absolute(home)?;
    let is_home = absolute(root)? == home;
    let top = if is_home {
        home.parent()
    } else if root.parent().and_then(Path::file_name) == Some(OsStr::new(".Trash")) {
        root.parent().and_then(Path::parent)
    } else {
        root.parent()
    }
    .ok_or_else(|| failure(root, "trash has no parent"))?;
    if value.is_absolute() {
        if !is_home {
            return Err(failure(root, "absolute path in volume trash"));
        }
        return Ok(value);
    }
    let path = top.join(value);
    let resolved = absolute(&path)?;
    if !resolved.starts_with(std::fs::canonicalize(top).map_err(|e| failure(top, e))?) {
        return Err(failure(&path, "relative original path escapes the trash volume"));
    }
    Ok(resolved)
}

/// Whether `root` is a trash this user's desktop would restore from.
///
/// The home one, or one of the two the specification allows on a volume. A
/// directory that merely looks like a trash is refused, because everything that
/// reads a trash here also writes to it or erases inside it.
fn recognised(home: &Path, root: &Path) -> Result<(), Problem> {
    let mounts = mount_paths();
    let valid_volume = mount_of(root, &mounts).is_some_and(|top| {
        root == top.join(format!(".Trash-{}", uid()))
            || shared(top).is_some_and(|shared| root == shared)
    });
    if absolute(root)? != absolute(home)? && !valid_volume {
        return Err(failure(root, "not a recognized trash location"));
    }
    Ok(())
}

impl Trash {
    /// Validate metadata for a direct trash child without following information-file symlinks.
    pub fn read(&self, stored: &Path) -> Result<Entry, Problem> {
        read(self.home()?, stored)
    }

    /// Restore without replacing anything; the information record is removed only after moving.
    pub fn restore(&self, stored: &Path) -> Result<Entry, Problem> {
        restore(self.home()?, stored)
    }
}

fn read(home: &Path, stored: &Path) -> Result<Entry, Problem> {
    let files = stored.parent().ok_or_else(|| failure(stored, "no trash directory"))?;
    if files.file_name() != Some(OsStr::new("files")) {
        return Err(failure(stored, "not a trash entry"));
    }
    let root = files.parent().ok_or_else(|| failure(stored, "no trash root"))?;
    recognised(home, root).map_err(|problem| failure(stored, problem.detail()))?;
    let storage = Storage::open(root, false)?;
    let mut name = stored.file_name().ok_or_else(|| failure(stored, "no name"))?.to_owned();
    name.push(".trashinfo");
    let fd = rustix::fs::openat(
        &storage.info,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| failure(stored, e))?;
    let stat = rustix::fs::fstat(&fd).map_err(|e| failure(stored, e))?;
    if stat.st_uid != uid()
        || stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_mode & 0o022 != 0
        || stat.st_size > 65536
    {
        return Err(failure(stored, "unsafe or oversized trash record"));
    }
    let mut record = String::new();
    File::from(fd).take(65537).read_to_string(&mut record).map_err(|e| failure(stored, e))?;
    if record.len() > 65536 {
        return Err(failure(stored, "oversized trash record"));
    }
    let original = origin(home, &record, root)?;
    if original.starts_with(root) || original.file_name().is_none() {
        return Err(failure(stored, "invalid restore destination"));
    }
    Ok(Entry {
        original,
        stored: stored.to_path_buf(),
        info: root.join("info").join(name),
        record,
        warning: None,
    })
}

fn restore(home: &Path, stored: &Path) -> Result<Entry, Problem> {
    let mut entry = read(home, stored)?;
    let root =
        entry.info.parent().and_then(Path::parent).ok_or_else(|| failure(stored, "no root"))?;
    let storage = Storage::open(root, false)?;
    let name = stored.file_name().ok_or_else(|| failure(stored, "no name"))?;
    rustix::fs::renameat_with(
        &storage.files,
        name,
        rustix::fs::CWD,
        &entry.original,
        RenameFlags::NOREPLACE,
    )
    .map_err(|e| crate::ops::problem(e, &entry.original))?;
    let info_name = entry.info.file_name().ok_or_else(|| failure(stored, "no info name"))?;
    if let Err(error) = rustix::fs::unlinkat(&storage.info, info_name, AtFlags::empty()) {
        let _ = error;
        entry.warning =
            Some(sk_model::i18n::format("notice.trash_metadata", &[&entry.original.display()]));
    }
    Ok(entry)
}

/// Undo restoration without replacing a new trash entry or deleting restored data.
pub fn put_back(entry: &Entry) -> Result<(), Problem> {
    let root = entry
        .info
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| failure(&entry.info, "no root"))?;
    let storage = Storage::open(root, false)?;
    let info = entry.info.file_name().ok_or_else(|| failure(&entry.info, "no name"))?;
    storage.reserve(info, &entry.record)?;
    let result = rustix::fs::renameat_with(
        rustix::fs::CWD,
        &entry.original,
        &storage.files,
        entry.stored.file_name().ok_or_else(|| failure(&entry.stored, "no name"))?,
        RenameFlags::NOREPLACE,
    );
    if let Err(error) = result {
        let _ = rustix::fs::unlinkat(&storage.info, info, AtFlags::empty());
        return Err(crate::ops::problem(error, &entry.original));
    }
    Ok(())
}

/// What a trash is holding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Held {
    /// How many entries somebody put there, counted at the top level.
    ///
    /// The top level and not the leaves: one folder of four hundred photographs
    /// is *one* thing a person threw away, and it is the number the question
    /// "empty this?" is asking about.
    pub items: u64,
    /// How many bytes everything under them comes to.
    pub bytes: u64,
}

impl Held {
    /// Whether there is anything in it.
    #[must_use]
    pub const fn any(self) -> bool {
        self.items != 0
    }
}

/// How deep a walk of a trash goes before it calls the tree a cycle.
///
/// The same ceiling [`crate::copy`] uses, for the same reason: a hard link cycle
/// is not expressible, but a bind mount inside a trash is.
const MAX_DEPTH: u32 = 128;

/// What `files` is holding, walked to the leaves.
///
/// **Runs on a job**, and answers with what it managed to count rather than with
/// a failure: this is a figure beside a row, and a trash with one unreadable
/// directory in it is still a trash whose size is worth saying.
#[must_use]
pub fn held(files: &Path, cancel: &sk_util::Cancel) -> Held {
    let Ok(fd) = rustix::fs::open(
        files,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Held::default();
    };
    let Ok(dir) = rustix::fs::Dir::read_from(&fd) else { return Held::default() };
    let mut total = Held::default();
    for entry in dir.flatten() {
        if cancel.is_cancelled() {
            return total;
        }
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        total.items += 1;
        total.bytes += bytes_under(&fd, OsStr::from_bytes(name), cancel, 0);
    }
    total
}

/// How many bytes `name` under `parent` accounts for.
fn bytes_under(
    parent: &impl std::os::fd::AsFd,
    name: &OsStr,
    cancel: &sk_util::Cancel,
    depth: u32,
) -> u64 {
    if depth > MAX_DEPTH || cancel.is_cancelled() {
        return 0;
    }
    let Ok(stat) = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) else { return 0 };
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
        return stat.st_size.unsigned_abs();
    }
    let Ok(fd) = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return 0;
    };
    let Ok(dir) = rustix::fs::Dir::read_from(&fd) else { return 0 };
    let mut bytes = 0;
    for entry in dir.flatten() {
        let child = entry.file_name().to_bytes();
        if child == b"." || child == b".." {
            continue;
        }
        bytes += bytes_under(&fd, OsStr::from_bytes(child), cancel, depth + 1);
    }
    bytes
}

fn empty(home: &Path, files: &Path, cancel: &sk_util::Cancel) -> Result<(), Problem> {
    if files.file_name() != Some(OsStr::new("files")) {
        return Err(failure(files, "not a trash's files directory"));
    }
    let root = files.parent().ok_or_else(|| failure(files, "no trash root"))?;
    recognised(home, root)?;
    let storage = Storage::open(root, false)?;
    #[cfg(test)]
    if let Some(hook) = AFTER_OPEN.take() {
        hook(root);
    }
    // **Through the two descriptors just checked, never by name again.** The
    // path is resolved once, by `Storage::open`; somebody who can write the
    // folder above a `.Trash-$uid` could otherwise rename it away and plant a
    // symlink, and every entry would be deleted under the link's target.
    let mut first = None;
    let mut kept = std::collections::HashSet::new();
    for name in children(&storage.files)? {
        if cancel.is_cancelled() {
            return Err(failure(files, "cancelled"));
        }
        let subject = files.join(&name);
        if let Err(problem) =
            crate::copy::remove_at(&storage.files, Path::new(&name), &subject, cancel)
        {
            first.get_or_insert(problem);
            let mut record = name;
            record.push(".trashinfo");
            kept.insert(record);
        }
    }
    let info = root.join("info");
    for name in children(&storage.info)? {
        if cancel.is_cancelled() {
            return Err(failure(files, "cancelled"));
        }
        if kept.contains(&name) {
            continue;
        }
        let subject = info.join(&name);
        if let Err(problem) =
            crate::copy::remove_at(&storage.info, Path::new(&name), &subject, cancel)
        {
            first.get_or_insert(problem);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
std::thread_local! {
    /// Runs once [`empty`] has opened the trash and before it removes anything.
    pub(crate) static AFTER_OPEN: std::cell::Cell<Option<fn(&Path)>> =
        const { std::cell::Cell::new(None) };
}

/// Every name in an already-open directory, read before anything is removed.
///
/// Read first rather than removed as the walk goes: `getdents64` over a
/// directory being unlinked from is what the manual page calls unspecified, and
/// a trash half emptied for that reason is a trash whose second Empty finishes
/// the job silently.
fn children(directory: &impl std::os::fd::AsFd) -> Result<Vec<OsString>, Problem> {
    let fd = rustix::fs::openat(
        directory,
        OsStr::new("."),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| failure(Path::new("."), error))?;
    let dir = rustix::fs::Dir::read_from(&fd).map_err(|error| failure(Path::new("."), error))?;
    let mut names = Vec::new();
    for entry in dir {
        let entry = entry.map_err(|error| failure(Path::new("."), error))?;
        let raw = entry.file_name().to_bytes();
        if raw != b"." && raw != b".." {
            names.push(OsStr::from_bytes(raw).to_owned());
        }
    }
    Ok(names)
}

impl Trash {
    /// Erases everything in `files` and every record beside it.
    ///
    /// **The contents and not the directories.** `files` and `info` stay, because
    /// [`put_back`] — the inverse of a restore that is already on the undo stack —
    /// opens them without creating them, and a trash emptied down to its own root
    /// would turn every one of those entries into a `TrashUnavailable` about a path
    /// nobody has looked at. Undoing a restore after an Empty Trash puts the file
    /// back into an empty trash, which is exactly what it says it does.
    ///
    /// **The records go too, and they go second.** A record whose file is gone has
    /// nothing left to restore; a file whose record is gone has lost the one thing
    /// that says where it came from. So an interrupted empty leaves the first kind
    /// rather than the second — and emptying again takes what is left, because both
    /// directories are walked whole.
    ///
    /// **One entry that will not go does not keep the rest.** Stopping at it left
    /// every name `getdents64` happened to return after it, and every record — a
    /// trash that looked untouched, with the failure in a status line. The first
    /// problem is still the answer; the entries that failed keep their records.
    ///
    /// **Runs on a job.**
    pub fn empty(&self, files: &Path, cancel: &sk_util::Cancel) -> Result<(), Problem> {
        empty(self.home()?, files, cancel)
    }

    /// Whether a path lies in a trash namespace. Pure path inspection for UI policy.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.home().is_ok_and(|root| path.starts_with(root)) || in_a_volume_trash(path)
    }

    pub(crate) fn writable(&self, path: &Path) -> Result<(), Problem> {
        if self.contains(path) {
            Err(failure(path, "restore the item before changing it"))
        } else {
            Ok(())
        }
    }
}

fn in_a_volume_trash(path: &Path) -> bool {
    let mine = format!(".Trash-{}", uid());
    let user = uid().to_string();
    path.ancestors().any(|part| {
        part.file_name() == Some(OsStr::new(&mine))
            || (part.file_name() == Some(OsStr::new(&user))
                && part.parent().and_then(Path::file_name) == Some(OsStr::new(".Trash")))
    })
}

/// What a test needs to show its trash stayed in its own scratch directory.
///
/// **Here and not in a test module** because the tests that need it are in
/// `sk-app` as well as here, and it reads storage the way this module does —
/// hidden from the documentation, the way `sk_preview::sandbox::confine_below`
/// is, because nothing but a test has a reason to call it.
#[doc(hidden)]
pub mod testing {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::Read as _;
    use std::os::fd::OwnedFd;
    use std::path::{Path, PathBuf};

    use rustix::fs::{Mode, OFlags};

    use super::{Storage, Trash, children, decode_path, uid};

    /// Now, as a record's `DeletionDate=` says it: local, to the second.
    ///
    /// Taken when a test starts and handed to [`strays`], so a record left by an
    /// earlier run is not this one's.
    #[must_use]
    pub fn now() -> String {
        crate::ops::deletion_date()
    }

    /// Every record in a trash outside `scratch`, made at or after `since`, for
    /// something that was under it.
    ///
    /// A test hands [`Trash::at`] a directory inside its own scratch root and uses
    /// this to show that nothing it trashed went anywhere else. Read: the home
    /// trash the environment names, and the two volume trashes of every directory
    /// above `scratch` — the top of its filesystem is one of them, whichever that
    /// is. A test that did not route its trash this way left a folder in
    /// `/tmp/.Trash-1000` on every run, 104 of them on the machine that found it.
    ///
    /// **`since`, because a scratch directory's name is the process's and a pid is
    /// reused.** Those 104 are named after the test that left them, and a later
    /// run with one of their pids would otherwise be failed for them. Compared as
    /// text, which orders `YYYY-MM-DDThh:mm:ss` by time — local time, so the hour
    /// a clock goes back is an hour this cannot see into.
    ///
    /// A record's `Path=` is absolute in the home trash and relative to the top of
    /// the filesystem in a volume one, which is why each trash is read against its
    /// own top. Storage is opened the way every other reader here opens it, so a
    /// trash somebody else owns or a link planted at its name is skipped.
    #[must_use]
    pub fn strays(scratch: &Path, since: &str) -> Vec<PathBuf> {
        let Ok(scratch) = std::fs::canonicalize(scratch) else { return Vec::new() };
        let mut trashes: Vec<(PathBuf, Option<&Path>)> = Vec::new();
        if let Ok(home) = Trash::from_environment().home() {
            trashes.push((home.to_path_buf(), None));
        }
        for top in scratch.ancestors().skip(1) {
            trashes.push((top.join(format!(".Trash-{}", uid())), Some(top)));
            trashes.push((top.join(".Trash").join(uid().to_string()), Some(top)));
        }
        let mut found = Vec::new();
        for (root, top) in trashes {
            let Ok(storage) = Storage::open(&root, false) else { continue };
            let Ok(names) = children(&storage.info) else { continue };
            for name in names {
                let Some(record) = record_at(&storage.info, &name) else { continue };
                let field = |key: &str| record.lines().find_map(|line| line.strip_prefix(key));
                if field("DeletionDate=").is_none_or(|made| made < since) {
                    continue;
                }
                let Some(original) = field("Path=").and_then(decode_path) else { continue };
                let original = match top {
                    Some(top) if original.is_relative() => top.join(original),
                    _ => original,
                };
                if original.starts_with(&scratch) {
                    found.push(root.join("info").join(name));
                }
            }
        }
        found
    }

    /// One record's text, read through the open `info` directory without
    /// following a link or waiting on a FIFO, and no longer than a read of a
    /// trash entry accepts.
    fn record_at(info: &OwnedFd, name: &OsStr) -> Option<String> {
        let fd = rustix::fs::openat(
            info,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let mut record = String::new();
        File::from(fd).take(65537).read_to_string(&mut record).ok()?;
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::testing::strays;
    use super::{Trash, uid};
    use std::ffi::OsString;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("skerry-trash-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The specification's order, read from two values rather than from the
    /// process — so this runs beside every other test without writing either.
    #[test]
    fn the_home_trash_is_data_home_then_home_and_only_ever_absolute() {
        let both = |data: Option<&str>, home: Option<&str>| {
            Trash::from_variables(data.map(OsString::from), home.map(OsString::from))
        };
        assert_eq!(both(Some("/d"), Some("/h")).home().unwrap(), Path::new("/d/Trash"));
        assert_eq!(both(None, Some("/h")).home().unwrap(), Path::new("/h/.local/share/Trash"));
        // A relative data home is invalid by the base directory specification,
        // and is passed over rather than resolved against the working directory.
        assert_eq!(both(Some("d"), Some("/h")).home().unwrap(), Path::new("/h/.local/share/Trash"));
        assert!(both(Some("d"), Some("h")).home().is_err());
        assert!(both(None, None).home().is_err());
        assert!(Trash::at("relative/Trash").home().is_err());
        assert_eq!(Trash::at("/x/Bin").home().unwrap(), Path::new("/x/Bin"));
    }

    /// **The check has to be seen finding one.** A volume trash on a directory
    /// above the scratch holds a record for something inside it, which is the
    /// shape `/tmp/.Trash-1000` had — built one level down, inside a scratch of
    /// its own, so no real trash is written to. **And seen not finding one left
    /// before the run began**, which is what a reused pid meets.
    #[test]
    fn a_record_above_the_scratch_for_something_in_it_is_a_stray() {
        let top = scratch("strays");
        let inner = top.join("inner");
        let volume = top.join(format!(".Trash-{}", uid()));
        for directory in [&inner, &volume.join("files"), &volume.join("info")] {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory).unwrap();
        }
        let record = |name: &str, path: &str, when: &str| {
            std::fs::write(
                volume.join("info").join(format!("{name}.trashinfo")),
                format!("[Trash Info]\nPath={path}\nDeletionDate={when}\n"),
            )
            .unwrap();
        };
        let since = "2026-10-26T10:00:00";
        record("elsewhere", "elsewhere/source", since);
        record("earlier", "inner/into/source", "2026-10-25T17:15:24");
        assert!(strays(&inner, since).is_empty(), "a sibling's record, or an older run's");

        record("source", "inner/into/source", since);
        let found = strays(&inner, since);
        assert_eq!(found, vec![volume.join("info/source.trashinfo")], "{found:?}");
        std::fs::remove_dir_all(&top).unwrap();
    }
}
