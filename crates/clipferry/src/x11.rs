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
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Timestamp, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::SelKind;

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
        CLIPFERRY_TARGETS_PRIMARY: b"_CLIPFERRY_TARGETS_PRIMARY",
    }
}

pub struct X11 {
    pub conn: Arc<RustConnection>,
    pub win: Window,
    pub atoms: Atoms,
    /// Server timestamp of our ownership per selection (§4.1); `None` when
    /// a real X11 app (or nobody) owns it. Indexed by `SelKind::idx`.
    pub owned_since: [Option<Timestamp>; 2],
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
                    info!("event=retry side=x11 error={:?}", e.to_string());
                    logged = true;
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

impl X11 {
    pub const fn selection_atom(&self, kind: SelKind) -> Atom {
        match kind {
            SelKind::Clipboard => self.atoms.CLIPBOARD,
            SelKind::Primary => 1, // AtomEnum::PRIMARY
        }
    }

    const fn targets_property(&self, kind: SelKind) -> Atom {
        match kind {
            SelKind::Clipboard => self.atoms.CLIPFERRY_TARGETS,
            SelKind::Primary => self.atoms.CLIPFERRY_TARGETS_PRIMARY,
        }
    }

    pub fn new(
        conn: Arc<RustConnection>,
        screen_num: usize,
        primary: bool,
    ) -> anyhow::Result<Self> {
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
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        conn.xfixes_select_selection_input(win, atoms.CLIPBOARD, mask)
            .context("subscribe to XFIXES selection events")?;
        if primary {
            conn.xfixes_select_selection_input(win, AtomEnum::PRIMARY.into(), mask)
                .context("subscribe to XFIXES PRIMARY events")?;
        }

        // Headroom under the max request size for the ChangeProperty header.
        let max_payload = conn.maximum_request_bytes().saturating_sub(1024);
        conn.flush()?;

        Ok(Self {
            conn,
            win,
            atoms,
            owned_since: [None, None],
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

    /// Claim `kind` as proxy for the Wayland owner. Returns any unrelated
    /// events soaked up while acquiring the timestamp.
    pub fn claim(&mut self, kind: SelKind) -> anyhow::Result<Vec<Event>> {
        let mut pending = Vec::new();
        let time = self.server_time(&mut pending)?;
        let selection = self.selection_atom(kind);
        self.conn.set_selection_owner(self.win, selection, time)?;
        let owner = self.conn.get_selection_owner(selection)?.reply()?.owner;
        if owner == self.win {
            self.owned_since[kind.idx()] = Some(time);
            info!(
                "event=claim side=x11 sel={} reason=proxy-wayland",
                kind.key()
            );
        } else {
            // Somebody beat us to it; XFIXES will tell us about them.
            self.owned_since[kind.idx()] = None;
            warn!(
                "event=claim_race side=x11 sel={} owner=0x{owner:x}",
                kind.key()
            );
        }
        self.conn.flush()?;
        Ok(pending)
    }

    pub fn release(&mut self, kind: SelKind) -> anyhow::Result<()> {
        if let Some(time) = self.owned_since[kind.idx()].take() {
            self.conn
                .set_selection_owner(x11rb::NONE, self.selection_atom(kind), time)?;
            self.conn.flush()?;
            info!("event=release side=x11 sel={}", kind.key());
        }
        Ok(())
    }

    /// Current owner window of `kind` (0 = none). Startup probe (§4.1).
    pub fn selection_owner(&self, kind: SelKind) -> anyhow::Result<Window> {
        Ok(self
            .conn
            .get_selection_owner(self.selection_atom(kind))?
            .reply()?
            .owner)
    }

    /// Ask the current X11 owner for its TARGETS; the reply lands as a
    /// `SelectionNotify` in the main event loop.
    pub fn fetch_targets(&self, kind: SelKind) -> anyhow::Result<()> {
        self.conn.convert_selection(
            self.win,
            self.selection_atom(kind),
            self.atoms.TARGETS,
            self.targets_property(kind),
            x11rb::CURRENT_TIME,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    /// Read and delete the TARGETS reply property; returns the target atoms.
    pub fn read_targets_property(
        &self,
        kind: SelKind,
    ) -> anyhow::Result<Vec<x11rb::protocol::xproto::Atom>> {
        let reply = self
            .conn
            .get_property(
                true, // delete
                self.win,
                self.targets_property(kind),
                AtomEnum::ATOM,
                0,
                u32::MAX / 4,
            )?
            .reply()
            .context("read TARGETS property")?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    /// The Xwayland window manager's check window (root
    /// `_NET_SUPPORTING_WM_CHECK`) — xwayland-satellite's own claims come
    /// from it. Telemetry (§10.1).
    pub fn wm_check_window(&self) -> Option<Window> {
        let atom = self
            .conn
            .intern_atom(false, b"_NET_SUPPORTING_WM_CHECK")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let root = self.conn.setup().roots.first()?.root;
        let reply = self
            .conn
            .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply
            .value32()
            .and_then(|mut v| v.next())
            .filter(|&w| w != x11rb::NONE)
    }

    /// Best-effort identification of an X11 client by window: `WM_CLASS`
    /// (instance\0class\0). Wayland offers no equivalent, and /proc is
    /// unreadable under our own Landlock — the X server is the only source.
    pub fn owner_class(&self, win: Window) -> Option<String> {
        let reply = self
            .conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            return None;
        }
        let parts: Vec<&[u8]> = reply.value.split(|&b| b == 0).collect();
        let class = parts.iter().rev().find(|p| !p.is_empty())?;
        Some(String::from_utf8_lossy(class).into_owned())
    }

    /// Resolve target atoms to their names; translation happens in
    /// `mime::x2w_translate` (§7).
    pub fn target_names(&self, targets: &[x11rb::protocol::xproto::Atom]) -> Vec<String> {
        let cookies: Vec<_> = targets
            .iter()
            .map(|&a| self.conn.get_atom_name(a))
            .collect();
        let mut names = Vec::new();
        for cookie in cookies {
            let Ok(cookie) = cookie else { continue };
            let Ok(reply) = cookie.reply() else { continue };
            names.push(String::from_utf8_lossy(&reply.name).into_owned());
        }
        names
    }

    /// Intern atoms for the MIME types we advertise as X11 targets when
    /// proxying W→X. Legacy text names are covered by the alias targets.
    pub fn intern_mimes(
        &self,
        mimes: &[String],
    ) -> anyhow::Result<std::collections::HashMap<x11rb::protocol::xproto::Atom, String>> {
        let cookies: Vec<_> = mimes
            .iter()
            .filter(|m| !matches!(m.as_str(), "UTF8_STRING" | "TEXT" | "STRING"))
            .map(|m| (m.clone(), self.conn.intern_atom(false, m.as_bytes())))
            .collect();
        let mut map = std::collections::HashMap::new();
        for (mime, cookie) in cookies {
            map.insert(cookie?.reply()?.atom, mime);
        }
        Ok(map)
    }
}
