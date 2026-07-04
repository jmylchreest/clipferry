//! Glue between the sans-IO brokers (one per selection) and the two
//! backends: Wayland events come in via the Dispatch impls (wayland.rs),
//! X11 events via `drain_x11`, and broker commands fan back out.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use calloop::{LoopHandle, LoopSignal};
use log::{debug, error, info};
use wayland_client::QueueHandle;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{Atom, AtomEnum, SelectionRequestEvent};

use crate::broker::{Broker, Command};
use crate::cli::SyncMode;
use crate::mime::Transform;
use crate::payload::{PayloadRope, ReadOutcome, Snapshot};
use crate::transfer::{self, Conv, PasteReply, X2wRequest};
use crate::wayland::{Device, Manager, Offer, Source};
use crate::x11::X11;
use crate::{SelKind, broker, mime};

/// How long PRIMARY owner changes are debounced (§14): highlight-drag
/// generates high-frequency changes; only the settled owner matters.
const PRIMARY_DEBOUNCE: Duration = Duration::from_millis(50);

/// Per-selection state: broker plus everything a proxy claim carries.
#[derive(Default)]
struct SelCtx {
    broker: Broker,
    current_offer: Option<Offer>,
    /// Our own data source while proxying X→W. Identity anchor for the §4.3
    /// Wayland-side loop rule: while this is alive, any selection event is
    /// our own claim echoing back (a real takeover cancels the source first).
    our_source: Option<Source>,
    /// W→X: atom → (source MIME to read, transform) for advertised targets.
    proxy_targets: HashMap<Atom, (String, Transform)>,
    /// X→W: advertised MIME → (x11 target, transform) read-plan overrides.
    x2w_plans: HashMap<String, (String, Transform)>,
    pending_targets_epoch: Option<u64>,
    /// §8: the current offer carries the password-manager hint.
    sensitive: bool,
    /// §4.2.1 eager snapshot for the current claim.
    snapshot: Option<Arc<Snapshot>>,
}

/// Pending debounced PRIMARY change (either side; the latest wins).
enum PendingPrimary {
    Wayland(Option<Offer>),
    X11 { has_owner: bool },
}

/// Result of an eager snapshot fetch, delivered via the calloop channel.
pub struct SnapshotMsg {
    pub kind: SelKind,
    pub snapshot: Snapshot,
}

pub struct App {
    pub x11: X11,
    pub wl_conn: wayland_client::Connection,
    pub manager: Manager,
    pub device: Device,
    pub qh: QueueHandle<Self>,
    ctx: [SelCtx; 2],
    pub primary: bool,
    pub skip_sensitive: bool,
    pub sync_mode: SyncMode,
    pub eager_max: Option<usize>,
    pub transfer_timeout: Option<Duration>,
    pub snapshot_tx: Option<calloop::channel::Sender<SnapshotMsg>>,
    pub loop_handle: Option<LoopHandle<'static, Self>>,
    pending_primary: Option<PendingPrimary>,
    primary_gen: u64,
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
        options: &crate::cli::Options,
    ) -> Self {
        Self {
            x11,
            wl_conn,
            manager,
            device,
            qh,
            ctx: [SelCtx::default(), SelCtx::default()],
            primary: options.primary,
            skip_sensitive: options.skip_sensitive,
            sync_mode: options.sync_mode,
            eager_max: options.eager_max_size,
            transfer_timeout: (options.transfer_timeout > 0)
                .then(|| Duration::from_secs(options.transfer_timeout)),
            snapshot_tx: None,
            loop_handle: None,
            pending_primary: None,
            primary_gen: 0,
            exit: None,
            loop_signal: None,
        }
    }

    // --- Wayland side -----------------------------------------------------

    pub fn on_wayland_selection(&mut self, kind: SelKind, offer: Option<Offer>) {
        // Loop prevention by identity (§4.3): while our own source is alive,
        // this event is our claim echoing back — a real takeover would have
        // cancelled the source first (same-connection event ordering).
        if self.ctx[kind.idx()].our_source.is_some() {
            if let Some(o) = offer {
                o.destroy();
            }
            return;
        }
        if kind == SelKind::Primary {
            self.debounce_primary(PendingPrimary::Wayland(offer));
            return;
        }
        self.process_wayland_selection(kind, offer);
    }

    pub fn on_wayland_primary(&mut self, offer: Option<Offer>) {
        if self.primary {
            self.on_wayland_selection(SelKind::Primary, offer);
        } else if let Some(o) = offer {
            o.destroy();
        }
    }

    fn process_wayland_selection(&mut self, kind: SelKind, offer: Option<Offer>) {
        if let Some(old) = self.ctx[kind.idx()].current_offer.take() {
            old.destroy();
        }
        if let Some(offer) = offer {
            let mime_types = offer.mime_types();
            let sensitive = mime::is_sensitive(&mime_types);
            self.ctx[kind.idx()].sensitive = sensitive;
            if sensitive {
                if self.skip_sensitive {
                    info!(
                        "event=skip side=wayland sel={} reason=sensitive",
                        kind.key()
                    );
                    offer.destroy();
                    self.dispatch_broker(kind, broker::Event::WaylandCleared);
                    return;
                }
                info!("event=offer side=wayland sel={} sensitive=true", kind.key());
            } else {
                debug!(
                    "event=offer side=wayland sel={} mimes={}",
                    kind.key(),
                    mime_types.len()
                );
            }
            self.ctx[kind.idx()].current_offer = Some(offer);
            // §7: bridge everything except X11 protocol machinery names.
            let bridgeable: Vec<String> = mime_types
                .into_iter()
                .filter(|m| !mime::PROTOCOL_TARGETS.contains(&m.as_str()))
                .collect();
            let event = if bridgeable.is_empty() {
                debug!("event=offer side=wayland sel={} bridgeable=0", kind.key());
                broker::Event::WaylandCleared
            } else {
                broker::Event::WaylandSelection {
                    mime_types: bridgeable,
                }
            };
            self.dispatch_broker(kind, event);
        } else if self.survives_source_exit(kind, false) {
            debug!(
                "event=snapshot_serve side=wayland sel={} reason=source-exited",
                kind.key()
            );
        } else {
            debug!("event=clear side=wayland sel={}", kind.key());
            self.ctx[kind.idx()].sensitive = false;
            self.dispatch_broker(kind, broker::Event::WaylandCleared);
        }
    }

    pub fn on_wayland_finished(&mut self) {
        // Per §5: protocol says this device is done; exit nonzero, systemd
        // restarts us with a fresh connection.
        self.fatal(anyhow!("compositor finished our data-control device"));
    }

    /// A Wayland client pastes from our proxy source: stream from the X11
    /// owner (lazy, §4.2) or the eager snapshot (§4.2.1).
    pub fn on_source_send(
        &mut self,
        kind: SelKind,
        source: &Source,
        mime: &str,
        fd: std::os::fd::OwnedFd,
    ) {
        let ctx = &self.ctx[kind.idx()];
        let is_current = ctx
            .our_source
            .as_ref()
            .is_some_and(|s| s.id() == source.id());
        if !is_current {
            debug!("event=refuse dir=x2w reason=superseded-source");
            return; // dropping fd closes the pipe → empty paste
        }
        // Over-cap types miss the snapshot and degrade to lazy (§4.2.1).
        if let Some(snapshot) = &ctx.snapshot
            && snapshot.data.contains_key(mime)
        {
            transfer::spawn_rope_to_fd(snapshot.clone(), mime.to_owned(), fd);
            return;
        }
        let plan = ctx.x2w_plans.get(mime).cloned();
        transfer::spawn_x11_read(X2wRequest {
            mime: mime.to_owned(),
            plan,
            kind,
            fd,
            timeout: self.transfer_timeout,
        });
    }

    /// The compositor cancelled a source of ours (someone else claimed, or
    /// we replaced our own claim).
    pub fn on_source_cancelled(&mut self, kind: SelKind, source: &Source) {
        source.destroy();
        let ctx = &mut self.ctx[kind.idx()];
        if ctx
            .our_source
            .as_ref()
            .is_some_and(|s| s.id() == source.id())
        {
            ctx.our_source = None;
        }
    }

    /// Startup rule (§4.1), X11 half: fill only the missing side.
    pub fn probe_x11_startup(&mut self) {
        for kind in SelKind::ALL {
            if kind == SelKind::Primary && !self.primary {
                continue;
            }
            if !matches!(self.ctx[kind.idx()].broker.state(), broker::State::Idle) {
                continue;
            }
            match self.x11.selection_owner(kind) {
                Ok(owner) if owner != x11rb::NONE && owner != self.x11.win => {
                    info!("event=startup_fill side=x11 sel={}", kind.key());
                    self.dispatch_broker(kind, broker::Event::X11Selection);
                }
                Ok(_) => {}
                Err(e) => self.fatal(e.context("startup X11 owner probe")),
            }
        }
    }

    // --- PRIMARY debounce (§14) --------------------------------------------

    fn debounce_primary(&mut self, pending: PendingPrimary) {
        self.pending_primary = Some(pending);
        self.primary_gen += 1;
        let generation = self.primary_gen;
        if let Some(handle) = &self.loop_handle {
            let timer = calloop::timer::Timer::from_duration(PRIMARY_DEBOUNCE);
            let result = handle.insert_source(timer, move |_, (), app: &mut Self| {
                app.fire_primary_debounce(generation);
                calloop::timer::TimeoutAction::Drop
            });
            if result.is_err() {
                // No timer — degrade to immediate processing.
                self.fire_primary_debounce(generation);
            }
        } else {
            self.fire_primary_debounce(generation);
        }
    }

    fn fire_primary_debounce(&mut self, generation: u64) {
        if generation != self.primary_gen {
            return; // superseded by a newer change
        }
        match self.pending_primary.take() {
            Some(PendingPrimary::Wayland(offer)) => {
                self.process_wayland_selection(SelKind::Primary, offer);
            }
            Some(PendingPrimary::X11 { has_owner }) => {
                if has_owner {
                    self.dispatch_broker(SelKind::Primary, broker::Event::X11Selection);
                } else if self.survives_source_exit(SelKind::Primary, true) {
                    debug!("event=snapshot_serve side=x11 sel=primary reason=source-exited");
                } else {
                    self.dispatch_broker(SelKind::Primary, broker::Event::X11Cleared);
                }
            }
            None => {}
        }
    }

    // --- X11 side -----------------------------------------------------------

    pub fn drain_x11(&mut self) -> anyhow::Result<()> {
        while let Some(event) = self.x11.conn.poll_for_event()? {
            self.handle_x11_event(&event);
        }
        Ok(())
    }

    /// Eager survival (§4.2.1a): when the source app exits while we hold a
    /// snapshot for this claim, keep the opposite-side proxy claim alive and
    /// serve from memory instead of tearing everything down.
    fn survives_source_exit(&self, kind: SelKind, source_state_is_x11: bool) -> bool {
        if self.sync_mode != SyncMode::Eager {
            return false;
        }
        let ctx = &self.ctx[kind.idx()];
        if ctx.snapshot.is_none() {
            return false;
        }
        match ctx.broker.state() {
            broker::State::X11App { .. } => source_state_is_x11,
            broker::State::WaylandApp { .. } => !source_state_is_x11,
            _ => false,
        }
    }

    fn kind_for_selection(&self, selection: Atom) -> Option<SelKind> {
        if selection == self.x11.atoms.CLIPBOARD {
            Some(SelKind::Clipboard)
        } else if selection == Atom::from(AtomEnum::PRIMARY) && self.primary {
            Some(SelKind::Primary)
        } else {
            None
        }
    }

    pub fn handle_x11_event(&mut self, event: &Event) {
        match event {
            Event::SelectionClear(e) => {
                if let Some(kind) = self.kind_for_selection(e.selection) {
                    info!("event=lost side=x11 sel={}", kind.key());
                    self.x11.owned_since[kind.idx()] = None;
                    self.dispatch_broker(kind, broker::Event::X11Lost);
                }
            }
            Event::SelectionRequest(req) => self.on_selection_request(*req),
            Event::XfixesSelectionNotify(e) => {
                let Some(kind) = self.kind_for_selection(e.selection) else {
                    return;
                };
                // Loop prevention by identity (§4.3), X11 side.
                if e.owner == self.x11.win {
                    return;
                }
                let has_owner = e.owner != x11rb::NONE
                    && e.subtype == x11rb::protocol::xfixes::SelectionEvent::SET_SELECTION_OWNER;
                if kind == SelKind::Primary {
                    self.debounce_primary(PendingPrimary::X11 { has_owner });
                    return;
                }
                if has_owner {
                    let class = self.x11.owner_class(e.owner);
                    info!(
                        "event=owner side=x11 sel={} owner=0x{:x} class={:?}",
                        kind.key(),
                        e.owner,
                        class.as_deref().unwrap_or("unknown")
                    );
                    self.dispatch_broker(kind, broker::Event::X11Selection);
                } else if self.survives_source_exit(kind, true) {
                    debug!(
                        "event=snapshot_serve side=x11 sel={} reason=source-exited",
                        kind.key()
                    );
                } else {
                    self.dispatch_broker(kind, broker::Event::X11Cleared);
                }
            }
            // TARGETS reply for a FetchX11Targets command.
            Event::SelectionNotify(e)
                if e.requestor == self.x11.win && e.target == self.x11.atoms.TARGETS =>
            {
                let Some(kind) = self.kind_for_selection(e.selection) else {
                    return;
                };
                self.on_targets_reply(kind, e.property);
            }
            _ => {}
        }
    }

    fn on_targets_reply(&mut self, kind: SelKind, property: Atom) {
        let Some(epoch) = self.ctx[kind.idx()].pending_targets_epoch.take() else {
            return;
        };
        let mut mime_types = Vec::new();
        if property != x11rb::NONE {
            match self.x11.read_targets_property(kind) {
                Ok(targets) => {
                    let names = self.x11.target_names(&targets);
                    let sensitive = mime::is_sensitive(&names);
                    self.ctx[kind.idx()].sensitive = sensitive;
                    if sensitive {
                        if self.skip_sensitive {
                            info!("event=skip side=x11 sel={} reason=sensitive", kind.key());
                            self.dispatch_broker(
                                kind,
                                broker::Event::X11Targets {
                                    epoch,
                                    mime_types: Vec::new(),
                                },
                            );
                            return;
                        }
                        info!("event=offer side=x11 sel={} sensitive=true", kind.key());
                    }
                    let (advertised, plans) = mime::x2w_translate(&names);
                    self.ctx[kind.idx()].x2w_plans = plans
                        .into_iter()
                        .map(|(mime, target, transform)| (mime, (target, transform)))
                        .collect();
                    mime_types = advertised;
                }
                Err(err) => debug!(
                    "event=targets sel={} error={:?}",
                    kind.key(),
                    format!("{err:#}")
                ),
            }
        }
        self.dispatch_broker(kind, broker::Event::X11Targets { epoch, mime_types });
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

        let Some(kind) = self.kind_for_selection(req.selection) else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        let Some(owned_since) = self.x11.owned_since[kind.idx()] else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };

        let string_atom = Atom::from(AtomEnum::STRING);
        let has_text = match self.ctx[kind.idx()].broker.state() {
            broker::State::WaylandApp { mime_types } => mime::pick_text(mime_types).is_some(),
            _ => false,
        };
        if req.target == atoms.TARGETS {
            let mut targets = vec![atoms.TARGETS, atoms.TIMESTAMP];
            if has_text {
                targets.extend([atoms.UTF8_STRING, atoms.TEXT, string_atom]);
            }
            targets.extend(self.ctx[kind.idx()].proxy_targets.keys().copied());
            self.reply_atoms(&req, property, AtomEnum::ATOM, &targets);
        } else if req.target == atoms.TIMESTAMP {
            self.reply_atoms(&req, property, AtomEnum::INTEGER, &[owned_since]);
        } else if req.target == atoms.MULTIPLE {
            // §6: refuse, but log at INFO so we find out if a real app needs it.
            info!(
                "event=refuse target=MULTIPLE requestor=0x{:x}",
                req.requestor
            );
            transfer::notify(&self.x11.conn, &req, None);
        } else if (req.target == atoms.UTF8_STRING
            || req.target == atoms.TEXT
            || req.target == string_atom)
            && has_text
        {
            let Some(mime) = self.ctx[kind.idx()]
                .current_offer
                .as_ref()
                .and_then(|o| mime::pick_text(&o.mime_types()))
            else {
                transfer::notify(&self.x11.conn, &req, None);
                return;
            };
            let to_latin1 = req.target == string_atom;
            let (reply_type, conversion) = if to_latin1 {
                (string_atom, Conv::Utf8ToLatin1)
            } else {
                (atoms.UTF8_STRING, Conv::None)
            };
            self.start_paste(
                kind,
                req,
                property,
                mime.to_owned(),
                reply_type,
                conversion,
                Transform::None,
            );
        } else if let Some((mime, transform)) =
            self.ctx[kind.idx()].proxy_targets.get(&req.target).cloned()
        {
            // §7 pass-through / synthesized targets.
            self.start_paste(kind, req, property, mime, req.target, Conv::None, transform);
        } else {
            debug!("event=refuse target={} reason=unadvertised", req.target);
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

    #[allow(clippy::too_many_arguments)] // paste context is genuinely this wide
    fn start_paste(
        &mut self,
        kind: SelKind,
        req: SelectionRequestEvent,
        property: Atom,
        mime: String,
        reply_type: Atom,
        conversion: Conv,
        transform: Transform,
    ) {
        let reply = PasteReply {
            kind,
            mime: mime.clone(),
            req,
            property,
            reply_type,
            conversion,
            transform,
            timeout: self.transfer_timeout,
        };
        // Eager (§4.2.1): serve from the snapshot when this type made it in.
        if let Some(snapshot) = &self.ctx[kind.idx()].snapshot
            && snapshot.data.contains_key(&mime)
        {
            transfer::spawn_snapshot_serve(reply, snapshot.clone(), mime);
            return;
        }
        let Some(offer) = self.ctx[kind.idx()].current_offer.clone() else {
            transfer::notify(&self.x11.conn, &req, None);
            return;
        };
        let (reader, writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(e) => {
                error!("event=paste error={:?}", e.to_string());
                transfer::notify(&self.x11.conn, &req, None);
                return;
            }
        };
        // Lazy proxying (§4.2): this is the moment payload bytes start
        // moving — an actual paste, never before.
        offer.receive(&mime, std::os::fd::AsFd::as_fd(&writer));
        drop(writer);
        if let Err(e) = self.wl_conn.flush() {
            self.fatal(anyhow!(e).context("flush Wayland after receive"));
            return;
        }
        transfer::spawn_wayland_read(reply, reader);
    }

    // --- Eager snapshots (§4.2.1) -------------------------------------------

    /// Kick off an eager fetch of every bridgeable type for the current
    /// claim. Data lands back on the event loop via the snapshot channel.
    fn start_eager_fetch(&mut self, kind: SelKind, mimes: &[String]) {
        if self.sync_mode != SyncMode::Eager {
            return;
        }
        let Some(tx) = self.snapshot_tx.clone() else {
            return;
        };
        let epoch = self.ctx[kind.idx()].broker.epoch();
        let cap = self.eager_max;
        let timeout = self.transfer_timeout;
        match self.ctx[kind.idx()].broker.state() {
            broker::State::WaylandApp { .. } => {
                // W-side owner: receive() every type into pipes now, collect
                // on a thread.
                let Some(offer) = self.ctx[kind.idx()].current_offer.clone() else {
                    return;
                };
                let mut pipes = Vec::new();
                for mime in mimes {
                    match std::io::pipe() {
                        Ok((reader, writer)) => {
                            offer.receive(mime, std::os::fd::AsFd::as_fd(&writer));
                            drop(writer);
                            pipes.push((mime.clone(), reader));
                        }
                        Err(e) => error!("event=snapshot error={:?}", e.to_string()),
                    }
                }
                if let Err(e) = self.wl_conn.flush() {
                    self.fatal(anyhow!(e).context("flush Wayland after eager receive"));
                    return;
                }
                std::thread::spawn(move || {
                    collect_snapshot(kind, epoch, pipes, cap, timeout, &tx);
                });
            }
            broker::State::X11App { .. } => {
                // X-side owner: run one lazy read per type into pipes, then
                // collect. spawn_x11_read serializes via the X→W gate.
                let mut pipes = Vec::new();
                for mime in mimes {
                    match std::io::pipe() {
                        Ok((reader, writer)) => {
                            let plan = self.ctx[kind.idx()].x2w_plans.get(mime).cloned();
                            transfer::spawn_x11_read(X2wRequest {
                                mime: mime.clone(),
                                plan,
                                kind,
                                fd: writer.into(),
                                timeout,
                            });
                            pipes.push((mime.clone(), reader));
                        }
                        Err(e) => error!("event=snapshot error={:?}", e.to_string()),
                    }
                }
                std::thread::spawn(move || {
                    collect_snapshot(kind, epoch, pipes, cap, timeout, &tx);
                });
            }
            _ => {}
        }
    }

    /// Snapshot fetch finished: adopt it if it still matches the epoch.
    pub fn on_snapshot(&mut self, msg: SnapshotMsg) {
        let ctx = &mut self.ctx[msg.kind.idx()];
        if msg.snapshot.epoch != ctx.broker.epoch() {
            return; // superseded claim; ropes zero on drop
        }
        if !msg.snapshot.lock_in_memory() {
            debug!("event=mlock status=partial");
        }
        let types = msg.snapshot.data.len();
        ctx.snapshot = Some(Arc::new(msg.snapshot));
        if ctx.sensitive {
            info!("event=snapshot sel={} sensitive=true", msg.kind.key());
        } else {
            debug!("event=snapshot sel={} types={types}", msg.kind.key());
        }
    }

    // --- Broker plumbing ------------------------------------------------------

    fn dispatch_broker(&mut self, kind: SelKind, event: broker::Event) {
        let before = self.ctx[kind.idx()].broker.epoch();
        let commands = self.ctx[kind.idx()].broker.handle(event);
        if self.ctx[kind.idx()].broker.epoch() != before {
            // Every legitimate ownership change invalidates the snapshot;
            // a fresh one arrives after the new claim (§4.2.1).
            self.ctx[kind.idx()].snapshot = None;
        }
        for command in commands {
            self.run_command(kind, &command);
        }
    }

    fn run_command(&mut self, kind: SelKind, command: &Command) {
        match command {
            Command::ClaimX11 { .. } => {
                let (map, mimes) = match self.ctx[kind.idx()].broker.state() {
                    broker::State::WaylandApp { mime_types } => {
                        let mut map = match self.x11.intern_mimes(mime_types) {
                            Ok(map) => map
                                .into_iter()
                                .map(|(atom, mime)| (atom, (mime, Transform::None)))
                                .collect::<HashMap<_, _>>(),
                            Err(e) => {
                                self.fatal(e.context("intern offer MIME atoms"));
                                return;
                            }
                        };
                        // §7 synthesized targets (gnome-copied-files).
                        for (target, source_mime, transform) in
                            mime::synthesized_x11_targets(mime_types)
                        {
                            match self.x11.intern_mimes(std::slice::from_ref(&target)) {
                                Ok(extra) => {
                                    for (atom, _) in extra {
                                        map.insert(atom, (source_mime.clone(), transform));
                                    }
                                }
                                Err(e) => {
                                    self.fatal(e.context("intern synthesized target"));
                                    return;
                                }
                            }
                        }
                        (map, mime_types.clone())
                    }
                    _ => (HashMap::new(), Vec::new()),
                };
                self.ctx[kind.idx()].proxy_targets = map;
                match self.x11.claim(kind) {
                    Ok(pending) => {
                        for event in pending {
                            self.handle_x11_event(&event);
                        }
                        self.start_eager_fetch(kind, &mimes);
                    }
                    Err(e) => self.fatal(e.context("claim X11 selection")),
                }
            }
            Command::ReleaseX11 => {
                self.ctx[kind.idx()].proxy_targets.clear();
                if let Err(e) = self.x11.release(kind) {
                    self.fatal(e.context("release X11 selection"));
                }
            }
            Command::FetchX11Targets { epoch } => {
                self.ctx[kind.idx()].pending_targets_epoch = Some(*epoch);
                if let Err(e) = self.x11.fetch_targets(kind) {
                    self.fatal(e.context("fetch X11 TARGETS"));
                }
            }
            Command::ClaimWayland { mime_types, .. } => {
                // Replacing our own claim: drop the old source now; its
                // Cancelled event dies with the destroyed proxy.
                if let Some(old) = self.ctx[kind.idx()].our_source.take() {
                    old.destroy();
                }
                let source = self.manager.create_source(mime_types, kind, &self.qh);
                self.device.set_selection(kind, Some(&source));
                self.ctx[kind.idx()].our_source = Some(source);
                if let Err(e) = self.wl_conn.flush() {
                    self.fatal(anyhow!(e).context("flush Wayland after claim"));
                } else {
                    if self.ctx[kind.idx()].sensitive {
                        info!(
                            "event=claim side=wayland sel={} reason=proxy-x11 sensitive=true",
                            kind.key()
                        );
                    } else {
                        info!(
                            "event=claim side=wayland sel={} reason=proxy-x11",
                            kind.key()
                        );
                    }
                    let mimes = mime_types.clone();
                    self.start_eager_fetch(kind, &mimes);
                }
            }
            Command::ReleaseWayland => {
                if let Some(source) = self.ctx[kind.idx()].our_source.take() {
                    self.device.set_selection(kind, None);
                    source.destroy();
                    if let Err(e) = self.wl_conn.flush() {
                        self.fatal(anyhow!(e).context("flush Wayland after release"));
                    } else {
                        info!("event=release side=wayland sel={}", kind.key());
                    }
                }
            }
        }
    }

    pub fn fatal(&mut self, error: anyhow::Error) {
        error!("event=fatal error={:?}", format!("{error:#}"));
        if self.exit.is_none() {
            self.exit = Some(error);
        }
        if let Some(signal) = &self.loop_signal {
            signal.stop();
        }
    }
}

/// Read each eager pipe to EOF (bounded by `cap` per type) and deliver the
/// snapshot back to the event loop. Over-cap or failed types are skipped —
/// they degrade to lazy (§4.2.1).
fn collect_snapshot(
    kind: SelKind,
    epoch: u64,
    pipes: Vec<(String, std::io::PipeReader)>,
    cap: Option<usize>,
    _timeout: Option<Duration>,
    tx: &calloop::channel::Sender<SnapshotMsg>,
) {
    let mut data = HashMap::new();
    for (mime, mut reader) in pipes {
        match PayloadRope::read_to_end(&mut reader, cap) {
            Ok(ReadOutcome::Complete(rope)) => {
                if !rope.is_empty() {
                    data.insert(mime, rope);
                }
            }
            Ok(ReadOutcome::Overflow(_)) => {
                debug!(
                    "event=snapshot_skip sel={} mime={mime:?} reason=over-cap",
                    kind.key()
                );
            }
            Err(e) => debug!(
                "event=snapshot_skip sel={} mime={mime:?} error={:?}",
                kind.key(),
                e.to_string()
            ),
        }
    }
    let _ = tx.send(SnapshotMsg {
        kind,
        snapshot: Snapshot { epoch, data },
    });
}
