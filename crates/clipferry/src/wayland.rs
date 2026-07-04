//! Wayland backend: bind `ext-data-control-v1` (preferred) or
//! `zwlr_data_control_manager_v1` (fallback, §5), track the current selection
//! offer, and expose `receive()` for lazy reads.
//!
//! The two protocols are structurally identical for our purposes; small enum
//! wrappers keep the duplication mechanical and contained to this module.

use std::os::fd::BorrowedFd;
use std::sync::Mutex;

use anyhow::{Context as _, bail};
use wayland_client::globals::{GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy as _, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use crate::app::App;

/// Per-offer user data: MIME types accumulate here between the offer's
/// introduction and the `selection` event that activates it.
#[derive(Default)]
pub struct OfferMimes(Mutex<Vec<String>>);

impl OfferMimes {
    fn push(&self, mime: String) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(mime);
    }

    fn snapshot(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

impl Manager {
    pub const fn protocol_name(&self) -> &'static str {
        match self {
            Self::Ext(_) => "ext-data-control-v1",
            Self::Wlr(_) => "zwlr-data-control-v1",
        }
    }

    pub fn get_data_device(&self, seat: &WlSeat, qh: &QueueHandle<App>) -> Device {
        match self {
            Self::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Self::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }
}

pub enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone)]
pub enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Offer {
    pub fn mime_types(&self) -> Vec<String> {
        let data = match self {
            Self::Ext(o) => o.data::<OfferMimes>(),
            Self::Wlr(o) => o.data::<OfferMimes>(),
        };
        data.map(OfferMimes::snapshot).unwrap_or_default()
    }

    /// Ask the source client to write `mime` into `fd`. The caller must flush
    /// the connection afterwards; the actual bytes are read from the pipe's
    /// other end (on a transfer thread — never on the event loop).
    pub fn receive(&self, mime: &str, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(o) => o.receive(mime.to_owned(), fd),
            Self::Wlr(o) => o.receive(mime.to_owned(), fd),
        }
    }

    pub fn destroy(&self) {
        match self {
            Self::Ext(o) => o.destroy(),
            Self::Wlr(o) => o.destroy(),
        }
    }
}

/// Bind whichever data-control global the compositor advertises; prefer ext.
pub fn bind_manager(globals: &GlobalList, qh: &QueueHandle<App>) -> anyhow::Result<Manager> {
    if let Ok(m) = globals.bind::<ExtDataControlManagerV1, App, ()>(qh, 1..=1, ()) {
        return Ok(Manager::Ext(m));
    }
    if let Ok(m) = globals.bind::<ZwlrDataControlManagerV1, App, ()>(qh, 1..=1, ()) {
        return Ok(Manager::Wlr(m));
    }
    bail!(
        "compositor advertises neither ext-data-control-v1 nor zwlr-data-control-v1; \
         clipferry cannot run here (integrated-Xwayland compositors like GNOME \
         don't need it anyway)"
    )
}

pub fn bind_seat(globals: &GlobalList, qh: &QueueHandle<App>) -> anyhow::Result<WlSeat> {
    globals
        .bind::<WlSeat, App, ()>(qh, 1..=1, ())
        .context("compositor advertises no wl_seat")
}

// --- Dispatch plumbing ------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(App: ignore WlSeat);
wayland_client::delegate_noop!(App: ExtDataControlManagerV1);
wayland_client::delegate_noop!(App: ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id } => {
                state.on_wayland_selection(id.map(Offer::Ext));
            }
            ext_data_control_device_v1::Event::Finished => state.on_wayland_finished(),
            // --primary is M4; destroy its offers so they don't leak.
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            // DataOffer: MIME types accumulate in the offer's user data
            // until a Selection event activates the offer.
            _ => {}
        }
    }

    event_created_child!(App, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.on_wayland_selection(id.map(Offer::Wlr));
            }
            zwlr_data_control_device_v1::Event::Finished => state.on_wayland_finished(),
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(o) } => o.destroy(),
            _ => {}
        }
    }

    event_created_child!(App, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, OfferMimes::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferMimes> for App {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        data: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, OfferMimes> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        data: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.push(mime_type);
        }
    }
}
