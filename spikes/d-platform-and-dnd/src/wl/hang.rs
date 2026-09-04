// SPDX-License-Identifier: MIT OR Apache-2.0
//! A peer that offers data and never delivers it, so that rule 1's clipboard case
//! can be measured rather than argued.
//!
//! `zwlr_data_control_manager_v1` is what makes this possible without a focus
//! dance: a data-control client can own the selection without a surface and
//! without taking keyboard focus, so the process under test keeps focus and
//! therefore keeps receiving selection events. It is an instrument — `sk-plat`
//! will never bind a data-control manager, because owning the clipboard without
//! focus is precisely the thing the protocol is trying to prevent.

use std::os::fd::OwnedFd;

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use super::App;

#[derive(Default)]
pub struct HangState {
    pub source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
    pub device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    /// Every fd a reader is blocked on. Kept, never written, never closed.
    pub held: Vec<OwnedFd>,
    pub requests: u32,
}

pub fn own_selection(app: &mut App, qh: &QueueHandle<App>, mime: &str) -> bool {
    let Some(mgr) = app.globals.dcm.clone() else { return false };
    let device = mgr.get_data_device(&app.globals.seat, qh, ());
    let source = mgr.create_data_source(qh, ());
    source.offer(mime.into());
    device.set_selection(Some(&source));
    app.hang.device = Some(device);
    app.hang.source = Some(source);
    true
}

impl Dispatch<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        _: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(App, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE
            => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        _: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_source_v1::Event::Send { mime_type, fd } = event {
            let _ = mime_type;
            app.hang.requests += 1;
            // The whole point: the reader is now waiting on an fd that will never
            // be written to and will never be closed.
            app.hang.held.push(fd);
        }
    }
}
