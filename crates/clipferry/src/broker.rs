//! The broker: a pure state machine — events in, commands out (sans-IO per
//! DESIGN.md §12), so every transition is testable without a display server.
//!
//! Identity filtering happens *before* events reach the broker (§4.3): the
//! X11 backend drops XFIXES notifies whose owner is our own window, and the
//! Wayland backend drops selection events while our own data source is
//! alive (a real takeover always cancels our source first). The broker only
//! ever sees events caused by real applications.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Neither side has a selection we care about.
    Idle,
    /// A real Wayland client owns the selection; we proxy it on the X11 side.
    WaylandApp { mime_types: Vec<String> },
    /// A real X11 client took the X11 selection; we are waiting for its
    /// TARGETS reply before claiming the Wayland side.
    X11AppPendingTargets,
    /// A real X11 client owns the selection; we proxy it on the Wayland side.
    X11App { mime_types: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A real Wayland client took the selection, offering these MIME types.
    WaylandSelection { mime_types: Vec<String> },
    /// The Wayland selection was cleared (owner exited or set to null).
    WaylandCleared,
    /// A real X11 client took the X11 selection (XFIXES, owner ≠ us).
    X11Selection,
    /// The X11 selection owner's TARGETS arrived, already translated to
    /// Wayland MIME types. Tagged with the epoch of the fetch request so a
    /// stale reply from a superseded owner is ignored.
    X11Targets { epoch: u64, mime_types: Vec<String> },
    /// The X11 selection became ownerless (XFIXES owner = none, or the
    /// owner window/client went away).
    X11Cleared,
    /// We lost the X11 selection to a real client (`SelectionClear`) while
    /// proxying W→X. The matching XFIXES notify drives the new claim.
    X11Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Claim the X11 CLIPBOARD as proxy for the current Wayland owner.
    ClaimX11 { epoch: u64 },
    /// Drop our X11 claim.
    ReleaseX11,
    /// Ask the X11 owner for its TARGETS (async; answer comes back as
    /// `Event::X11Targets` with this epoch).
    FetchX11Targets { epoch: u64 },
    /// Claim the Wayland selection as proxy for the current X11 owner.
    ClaimWayland { epoch: u64, mime_types: Vec<String> },
    /// Drop our Wayland claim.
    ReleaseWayland,
}

pub struct Broker {
    state: State,
    /// Incremented per legitimate ownership change; guards against
    /// late-arriving results from a superseded state (§4.3).
    epoch: u64,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl Broker {
    pub const fn new() -> Self {
        Self {
            state: State::Idle,
            epoch: 0,
        }
    }

    pub const fn state(&self) -> &State {
        &self.state
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn handle(&mut self, event: Event) -> Vec<Command> {
        match event {
            Event::WaylandSelection { mime_types } => {
                self.epoch += 1;
                // If we were proxying X→W, the compositor already cancelled
                // our source before delivering this — no ReleaseWayland.
                self.state = State::WaylandApp { mime_types };
                vec![Command::ClaimX11 { epoch: self.epoch }]
            }
            Event::WaylandCleared => {
                self.epoch += 1;
                match std::mem::replace(&mut self.state, State::Idle) {
                    State::WaylandApp { .. } => vec![Command::ReleaseX11],
                    // A Wayland client cleared the selection while a real
                    // X11 app owns the other side: re-fill the missing side
                    // (§4.1) by claiming Wayland again.
                    State::X11App { mime_types } => {
                        let cmd = Command::ClaimWayland {
                            epoch: self.epoch,
                            mime_types: mime_types.clone(),
                        };
                        self.state = State::X11App { mime_types };
                        vec![cmd]
                    }
                    State::X11AppPendingTargets => {
                        // The claim happens once TARGETS arrive.
                        self.state = State::X11AppPendingTargets;
                        vec![]
                    }
                    State::Idle => vec![],
                }
            }
            Event::X11Selection => {
                self.epoch += 1;
                self.state = State::X11AppPendingTargets;
                vec![Command::FetchX11Targets { epoch: self.epoch }]
            }
            Event::X11Targets { epoch, mime_types } => {
                if epoch != self.epoch || self.state != State::X11AppPendingTargets {
                    return vec![]; // stale reply from a superseded owner
                }
                if mime_types.is_empty() {
                    // Nothing we can bridge (M2: text only); stand down.
                    self.state = State::Idle;
                    return vec![];
                }
                self.state = State::X11App {
                    mime_types: mime_types.clone(),
                };
                vec![Command::ClaimWayland {
                    epoch: self.epoch,
                    mime_types,
                }]
            }
            Event::X11Cleared => {
                self.epoch += 1;
                match std::mem::replace(&mut self.state, State::Idle) {
                    State::X11App { .. } | State::X11AppPendingTargets => {
                        vec![Command::ReleaseWayland]
                    }
                    // Our own ReleaseX11 echoes back as owner-none once
                    // we're already Idle; and while proxying W→X an
                    // owner-none can only be our own release.
                    State::WaylandApp { mime_types } => {
                        self.state = State::WaylandApp { mime_types };
                        vec![]
                    }
                    State::Idle => vec![],
                }
            }
            Event::X11Lost => {
                // Only meaningful while proxying W→X: a real X11 client is
                // taking over. The XFIXES notify that follows carries the
                // new owner and drives the Wayland claim.
                if matches!(self.state, State::WaylandApp { .. }) {
                    self.epoch += 1;
                    self.state = State::Idle;
                }
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Vec<String> {
        vec!["text/plain;charset=utf-8".into()]
    }

    fn wl_sel() -> Event {
        Event::WaylandSelection { mime_types: text() }
    }

    #[test]
    fn w_to_x_claim_and_release() {
        let mut b = Broker::new();
        assert_eq!(b.handle(wl_sel()), vec![Command::ClaimX11 { epoch: 1 }]);
        assert_eq!(b.handle(Event::WaylandCleared), vec![Command::ReleaseX11]);
        assert_eq!(b.state(), &State::Idle);
    }

    #[test]
    fn x_to_w_full_cycle() {
        let mut b = Broker::new();
        assert_eq!(
            b.handle(Event::X11Selection),
            vec![Command::FetchX11Targets { epoch: 1 }]
        );
        assert_eq!(b.state(), &State::X11AppPendingTargets);
        assert_eq!(
            b.handle(Event::X11Targets {
                epoch: 1,
                mime_types: text()
            }),
            vec![Command::ClaimWayland {
                epoch: 1,
                mime_types: text()
            }]
        );
        assert!(matches!(b.state(), State::X11App { .. }));
        assert_eq!(b.handle(Event::X11Cleared), vec![Command::ReleaseWayland]);
        assert_eq!(b.state(), &State::Idle);
    }

    #[test]
    fn stale_targets_are_ignored() {
        let mut b = Broker::new();
        b.handle(Event::X11Selection); // epoch 1
        b.handle(wl_sel()); // epoch 2 — supersedes the X11 owner
        assert_eq!(
            b.handle(Event::X11Targets {
                epoch: 1,
                mime_types: text()
            }),
            vec![]
        );
        assert!(matches!(b.state(), State::WaylandApp { .. }));
    }

    #[test]
    fn empty_targets_stand_down() {
        let mut b = Broker::new();
        b.handle(Event::X11Selection);
        assert_eq!(
            b.handle(Event::X11Targets {
                epoch: 1,
                mime_types: vec![]
            }),
            vec![]
        );
        assert_eq!(b.state(), &State::Idle);
    }

    #[test]
    fn x11_lost_then_new_owner_claims_wayland() {
        let mut b = Broker::new();
        b.handle(wl_sel()); // we proxy W→X
        assert_eq!(b.handle(Event::X11Lost), vec![]); // real X11 app takes over
        assert_eq!(b.state(), &State::Idle);
        // XFIXES notify for the new owner follows:
        assert_eq!(
            b.handle(Event::X11Selection),
            vec![Command::FetchX11Targets { epoch: 3 }]
        );
    }

    #[test]
    fn own_release_echo_is_inert() {
        let mut b = Broker::new();
        b.handle(Event::X11Selection);
        b.handle(Event::X11Targets {
            epoch: 1,
            mime_types: text(),
        });
        b.handle(Event::X11Cleared); // → Idle + ReleaseWayland
        // Our ReleaseWayland produces a selection(null) echo:
        assert_eq!(b.handle(Event::WaylandCleared), vec![]);
        assert_eq!(b.state(), &State::Idle);
    }

    #[test]
    fn wayland_clear_while_x11_owner_refills() {
        let mut b = Broker::new();
        b.handle(Event::X11Selection);
        b.handle(Event::X11Targets {
            epoch: 1,
            mime_types: text(),
        });
        // Some Wayland client sets the selection to null; the X11 app still
        // owns its side → re-fill the empty Wayland side.
        let cmds = b.handle(Event::WaylandCleared);
        assert_eq!(
            cmds,
            vec![Command::ClaimWayland {
                epoch: 2,
                mime_types: text()
            }]
        );
        assert!(matches!(b.state(), State::X11App { .. }));
    }

    #[test]
    fn alternating_copies_produce_exactly_one_claim_each() {
        // Broker-level slice of the §12 loop test: 100 alternating copies
        // must produce exactly 100 claims, zero extra.
        let mut b = Broker::new();
        let mut x11_claims = 0;
        let mut wl_claims = 0;
        for i in 0..100 {
            if i % 2 == 0 {
                for c in b.handle(wl_sel()) {
                    assert!(matches!(c, Command::ClaimX11 { .. }));
                    x11_claims += 1;
                }
            } else {
                let epoch = match b.handle(Event::X11Selection).as_slice() {
                    [Command::FetchX11Targets { epoch }] => *epoch,
                    other => panic!("unexpected commands: {other:?}"),
                };
                for c in b.handle(Event::X11Targets {
                    epoch,
                    mime_types: text(),
                }) {
                    assert!(matches!(c, Command::ClaimWayland { .. }));
                    wl_claims += 1;
                }
            }
        }
        assert_eq!((x11_claims, wl_claims), (50, 50));
    }

    #[test]
    fn identical_content_still_counts_as_new_claim() {
        let mut b = Broker::new();
        assert_eq!(b.handle(wl_sel()), vec![Command::ClaimX11 { epoch: 1 }]);
        assert_eq!(b.handle(wl_sel()), vec![Command::ClaimX11 { epoch: 2 }]);
    }
}
