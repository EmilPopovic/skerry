// SPDX-License-Identifier: Apache-2.0
//! Filesystem-local XDG trash and bounded, reversible recovery records (ADR 0035).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
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
    // SAFETY: geteuid has no pointer arguments or side effects.
    unsafe { libc::geteuid() }
}

/// Configured home trash location. No filesystem access.
pub fn home() -> Result<PathBuf, Problem> {
    if let Some(data) =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
    {
        return Ok(data.join("Trash"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(".local/share/Trash"))
        .ok_or_else(|| failure(Path::new("Trash"), "no absolute home directory configured"))
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

/// Trash on the source mount; no copy/delete fallback. Runs on a filesystem job.
pub fn put(path: &Path) -> Result<Entry, Problem> {
    let original = absolute(path)?;
    let configured = home()?;
    let parent = configured.parent().ok_or_else(|| failure(path, "no trash parent"))?;
    std::fs::create_dir_all(parent).map_err(|e| failure(path, e))?;
    let home = std::fs::canonicalize(parent).map_err(|e| failure(path, e))?.join("Trash");
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

/// Prepare the home Trash place. Runs on a filesystem job.
pub fn home_files() -> Result<PathBuf, Problem> {
    let home = home()?;
    if let Some(parent) = home.parent() {
        std::fs::create_dir_all(parent).map_err(|e| failure(&home, e))?;
    }
    Storage::open(&home, true).map(|s| s.root.join("files"))
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

fn origin(record: &str, root: &Path) -> Result<PathBuf, Problem> {
    let mut lines = record.lines();
    if lines.next() != Some("[Trash Info]") {
        return Err(failure(root, "invalid trash record header"));
    }
    let value = lines
        .find_map(|line| line.strip_prefix("Path="))
        .and_then(decode_path)
        .ok_or_else(|| failure(root, "invalid original path in trash record"))?;
    let home = home()?;
    let home = absolute(&home)?;
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

/// Validate metadata for a direct trash child without following information-file symlinks.
pub fn read(stored: &Path) -> Result<Entry, Problem> {
    let files = stored.parent().ok_or_else(|| failure(stored, "no trash directory"))?;
    if files.file_name() != Some(OsStr::new("files")) {
        return Err(failure(stored, "not a trash entry"));
    }
    let root = files.parent().ok_or_else(|| failure(stored, "no trash root"))?;
    let home_root = absolute(&home()?)?;
    let mounts = mount_paths();
    let valid_volume = mount_of(root, &mounts).is_some_and(|top| {
        root == top.join(format!(".Trash-{}", uid()))
            || shared(top).is_some_and(|shared| root == shared)
    });
    if absolute(root)? != home_root && !valid_volume {
        return Err(failure(stored, "not a recognized trash location"));
    }
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
    let original = origin(&record, root)?;
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

/// Restore without replacing anything; the information record is removed only after moving.
pub fn restore(stored: &Path) -> Result<Entry, Problem> {
    let mut entry = read(stored)?;
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

/// Whether a path lies in a trash namespace. Pure path inspection for UI policy.
#[must_use]
pub fn contains(path: &Path) -> bool {
    if home().is_ok_and(|root| path.starts_with(root)) {
        return true;
    }
    let mine = format!(".Trash-{}", uid());
    let user = uid().to_string();
    path.ancestors().any(|part| {
        part.file_name() == Some(OsStr::new(&mine))
            || (part.file_name() == Some(OsStr::new(&user))
                && part.parent().and_then(Path::file_name) == Some(OsStr::new(".Trash")))
    })
}

pub(crate) fn writable(path: &Path) -> Result<(), Problem> {
    if contains(path) { Err(failure(path, "restore the item before changing it")) } else { Ok(()) }
}
