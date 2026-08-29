//! XDND: drag-and-drop on X11, in client messages.
//!
//! X11 has no drag protocol. XDND is a convention built out of properties and
//! `ClientMessage`s that every toolkit has implemented the same way since 1998,
//! and version 5 is what GTK, Qt and this speak.
//!
//! The shape of a drag, as a target:
//!
//! * `XdndEnter` names the source and up to three types; more than three live in
//!   the source's `XdndTypeList` property, which is why that is read whenever the
//!   flag says so rather than only when three arrive.
//! * `XdndPosition` says where, and expects an `XdndStatus` **for every one** —
//!   a source that gets no reply stops sending them and the drag stalls with the
//!   pointer still grabbed.
//! * `XdndDrop` is the release. The bytes come through a *selection*,
//!   `XdndSelection`, so the whole of the clipboard's machinery — `INCR`
//!   included — is what carries them.
//! * `XdndFinished` tells the source it may let go. Without it the source waits,
//!   and a GTK application waits for several seconds before giving up.
//!
//! As a source it is the same conversation from the other side, with one thing
//! that is easy to miss: **the window under the pointer has to be found by hand**.
//! X11 has no "who is under here" that respects the drag; `XTranslateCoordinates`
//! walks down from the root, and the toplevel that answers is the one whose
//! `XdndAware` property exists.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;

use crate::clipboard::{Content, Transfer};
use crate::drag::{Drag, DragAction};
use crate::event::Point;

use super::atoms::Atoms;
use super::selection::{Ask, Selections};

/// The version this speaks. 5 is what every current toolkit implements.
const XDND_VERSION: u32 = 5;

/// `XdndEnter`'s first data word packs the version into its top byte, and the
/// low bit says whether there are more than three types to fetch.
const MORE_TYPES: u32 = 1;

/// Which window, on which root, at what time.
///
/// One value rather than three arguments that always travel together, and the
/// timestamp is one of them for the same reason it is on every selection
/// request: `CurrentTime` is a race.
#[derive(Clone, Copy)]
pub(super) struct Where {
    /// This window.
    pub(super) window: xproto::Window,
    /// The screen's root, which every position is relative to.
    pub(super) root: xproto::Window,
    /// The timestamp of the input event that caused this.
    pub(super) time: xproto::Timestamp,
}

/// A drag from another application, over this window.
struct Incoming {
    /// The window that started it, which every reply goes back to.
    source: xproto::Window,
    /// What it says it is offering, and where the pointer is.
    drag: Drag,
    /// The version the source speaks. XDND gained `XdndFinished`'s data words in
    /// version 5, and sending them to an older source is sending it garbage.
    version: u32,
    /// What we last told the source we would do, so the reply is not resent.
    accepted: bool,
}

/// A drag out of this window, in flight.
struct Outgoing {
    /// What is being carried, served through `XdndSelection`.
    content: Content,
    /// The toplevel currently under the pointer, if it takes drags.
    over: Option<xproto::Window>,
    /// The version *that* window speaks.
    version: u32,
    /// Whether it last said it would accept a drop.
    accepted: bool,
    /// What we are asking for.
    action: xproto::Atom,
}

/// Both directions of XDND for one window.
#[derive(Default)]
pub(super) struct Xdnd {
    incoming: Option<Incoming>,
    outgoing: Option<Outgoing>,
}

/// What a drag needs from the window starting it, at the moment it starts.
///
/// Three values rather than three parameters because they are one thing — the
/// grab this gesture runs under — and because the eighth argument is where a
/// call site stops being readable.
pub(super) struct Grab {
    pub(super) window: xproto::Window,
    pub(super) time: xproto::Timestamp,
    /// The pointer image for the whole gesture. X11 has no drag icon and no
    /// protocol for one, so the grab's cursor is the only thing that says a drag
    /// is in flight at all.
    pub(super) cursor: xproto::Cursor,
}

impl Xdnd {
    /// Declares this window able to receive drags.
    ///
    /// One property, and without it no source will ever look at this window
    /// twice — a target that does not advertise is a target nobody offers to.
    pub(super) fn advertise(connection: &impl Connection, atoms: &Atoms, window: xproto::Window) {
        let _ = connection.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms.XdndAware,
            xproto::AtomEnum::ATOM,
            &[XDND_VERSION],
        );
    }

    pub(super) const fn incoming(&self) -> Option<&Drag> {
        match &self.incoming {
            Some(incoming) => Some(&incoming.drag),
            None => None,
        }
    }

    pub(super) const fn outgoing(&self) -> bool {
        self.outgoing.is_some()
    }

    /// Handles one XDND client message. Whether it was one.
    pub(super) fn message(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
        event: &xproto::ClientMessageEvent,
        events: &mut std::collections::VecDeque<crate::Event>,
    ) -> bool {
        let data = event.data.as_data32();
        if event.type_ == atoms.XdndEnter {
            let source = data[0];
            let version = data[1] >> 24;
            let offered = if data[1] & MORE_TYPES != 0 {
                type_list(connection, atoms, source)
            } else {
                data[2..5].iter().copied().filter(|atom| *atom != 0).collect()
            };
            let offered = offered.into_iter().filter_map(|atom| mime_of(atoms, atom)).collect();
            self.incoming = Some(Incoming {
                source,
                drag: Drag { offered, actions: Vec::new(), position: Point::default() },
                version,
                accepted: false,
            });
            events.push_back(crate::Event::DragEntered { position: Point::default() });
            return true;
        }
        if event.type_ == atoms.XdndPosition {
            let Some(incoming) = &mut self.incoming else { return true };
            incoming.drag.actions = vec![action_of(atoms, data[4])];
            // Root coordinates, packed one per half-word. Turned into window
            // coordinates below, because everything above this crate works in
            // those and nothing else on this window speaks root.
            let (root_x, root_y) = ((data[2] >> 16) as i16, (data[2] & 0xffff) as i16);
            let position = window_point(connection, window, root_x, root_y);
            incoming.drag.position = position;
            events.push_back(crate::Event::DragMoved { position });
            // **The reply is not sent here.** A status for every position is
            // required — a source that gets no answer stops asking and the drag
            // stalls with the pointer grabbed — but the answer is the *caller's*,
            // and the caller has not seen this position yet. Replying now would
            // answer every position with the verdict on the one before it, so the
            // first would always be a refusal and a source that believes the
            // first refusal never asks again. `accept` sends it, and the caller
            // calls that on every `DragMoved`.
            return true;
        }
        if event.type_ == atoms.XdndLeave {
            if self.incoming.take().is_some() {
                events.push_back(crate::Event::DragLeft);
            }
            return true;
        }
        if event.type_ == atoms.XdndDrop {
            let position =
                self.incoming.as_ref().map_or_else(Point::default, |it| it.drag.position);
            events.push_back(crate::Event::Dropped { position });
            return true;
        }
        if event.type_ == atoms.XdndStatus {
            if let Some(outgoing) = &mut self.outgoing {
                outgoing.accepted = data[1] & 1 != 0;
            }
            return true;
        }
        if event.type_ == atoms.XdndFinished {
            // The peer has what it asked for. **Not a move**, whatever action it
            // reports: see the header of `crate::drag`.
            self.outgoing = None;
            return true;
        }
        false
    }

    /// Tells the source what would happen if the drag were dropped now.
    pub(super) fn accept(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
        mime: Option<&str>,
        action: DragAction,
    ) {
        let _ = action;
        let Some(incoming) = &mut self.incoming else { return };
        incoming.accepted = mime.is_some();
        let (source, accepted) = (incoming.source, incoming.accepted);
        status(connection, atoms, window, source, accepted);
    }

    /// Reads what was dropped, and tells the source it may let go.
    pub(super) fn take(
        &mut self,
        ask: &Ask<'_, impl Connection>,
        selections: &mut Selections,
        transfers: &mut Vec<Transfer>,
        mime: &str,
    ) -> Option<std::os::fd::OwnedFd> {
        let _ = transfers;
        // **`XdndFinished` is not sent here.** The specification puts it after
        // the transfer, and sending it with the conversion still in flight tells
        // a source it may stop serving before it has served — the same race the
        // Wayland half had, where one drag in three from a GTK peer transferred
        // nothing. The caller closes it with `finish` once the bytes are read.
        let _ = &self.incoming;
        selections.request_from(ask, ask.atoms.XdndSelection, mime)
    }

    /// Abandons a drag out of this window.
    pub(super) fn cancel(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
        time: xproto::Timestamp,
    ) {
        let _ = connection.ungrab_pointer(time);
        if let Some(outgoing) = self.outgoing.take()
            && let Some(target) = outgoing.over
        {
            send(connection, target, atoms.XdndLeave, [window, 0, 0, 0, 0]);
        }
        self.incoming = None;
        let _ = connection.flush();
    }

    /// Ends a drop without reading it.
    pub(super) fn finish(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
    ) {
        let Some(incoming) = self.incoming.take() else { return };
        finished(connection, atoms, window, incoming.source, true, incoming.version);
    }

    /// Starts a drag out of this window.
    pub(super) fn start(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        grab: &Grab,
        content: Content,
        actions: &[DragAction],
    ) -> bool {
        let Grab { window, time, cursor } = *grab;
        if self.outgoing.is_some() {
            return false;
        }
        // The bytes travel as a selection, so the whole of the clipboard's
        // machinery carries them — `INCR` included, which a directory's worth of
        // URIs needs.
        if connection.set_selection_owner(window, atoms.XdndSelection, time).is_err() {
            return false;
        }
        // Every type, because `XdndEnter` carries three and the rest come from
        // here. Written whether or not there are more than three: a source that
        // sets the flag and has no list is one a target reads nothing from.
        let types: Vec<xproto::Atom> =
            content.types.iter().filter_map(|(mime, _)| atom_of(atoms, mime)).collect();
        let _ = connection.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms.XdndTypeList,
            xproto::AtomEnum::ATOM,
            &types,
        );
        // The pointer has to be ours for the whole gesture: XDND is driven by
        // motion, and a drag that loses the pointer half way leaves a peer
        // waiting for a position that never comes.
        let grabbed = connection
            .grab_pointer(
                false,
                window,
                xproto::EventMask::BUTTON_RELEASE | xproto::EventMask::POINTER_MOTION,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                x11rb::NONE,
                // **The grab's cursor is the drag's cursor.** X11 has no drag
                // icon and no protocol for one; the shape a person sees for the
                // whole gesture is whatever the grab was taken with, and
                // `x11rb::NONE` here left it as the arrow it was before the
                // button went down.
                cursor,
                time,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.status == xproto::GrabStatus::SUCCESS);
        if !grabbed {
            return false;
        }
        self.outgoing = Some(Outgoing {
            content,
            over: None,
            version: XDND_VERSION,
            accepted: false,
            action: action_atom(atoms, actions.first().copied().unwrap_or_default()),
        });
        let _ = connection.flush();
        true
    }

    /// Follows the pointer while a drag out of this window is in flight.
    pub(super) fn moved(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        at: Where,
        root_x: i16,
        root_y: i16,
    ) {
        let Where { window, root, time } = at;
        let Some(outgoing) = &mut self.outgoing else { return };
        let under = aware_window_at(connection, atoms, root, root_x, root_y);
        if under.map(|(target, _)| target) != outgoing.over {
            if let Some(old) = outgoing.over {
                send(connection, old, atoms.XdndLeave, [window, 0, 0, 0, 0]);
            }
            outgoing.accepted = false;
            outgoing.over = under.map(|(target, _)| target);
            outgoing.version = under.map_or(XDND_VERSION, |(_, version)| version);
            if let Some((target, version)) = under {
                let types: Vec<xproto::Atom> = outgoing
                    .content
                    .types
                    .iter()
                    .filter_map(|(mime, _)| atom_of(atoms, mime))
                    .collect();
                let more = u32::from(types.len() > 3) | (version.min(XDND_VERSION) << 24);
                send(
                    connection,
                    target,
                    atoms.XdndEnter,
                    [
                        window,
                        more,
                        types.first().copied().unwrap_or(0),
                        types.get(1).copied().unwrap_or(0),
                        types.get(2).copied().unwrap_or(0),
                    ],
                );
            }
        }
        if let Some(target) = outgoing.over {
            let packed =
                (u32::from(root_x.cast_unsigned()) << 16) | u32::from(root_y.cast_unsigned());
            send(
                connection,
                target,
                atoms.XdndPosition,
                [window, 0, packed, time, outgoing.action],
            );
        }
        let _ = connection.flush();
    }

    /// The button came up. Whether anything was dropped on a peer.
    pub(super) fn released(
        &mut self,
        connection: &impl Connection,
        atoms: &Atoms,
        window: xproto::Window,
        time: xproto::Timestamp,
    ) -> bool {
        let _ = connection.ungrab_pointer(time);
        let Some(outgoing) = &mut self.outgoing else { return false };
        let Some(target) = outgoing.over.filter(|_| outgoing.accepted) else {
            // Nobody took it. `XdndLeave` rather than silence, so a peer that was
            // showing a drop cursor stops.
            if let Some(target) = outgoing.over {
                send(connection, target, atoms.XdndLeave, [window, 0, 0, 0, 0]);
            }
            self.outgoing = None;
            let _ = connection.flush();
            return false;
        };
        send(connection, target, atoms.XdndDrop, [window, 0, time, 0, 0]);
        let _ = connection.flush();
        // Kept until `XdndFinished`: the target asks for the bytes *after* the
        // drop, and a source that forgot what it was carrying serves nothing.
        true
    }
}

/// Sends one XDND client message.
fn send(connection: &impl Connection, to: xproto::Window, kind: xproto::Atom, data: [u32; 5]) {
    let event = xproto::ClientMessageEvent::new(32, to, kind, data);
    let _ = connection.send_event(false, to, xproto::EventMask::NO_EVENT, event);
}

/// Replies to an `XdndPosition`.
fn status(
    connection: &impl Connection,
    atoms: &Atoms,
    window: xproto::Window,
    source: xproto::Window,
    accepted: bool,
) {
    // A zero rectangle means "ask me again for every position", which is what a
    // list whose rows each answer differently needs.
    send(
        connection,
        source,
        atoms.XdndStatus,
        [window, u32::from(accepted), 0, 0, if accepted { atoms.XdndActionCopy } else { 0 }],
    );
    let _ = connection.flush();
}

/// Tells a source the drop is over.
fn finished(
    connection: &impl Connection,
    atoms: &Atoms,
    window: xproto::Window,
    source: xproto::Window,
    accepted: bool,
    version: u32,
) {
    // The accepted flag and the action are version 5's. A source older than that
    // reads whatever is in those words as its own meaning, so they stay zero.
    let (flag, action) = if version >= 5 && accepted { (1, atoms.XdndActionCopy) } else { (0, 0) };
    send(connection, source, atoms.XdndFinished, [window, flag, action, 0, 0]);
    let _ = connection.flush();
}

/// The types a source lists in `XdndTypeList`, for the more-than-three case.
fn type_list(
    connection: &impl Connection,
    atoms: &Atoms,
    source: xproto::Window,
) -> Vec<xproto::Atom> {
    connection
        .get_property(false, source, atoms.XdndTypeList, xproto::AtomEnum::ATOM, 0, 64)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(Iterator::collect))
        .unwrap_or_default()
}

/// The topmost `XdndAware` window at a root position, and the version it speaks.
///
/// Walked down from the root by hand, because X11 has no request that answers
/// "who would take a drop here": `QueryPointer` names the child under the
/// pointer, and the toplevel that matters may be several frames above the one it
/// returns.
fn aware_window_at(
    connection: &impl Connection,
    atoms: &Atoms,
    root: xproto::Window,
    x: i16,
    y: i16,
) -> Option<(xproto::Window, u32)> {
    let mut window = root;
    for _ in 0..16 {
        let aware = connection
            .get_property(false, window, atoms.XdndAware, xproto::AtomEnum::ATOM, 0, 1)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32().and_then(|mut it| it.next()));
        if let Some(version) = aware {
            return Some((window, version));
        }
        let reply = connection
            .translate_coordinates(root, window, x, y)
            .ok()
            .and_then(|cookie| cookie.reply().ok())?;
        if reply.child == x11rb::NONE {
            return None;
        }
        window = reply.child;
    }
    None
}

/// A root position, in this window's logical pixels.
fn window_point(connection: &impl Connection, window: xproto::Window, x: i16, y: i16) -> Point {
    connection
        .translate_coordinates(
            connection.setup().roots.first().map_or(window, |screen| screen.root),
            window,
            x,
            y,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map_or_else(Point::default, |reply| {
            Point::new(f32::from(reply.dst_x), f32::from(reply.dst_y))
        })
}

/// The atom for one action, and the action for one atom.
fn action_atom(atoms: &Atoms, action: DragAction) -> xproto::Atom {
    match action {
        DragAction::Copy => atoms.XdndActionCopy,
        DragAction::Move => atoms.XdndActionMove,
        DragAction::Link => atoms.XdndActionLink,
        DragAction::Ask | DragAction::Private => atoms.XdndActionPrivate,
    }
}

fn action_of(atoms: &Atoms, atom: xproto::Atom) -> DragAction {
    if atom == atoms.XdndActionMove {
        DragAction::Move
    } else if atom == atoms.XdndActionLink {
        DragAction::Link
    } else if atom == atoms.XdndActionPrivate {
        DragAction::Private
    } else {
        DragAction::Copy
    }
}

/// The MIME name of a type atom, and the atom of a name.
fn mime_of(atoms: &Atoms, atom: xproto::Atom) -> Option<String> {
    super::selection::Types::new(atoms).mime(atom).map(str::to_owned)
}

fn atom_of(atoms: &Atoms, mime: &str) -> Option<xproto::Atom> {
    super::selection::Types::new(atoms).atom(mime)
}
