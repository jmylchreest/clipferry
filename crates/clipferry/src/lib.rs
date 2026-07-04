//! clipferry — a lazy, event-driven X11 ⇄ Wayland clipboard bridge.
//!
//! Architecture and rationale live in `DESIGN.md` at the repository root.
//! The crate is split lib/bin so the broker state machine (sans-IO: events
//! in, commands out) is unit-testable without a display server.

pub const NAME: &str = env!("CARGO_PKG_NAME");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn version_is_semver_shaped() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3);
        for part in parts {
            part.parse::<u64>().unwrap();
        }
    }
}
