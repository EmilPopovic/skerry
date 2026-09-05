// SPDX-License-Identifier: Apache-2.0
//! The X11 clipboard: selection ownership, `TARGETS`, and `INCR`.
//!
//! Wayland hands a peer a pipe and lets it read at its own pace. X11 has no
//! pipes: the owner writes bytes into a *property on the requestor's window* and
//! sends a `SelectionNotify` to say it is there. Everything awkward about this
//! module follows from that one difference.
//!
//! Three things are protocol rather than choice, and each is a silent failure
//! when it is missing:
//!
//! * **A request needs a real timestamp.** `CurrentTime` is legal and is a race —
//!   it means "whenever this arrives", so two clients asking at once can both
//!   believe they own the selection. Every request here carries the timestamp of
//!   the input event that caused it, which is what ICCCM section 2.1 requires.
//! * **A property larger than the server's maximum request cannot be sent at
//!   once.** ICCCM section 2.7's answer is `INCR`: the owner writes the total
//!   size, and then one chunk each time the requestor deletes the property.
//!   `Ctrl+A` over the 200,000 entries the budgets are written for builds a
//!   `text/uri-list` of several megabytes, so this is the ordinary path and not
//!   the exotic one.
//! * **Knowing what a peer offers needs XFIXES.** Core X11 has no notification
//!   that the selection changed; a client either polls or does without. XFIXES
//!   `SelectSelectionInput` is the event, and `TARGETS` is the question it
//!   prompts.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard::{Content, Selection, Transfer, send_to};

use super::atoms::Atoms;

/// How much of a property one request may carry.
///
/// The server states a maximum request length; the rest of the request — the
/// opcode, the window, the atoms — is well under a hundred bytes, and 4 KiB of
/// headroom costs one extra round trip on a multi-megabyte transfer.
const CHUNK_HEADROOM: usize = 4096;

/// What this window is publishing on one selection.
struct Published {
    content: Content,
    /// When ownership was taken, which `TIMESTAMP` requests ask for.
    since: xproto::Timestamp,
}

/// A transfer to a peer that did not fit in one property.
struct Outgoing {
    requestor: xproto::Window,
    property: xproto::Atom,
    kind: xproto::Atom,
    bytes: std::sync::Arc<[u8]>,
    sent: usize,
}

/// A conversion this window asked for and is waiting on.
struct Incoming {
    /// The type that was asked for.
    ///
    /// **A refusal names no property.** `SelectionNotify` carries `property =
    /// None` when the owner declines, so a pending conversion cannot be found by
    /// the property it was going to land in — it is found by what it asked for.
    /// Matching on the property alone left the entry, and its pipe's write end,
    /// alive for the life of the process: the job reading that pipe waited for an
    /// end-of-file that could never come, and a paste did nothing at all with no
    /// error to show for it.
    target: xproto::Atom,
    /// Where the bytes go once they arrive. The caller already has the other end.
    pipe: Option<OwnedFd>,
    /// Set once the reply said `INCR`: the payload arrives one property at a
    /// time and is finished by a zero-length one.
    incremental: bool,
    /// What has arrived so far, when it is arriving incrementally.
    collected: Vec<u8>,
    /// Which selection this answers, so a `TARGETS` reply updates the right
    /// offer list.
    primary: bool,
}

/// What a `TARGETS` reply said, and which selection said it.
pub(super) struct Offered {
    /// Whether this is the middle-click selection rather than the clipboard.
    pub(super) primary: bool,
    /// The MIME types the owner is offering.
    pub(super) mimes: Vec<String>,
}

/// Everything a selection request needs in order to name itself.
///
/// One value rather than five arguments: the connection, the atoms, the type
/// table, our window and the timestamp travel together through every request in
/// this module, and a timestamp that got separated from its request is the race
/// this module's header warns about.
#[derive(Clone, Copy)]
pub(super) struct Ask<'a, C> {
    /// The connection to speak on.
    pub(super) connection: &'a C,
    /// The interned atoms.
    pub(super) atoms: &'a Atoms,
    /// Which MIME types map to which atoms.
    pub(super) types: Types,
    /// Our own window, which owns and receives.
    pub(super) window: xproto::Window,
    /// The timestamp of the input event that caused this. Never `CurrentTime`.
    pub(super) time: xproto::Timestamp,
}

/// The two selections, as this backend sees them.
#[derive(Default)]
pub(super) struct Selections {
    clipboard: Option<Published>,
    primary: Option<Published>,
    /// Conversions in flight, keyed by the property they will land in.
    ///
    /// A `VecDeque` and not a map: there is never more than a handful, and a
    /// scan of four is cheaper than a hash.
    incoming: VecDeque<(xproto::Atom, Incoming)>,
    outgoing: Vec<Outgoing>,
    /// A selection that is neither the clipboard nor primary. `XdndSelection` is
    /// the only one today, and it is owned for the length of one drag.
    extra: Option<(xproto::Atom, Published)>,
    /// Answers that matched nothing outstanding. See [`crate::Selections`].
    unmatched: u32,
}

impl Selections {
    /// Asks the server to notify us when either selection changes hands.
    ///
    /// Without XFIXES there is no such notification and
    /// [`crate::Clipboard::offered`] would be permanently empty — which reads as
    /// "there is nothing on the clipboard" and is not the same thing.
    pub(super) fn watch(
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
    ) -> bool {
        if xfixes::query_version(connection, 5, 0).ok().and_then(|c| c.reply().ok()).is_none() {
            return false;
        }
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        for selection in [atoms.CLIPBOARD, xproto::AtomEnum::PRIMARY.into()] {
            if connection.xfixes_select_selection_input(window, selection, mask).is_err() {
                return false;
            }
        }
        true
    }

    fn slot(&mut self, selection: Selection) -> &mut Option<Published> {
        match selection {
            Selection::Clipboard => &mut self.clipboard,
            Selection::Primary => &mut self.primary,
        }
    }

    /// Content published outside the two selections — a drag's, which lives in
    /// `Xdnd` because it goes away when the drag does.
    pub(super) fn set_extra(
        &mut self,
        atom: xproto::Atom,
        content: Option<Content>,
        time: xproto::Timestamp,
    ) {
        self.extra = content.map(|content| (atom, Published { content, since: time }));
    }

    fn published_for(&self, atoms: &Atoms, atom: xproto::Atom) -> Option<&Published> {
        if let Some((extra, published)) = &self.extra
            && *extra == atom
        {
            return Some(published);
        }
        if atom == atoms.CLIPBOARD {
            self.clipboard.as_ref()
        } else if atom == xproto::Atom::from(xproto::AtomEnum::PRIMARY) {
            self.primary.as_ref()
        } else {
            None
        }
    }

    /// Takes ownership of `selection` and remembers what to hand out.
    pub(super) fn publish(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selection: Selection,
        content: Content,
    ) -> Vec<String> {
        let Ask { connection, atoms, window, time, .. } = *ask;
        let atom = atom_of(atoms, selection);
        if connection.set_selection_owner(window, atom, time).is_err() {
            return Vec::new();
        }
        // Verified rather than assumed: `SetSelectionOwner` is asynchronous and
        // silently does nothing if the timestamp is older than the current
        // owner's, which is the failure a stale event timestamp produces.
        let ours = connection
            .get_selection_owner(atom)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.owner == window);
        let _ = connection.flush();
        if !ours {
            return Vec::new();
        }
        // **What we are now offering, reported back.** X11 has no equivalent of
        // `wl_data_device.selection`, which tells a client about its own
        // selection — so without this the offer list stayed empty after a copy
        // and `Ctrl+V` said the clipboard was empty.
        let offered = content.types.iter().map(|(mime, _)| mime.clone()).collect();
        *self.slot(selection) = Some(Published { content, since: time });
        offered
    }

    /// Forgets a selection somebody else has taken.
    pub(super) fn lost(&mut self, atoms: &Atoms, atom: xproto::Atom) {
        if atom == atoms.CLIPBOARD {
            self.clipboard = None;
        } else if atom == xproto::Atom::from(xproto::AtomEnum::PRIMARY) {
            self.primary = None;
        }
    }

    /// Answers a peer that wants what we published.
    pub(super) fn answer(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        request: &xproto::SelectionRequestEvent,
    ) {
        let connection = ask.connection;
        // ICCCM 2.2: a property of `None` is an obsolete client asking us to
        // choose one, and the target is the conventional answer.
        let property = if request.property == 0 { request.target } else { request.property };
        let granted = self.fill(ask, request, property);
        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            // Refusal is a reply with no property, not silence. A peer that gets
            // silence waits for its own timeout, which is seconds of a paste
            // that looks frozen.
            property: if granted { property } else { 0 },
        };
        let _ = connection.send_event(false, request.requestor, xproto::EventMask::NO_EVENT, event);
        let _ = connection.flush();
    }

    /// Writes the answer into the requestor's property. Whether there was one.
    fn fill(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        request: &xproto::SelectionRequestEvent,
        property: xproto::Atom,
    ) -> bool {
        let Ask { connection, atoms, types, .. } = *ask;
        let Some(published) = self.published_for(atoms, request.selection) else { return false };
        let since = published.since;

        if request.target == atoms.TARGETS {
            let mut targets: Vec<xproto::Atom> = vec![atoms.TARGETS, atoms.TIMESTAMP];
            for (mime, _) in &published.content.types {
                if let Some(atom) = types.atom(mime) {
                    targets.push(atom);
                }
            }
            return connection
                .change_property32(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    xproto::AtomEnum::ATOM,
                    &targets,
                )
                .is_ok();
        }
        if request.target == atoms.TIMESTAMP {
            return connection
                .change_property32(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    xproto::AtomEnum::INTEGER,
                    &[since],
                )
                .is_ok();
        }

        let Some(mime) = types.mime(request.target) else { return false };
        let Some(bytes) = published.content.share(mime) else { return false };

        let chunk = chunk_size(connection);
        if bytes.len() <= chunk {
            return connection
                .change_property8(
                    xproto::PropMode::REPLACE,
                    request.requestor,
                    property,
                    request.target,
                    &bytes,
                )
                .is_ok();
        }

        // ICCCM 2.7. The first property is the total size and its type is `INCR`;
        // the peer then deletes the property once per chunk and we refill it.
        // Watching for those deletions needs the requestor's property changes.
        let aux =
            xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE);
        let _ = connection.change_window_attributes(request.requestor, &aux);
        let total = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        if connection
            .change_property32(
                xproto::PropMode::REPLACE,
                request.requestor,
                property,
                atoms.INCR,
                &[total],
            )
            .is_err()
        {
            return false;
        }
        self.outgoing.push(Outgoing {
            requestor: request.requestor,
            property,
            kind: request.target,
            bytes,
            sent: 0,
        });
        true
    }

    /// Conversions asked for and not yet answered.
    pub(super) fn awaiting(&self) -> usize {
        self.incoming.len()
    }

    /// How many answers have arrived matching nothing outstanding.
    pub(super) const fn unmatched(&self) -> u32 {
        self.unmatched
    }

    /// Asks a peer for `mime`, and hands back the end to read it from.
    pub(super) fn request(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selection: Selection,
        mime: &str,
    ) -> Option<OwnedFd> {
        self.request_from(ask, atom_of(ask.atoms, selection), mime)
    }

    /// The same, against any selection atom — `XdndSelection` is one too.
    pub(super) fn request_from(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selection: xproto::Atom,
        mime: &str,
    ) -> Option<OwnedFd> {
        let Ask { connection, atoms, types, window, time } = *ask;
        let target = types.atom(mime)?;
        let (read, write) = rustix::pipe::pipe().ok()?;
        // The property is deleted first so the arrival of *this* conversion is
        // distinguishable from whatever was left in it by the last one.
        let _ = connection.delete_property(window, atoms.SKERRY_SELECTION);
        connection
            .convert_selection(window, selection, target, atoms.SKERRY_SELECTION, time)
            .ok()?;
        let _ = connection.flush();
        self.incoming.push_back((
            atoms.SKERRY_SELECTION,
            Incoming {
                target,
                pipe: Some(write),
                incremental: false,
                collected: Vec::new(),
                primary: false,
            },
        ));
        Some(read)
    }

    /// Asks an owner what it offers. The reply lands in [`Self::arrived`].
    pub(super) fn ask_targets(&mut self, ask: &Ask<'_, impl Connection>, selection: Selection) {
        let Ask { connection, atoms, window, time, .. } = *ask;
        let property = atoms.SKERRY_TARGETS;
        let _ = connection.delete_property(window, property);
        if connection
            .convert_selection(window, atom_of(atoms, selection), atoms.TARGETS, property, time)
            .is_err()
        {
            return;
        }
        let _ = connection.flush();
        // No pipe: the answer is a list of atoms for the offer table, not bytes
        // for a caller. `arrived` tells the two apart by this being `None`.
        self.incoming.push_back((
            property,
            Incoming {
                target: atoms.TARGETS,
                pipe: None,
                incremental: false,
                collected: Vec::new(),
                primary: matches!(selection, Selection::Primary),
            },
        ));
    }

    /// A conversion we asked for has landed.
    pub(super) fn arrived(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        transfers: &mut Vec<Transfer>,
        event: &xproto::SelectionNotifyEvent,
    ) -> Option<Offered> {
        let Ask { connection, atoms, types, window, .. } = *ask;
        let found = if event.property == 0 {
            self.incoming.iter().position(|(_, it)| it.target == event.target)
        } else {
            self.incoming.iter().position(|(atom, _)| *atom == event.property)
        };
        let Some(index) = found else {
            // **Counted rather than ignored.** An answer that matches nothing is
            // a conversion this process asked for and then lost track of, and the
            // pipe behind it is one somebody is blocked reading — see
            // [`crate::Selections`].
            self.unmatched = self.unmatched.saturating_add(1);
            return None;
        };
        if event.property == 0 {
            // The owner refused. Dropping the entry drops the pipe's write end,
            // which turns the refusal into an end-of-file the reader can act on
            // rather than a wait it cannot.
            self.incoming.remove(index);
            return None;
        }
        let reply = connection
            .get_property(false, window, event.property, xproto::AtomEnum::ANY, 0, u32::MAX / 4)
            .ok()?
            .reply()
            .ok()?;

        if reply.type_ == atoms.INCR {
            let _ = connection.delete_property(window, event.property);
            let _ = connection.flush();
            if let Some((_, incoming)) = self.incoming.get_mut(index) {
                incoming.incremental = true;
            }
            return None;
        }

        let (_, mut incoming) = self.incoming.remove(index)?;
        let _ = connection.delete_property(window, event.property);
        if reply.type_ == atoms.TARGETS || reply.type_ == xproto::Atom::from(xproto::AtomEnum::ATOM)
        {
            let mimes = reply
                .value32()
                .into_iter()
                .flatten()
                .filter_map(|atom| types.mime(atom))
                .map(str::to_owned)
                .collect();
            return Some(Offered { primary: incoming.primary, mimes });
        }
        if let Some(pipe) = incoming.pipe.take() {
            send_to(transfers, pipe, reply.value.into());
        }
        None
    }

    /// One chunk of an incremental transfer, in either direction.
    pub(super) fn property_changed(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        transfers: &mut Vec<Transfer>,
        event: &xproto::PropertyNotifyEvent,
    ) {
        let _ = atoms;
        if event.state == xproto::Property::DELETE {
            self.push_next_chunk(connection, event, transfers);
            return;
        }
        // A `NewValue` on a property we are receiving into is the next chunk of
        // an `INCR` transfer, and a zero-length one is the end of it.
        let Some(index) =
            self.incoming.iter().position(|(atom, it)| *atom == event.atom && it.incremental)
        else {
            return;
        };
        let Some(reply) = connection
            .get_property(true, event.window, event.atom, xproto::AtomEnum::ANY, 0, u32::MAX / 4)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        let _ = connection.flush();
        if reply.value.is_empty() {
            // A zero-length property ends an `INCR` transfer, and everything
            // collected goes to the caller in one piece.
            if let Some((_, mut incoming)) = self.incoming.remove(index)
                && let Some(pipe) = incoming.pipe.take()
            {
                let bytes = std::mem::take(&mut incoming.collected);
                send_to(transfers, pipe, bytes.into());
            }
            return;
        }
        if let Some((_, incoming)) = self.incoming.get_mut(index) {
            incoming.collected.extend_from_slice(&reply.value);
        }
    }

    /// The peer deleted the property, so the next chunk goes in.
    fn push_next_chunk(
        &mut self,
        connection: &impl Connection,
        event: &xproto::PropertyNotifyEvent,
        transfers: &mut Vec<Transfer>,
    ) {
        let _ = transfers;
        let Some(index) = self
            .outgoing
            .iter()
            .position(|out| out.requestor == event.window && out.property == event.atom)
        else {
            return;
        };
        let chunk = chunk_size(connection);
        let done = {
            let out = &mut self.outgoing[index];
            let end = (out.sent + chunk).min(out.bytes.len());
            let slice = &out.bytes[out.sent..end];
            let ok = connection
                .change_property8(
                    xproto::PropMode::REPLACE,
                    out.requestor,
                    out.property,
                    out.kind,
                    slice,
                )
                .is_ok();
            out.sent = end;
            // A zero-length property is the terminator, so the transfer is only
            // finished after it has been written — one chunk past the last byte.
            !ok || slice.is_empty()
        };
        let _ = connection.flush();
        if done {
            self.outgoing.remove(index);
        }
    }
}

/// The atom naming one selection.
fn atom_of(atoms: &Atoms, selection: Selection) -> xproto::Atom {
    match selection {
        Selection::Clipboard => atoms.CLIPBOARD,
        Selection::Primary => xproto::AtomEnum::PRIMARY.into(),
    }
}

/// How much of a property fits in one request.
fn chunk_size(connection: &impl Connection) -> usize {
    connection.maximum_request_bytes().saturating_sub(CHUNK_HEADROOM).max(4096)
}

/// The MIME types this backend publishes, and the atoms they are named by.
///
/// A value rather than two free functions over [`Atoms`], because the two
/// directions have to agree: a `TARGETS` reply that lists an atom the reader
/// cannot name back is a type a peer can ask for and never receive. Six numbers
/// is also something a test can build without an X server.
#[derive(Clone, Copy)]
pub(super) struct Types {
    uri_list: xproto::Atom,
    utf8: xproto::Atom,
    plain: xproto::Atom,
    plain_utf8: xproto::Atom,
    gnome: xproto::Atom,
    targets: xproto::Atom,
}

impl Types {
    pub(super) const fn new(atoms: &Atoms) -> Self {
        Self {
            uri_list: atoms.text_uri_list,
            utf8: atoms.UTF8_STRING,
            plain: atoms.text_plain,
            plain_utf8: atoms.text_plain_utf8,
            gnome: atoms.gnome_copied_files,
            targets: atoms.TARGETS,
        }
    }

    /// The atom a MIME type is published under.
    ///
    /// X11 predates MIME, so `text/plain;charset=utf-8` is `UTF8_STRING` here —
    /// which is the name every X11 peer written before 2005 asks for.
    pub(super) fn atom(self, mime: &str) -> Option<xproto::Atom> {
        Some(match mime {
            "text/uri-list" => self.uri_list,
            "text/plain;charset=utf-8" | "text/plain;charset=UTF-8" => self.utf8,
            "text/plain" => self.plain,
            "x-special/gnome-copied-files" => self.gnome,
            "TARGETS" => self.targets,
            _ => return None,
        })
    }

    /// The MIME type an atom means, for the offer list a caller reads.
    pub(super) fn mime(self, atom: xproto::Atom) -> Option<&'static str> {
        if atom == self.uri_list {
            Some("text/uri-list")
        } else if atom == self.utf8 || atom == self.plain_utf8 {
            Some("text/plain;charset=utf-8")
        } else if atom == self.plain || atom == xproto::Atom::from(xproto::AtomEnum::STRING) {
            Some("text/plain")
        } else if atom == self.gnome {
            Some("x-special/gnome-copied-files")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: Types =
        Types { uri_list: 100, utf8: 101, plain: 102, plain_utf8: 103, gnome: 104, targets: 105 };

    /// A round trip has to land on the same MIME type, or a `TARGETS` reply
    /// lists a type a peer can ask for and never receive.
    #[test]
    fn every_published_type_survives_the_round_trip() {
        for mime in [
            "text/uri-list",
            "text/plain;charset=utf-8",
            "text/plain",
            "x-special/gnome-copied-files",
        ] {
            let Some(atom) = TYPES.atom(mime) else { panic!("{mime} has no atom") };
            assert_eq!(TYPES.mime(atom), Some(mime), "{mime} did not survive");
        }
    }

    #[test]
    fn a_type_nobody_publishes_has_no_atom() {
        assert_eq!(TYPES.atom("image/png"), None);
        assert_eq!(TYPES.mime(9_999), None);
    }

    /// `STRING` is what an X11 peer written before UTF-8 offers, and reading it
    /// as nothing at all is a paste that silently does nothing.
    #[test]
    fn the_ancient_string_atom_is_still_plain_text() {
        assert_eq!(TYPES.mime(xproto::AtomEnum::STRING.into()), Some("text/plain"));
    }

    /// Both spellings of the UTF-8 parameter appear in the wild, and a `Content`
    /// built by `Content::text` uses the lowercase one.
    #[test]
    fn either_spelling_of_the_utf8_parameter_finds_the_atom() {
        assert_eq!(TYPES.atom("text/plain;charset=utf-8"), Some(TYPES.utf8));
        assert_eq!(TYPES.atom("text/plain;charset=UTF-8"), Some(TYPES.utf8));
    }
}
