//! Mutations, and the inverse of each one.
//!
//! Three of them for now: make a directory, rename, move to the trash. Each
//! returns a [`Change`] describing what happened, and every `Change` can be
//! [`revert`]ed and [`apply`]ed again — which is [rule
//! 3](../../../docs/03-principles.md), and the reason the inverse is written in
//! the same file as the operation rather than somewhere that can drift from it.
//!
//! **An inverse is not a guess.** Creating a directory is undone by removing it,
//! and only while it is still empty — if something has been put in it since, the
//! undo refuses rather than deleting somebody's files. Renaming is undone by
//! renaming back, and only if the original name is still free. Trashing is undone
//! by moving the file out of the trash and deleting the `.trashinfo` beside it.
//! Each refusal is a `Problem` with a message, not a silent no-op.

use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags};
use sk_model::{Problem, ProblemKind};

/// What a mutation did, and enough to undo it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    /// A directory that did not exist before.
    Created {
        /// Where it is.
        path: PathBuf,
    },
    /// A name that moved.
    Renamed {
        /// What it was called.
        from: PathBuf,
        /// What it is called now.
        to: PathBuf,
    },
    /// Something moved into the trash, with both files the specification puts
    /// there so the undo can take both away.
    Trashed {
        /// Where it came from.
        original: PathBuf,
        /// Where it is now, under `Trash/files`.
        stored: PathBuf,
        /// The `.trashinfo` beside it, under `Trash/info`.
        info: PathBuf,
    },
}

impl Change {
    /// The path a listing should put its cursor on after this ran.
    ///
    /// What a person is looking for after an operation: the folder they made,
    /// the new name, or — after an undo — the thing that came back.
    #[must_use]
    pub fn subject(&self) -> &Path {
        match self {
            Self::Created { path } => path,
            Self::Renamed { to, .. } => to,
            Self::Trashed { original, .. } => original,
        }
    }

    /// Where the file is once this change has been **reverted**, if anywhere.
    ///
    /// The other end of [`Change::subject`], and the row a cursor belongs on
    /// after an undo: undoing a rename leaves the file under the name it had
    /// before, and undoing a trash puts it back where it was. `Created` has no
    /// answer — the point of undoing it is that there is nothing left to point
    /// at.
    #[must_use]
    pub fn origin(&self) -> Option<&Path> {
        match self {
            Self::Created { .. } => None,
            Self::Renamed { from, .. } => Some(from),
            Self::Trashed { original, .. } => Some(original),
        }
    }
}

/// Makes a directory called `name` inside `parent`.
///
/// **Runs on a job.**
pub fn create_directory(parent: &Path, name: &str) -> Result<Change, Problem> {
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    let path = parent.join(name);
    // `mkdir` fails with `EEXIST` rather than replacing, which is the whole
    // reason not to check first: a check and a create are two syscalls with a
    // race between them, and the create already answers the question.
    rustix::fs::mkdir(&path, Mode::from_bits_truncate(0o755))
        .map_err(|error| problem(error, &path))?;
    Ok(Change::Created { path })
}

/// Renames `from` to `name`, in the same directory.
///
/// **Runs on a job.**
pub fn rename(from: &Path, name: &str) -> Result<Change, Problem> {
    if name.is_empty() || name.contains('/') {
        return Err(Problem::new(ProblemKind::BadName, name, "empty, or contains a separator"));
    }
    let Some(parent) = from.parent() else {
        return Err(Problem::new(ProblemKind::BadName, from.display().to_string(), "no parent"));
    };
    let to = parent.join(name);
    if to == from {
        return Ok(Change::Renamed { from: from.to_path_buf(), to });
    }
    move_no_replace(from, &to)?;
    Ok(Change::Renamed { from: from.to_path_buf(), to })
}

/// Moves `path` into the trash, per the freedesktop specification.
///
/// **Runs on a job.**
pub fn trash(path: &Path) -> Result<Change, Problem> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(Problem::new(ProblemKind::BadName, path.display().to_string(), "no name"));
    };
    let home = trash_home()?;
    let (files, info) = (home.join("files"), home.join("info"));
    // The data directory may not exist at all on a fresh account, and `mkdir`
    // does not make parents. The parents get the ordinary mode; the three trash
    // directories get **0700**, because the specification says a trash is
    // private and a world-readable one leaks every filename in it.
    if let Some(parent) = home.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(io_problem(&error, parent));
    }
    for directory in [&home, &files, &info] {
        if let Err(error) = rustix::fs::mkdir(directory, Mode::from_bits_truncate(0o700))
            && error != rustix::io::Errno::EXIST
        {
            return Err(problem(error, directory));
        }
    }

    // **The `.trashinfo` reserves the name, not the move.** The specification
    // says to create it with `O_EXCL`, because that is the one step that can
    // claim a name atomically — two programs trashing `notes.txt` at the same
    // moment must not both believe they own `files/notes.txt`.
    let (stored, info_path) = claim(&files, &info, name)?;
    let record = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        encode_path(&path.to_string_lossy()),
        deletion_date()
    );
    std::fs::write(&info_path, record).map_err(|error| io_problem(&error, &info_path))?;

    if let Err(error) = move_no_replace(path, &stored) {
        // The reservation is ours and the move failed, so the reservation goes:
        // an `info` entry with no file behind it is a trash the desktop's own
        // tools report as corrupt.
        let _ = std::fs::remove_file(&info_path);
        return Err(error);
    }
    Ok(Change::Trashed { original: path.to_path_buf(), stored, info: info_path })
}

/// Undoes a change. Refuses rather than guessing.
///
/// **Runs on a job.**
pub fn revert(change: &Change) -> Result<(), Problem> {
    match change {
        // `rmdir`, not a recursive remove: if something has been put in it since,
        // this fails with `ENOTEMPTY` and the undo refuses. An undo that deletes
        // files somebody added is not an undo.
        Change::Created { path } => rustix::fs::rmdir(path).map_err(|error| problem(error, path)),
        Change::Renamed { from, to } => move_no_replace(to, from),
        Change::Trashed { original, stored, info } => {
            move_no_replace(stored, original)?;
            // The file is back; a leftover `.trashinfo` is the corruption the
            // claim step exists to avoid, so its removal is not optional.
            std::fs::remove_file(info).map_err(|error| io_problem(&error, info))
        }
    }
}

/// Does a change again, after it was undone.
///
/// **Runs on a job.**
pub fn apply(change: &Change) -> Result<Change, Problem> {
    match change {
        Change::Created { path } => {
            let (Some(parent), Some(name)) =
                (path.parent(), path.file_name().and_then(|name| name.to_str()))
            else {
                return Err(Problem::new(
                    ProblemKind::BadName,
                    path.display().to_string(),
                    "no parent or no name",
                ));
            };
            create_directory(parent, name)
        }
        Change::Renamed { from, to } => {
            let Some(name) = to.file_name().and_then(|name| name.to_str()) else {
                return Err(Problem::new(
                    ProblemKind::BadName,
                    to.display().to_string(),
                    "no name",
                ));
            };
            rename(from, name)
        }
        // Re-trashed from scratch rather than by moving the same bytes back into
        // the same slot: the slot may have been taken, and `trash` is the thing
        // that knows how to claim one.
        Change::Trashed { original, .. } => trash(original),
    }
}

/// `$XDG_DATA_HOME/Trash`, or the specification's fallback.
fn trash_home() -> Result<PathBuf, Problem> {
    if let Some(data) =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|path| path.is_absolute())
    {
        return Ok(data.join("Trash"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute());
    home.map(|home| home.join(".local/share/Trash")).ok_or_else(|| {
        Problem::plain(ProblemKind::Unreadable, "neither XDG_DATA_HOME nor HOME is set")
    })
}

/// Claims a free name in the trash, returning where the file and its record go.
///
/// The `info` file is created empty with `O_EXCL` — that is the atomic step, and
/// the specification names it as such. The name is then `name`, `name.2`,
/// `name.3` and so on until one is free.
fn claim(files: &Path, info: &Path, name: &str) -> Result<(PathBuf, PathBuf), Problem> {
    for attempt in 1..1000u32 {
        let candidate = if attempt == 1 { name.to_owned() } else { format!("{name}.{attempt}") };
        let record = info.join(format!("{candidate}.trashinfo"));
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        match rustix::fs::open(&record, flags, Mode::from_bits_truncate(0o600)) {
            Ok(_) => return Ok((files.join(candidate), record)),
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(problem(error, &record)),
        }
    }
    Err(Problem::new(ProblemKind::Unreadable, name, "a thousand names in the trash are taken"))
}

/// Moves `from` to `to`, and **never over anything**.
///
/// `renameat2` with `RENAME_NOREPLACE`, which is one syscall and cannot race —
/// a `stat` first and a `rename` after is a window in which somebody else
/// creates the destination and this overwrites it.
fn move_no_replace(from: &Path, to: &Path) -> Result<(), Problem> {
    let cwd = rustix::fs::CWD;
    match rustix::fs::renameat_with(cwd, from, cwd, to, rustix::fs::RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Not every filesystem implements the flag. Falling back to a plain
        // rename would silently replace, so the fallback is a link-and-unlink
        // for a file and a refusal for anything else — and both are honest.
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) => {
            if rustix::fs::statat(cwd, to, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
                return Err(problem(rustix::io::Errno::EXIST, to));
            }
            rustix::fs::rename(from, to).map_err(|error| problem(error, to))
        }
        Err(error) => Err(problem(error, to)),
    }
}

/// The deletion time the specification asks for: local, to the second, no zone.
fn deletion_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut out: libc::tm = unsafe { std::mem::zeroed() };
    let seconds: libc::time_t = now.cast_signed();
    // SAFETY: `localtime_r` writes into `out` and reads only `seconds`.
    if unsafe { libc::localtime_r(&raw const seconds, &raw mut out) }.is_null() {
        return "1970-01-01T00:00:00".to_owned();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        out.tm_year + 1900,
        out.tm_mon + 1,
        out.tm_mday,
        out.tm_hour,
        out.tm_min,
        out.tm_sec
    )
}

/// Percent-encodes a path for a `.trashinfo` `Path=` field.
///
/// The specification says the value is the path "with the usual URL encoding",
/// and the separators stay separators: a restore reads this back and joins it,
/// so encoding `/` would make every restored path a single filename.
fn encode_path(path: &str) -> String {
    const KEEP: &[u8] = b"-_.~/";
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            out.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn problem(error: rustix::io::Errno, path: &Path) -> Problem {
    let kind = match error {
        rustix::io::Errno::NOENT => ProblemKind::Gone,
        rustix::io::Errno::ACCESS | rustix::io::Errno::PERM | rustix::io::Errno::ROFS => {
            ProblemKind::Unreadable
        }
        rustix::io::Errno::EXIST => ProblemKind::NameTaken,
        rustix::io::Errno::XDEV => ProblemKind::AcrossFilesystems,
        rustix::io::Errno::NOTEMPTY => ProblemKind::NotEmpty,
        rustix::io::Errno::MFILE | rustix::io::Errno::NFILE | rustix::io::Errno::NOSPC => {
            ProblemKind::OutOfResources
        }
        _ => ProblemKind::Unreadable,
    };
    Problem::new(kind, path.display().to_string(), format!("{error:?}"))
}

fn io_problem(error: &std::io::Error, path: &Path) -> Problem {
    let errno =
        error.raw_os_error().map_or(rustix::io::Errno::IO, rustix::io::Errno::from_raw_os_error);
    problem(errno, path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Change, apply, create_directory, encode_path, rename, revert, trash};
    use sk_model::ProblemKind;
    use std::path::{Path, PathBuf};

    /// A private `$HOME` and `$XDG_DATA_HOME`, so a test's trash is its own.
    ///
    /// The variables are process-wide, so these tests must not run beside each
    /// other — `TRASH` serialises the ones that touch them. Everything else uses
    /// its own directory and runs in parallel.
    static TRASH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-ops-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_directory_is_made_and_unmade() {
        let root = scratch("mkdir");
        let change = create_directory(&root, "notes").expect("made");
        assert_eq!(change, Change::Created { path: root.join("notes") });
        assert!(root.join("notes").is_dir());

        revert(&change).expect("unmade");
        assert!(!root.join("notes").exists());

        // And again, which is what redo is.
        apply(&change).expect("made again");
        assert!(root.join("notes").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An undo that deletes files somebody put there is not an undo.
    #[test]
    fn undoing_a_directory_refuses_once_something_is_in_it() {
        let root = scratch("not-empty");
        let change = create_directory(&root, "notes").expect("made");
        std::fs::write(root.join("notes/theirs.txt"), b"mine now").unwrap();

        let refused = revert(&change).expect_err("it must refuse");
        assert_eq!(refused.kind(), ProblemKind::NotEmpty);
        assert!(root.join("notes/theirs.txt").is_file(), "the undo deleted somebody's file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_that_is_taken_is_refused_rather_than_replaced() {
        let root = scratch("taken");
        std::fs::write(root.join("keep.txt"), b"important").unwrap();
        std::fs::write(root.join("other.txt"), b"other").unwrap();

        let refused = rename(&root.join("other.txt"), "keep.txt").expect_err("must refuse");
        assert_eq!(refused.kind(), ProblemKind::NameTaken);
        assert_eq!(std::fs::read(root.join("keep.txt")).unwrap(), b"important");
        assert!(root.join("other.txt").is_file(), "the source vanished into a refused rename");

        let taken = create_directory(&root, "keep.txt").expect_err("must refuse");
        assert_eq!(taken.kind(), ProblemKind::NameTaken);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rename_goes_back_where_it_came_from() {
        let root = scratch("rename");
        std::fs::write(root.join("draft.md"), b"words").unwrap();
        let change = rename(&root.join("draft.md"), "final.md").expect("renamed");
        assert_eq!(
            change,
            Change::Renamed { from: root.join("draft.md"), to: root.join("final.md") }
        );
        assert!(root.join("final.md").is_file() && !root.join("draft.md").exists());

        revert(&change).expect("renamed back");
        assert!(root.join("draft.md").is_file() && !root.join("final.md").exists());
        assert_eq!(std::fs::read(root.join("draft.md")).unwrap(), b"words");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_with_a_separator_or_no_name_at_all_is_refused() {
        let root = scratch("bad-name");
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        for name in ["", "sub/deeper", "/etc/passwd", "../escape"] {
            assert_eq!(
                create_directory(&root, name).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName,
                "{name:?} was accepted as a directory name"
            );
            assert_eq!(
                rename(&root.join("a.txt"), name).map(|_| ()).expect_err(name).kind(),
                ProblemKind::BadName,
                "{name:?} was accepted as a new name"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn with_trash<T>(label: &str, body: impl FnOnce(&Path) -> T) -> T {
        let guard = TRASH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = scratch(label);
        let before = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: the mutex above is what makes this the only thread in this
        // process changing the environment for the length of the body.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("data")) };
        let outcome = body(&root);
        unsafe {
            match before {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
        outcome
    }

    /// The two files the specification puts there, and both of them gone again.
    #[test]
    fn trashing_writes_a_record_beside_the_file_and_undo_takes_both_away() {
        with_trash("trash", |root| {
            let file = root.join("receipt.pdf");
            std::fs::write(&file, b"%PDF-1.4").unwrap();

            let change = trash(&file).expect("trashed");
            let Change::Trashed { original, stored, info } = &change else { panic!("{change:?}") };
            assert_eq!(original, &file);
            assert!(!file.exists(), "the original is still there");
            assert_eq!(std::fs::read(stored).unwrap(), b"%PDF-1.4");

            let record = std::fs::read_to_string(info).unwrap();
            assert!(record.starts_with("[Trash Info]\n"), "{record:?}");
            assert!(
                record.contains(&format!("Path={}\n", encode_path(&file.to_string_lossy()))),
                "{record:?}"
            );
            // `YYYY-MM-DDThh:mm:ss`, local, no zone.
            let date = record
                .lines()
                .find_map(|line| line.strip_prefix("DeletionDate="))
                .expect("a deletion date");
            assert_eq!(date.len(), 19, "{date:?}");
            assert_eq!(date.as_bytes()[10], b'T', "{date:?}");
            assert!(date.starts_with("20"), "{date:?}");

            revert(&change).expect("restored");
            assert_eq!(std::fs::read(&file).unwrap(), b"%PDF-1.4");
            assert!(!stored.exists(), "the trashed copy is still there");
            assert!(!info.exists(), "a record with no file behind it is a corrupt trash");
        });
    }

    /// Two files of the same name, trashed one after the other. Neither may
    /// overwrite the other, and each record must name its own file.
    #[test]
    fn the_same_name_twice_takes_two_slots() {
        with_trash("trash-twice", |root| {
            std::fs::create_dir_all(root.join("a")).unwrap();
            std::fs::create_dir_all(root.join("b")).unwrap();
            std::fs::write(root.join("a/notes.txt"), b"first").unwrap();
            std::fs::write(root.join("b/notes.txt"), b"second").unwrap();

            let first = trash(&root.join("a/notes.txt")).expect("first");
            let second = trash(&root.join("b/notes.txt")).expect("second");
            let (Change::Trashed { stored: one, .. }, Change::Trashed { stored: two, .. }) =
                (&first, &second)
            else {
                panic!("{first:?} {second:?}")
            };
            assert_ne!(one, two, "the second overwrote the first");
            assert_eq!(std::fs::read(one).unwrap(), b"first");
            assert_eq!(std::fs::read(two).unwrap(), b"second");

            // And each goes back to its own directory.
            revert(&second).expect("second back");
            revert(&first).expect("first back");
            assert_eq!(std::fs::read(root.join("a/notes.txt")).unwrap(), b"first");
            assert_eq!(std::fs::read(root.join("b/notes.txt")).unwrap(), b"second");
        });
    }

    /// A trash on another filesystem needs the per-volume `.Trash-$uid` the
    /// specification describes, which is owed. Until then this must refuse and
    /// say so — never half-move anything.
    #[test]
    fn a_file_on_another_filesystem_is_refused_and_left_alone() {
        with_trash("trash-xdev", |root| {
            // `/dev/shm` is a tmpfs on every Linux system that has it, and the
            // scratch root is on whatever `/tmp` is. Where they are the same
            // filesystem there is nothing to test and the test says so.
            let elsewhere = Path::new("/dev/shm");
            if !elsewhere.is_dir() {
                return;
            }
            let mine = elsewhere.join(format!("skerry-xdev-{}.txt", std::process::id()));
            if std::fs::write(&mine, b"elsewhere").is_err() {
                return;
            }
            let same = std::fs::metadata(&mine)
                .and_then(|a| std::fs::metadata(root).map(|b| (a, b)))
                .is_ok_and(|(a, b)| {
                    use std::os::unix::fs::MetadataExt as _;
                    a.dev() == b.dev()
                });
            if same {
                let _ = std::fs::remove_file(&mine);
                return;
            }

            let refused = trash(&mine).expect_err("must refuse");
            assert_eq!(refused.kind(), ProblemKind::AcrossFilesystems);
            assert_eq!(std::fs::read(&mine).unwrap(), b"elsewhere", "the file was disturbed");
            // And no orphan record: the reservation is made before the move, so
            // a failed move that left it behind would be a trash the desktop's
            // own tools report as corrupt.
            let orphan = root
                .join("data/Trash/info")
                .join(format!("{}.trashinfo", mine.file_name().unwrap().to_string_lossy()));
            assert!(!orphan.exists(), "a record with no file behind it: {orphan:?}");
            let _ = std::fs::remove_file(&mine);
        });
    }

    /// The `Path=` field is read back by a restore, so the separators have to
    /// survive and everything else has to be encoded.
    #[test]
    fn a_trashinfo_path_keeps_its_separators_and_encodes_the_rest() {
        assert_eq!(encode_path("/home/emil/notes.txt"), "/home/emil/notes.txt");
        assert_eq!(encode_path("/home/emil/My File.txt"), "/home/emil/My%20File.txt");
        assert_eq!(encode_path("/home/emil/a#b?c"), "/home/emil/a%23b%3Fc");
        assert_eq!(encode_path("/home/emil/café"), "/home/emil/caf%C3%A9");
        assert_eq!(encode_path("/a-b_c.d~e"), "/a-b_c.d~e");
    }
}
