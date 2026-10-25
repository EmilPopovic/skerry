// SPDX-License-Identifier: Apache-2.0
//! The two selections, in terms neither display server owns.
//!
//! `Selection` and `Content` are the same idea on both: a peer asks for a MIME
//! type and gets bytes. Only the transport differs — a `wl_data_source` and a
//! pipe on one side, an X11 selection owner and a property on the other — so the
//! transport is what each backend keeps and this is what they share.
//!
//! The X11 names are the original ones. Wayland kept `PRIMARY` and the idea of
//! offering several types of the same thing; this module is the part of that
//! design that predates both backends.
//!
//! The write side is shared too: a peer that asks for a multi-megabyte
//! selection is read at its own pace on both servers, and [`Transfer`] is the
//! same buffer either way.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt as _;

/// Which of the two selections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    /// `Ctrl+C` and `Ctrl+V`.
    Clipboard,
    /// The middle-click selection, which X11 called PRIMARY and Wayland kept.
    Primary,
}

/// Whether a set of paths was copied or cut.
///
/// On the clipboard this is not a property of the paths, it is a *type*:
/// `text/uri-list` says which files and nothing about what to do with them, and
/// every desktop that supports cut-and-paste for files does it by publishing a
/// second type that says. Without it a cut arrives in Nautilus as a copy — the
/// files land, the originals stay, and nobody is told why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    /// The originals stay.
    Copy,
    /// The originals are the receiver's to remove, once it has them.
    Cut,
}

impl Intent {
    /// The word `x-special/gnome-copied-files` opens with.
    const fn word(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Cut => "cut",
        }
    }

    /// Reads that word back. Anything else is a copy, which is the safe answer:
    /// a verb we did not understand must not delete somebody's files.
    fn of(word: &str) -> Self {
        if word.trim() == "cut" { Self::Cut } else { Self::Copy }
    }
}

/// Content to publish, as one or more types of the same thing.
///
/// More than one because a peer asks for what it understands: a file manager
/// wants `text/uri-list`, a terminal wants `text/plain`, and offering only one of
/// them means half the paste targets get nothing.
#[derive(Clone, Debug, Default)]
pub struct Content {
    /// MIME type to bytes, in the order they are offered.
    ///
    /// `Arc`, not `Vec`: a peer asking for the selection gets a handle to the
    /// bytes rather than a copy of them. `Ctrl+A` over the 200,000 entries the
    /// budgets are written for builds a multi-megabyte `text/uri-list`, and
    /// copying it inside `wl_data_source::Send` — which is dispatched from the
    /// frame loop — was a memcpy of that inside a frame, once per asking peer.
    pub types: Vec<(String, std::sync::Arc<[u8]>)>,
}

impl Content {
    /// A list of paths, as both a URI list and as text.
    #[must_use]
    pub fn paths(paths: &[std::path::PathBuf], intent: Intent) -> Self {
        let mut uris = String::new();
        for path in paths {
            use std::fmt::Write as _;
            let _ =
                writeln!(uris, "file://{}\r", encode_bytes(path.as_os_str().as_encoded_bytes()));
        }
        // `x-special/gnome-copied-files`: the verb, then one URI per line, and
        // **no trailing newline** — GTK joins rather than terminates, and a
        // trailing one reads as an empty final URI. Undocumented by any
        // standard and read by GTK, Qt and every file manager that interoperates
        // with them.
        let mut cut_or_copy = String::from(intent.word());
        for path in paths {
            use std::fmt::Write as _;
            let _ = write!(
                cut_or_copy,
                "\nfile://{}",
                encode_bytes(path.as_os_str().as_encoded_bytes())
            );
        }
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            types: vec![
                // First, because a peer takes the first type it understands and
                // this is the only one that carries the verb.
                ("x-special/gnome-copied-files".to_owned(), cut_or_copy.into_bytes().into()),
                ("text/uri-list".to_owned(), uris.into_bytes().into()),
                (
                    "application/x-kde-cutselection".to_owned(),
                    Vec::from(if intent == Intent::Cut { b"1" } else { b"0" }).into(),
                ),
                ("text/plain;charset=utf-8".to_owned(), text.clone().into_bytes().into()),
                ("text/plain".to_owned(), text.into_bytes().into()),
                // X11's name for UTF-8 text, which GTK and Qt both still offer
                // and some applications still ask for by preference.
                ("UTF8_STRING".to_owned(), std::sync::Arc::from(&[][..])),
            ],
        }
        .with_utf8_alias()
    }

    /// Plain text.
    #[must_use]
    pub fn text(value: &str) -> Self {
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(value.as_bytes());
        Self {
            types: vec![
                ("text/plain;charset=utf-8".to_owned(), std::sync::Arc::clone(&bytes)),
                ("text/plain".to_owned(), std::sync::Arc::clone(&bytes)),
                ("UTF8_STRING".to_owned(), bytes),
            ],
        }
    }

    /// Fills in the alias entry with the same bytes as the canonical text type.
    fn with_utf8_alias(mut self) -> Self {
        let text = self
            .types
            .iter()
            .find(|(mime, _)| mime == "text/plain;charset=utf-8")
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_default();
        for (mime, bytes) in &mut self.types {
            if mime == "UTF8_STRING" && bytes.is_empty() {
                bytes.clone_from(&text);
            }
        }
        self
    }

    /// A handle to the bytes for `mime`, without copying them.
    pub(crate) fn share(&self, mime: &str) -> Option<std::sync::Arc<[u8]>> {
        self.types.iter().find(|(name, _)| name == mime).map(|(_, bytes)| bytes.clone())
    }

    /// The bytes for `mime`, borrowed. For tests and for anything that wants to
    /// look without taking a handle.
    #[must_use]
    pub fn bytes_for(&self, mime: &str) -> Option<&[u8]> {
        self.types.iter().find(|(name, _)| name == mime).map(|(_, bytes)| &bytes[..])
    }
}

/// The most a peer's answer to a paste or a drop may hold.
///
/// **Counted where the bytes first gather.** Wayland hands the peer a pipe, so
/// [`receive`] counts what comes through it. X11 has the peer write properties
/// that this process collects before anything reaches the pipe, so the X11
/// backend counts too: a peer answering with `INCR` could otherwise keep
/// writing until this process ran out of memory.
///
/// **Read with [`receive`] and a limit no larger than this.** An X11 answer
/// that passes it reaches the pipe as more than this many bytes, so the
/// reader's own count refuses it with `FileTooLarge` and can say so; a reader
/// that did not count would take the first sixteen megabytes of an answer as
/// all of it.
pub const SELECTION_LIMIT: usize = 16 * 1024 * 1024;

/// How long a peer has to finish answering a paste or a drop.
///
/// Measured from **before** the request: a caller takes
/// `Instant::now() + SELECTION_TIMEOUT` and only then asks, so the reader's
/// deadline can never be later than the X11 backend's, which is taken at the
/// request. That backend abandons a conversion a moment after the reader has
/// given up and said why, so nothing is left holding a peer that stopped
/// halfway or never started.
pub const SELECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why a selection could not be asked for at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unasked {
    /// Nothing is on offer, or the display server would not take the request.
    Unavailable,
    /// Every conversion this process can have in flight is waiting on a peer.
    ///
    /// X11 only, where each lands in a property of its own and there are eight.
    /// A peer that does not answer holds one until its deadline, so this
    /// passes in seconds — and a caller says so, because a paste that did
    /// nothing reads exactly like an empty clipboard.
    Busy,
}

/// A pipe for one selection read, closed on exec at both ends.
///
/// **At creation, not after.** A helper or an application launched while a paste
/// or a drop is in flight otherwise inherits the write end: the job reading the
/// other end waits for an end-of-file that comes only when that process exits,
/// and the process can read the bytes or write its own. An `fcntl` after `pipe`
/// leaves a window in which another thread's `fork` takes both ends with it.
pub(crate) fn selection_pipe() -> Option<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()
}

/// Read a selection on a worker, refusing oversized or stalled transfers.
///
/// `deadline` is when the peer must have finished, taken before the request —
/// see [`SELECTION_TIMEOUT`]. **What is already in the pipe is read even after
/// it**, so a job the pool started late does not refuse an answer that arrived
/// in time; only waiting for more stops there.
pub fn receive(
    fd: &OwnedFd,
    limit: usize,
    deadline: std::time::Instant,
) -> std::io::Result<Vec<u8>> {
    use rustix::event::{PollFd, PollFlags};
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let timeout = rustix::event::Timespec::try_from(left)
            .map_err(|_| std::io::ErrorKind::InvalidInput)?;
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
            Ok(0) => return Err(std::io::ErrorKind::TimedOut.into()),
            Ok(_) => {}
        }
        match rustix::io::read(fd, &mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(count) if bytes.len().saturating_add(count) <= limit => {
                bytes.extend_from_slice(&buffer[..count]);
            }
            Ok(_) => return Err(std::io::ErrorKind::FileTooLarge.into()),
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Percent-encodes the characters a `file://` URI may not contain.
///
/// A filename may hold anything but `/` and NUL, and `text/uri-list` is a URI
/// list: a space or a `#` in a name produces something the peer parses as two
/// entries or as a fragment. Only unreserved characters and `/` survive.
#[cfg(test)]
fn encode_uri(path: &str) -> String {
    encode_bytes(path.as_bytes())
}

fn encode_bytes(path: &[u8]) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Reads `x-special/gnome-copied-files` back: the verb and the paths it names.
///
/// Returns nothing for anything that is not that format, so a caller can try it
/// and fall back to `text/uri-list` without having to know which it asked for.
#[must_use]
pub fn decode_copied_files(bytes: &[u8]) -> Option<(Intent, Vec<std::path::PathBuf>)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    let verb = lines.next()?;
    if !matches!(verb.trim(), "copy" | "cut") {
        return None;
    }
    let paths: Vec<std::path::PathBuf> =
        lines.filter(|line| !line.trim().is_empty()).filter_map(decode_uri).collect();
    (!paths.is_empty()).then(|| (Intent::of(verb), paths))
}

/// Decodes a `file://` URI back into a path.
///
/// The other half of [`encode_uri`], and the reason a peer's `text/uri-list` can
/// be trusted: a name with a `%20` in it is a name with a space, and a name with
/// a literal `%` is `%25`. Returns nothing for a URI that is not a local file,
/// because a `http://` entry is not a path however much it looks like one.
#[must_use]
pub fn decode_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` has an empty authority; `file://host/path` names another
    // machine and is not ours to open.
    let rest = format!("/{}", rest.strip_prefix('/')?);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    if out.contains(&0) {
        return None;
    }
    (!out.is_empty()).then(|| std::path::PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// What the peer is currently offering, on each selection.
///
/// The types alone, not the bytes: a selection can be megabytes and nobody has
/// asked for it yet. Reading it is [`crate::Window::request_selection`], which
/// hands back a descriptor rather than a buffer for the same reason.
#[derive(Default, Debug)]
pub struct Clipboard {
    pub(crate) generation: u64,
    pub(crate) offer_types: Vec<String>,
    pub(crate) primary_types: Vec<String>,
}

impl Clipboard {
    /// Identity of the current offer, invalidated when ownership changes.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The MIME types the peer is currently offering on the clipboard.
    #[must_use]
    pub fn offered(&self) -> &[String] {
        &self.offer_types
    }

    /// The MIME types the peer is currently offering on the primary selection.
    #[must_use]
    pub fn primary_offered(&self) -> &[String] {
        &self.primary_types
    }
}

/// A transfer to a peer that has not finished.
///
/// A pipe holds 64 KiB. `Ctrl+A` in a directory the budgets are written for —
/// 200,000 entries — builds a `text/uri-list` of several megabytes, so the
/// overflow case is not exotic, it is the case the product is measured on.
pub struct Transfer {
    fd: OwnedFd,
    bytes: std::sync::Arc<[u8]>,
    written: usize,
}

impl Transfer {
    /// Writes what fits and reports whether there is more.
    fn resume(&mut self) -> bool {
        while self.written < self.bytes.len() {
            match rustix::io::write(&self.fd, &self.bytes[self.written..]) {
                Ok(0) => return false,
                Ok(count) => self.written += count,
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return true,
                // The peer asked and then went away. There is nothing to finish
                // and nobody to tell.
                Err(_) => return false,
            }
        }
        false
    }

    /// The descriptor to wait on for room in the pipe.
    pub(super) fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Starts writing `bytes` to a pipe the peer will read from.
///
/// **Never blocks.** This runs inside `wl_data_source::Send`, which is dispatched
/// from the frame loop, and a full pipe blocks until the peer drains it — a peer
/// that asks for the selection and then reads slowly, or never, would otherwise
/// hold the window still, `xdg_wm_base.ping` included, until the compositor gave
/// up on it. What does not fit is parked and finished by `Window::pump`, which
/// waits on the pipe for room alongside the display connection.
pub(crate) fn send_to(pending: &mut Vec<Transfer>, fd: OwnedFd, bytes: std::sync::Arc<[u8]>) {
    let flags = rustix::fs::fcntl_getfl(&fd).unwrap_or(rustix::fs::OFlags::empty());
    if rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).is_err() {
        // The peer asked, we said yes, and now nothing is coming. Dropping the
        // descriptor turns that into an end-of-file it can act on rather than a
        // wait it cannot.
        return;
    }
    // Nothing is copied at any point: the payload is shared with the published
    // selection and a transfer is an index into it.
    let mut transfer = Transfer { fd, bytes, written: 0 };
    if transfer.resume() {
        // **Bounded.** A peer that asks and then reads slowly, or loops on
        // `receive`, would otherwise hold a descriptor and a payload handle each
        // until this process ran out of descriptors. The oldest goes first: it
        // is the one whose peer has had the longest to read.
        if pending.len() >= MAX_TRANSFERS {
            pending.remove(0);
        }
        pending.push(transfer);
    }
}

/// What the selection machinery is holding, for a diagnostic report.
///
/// **Because a read that never lands looks exactly like a drop that never
/// happened.** A drag in from another application hands a pipe to a job and the
/// job blocks on it; if the answer is mismatched or never arrives, the only
/// visible difference is that nothing appears — and the numbers below are what
/// separate the three ways that can happen without touching the frame loop to
/// find out.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Selections {
    /// Conversions asked for and not yet answered. Each holds a pipe's write
    /// end, and somebody is blocked reading the other one.
    pub awaiting: usize,
    /// Writes parked for room in a peer's pipe.
    pub sending: usize,
    /// Answers that arrived and matched nothing outstanding, ever. Any value but
    /// zero is a reply this process threw away.
    pub unmatched: u32,
}

/// How many unfinished selection writes to keep at once.
///
/// A peer that asks for the selection and reads it takes microseconds. One that
/// does not has stopped, and eight of them is already more applications than
/// have a paste target open.
const MAX_TRANSFERS: usize = 8;

/// Finishes what it can of every parked transfer, keeping the rest.
pub(crate) fn resume_all(pending: &mut Vec<Transfer>) {
    pending.retain_mut(Transfer::resume);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::io::Read as _;
    use std::path::PathBuf;
    use std::time::Instant;

    /// The selection the budgets are written for is bigger than a pipe.
    ///
    /// `Ctrl+A` in 200,000 entries builds a `text/uri-list` of several megabytes;
    /// a pipe holds 64 KiB. This used to be a `write_all` inside the dispatch
    /// handler, which is the frame loop — so a peer that asked and then read
    /// slowly held the window still, `xdg_wm_base.ping` included, until the
    /// compositor gave up on it. Watched failing with the `write_all` restored,
    /// this test does not finish.
    #[test]
    fn selection_reads_stop_at_the_byte_limit_and_deadline() {
        let soon = || Instant::now() + std::time::Duration::from_secs(1);
        let (read, write) = selection_pipe().unwrap();
        rustix::io::write(&write, b"abc").unwrap();
        drop(write);
        assert_eq!(super::receive(&read, 3, soon()).unwrap(), b"abc");
        let (read, write) = selection_pipe().unwrap();
        rustix::io::write(&write, b"abcd").unwrap();
        assert_eq!(
            super::receive(&read, 3, soon()).unwrap_err().kind(),
            std::io::ErrorKind::FileTooLarge
        );
        let (read, _held) = selection_pipe().unwrap();
        let passed = Instant::now() + std::time::Duration::from_millis(1);
        assert_eq!(
            super::receive(&read, 3, passed).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    /// The deadline is taken before the request, so a pool busy for longer
    /// than it starts the reader after it has passed. An answer that had
    /// finished by then is still the answer; one still arriving is not.
    #[test]
    fn an_answer_that_finished_in_time_is_read_by_a_reader_that_started_late() {
        let passed = Instant::now();
        let (read, write) = selection_pipe().unwrap();
        rustix::io::write(&write, b"abc").unwrap();
        drop(write);
        assert_eq!(super::receive(&read, 3, passed).unwrap(), b"abc");

        let (read, held) = selection_pipe().unwrap();
        rustix::io::write(&held, b"ab").unwrap();
        assert_eq!(
            super::receive(&read, 3, passed).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    /// Both ends, because both leak something different: the write end held by
    /// a launched application keeps the reader from ever seeing end-of-file, and
    /// the read end hands it the bytes. Watched failing with `pipe()` restored.
    #[test]
    fn a_selection_pipe_is_not_inherited_by_anything_this_process_launches() {
        let (read, write) = selection_pipe().unwrap();
        for (end, fd) in [("read", &read), ("write", &write)] {
            let flags = rustix::io::fcntl_getfd(fd).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC), "the {end} end survives exec");
        }
    }

    #[test]
    fn kde_marker_tracks_cut_and_copy_intent() {
        let paths = [std::path::PathBuf::from("/tmp/a file")];
        assert_eq!(
            Content::paths(&paths, Intent::Cut).bytes_for("application/x-kde-cutselection"),
            Some(&b"1"[..])
        );
        assert_eq!(
            Content::paths(&paths, Intent::Copy).bytes_for("application/x-kde-cutselection"),
            Some(&b"0"[..])
        );
    }

    #[test]
    fn a_selection_larger_than_a_pipe_does_not_block_the_caller() {
        let (read, write) =
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).expect("a pipe");
        let payload: std::sync::Arc<[u8]> =
            (0..4 * 1024 * 1024).map(|index| (index % 251) as u8).collect();

        let mut pending: Vec<Transfer> = Vec::new();
        let started = Instant::now();
        send_to(&mut pending, write, std::sync::Arc::clone(&payload));
        assert!(
            started.elapsed().as_millis() < 200,
            "the write blocked for {:?} with nobody reading",
            started.elapsed()
        );
        assert_eq!(pending.len(), 1, "a transfer that cannot finish has to be parked");

        // And it does finish, once somebody reads.
        let mut got = Vec::new();
        let mut file = std::fs::File::from(read);
        let mut chunk = vec![0u8; 64 * 1024];
        while got.len() < payload.len() {
            resume_all(&mut pending);
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => got.extend_from_slice(&chunk[..count]),
                Err(error) => panic!("read: {error}"),
            }
        }
        assert_eq!(got, &payload[..], "the peer got something other than what was published");
        assert!(pending.is_empty(), "a finished transfer stayed parked");
    }

    /// A peer that asks and then closes must not leave a transfer waiting for
    /// room that will never come.
    #[test]
    fn a_transfer_to_a_peer_that_left_is_dropped_rather_than_kept() {
        let (read, write) =
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).expect("a pipe");
        drop(read);
        // Rust's runtime ignores `SIGPIPE`, so the write returns `EPIPE` rather
        // than killing the process.
        let mut pending: Vec<Transfer> = Vec::new();
        send_to(&mut pending, write, std::sync::Arc::from(vec![0u8; 4 * 1024 * 1024]));
        resume_all(&mut pending);
        assert!(pending.is_empty(), "a transfer to a closed pipe was kept");
    }

    /// A peer that asks for the selection and never reads it holds a descriptor
    /// and a payload handle. One that loops on `receive` holds one of each per
    /// call, until this process runs out of descriptors.
    #[test]
    fn a_peer_that_never_reads_cannot_accumulate_transfers_without_bound() {
        let payload: std::sync::Arc<[u8]> = std::sync::Arc::from(vec![7u8; 4 * 1024 * 1024]);
        let mut pending: Vec<Transfer> = Vec::new();
        // The read ends are kept, so every pipe stays open and full.
        let mut ends = Vec::new();
        for _ in 0..64 {
            let (read, write) =
                rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).expect("a pipe");
            ends.push(read);
            send_to(&mut pending, write, std::sync::Arc::clone(&payload));
        }
        assert_eq!(pending.len(), MAX_TRANSFERS, "{} parked", pending.len());
    }

    #[test]
    fn a_path_with_a_space_or_a_hash_survives_the_uri_list() {
        let encoded = encode_uri("/home/tester/my file #1.txt");
        assert_eq!(encoded, "/home/tester/my%20file%20%231.txt");
        assert!(!encoded.contains(' '), "a space splits a uri-list entry in two");
        assert!(!encoded.contains('#'), "a hash makes the rest a fragment");
    }

    #[test]
    fn slashes_and_unreserved_characters_are_left_alone() {
        assert_eq!(encode_uri("/a/b-c_d.e~f/G9"), "/a/b-c_d.e~f/G9");
    }

    #[test]
    fn a_non_ascii_name_is_encoded_as_its_utf8_bytes() {
        assert_eq!(encode_uri("/tmp/naïve"), "/tmp/na%C3%AFve");
    }

    #[test]
    fn a_path_selection_is_offered_as_both_a_uri_list_and_as_text() {
        let content =
            Content::paths(&[PathBuf::from("/a/one"), PathBuf::from("/a/two")], Intent::Copy);
        let uris = content.bytes_for("text/uri-list").expect("uri-list");
        assert_eq!(String::from_utf8_lossy(uris), "file:///a/one\r\nfile:///a/two\r\n");
        let text = content.bytes_for("text/plain;charset=utf-8").expect("text");
        assert_eq!(String::from_utf8_lossy(text), "/a/one\n/a/two");
    }

    /// The alias every GTK and Qt application still asks for.
    #[test]
    fn the_x11_utf8_alias_carries_the_same_bytes_as_the_text_type() {
        for content in
            [Content::paths(&[PathBuf::from("/a/one")], Intent::Copy), Content::text("hello")]
        {
            let text = content.bytes_for("text/plain;charset=utf-8").expect("text");
            let alias = content.bytes_for("UTF8_STRING").expect("alias");
            assert_eq!(text, alias);
            assert!(!alias.is_empty());
        }
    }

    #[test]
    fn a_uri_round_trips_through_both_halves() {
        for original in ["/home/tester/my file #1.txt", "/tmp/naïve", "/a/100%25.txt", "/plain"] {
            let uri = format!("file://{}", encode_uri(original));
            let back = decode_uri(&uri).expect(&uri);
            assert_eq!(back, PathBuf::from(original), "{uri}");
        }
    }

    #[test]
    fn a_uri_that_is_not_a_local_file_is_refused() {
        assert!(decode_uri("http://example.com/x").is_none());
        assert!(decode_uri("file://otherhost/path").is_none());
        assert!(decode_uri("/not/a/uri").is_none());
    }

    #[test]
    fn a_type_nobody_offered_has_no_bytes() {
        let content = Content::text("hello");
        assert!(content.bytes_for("image/png").is_none());
    }

    #[test]
    fn a_uri_survives_the_round_trip_with_a_space_and_a_percent_in_it() {
        let path = std::path::PathBuf::from("/tmp/a name with 100% in it.txt");
        let content = Content::paths(std::slice::from_ref(&path), Intent::Copy);
        let uris = content.bytes_for("text/uri-list").expect("a uri list");
        let line = std::str::from_utf8(uris).expect("utf-8").trim_end();
        assert_eq!(decode_uri(line).as_deref(), Some(path.as_path()));
    }

    #[test]
    fn a_uri_that_is_not_a_local_file_is_not_a_path() {
        assert_eq!(decode_uri("https://example.invalid/x"), None);
        assert_eq!(decode_uri("file://"), None);
    }

    #[test]
    fn a_copy_and_a_cut_differ_only_in_the_verb() {
        let paths = [std::path::PathBuf::from("/tmp/one"), std::path::PathBuf::from("/tmp/two")];
        let copied = Content::paths(&paths, Intent::Copy);
        let cut = Content::paths(&paths, Intent::Cut);
        let of = |content: &Content| {
            String::from_utf8(content.bytes_for("x-special/gnome-copied-files").unwrap().to_vec())
                .unwrap()
        };
        assert_eq!(of(&copied), "copy\nfile:///tmp/one\nfile:///tmp/two");
        assert_eq!(of(&cut), "cut\nfile:///tmp/one\nfile:///tmp/two");
        // The uri-list is the same either way: it names files and says nothing
        // about what to do with them, which is the whole reason the other type
        // exists.
        assert_eq!(copied.bytes_for("text/uri-list"), cut.bytes_for("text/uri-list"));
    }

    /// GTK joins with newlines rather than terminating, so a trailing one reads
    /// as an empty final URI.
    #[test]
    fn the_copied_files_list_has_no_trailing_newline() {
        let content = Content::paths(&[std::path::PathBuf::from("/tmp/one")], Intent::Copy);
        let bytes = content.bytes_for("x-special/gnome-copied-files").unwrap();
        assert!(!bytes.ends_with(b"\n"), "{:?}", String::from_utf8_lossy(bytes));
    }

    #[test]
    fn a_cut_survives_the_round_trip_with_a_space_in_the_name() {
        let paths = [std::path::PathBuf::from("/tmp/a name.txt")];
        let content = Content::paths(&paths, Intent::Cut);
        let bytes = content.bytes_for("x-special/gnome-copied-files").unwrap();
        let (intent, back) = decode_copied_files(bytes).expect("our own format reads back");
        assert_eq!(intent, Intent::Cut);
        assert_eq!(back, paths);
    }

    /// A verb nobody recognises must not delete anybody's files.
    #[test]
    fn an_unknown_verb_is_not_a_cut() {
        assert_eq!(decode_copied_files(b"obliterate\nfile:///tmp/one"), None);
        let (intent, _) = decode_copied_files(b"copy\nfile:///tmp/one").expect("a copy");
        assert_eq!(intent, Intent::Copy);
    }

    #[test]
    fn something_that_is_not_this_format_is_not_read_as_it() {
        assert_eq!(decode_copied_files(b"file:///tmp/one\r\n"), None);
        assert_eq!(decode_copied_files(b""), None);
        assert_eq!(decode_copied_files(b"cut"), None);
        assert_eq!(decode_copied_files(&[0xff, 0xfe]), None);
    }

    #[test]
    fn paths_are_offered_as_text_as_well_as_uris() {
        let content = Content::paths(&[std::path::PathBuf::from("/tmp/one")], Intent::Copy);
        let offered: Vec<&str> = content.types.iter().map(|(mime, _)| mime.as_str()).collect();
        assert!(offered.contains(&"text/uri-list"), "{offered:?}");
        assert!(offered.iter().any(|mime| mime.starts_with("text/plain")), "{offered:?}");
    }
    #[test]
    fn invalid_bytes_survive_a_clipboard_uri_round_trip() {
        use std::os::unix::ffi::OsStringExt as _;
        let path =
            PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/parent\xff/file\xfe".to_vec()));
        let content = Content::paths(std::slice::from_ref(&path), Intent::Copy);
        let bytes = content.bytes_for("text/uri-list").unwrap();
        let uri = std::str::from_utf8(bytes).unwrap().trim();
        assert_eq!(super::decode_uri(uri), Some(path));
        assert!(super::decode_uri("file:///tmp/%00").is_none());
    }
}
