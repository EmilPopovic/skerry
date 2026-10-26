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
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard::{
    Content, SELECTION_LIMIT, SELECTION_TIMEOUT, Selection, Transfer, Unasked, selection_pipe,
    send_to,
};

use super::atoms::Atoms;

/// How much of a property one request may carry.
///
/// The server states a maximum request length; the rest of the request — the
/// opcode, the window, the atoms — is well under a hundred bytes, and 4 KiB of
/// headroom costs one extra round trip on a multi-megabyte transfer.
const CHUNK_HEADROOM: usize = 4096;

/// How long past [`SELECTION_TIMEOUT`] a conversion is kept.
///
/// **The reader gives up first, so it can say why.** Its deadline is taken
/// before the request and this one at it, so the reader's is never the later
/// of the two — but a pipe closed at the instant the reader's `poll` times out
/// can wake it as an end-of-file instead, which reads as an answer of nothing:
/// a drop that did nothing and said nothing. The margin is for that wake-up,
/// not for a pool slow to start the reader, which the shared start covers.
const OUTLIVES_THE_READER: Duration = Duration::from_secs(1);

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
    /// The selection and the type that were asked for.
    ///
    /// **A refusal names no property.** `SelectionNotify` carries `property =
    /// None` when the owner declines, so a pending conversion cannot be found by
    /// the property it was going to land in — it is found by what it asked for.
    /// Matching on the property alone left the entry, and its pipe's write end,
    /// alive for the life of the process: the job reading that pipe waited for an
    /// end-of-file that could never come, and a paste did nothing at all with no
    /// error to show for it.
    selection: xproto::Atom,
    target: xproto::Atom,
    /// Where the bytes go once they arrive. The caller already has the other end.
    pipe: Option<OwnedFd>,
    /// Set once the reply said `INCR`: the payload arrives one property at a
    /// time and is finished by a zero-length one.
    incremental: bool,
    /// What has arrived so far, when it is arriving incrementally.
    collected: Vec<u8>,
    /// The most `collected` may hold before the conversion is abandoned.
    limit: usize,
    /// When the conversion is abandoned if it has not finished.
    ///
    /// A peer can stop answering anywhere — before its first property, or
    /// between two chunks of an `INCR` — and nothing on the wire says so.
    deadline: Instant,
    /// Which selection this answers, so a `TARGETS` reply updates the right
    /// offer list.
    primary: bool,
}

impl Incoming {
    fn new(
        selection: xproto::Atom,
        target: xproto::Atom,
        pipe: Option<OwnedFd>,
        primary: bool,
        now: Instant,
    ) -> Self {
        Self {
            selection,
            target,
            pipe,
            incremental: false,
            collected: Vec::new(),
            limit: SELECTION_LIMIT,
            deadline: now + SELECTION_TIMEOUT + OUTLIVES_THE_READER,
            primary,
        }
    }

    /// Whether `more` bytes would still be under the limit.
    fn fits(&self, more: usize) -> bool {
        self.collected.len().saturating_add(more) <= self.limit
    }

    /// How much of a property to ask for, in the 32-bit units `GetProperty`
    /// counts in.
    ///
    /// **What is left under the limit, and no more.** A property is the peer's
    /// to size, so asking for all of it is a reply as large as the peer cares
    /// to make; asking for the room left makes one that would pass the limit
    /// come back with `bytes_after` set instead.
    fn words(&self) -> u32 {
        let room = self.limit.saturating_sub(self.collected.len());
        u32::try_from(room.div_ceil(4)).unwrap_or(u32::MAX)
    }

    /// Hands the reader more than the limit, with `chunk` as the last of it.
    ///
    /// **Neither dropped nor cut short.** Closing the pipe with nothing written
    /// read as an empty answer — a paste of no files, a drop that did nothing
    /// and said nothing — and the bytes up to the limit would read as all of
    /// one. Past it, [`crate::clipboard::receive`] refuses the answer as
    /// `FileTooLarge`, which is what a caller can tell a person.
    fn past_the_limit(mut self, chunk: &[u8], transfers: &mut Vec<Transfer>) {
        let Some(pipe) = self.pipe.take() else { return };
        self.collected.extend_from_slice(chunk);
        if self.collected.len() <= self.limit {
            // The server kept the rest of the property back, so what arrived
            // can end exactly at the limit.
            self.collected.resize(self.limit.saturating_add(1), 0);
        }
        send_to(transfers, pipe, self.collected.into());
    }
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
    /// Where [`Self::free_property`] looks first.
    cursor: usize,
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

    /// A property for one more conversion to land in, or `None` with every
    /// one of `pool` in use.
    ///
    /// **One per conversion in flight, and the one used longest ago.** Every
    /// conversion used to land in the same property, and `arrived` took the
    /// first entry waiting on it — so a peer abandoned mid-`INCR`, which writes
    /// its next chunk whenever the property is deleted, fed its bytes to the
    /// next conversion, which deletes the property to begin. This guards
    /// against a peer that is late, not one that is hostile: any X client may
    /// write any property on any window.
    fn free_property(&mut self, pool: &[xproto::Atom]) -> Option<xproto::Atom> {
        for step in 0..pool.len() {
            let slot = (self.cursor + step) % pool.len();
            let atom = pool[slot];
            if self.incoming.iter().all(|(used, _)| *used != atom) {
                self.cursor = (slot + 1) % pool.len();
                return Some(atom);
            }
        }
        None
    }

    /// Asks a peer for `mime`, and hands back the end to read it from.
    pub(super) fn request(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selection: Selection,
        mime: &str,
    ) -> Result<OwnedFd, Unasked> {
        self.request_from(ask, atom_of(ask.atoms, selection), mime)
    }

    /// The same, against any selection atom — `XdndSelection` is one too.
    pub(super) fn request_from(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selection: xproto::Atom,
        mime: &str,
    ) -> Result<OwnedFd, Unasked> {
        let Ask { connection, atoms, types, window, time } = *ask;
        let target = types.atom(mime).ok_or(Unasked::Unavailable)?;
        let property = self.free_property(&atoms.conversions()).ok_or(Unasked::Busy)?;
        let (read, write) = selection_pipe().ok_or(Unasked::Unavailable)?;
        // The property is deleted first so the arrival of *this* conversion is
        // distinguishable from whatever was left in it by the last one.
        let _ = connection.delete_property(window, property);
        connection
            .convert_selection(window, selection, target, property, time)
            .map_err(|_| Unasked::Unavailable)?;
        let _ = connection.flush();
        let incoming = Incoming::new(selection, target, Some(write), false, Instant::now());
        self.incoming.push_back((property, incoming));
        Ok(read)
    }

    /// Asks an owner what it offers. The reply lands in [`Self::arrived`].
    pub(super) fn ask_targets(&mut self, ask: &Ask<'_, impl Connection>, selection: Selection) {
        let Ask { connection, atoms, window, time, .. } = *ask;
        let asked = atom_of(atoms, selection);
        let Some(property) = self.targets_property(&atoms.conversions(), asked) else { return };
        let _ = connection.delete_property(window, property);
        if connection.convert_selection(window, asked, atoms.TARGETS, property, time).is_err() {
            return;
        }
        let _ = connection.flush();
        // No pipe: the answer is a list of atoms for the offer table, not bytes
        // for a caller. `arrived` tells the two apart by this being `None`.
        let primary = matches!(selection, Selection::Primary);
        let incoming = Incoming::new(asked, atoms.TARGETS, None, primary, Instant::now());
        self.incoming.push_back((property, incoming));
    }

    /// A conversion we asked for has landed.
    pub(super) fn arrived(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        transfers: &mut Vec<Transfer>,
        event: &xproto::SelectionNotifyEvent,
    ) -> Option<Offered> {
        let Ask { connection, atoms, types, window, .. } = *ask;
        let Some(index) = self.answered(event) else {
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
        let words = self.incoming.get(index).map_or(0, |(_, incoming)| incoming.words());
        let reply = connection
            .get_property(false, window, event.property, xproto::AtomEnum::ANY, 0, words)
            .ok()?
            .reply()
            .ok()?;

        if reply.type_ == atoms.INCR {
            // Not refused on the size it announces, even one past the limit: the
            // reader learns why only from more bytes than its limit, so the
            // answer is collected until it gets there.
            let _ = connection.delete_property(window, event.property);
            let _ = connection.flush();
            if let Some((_, incoming)) = self.incoming.get_mut(index) {
                incoming.incremental = true;
            }
            return None;
        }

        let (_, mut incoming) = self.incoming.remove(index)?;
        let _ = connection.delete_property(window, event.property);
        if reply.bytes_after > 0 {
            incoming.past_the_limit(&reply.value, transfers);
            return None;
        }
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

    /// A property for a `TARGETS` question about `selection`, after forgetting
    /// any earlier one still unanswered.
    ///
    /// **At most one per selection.** One is asked every time a selection
    /// changes hands, and they shared the pool with pastes and drops: an owner
    /// that never answered, or a client re-taking PRIMARY over and over, filled
    /// every property with questions nobody would answer, and each paste after
    /// that was refused. The earlier question is about an owner that has since
    /// lost the selection, so its answer was stale anyway.
    fn targets_property(
        &mut self,
        pool: &[xproto::Atom],
        selection: xproto::Atom,
    ) -> Option<xproto::Atom> {
        self.incoming.retain(|(_, it)| it.pipe.is_some() || it.selection != selection);
        self.free_property(pool)
    }

    /// Which conversion in flight `event` answers: by its property, or by what
    /// was asked for when a refusal names none.
    fn answered(&self, event: &xproto::SelectionNotifyEvent) -> Option<usize> {
        if event.property == 0 {
            self.incoming
                .iter()
                .position(|(_, it)| it.selection == event.selection && it.target == event.target)
        } else {
            self.incoming.iter().position(|(atom, _)| *atom == event.property)
        }
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
        let words = self.incoming.get(index).map_or(0, |(_, incoming)| incoming.words());
        let Some(reply) = connection
            .get_property(true, event.window, event.atom, xproto::AtomEnum::ANY, 0, words)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        let _ = connection.flush();
        self.collect(index, &reply.value, reply.bytes_after, transfers);
    }

    /// Adds one property of an `INCR` transfer, already read, to what has
    /// arrived for the conversion at `index`.
    ///
    /// `beyond` is the reply's `bytes_after`: what the property held past what
    /// [`Incoming::words`] asked for, which is anything over the limit.
    fn collect(&mut self, index: usize, chunk: &[u8], beyond: u32, transfers: &mut Vec<Transfer>) {
        let Some((_, incoming)) = self.incoming.get_mut(index) else { return };
        if beyond > 0 || !incoming.fits(chunk.len()) {
            if let Some((_, incoming)) = self.incoming.remove(index) {
                incoming.past_the_limit(chunk, transfers);
            }
            return;
        }
        if !chunk.is_empty() {
            incoming.collected.extend_from_slice(chunk);
            return;
        }
        // A zero-length property ends an `INCR` transfer, and everything
        // collected goes to the caller in one piece.
        if let Some((_, mut incoming)) = self.incoming.remove(index)
            && let Some(pipe) = incoming.pipe.take()
        {
            let bytes = std::mem::take(&mut incoming.collected);
            send_to(transfers, pipe, bytes.into());
        }
    }

    /// Abandons every conversion whose peer has had [`SELECTION_TIMEOUT`], and
    /// [`OUTLIVES_THE_READER`] more, and not finished.
    ///
    /// Dropping an entry drops its pipe's write end, which is what turns a peer
    /// that went quiet into an end-of-file the reading job can act on. Without
    /// it a drop whose source stopped between two chunks held a job blocked on
    /// that pipe, and the entry itself, for the life of the process.
    pub(super) fn expire(&mut self, now: Instant) {
        self.incoming.retain(|(_, incoming)| incoming.deadline > now);
    }

    /// How long until the next conversion is abandoned.
    ///
    /// `None` with nothing in flight, so a window with no paste or drop
    /// outstanding still sleeps with no timeout at all.
    pub(super) fn next_deadline(&self, now: Instant) -> Option<Duration> {
        self.incoming
            .iter()
            .map(|(_, incoming)| incoming.deadline.saturating_duration_since(now))
            .min()
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
    kde: xproto::Atom,
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
            kde: atoms.kde_cutselection,
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
            "application/x-kde-cutselection" => self.kde,
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
        } else if atom == self.kde {
            Some("application/x-kde-cutselection")
        } else if atom == self.gnome {
            Some("x-special/gnome-copied-files")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// One `INCR` conversion in flight, with the end its reader holds.
    fn incremental(limit: usize, now: Instant) -> (Selections, OwnedFd) {
        let (read, write) = selection_pipe().unwrap();
        let mut incoming = Incoming::new(1, TYPES.uri_list, Some(write), false, now);
        incoming.limit = limit;
        incoming.incremental = true;
        let mut selections = Selections::default();
        selections.incoming.push_back((1, incoming));
        (selections, read)
    }

    /// What a reader with `limit` ends up with, read the way every caller
    /// reads. A pipe whose writer has gone reads to end-of-file at once; one
    /// still held times out, which is the failure.
    fn read(fd: &OwnedFd, limit: usize) -> std::io::Result<Vec<u8>> {
        crate::clipboard::receive(fd, limit, Instant::now() + Duration::from_millis(200))
    }

    #[test]
    fn an_incremental_answer_reaches_the_reader_whole_at_its_terminator() {
        let (mut selections, reader) = incremental(8, Instant::now());
        let mut transfers = Vec::new();
        for chunk in [&b"file"[..], b"://a", b""] {
            selections.collect(0, chunk, 0, &mut transfers);
        }
        assert_eq!(selections.awaiting(), 0, "a finished transfer stayed in flight");
        assert_eq!(read(&reader, 8).unwrap(), b"file://a", "exactly the limit is not past it");
    }

    /// A peer that owns the clipboard and answers with `INCR` decides how much
    /// it sends, and the reader has to learn which limit stopped it. Watched
    /// failing with the entry dropped instead: the reader read an empty answer,
    /// which a drop takes in silence.
    #[test]
    fn an_incremental_answer_past_the_limit_reaches_the_reader_as_too_large() {
        let (mut selections, reader) = incremental(8, Instant::now());
        let mut transfers = Vec::new();
        selections.collect(0, b"file", 0, &mut transfers);
        selections.collect(0, b"://ab", 0, &mut transfers);
        assert_eq!(selections.awaiting(), 0, "an over-limit transfer was kept");
        selections.collect(0, b"cdef", 0, &mut transfers);
        selections.collect(0, b"", 0, &mut transfers);
        let refused = read(&reader, 8).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::FileTooLarge);
    }

    /// The server says a property was longer than what was asked for, which is
    /// the room left under the limit — so a chunk that ends exactly at the
    /// limit is still over, and the reader is told so.
    #[test]
    fn a_property_with_bytes_left_behind_is_past_the_limit() {
        let (mut selections, reader) = incremental(8, Instant::now());
        selections.collect(0, b"file://a", 1, &mut Vec::new());
        assert_eq!(selections.awaiting(), 0);
        let refused = read(&reader, 8).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn a_property_is_asked_for_no_further_than_the_limit_leaves_room() {
        let (mut selections, _reader) = incremental(10, Instant::now());
        assert_eq!(selections.incoming[0].1.words(), 3);
        selections.collect(0, b"abc", 0, &mut Vec::new());
        assert_eq!(selections.incoming[0].1.words(), 2);
        selections.collect(0, b"defghij", 0, &mut Vec::new());
        assert_eq!(selections.incoming[0].1.words(), 0);
    }

    /// A drop whose source stops between two chunks never sends the terminator
    /// and never says so. Watched failing with `expire` emptied: the entry, and
    /// the write end its reader waits on, outlived the deadline.
    #[test]
    fn a_conversion_whose_peer_goes_quiet_is_abandoned_at_its_deadline() {
        let asked = Instant::now();
        let deadline = SELECTION_TIMEOUT + OUTLIVES_THE_READER;
        let (mut selections, reader) = incremental(SELECTION_LIMIT, asked);
        selections.collect(0, b"file://a", 0, &mut Vec::new());
        assert_eq!(selections.next_deadline(asked), Some(deadline));

        // Still held when the reader's own timeout runs out, so the reader's
        // error is the one a person is told.
        selections.expire(asked + SELECTION_TIMEOUT);
        assert_eq!(selections.awaiting(), 1, "abandoned before the reader gave up");
        selections.expire(asked + deadline.saturating_sub(Duration::from_millis(1)));
        assert_eq!(selections.awaiting(), 1, "abandoned before its deadline");

        selections.expire(asked + deadline);
        assert_eq!(selections.awaiting(), 0, "kept past its deadline");
        assert_eq!(read(&reader, SELECTION_LIMIT).unwrap(), b"", "the reader is still waiting");
        assert_eq!(selections.next_deadline(asked), None, "an idle window would still wake");
    }

    /// Takes a property from `pool` and puts a conversion in it, as a request
    /// does.
    fn ask(selections: &mut Selections, pool: &[xproto::Atom]) -> Option<xproto::Atom> {
        let property = selections.free_property(pool)?;
        let (_, write) = selection_pipe().unwrap();
        let incoming = Incoming::new(1, TYPES.uri_list, Some(write), false, Instant::now());
        selections.incoming.push_back((property, incoming));
        Some(property)
    }

    /// Puts a `TARGETS` question in flight, as a change of owner does.
    fn ask_what_is_offered(selections: &mut Selections, pool: &[xproto::Atom], of: xproto::Atom) {
        if let Some(property) = selections.targets_property(pool, of) {
            let incoming = Incoming::new(of, TYPES.targets, None, false, Instant::now());
            selections.incoming.push_back((property, incoming));
        }
    }

    /// Two conversions in one property is how a late peer's chunk became the
    /// next paste. Watched failing with every request given `pool[0]`.
    #[test]
    fn every_conversion_in_flight_lands_in_a_property_of_its_own() {
        let pool = [10, 11, 12];
        let mut selections = Selections::default();
        let taken: Vec<_> = (0..3).filter_map(|_| ask(&mut selections, &pool)).collect();
        assert_eq!(taken, [10, 11, 12]);
        assert_eq!(ask(&mut selections, &pool), None, "a fourth shared a property");
    }

    /// The property a peer was abandoned in is the one it may still write to,
    /// so it is the last to be handed out again.
    #[test]
    fn a_property_given_up_is_the_last_to_be_handed_out_again() {
        let pool = [10, 11, 12];
        let mut selections = Selections::default();
        assert_eq!(ask(&mut selections, &pool), Some(10));
        assert_eq!(ask(&mut selections, &pool), Some(11));
        selections.incoming.retain(|(property, _)| *property != 10);
        assert_eq!(ask(&mut selections, &pool), Some(12));
        assert_eq!(ask(&mut selections, &pool), Some(10), "only once nothing else is free");
    }

    /// A selection is asked about every time it changes hands, and a client
    /// that re-takes PRIMARY over and over without answering is enough to ask
    /// a hundred times. Watched failing with the earlier question kept: all
    /// eight properties held one, which leaves a paste none.
    #[test]
    fn owners_that_never_answer_cannot_use_up_the_properties_a_paste_needs() {
        let pool = [10, 11, 12, 13, 14, 15, 16, 17];
        let mut selections = Selections::default();
        for change in 0..100 {
            ask_what_is_offered(&mut selections, &pool, if change % 2 == 0 { 1 } else { 2 });
        }
        assert_eq!(selections.awaiting(), 2, "more than one question per selection");
        assert!(ask(&mut selections, &pool).is_some(), "a paste found no property");
    }

    /// A refusal names no property, so it is matched on what was asked — and
    /// the selection is part of that, or the primary selection's refusal of
    /// `TARGETS` took the clipboard's question with it.
    #[test]
    fn a_refusal_is_matched_on_the_selection_as_well_as_the_type() {
        let mut selections = Selections::default();
        for (property, selection) in [(10, 1), (11, 2)] {
            let incoming = Incoming::new(selection, TYPES.targets, None, false, Instant::now());
            selections.incoming.push_back((property, incoming));
        }
        let refusal = |selection| xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: 0,
            requestor: 0,
            selection,
            target: TYPES.targets,
            property: 0,
        };
        assert_eq!(selections.answered(&refusal(2)), Some(1));
        assert_eq!(selections.answered(&refusal(3)), None);
    }

    const TYPES: Types = Types {
        uri_list: 100,
        utf8: 101,
        plain: 102,
        plain_utf8: 103,
        gnome: 104,
        targets: 105,
        kde: 106,
    };

    /// A round trip has to land on the same MIME type, or a `TARGETS` reply
    /// lists a type a peer can ask for and never receive.
    #[test]
    fn every_published_type_survives_the_round_trip() {
        for mime in [
            "text/uri-list",
            "text/plain;charset=utf-8",
            "text/plain",
            "x-special/gnome-copied-files",
            "application/x-kde-cutselection",
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

    /// What a reader of one conversion must end up with.
    #[derive(Debug, PartialEq)]
    enum Owed {
        /// Still arriving: abandoned at its deadline, so the reader sees nothing.
        Arriving(Vec<u8>),
        /// Finished under the limit, and these are the bytes.
        Whole(Vec<u8>),
        /// Past the limit, which the reader is told rather than handed a prefix.
        TooLarge,
    }

    /// A script for [`parser_campaign_selection`]: two limits less one, then a
    /// byte per step — the conversion in bit 0, the server keeping bytes back
    /// when bits 1 and 2 are both set, and the chunk's length above them.
    fn script(limits: [u8; 2], steps: &[(u8, bool, &[u8])]) -> Vec<u8> {
        let mut out = limits.to_vec();
        for (which, beyond, chunk) in steps {
            let length = u8::try_from(chunk.len()).unwrap();
            out.push((length << 3) | (u8::from(*beyond) * 6) | (which & 1));
            out.extend_from_slice(chunk);
        }
        out
    }

    /// **An `INCR` answer is sized and paced by the peer that owns the
    /// selection** — ADR 0040's campaign over the collector, through the seam
    /// the tests above drive it by, with no X server: two conversions in flight,
    /// and chunks, terminators and held-back bytes in whatever order the input
    /// says. Seeded with those tests' own transfers.
    ///
    /// A model says what each reader must end up with, and the pipe is read to
    /// see that it did: every byte under the limit, a refusal past it, or nothing
    /// once a conversion that never finished is abandoned. [`Incoming::words`]
    /// is checked at every step against the room left, because asking for more
    /// is a reply as large as the peer cares to make.
    #[test]
    fn parser_campaign_selection() {
        let seeds = [
            script([7, 7], &[(0, false, b"file"), (0, false, b"://a"), (0, false, b"")]),
            script([7, 7], &[(0, false, b"file"), (0, false, b"://ab"), (0, false, b"")]),
            script([7, 7], &[(0, true, b"file://a")]),
            script(
                [9, 255],
                &[(0, false, b"abc"), (1, false, b"xyz"), (0, false, b"defghij"), (1, false, b"")],
            ),
        ];
        let seeds: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
        crate::fuzz_support::campaign("selection", &seeds, |bytes| {
            let (Some(&first), Some(&second)) = (bytes.first(), bytes.get(1)) else { return };
            let limits = [usize::from(first) + 1, usize::from(second) + 1];
            let mut selections = Selections::default();
            let mut readers = Vec::new();
            for (atom, limit) in [1, 2].into_iter().zip(limits) {
                let (read, write) = selection_pipe().unwrap();
                let mut incoming =
                    Incoming::new(atom, TYPES.uri_list, Some(write), false, Instant::now());
                incoming.limit = limit;
                incoming.incremental = true;
                selections.incoming.push_back((atom, incoming));
                readers.push(read);
            }
            let mut owed = [Owed::Arriving(Vec::new()), Owed::Arriving(Vec::new())];
            let mut transfers = Vec::new();
            let mut rest = &bytes[2..];
            while let Some((&step, after)) = rest.split_first() {
                let which = usize::from(step & 1);
                let beyond = u32::from(step & 6 == 6);
                let (chunk, after) = after.split_at(usize::from(step >> 3).min(after.len()));
                rest = after;
                owed[which] = match std::mem::replace(&mut owed[which], Owed::TooLarge) {
                    Owed::Arriving(_) if beyond > 0 => Owed::TooLarge,
                    Owed::Arriving(so_far) if so_far.len() + chunk.len() > limits[which] => {
                        Owed::TooLarge
                    }
                    Owed::Arriving(so_far) if chunk.is_empty() => Owed::Whole(so_far),
                    Owed::Arriving(mut so_far) => {
                        so_far.extend_from_slice(chunk);
                        Owed::Arriving(so_far)
                    }
                    finished => finished,
                };
                let atom = u32::try_from(which + 1).unwrap();
                if let Some(index) = selections
                    .incoming
                    .iter()
                    .position(|(it, held)| *it == atom && held.incremental)
                {
                    selections.collect(index, chunk, beyond, &mut transfers);
                }
                for (_, incoming) in &selections.incoming {
                    let room = incoming.limit - incoming.collected.len();
                    assert_eq!(incoming.words() as usize, room.div_ceil(4), "asked past the room");
                }
            }
            selections.expire(Instant::now() + Duration::from_secs(3600));
            assert_eq!(selections.awaiting(), 0, "a conversion outlived its deadline");
            assert!(transfers.is_empty(), "a write to a reader was left unfinished");
            for ((reader, limit), owed) in readers.iter().zip(limits).zip(owed) {
                let got = read(reader, limit);
                match owed {
                    Owed::Arriving(_) => assert_eq!(got.unwrap(), b""),
                    Owed::Whole(bytes) => assert_eq!(got.unwrap(), bytes),
                    Owed::TooLarge => {
                        assert_eq!(got.unwrap_err().kind(), std::io::ErrorKind::FileTooLarge);
                    }
                }
            }
        });
    }
}
