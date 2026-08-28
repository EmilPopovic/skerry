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
use wayland_client::{Connection, Dispatch, QueueHandle, event_created_child};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1, zwp_primary_selection_device_v1,
    zwp_primary_selection_offer_v1, zwp_primary_selection_source_v1,
};

use super::State;
use crate::clipboard::{Content, Selection, send_to};

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
                // the `selection` that adopts it, so it is parked here.
                state.pending_offer = Some(id);
                state.pending_types.clear();
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
            wl_data_device::Event::Enter { id, .. } => {
                state.pending_types.clear();
                state.pending_offer = None;
                if let Some(offer) = id {
                    offer.destroy();
                }
            }
            wl_data_device::Event::Leave => {
                state.pending_types.clear();
                if let Some(offer) = state.pending_offer.take() {
                    offer.destroy();
                }
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
                    .and_then(|content| content.share(&mime_type))
                    .unwrap_or_else(|| std::sync::Arc::from(&[][..]));
                send_to(&mut state.transfers, fd, bytes);
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
