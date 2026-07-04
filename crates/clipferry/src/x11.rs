//! X11 backend (§6): one invisible unmapped window as selection owner,
//! serving TARGETS/TIMESTAMP plus text targets in M1. INCR lands in M3;
//! XFIXES selection watching lands in M2.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use log::{info, warn};
use x11rb::atom_manager;
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Timestamp, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

atom_manager! {
    pub Atoms:
    AtomsCookie {
        CLIPBOARD,
        TARGETS,
        TIMESTAMP,
        MULTIPLE,
        INCR,
        UTF8_STRING,
        TEXT,
        TEXT_PLAIN: b"text/plain",
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        CLIPFERRY: b"_CLIPFERRY",
        CLIPFERRY_TARGETS: b"_CLIPFERRY_TARGETS",
    }
}

pub struct X11 {
    pub conn: Arc<RustConnection>,
    pub win: Window,
    pub atoms: Atoms,
    /// Server timestamp of our current `CLIPBOARD` ownership; `None` when a
    /// real X11 app (or nobody) owns the selection.
    pub owned_since: Option<Timestamp>,
    /// Largest payload we can ship in a single non-INCR `ChangeProperty`.
    pub max_payload: usize,
    pub xfixes_version: (u32, u32),
}

/// Retry with backoff (500 ms → cap 5 s) until `$DISPLAY` appears (§6);
/// log once at INFO, not per attempt.
pub fn connect_with_retry() -> (Arc<RustConnection>, usize) {
    let mut delay = Duration::from_millis(500);
    let mut logged = false;
    loop {
        match RustConnection::connect(None) {
            Ok((conn, screen)) => return (Arc::new(conn), screen),
            Err(e) => {
                if !logged {
                    info!("X11 connect failed ({e}); retrying until $DISPLAY appears");
                    logged = true;
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

impl X11 {
    pub fn new(conn: Arc<RustConnection>, screen_num: usize) -> anyhow::Result<Self> {
        let screen = &conn.setup().roots[screen_num];
        let win = conn.generate_id().context("allocate window id")?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            screen.root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            // PROPERTY_CHANGE feeds the server-timestamp trick in claim().
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .context("create selection window")?;

        let atoms = Atoms::new(&*conn).context("intern atoms")?.reply()?;
        let xfixes_reply = conn
            .xfixes_query_version(5, 0)
            .context("XFIXES is mandatory (§6)")?
            .reply()
            .context("XFIXES version handshake")?;
        conn.xfixes_select_selection_input(
            win,
            atoms.CLIPBOARD,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .context("subscribe to XFIXES selection events")?;

        // Headroom under the max request size for the ChangeProperty header.
        let max_payload = conn.maximum_request_bytes().saturating_sub(1024);
        conn.flush()?;

        Ok(Self {
            conn,
            win,
            atoms,
            owned_since: None,
            max_payload,
            xfixes_version: (xfixes_reply.major_version, xfixes_reply.minor_version),
        })
    }

    pub fn vendor(&self) -> String {
        String::from_utf8_lossy(&self.conn.setup().vendor).into_owned()
    }

    /// Fetch a real server timestamp via the ICCCM zero-length-append trick.
    /// Events that arrive while we wait are handed back for normal handling.
    fn server_time(&self, pending: &mut Vec<Event>) -> anyhow::Result<Timestamp> {
        self.conn
            .change_property8(
                x11rb::protocol::xproto::PropMode::APPEND,
                self.win,
                self.atoms.CLIPFERRY,
                AtomEnum::STRING,
                &[],
            )?
            .check()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(event) = self.conn.poll_for_event()? {
                if let Event::PropertyNotify(ref p) = event
                    && p.window == self.win
                    && p.atom == self.atoms.CLIPFERRY
                {
                    return Ok(p.time);
                }
                pending.push(event);
            } else if Instant::now() > deadline {
                bail!("timed out waiting for PropertyNotify server timestamp");
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Claim CLIPBOARD as proxy for the Wayland owner. Returns any unrelated
    /// events soaked up while acquiring the timestamp.
    pub fn claim(&mut self) -> anyhow::Result<Vec<Event>> {
        let mut pending = Vec::new();
        let time = self.server_time(&mut pending)?;
        self.conn
            .set_selection_owner(self.win, self.atoms.CLIPBOARD, time)?;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?
            .owner;
        if owner == self.win {
            self.owned_since = Some(time);
            info!("claimed X11 CLIPBOARD (proxying Wayland selection)");
        } else {
            // Somebody beat us to it; XFIXES (M2) will tell us about them.
            self.owned_since = None;
            warn!("X11 CLIPBOARD claim lost the race (owner=0x{owner:x})");
        }
        self.conn.flush()?;
        Ok(pending)
    }

    pub fn release(&mut self) -> anyhow::Result<()> {
        if let Some(time) = self.owned_since.take() {
            self.conn
                .set_selection_owner(x11rb::NONE, self.atoms.CLIPBOARD, time)?;
            self.conn.flush()?;
            info!("released X11 CLIPBOARD");
        }
        Ok(())
    }

    /// Current CLIPBOARD owner window (0 = none). Startup probe (§4.1).
    pub fn selection_owner(&self) -> anyhow::Result<Window> {
        Ok(self
            .conn
            .get_selection_owner(self.atoms.CLIPBOARD)?
            .reply()?
            .owner)
    }

    /// Ask the current X11 owner for its TARGETS; the reply lands as a
    /// `SelectionNotify` in the main event loop.
    pub fn fetch_targets(&self) -> anyhow::Result<()> {
        self.conn.convert_selection(
            self.win,
            self.atoms.CLIPBOARD,
            self.atoms.TARGETS,
            self.atoms.CLIPFERRY_TARGETS,
            x11rb::CURRENT_TIME,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Read and delete the TARGETS reply property; returns the target atoms.
    pub fn read_targets_property(&self) -> anyhow::Result<Vec<x11rb::protocol::xproto::Atom>> {
        let reply = self
            .conn
            .get_property(
                true, // delete
                self.win,
                self.atoms.CLIPFERRY_TARGETS,
                AtomEnum::ATOM,
                0,
                u32::MAX / 4,
            )?
            .reply()
            .context("read TARGETS property")?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    /// M2: translate the owner's target atoms to the Wayland MIME types we
    /// can proxy (text only until M3).
    pub fn targets_to_mimes(&self, targets: &[x11rb::protocol::xproto::Atom]) -> Vec<String> {
        let text_atoms = [
            self.atoms.UTF8_STRING,
            self.atoms.TEXT,
            self.atoms.TEXT_PLAIN,
            self.atoms.TEXT_PLAIN_UTF8,
            x11rb::protocol::xproto::Atom::from(AtomEnum::STRING),
        ];
        if targets.iter().any(|t| text_atoms.contains(t)) {
            crate::mime::X2W_TEXT_MIMES
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        } else {
            Vec::new()
        }
    }
}
