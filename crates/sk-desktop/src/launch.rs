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

impl Launcher {
    /// Starts `entry` on `paths`.
    ///
    /// Returns whether the process was created — not whether the application
    /// liked what it was given, which is between the user and the application.
    pub fn open(&mut self, entry: &DesktopEntry, paths: &[PathBuf]) -> bool {
        let Some(argv) = expand(&entry.exec, paths) else { return false };
        let Some((program, arguments)) = argv.split_first() else { return false };

        let mut command = Command::new(program);
        command
            .args(arguments)
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

        match command.spawn() {
            Ok(child) => {
                self.running.push(child);
                self.started += 1;
                true
            }
            Err(_) => false,
        }
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
    let mut argv = Vec::new();
    for token in split(exec)? {
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
        return path.is_file().then(|| path.to_path_buf());
    }
    let value = std::env::var_os("PATH")?;
    std::env::split_paths(&value).map(|dir| dir.join(binary)).find(|candidate| candidate.is_file())
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
