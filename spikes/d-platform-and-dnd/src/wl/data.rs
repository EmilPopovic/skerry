// SPDX-License-Identifier: Apache-2.0
//! `wl_data_device`: clipboard, primary selection, and drag-and-drop as source
//! *and* target. This file is the reason ADR 0002 exists — there is no policy
//! here that a general-purpose windowing crate would have made for us, and every
//! decision in it is one a file manager has an opinion about.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager::DndAction, wl_data_offer, wl_data_source, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1, zwp_primary_selection_offer_v1,
    zwp_primary_selection_source_v1,
};

use super::App;

/// What a file manager puts on the clipboard. `text/uri-list` is the
/// specification; the other two are what GNOME and KDE actually read, and
/// omitting them is why cut-and-paste between file managers so often does
/// nothing. See [05-platform-integration.md](../../../docs/05-platform-integration.md).
pub const URI_LIST: &str = "text/uri-list";
pub const TEXT: &str = "text/plain;charset=utf-8";
pub const GNOME_COPIED: &str = "x-special/gnome-copied-files";
pub const KDE_CUT: &str = "application/x-kde-cutselection";

/// Every action Wayland has. `wl_data_device_manager.dnd_action` is
/// none/copy/move/**ask** — there is no `link`, where XDND has
/// `XdndActionLink`. A "link here" entry on Wayland therefore has to be built
/// out of `ask` plus a menu of our own, and the two display servers cannot share
/// one action enum.
pub const ALL_ACTIONS: DndAction =
    DndAction::from_bits_retain(DndAction::Copy.bits() | DndAction::Move.bits() | DndAction::Ask.bits());

/// What we actually advertise. `SPIKE_D_ACTIONS=copy` narrows it to copy alone,
/// which is how the question "does including `ask` break the negotiation?" gets
/// an answer instead of a theory.
pub fn offered_actions() -> DndAction {
    match std::env::var("SPIKE_D_ACTIONS").as_deref() {
        Ok("copy") => DndAction::Copy,
        Ok("copymove") => DndAction::from_bits_retain(DndAction::Copy.bits() | DndAction::Move.bits()),
        _ => ALL_ACTIONS,
    }
}

#[derive(Default)]
pub struct Incoming {
    pub mimes: Vec<String>,
    pub source_actions: Option<DndAction>,
    pub action: Option<DndAction>,
    pub entered: bool,
    pub dropped: bool,
    pub surface_local: (f64, f64),
}

#[derive(Default)]
pub struct Outgoing {
    /// Which MIME types peers actually asked for, in order. A GTK target and a
    /// Qt target do not ask for the same thing, and this is how we find out.
    pub requested: Vec<String>,
    pub action: Option<DndAction>,
    pub target_accepted: Option<String>,
    pub dropped: bool,
    pub finished: bool,
    pub cancelled: bool,
    pub short_writes: u32,
}

#[derive(Default)]
pub struct DataState {
    pub device: Option<wl_data_device::WlDataDevice>,
    pub primary_device: Option<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1>,

    /// MIME advertisements arrive on an offer before the compositor says whether
    /// that offer is a selection or a drag, so they are accumulated by object id
    /// and claimed later.
    pub offer_mimes: HashMap<ObjectId, Vec<String>>,
    pub offer_source_actions: HashMap<ObjectId, DndAction>,
    pub primary_offer_mimes: HashMap<ObjectId, Vec<String>>,

    pub selection: Option<wl_data_offer::WlDataOffer>,
    pub selection_mimes: Vec<String>,
    pub selection_events: u32,
    pub primary_selection: Option<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1>,
    pub primary_mimes: Vec<String>,
    pub primary_events: u32,
    /// Set when a peer takes a selection away from us. A source that is cancelled
    /// and not destroyed is a leak the compositor cannot clean up.
    pub source_cancelled: u32,

    pub dnd_offer: Option<wl_data_offer::WlDataOffer>,
    pub incoming: Incoming,
    pub outgoing: Outgoing,

    /// What we serve when asked. Set before a source is created.
    pub payload: HashMap<String, Vec<u8>>,
    /// What we prefer to accept, most preferred first.
    pub accept_order: Vec<String>,
}

impl DataState {
    pub fn serve(&mut self, uris: &[&str]) {
        let uri_list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
        let paths = uris.join("\n");
        self.payload.insert(URI_LIST.into(), uri_list.clone().into_bytes());
        self.payload.insert(TEXT.into(), paths.into_bytes());
        self.payload
            .insert(GNOME_COPIED.into(), format!("copy\n{}", uris.join("\n")).into_bytes());
    }
}

/// A transfer that is read a chunk at a time from the event loop rather than in
/// one blocking call. This is rule 1 applied to the clipboard: the peer on the
/// other end of this pipe is another process, and it is allowed to be slow, hung
/// or malicious.
pub struct Transfer {
    fd: OwnedFd,
    pub bytes: Vec<u8>,
    pub done: bool,
    pub started: Instant,
}

impl Transfer {
    fn from_fd(fd: OwnedFd) -> Self {
        rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).ok();
        Self { fd, bytes: Vec::new(), done: false, started: Instant::now() }
    }

    pub fn offer(conn: &Connection, offer: &wl_data_offer::WlDataOffer, mime: &str) -> Self {
        let (r, w) = rustix::pipe::pipe().expect("pipe");
        if std::env::var_os("SPIKE_D_TRACE").is_some() {
            eprintln!("    target: receive({mime}) on offer {:?}", offer.id());
        }
        offer.receive(mime.into(), w.as_fd());
        // The peer cannot see the request until it is on the wire, and it will
        // never write to a pipe it has not been told about.
        conn.flush().ok();
        drop(w);
        Self::from_fd(r)
    }

    pub fn primary(
        conn: &Connection,
        offer: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        mime: &str,
    ) -> Self {
        let (r, w) = rustix::pipe::pipe().expect("pipe");
        offer.receive(mime.into(), w.as_fd());
        conn.flush().ok();
        drop(w);
        Self::from_fd(r)
    }

    /// Returns true when the writer closed. Never blocks.
    pub fn poll(&mut self) -> bool {
        if self.done {
            return true;
        }
        let mut buf = [0u8; 4096];
        loop {
            match std::fs::File::from(self.fd.try_clone().unwrap()).read(&mut buf) {
                Ok(0) => {
                    self.done = true;
                    return true;
                }
                Ok(n) => self.bytes.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
                Err(_) => {
                    self.done = true;
                    return true;
                }
            }
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).to_string()
    }
}

/// Pump the queue and the transfer together, recording the longest gap between
/// two dispatches. That gap is the number rule 1 cares about: a clipboard read
/// must not be able to stall a frame.
pub fn drain(
    app: &mut App,
    queue: &mut wayland_client::EventQueue<App>,
    t: &mut Transfer,
    timeout: Duration,
) -> (bool, f64) {
    let end = Instant::now() + timeout;
    let mut worst_gap_ms = 0.0f64;
    let mut last = Instant::now();
    while Instant::now() < end {
        queue.flush().ok();
        queue.dispatch_pending(app).ok();
        let now = Instant::now();
        worst_gap_ms = worst_gap_ms.max((now - last).as_secs_f64() * 1000.0);
        last = now;
        if t.poll() {
            return (true, worst_gap_ms);
        }
        if let Some(g) = queue.prepare_read() {
            g.read().ok();
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    (t.done, worst_gap_ms)
}

// ---------------------------------------------------------------------------

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                app.data.offer_mimes.insert(id.id(), Vec::new());
            }
            wl_data_device::Event::Selection { id } => {
                app.data.selection_events += 1;
                app.data.selection_mimes = id
                    .as_ref()
                    .and_then(|o| app.data.offer_mimes.get(&o.id()).cloned())
                    .unwrap_or_default();
                app.data.selection = id;
            }
            wl_data_device::Event::Enter { serial, x, y, id, .. } => {
                let mimes = id
                    .as_ref()
                    .and_then(|o| app.data.offer_mimes.get(&o.id()).cloned())
                    .unwrap_or_default();
                let source_actions =
                    id.as_ref().and_then(|o| app.data.offer_source_actions.get(&o.id()).copied());
                app.data.incoming =
                    Incoming { mimes, source_actions, entered: true, surface_local: (x, y), ..Default::default() };
                app.paint.dnd_hover = true;

                if let Some(offer) = &id {
                    // Accepting a type is what makes the source's cursor say
                    // "yes". Declining every type is how a target says no, and a
                    // target that accepts everything is why drops land in the
                    // wrong place.
                    let pick = app
                        .data
                        .accept_order
                        .iter()
                        .find(|m| app.data.incoming.mimes.iter().any(|o| o == *m))
                        .cloned();
                    offer.accept(serial, pick);
                    offer.set_actions(offered_actions(), DndAction::Copy);
                }
                app.data.dnd_offer = id;
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                app.data.incoming.surface_local = (x, y);
            }
            wl_data_device::Event::Leave => {
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    eprintln!("    target: leave");
                }
                app.paint.dnd_hover = false;
                app.data.incoming.entered = false;
                // The offer has to survive this. Hyprland sends a `leave` around
                // the `drop` — before it as well as after it — and a target that
                // drops the offer on `leave` has nothing left to call `receive` on
                // when the drop arrives. This cost an afternoon: the drop looked
                // like it was being refused by the compositor, and the compositor
                // was doing exactly what wl_data_device says.
                if !app.data.incoming.dropped {
                    app.data.dnd_offer = None;
                }
            }
            wl_data_device::Event::Drop => {
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    eprintln!("    target: drop");
                }
                app.data.incoming.dropped = true;
                app.paint.dnd_hover = false;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for App {
    fn event(
        app: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                app.data.offer_mimes.entry(offer.id()).or_default().push(mime_type.clone());
                if app.data.incoming.entered {
                    app.data.incoming.mimes.push(mime_type);
                }
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                if let WEnum::Value(a) = source_actions {
                    app.data.offer_source_actions.insert(offer.id(), a);
                    app.data.incoming.source_actions = Some(a);
                }
            }
            wl_data_offer::Event::Action { dnd_action } => {
                if let WEnum::Value(a) = dnd_action {
                    app.data.incoming.action = Some(a);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if std::env::var_os("SPIKE_D_TRACE").is_some() {
                    eprintln!("    source: asked for {mime_type}");
                }
                app.data.outgoing.requested.push(mime_type.clone());
                write_payload(&app.data.payload, &mime_type, fd, &mut app.data.outgoing.short_writes);
            }
            wl_data_source::Event::Target { mime_type } => {
                app.data.outgoing.target_accepted = mime_type;
            }
            wl_data_source::Event::Action { dnd_action } => {
                if let WEnum::Value(a) = dnd_action {
                    app.data.outgoing.action = Some(a);
                }
            }
            wl_data_source::Event::DndDropPerformed => app.data.outgoing.dropped = true,
            wl_data_source::Event::DndFinished => app.data.outgoing.finished = true,
            wl_data_source::Event::Cancelled => {
                app.data.outgoing.cancelled = true;
                app.data.source_cancelled += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                app.data.primary_offer_mimes.insert(offer.id(), Vec::new());
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => {
                app.data.primary_events += 1;
                app.data.primary_mimes = id
                    .as_ref()
                    .and_then(|o| app.data.primary_offer_mimes.get(&o.id()).cloned())
                    .unwrap_or_default();
                app.data.primary_selection = id;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE
            => (zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1, ()> for App {
    fn event(
        app: &mut Self,
        offer: &zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
        event: zwp_primary_selection_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_offer_v1::Event::Offer { mime_type } = event {
            app.data.primary_offer_mimes.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_source_v1::Event::Send { mime_type, fd } = event {
            app.data.outgoing.requested.push(mime_type.clone());
            write_payload(&app.data.payload, &mime_type, fd, &mut app.data.outgoing.short_writes);
        }
    }
}

/// One non-blocking write. A short write is *recorded* rather than retried,
/// because a spike that silently loops here would hide the fact that a real
/// implementation needs a write queue on the event loop — which is a finding, not
/// an implementation detail.
fn write_payload(
    payload: &HashMap<String, Vec<u8>>,
    mime: &str,
    fd: OwnedFd,
    short_writes: &mut u32,
) {
    let Some(bytes) = payload.get(mime) else { return };
    rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).ok();
    match rustix::io::write(&fd, bytes) {
        Ok(n) if n < bytes.len() => *short_writes += 1,
        _ => {}
    }
    let _ = fd.as_raw_fd();
}

/// Held write ends of pipes a peer is waiting on. This is the hung peer that rule
/// 1's clipboard case needs: the fds stay open, so the reader sees neither data
/// nor EOF.
#[derive(Default)]
pub struct HungFds(pub Vec<OwnedFd>);

/// A drag icon surface has to be attached to the source's surface list before the
/// drag starts, so it is created by the caller and passed in.
pub fn start_drag(
    app: &App,
    source: &wl_data_source::WlDataSource,
    icon: Option<&wl_surface::WlSurface>,
    serial: u32,
) {
    app.data
        .device
        .as_ref()
        .expect("data device")
        .start_drag(Some(source), &app.surface, icon, serial);
}
