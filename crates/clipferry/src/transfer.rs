//! Lazy paste transfers (§4.2).
//!
//! Bytes move only when a client actually pastes. One short-lived thread per
//! transfer (§4.4), each with its **own X11 connection** so blocking
//! handshakes and INCR dances never touch the event loop. Payloads larger
//! than one X11 request are chunked via INCR in both directions (§6).

use std::io::{PipeReader, Read};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use log::{debug, warn};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use zeroize::Zeroizing;

use crate::payload::{CHUNK_SIZE, PayloadRope, ReadOutcome};
use crate::x11::Atoms;

/// How long we wait for the peer to answer pure protocol handshakes
/// (`SelectionNotify` acks). Distinct from the payload idle timeout (§4.2,
/// default infinite): a peer that won't even acknowledge is dead.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Text conversion applied while serving.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Conv {
    None,
    /// Requestor asked for legacy `STRING`.
    Utf8ToLatin1,
    /// UTF-8 MIME requested but the X11 owner only has `STRING`.
    Latin1ToUtf8,
}

/// Answer a `SelectionRequest`, positively (`property = Some`) or as a
/// refusal (`property = None`). Best-effort: the requestor may be gone.
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

/// A pipe reader that enforces the §4.2 *idle* timeout when one is set:
/// each read may wait at most `timeout` for data; any progress resets it.
struct IdleReader {
    inner: PipeReader,
    timeout: Option<Duration>,
}

impl Read for IdleReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(timeout) = self.timeout {
            let mut fds = [rustix::event::PollFd::new(
                &self.inner,
                rustix::event::PollFlags::IN,
            )];
            let spec = rustix::event::Timespec {
                tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(timeout.subsec_nanos()),
            };
            match rustix::event::poll(&mut fds, Some(&spec)) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "transfer idle timeout",
                    ));
                }
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.inner.read(buf)
    }
}

// --- W→X: serve an X11 paste from the Wayland owner ------------------------

pub struct PasteReply {
    pub req: SelectionRequestEvent,
    pub property: Atom,
    pub reply_type: Atom,
    pub conversion: Conv,
    pub timeout: Option<Duration>,
}

pub fn spawn_wayland_read(reply: PasteReply, src: PipeReader) {
    std::thread::spawn(move || {
        let (conn, _) = match RustConnection::connect(None) {
            Ok(pair) => pair,
            Err(e) => {
                warn!("W→X transfer: no X11 connection: {e}");
                return;
            }
        };
        if let Err(e) = serve_w2x(&conn, &reply, src) {
            warn!("W→X transfer failed: {e:#}");
            notify(&conn, &reply.req, None);
            let _ = conn.flush();
        }
    });
}

fn serve_w2x(conn: &RustConnection, reply: &PasteReply, src: PipeReader) -> anyhow::Result<()> {
    let atoms = Atoms::new(conn).context("intern transfer atoms")?.reply()?;
    let max_payload = conn.maximum_request_bytes().saturating_sub(1024);
    let mut reader = IdleReader {
        inner: src,
        timeout: reply.timeout,
    };

    match PayloadRope::read_to_end(&mut reader, Some(max_payload))
        .context("read from Wayland source")?
    {
        ReadOutcome::Complete(rope) => {
            let data = rope.to_contiguous();
            let data = match reply.conversion {
                Conv::None => data,
                Conv::Utf8ToLatin1 => {
                    if let Some(converted) = utf8_to_latin1(&data) {
                        converted
                    } else {
                        warn!("source data is not valid UTF-8; refusing STRING conversion");
                        notify(conn, &reply.req, None);
                        conn.flush()?;
                        return Ok(());
                    }
                }
                Conv::Latin1ToUtf8 => latin1_to_utf8(&data),
            };
            conn.change_property8(
                PropMode::REPLACE,
                reply.req.requestor,
                reply.property,
                reply.reply_type,
                &data,
            )
            .context("write requestor property")?;
            notify(conn, &reply.req, Some(reply.property));
            conn.flush()?;
            debug!("served X11 paste: {} bytes", data.len());
        }
        ReadOutcome::Overflow(head) => {
            if reply.conversion != Conv::None {
                // Charset conversion needs the whole payload; nobody pastes
                // multi-megabyte content into a STRING-only client.
                warn!("payload too large for charset conversion; refusing");
                notify(conn, &reply.req, None);
                conn.flush()?;
                return Ok(());
            }
            serve_w2x_incr(conn, &atoms, reply, &head, &mut reader)?;
        }
    }
    Ok(())
}

/// The INCR protocol, serving side (§6): announce with an INCR property,
/// then write chunks each time the requestor deletes the previous one.
fn serve_w2x_incr(
    conn: &RustConnection,
    atoms: &Atoms,
    reply: &PasteReply,
    head: &PayloadRope,
    reader: &mut impl Read,
) -> anyhow::Result<()> {
    let requestor = reply.req.requestor;
    conn.change_window_attributes(
        requestor,
        &ChangeWindowAttributesAux::new()
            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
    )?
    .check()
    .context("subscribe to requestor property events")?;

    // Announce: property of type INCR holding a size lower bound.
    let lower_bound = u32::try_from(head.len()).unwrap_or(u32::MAX);
    conn.change_property32(
        PropMode::REPLACE,
        requestor,
        reply.property,
        atoms.INCR,
        &[lower_bound],
    )?;
    notify(conn, &reply.req, Some(reply.property));
    conn.flush()?;
    wait_property_delete(conn, requestor, reply.property, reply.timeout)
        .context("INCR: requestor did not accept the transfer")?;

    let mut total = 0_usize;
    // First the buffered head, then stream the rest of the pipe.
    for chunk in head.chunks() {
        total += chunk.len();
        write_incr_chunk(conn, reply, chunk)?;
    }
    loop {
        let mut chunk = Zeroizing::new(vec![0_u8; CHUNK_SIZE]);
        let mut filled = 0;
        while filled < CHUNK_SIZE {
            match reader.read(&mut chunk[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("read from Wayland source (INCR)"),
            }
        }
        if filled == 0 {
            break;
        }
        chunk.truncate(filled);
        total += filled;
        write_incr_chunk(conn, reply, &chunk)?;
        if filled < CHUNK_SIZE {
            break;
        }
    }
    // Zero-length chunk terminates the INCR transfer.
    conn.change_property8(
        PropMode::REPLACE,
        requestor,
        reply.property,
        reply.reply_type,
        &[],
    )?;
    conn.flush()?;
    debug!("served X11 paste via INCR: {total} bytes");
    Ok(())
}

fn write_incr_chunk(conn: &RustConnection, reply: &PasteReply, chunk: &[u8]) -> anyhow::Result<()> {
    conn.change_property8(
        PropMode::REPLACE,
        reply.req.requestor,
        reply.property,
        reply.reply_type,
        chunk,
    )?;
    conn.flush()?;
    wait_property_delete(conn, reply.req.requestor, reply.property, reply.timeout)
        .context("INCR: requestor stopped consuming chunks")
}

/// Wait until `win` deletes `prop` (INCR flow control). A destroyed
/// requestor window aborts; the §4.2 idle timeout applies per wait when set.
fn wait_property_delete(
    conn: &RustConnection,
    win: Window,
    prop: Atom,
    timeout: Option<Duration>,
) -> anyhow::Result<()> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if let Some(event) = conn.poll_for_event()? {
            match event {
                Event::PropertyNotify(e)
                    if e.window == win && e.atom == prop && e.state == Property::DELETE =>
                {
                    return Ok(());
                }
                Event::DestroyNotify(e) if e.window == win => {
                    bail!("requestor window destroyed mid-transfer");
                }
                _ => {}
            }
        } else if deadline.is_some_and(|d| Instant::now() > d) {
            bail!("transfer idle timeout");
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

// --- X→W: serve a Wayland paste from the X11 owner -------------------------

/// A Wayland client asked our data source for `mime`.
pub fn spawn_x11_read(mime: String, fd: std::os::fd::OwnedFd, timeout: Option<Duration>) {
    std::thread::spawn(move || {
        if let Err(e) = serve_x2w(&mime, fd, timeout) {
            // EPIPE is routine: history managers close their read end early.
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

fn serve_x2w(
    mime: &str,
    fd: std::os::fd::OwnedFd,
    timeout: Option<Duration>,
) -> anyhow::Result<()> {
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

    // Text MIMEs try UTF8_STRING then STRING; anything else converts the
    // exact atom named like the MIME (§7: pass-through, clipferry is a pipe).
    let candidates: Vec<(Atom, Conv)> = if crate::mime::is_plain_text(mime) {
        let want_utf8 = mime != "STRING";
        vec![
            (atoms.UTF8_STRING, Conv::None),
            (
                Atom::from(AtomEnum::STRING),
                if want_utf8 {
                    Conv::Latin1ToUtf8
                } else {
                    Conv::None
                },
            ),
        ]
    } else {
        let atom = conn
            .intern_atom(false, mime.as_bytes())?
            .reply()
            .context("intern MIME atom")?
            .atom;
        vec![(atom, Conv::None)]
    };

    for (target, conversion) in candidates {
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
                return stream_x11_property(&conn, win, &atoms, conversion, timeout, &mut out);
            }
            _ => {} // owner refused this target — try the next candidate
        }
    }
    warn!("X11 owner refused every candidate target for {mime:?}; serving empty paste");
    Ok(())
}

fn wait_selection_notify(conn: &RustConnection, win: Window) -> anyhow::Result<Option<Atom>> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if let Some(event) = conn.poll_for_event()? {
            if let Event::SelectionNotify(e) = event
                && e.requestor == win
            {
                return Ok(Some(e.property));
            }
        } else if Instant::now() > deadline {
            bail!("X11 owner did not answer ConvertSelection within {HANDSHAKE_TIMEOUT:?}");
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn stream_x11_property(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    conversion: Conv,
    timeout: Option<Duration>,
    out: &mut std::fs::File,
) -> anyhow::Result<()> {
    let head = conn
        .get_property(false, win, atoms.CLIPFERRY, AtomEnum::ANY, 0, 0)?
        .reply()
        .context("probe property type")?;
    if head.type_ == atoms.INCR {
        return stream_x11_incr(conn, win, atoms, conversion, timeout, out);
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
        let chunk = Zeroizing::new(reply.value);
        total += write_converted(out, &chunk, conversion)?;
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

/// The INCR protocol, reading side (§6): ack by deleting the INCR property,
/// then read+delete each chunk as the owner posts it; empty chunk ends.
fn stream_x11_incr(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    conversion: Conv,
    timeout: Option<Duration>,
    out: &mut std::fs::File,
) -> anyhow::Result<()> {
    conn.delete_property(win, atoms.CLIPFERRY)?;
    conn.flush()?;
    let mut total = 0_usize;
    let mut writer_gone: Option<std::io::Error> = None;
    loop {
        wait_property_new_value(conn, win, atoms.CLIPFERRY, timeout)?;
        let reply = conn
            .get_property(true, win, atoms.CLIPFERRY, AtomEnum::ANY, 0, u32::MAX / 4)?
            .reply()
            .context("read INCR chunk")?;
        conn.flush()?;
        let chunk = Zeroizing::new(reply.value);
        if chunk.is_empty() {
            break;
        }
        if writer_gone.is_none() {
            // Keep draining after EPIPE: aborting mid-dance leaves a
            // single-threaded owner wedged for every later reader.
            match write_converted(out, &chunk, conversion) {
                Ok(n) => total += n,
                Err(e) => match e.downcast::<std::io::Error>() {
                    Ok(io_err) if io_err.kind() == std::io::ErrorKind::BrokenPipe => {
                        writer_gone = Some(io_err);
                    }
                    Ok(io_err) => return Err(io_err.into()),
                    Err(other) => return Err(other),
                },
            }
        }
    }
    if let Some(io_err) = writer_gone {
        return Err(io_err.into());
    }
    debug!("served Wayland paste via INCR: {total} bytes");
    Ok(())
}

fn wait_property_new_value(
    conn: &RustConnection,
    win: Window,
    prop: Atom,
    timeout: Option<Duration>,
) -> anyhow::Result<()> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if let Some(event) = conn.poll_for_event()? {
            if let Event::PropertyNotify(e) = event
                && e.window == win
                && e.atom == prop
                && e.state == Property::NEW_VALUE
            {
                return Ok(());
            }
        } else if deadline.is_some_and(|d| Instant::now() > d) {
            bail!("transfer idle timeout waiting for INCR chunk");
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn write_converted(
    out: &mut std::fs::File,
    chunk: &[u8],
    conversion: Conv,
) -> anyhow::Result<usize> {
    use std::io::Write as _;
    if chunk.is_empty() {
        return Ok(0);
    }
    match conversion {
        // Latin-1 → UTF-8 is stateless per byte, so chunk-safe.
        Conv::Latin1ToUtf8 => out.write_all(&latin1_to_utf8(chunk))?,
        _ => out.write_all(chunk)?,
    }
    Ok(chunk.len())
}

// --- Text conversions -------------------------------------------------------

/// Latin-1 → UTF-8 (each byte is a code point; stateless, so chunk-safe).
/// Exact-capacity output (at most 2× the input) — §8.1: no growth.
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
/// when the input is not UTF-8 at all. Exact-capacity output — §8.1.
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

    #[test]
    fn idle_reader_times_out_without_data() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let mut r = IdleReader {
            inner: reader,
            timeout: Some(Duration::from_millis(50)),
        };
        let mut buf = [0_u8; 8];
        let err = r.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn idle_reader_passes_data_through() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        std::io::Write::write_all(&mut writer, b"ping").unwrap();
        drop(writer);
        let mut r = IdleReader {
            inner: reader,
            timeout: Some(Duration::from_secs(5)),
        };
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"ping");
    }
}
