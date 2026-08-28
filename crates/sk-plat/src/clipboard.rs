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

/// Which of the two selections.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    /// `Ctrl+C` and `Ctrl+V`.
    Clipboard,
    /// The middle-click selection, which X11 called PRIMARY and Wayland kept.
    Primary,
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
    pub fn paths(paths: &[std::path::PathBuf]) -> Self {
        let mut uris = String::new();
        for path in paths {
            use std::fmt::Write as _;
            let _ = writeln!(uris, "file://{}\r", encode_uri(&path.to_string_lossy()));
        }
        let text = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            types: vec![
                ("text/uri-list".to_owned(), uris.into_bytes().into()),
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

/// Percent-encodes the characters a `file://` URI may not contain.
///
/// A filename may hold anything but `/` and NUL, and `text/uri-list` is a URI
/// list: a space or a `#` in a name produces something the peer parses as two
/// entries or as a fragment. Only unreserved characters and `/` survive.
fn encode_uri(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.as_bytes() {
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
    let rest = match rest.strip_prefix('/') {
        Some(path) => format!("/{path}"),
        None => return None,
    };
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
    let text = String::from_utf8(out).ok()?;
    (!text.is_empty()).then(|| std::path::PathBuf::from(text))
}

/// What the peer is currently offering, on each selection.
///
/// The types alone, not the bytes: a selection can be megabytes and nobody has
/// asked for it yet. Reading it is [`crate::Window::request_selection`], which
/// hands back a descriptor rather than a buffer for the same reason.
#[derive(Default, Debug)]
pub struct Clipboard {
    pub(crate) offer_types: Vec<String>,
    pub(crate) primary_types: Vec<String>,
}

impl Clipboard {
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
        let encoded = encode_uri("/home/emil/my file #1.txt");
        assert_eq!(encoded, "/home/emil/my%20file%20%231.txt");
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
        let content = Content::paths(&[PathBuf::from("/a/one"), PathBuf::from("/a/two")]);
        let uris = content.bytes_for("text/uri-list").expect("uri-list");
        assert_eq!(String::from_utf8_lossy(uris), "file:///a/one\r\nfile:///a/two\r\n");
        let text = content.bytes_for("text/plain;charset=utf-8").expect("text");
        assert_eq!(String::from_utf8_lossy(text), "/a/one\n/a/two");
    }

    /// The alias every GTK and Qt application still asks for.
    #[test]
    fn the_x11_utf8_alias_carries_the_same_bytes_as_the_text_type() {
        for content in [Content::paths(&[PathBuf::from("/a/one")]), Content::text("hello")] {
            let text = content.bytes_for("text/plain;charset=utf-8").expect("text");
            let alias = content.bytes_for("UTF8_STRING").expect("alias");
            assert_eq!(text, alias);
            assert!(!alias.is_empty());
        }
    }

    #[test]
    fn a_uri_round_trips_through_both_halves() {
        for original in ["/home/emil/my file #1.txt", "/tmp/naïve", "/a/100%25.txt", "/plain"] {
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
        let content = Content::paths(std::slice::from_ref(&path));
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
    fn paths_are_offered_as_text_as_well_as_uris() {
        let content = Content::paths(&[std::path::PathBuf::from("/tmp/one")]);
        let offered: Vec<&str> = content.types.iter().map(|(mime, _)| mime.as_str()).collect();
        assert!(offered.contains(&"text/uri-list"), "{offered:?}");
        assert!(offered.iter().any(|mime| mime.starts_with("text/plain")), "{offered:?}");
    }
}
