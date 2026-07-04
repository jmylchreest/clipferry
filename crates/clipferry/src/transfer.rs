//! Lazy paste transfers (§4.2).
//!
//! Bytes move only when an X11 client actually pastes. One short-lived
//! thread per transfer (§4.4 decision) streams from the Wayland source's
//! pipe into the requestor's window property.
//!
//! M1 limit: payloads must fit one non-INCR `ChangeProperty`; INCR is M3.

use std::io::PipeReader;
use std::sync::Arc;

use anyhow::Context as _;
use log::{debug, warn};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    Atom, ConnectionExt as _, EventMask, PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
    SelectionRequestEvent,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use zeroize::Zeroizing;

use crate::payload::{PayloadRope, ReadOutcome};

pub struct PasteReply {
    pub conn: Arc<RustConnection>,
    pub req: SelectionRequestEvent,
    pub property: Atom,
    pub reply_type: Atom,
    /// The requestor asked for legacy `STRING`: convert UTF-8 → Latin-1.
    pub to_latin1: bool,
    pub max_payload: usize,
}

/// Answer a `SelectionRequest`, positively (`property = Some`) or as a
/// refusal (`property = None`). Best-effort: the requestor may already be gone.
pub fn notify(conn: &RustConnection, req: &SelectionRequestEvent, property: Option<Atom>) {
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: property.unwrap_or(x11rb::NONE),
    };
    let sent = conn
        .send_event(false, req.requestor, EventMask::NO_EVENT, event)
        .map(x11rb::cookie::VoidCookie::check);
    if !matches!(sent, Ok(Ok(()))) {
        debug!(
            "requestor 0x{:x} vanished before SelectionNotify",
            req.requestor
        );
    }
}

pub fn spawn_text_paste(reply: PasteReply, mut src: PipeReader) {
    std::thread::spawn(move || {
        if let Err(e) = serve(&reply, &mut src) {
            warn!("paste transfer failed: {e:#}");
            notify(&reply.conn, &reply.req, None);
            let _ = reply.conn.flush();
        }
    });
}

fn serve(reply: &PasteReply, src: &mut PipeReader) -> anyhow::Result<()> {
    match PayloadRope::read_to_end(src, Some(reply.max_payload))
        .context("read from Wayland source")?
    {
        ReadOutcome::CapExceeded => {
            warn!(
                "payload exceeds the single-request limit ({} bytes); INCR arrives in M3 — refusing",
                reply.max_payload
            );
            notify(&reply.conn, &reply.req, None);
        }
        ReadOutcome::Complete(rope) => {
            let data = rope.to_contiguous();
            let data = if reply.to_latin1 {
                if let Some(converted) = utf8_to_latin1(&data) {
                    converted
                } else {
                    warn!("source data is not valid UTF-8; refusing STRING conversion");
                    notify(&reply.conn, &reply.req, None);
                    reply.conn.flush().context("flush X11")?;
                    return Ok(());
                }
            } else {
                data
            };
            reply
                .conn
                .change_property8(
                    PropMode::REPLACE,
                    reply.req.requestor,
                    reply.property,
                    reply.reply_type,
                    &data,
                )
                .context("write requestor property")?;
            notify(&reply.conn, &reply.req, Some(reply.property));
            debug!("served X11 paste: {} bytes", data.len());
        }
    }
    reply.conn.flush().context("flush X11")?;
    Ok(())
}

// --- X→W: serve a Wayland paste from the X11 owner ------------------------

/// A Wayland client asked our data source for `mime`.
///
/// Runs on its own thread with a dedicated X11 connection so the blocking
/// `ConvertSelection` handshake never touches the event loop (§4.4).
pub fn spawn_x11_read(mime: String, fd: std::os::fd::OwnedFd) {
    std::thread::spawn(move || {
        if let Err(e) = serve_x2w(&mime, fd) {
            // EPIPE is routine: history managers and probes close their
            // read end early. Not worth alarming anyone.
            let broken_pipe = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe);
            if broken_pipe {
                debug!("X→W transfer: reader closed early (EPIPE)");
            } else {
                warn!("X→W transfer failed: {e:#}");
            }
        }
    });
}

/// How long we wait for the X11 owner to answer protocol handshakes
/// (`SelectionNotify`). This is not the payload timeout (§4.2 — that
/// defaults to infinite); a source that won't even acknowledge the
/// conversion is dead.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn serve_x2w(mime: &str, fd: std::os::fd::OwnedFd) -> anyhow::Result<()> {
    use crate::x11::Atoms;
    use x11rb::protocol::xproto::{AtomEnum, CreateWindowAux, WindowClass};

    let mut out = std::fs::File::from(fd);
    let (conn, screen_num) = RustConnection::connect(None).context("transfer X11 connection")?;
    let atoms = Atoms::new(&conn)
        .context("intern transfer atoms")?
        .reply()?;
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id()?;
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
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    // Prefer the UTF-8 target; fall back to legacy STRING (Latin-1).
    let want_utf8 = mime != "STRING";
    let candidates = [
        (atoms.UTF8_STRING, false),
        (Atom::from(AtomEnum::STRING), true),
    ];
    for (target, source_is_latin1) in candidates {
        conn.convert_selection(
            win,
            atoms.CLIPBOARD,
            target,
            atoms.CLIPFERRY,
            x11rb::CURRENT_TIME,
        )?;
        conn.flush()?;
        match wait_selection_notify(&conn, win)? {
            Some(property) if property != x11rb::NONE => {
                return stream_x11_property(
                    &conn,
                    win,
                    &atoms,
                    source_is_latin1 && want_utf8,
                    &mut out,
                );
            }
            // Owner refused this target — try the next candidate.
            _ => {}
        }
    }
    warn!("X11 owner offered no readable text target; serving empty paste");
    Ok(())
}

fn wait_selection_notify(conn: &RustConnection, win: u32) -> anyhow::Result<Option<Atom>> {
    use x11rb::protocol::Event;
    let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if let Some(event) = conn.poll_for_event()? {
            if let Event::SelectionNotify(e) = event
                && e.requestor == win
            {
                return Ok(Some(e.property));
            }
        } else if std::time::Instant::now() > deadline {
            anyhow::bail!("X11 owner did not answer ConvertSelection within {HANDSHAKE_TIMEOUT:?}");
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

fn stream_x11_property(
    conn: &RustConnection,
    win: u32,
    atoms: &crate::x11::Atoms,
    latin1_to_utf8_needed: bool,
    out: &mut std::fs::File,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    use x11rb::protocol::xproto::AtomEnum;

    let head = conn
        .get_property(false, win, atoms.CLIPFERRY, AtomEnum::ANY, 0, 0)?
        .reply()
        .context("probe property type")?;
    if head.type_ == atoms.INCR {
        conn.delete_property(win, atoms.CLIPFERRY)?;
        conn.flush()?;
        warn!("X11 source answers with INCR; support arrives in M3 — serving empty paste");
        return Ok(());
    }

    let mut offset_units = 0_u32;
    let mut total = 0_usize;
    loop {
        let reply = conn
            .get_property(
                false,
                win,
                atoms.CLIPFERRY,
                AtomEnum::ANY,
                offset_units,
                16_384,
            )?
            .reply()
            .context("read selection property")?;
        // Take ownership immediately so the payload zeroes on drop (§8.1).
        // (x11rb's internal receive buffers are outside our control; the
        // invariant covers every buffer clipferry itself manages.)
        let chunk = Zeroizing::new(reply.value);
        if !chunk.is_empty() {
            total += chunk.len();
            if latin1_to_utf8_needed {
                out.write_all(&latin1_to_utf8(&chunk))?;
            } else {
                out.write_all(&chunk)?;
            }
        }
        if reply.bytes_after == 0 {
            break;
        }
        offset_units += u32::try_from(chunk.len() / 4).unwrap_or(u32::MAX);
    }
    conn.delete_property(win, atoms.CLIPFERRY)?;
    conn.flush()?;
    debug!("served Wayland paste: {total} bytes");
    Ok(())
}

/// Latin-1 → UTF-8 (each byte is a code point; stateless, so chunk-safe).
/// Exact-capacity output (UTF-8 of Latin-1 is at most 2× the input) — §8.1.
fn latin1_to_utf8(data: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(data.len() * 2));
    for &b in data {
        if b < 0x80 {
            out.push(b);
        } else {
            out.push(0xC0 | (b >> 6));
            out.push(0x80 | (b & 0x3F));
        }
    }
    out
}

/// Lossy UTF-8 → Latin-1 (unmappable characters become '?'). Returns `None`
/// when the input is not UTF-8 at all. Output allocation is exact-capacity
/// (Latin-1 is never longer than the UTF-8 encoding) — §8.1: no growth.
fn utf8_to_latin1(data: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let text = std::str::from_utf8(data).ok()?;
    let mut out = Zeroizing::new(Vec::with_capacity(data.len()));
    for ch in text.chars() {
        out.push(u8::try_from(u32::from(ch)).unwrap_or(b'?'));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn latin1_passthrough_and_replacement() {
        let out = utf8_to_latin1("héllo € wörld".as_bytes()).unwrap();
        assert_eq!(&*out, b"h\xe9llo ? w\xf6rld");
    }

    #[test]
    fn latin1_rejects_invalid_utf8() {
        assert!(utf8_to_latin1(&[0xFF, 0xFE]).is_none());
    }

    #[test]
    fn latin1_to_utf8_round_trips_through_utf8_to_latin1() {
        let latin1: Vec<u8> = (1..=0xFF).collect();
        let utf8 = latin1_to_utf8(&latin1);
        assert!(std::str::from_utf8(&utf8).is_ok());
        let back = utf8_to_latin1(&utf8).unwrap();
        assert_eq!(&*back, &latin1[..]);
    }
}
