// SPDX-License-Identifier: Apache-2.0
//! Reading a file's bytes as text, bounded.
//!
//! One function, for the one thing that wants a file's *contents* rather than
//! its name and its size: copying the text of a source file or a note to the
//! clipboard without starting an editor.
//!
//! **Runs on a job**, like everything else in this crate. A file on an
//! unresponsive mount takes as long to open as a directory on one does.

use std::io::Read as _;
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};
use sk_model::{Problem, ProblemKind};

/// The most text that is worth putting on a clipboard.
///
/// A megabyte is about fifteen thousand lines of source, which is longer than
/// any file a person copies whole; past it the answer is to open the file. The
/// bound exists because the alternative is unbounded: a listing holds log files
/// and disk images, and a "copy the text" that reads a 2 GB one has turned a
/// keystroke into a memory exhaustion.
pub const TEXT_LIMIT: usize = 1 << 20;

/// Reads `path` as UTF-8 text, refusing anything that is not text or is larger
/// than `limit`.
///
/// Three refusals, and each of them names the file rather than the operation:
///
/// * **Not a regular file.** A directory, a device, a socket — and a FIFO, which
///   matters more than it looks: opening one for reading blocks until somebody
///   writes to it, which on a pool worker is a thread that never comes back.
///   `O_NONBLOCK` on the open is what makes the refusal possible at all.
/// * **Larger than `limit`.** Checked twice: once against the size `fstat`
///   reports, which is the cheap answer, and once against the bytes actually
///   read, because a file in `/proc` reports a size of zero and then hands over
///   a page of text.
/// * **Not UTF-8.** The clipboard's text targets are UTF-8 by specification, and
///   a lossy conversion would put replacement characters on it — text that looks
///   like the file and is not.
pub fn read_text(path: &Path, limit: usize) -> Result<String, Problem> {
    let subject = || path.display().to_string();
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = rustix::fs::open(path, flags, Mode::empty()).map_err(|error| {
        let kind = match error {
            rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR => ProblemKind::Gone,
            _ => ProblemKind::Unreadable,
        };
        Problem::new(kind, subject(), format!("open: {error}"))
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, subject(), format!("stat: {error}"))
    })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Problem::new(ProblemKind::NotText, subject(), "not a regular file"));
    }
    if usize::try_from(stat.st_size).unwrap_or(usize::MAX) > limit {
        return Err(Problem::new(
            ProblemKind::TooLarge,
            subject(),
            format!("{} bytes, over the {limit} byte limit", stat.st_size),
        ));
    }

    // One byte past the limit, so a file whose reported size lied is refused
    // rather than truncated: half of a document is not the document.
    let mut bytes = Vec::new();
    let file = std::fs::File::from(fd);
    file.take(limit as u64 + 1).read_to_end(&mut bytes).map_err(|error| {
        Problem::new(ProblemKind::Unreadable, subject(), format!("read: {error}"))
    })?;
    if bytes.len() > limit {
        return Err(Problem::new(
            ProblemKind::TooLarge,
            subject(),
            format!("more than {limit} bytes, whatever `stat` said"),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| Problem::new(ProblemKind::NotText, subject(), format!("utf-8: {error}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{TEXT_LIMIT, read_text};
    use sk_model::ProblemKind;
    use std::path::PathBuf;

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("skerry-text-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture");
        root
    }

    #[test]
    fn a_text_file_comes_back_whole_and_unchanged() {
        let root = fixture("plain");
        let path = root.join("notes.txt");
        let content = "first line\nsecond line\n\ncafé — em dash and a tab\t\n";
        std::fs::write(&path, content).expect("write");
        assert_eq!(read_text(&path, TEXT_LIMIT).expect("read"), content);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_file_is_empty_text_and_not_an_error() {
        let root = fixture("empty");
        let path = root.join("empty.txt");
        std::fs::write(&path, b"").expect("write");
        assert_eq!(read_text(&path, TEXT_LIMIT).expect("read"), "");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Both bounds: the one `stat` answers, and the one the read answers for a
    /// file whose reported size is a lie.
    #[test]
    fn a_file_over_the_limit_is_refused_rather_than_truncated() {
        let root = fixture("large");
        let path = root.join("big.txt");
        std::fs::write(&path, "x".repeat(4096)).expect("write");
        let refused = read_text(&path, 1024).expect_err("refused");
        assert_eq!(refused.kind(), ProblemKind::TooLarge);
        assert_eq!(read_text(&path, 4096).expect("at the limit").len(), 4096);

        // `/proc/self/status` reports a size of zero and hands over a page of
        // text, so the read's own bound is the one that catches it.
        let lying = read_text(std::path::Path::new("/proc/self/status"), 16);
        assert_eq!(lying.expect_err("refused").kind(), ProblemKind::TooLarge);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bytes_that_are_not_utf8_are_not_text() {
        let root = fixture("binary");
        let path = root.join("looks-like-text.txt");
        std::fs::write(&path, [0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x0a]).expect("write");
        let refused = read_text(&path, TEXT_LIMIT).expect_err("refused");
        assert_eq!(refused.kind(), ProblemKind::NotText);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The refusal that has to happen before the read.** Opening a FIFO for
    /// reading blocks until somebody writes to it; without `O_NONBLOCK` this test
    /// would hang rather than fail.
    #[test]
    fn a_fifo_is_refused_and_does_not_block() {
        let root = fixture("fifo");
        let path = root.join("pipe");
        let made = rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        );
        if made.is_err() {
            // A filesystem with no FIFOs. Nothing to assert, and nothing wrong.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let refused = read_text(&path, TEXT_LIMIT).expect_err("refused");
        assert_eq!(refused.kind(), ProblemKind::NotText);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_and_a_missing_file_each_say_what_they_are() {
        let root = fixture("kinds");
        assert_eq!(read_text(&root, TEXT_LIMIT).expect_err("dir").kind(), ProblemKind::NotText);
        let gone = root.join("nothing-here.txt");
        assert_eq!(read_text(&gone, TEXT_LIMIT).expect_err("gone").kind(), ProblemKind::Gone);
        let _ = std::fs::remove_dir_all(&root);
    }
}
