//! Glue between the sans-IO broker and the two backends: wayland events come
//! in via the Dispatch impls (wayland.rs), X11 events via `drain_x11`, and
//! broker commands fan back out to the backends.

use anyhow::anyhow;
use calloop::LoopSignal;
use log::{debug, error, info};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, AtomEnum, SelectionRequestEvent};

use std::collections::HashMap;
use std::time::Duration;

use wayland_client::QueueHandle;

use crate::broker::{Broker, Command};
use crate::transfer::{self, Conv, PasteReply};
use crate::wayland::{Device, Manager, Offer, Source};
use crate::x11::X11;
use crate::{broker, mime};

pub struct App {
    pub broker: Broker,
    pub x11: X11,
    pub wl_conn: wayland_client::Connection,
    pub manager: Manager,
    pub device: Device,
    pub qh: QueueHandle<Self>,
    pub current_offer: Option<Offer>,
    /// Our own data source while we proxy X→W. Identity anchor for the §4.3
    /// Wayland-side loop rule: while this is alive, any selection event is
    /// our own claim echoing back (a real takeover cancels the source first).
    pub our_source: Option<Source>,
    /// Epoch of the in-flight TARGETS fetch, if any (§4.3 staleness guard).
    pending_targets_epoch: Option<u64>,
    /// Atom → MIME map for our current W→X proxy claim: the arbitrary
    /// targets we advertise on X11 beyond the text aliases (§7 pass-through).
    proxy_targets: HashMap<Atom, String>,
    /// §4.2 idle timeout for payload transfers; `None` = infinite (default).
    pub transfer_timeout: Option<Duration>,
    pub exit: Option<anyhow::Error>,
    pub loop_signal: Option<LoopSignal>,
}

impl App {
    pub fn new(
        x11: X11,
        wl_conn: wayland_client::Connection,
        manager: Manager,
        device: Device,
        qh: QueueHandle<Self>,
        transfer_timeout: Option<Duration>,
    ) -> Self {
        Self {
            broker: Broker::new(),
            x11,
            wl_conn,
            manager,
            device,
            qh,
            current_offer: None,
            our_source: None,
            pending_targets_epoch: None,
            proxy_targets: HashMap::new(),
            transfer_timeout,
            exit: None,
            loop_signal: None,
        }
    }

    // --- Wayland side --------------------------------------------------

    pub fn on_wayland_selection(&mut self, offer: Option<Offer>) {
        // Loop prevention by identity (§4.3): while our own source is alive,
        // this event is our claim echoing back — a real takeover would have
        // cancelled the source first (same-connection event ordering).
        if self.our_source.is_some() {
            if let Some(o) = offer {
                o.destroy();
            }
            return;
        }
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
            // §7: bridge everything except X11 protocol machinery names.
            let bridgeable: Vec<String> = mime_types
                .into_iter()
                .filter(|m| !mime::PROTOCOL_TARGETS.contains(&m.as_str()))
                .collect();
            let event = if bridgeable.is_empty() {
                debug!("offer has no bridgeable types; standing down");
                broker::Event::WaylandCleared
            } else {
                broker::Event::WaylandSelection {
                    mime_types: bridgeable,
                }
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

    /// A Wayland client pastes from our proxy source: stream from the X11
    /// owner (lazy, §4.2).
    pub fn on_source_send(&mut self, source: &Source, mime: &str, fd: std::os::fd::OwnedFd) {
        let is_current = self
            .our_source
            .as_ref()
            .is_some_and(|s| s.id() == source.id());
        if !is_current {
            debug!("send request on a superseded source; dropping (empty paste)");
            return; // dropping fd closes the pipe → empty paste
        }
        transfer::spawn_x11_read(mime.to_owned(), fd, self.transfer_timeout);
    }

    /// The compositor cancelled a source of ours (someone else claimed, or
    /// we replaced our own claim).
    pub fn on_source_cancelled(&mut self, source: &Source) {
        source.destroy();
        if self
            .our_source
            .as_ref()
            .is_some_and(|s| s.id() == source.id())
        {
            self.our_source = None;
        }
    }

    /// Startup rule (§4.1), X11 half: if nothing came from the initial
    /// Wayland roundtrip but a real X11 app owns CLIPBOARD, fill the
    /// missing Wayland side.
    pub fn probe_x11_startup(&mut self) {
        if !matches!(self.broker.state(), broker::State::Idle) {
            return;
        }
        match self.x11.selection_owner() {
            Ok(owner) if owner != x11rb::NONE && owner != self.x11.win => {
                info!("startup: X11 CLIPBOARD has an owner; filling the Wayland side");
                self.dispatch_broker(broker::Event::X11Selection);
            }
            Ok(_) => {}
            Err(e) => self.fatal(e.context("startup X11 owner probe")),
        }
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
            Event::XfixesSelectionNotify(e) if e.selection == self.x11.atoms.CLIPBOARD => {
                // Loop prevention by identity (§4.3), X11 side: our own
                // claims echo back with owner == our window — skip them.
                if e.owner == self.x11.win {
                    return;
                }
                if e.owner == x11rb::NONE {
                    self.dispatch_broker(broker::Event::X11Cleared);
                } else {
                    debug!("X11 CLIPBOARD claimed by 0x{:x}", e.owner);
                    self.dispatch_broker(broker::Event::X11Selection);
                }
            }
            // TARGETS reply for a FetchX11Targets command.
            Event::SelectionNotify(e)
                if e.requestor == self.x11.win && e.target == self.x11.atoms.TARGETS =>
            {
                let Some(epoch) = self.pending_targets_epoch.take() else {
                    return;
                };
                let mime_types = if e.property == x11rb::NONE {
                    Vec::new() // owner refused TARGETS — nothing to bridge
                } else {
                    match self.x11.read_targets_property() {
                        Ok(targets) => self.x11.targets_to_mimes(&targets),
                        Err(err) => {
                            debug!("reading TARGETS reply failed: {err:#}");
                            Vec::new()
                        }
                    }
                };
                self.dispatch_broker(broker::Event::X11Targets { epoch, mime_types });
            }
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
        let has_text = match self.broker.state() {
            broker::State::WaylandApp { mime_types } => mime::pick_text(mime_types).is_some(),
            _ => false,
        };
        if req.target == atoms.TARGETS {
            let mut targets = vec![atoms.TARGETS, atoms.TIMESTAMP];
            if has_text {
                targets.extend([atoms.UTF8_STRING, atoms.TEXT, string_atom]);
            }
            targets.extend(self.proxy_targets.keys().copied());
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
        } else if (req.target == atoms.UTF8_STRING
            || req.target == atoms.TEXT
            || req.target == string_atom)
            && has_text
        {
            self.start_text_paste(req, property);
        } else if let Some(mime) = self.proxy_targets.get(&req.target).cloned() {
            // §7 pass-through: serve the offered MIME verbatim under its
            // own atom.
            self.start_paste(req, property, &mime, req.target, Conv::None);
        } else {
            debug!("refusing unadvertised target atom {}", req.target);
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
        let Some(mime) = self
            .current_offer
            .as_ref()
            .and_then(|o| mime::pick_text(&o.mime_types()))
        else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        let to_latin1 = req.target == Atom::from(AtomEnum::STRING);
        let (reply_type, conversion) = if to_latin1 {
            (Atom::from(AtomEnum::STRING), Conv::Utf8ToLatin1)
        } else {
            (self.x11.atoms.UTF8_STRING, Conv::None)
        };
        self.start_paste(req, property, mime, reply_type, conversion);
    }

    fn start_paste(
        &mut self,
        req: SelectionRequestEvent,
        property: Atom,
        mime: &str,
        reply_type: Atom,
        conversion: Conv,
    ) {
        let Some(offer) = self.current_offer.clone() else {
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
        transfer::spawn_wayland_read(
            PasteReply {
                req,
                property,
                reply_type,
                conversion,
                timeout: self.transfer_timeout,
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
            Command::ClaimX11 { .. } => {
                self.proxy_targets = match self.broker.state() {
                    broker::State::WaylandApp { mime_types } => {
                        match self.x11.intern_mimes(mime_types) {
                            Ok(map) => map,
                            Err(e) => {
                                self.fatal(e.context("intern offer MIME atoms"));
                                return;
                            }
                        }
                    }
                    _ => HashMap::new(),
                };
                match self.x11.claim() {
                    Ok(pending) => {
                        for event in pending {
                            self.handle_x11_event(&event);
                        }
                    }
                    Err(e) => self.fatal(e.context("claim X11 selection")),
                }
            }
            Command::ReleaseX11 => {
                self.proxy_targets.clear();
                if let Err(e) = self.x11.release() {
                    self.fatal(e.context("release X11 selection"));
                }
            }
            Command::FetchX11Targets { epoch } => {
                self.pending_targets_epoch = Some(*epoch);
                if let Err(e) = self.x11.fetch_targets() {
                    self.fatal(e.context("fetch X11 TARGETS"));
                }
            }
            Command::ClaimWayland { mime_types, .. } => {
                // Replacing our own claim: drop the old source now; its
                // Cancelled event dies with the destroyed proxy.
                if let Some(old) = self.our_source.take() {
                    old.destroy();
                }
                let source = self.manager.create_source(mime_types, &self.qh);
                self.device.set_selection(Some(&source));
                self.our_source = Some(source);
                if let Err(e) = self.wl_conn.flush() {
                    self.fatal(anyhow!(e).context("flush Wayland after claim"));
                } else {
                    info!("claimed Wayland selection (proxying X11 owner)");
                }
            }
            Command::ReleaseWayland => {
                if let Some(source) = self.our_source.take() {
                    self.device.set_selection(None);
                    source.destroy();
                    if let Err(e) = self.wl_conn.flush() {
                        self.fatal(anyhow!(e).context("flush Wayland after release"));
                    } else {
                        info!("released Wayland selection");
                    }
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
