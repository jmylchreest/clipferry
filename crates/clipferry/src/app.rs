//! Glue between the sans-IO brokers (one per selection) and the two
//! backends: Wayland events come in via the Dispatch impls (wayland.rs),
//! X11 events via `drain_x11`, and broker commands fan back out.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use calloop::{LoopHandle, LoopSignal};
use log::{debug, error, info, trace, warn};
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

/// §10.1 backstop mode: after a copy on one side, how long the other side
/// gets to update by itself (another bridge, e.g. Xwayland's builtin sync)
/// before we fill the gap. Claims land only in voids — contention with
/// other bridges is impossible by construction, and requestors like Wine
/// see at most one ownership change per copy.
const GAP_WINDOW: Duration = Duration::from_millis(200);

/// §4.3 (W-side): how long after our own W→X proxy claim an incoming Wayland
/// offer may still be that claim mirrored back by the Xwayland WM, *when the
/// offer carries no X11 protocol fingerprint* to identify it outright. The
/// mirror is a direct causal consequence of the claim and lands in well under
/// a millisecond (~0.3 ms observed), so this is deliberately tight rather than
/// generous. The two failure modes are not symmetric: too wide and a genuine
/// second copy landing in `[claim, claim + window]` — i.e. `GAP_WINDOW` after
/// its predecessor — is silently swallowed, losing real content; too narrow
/// and an occasional mirror slips through to the pre-existing behaviour. Only
/// the first is a regression, so prefer the margin here (~30× observed).
const MIRROR_ECHO_WINDOW: Duration = Duration::from_millis(10);

/// Per-selection state: broker plus everything a proxy claim carries.
#[derive(Default)]
struct SelCtx {
    broker: Broker,
    current_offer: Option<Offer>,
    /// Our own data source while proxying X→W. Identity anchor for the §4.3
    /// Wayland-side loop rule: while this is alive, any selection event is
    /// our own claim echoing back (a real takeover cancels the source first).
    our_source: Option<Source>,
    /// Our own W→X proxy claim. Identity anchor for the §4.3 rule's other
    /// half: the Xwayland WM mirrors that claim straight back as a Wayland
    /// offer, which is otherwise indistinguishable from a fresh copy.
    our_x11_claim: Option<OwnX11Claim>,
    /// W→X: atom → (source MIME to read, transform) for advertised targets.
    proxy_targets: HashMap<Atom, (String, Transform)>,
    /// X→W: advertised MIME → (x11 target, transform) read-plan overrides.
    x2w_plans: HashMap<String, (String, Transform)>,
    pending_targets_epoch: Option<u64>,
    /// The real X11 owner window we are proxying (`X11App` states).
    x11_owner: Option<x11rb::protocol::xproto::Window>,
    /// §10.1: generation counters — bumped on every non-self selection
    /// change per side. A gap-fill only fires if the target side's gen is
    /// unchanged since the copy that scheduled it.
    wl_gen: u64,
    x11_gen: u64,
    /// Latest scheduled gap-fill (newest wins; stale ones no-op via gens).
    pending_fill: Option<PendingFill>,
    /// §8: the current offer carries the password-manager hint.
    sensitive: bool,
    /// §4.2.1 eager snapshot for the current claim.
    snapshot: Option<Arc<Snapshot>>,
    /// §4.2.2: broker epoch whose W→X capture is still in flight.
    capture_epoch: Option<u64>,
    /// W→X pastes held until that capture lands.
    parked: Vec<ParkedPaste>,
}

/// A W→X paste that arrived while the §4.2.2 capture for its claim was still
/// in flight. Everything `start_paste` needs, plus the epoch it belongs to —
/// a capture superseded before it lands can only answer with stale bytes.
struct ParkedPaste {
    epoch: u64,
    req: SelectionRequestEvent,
    property: Atom,
    mime: String,
    reply_type: Atom,
    conversion: Conv,
    transform: Transform,
}

/// A proxy claim we made on the X11 side, kept just long enough to recognise
/// the Xwayland WM mirroring it back onto the Wayland clipboard.
struct OwnX11Claim {
    at: Instant,
    mime_types: Vec<String>,
}

impl OwnX11Claim {
    /// True when an offer carrying `mime_types` can only be this claim coming
    /// back at us: it advertises no *content* type we did not ourselves
    /// advertise, and it is identifiable as a mirror — either by fingerprint
    /// (it re-exports X11 protocol machinery, which no Wayland application
    /// offers) or, failing that, by landing inside `MIRROR_ECHO_WINDOW`.
    ///
    /// Protocol targets are compared out rather than required to be absent:
    /// xwayland-satellite re-publishes our X11 `TARGETS` list verbatim except
    /// for `TARGETS` itself, so the mirror of an `image/png` claim comes back
    /// as `["image/png", "TIMESTAMP"]` — a *superset* of what we advertised,
    /// not the strict subset a naive reading suggests.
    fn is_echo(&self, mime_types: &[String]) -> bool {
        let content = mime::content_types(mime_types);
        !content.is_empty()
            && content
                .iter()
                .all(|m| self.mime_types.iter().any(|c| c == *m))
            && (mime::is_x11_mirror(mime_types) || self.at.elapsed() <= MIRROR_ECHO_WINDOW)
    }
}

/// §10.1 backstop: a copy happened on one side; fill the other side at
/// `GAP_WINDOW` unless it updates by itself first.
struct PendingFill {
    fill_x11: bool,
    wl_gen: u64,
    x11_gen: u64,
    /// W→X fills carry the offer's types; X→W fills fetch TARGETS at fire.
    mime_types: Vec<String>,
    owner: Option<x11rb::protocol::xproto::Window>,
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
    /// §10.1: false = backstop (default), true = --aggressive-claims.
    pub aggressive_claims: bool,
    /// Once-per-epoch guard for aggressive re-claims from bridge claims.
    reclaimed_epoch: [Option<u64>; 2],
    /// Xwayland WM check window — telemetry for §10.1 diagnosis.
    pub wm_window: Option<x11rb::protocol::xproto::Window>,
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
            aggressive_claims: options.aggressive_claims,
            reclaimed_epoch: [None, None],
            wm_window: None,
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

    /// List an incoming offer's types alongside our own live claim — the two
    /// lists are what every mirror-versus-copy question turns on. §8: types
    /// stay out of the log entirely for sensitive offers.
    fn trace_offer_types(&self, kind: SelKind, mime_types: &[String]) {
        if mime::is_sensitive(mime_types) {
            trace!(
                "event=offer_types side=wayland sel={} sensitive=true",
                kind.key()
            );
            return;
        }
        let claim = self.ctx[kind.idx()].our_x11_claim.as_ref();
        trace!(
            "event=offer_types side=wayland sel={} mimes={mime_types:?} own_claim={:?} claim_age_ms={:?}",
            kind.key(),
            claim.map(|c| &c.mime_types),
            claim.map(|c| c.at.elapsed().as_millis()),
        );
    }

    /// True when this offer is the Xwayland WM mirroring the proxy claim we
    /// just made for the very same content.
    fn is_own_claim_echo(&self, kind: SelKind, mime_types: &[String]) -> bool {
        self.ctx[kind.idx()]
            .our_x11_claim
            .as_ref()
            .is_some_and(|claim| claim.is_echo(mime_types))
    }

    pub fn on_wayland_primary(&mut self, offer: Option<Offer>) {
        if self.primary {
            self.on_wayland_selection(SelKind::Primary, offer);
        } else if let Some(o) = offer {
            o.destroy();
        }
    }

    fn process_wayland_selection(&mut self, kind: SelKind, offer: Option<Offer>) {
        if let Some(o) = &offer {
            self.trace_offer_types(kind, &o.mime_types());
        }
        // §4.3: the Xwayland WM re-publishing our own proxy claim is not a
        // copy. Falling through would destroy `current_offer` below — the
        // live source the mirror itself ultimately reads through — leaving
        // the whole chain dead-ended on a cancelled source, so every paste
        // (including one already streaming) yields nothing.
        if let Some(o) = &offer
            && self.is_own_claim_echo(kind, &o.mime_types())
        {
            debug!(
                "event=coexist side=wayland sel={} action=observe-own-claim",
                kind.key()
            );
            o.destroy();
            return;
        }
        if let Some(old) = self.ctx[kind.idx()].current_offer.take() {
            old.destroy();
        }
        self.ctx[kind.idx()].wl_gen += 1;
        if let Some(offer) = offer {
            let mime_types = offer.mime_types();
            // §10.1: another bridge's mirror of the X11 side is never a
            // reason for us to act — its own copy event already ran (or
            // will run) our gap logic. Track the change, touch nothing.
            if mime::is_x11_mirror(&mime_types) {
                debug!(
                    "event=coexist side=wayland sel={} action=observe-mirror mimes={}",
                    kind.key(),
                    mime_types.len()
                );
                offer.destroy();
                return;
            }
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
            // Causality pairing (§10.1): a Wayland update while we are
            // waiting to fill the Wayland side is the answer to the X11
            // copy that scheduled the wait — another bridge acted. Consume
            // the pending fill and never bridge the answer back.
            if !self.aggressive_claims
                && self.ctx[kind.idx()]
                    .pending_fill
                    .as_ref()
                    .is_some_and(|f| !f.fill_x11)
            {
                self.ctx[kind.idx()].pending_fill = None;
                debug!(
                    "event=backstop sel={} fill=wayland action=stand-down reason=bridged",
                    kind.key()
                );
                return;
            }
            if bridgeable.is_empty() {
                debug!("event=offer side=wayland sel={} bridgeable=0", kind.key());
                self.dispatch_broker(kind, broker::Event::WaylandCleared);
            } else if self.aggressive_claims {
                self.ctx[kind.idx()].x11_owner = None;
                self.dispatch_broker(
                    kind,
                    broker::Event::WaylandSelection {
                        mime_types: bridgeable,
                    },
                );
            } else {
                // Backstop (§10.1): give any other bridge GAP_WINDOW to
                // mirror this copy to X11; claim only if X11 stays silent.
                self.schedule_gap_fill(
                    kind,
                    PendingFill {
                        fill_x11: true,
                        wl_gen: self.ctx[kind.idx()].wl_gen,
                        x11_gen: self.ctx[kind.idx()].x11_gen,
                        mime_types: bridgeable,
                        owner: None,
                    },
                );
            }
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
                    self.ctx[kind.idx()].x11_owner = Some(owner);
                    self.dispatch_broker(kind, broker::Event::X11Selection);
                }
                Ok(_) => {}
                Err(e) => self.fatal(e.context("startup X11 owner probe")),
            }
        }
    }

    // --- Backstop gap-fill (§10.1) -----------------------------------------

    fn schedule_gap_fill(&mut self, kind: SelKind, fill: PendingFill) {
        debug!(
            "event=backstop sel={} fill={} action=scheduled",
            kind.key(),
            if fill.fill_x11 { "x11" } else { "wayland" }
        );
        self.ctx[kind.idx()].pending_fill = Some(fill);
        if let Some(handle) = &self.loop_handle {
            let timer = calloop::timer::Timer::from_duration(GAP_WINDOW);
            let result = handle.insert_source(timer, move |_, (), app: &mut Self| {
                app.fire_gap_fill(kind);
                calloop::timer::TimeoutAction::Drop
            });
            if result.is_ok() {
                return;
            }
        }
        // No loop yet (startup roundtrip) — fill immediately; nothing else
        // has had a chance to act during a synchronous startup anyway.
        self.fire_gap_fill(kind);
    }

    fn fire_gap_fill(&mut self, kind: SelKind) {
        let Some(fill) = self.ctx[kind.idx()].pending_fill.take() else {
            return;
        };
        let ctx = &self.ctx[kind.idx()];
        // Superseded on either side → the void got filled (or the copy got
        // replaced); stand down.
        if ctx.wl_gen != fill.wl_gen || ctx.x11_gen != fill.x11_gen {
            debug!(
                "event=backstop sel={} fill={} action=stand-down reason=other-bridge-acted",
                kind.key(),
                if fill.fill_x11 { "x11" } else { "wayland" }
            );
            return;
        }
        // Already providing the target side means our existing claim is
        // STALE — the copy that scheduled this fill superseded whatever we
        // were proxying. Refresh (re-dispatch) rather than skip: skipping
        // leaves X11 advertising a dead offer's types forever (observed:
        // Wine enumerating a long-gone Chromium target list, every read
        // 0 bytes, screenshots never crossing). Mirror echoes can't reach
        // here: fingerprinted offers die at ingress and bridge answers are
        // consumed by causality pairing.
        let refresh = if fill.fill_x11 {
            self.x11.owned_since[kind.idx()].is_some()
        } else {
            ctx.our_source.is_some()
        };
        debug!(
            "event=backstop sel={} fill={} action={}",
            kind.key(),
            if fill.fill_x11 { "x11" } else { "wayland" },
            if refresh { "refresh" } else { "fill" }
        );
        if fill.fill_x11 {
            self.ctx[kind.idx()].x11_owner = None;
            self.dispatch_broker(
                kind,
                broker::Event::WaylandSelection {
                    mime_types: fill.mime_types,
                },
            );
        } else {
            self.ctx[kind.idx()].x11_owner = fill.owner;
            self.dispatch_broker(kind, broker::Event::X11Selection);
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

    /// Eager survival (§4.2.1a): when the source app exits while we hold —
    /// or are still pulling — its content, keep the opposite-side proxy claim
    /// alive and serve from memory instead of tearing everything down.
    fn survives_source_exit(&self, kind: SelKind, source_state_is_x11: bool) -> bool {
        let ctx = &self.ctx[kind.idx()];
        match ctx.broker.state() {
            // W→X (§4.2.2): the source going away is the *expected* outcome
            // of our own claim under a mirroring Xwayland bridge, not a
            // reason to release. The capture is what the claim rests on, so
            // this holds in lazy mode too.
            broker::State::WaylandApp { .. } if !source_state_is_x11 => {
                ctx.capture_epoch.is_some() || ctx.snapshot.is_some()
            }
            broker::State::X11App { .. } if source_state_is_x11 => {
                self.sync_mode == SyncMode::Eager && ctx.snapshot.is_some()
            }
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
                    self.ctx[kind.idx()].our_x11_claim = None;
                    self.dispatch_broker(kind, broker::Event::X11Lost);
                }
            }
            Event::SelectionRequest(req) => self.on_selection_request(*req),
            Event::XfixesSelectionNotify(e) => self.on_xfixes_notify(e),
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

    fn on_xfixes_notify(&mut self, e: &x11rb::protocol::xfixes::SelectionNotifyEvent) {
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
        self.ctx[kind.idx()].x11_gen += 1;
        if has_owner {
            // §10.1: another bridge's X11 claims (the Xwayland WM mirroring
            // the Wayland side) are tracked as a change but never proxied —
            // bridging a bridge loops.
            if self.wm_window.is_some_and(|wm| wm == e.owner) {
                // --aggressive-claims: take our claim back from the bridge
                // (its mirror is a text-only subset), once per epoch so a
                // persistent bridge can't drive a war. Never applies to
                // real applications.
                let epoch = self.ctx[kind.idx()].broker.epoch();
                if self.aggressive_claims
                    && matches!(
                        self.ctx[kind.idx()].broker.state(),
                        broker::State::WaylandApp { .. }
                    )
                    && self.reclaimed_epoch[kind.idx()] != Some(epoch)
                {
                    self.reclaimed_epoch[kind.idx()] = Some(epoch);
                    info!(
                        "event=coexist side=x11 sel={} action=reclaim owner=0x{:x}",
                        kind.key(),
                        e.owner
                    );
                    match self.x11.claim(kind) {
                        Ok(pending) => {
                            for event in pending {
                                self.handle_x11_event(&event);
                            }
                        }
                        Err(err) => {
                            self.fatal(err.context("re-claim over bridge"));
                        }
                    }
                } else {
                    debug!(
                        "event=coexist side=x11 sel={} action=observe-wm-claim",
                        kind.key()
                    );
                }
                return;
            }
            let class = self.x11.owner_class(e.owner);
            info!(
                "event=owner side=x11 sel={} owner=0x{:x} class={:?}",
                kind.key(),
                e.owner,
                class.as_deref().unwrap_or("unknown")
            );
            // Causality pairing (§10.1): an X11 update while we are waiting
            // to fill the X11 side answers the Wayland copy that scheduled
            // the wait — another bridge acted. Consume, never re-bridge.
            if !self.aggressive_claims
                && self.ctx[kind.idx()]
                    .pending_fill
                    .as_ref()
                    .is_some_and(|f| f.fill_x11)
            {
                self.ctx[kind.idx()].pending_fill = None;
                debug!(
                    "event=backstop sel={} fill=x11 action=stand-down reason=bridged",
                    kind.key()
                );
                return;
            }
            if self.aggressive_claims {
                self.ctx[kind.idx()].x11_owner = Some(e.owner);
                self.dispatch_broker(kind, broker::Event::X11Selection);
            } else {
                // Backstop (§10.1): claim Wayland only if it stays silent
                // for GAP_WINDOW after this X11 copy.
                self.schedule_gap_fill(
                    kind,
                    PendingFill {
                        fill_x11: false,
                        wl_gen: self.ctx[kind.idx()].wl_gen,
                        x11_gen: self.ctx[kind.idx()].x11_gen,
                        mime_types: Vec::new(),
                        owner: Some(e.owner),
                    },
                );
            }
        } else if self.survives_source_exit(kind, true) {
            debug!(
                "event=snapshot_serve side=x11 sel={} reason=source-exited",
                kind.key()
            );
        } else {
            self.ctx[kind.idx()].x11_owner = None;
            self.dispatch_broker(kind, broker::Event::X11Cleared);
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
        // §4.2.2: a capture for this claim is still in flight. The Wayland
        // source may already be cancelled, so the snapshot is the only place
        // the bytes will ever be — park the request rather than race it into
        // an `empty-source` refusal. `collect_snapshot`'s zero-progress bound
        // means the answer always arrives.
        let ctx = &self.ctx[kind.idx()];
        if ctx.capture_epoch.is_some_and(|e| e == ctx.broker.epoch()) {
            debug!(
                "event=park dir=w2x sel={} mime={mime:?} reason=capture-in-flight",
                kind.key()
            );
            let epoch = ctx.broker.epoch();
            self.ctx[kind.idx()].parked.push(ParkedPaste {
                epoch,
                req,
                property,
                mime,
                reply_type,
                conversion,
                transform,
            });
            return;
        }
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
        // A request for a type the live offer doesn't carry can only yield
        // a bogus empty payload — refuse promptly instead (requestors like
        // Wine handle refusals per-format gracefully).
        if !offer.mime_types().iter().any(|m| m == &mime) {
            debug!(
                "event=refuse target-mime={mime:?} reason=not-in-live-offer sel={}",
                kind.key()
            );
            transfer::notify(&self.x11.conn, &req, None);
            return;
        }
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

    /// §4.2.2 W→X capture: pull every bridgeable type out of the Wayland
    /// source *before* the caller takes the X11 selection.
    ///
    /// An Xwayland bridge that also syncs X→W republishes our fresh X11 claim
    /// onto the Wayland clipboard within a millisecond of us making it
    /// (~0.3 ms observed with xwayland-satellite 0.8). That republish is a
    /// selection change like any other, so the compositor cancels the source
    /// we are proxying — `wl-copy` exits, and the screenshot is gone before
    /// the first paste arrives. Lazy proxying (§4.2) cannot survive that: by
    /// the time an X11 client asks, there is nothing left to read.
    ///
    /// Issuing `receive()` first puts the `send` events in the source's queue
    /// ahead of any cancellation, so the bytes are already on their way.
    /// Ordering is the whole point — doing this after the claim is a race we
    /// lose. In backstop mode we only claim into genuine voids, so this costs
    /// a transfer exactly when the alternative was losing the content.
    fn start_w2x_capture(&mut self, kind: SelKind, mimes: &[String]) {
        let Some(tx) = self.snapshot_tx.clone() else {
            return; // no event loop yet (startup roundtrip) — nothing to park
        };
        let Some(offer) = self.ctx[kind.idx()].current_offer.clone() else {
            return;
        };
        let epoch = self.ctx[kind.idx()].broker.epoch();
        let (cap, timeout) = (self.eager_max, self.transfer_timeout);
        let mut pipes = Vec::new();
        for mime in mimes {
            match std::io::pipe() {
                Ok((reader, writer)) => {
                    offer.receive(mime, std::os::fd::AsFd::as_fd(&writer));
                    drop(writer);
                    pipes.push((mime.clone(), reader));
                }
                Err(e) => error!("event=capture error={:?}", e.to_string()),
            }
        }
        if pipes.is_empty() {
            return;
        }
        if let Err(e) = self.wl_conn.flush() {
            self.fatal(anyhow!(e).context("flush Wayland after capture receive"));
            return;
        }
        debug!(
            "event=capture dir=w2x sel={} types={} action=started",
            kind.key(),
            pipes.len()
        );
        self.ctx[kind.idx()].capture_epoch = Some(epoch);
        std::thread::spawn(move || {
            collect_snapshot(kind, epoch, pipes, cap, timeout, "w2x", &tx);
        });
    }

    /// §4.2.1 X→W eager snapshot: read the X11 owner's targets now so the
    /// content survives the owner exiting.
    fn start_eager_fetch(&self, kind: SelKind, mimes: &[String]) {
        if self.sync_mode != SyncMode::Eager {
            return;
        }
        let Some(tx) = self.snapshot_tx.clone() else {
            return;
        };
        if !matches!(
            self.ctx[kind.idx()].broker.state(),
            broker::State::X11App { .. }
        ) {
            return;
        }
        let epoch = self.ctx[kind.idx()].broker.epoch();
        let (cap, timeout) = (self.eager_max, self.transfer_timeout);
        // One lazy read per type into pipes, then collect. spawn_x11_read
        // serializes via the X→W gate.
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
            collect_snapshot(kind, epoch, pipes, cap, timeout, "x2w", &tx);
        });
    }

    /// Snapshot fetch finished: adopt it if it still matches the epoch, then
    /// answer anything parked on it either way — a superseded capture still
    /// owes its requestors a reply (§4.2.2).
    pub fn on_snapshot(&mut self, msg: SnapshotMsg) {
        let kind = msg.kind;
        let epoch = msg.snapshot.epoch;
        let ctx = &mut self.ctx[kind.idx()];
        if epoch == ctx.broker.epoch() {
            if !msg.snapshot.lock_in_memory() {
                debug!("event=mlock status=partial");
            }
            let types = msg.snapshot.data.len();
            ctx.snapshot = Some(Arc::new(msg.snapshot));
            if ctx.sensitive {
                info!("event=snapshot sel={} sensitive=true", kind.key());
            } else {
                debug!("event=snapshot sel={} types={types}", kind.key());
            }
        }
        // Superseded claim: ropes zero on drop.
        if ctx.capture_epoch == Some(epoch) {
            ctx.capture_epoch = None;
            self.flush_parked(kind);
        }
    }

    /// Replay the pastes held for a capture that has now landed. Requests
    /// whose claim was superseded meanwhile can only be answered with stale
    /// bytes, so they are refused instead (§6: a refusal lets the requestor
    /// retry; an empty property would be cached as the clipboard contents).
    fn flush_parked(&mut self, kind: SelKind) {
        let parked = std::mem::take(&mut self.ctx[kind.idx()].parked);
        if parked.is_empty() {
            return;
        }
        let epoch = self.ctx[kind.idx()].broker.epoch();
        debug!(
            "event=unpark dir=w2x sel={} held={}",
            kind.key(),
            parked.len()
        );
        for p in parked {
            if p.epoch != epoch {
                debug!("event=refuse dir=w2x reason=superseded-capture");
                transfer::notify(&self.x11.conn, &p.req, None);
                continue;
            }
            self.start_paste(
                kind,
                p.req,
                p.property,
                p.mime,
                p.reply_type,
                p.conversion,
                p.transform,
            );
        }
        if let Err(e) = self.x11.conn.flush() {
            self.fatal(anyhow!(e).context("flush X11 after unparking pastes"));
        }
    }

    /// Refuse everything parked: whatever they were waiting for is gone.
    fn discard_parked(&mut self, kind: SelKind) {
        let parked = std::mem::take(&mut self.ctx[kind.idx()].parked);
        if parked.is_empty() {
            return;
        }
        debug!(
            "event=refuse dir=w2x sel={} held={} reason=claim-gone",
            kind.key(),
            parked.len()
        );
        for p in parked {
            transfer::notify(&self.x11.conn, &p.req, None);
        }
        if let Err(e) = self.x11.conn.flush() {
            self.fatal(anyhow!(e).context("flush X11 after discarding pastes"));
        }
    }

    // --- Broker plumbing ------------------------------------------------------

    fn dispatch_broker(&mut self, kind: SelKind, event: broker::Event) {
        let before = self.ctx[kind.idx()].broker.epoch();
        let commands = self.ctx[kind.idx()].broker.handle(event);
        if self.ctx[kind.idx()].broker.epoch() != before {
            // Every legitimate ownership change invalidates the snapshot;
            // a fresh one arrives after the new claim (§4.2.1). Anything
            // parked on the old capture is owed a refusal before the
            // commands below start a new one (§4.2.2).
            self.ctx[kind.idx()].snapshot = None;
            self.ctx[kind.idx()].capture_epoch = None;
            self.discard_parked(kind);
        }
        for command in commands {
            self.run_command(kind, &command);
        }
    }

    /// W→X: intern the offer's types as X11 targets, capture the payload,
    /// then take the selection. Order matters — see `start_w2x_capture`.
    fn claim_x11(&mut self, kind: SelKind) {
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
                for (target, source_mime, transform) in mime::synthesized_x11_targets(mime_types) {
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
        // §4.2.2: the bytes have to be on their way out of the Wayland source
        // before the claim, because the claim is what kills it.
        if !mimes.is_empty() {
            self.start_w2x_capture(kind, &mimes);
        }
        match self.x11.claim(kind) {
            Ok(pending) => {
                // Only W→X proxy claims get mirrored back onto the Wayland
                // clipboard; an empty list would make the subset test in
                // `is_own_claim_echo` match anything.
                self.ctx[kind.idx()].our_x11_claim = (!mimes.is_empty()).then(|| OwnX11Claim {
                    at: Instant::now(),
                    mime_types: mimes,
                });
                for event in pending {
                    self.handle_x11_event(&event);
                }
            }
            Err(e) => self.fatal(e.context("claim X11 selection")),
        }
    }

    fn run_command(&mut self, kind: SelKind, command: &Command) {
        match command {
            Command::ClaimX11 { .. } => self.claim_x11(kind),
            Command::ReleaseX11 => {
                self.ctx[kind.idx()].proxy_targets.clear();
                self.ctx[kind.idx()].our_x11_claim = None;
                self.ctx[kind.idx()].capture_epoch = None;
                self.discard_parked(kind);
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

/// Read each pipe to EOF (bounded by `cap` per type) and deliver the snapshot
/// back to the event loop. Over-cap or failed types are skipped — they degrade
/// to lazy (§4.2.1), which on the W→X capture path (§4.2.2) means the type is
/// lost outright once the bridge cancels the source, so say so out loud there.
fn collect_snapshot(
    kind: SelKind,
    epoch: u64,
    pipes: Vec<(String, std::io::PipeReader)>,
    cap: Option<usize>,
    timeout: Option<Duration>,
    dir: &'static str,
    tx: &calloop::channel::Sender<SnapshotMsg>,
) {
    let mut data = HashMap::new();
    for (mime, reader) in pipes {
        // Same zero-progress bound as lazy transfers: a source that never
        // writes must not pin the collector (§4.2 governs progress only).
        let mut reader = transfer::bounded_reader(reader, timeout);
        match PayloadRope::read_to_end(&mut reader, cap) {
            Ok(ReadOutcome::Complete(rope)) => {
                if !rope.is_empty() {
                    data.insert(mime, rope);
                }
            }
            Ok(ReadOutcome::Overflow(_)) if dir == "w2x" => {
                warn!(
                    "event=snapshot_skip dir={dir} sel={} mime={mime:?} reason=over-cap",
                    kind.key()
                );
            }
            Ok(ReadOutcome::Overflow(_)) => {
                debug!(
                    "event=snapshot_skip dir={dir} sel={} mime={mime:?} reason=over-cap",
                    kind.key()
                );
            }
            Err(e) => debug!(
                "event=snapshot_skip dir={dir} sel={} mime={mime:?} error={:?}",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn claim_now(list: &[&str]) -> OwnX11Claim {
        OwnX11Claim {
            at: Instant::now(),
            mime_types: mimes(list),
        }
    }

    /// An instant far enough back that `MIRROR_ECHO_WINDOW` has expired.
    fn aged_out() -> Instant {
        Instant::now()
            .checked_sub(MIRROR_ECHO_WINDOW + Duration::from_millis(1))
            .expect("clock far enough past boot to predate the echo window")
    }

    /// The niri-screenshot regression: we backstop-claim X11 for an
    /// `image/png` offer and Xwayland republishes it onto Wayland ~0.3 ms
    /// later. Mistaking that for a copy destroys the live offer and every
    /// subsequent paste reads a cancelled source.
    #[test]
    fn own_claim_mirrored_back_is_an_echo() {
        assert!(claim_now(&["image/png"]).is_echo(&mimes(&["image/png"])));
    }

    /// Translation drops protocol targets, so the mirror usually comes back
    /// as a strict subset of what we advertised.
    #[test]
    fn echo_may_be_a_subset_of_the_claim() {
        let claim = claim_now(&["text/plain;charset=utf-8", "text/html", "STRING"]);
        assert!(claim.is_echo(&mimes(&["text/html"])));
    }

    /// What xwayland-satellite actually mirrors back: our advertised types
    /// *plus* `TIMESTAMP` from the X11 `TARGETS` list. Comparing the raw
    /// lists makes that a superset and the echo goes unrecognised — the
    /// live offer is destroyed and the paste comes back empty (the
    /// two-screenshots-to-copy report).
    #[test]
    fn satellite_mirror_re_exports_timestamp() {
        assert!(claim_now(&["image/png"]).is_echo(&mimes(&["image/png", "TIMESTAMP"])));
        let claim = claim_now(&["text/plain;charset=utf-8", "STRING", "text/plain"]);
        assert!(claim.is_echo(&mimes(&["TIMESTAMP", "STRING", "text/plain"])));
    }

    /// A protocol-fingerprinted mirror is identified by what it is, not when
    /// it arrived: no Wayland application advertises `TIMESTAMP`, so the
    /// timing window is irrelevant for it.
    #[test]
    fn fingerprinted_mirror_is_an_echo_past_the_window() {
        let claim = OwnX11Claim {
            at: aged_out(),
            mime_types: mimes(&["image/png"]),
        };
        assert!(claim.is_echo(&mimes(&["image/png", "TIMESTAMP"])));
    }

    /// A mirror carrying nothing but protocol machinery has no content to
    /// match against — never let it swallow a selection change.
    #[test]
    fn protocol_only_offer_is_never_an_echo() {
        assert!(!claim_now(&["image/png"]).is_echo(&mimes(&["TARGETS", "TIMESTAMP"])));
    }

    /// A real copy adding a type we never advertised is not our echo.
    #[test]
    fn superset_offer_is_a_real_copy() {
        let claim = claim_now(&["image/png"]);
        assert!(!claim.is_echo(&mimes(&["image/png", "text/uri-list"])));
    }

    /// Past the window an identical, unfingerprinted offer is a genuine
    /// re-copy: a human copying the same content again must still bridge.
    #[test]
    fn identical_offer_after_the_window_is_a_real_copy() {
        let claim = OwnX11Claim {
            at: aged_out(),
            mime_types: mimes(&["image/png"]),
        };
        assert!(!claim.is_echo(&mimes(&["image/png"])));
    }

    /// An empty offer must never match — otherwise a claim with no types
    /// would swallow arbitrary selection changes.
    #[test]
    fn empty_offer_is_never_an_echo() {
        assert!(!claim_now(&["image/png"]).is_echo(&[]));
    }
}
