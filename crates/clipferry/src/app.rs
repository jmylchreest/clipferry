//! Glue between the sans-IO broker and the two backends: wayland events come
//! in via the Dispatch impls (wayland.rs), X11 events via `drain_x11`, and
//! broker commands fan back out to the backends.

use anyhow::anyhow;
use calloop::LoopSignal;
use log::{debug, error, info};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, AtomEnum, SelectionRequestEvent};

use crate::broker::{Broker, Command};
use crate::transfer::{self, PasteReply};
use crate::wayland::Offer;
use crate::x11::X11;
use crate::{broker, mime};

pub struct App {
    pub broker: Broker,
    pub x11: X11,
    pub wl_conn: wayland_client::Connection,
    pub current_offer: Option<Offer>,
    pub exit: Option<anyhow::Error>,
    pub loop_signal: Option<LoopSignal>,
}

impl App {
    pub const fn new(x11: X11, wl_conn: wayland_client::Connection) -> Self {
        Self {
            broker: Broker::new(),
            x11,
            wl_conn,
            current_offer: None,
            exit: None,
            loop_signal: None,
        }
    }

    // --- Wayland side --------------------------------------------------

    pub fn on_wayland_selection(&mut self, offer: Option<Offer>) {
        if let Some(old) = self.current_offer.take() {
            old.destroy();
        }
        if let Some(offer) = offer {
            let mime_types = offer.mime_types();
            debug!(
                "wayland selection changed: {} MIME types offered",
                mime_types.len()
            );
            self.current_offer = Some(offer);
            let event = if mime::pick_text(&mime_types).is_some() {
                broker::Event::WaylandSelection { mime_types }
            } else {
                // M1 bridges text only; a non-text owner means we have
                // nothing to proxy, which is the same as a clear.
                debug!("no text type in offer (all-MIME bridging is M3); standing down");
                broker::Event::WaylandCleared
            };
            self.dispatch_broker(event);
        } else {
            debug!("wayland selection cleared");
            self.dispatch_broker(broker::Event::WaylandCleared);
        }
    }

    pub fn on_wayland_finished(&mut self) {
        // Per §5: protocol says this device is done; exit nonzero, systemd
        // restarts us with a fresh connection.
        self.fatal(anyhow!("compositor finished our data-control device"));
    }

    // --- X11 side --------------------------------------------------------

    pub fn drain_x11(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.x11.conn.poll_for_event()? {
            self.handle_x11_event(&event);
        }
        Ok(())
    }

    pub fn handle_x11_event(&mut self, event: &Event) {
        match event {
            Event::SelectionClear(e) if e.selection == self.x11.atoms.CLIPBOARD => {
                info!("lost X11 CLIPBOARD to a real X11 client");
                self.x11.owned_since = None;
                self.dispatch_broker(broker::Event::X11Lost);
            }
            Event::SelectionRequest(req) => self.on_selection_request(*req),
            _ => {}
        }
    }

    fn on_selection_request(&mut self, req: SelectionRequestEvent) {
        let atoms = self.x11.atoms;
        // Obsolete-client convention (ICCCM): property None means "use the
        // target atom as the property".
        let property = if req.property == x11rb::NONE {
            req.target
        } else {
            req.property
        };

        let Some(owned_since) = self.x11.owned_since else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        if req.selection != atoms.CLIPBOARD {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        }

        let string_atom = Atom::from(AtomEnum::STRING);
        if req.target == atoms.TARGETS {
            let targets = [
                atoms.TARGETS,
                atoms.TIMESTAMP,
                atoms.UTF8_STRING,
                atoms.TEXT,
                string_atom,
            ];
            self.reply_atoms(&req, property, AtomEnum::ATOM, &targets);
        } else if req.target == atoms.TIMESTAMP {
            self.reply_atoms(&req, property, AtomEnum::INTEGER, &[owned_since]);
        } else if req.target == atoms.MULTIPLE {
            // §6: refuse, but log at INFO so we find out if a real app needs it.
            info!(
                "MULTIPLE requested by 0x{:x} — unsupported for now",
                req.requestor
            );
            transfer::notify(&self.x11.conn, &req, None);
        } else if req.target == atoms.UTF8_STRING
            || req.target == atoms.TEXT
            || req.target == string_atom
        {
            self.start_text_paste(req, property);
        } else {
            debug!(
                "refusing target atom {} (all-MIME passthrough is M3)",
                req.target
            );
            transfer::notify(&self.x11.conn, &req, None);
        }
        if let Err(e) = self.x11.conn.flush() {
            self.fatal(anyhow!(e).context("flush X11 after selection request"));
        }
    }

    fn reply_atoms(
        &self,
        req: &SelectionRequestEvent,
        property: Atom,
        kind: AtomEnum,
        values: &[u32],
    ) {
        use x11rb::wrapper::ConnectionExt as _;
        let ok = self
            .x11
            .conn
            .change_property32(
                x11rb::protocol::xproto::PropMode::REPLACE,
                req.requestor,
                property,
                kind,
                values,
            )
            .is_ok();
        transfer::notify(&self.x11.conn, req, ok.then_some(property));
    }

    fn start_text_paste(&mut self, req: SelectionRequestEvent, property: Atom) {
        let Some(offer) = self.current_offer.clone() else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        let Some(mime) = mime::pick_text(&offer.mime_types()) else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        let (reader, writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(e) => {
                error!("pipe() for paste transfer failed: {e}");
                transfer::notify(&self.x11.conn, &req, None);
                return;
            }
        };
        // Lazy proxying (§4.2): this is the moment payload bytes start
        // moving — an actual paste, never before.
        offer.receive(mime, std::os::fd::AsFd::as_fd(&writer));
        drop(writer);
        if let Err(e) = self.wl_conn.flush() {
            self.fatal(anyhow!(e).context("flush Wayland after receive"));
            return;
        }
        let to_latin1 = req.target == Atom::from(AtomEnum::STRING);
        let reply_type = if to_latin1 {
            Atom::from(AtomEnum::STRING)
        } else {
            self.x11.atoms.UTF8_STRING
        };
        transfer::spawn_text_paste(
            PasteReply {
                conn: self.x11.conn.clone(),
                req,
                property,
                reply_type,
                to_latin1,
                max_payload: self.x11.max_payload,
            },
            reader,
        );
    }

    // --- Broker plumbing -------------------------------------------------

    fn dispatch_broker(&mut self, event: broker::Event) {
        for command in self.broker.handle(event) {
            self.run_command(&command);
        }
    }

    fn run_command(&mut self, command: &Command) {
        match command {
            Command::ClaimX11 { .. } => match self.x11.claim() {
                Ok(pending) => {
                    for event in pending {
                        self.handle_x11_event(&event);
                    }
                }
                Err(e) => self.fatal(e.context("claim X11 selection")),
            },
            Command::ReleaseX11 => {
                if let Err(e) = self.x11.release() {
                    self.fatal(e.context("release X11 selection"));
                }
            }
        }
    }

    pub fn fatal(&mut self, error: anyhow::Error) {
        error!("fatal: {error:#}");
        if self.exit.is_none() {
            self.exit = Some(error);
        }
        if let Some(signal) = &self.loop_signal {
            signal.stop();
        }
    }
}
