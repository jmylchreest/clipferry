//! clipferry — a lazy, event-driven X11 ⇄ Wayland clipboard bridge.
//!
//! Architecture and rationale live in `DESIGN.md` at the repository root.
//! The crate is split lib/bin so the broker state machine (sans-IO: events
//! in, commands out) is unit-testable without a display server.

pub mod app;
pub mod broker;
pub mod cli;
pub mod logging;
pub mod mime;
pub mod payload;
pub mod sandbox;
pub mod transfer;
pub mod wayland;
pub mod x11;

/// Which selection a piece of state belongs to. PRIMARY is opt-in (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelKind {
    Clipboard,
    Primary,
}

impl SelKind {
    pub const ALL: [Self; 2] = [Self::Clipboard, Self::Primary];

    #[must_use]
    pub const fn idx(self) -> usize {
        match self {
            Self::Clipboard => 0,
            Self::Primary => 1,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clipboard => "CLIPBOARD",
            Self::Primary => "PRIMARY",
        }
    }

    /// logfmt field value (`sel=`).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Primary => "primary",
        }
    }
}

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
