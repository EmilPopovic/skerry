//! The clipboard and the primary selection, as a source and as a target.
//!
//! Four things [Spike D](../../../../spikes/d-platform-and-dnd/README.md) learned
//! the expensive way are structural here, because each was a silent failure that
//! looked like a compositor bug:
//!
//! * **The devices are created before the first surface commit.** A data device
//!   created after the first keyboard `enter` never learns the current selection,
//!   and the symptom is an empty clipboard with no error anywhere.
//! * **A selection is set with a serial from a real input event.** The protocol
//!   requires it so that a background window cannot steal the clipboard, and a
//!   stale serial is refused silently.
//! * **The source keeps dispatching after the transfer starts.** A peer reads on
//!   its own schedule; a client that stops dispatching hands it an empty pipe.
//! * **`zwp_primary_selection_device_v1.set_selection` is silently discarded by
//!   Hyprland** unless `misc:middle_click_paste` is on. Nothing reports it. That
//!   is a documented compositor limitation with no fallback, and it is why
//!   [`Clipboard::primary_offered`] exists — the only way to know is to look.
//!
//! Reading a peer's selection **never happens on the frame loop**. The offer's
//! `receive` gives us a pipe and the peer writes to it whenever it feels like it;
//! Spike D measured a deliberately hung peer costing the loop 0.394 ms against
//! rule 1's 8 ms because the read was somewhere else. Here the read end is handed
//! to the caller, which puts it on a job.

use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat,
};
use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};

use super::State;

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
    pub types: Vec<(String, Vec<u8>)>,
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
                ("text/uri-list".to_owned(), uris.into_bytes()),
                ("text/plain;charset=utf-8".to_owned(), text.clone().into_bytes()),
                ("text/plain".to_owned(), text.into_bytes()),
                // X11's name for UTF-8 text, which GTK and Qt both still offer
                // and some applications still ask for by preference.
                ("UTF8_STRING".to_owned(), Vec::new()),
            ],
        }
        .with_utf8_alias()
    }

    /// Plain text.
    #[must_use]
    pub fn text(value: &str) -> Self {
        let bytes = value.as_bytes().to_vec();
        Self {
            types: vec![
                ("text/plain;charset=utf-8".to_owned(), bytes.clone()),
                ("text/plain".to_owned(), bytes.clone()),
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

    fn bytes_for(&self, mime: &str) -> Option<&[u8]> {
        self.types.iter().find(|(name, _)| name == mime).map(|(_, bytes)| bytes.as_slice())
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

/// The selections this window publishes and sees.
#[derive(Default)]
pub struct Clipboard {
    pub(super) device: Option<wl_data_device::WlDataDevice>,
    pub(super) primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,

    /// What we are publishing, kept because the peer asks for it later.
    pub(super) published: Option<Content>,
    pub(super) published_primary: Option<Content>,
    pub(super) source: Option<wl_data_source::WlDataSource>,
    pub(super) primary_source: Option<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1>,

    /// The peer's current offers, and what they contain.
    pub(super) offer: Option<wl_data_offer::WlDataOffer>,
    pub(super) offer_types: Vec<String>,
    pub(super) primary_offer: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    pub(super) primary_types: Vec<String>,
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

impl super::Window {
    /// Publishes `content` on `selection`.
    ///
    /// Needs a serial from a real input event, which is why this can fail
    /// silently before the window has ever been touched — the protocol refuses a
    /// selection from a client that has not been interacted with, and that
    /// refusal has no event.
    pub fn set_selection(&mut self, selection: Selection, content: Content) {
        let Some(serial) = self.state.seat.keyboard_serial.or(self.state.seat.button_serial) else {
            return;
        };
        let handle = self.queue.handle();
        let types: Vec<String> = content.types.iter().map(|(mime, _)| mime.clone()).collect();

        match selection {
            Selection::Clipboard => {
                let Some(manager) = self.state.data_manager.clone() else { return };
                let Some(device) = self.state.clipboard.device.clone() else { return };
                let source = manager.create_data_source(&handle, ());
                for mime in &types {
                    source.offer(mime.clone());
                }
                device.set_selection(Some(&source), serial);
                if let Some(old) = self.state.clipboard.source.replace(source) {
                    old.destroy();
                }
                self.state.clipboard.published = Some(content);
            }
            Selection::Primary => {
                let Some(manager) = self.state.primary_manager.clone() else { return };
                let Some(device) = self.state.clipboard.primary_device.clone() else { return };
                let source = manager.create_source(&handle, ());
                for mime in &types {
                    source.offer(mime.clone());
                }
                device.set_selection(Some(&source), serial);
                if let Some(old) = self.state.clipboard.primary_source.replace(source) {
                    old.destroy();
                }
                self.state.clipboard.published_primary = Some(content);
            }
        }
        let _ = self.queue.flush();
    }

    /// Asks the peer for `mime` and returns the pipe to read it from.
    ///
    /// **The caller reads on a job.** The peer writes whenever it likes and may
    /// never write at all; a read here would be rule 1's canonical failure with a
    /// different cause.
    #[must_use]
    pub fn request_selection(&mut self, selection: Selection, mime: &str) -> Option<OwnedFd> {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
        match selection {
            Selection::Clipboard => {
                let offer = self.state.clipboard.offer.as_ref()?;
                offer.receive(mime.to_owned(), write.as_fd());
            }
            Selection::Primary => {
                let offer = self.state.clipboard.primary_offer.as_ref()?;
                offer.receive(mime.to_owned(), write.as_fd());
            }
        }
        // The write end must close here, or the read never sees end-of-file: the
        // peer holds a copy and ours would keep the pipe open forever.
        drop(write);
        // And the request has to reach the compositor before anyone waits on the
        // pipe, which is the whole reason this flush is not optional.
        self.queue.flush().ok()?;
        Some(read)
    }

    /// What the peer is offering, on both selections.
    #[must_use]
    pub fn clipboard(&self) -> &Clipboard {
        &self.state.clipboard
    }
}

/// Writes `bytes` to a pipe the peer will read from.
///
/// A pipe holds 64 KiB, and a thousand selected paths is about 50 KiB, so the
/// write completes here in every case that occurs. When it does not — a peer that
/// asked and then stopped reading — the rest goes to a short-lived thread rather
/// than to the frame loop, because a full pipe blocks until the peer drains it
/// and the peer may never do so.
pub(super) fn send_to(fd: OwnedFd, bytes: &[u8]) {
    // A failure part-way through means the peer asked and then went away, which
    // is not an error anybody can act on: there is nothing to finish and nobody
    // to tell.
    let _ = std::fs::File::from(fd).write_all(bytes);
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                // A new offer object. Its types arrive as separate events before
                // the `selection` that adopts it, so it is parked here.
                state.pending_offer = Some(id);
                state.pending_types.clear();
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(old) = state.clipboard.offer.take() {
                    old.destroy();
                }
                state.clipboard.offer = id;
                state.clipboard.offer_types = std::mem::take(&mut state.pending_types);
                state.pending_offer = None;
            }
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.pending_types.push(mime_type);
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let bytes = state
                    .clipboard
                    .published
                    .as_ref()
                    .and_then(|content| content.bytes_for(&mime_type))
                    .unwrap_or_default();
                send_to(fd, bytes);
            }
            wl_data_source::Event::Cancelled => {
                // Somebody else took the clipboard. Ours is no longer the
                // selection and holding the bytes would be a lie about what a
                // paste would produce.
                if state.clipboard.source.as_ref() == Some(source) {
                    state.clipboard.source = None;
                    state.clipboard.published = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                state.pending_primary_offer = Some(offer);
                state.pending_primary_types.clear();
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                if let Some(old) = state.clipboard.primary_offer.take() {
                    old.destroy();
                }
                state.clipboard.primary_offer = id;
                state.clipboard.primary_types = std::mem::take(&mut state.pending_primary_types);
                state.pending_primary_offer = None;
            }
            _ => {}
        }
    }

    event_created_child!(State, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE =>
            (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            state.pending_primary_types.push(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_source_v1::Event::Send { mime_type, fd } => {
                let bytes = state
                    .clipboard
                    .published_primary
                    .as_ref()
                    .and_then(|content| content.bytes_for(&mime_type))
                    .unwrap_or_default();
                send_to(fd, bytes);
            }
            zwp_primary_selection_source_v1::Event::Cancelled => {
                if state.clipboard.primary_source.as_ref() == Some(source) {
                    state.clipboard.primary_source = None;
                    state.clipboard.published_primary = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_device_manager::WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1, ()>
    for State
{
    fn event(
        _: &mut Self,
        _: &zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
        _: zwp_primary_selection_device_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Unused, but the seat type has to be nameable from here.
const _: Option<wl_seat::WlSeat> = None;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Content, encode_uri};
    use std::path::PathBuf;

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
            let back = super::decode_uri(&uri).expect(&uri);
            assert_eq!(back, PathBuf::from(original), "{uri}");
        }
    }

    #[test]
    fn a_uri_that_is_not_a_local_file_is_refused() {
        assert!(super::decode_uri("http://example.com/x").is_none());
        assert!(super::decode_uri("file://otherhost/path").is_none());
        assert!(super::decode_uri("/not/a/uri").is_none());
    }

    #[test]
    fn a_type_nobody_offered_has_no_bytes() {
        let content = Content::text("hello");
        assert!(content.bytes_for("image/png").is_none());
    }
}
