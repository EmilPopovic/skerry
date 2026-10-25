// SPDX-License-Identifier: Apache-2.0
//! Starting an application, and not becoming its parent.
//!
//! Two things here are more subtle than they look. **`Exec` is a format**, with
//! field codes and its own quoting rules, and expanding it by splitting on spaces
//! puts a filename with a space in it into two arguments. And **a launched
//! application must outlive us and must not be in our session**, or closing the
//! file manager takes the text editor with it and a `Ctrl+C` in the terminal that
//! started us reaches both.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::entry::DesktopEntry;

/// Applications this process has started.
///
/// Held so they can be reaped. A child that is never waited for becomes a zombie
/// for as long as we run, and a file manager is a program that runs for days.
#[derive(Default)]
pub struct Launcher {
    running: Vec<Child>,
    /// How many were started, for the diagnostics overlay.
    started: u64,
}

/// Why a launch did not happen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The `Exec` line does not name a program — it is field codes only, so the
    /// first argument would be the user's own file. "Open this" would become
    /// "run this", which is a `.desktop` file anybody can drop into
    /// `~/.local/share/applications`.
    NotAProgram,
    /// `Terminal=true` and no terminal emulator could be found.
    NoTerminal,
    /// `fork`/`exec` failed.
    Failed,
}

/// Terminal emulators to try for a `Terminal=true` entry, in order.
///
/// There is no specification for this and no D-Bus interface that answers it, so
/// it is a list — the same shape as the list every other file manager carries.
/// `$TERMINAL` comes first because it is the only part a user can set.
///
/// They are passed the command after `-e`, which `x-terminal-emulator`,
/// `konsole` and `xterm` document and the wlroots-era three accept for
/// compatibility. It is a claim about seven third-party programs with no test
/// behind it, which is why a failure to start one is reported rather than
/// swallowed.
const TERMINALS: &[&str] =
    &["x-terminal-emulator", "foot", "kitty", "alacritty", "wezterm", "konsole", "xterm"];

impl Launcher {
    /// Starts `entry` on `paths` and takes ownership of the child.
    pub fn open(&mut self, entry: &DesktopEntry, paths: &[PathBuf]) -> bool {
        match Self::start(entry, paths) {
            Ok(child) => {
                self.adopt(child);
                true
            }
            Err(_) => false,
        }
    }

    /// Starts `entry` on `paths`, without needing the launcher.
    ///
    /// **This is what runs on a job.** `Command::spawn` is `fork` plus `exec`,
    /// and with a `pre_exec` set the parent blocks on the child's exec-status
    /// pipe — so a handler whose binary lives on a stalled mount stops the frame
    /// loop. The caller hands the child back with [`Launcher::adopt`] so it can
    /// still be reaped.
    pub fn start(entry: &DesktopEntry, paths: &[PathBuf]) -> Result<Child, Refused> {
        let argv = expand(&entry.exec, paths).ok_or(Refused::NotAProgram)?;
        let (program, arguments) = argv.split_first().ok_or(Refused::NotAProgram)?;

        let mut command = if entry.terminal {
            // `Terminal=true` was parsed and then dropped, so `nvim.desktop` or
            // `htop.desktop` reported "Opening Neovim" and nothing appeared —
            // the notice actively told the user it had worked.
            let emulator = find_terminal().ok_or(Refused::NoTerminal)?;
            let mut command = Command::new(emulator);
            command.arg("-e").arg(program).args(arguments);
            command
        } else {
            let mut command = Command::new(program);
            command.args(arguments);
            command
        };
        command
            // A launched application inherits nothing: its output would otherwise
            // land in whatever terminal started the file manager, interleaved
            // with ours.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // SAFETY: `setsid` is async-signal-safe and is the documented way to
        // detach a child from the caller's session and process group. Nothing
        // else runs between `fork` and `exec` here.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, || {
                libc::setsid();
                Ok(())
            });
        }
        command.spawn().map_err(|_| Refused::Failed)
    }

    /// Starts the user's terminal with `at` as its working directory.
    ///
    /// **The working directory rather than a flag.** Every emulator spells it
    /// differently — `--working-directory`, `--workdir`, `-cd`, and konsole
    /// wants `--workdir` before the profile — and every one of them inherits the
    /// cwd of the process that spawned it. One mechanism that works everywhere
    /// beats a table of six that is wrong on the seventh.
    ///
    /// **Runs on a job**, for the reason [`Launcher::start`] does.
    pub fn terminal_in(at: &Path) -> Result<Child, Refused> {
        let emulator = find_terminal().ok_or(Refused::NoTerminal)?;
        let mut command = Command::new(emulator);
        command.current_dir(at).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: as in `start` — `setsid` is async-signal-safe and nothing else
        // runs between `fork` and `exec`.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, || {
                libc::setsid();
                Ok(())
            });
        }
        command.spawn().map_err(|_| Refused::Failed)
    }

    /// Takes a child started by [`Launcher::start`], so it is reaped.
    pub fn adopt(&mut self, child: Child) {
        self.running.push(child);
        self.started += 1;
    }

    /// Collects any application that has exited.
    ///
    /// Called every frame. `try_wait` does not block, and the list is as long as
    /// the number of applications this session has open.
    pub fn reap(&mut self) {
        self.running.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
    }

    /// Applications started, and how many are still running.
    #[must_use]
    pub fn stats(&self) -> (u64, usize) {
        (self.started, self.running.len())
    }
}

/// Expands an `Exec` line into an argument vector.
///
/// The field codes from the desktop-entry specification. `%f` and `%u` take one
/// path and `%F` and `%U` take all of them; `%i`, `%c` and `%k` are about icons
/// and menus and are dropped; a literal percent is `%%`. Anything else beginning
/// with `%` is deprecated and dropped, which the specification requires.
#[must_use]
pub fn expand(exec: &str, paths: &[PathBuf]) -> Option<Vec<String>> {
    let tokens = split(exec)?;
    // **The program cannot come from a path.** `Exec=%f` expands to the file the
    // user asked to open, and `argv[0]` is what gets executed — so "open this
    // document" becomes "run this document", from a `.desktop` file anyone can
    // write into `~/.local/share/applications`.
    if tokens.first().is_none_or(|token| is_field_code(token)) {
        return None;
    }
    let mut argv = Vec::new();
    for token in tokens {
        match token.as_str() {
            "%f" | "%u" => {
                if let Some(first) = paths.first() {
                    argv.push(first.to_string_lossy().into_owned());
                }
            }
            "%F" | "%U" => {
                argv.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
            }
            "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            other => argv.push(other.replace("%%", "%")),
        }
    }
    (!argv.is_empty()).then_some(argv)
}

/// Whether a token is a field code rather than a word.
fn is_field_code(token: &str) -> bool {
    token.len() == 2 && token.starts_with('%') && !token.ends_with('%')
}

/// The first terminal emulator on `PATH`, `$TERMINAL` first.
fn find_terminal() -> Option<PathBuf> {
    // `$TERMINAL` is a program, not a command line: `urxvt -depth 32` is one
    // string here and would be looked up as a filename with a space in it. The
    // first word is the program a person meant; the arguments beside it are not
    // ours to pass on, because the command goes after `-e`.
    let named = std::env::var("TERMINAL")
        .ok()
        .and_then(|value| value.split_whitespace().next().map(PathBuf::from));
    let candidates = named.into_iter().chain(TERMINALS.iter().map(PathBuf::from));
    for candidate in candidates {
        if candidate.is_absolute() {
            // `continue`, not `return`: a `$TERMINAL` pointing at something no
            // longer installed would otherwise disable terminal launching
            // entirely, and the message a user then sees is "set $TERMINAL" —
            // advice for the variable that is already set and is the cause.
            if executable(&candidate) {
                return Some(candidate);
            }
            continue;
        }
        if let Some(found) = find_on_path(&candidate.to_string_lossy()) {
            return Some(found);
        }
    }
    None
}

/// Splits an `Exec` line the way the specification says to.
///
/// Double quotes group, backslash escapes inside them, and unquoted whitespace
/// separates. Not a shell: `Exec` is explicitly not passed to one, and treating
/// it as if it were is how a filename containing `;` runs a second command.
fn split(exec: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut started = false;

    for character in exec.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => {
                quoted = !quoted;
                started = true;
            }
            character if character.is_whitespace() && !quoted => {
                if started || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
    }
    if quoted {
        // An unterminated quote is a malformed line, and guessing where it was
        // meant to end is how a launcher runs something nobody wrote.
        return None;
    }
    if started || !current.is_empty() {
        out.push(current);
    }
    Some(out)
}

/// Where a binary is on `PATH`, if it is anywhere.
#[must_use]
pub fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = Path::new(binary);
    if path.is_absolute() {
        return executable(path).then(|| path.to_path_buf());
    }
    let value = std::env::var_os("PATH")?;
    std::env::split_paths(&value).map(|dir| dir.join(binary)).find(|it| executable(it))
}

/// Whether a path names something that can actually be run.
///
/// `is_file` is not the question: a file on `PATH` without the executable bit is
/// still a file, and `exec` on it is `EACCES`. Checked here rather than left to
/// the spawn, so a `TryExec=` naming a stub is one entry skipped rather than one
/// launch that fails after the notice has said it worked.
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{expand, find_on_path, split};
    use std::path::PathBuf;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_single_file_code_takes_one_path_and_a_plural_one_takes_all() {
        let one = paths(&["/a/one.txt", "/a/two.txt"]);
        assert_eq!(expand("app %f", &one).unwrap(), vec!["app", "/a/one.txt"]);
        assert_eq!(expand("app %F", &one).unwrap(), vec!["app", "/a/one.txt", "/a/two.txt"]);
        assert_eq!(expand("app %u", &one).unwrap(), vec!["app", "/a/one.txt"]);
    }

    #[test]
    fn a_file_code_with_no_files_leaves_the_program_alone() {
        assert_eq!(expand("app %F", &[]).unwrap(), vec!["app"]);
        assert_eq!(expand("app --flag %f", &[]).unwrap(), vec!["app", "--flag"]);
    }

    #[test]
    fn the_codes_about_icons_and_menus_are_dropped() {
        let one = paths(&["/a/x"]);
        assert_eq!(expand("app %i %c %k %f", &one).unwrap(), vec!["app", "/a/x"]);
    }

    /// The reason this is not a `split_whitespace`.
    #[test]
    fn a_quoted_argument_with_a_space_stays_one_argument() {
        let argv = expand("\"/opt/My App/bin/run\" --flag %f", &paths(&["/a/x"])).unwrap();
        assert_eq!(argv, vec!["/opt/My App/bin/run", "--flag", "/a/x"]);
    }

    #[test]
    fn a_path_with_a_space_or_a_semicolon_is_one_argument_and_not_a_command() {
        let argv = expand("app %f", &paths(&["/a/my file; rm -rf ~.txt"])).unwrap();
        assert_eq!(argv, vec!["app", "/a/my file; rm -rf ~.txt"]);
    }

    #[test]
    fn an_escaped_quote_inside_a_quoted_argument_survives() {
        assert_eq!(split(r#"app "a\"b" c"#).unwrap(), vec!["app", "a\"b", "c"]);
    }

    #[test]
    fn an_unterminated_quote_is_refused_rather_than_guessed_at() {
        assert!(split("app \"unfinished").is_none());
        assert!(expand("app \"unfinished %f", &paths(&["/a/x"])).is_none());
    }

    #[test]
    fn a_double_percent_is_a_literal_percent() {
        assert_eq!(expand("app 100%% %f", &paths(&["/a/x"])).unwrap()[1], "100%");
    }

    /// `Exec=%f` expands to the user's own file, and `argv[0]` is what runs.
    /// "Open this document" would become "run this document", from a `.desktop`
    /// file anyone can write into `~/.local/share/applications`.
    #[test]
    fn an_exec_line_that_is_only_field_codes_names_no_program() {
        for exec in ["%f", "%F", "%u", "%U", "%f %F"] {
            assert_eq!(
                expand(exec, &[PathBuf::from("/home/tester/report.pdf")]),
                None,
                "{exec:?} turned a document into a program"
            );
        }
        // A literal that merely starts with a percent is still a program: `%%`
        // is an escaped percent, not a field code.
        assert_eq!(
            expand("%%viewer %f", &[PathBuf::from("/a/b")]),
            Some(vec!["%viewer".to_owned(), "/a/b".to_owned()])
        );
    }

    #[test]
    fn an_empty_exec_line_produces_no_command() {
        assert!(expand("", &[]).is_none());
        assert!(expand("   ", &[]).is_none());
    }

    #[test]
    fn a_binary_on_the_path_is_found_and_one_that_is_not_there_is_not() {
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("skerry-definitely-not-a-binary").is_none());
        assert!(find_on_path("/bin/sh").is_some());
        assert!(find_on_path("/bin/nope-not-here").is_none());
    }
}
