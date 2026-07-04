//! The broker: a pure state machine — events in, commands out (sans-IO per
//! DESIGN.md §12), so every transition is testable without a display server.
//!
//! M1 scope: Wayland → X11 only. The `X11App` state and the X11-side events
//! arrive with M2.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Neither side has a selection we care about.
    Idle,
    /// A real Wayland client owns the selection; we proxy it on the X11 side.
    WaylandApp { mime_types: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A real Wayland client took the selection, offering these MIME types.
    /// (Self-claims are filtered out by identity before reaching the broker,
    /// §4.3 — and in M1 we never create a Wayland source at all.)
    WaylandSelection { mime_types: Vec<String> },
    /// The Wayland selection was cleared (owner exited or set to null).
    WaylandCleared,
    /// A real X11 client took the X11 selection away from us
    /// (`SelectionClear`). In M1 we just stand down; M2 turns this into a
    /// Wayland-side claim.
    X11Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Claim the X11 CLIPBOARD as proxy for the current Wayland owner.
    ClaimX11 { epoch: u64 },
    /// Drop our X11 claim.
    ReleaseX11,
}

pub struct Broker {
    state: State,
    /// Incremented per legitimate claim; guards against late-arriving events
    /// from a superseded state (§4.3).
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
                self.state = State::WaylandApp { mime_types };
                vec![Command::ClaimX11 { epoch: self.epoch }]
            }
            Event::WaylandCleared => {
                self.epoch += 1;
                let previous = std::mem::replace(&mut self.state, State::Idle);
                match previous {
                    State::WaylandApp { .. } => vec![Command::ReleaseX11],
                    State::Idle => vec![],
                }
            }
            Event::X11Lost => {
                self.epoch += 1;
                self.state = State::Idle;
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_offer() -> Event {
        Event::WaylandSelection {
            mime_types: vec!["text/plain;charset=utf-8".into()],
        }
    }

    #[test]
    fn wayland_selection_claims_x11() {
        let mut b = Broker::new();
        let cmds = b.handle(text_offer());
        assert_eq!(cmds, vec![Command::ClaimX11 { epoch: 1 }]);
        assert!(matches!(b.state(), State::WaylandApp { .. }));
    }

    #[test]
    fn cleared_releases_only_when_proxying() {
        let mut b = Broker::new();
        assert_eq!(b.handle(Event::WaylandCleared), vec![]);
        b.handle(text_offer());
        assert_eq!(b.handle(Event::WaylandCleared), vec![Command::ReleaseX11]);
        assert_eq!(b.state(), &State::Idle);
    }

    #[test]
    fn x11_lost_stands_down_without_commands() {
        let mut b = Broker::new();
        b.handle(text_offer());
        assert_eq!(b.handle(Event::X11Lost), vec![]);
        assert_eq!(b.state(), &State::Idle);
        // A later clear of the (already superseded) Wayland side is a no-op.
        assert_eq!(b.handle(Event::WaylandCleared), vec![]);
    }

    #[test]
    fn epoch_increments_per_transition_and_claims_are_counted() {
        // Broker-level slice of the design's 100-alternating-copies test:
        // every legitimate new selection must produce exactly one claim.
        let mut b = Broker::new();
        let mut claims = 0;
        for _ in 0..100 {
            claims += b
                .handle(text_offer())
                .iter()
                .filter(|c| matches!(c, Command::ClaimX11 { .. }))
                .count();
        }
        assert_eq!(claims, 100);
        assert_eq!(b.epoch(), 100);
    }

    #[test]
    fn identical_content_still_counts_as_new_claim() {
        // Loop prevention is by identity, not content — two copies of the
        // same MIME list are two claims (§12 self-echo test, broker slice).
        let mut b = Broker::new();
        assert_eq!(b.handle(text_offer()), vec![Command::ClaimX11 { epoch: 1 }]);
        assert_eq!(b.handle(text_offer()), vec![Command::ClaimX11 { epoch: 2 }]);
    }
}
