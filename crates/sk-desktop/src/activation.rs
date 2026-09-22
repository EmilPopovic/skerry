// SPDX-License-Identifier: Apache-2.0
//! The file that lets the desktop start this program when none of it is running.
//!
//! [`crate::bus`] answers `org.freedesktop.FileManager1` while a window is open.
//! A browser's *Open containing folder* with nothing open is a different
//! question: the bus has to know what to **start**, and that is a
//! `org.freedesktop.FileManager1.service` file naming an executable.
//!
//! # Why this file is written by hand and not by a package
//!
//! The obvious place is `$PREFIX/share/dbus-1/services/`, and it is taken. On
//! the machine this was written on, `/usr/share/dbus-1/services/` already holds
//! that name with `Exec=/usr/bin/nautilus --gapplication-service` in it, from
//! Nautilus's own package — and two packages cannot own one path. Worse, that
//! directory is scanned whatever `$XDG_DATA_DIRS` says, because `session.conf`
//! names the install prefix's service directory in its own right, so it cannot
//! be arranged around.
//!
//! **`$XDG_DATA_HOME` wins over it, and that is measured.** With both files
//! present and each naming a different program, `dbus-daemon` activated the one
//! in `$XDG_DATA_HOME`; with only the system one present it activated that. So a
//! file here states a preference without replacing anything, which is the shape
//! [`crate::mimeapps`] already has for `mimeapps.list` and
//! [`crate::places`] for GTK's `bookmarks`.
//!
//! It is also the same judgement the running program already makes: `sk-app`
//! asks for `FileManager1` **without replacing an owner**, because taking it
//! from a file manager that is running is not ours to do. A `.service` file in a
//! package would do exactly that at install time, to every machine, before
//! anybody had chosen anything.
//!
//! # The grammar
//!
//! Three lines, and the specification fixes all three: a `[D-BUS Service]`
//! group, `Name`, and `Exec`. There is nothing here that belongs to somebody
//! else, so there is nothing to preserve — the discipline `mimeapps.list` needs
//! becomes one rule instead: a file whose `Exec` names a program that is not a
//! `skerry` is somebody's own choice and is refused rather than replaced.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The name the desktop activates, which is what this file is about.
const NAME: &str = "org.freedesktop.FileManager1";

/// The file's own basename, which the bus derives the name from.
const FILE: &str = "org.freedesktop.FileManager1.service";

/// The group the two keys live under.
const GROUP: &str = "[D-BUS Service]";

/// The program a file we are allowed to replace runs.
const OURS: &str = "skerry";

/// Why the file was not written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// There is no `$XDG_DATA_HOME` and no `$HOME`, so there is nowhere to put
    /// it.
    NoDataHome,
    /// A file is already there and it starts something else, named here.
    ///
    /// **Refusing is the whole point.** Only one program can be what the desktop
    /// starts, and the one named in a person's own data directory was put there
    /// on purpose.
    Taken(String),
    /// This program's own path cannot go into an `Exec` value — `/proc/self/exe`
    /// could not be read, or the path holds a quote or a newline.
    Unnameable,
    /// The write or the removal itself failed — a full disk, a read-only home.
    NotWritten,
}

/// Where the file goes.
///
/// `$XDG_DATA_HOME` and never one of `$XDG_DATA_DIRS`: the others are the
/// distribution's, and writing into them is the collision this whole module
/// exists to avoid.
#[must_use]
pub fn place() -> Option<PathBuf> {
    crate::data_home().map(|dir| dir.join("dbus-1/services").join(FILE))
}

/// Whether the installed file starts exactly this program.
///
/// **Exactly, not "a skerry".** A file naming a binary that has since been moved
/// or uninstalled is activation that fails, and a settings row that said *yes*
/// about it would be stating something untrue — which [`announce`] then puts
/// right by rewriting it.
///
/// **Runs on a job**: a `readlink` on `/proc/self/exe` and a read.
#[must_use]
pub fn is_ours() -> bool {
    let (Some(path), Some(program)) = (place(), this_program()) else { return false };
    let Some(text) = crate::read(&path) else { return false };
    exec_of(&text).is_some_and(|exec| Path::new(&exec) == program)
}

/// Writes the file, so the desktop starts this program for `FileManager1`.
///
/// **Runs on a job**: a `readlink`, a read and a write.
pub fn announce() -> Result<PathBuf, Refused> {
    let path = place().ok_or(Refused::NoDataHome)?;
    let program = this_program().ok_or(Refused::Unnameable)?;
    let exec = quoted(&program)?;
    if let Some(held) = crate::read(&path).as_deref().and_then(exec_of)
        && !starts_a_skerry(&held)
    {
        return Err(Refused::Taken(held));
    }
    let text = format!("{GROUP}\nName={NAME}\nExec={exec}\n");
    if sk_fs::atomically(&path, &text) { Ok(path) } else { Err(Refused::NotWritten) }
}

/// Takes the file away again, so the desktop starts whatever it did before.
///
/// A file that is not ours is left where it is and reported as taken: the
/// postcondition — *nothing here starts Skerry* — already holds, and deleting
/// somebody else's choice to say so would be the hijack in reverse.
pub fn withdraw() -> Result<PathBuf, Refused> {
    let path = place().ok_or(Refused::NoDataHome)?;
    match crate::read(&path).as_deref().and_then(exec_of) {
        None => Ok(path),
        Some(held) if !starts_a_skerry(&held) => Err(Refused::Taken(held)),
        Some(_) => match std::fs::remove_file(&path) {
            Ok(()) => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(_) => Err(Refused::NotWritten),
        },
    }
}

/// Asks the bus to read its service directories again. Whether it did.
///
/// **Because the bus caches what it found at startup, and the directory this
/// file goes into did not exist then.** Measured, with the call removed: a bus
/// that already knew `org.freedesktop.FileManager1` from `/usr/share` started
/// `/usr/bin/nautilus --gapplication-service` for *show me this file*, with our
/// file in place beside it. So a reload that does not happen is not a choice that
/// arrives late — it is the other file manager opening, until the next login.
///
/// **Runs on a job**: a connect and a call.
#[must_use]
pub fn rescan() -> bool {
    let Some(mut bus) = crate::bus::Bus::session() else { return false };
    let call = crate::bus::Message::call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ReloadConfig",
        Vec::new(),
    );
    bus.call(call).is_some()
}

/// Whether the program an `Exec` names is a `skerry` — this one or a stale copy
/// of it, which [`announce`] may replace and nothing else may.
fn starts_a_skerry(program: &str) -> bool {
    Path::new(program).file_name().and_then(OsStr::to_str) == Some(OURS)
}

/// This program's path, as the desktop should start it.
///
/// **Not `current_exe` as it comes.** That is `readlink /proc/self/exe`, and once
/// the running binary has been replaced the kernel appends ` (deleted)` to the
/// link — and `make install` replaces it: measured with uutils `install` 0.8.0, a
/// binary installed over while it runs reads back with the suffix. An `Exec`
/// written from the raw link names a file that does not exist and never
/// activates, and a row asked whether it is ours would say *no* for the rest of
/// the process's life. The file now at the un-suffixed path is the reinstalled
/// program, which is the one to start.
fn this_program() -> Option<PathBuf> {
    let link = std::env::current_exe().ok()?;
    Some(without_deleted(&link))
}

/// `link` with the kernel's ` (deleted)` suffix taken off, if it has one.
fn without_deleted(link: &Path) -> PathBuf {
    match link.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        Some(stem) => PathBuf::from(OsStr::from_bytes(stem)),
        None => link.to_path_buf(),
    }
}

/// The program an `Exec` value starts, with one level of quoting removed.
///
/// Only the first word, because that is the executable and the rest is its
/// arguments — `Exec=/usr/bin/nautilus --gapplication-service` names Nautilus.
fn exec_of(text: &str) -> Option<String> {
    let mut group = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|it| it.strip_suffix(']')) {
            name.clone_into(&mut group);
            continue;
        }
        let Some(value) = line.strip_prefix("Exec=") else { continue };
        if format!("[{group}]") != GROUP {
            continue;
        }
        let value = value.trim();
        return Some(match value.strip_prefix('\'') {
            Some(rest) => rest.split('\'').next().unwrap_or(rest).to_owned(),
            None => value.split_whitespace().next().unwrap_or(value).to_owned(),
        });
    }
    None
}

/// `program` as an `Exec` value.
///
/// **Always quoted, because `dbus-daemon` splits the value into words.**
/// Measured against it: a service file whose `Exec` is an unquoted path with a
/// space in it is never activated, and the same path in single quotes is. Single
/// quotes are literal all the way to the closing one, which is why a path
/// holding one has no spelling here at all.
fn quoted(program: &Path) -> Result<String, Refused> {
    let text = program.to_str().ok_or(Refused::Unnameable)?;
    if text.contains('\'') || text.contains('\n') {
        return Err(Refused::Unnameable);
    }
    Ok(format!("'{text}'"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::path::Path;

    use super::{FILE, GROUP, NAME, Refused, exec_of, quoted, without_deleted};

    /// The file a fresh machine gets, read back by the parser that decides
    /// whether it is ours — the round trip a settings row depends on.
    #[test]
    fn the_file_we_write_names_the_program_we_wrote_it_for() {
        let program = Path::new("/usr/local/bin/skerry");
        let exec = quoted(program).expect("a path with no quote in it");
        let text = format!("{GROUP}\nName={NAME}\nExec={exec}\n");
        assert_eq!(exec_of(&text).as_deref(), Some("/usr/local/bin/skerry"));
        assert!(text.starts_with("[D-BUS Service]\n"));
        assert_eq!(FILE, "org.freedesktop.FileManager1.service");
    }

    /// A path with a space survives the round trip, which is the case the
    /// quoting exists for.
    #[test]
    fn a_path_with_a_space_is_quoted_and_comes_back_whole() {
        let exec = quoted(Path::new("/home/a b/bin/skerry")).expect("quotable");
        assert_eq!(exec, "'/home/a b/bin/skerry'");
        let text = format!("{GROUP}\nExec={exec}\n");
        assert_eq!(exec_of(&text).as_deref(), Some("/home/a b/bin/skerry"));
    }

    /// A path that cannot be spelled is refused rather than written badly: a
    /// half-quoted `Exec` is a service that never starts and says nothing.
    #[test]
    fn a_path_holding_a_quote_has_no_spelling() {
        assert_eq!(quoted(Path::new("/home/it's/skerry")), Err(Refused::Unnameable));
        assert_eq!(quoted(Path::new("/home/a\nb/skerry")), Err(Refused::Unnameable));
    }

    /// Somebody else's file, which is the one case that must be left alone —
    /// and the arguments after the program are not part of its name.
    #[test]
    fn another_file_managers_file_names_that_file_manager() {
        let theirs = "[D-BUS Service]\n\
                      Name=org.freedesktop.FileManager1\n\
                      Exec=/usr/bin/nautilus --gapplication-service\n";
        assert_eq!(exec_of(theirs).as_deref(), Some("/usr/bin/nautilus"));
        assert_ne!(Path::new("/usr/bin/nautilus").file_name().unwrap(), "skerry");
    }

    /// An `Exec` outside the group, or no `Exec` at all, names nothing — so a
    /// file this parser does not understand is never mistaken for ours.
    #[test]
    fn an_exec_under_another_group_is_not_the_one() {
        assert_eq!(exec_of("[Desktop Entry]\nExec=/usr/bin/thunar\n"), None);
        assert_eq!(exec_of("[D-BUS Service]\nName=org.freedesktop.FileManager1\n"), None);
        assert_eq!(exec_of(""), None);
    }

    /// A binary replaced while it runs is still this program, at the same path.
    #[test]
    fn a_reinstalled_binary_is_named_by_the_path_it_was_reinstalled_at() {
        let link = Path::new("/usr/local/bin/skerry (deleted)");
        assert_eq!(without_deleted(link), Path::new("/usr/local/bin/skerry"));
        assert_eq!(
            without_deleted(Path::new("/usr/local/bin/skerry")),
            Path::new("/usr/local/bin/skerry")
        );
    }

    /// Where it goes, when the environment says where `$XDG_DATA_HOME` is.
    ///
    /// **Under `$XDG_DATA_HOME` and nowhere else.** The first entry of
    /// `data_dirs` is `/usr/local/share` on a process with neither variable set,
    /// and writing there is the packaging collision this module is about.
    #[test]
    fn the_place_is_under_the_data_home() {
        let Some(place) = super::place() else { return };
        let home = crate::data_home().expect("a data home, since place() found one");
        assert!(place.starts_with(&home), "{place:?} is not under {home:?}");
        assert!(place.ends_with(Path::new("dbus-1/services").join(FILE)));
    }
}
