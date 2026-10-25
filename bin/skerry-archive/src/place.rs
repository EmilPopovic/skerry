// SPDX-License-Identifier: Apache-2.0
//! Where an entry's bytes go, and everything an entry may not do.
//!
//! **[ADR 0032](../../../docs/adr/0032-an-archive-is-unpacked-by-a-child-that-can-only-write-where-the-files-go.md)
//! puts a kernel between an archive and the rest of the disk**, and this is the
//! part above it. Landlock has already made everything but the destination
//! unwritable by the time anything here runs; what is here is the three rules
//! that hold on a kernel too old to have it, and the fourth, which is about
//! rights rather than paths and which Landlock cannot express at all.
//!
//! * **A path with `..` in it, or a leading `/`, is refused** before it is
//!   opened. This is zip-slip, and it is the check every extractor has.
//! * **Every directory on the way is opened with `O_NOFOLLOW`**, one component
//!   at a time from a descriptor on the destination. This is the check most
//!   extractors do not have: an archive can hold a symlink *and then* an entry
//!   underneath it, and a string check on the second path sees nothing wrong
//!   with `link/passwd` at all.
//! * **Every file is created with `O_EXCL`**, so an entry can never write
//!   through something that is already there — including a symlink the same
//!   archive put there a moment ago.
//! * **Nothing arrives with more rights than a file this process wrote itself.**
//!   The mode is the umask's, plus the execute bit where the archive set one.
//!   setuid, setgid and sticky are dropped, and there is no code here that
//!   could make a device node.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, Timespec, Timestamps};
use sk_preview::archive::{Progress, Refusal};

/// How much room must be left on the destination filesystem.
///
/// **Sixty-four megabytes**, and it is the whole bomb defence — ADR 0032. A
/// compression-ratio ceiling was tried and thrown away: a legitimate source
/// tarball measured 174:1, which is inside the range a zip bomb lives in.
const KEEP_FREE: u64 = 64 * 1024 * 1024;

/// How often the filesystem is asked how full it is.
///
/// Every 32 MiB written. A `statvfs` is cheap and this is not: at one check per
/// entry, an archive of a million empty files would ask a million times.
const CHECK_EVERY: u64 = 32 * 1024 * 1024;

/// The most inodes that must be left on the destination filesystem.
///
/// **[`KEEP_FREE`] in four-kilobyte files**, so what is left after an unpack is
/// the same 16,384 small files' worth whichever of the two ran out. Bytes alone
/// never see this: an archive of empty files writes none, and this — not a count
/// of entries — is what stops one, because a backup or a browser's source tarball
/// is honestly hundreds of thousands of files.
///
/// **At most a sixty-fourth of the filesystem's inodes**, because the figure is
/// absolute and some tables are small: `mkfs.ext4 -T largefile4` gives a 64 GiB
/// filesystem 16,384 inodes in all, and a reserve larger than that refused every
/// unpack onto it. See [`reserve`].
const KEEP_FREE_FILES: u64 = KEEP_FREE / 4096;

/// How often the filesystem is asked about inodes, in files, directories and
/// links made.
///
/// **Counted per thing made rather than per entry**, because one entry can make
/// thousands: a pax path may be a mebibyte long, and every directory in it that
/// does not exist yet is an inode.
pub(crate) const MAKE_EVERY: u64 = 1024;

/// The destination, and what has been put in it.
pub struct Place {
    root: OwnedFd,
    /// The last directory walked to, kept because tar entries arrive grouped.
    at: Option<(String, OwnedFd)>,
    /// What has been written.
    pub written: Progress,
    /// Entries this refused to create: a device node, or a path that escapes.
    pub skipped: u64,
    /// Whether the filesystem has been asked at all, which the first entry does.
    checked: bool,
    since_check: u64,
    made_since_check: u64,
    /// How the filesystem is asked: `statvfs`, or a test's filesystem that is
    /// running out.
    measure: fn(BorrowedFd<'_>) -> Result<Left, Refusal>,
}

/// What the destination's filesystem has left, as `statvfs` says it.
#[derive(Clone, Copy, Debug)]
pub struct Left {
    /// Bytes an unprivileged writer may still use.
    pub bytes: u64,
    /// Inodes in all, or zero where the filesystem keeps no count.
    pub files: u64,
    /// Inodes an unprivileged writer may still use.
    pub files_free: u64,
}

impl Place {
    /// Opens `into` as the one place anything may be written.
    pub fn under(into: &Path) -> Result<Self, Refusal> {
        let root = rustix::fs::open(
            into,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Refusal::Unreadable)?;
        Ok(Self {
            root,
            at: None,
            written: Progress::default(),
            skipped: 0,
            checked: false,
            since_check: 0,
            made_since_check: 0,
            measure: measured,
        })
    }

    /// This place, with its filesystem's answers coming from `measure`.
    #[cfg(test)]
    pub fn measured_by(self, measure: fn(BorrowedFd<'_>) -> Result<Left, Refusal>) -> Self {
        Self { measure, ..self }
    }

    /// Makes the directory an entry names, and its parents.
    pub fn directory(&mut self, path: &str) -> Result<(), Refusal> {
        let Some((dir, name)) = inside(path) else {
            self.skipped += 1;
            return Ok(());
        };
        self.room()?;
        if !self.reach(&dir)? {
            self.skipped += 1;
            return Ok(());
        }
        let made =
            rustix::fs::mkdirat(self.there(), name.as_str(), Mode::from_bits_truncate(0o755));
        if made.is_ok() {
            self.made_since_check += 1;
        }
        match made {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {
                self.written.entries += 1;
                Ok(())
            }
            Err(rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT) => Err(Refusal::NoRoom),
            Err(_) => {
                self.skipped += 1;
                Ok(())
            }
        }
    }

    /// Creates the file an entry names, or nothing where it is refused.
    ///
    /// A refusal here is counted and skipped rather than fatal: one entry with an
    /// impossible name in a thousand honest ones is not a reason to stop, and the
    /// count reaches the status line.
    pub fn create(&mut self, path: &str, executable: bool) -> Result<Option<File>, Refusal> {
        let Some((dir, name)) = inside(path) else {
            self.skipped += 1;
            return Ok(None);
        };
        self.room()?;
        if !self.reach(&dir)? {
            self.skipped += 1;
            return Ok(None);
        }
        let mode = if executable { 0o755 } else { 0o644 };
        let opened = rustix::fs::openat(
            self.there(),
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(mode),
        );
        if matches!(opened, Err(rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT)) {
            return Err(Refusal::NoRoom);
        }
        let Ok(fd) = opened else {
            // A name the filesystem refused: too long, or already taken by the
            // `O_EXCL` that is the point of this line.
            self.skipped += 1;
            return Ok(None);
        };
        self.made_since_check += 1;
        Ok(Some(File::from(fd)))
    }

    /// Counts bytes and checks capacity while a single entry is being written.
    pub fn writer(&mut self, file: File) -> Output<'_> {
        Output { place: self, file }
    }

    /// Records what a file cost and stamps it with the time the archive gave it.
    pub fn wrote(&mut self, path: &str, mtime: i64, bytes: u64) {
        self.written.entries += 1;
        let _ = bytes;
        self.stamp(path, mtime);
    }

    /// Makes a symbolic link.
    ///
    /// **Created as it is, including an absolute target.** A symlink is a string
    /// and writing one puts nothing anywhere; what would make it dangerous is
    /// something following it, and nothing here ever does — that is what the
    /// `O_NOFOLLOW` walk and `O_EXCL` above are for.
    pub fn link(&mut self, path: &str, target: &str) -> Result<(), Refusal> {
        let Some((dir, name)) = inside(path) else {
            self.skipped += 1;
            return Ok(());
        };
        if target.is_empty() {
            self.skipped += 1;
            return Ok(());
        }
        self.room()?;
        if !self.reach(&dir)? {
            self.skipped += 1;
            return Ok(());
        }
        let linked = rustix::fs::symlinkat(target, self.there(), name.as_str());
        if matches!(linked, Err(rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT)) {
            return Err(Refusal::NoRoom);
        }
        if linked.is_err() {
            self.skipped += 1;
            return Ok(());
        }
        self.made_since_check += 1;
        self.written.entries += 1;
        Ok(())
    }

    /// Makes a hard link to something already unpacked.
    ///
    /// **Only inside the destination**, which the `..` check guarantees and
    /// Landlock guarantees again: `linkat` needs write access to the target's
    /// directory, so a link to a file outside is refused by the kernel whether or
    /// not this asked for it.
    pub fn hardlink(&mut self, path: &str, target: &str) -> Result<(), Refusal> {
        let (Some((dir, name)), Some((target_dir, target_name))) = (inside(path), inside(target))
        else {
            self.skipped += 1;
            return Ok(());
        };
        if !self.reach(&target_dir)? {
            self.skipped += 1;
            return Ok(());
        }
        let Ok(from) = self.there().try_clone_to_owned() else {
            self.skipped += 1;
            return Ok(());
        };
        if !self.reach(&dir)? {
            self.skipped += 1;
            return Ok(());
        }
        let linked = rustix::fs::linkat(
            from.as_fd(),
            target_name.as_str(),
            self.there(),
            name.as_str(),
            AtFlags::empty(),
        );
        if matches!(linked, Err(rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT)) {
            return Err(Refusal::NoRoom);
        }
        if linked.is_err() {
            self.skipped += 1;
            return Ok(());
        }
        self.written.entries += 1;
        Ok(())
    }

    /// Counts an entry that will not be created at all.
    pub fn refused(&mut self) {
        self.skipped += 1;
    }

    /// Sets an entry's modification time, and shrugs where it cannot.
    ///
    /// **Not a failure.** A time that did not take is a file with today's date on
    /// it, which is worth trying for and not worth stopping an unpack over.
    fn stamp(&mut self, path: &str, mtime: i64) {
        let Some((dir, name)) = inside(path) else { return };
        if !matches!(self.reach(&dir), Ok(true)) {
            return;
        }
        let when = Timespec { tv_sec: mtime, tv_nsec: 0 };
        let times = Timestamps { last_access: when, last_modification: when };
        let _ =
            rustix::fs::utimensat(self.there(), name.as_str(), &times, AtFlags::SYMLINK_NOFOLLOW);
    }

    /// Whether there is still room, asked every [`CHECK_EVERY`] bytes and every
    /// [`MAKE_EVERY`] things made.
    fn room(&mut self) -> Result<(), Refusal> {
        if self.checked && self.since_check < CHECK_EVERY && self.made_since_check < MAKE_EVERY {
            return Ok(());
        }
        self.checked = true;
        self.since_check = 0;
        self.made_since_check = 0;
        if enough((self.measure)(self.root.as_fd())?) { Ok(()) } else { Err(Refusal::NoRoom) }
    }

    /// Makes `dir` under the destination and holds a descriptor on it.
    ///
    /// `Ok(false)` is an entry to skip: a component that is a symlink, or a name
    /// the filesystem refused. `Err` is the destination itself going wrong, which
    /// stops the unpack.
    ///
    /// **One component at a time, each opened `O_NOFOLLOW`.** This is the walk
    /// that makes a symlink planted by an earlier entry useless: the open of the
    /// component that *is* the symlink fails with `ELOOP`, so nothing underneath
    /// it is ever reached — and no string in any later path had to look wrong.
    fn reach(&mut self, dir: &str) -> Result<bool, Refusal> {
        if dir.is_empty() {
            self.at = None;
            return Ok(true);
        }
        if self.at.as_ref().is_some_and(|(held, _)| held == dir) {
            return Ok(true);
        }
        let mut fd: Option<OwnedFd> = None;
        for component in dir.split('/') {
            self.room()?;
            let here = fd.as_ref().map_or_else(|| self.root.as_fd(), AsFd::as_fd);
            match rustix::fs::mkdirat(here, component, Mode::from_bits_truncate(0o755)) {
                Ok(()) => self.made_since_check += 1,
                Err(rustix::io::Errno::EXIST) => {}
                Err(rustix::io::Errno::NOSPC | rustix::io::Errno::DQUOT) => {
                    return Err(Refusal::NoRoom);
                }
                Err(_) => return Ok(false),
            }
            let opened = rustix::fs::openat(
                here,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            );
            match opened {
                Ok(opened) => fd = Some(opened),
                // `ELOOP` here is the symlink attack, and every other error is a
                // directory this cannot enter. Both are the same answer.
                Err(_) => return Ok(false),
            }
        }
        self.at = fd.map(|fd| (dir.to_owned(), fd));
        Ok(self.at.is_some())
    }

    /// The descriptor [`Place::reach`] left, or the destination itself.
    fn there(&self) -> BorrowedFd<'_> {
        self.at.as_ref().map_or_else(|| self.root.as_fd(), |(_, fd)| fd.as_fd())
    }
}

/// A bounded write interval keeps one large entry inside the capacity policy.
pub struct Output<'a> {
    place: &'a mut Place,
    file: File,
}

impl std::io::Write for Output<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.place.room().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::StorageFull, "archive capacity reserve")
        })?;
        let length = bytes.len().min(64 * 1024);
        let written = self.file.write(&bytes[..length])?;
        self.place.since_check += written as u64;
        self.place.written.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// What the destination's filesystem has left, from `statvfs`.
fn measured(root: BorrowedFd<'_>) -> Result<Left, Refusal> {
    let space = rustix::fs::fstatvfs(root).map_err(|_| Refusal::NoRoom)?;
    Ok(Left {
        bytes: space.f_bavail.saturating_mul(space.f_frsize),
        files: space.f_files,
        files_free: space.f_favail,
    })
}

/// The inodes to keep free on a filesystem with `files` of them in all.
fn reserve(files: u64) -> u64 {
    KEEP_FREE_FILES.min(files / 64)
}

/// Whether a filesystem with this much `left` can take another [`CHECK_EVERY`]
/// bytes and [`MAKE_EVERY`] inodes and still keep what must be kept.
///
/// **A filesystem that counts no inodes has none to run out of.** vfat and exfat
/// report zero files in all — measured on this machine — and so does btrfs, and
/// reading that as none free would refuse every unpack onto them.
fn enough(left: Left) -> bool {
    left.bytes >= KEEP_FREE + CHECK_EVERY
        && (left.files == 0 || left.files_free >= reserve(left.files) + MAKE_EVERY)
}

/// An archive's path as this program will treat it: no `./` in front, no
/// separator behind.
///
/// **`tar czf x.tgz .` is the commonest tarball there is**, and every entry in
/// one is `./something`. Left alone, that is a folder called `.` in the panel
/// with the whole archive inside it — and, worse, an unpack that writes nothing
/// at all, because [`inside`] refuses a `.` component and is right to. The place
/// to fix it is here, where a path is read, rather than in either of the two
/// readers or in the writer: one rule, applied once.
pub fn tidy(path: &str) -> String {
    let mut rest = path;
    while let Some(shorter) = rest.strip_prefix("./") {
        rest = shorter;
    }
    // A bare `.` is the archive's own root, which is not an entry: it comes back
    // empty and both readers drop it.
    if rest == "." {
        return String::new();
    }
    rest.trim_end_matches('/').to_owned()
}

/// A path an archive gave, split into a directory and a name, or nothing where
/// it is not a path this will write.
///
/// **Refused: absolute, empty, `.`, `..` anywhere in it, and a NUL.** Not
/// sanitised — an entry whose name is repaired is an entry that lands somewhere
/// nobody asked for, and the count of what was skipped is the honest answer.
fn inside(path: &str) -> Option<(String, String)> {
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
        return None;
    }
    let (name, dir) = parts.split_last()?;
    Some((dir.join("/"), (*name).to_owned()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The prefix every `tar czf x.tgz .` puts on every entry.
    #[test]
    fn a_leading_dot_is_not_a_folder() {
        assert_eq!(tidy("./a.txt"), "a.txt");
        assert_eq!(tidy("././deep/b.txt"), "deep/b.txt");
        assert_eq!(tidy("."), "");
        assert_eq!(tidy("./"), "");
        assert_eq!(tidy("deep/"), "deep");
        // And a name that merely begins with a dot is a hidden file, not a
        // prefix: taking two characters off this one would rename it.
        assert_eq!(tidy(".bashrc"), ".bashrc");
        assert_eq!(tidy("./.config/x"), ".config/x");
    }

    /// The four shapes of the oldest bug in archive handling.
    #[test]
    fn a_path_that_escapes_is_not_a_path() {
        for escape in [
            "../outside",
            "a/../../outside",
            "/etc/passwd",
            "/",
            "..",
            "a/..",
            "",
            "a//b",
            "./a",
            "a/./b",
        ] {
            assert!(inside(escape).is_none(), "{escape} was let through");
        }
    }

    /// And what an ordinary entry splits into.
    #[test]
    fn an_ordinary_path_is_a_directory_and_a_name() {
        assert_eq!(inside("notes.txt"), Some((String::new(), "notes.txt".to_owned())));
        assert_eq!(inside("a/b/c.rs"), Some(("a/b".to_owned(), "c.rs".to_owned())));
        // A name that merely *contains* two dots is a name, not an escape.
        assert_eq!(inside("a/..hidden"), Some(("a".to_owned(), "..hidden".to_owned())));
    }

    /// The symlink attack the `O_NOFOLLOW` walk exists for, as a filesystem test:
    /// an entry that would land under a symlink lands nowhere at all.
    #[test]
    fn nothing_is_written_through_a_symlink() {
        let root = std::env::temp_dir().join(format!("skerry-place-{}", std::process::id()));
        let outside = root.join("outside");
        let inside_dir = root.join("into");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&inside_dir).unwrap();
        // What the archive's first entry would have made.
        std::os::unix::fs::symlink(&outside, inside_dir.join("link")).unwrap();

        let mut place = Place::under(&inside_dir).unwrap();
        let opened = place.create("link/passwd", false).unwrap();
        assert!(opened.is_none(), "a file was created through a symlink");
        assert_eq!(place.skipped, 1);
        assert!(!outside.join("passwd").exists(), "it landed outside the destination");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn output_is_counted_and_capacity_rechecked_before_an_entry_finishes() {
        use std::io::Write as _;
        let root = std::env::temp_dir().join(format!("skerry-place-stream-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut place = Place::under(&root).unwrap();
        let file = place.create("file", false).unwrap().unwrap();
        let mut output = place.writer(file);
        output.write_all(&[42; 4096]).unwrap();
        assert_eq!(output.place.written.bytes, 4096);
        assert_eq!(output.place.written.entries, 0);
        output.place.since_check = super::CHECK_EVERY;
        output.write_all(&[42; 4096]).unwrap();
        assert_eq!(output.place.since_check, 4096);
        assert_eq!(output.place.written.bytes, 8192);
        drop(output);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// **Inodes run out as surely as bytes do**, and an archive of empty files
    /// spends nothing else.
    #[test]
    fn a_filesystem_out_of_inodes_is_out_of_room_with_bytes_to_spare() {
        let left = |bytes, files, files_free| Left { bytes, files, files_free };
        let plenty = 1 << 40;
        assert!(enough(left(plenty, 1_000_000, 500_000)));
        assert!(!enough(left(plenty, 1_000_000, KEEP_FREE_FILES)), "the last inodes were spent");
        // A filesystem that keeps no count is never out of what it does not count.
        assert!(enough(left(plenty, 0, 0)));
        // And bytes still bind on their own.
        assert!(!enough(left(KEEP_FREE, 1_000_000, 500_000)));
    }

    /// **A small inode table keeps a proportionate reserve**, not all of itself:
    /// 64 GiB under `mkfs.ext4 -T largefile4` is 16,384 inodes in all, which an
    /// absolute reserve of 16,384 refused every unpack onto however empty it was.
    #[test]
    fn a_small_inode_table_is_not_all_reserve() {
        let largefile4 = Left { bytes: 1 << 36, files: 16_384, files_free: 16_373 };
        assert_eq!(reserve(largefile4.files), 256);
        assert!(enough(largefile4), "an empty filesystem refused an unpack");
        assert_eq!(reserve(92_061_696), KEEP_FREE_FILES, "a large table keeps the whole reserve");
    }

    /// **Making things asks the filesystem again without a byte written**, and a
    /// deep path counts every directory it makes — the two shapes of archive the
    /// byte cadence alone never rechecks.
    #[test]
    fn making_things_rechecks_room_without_writing_a_byte() {
        let root = std::env::temp_dir().join(format!("skerry-place-made-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut place = Place::under(&root).unwrap();
        place.directory("a").unwrap();
        assert!(place.checked, "the first entry did not ask");
        assert_eq!(place.made_since_check, 1);

        place.made_since_check = MAKE_EVERY;
        place.directory("b").unwrap();
        assert_eq!(place.made_since_check, 1, "a thousand things made and nobody asked");

        let file = place.create("c/d/e/f.txt", false).unwrap();
        assert!(file.is_some());
        assert_eq!(place.made_since_check, 5, "three directories and a file are four inodes");
        std::fs::remove_dir_all(root).unwrap();
    }
}
