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

use std::os::fd::{AsFd, OwnedFd};

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, event_created_child};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};

use super::State;
use crate::clipboard::{Content, Selection, send_to};
use crate::event::Event;

/// The Wayland transport for the two selections.
///
/// The protocol objects only. What a caller reads — which types the peer is
/// offering — is [`crate::Clipboard`], held here as `shared` so there is one
/// copy of it and both backends fill the same thing.
#[derive(Default)]
pub struct Selections {
    /// What the peer offers, in the crate-neutral shape.
    pub(crate) shared: crate::clipboard::Clipboard,

    pub(super) device: Option<wl_data_device::WlDataDevice>,
    pub(super) primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,

    /// What we are publishing, kept because the peer asks for it later.
    pub(super) published: Option<Content>,
    pub(super) published_primary: Option<Content>,
    pub(super) source: Option<wl_data_source::WlDataSource>,
    pub(super) primary_source: Option<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1>,

    /// The peer's current offers, and what they contain.
    pub(super) offer: Option<wl_data_offer::WlDataOffer>,
    pub(super) primary_offer: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
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
    pub fn clipboard(&self) -> &crate::Clipboard {
        &self.state.clipboard.shared
    }

    /// Starts a drag carrying `content`. Whether the compositor took it.
    ///
    /// Needs the serial of a **button press**, which is what makes a drag legal:
    /// the protocol will not let a client start one from nothing, and there is no
    /// event for the refusal.
    pub fn start_drag(&mut self, content: crate::Content, actions: &[crate::DragAction]) -> bool {
        if self.state.drags.out {
            return false;
        }
        let (Some(manager), Some(serial)) =
            (self.state.data_manager.clone(), self.state.seat.button_serial)
        else {
            return false;
        };
        let Some(device) = self.state.clipboard.device.clone() else { return false };
        let handle = self.queue.handle();
        let source = manager.create_data_source(&handle, ());
        for (mime, _) in &content.types {
            source.offer(mime.clone());
        }
        source.set_actions(super::actions_of(actions));
        // No icon surface: the compositor draws its own indication, and a
        // surface of ours would be a second thing to keep at the right scale for
        // no gain a person can see.
        device.start_drag(Some(&source), &self.state.surface, None, serial);
        self.state.drags.source = Some(source);
        self.state.drags.carrying = Some(content);
        self.state.drags.out = true;
        let _ = self.connection.flush();
        true
    }

    /// Whether a drag out of this window is still in flight.
    #[must_use]
    pub const fn dragging_out(&self) -> bool {
        self.state.drags.out
    }

    /// The drag hovering over this window, if one is.
    #[must_use]
    pub const fn dragging(&self) -> Option<&crate::Drag> {
        self.state.drags.over.as_ref()
    }

    /// Which actions this display server can express at all.
    #[must_use]
    pub fn drag_actions(&self) -> &'static [crate::DragAction] {
        use crate::DragAction;
        &[DragAction::Copy, DragAction::Move, DragAction::Ask]
    }

    /// Tells the source what would happen if the drag were dropped now.
    ///
    /// **Both halves, every time.** `accept` is what makes the pointer show a
    /// "you may drop this" cursor and `set_actions` is what decides which one;
    /// a target that accepts without setting an action gets a refusal cursor
    /// over a surface that would in fact take the drop.
    pub fn accept_drag(&mut self, mime: Option<&str>, action: crate::DragAction) {
        let Some(offer) = &self.state.drags.offer else { return };
        offer.accept(self.state.drags.serial, mime.map(str::to_owned));
        let bits = super::actions_of(&[action]);
        offer.set_actions(bits, bits);
        let _ = self.connection.flush();
    }

    /// Abandons a drag out of this window.
    ///
    /// For a gesture that ended inside this window: the drop is ours to perform
    /// and the display server has nothing left to arrange. Without it a
    /// compositor that never sends `drop` or `dnd_finished` — measured, this one
    /// — leaves a source alive and a stale `enter` arriving after the fact.
    pub fn cancel_drag(&mut self) {
        self.state.drags.out = false;
        if let Some(source) = self.state.drags.source.take() {
            source.destroy();
        }
        self.state.drags.carrying = None;
        if let Some(offer) = self.state.drags.offer.take() {
            offer.destroy();
        }
        self.state.drags.over = None;
        self.state.drags.dropped = false;
        let _ = self.connection.flush();
    }

    /// Ends a drop without reading it.
    ///
    /// For a drag this window started and landed itself: the paths are already
    /// in hand, and asking a pipe for what is on the other side of it would be a
    /// round trip to fetch something we published. The source still has to be
    /// told, or it keeps the pointer grabbed waiting for a `finish` that never
    /// comes.
    pub fn finish_drop(&mut self) {
        let Some(offer) = self.state.drags.offer.take() else { return };
        if wayland_client::Proxy::version(&offer) >= 3 {
            offer.finish();
        }
        offer.destroy();
        self.state.drags.over = None;
        self.state.drags.dropped = false;
        let _ = self.connection.flush();
    }

    /// Reads what was dropped, as a descriptor.
    ///
    /// Called after [`crate::Event::Dropped`]. `finish` and the destroy happen
    /// here rather than being left to the caller: a source waits for `finish`
    /// before it considers the drag over, and one that never gets it leaves a
    /// grab on the pointer.
    pub fn take_drop(&mut self, mime: &str) -> Option<std::os::fd::OwnedFd> {
        let offer = self.state.drags.offer.as_ref()?;
        let (read, write) = rustix::pipe::pipe().ok()?;
        offer.receive(mime.to_owned(), write.as_fd());
        drop(write);
        let _ = self.connection.flush();
        // **The offer stays until the bytes are read.** `wl_data_offer` says
        // `finish` comes *after* reading the data, and calling it here raced the
        // source's `send`: measured, one drag in three from a GTK peer produced
        // no transfer at all, with our side of it looking identical in the trace.
        // The caller closes it with `finish_drop` once the read is done.
        Some(read)
    }
}

/// Creates the data devices, **before the first surface commit**.
///
/// A data device created after the first keyboard `enter` never learns the
/// current selection, and the symptom is an empty clipboard with nothing
/// reporting an error — which is exactly how Spike D lost an afternoon.
pub(super) fn open(
    handle: &QueueHandle<State>,
    seat: &wl_seat::WlSeat,
    data: Option<&wl_data_device_manager::WlDataDeviceManager>,
    primary: Option<&zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1>,
) -> Selections {
    Selections {
        device: data.map(|manager| manager.get_data_device(seat, handle, ())),
        primary_device: primary.map(|manager| manager.get_device(seat, handle, ())),
        ..Selections::default()
    }
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
                // the `selection` or `enter` that adopts it, so it is parked here
                // — and the types are kept separately, because a drag's `enter`
                // comes after them and needs the ones that have already arrived.
                state.pending_offer = Some(id);
                state.pending_types.clear();
                state.drags.offer_types_in_flight.clear();
            }
            wl_data_device::Event::Selection { id } => {
                if let Some(old) = state.clipboard.offer.take() {
                    old.destroy();
                }
                state.clipboard.offer = id;
                state.clipboard.shared.offer_types = std::mem::take(&mut state.pending_types);
                state.pending_offer = None;
            }
            // A drag entered the window. An offer is adopted by *either*
            // `selection` or `enter`, so the one parked above belongs to this
            // drag and to nobody else — and until there is a drop target, saying
            // so and destroying it is the honest answer. Leaving it parked leaked
            // one server-side object per drag across the window.
            wl_data_device::Event::Enter { id, x, y, serial, .. } => {
                state.pending_types.clear();
                state.pending_offer = None;
                if let Some(old) = state.drags.offer.take() {
                    old.destroy();
                }
                let types = std::mem::take(&mut state.drags.offer_types_in_flight);
                state.drags.offer = id;
                state.drags.serial = serial;
                let position = state.logical_point(x, y);
                state.drags.over =
                    Some(crate::drag::Drag { offered: types, actions: Vec::new(), position });
                state.events.push_back(Event::DragEntered { position });
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                let position = state.logical_point(x, y);
                if let Some(drag) = &mut state.drags.over {
                    drag.position = position;
                }
                state.events.push_back(Event::DragMoved { position });
            }
            // **The offer must survive this.** Hyprland brackets a drop with
            // `leave`, `drop`, `leave` — measured — so destroying the offer here
            // left nothing to call `receive` on and every drop read zero bytes,
            // which looks exactly like the compositor refusing us. The offer is
            // released when the *drop* is finished or when a leave arrives with
            // no drop pending.
            wl_data_device::Event::Leave => {
                state.pending_types.clear();
                if let Some(offer) = state.pending_offer.take() {
                    offer.destroy();
                }
                if state.drags.dropped {
                    return;
                }
                if let Some(offer) = state.drags.offer.take() {
                    offer.destroy();
                }
                if state.drags.over.take().is_some() {
                    state.events.push_back(Event::DragLeft);
                }
            }
            wl_data_device::Event::Drop => {
                let position =
                    state.drags.over.as_ref().map(|drag| drag.position).unwrap_or_default();
                state.drags.dropped = true;
                state.events.push_back(Event::Dropped { position });
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
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                // Both lists: a `selection` takes `pending_types` and a drag's
                // `enter` takes the other, and the two arrive through the same
                // events with nothing yet saying which this offer will become.
                state.pending_types.push(mime_type.clone());
                state.drags.offer_types_in_flight.push(mime_type);
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                if let (Some(drag), WEnum::Value(actions)) = (&mut state.drags.over, source_actions)
                {
                    drag.actions = super::actions_from(actions);
                }
            }
            _ => {}
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
                // A drag's source and the clipboard's are different objects
                // carrying different bytes, and both arrive here.
                let holding = if state.drags.source.as_ref() == Some(source) {
                    state.drags.carrying.as_ref()
                } else {
                    state.clipboard.published.as_ref()
                };
                let bytes = holding
                    .and_then(|content| content.share(&mime_type))
                    .unwrap_or_else(|| std::sync::Arc::from(&[][..]));
                send_to(&mut state.transfers, fd, bytes);
            }
            // **The drag is over and nothing is owed.** A source must keep
            // dispatching after the button comes up — the target asks for the
            // bytes *after* the drop — so the release of what is being carried
            // happens here and not on the release of the button.
            wl_data_source::Event::DndFinished | wl_data_source::Event::DndDropPerformed
                if state.drags.source.as_ref() == Some(source) =>
            {
                // **Not a move, whatever the peer asked for.** Spike D measured
                // the negotiated action reported zero times across every peer,
                // and `dnd_finished` delivered by some and not others. Deleting
                // an original on a confirmation that may never come is how a
                // drag loses somebody's files, so this crate never reports one.
                state.drags.out = false;
            }
            wl_data_source::Event::Cancelled => {
                if state.drags.source.as_ref() == Some(source) {
                    state.drags.source = None;
                    state.drags.carrying = None;
                    state.drags.out = false;
                    source.destroy();
                    return;
                }
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
                state.clipboard.shared.primary_types =
                    std::mem::take(&mut state.pending_primary_types);
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
                    .and_then(|content| content.share(&mime_type))
                    .unwrap_or_else(|| std::sync::Arc::from(&[][..]));
                send_to(&mut state.transfers, fd, bytes);
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
