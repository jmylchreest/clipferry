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

use crate::SelKind;
use crate::mime::Transform;
use crate::payload::{CHUNK_SIZE, PayloadRope, ReadOutcome, Snapshot, SnapshotReader};
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
        debug!("event=notify_failed requestor=0x{:x}", req.requestor);
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
    pub kind: crate::SelKind,
    /// The source MIME being served (logging metadata only).
    pub mime: String,
    pub req: SelectionRequestEvent,
    pub property: Atom,
    pub reply_type: Atom,
    pub conversion: Conv,
    /// §7 translation applied while serving (e.g. synthesize the GNOME
    /// copied-files header in front of a uri-list).
    pub transform: Transform,
    pub timeout: Option<Duration>,
}

pub fn spawn_wayland_read(reply: PasteReply, src: PipeReader) {
    std::thread::spawn(move || {
        let timeout = reply.timeout;
        let mut reader = IdleReader {
            inner: src,
            timeout,
        };
        run_w2x(&reply, &mut reader);
    });
}

/// Eager mode: serve the same request from the in-memory snapshot instead
/// of touching the (possibly gone) source app.
pub fn spawn_snapshot_serve(reply: PasteReply, snapshot: std::sync::Arc<Snapshot>, mime: String) {
    std::thread::spawn(move || {
        let mut reader = SnapshotReader::new(snapshot, mime.clone());
        run_w2x(&reply, &mut reader);
    });
}

fn run_w2x(reply: &PasteReply, reader: &mut impl Read) {
    let (conn, _) = match RustConnection::connect(None) {
        Ok(pair) => pair,
        Err(e) => {
            warn!("event=paste dir=w2x error={:?}", e.to_string());
            return;
        }
    };
    if let Err(e) = serve_w2x(&conn, reply, reader) {
        warn!(
            "event=paste dir=w2x sel={} mime={:?} error={:?}",
            reply.kind.key(),
            reply.mime,
            format!("{e:#}")
        );
        notify(&conn, &reply.req, None);
        let _ = conn.flush();
    }
}

fn serve_w2x(
    conn: &RustConnection,
    reply: &PasteReply,
    reader: &mut impl Read,
) -> anyhow::Result<()> {
    let atoms = Atoms::new(conn).context("intern transfer atoms")?.reply()?;
    let max_payload = conn.maximum_request_bytes().saturating_sub(1024);

    match PayloadRope::read_to_end(reader, Some(max_payload)).context("read from Wayland source")? {
        ReadOutcome::Complete(rope) => {
            let data = rope.to_contiguous();
            let data = match reply.conversion {
                Conv::None => data,
                Conv::Utf8ToLatin1 => {
                    if let Some(converted) = utf8_to_latin1(&data) {
                        converted
                    } else {
                        warn!(
                            "event=paste dir=w2x sel={} mime={:?} reason=invalid-utf8",
                            reply.kind.key(),
                            reply.mime
                        );
                        notify(conn, &reply.req, None);
                        conn.flush()?;
                        return Ok(());
                    }
                }
                Conv::Latin1ToUtf8 => latin1_to_utf8(&data),
            };
            let data = match reply.transform {
                Transform::None => data,
                Transform::PrependCopyHeader => {
                    let mut with_header = Zeroizing::new(Vec::with_capacity(data.len() + 5));
                    with_header.extend_from_slice(b"copy\n");
                    with_header.extend_from_slice(&data);
                    with_header
                }
                Transform::StripCopyHeader => {
                    let start = data
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(data.len(), |nl| nl + 1);
                    let mut stripped = Zeroizing::new(Vec::with_capacity(data.len() - start));
                    stripped.extend_from_slice(&data[start..]);
                    stripped
                }
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
            debug!(
                "event=paste dir=w2x sel={} mime={:?} bytes={} mode=single",
                reply.kind.key(),
                reply.mime,
                data.len()
            );
        }
        ReadOutcome::Overflow(head) => {
            if reply.conversion != Conv::None || reply.transform == Transform::StripCopyHeader {
                // Charset conversion and header stripping need the whole
                // payload; neither occurs on multi-megabyte content (text
                // into STRING clients, file lists) in practice.
                warn!(
                    "event=paste dir=w2x sel={} mime={:?} reason=too-large-for-transform",
                    reply.kind.key(),
                    reply.mime
                );
                notify(conn, &reply.req, None);
                conn.flush()?;
                return Ok(());
            }
            serve_w2x_incr(conn, &atoms, reply, &head, reader)?;
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
    if reply.transform == Transform::PrependCopyHeader {
        write_incr_chunk(conn, reply, b"copy\n")?;
        total += 5;
    }
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
    debug!(
        "event=paste dir=w2x sel={} mime={:?} bytes={total} mode=incr",
        reply.kind.key(),
        reply.mime
    );
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

/// One X→W read: what to fetch from the X11 owner and how to deliver it.
pub struct X2wRequest {
    pub mime: String,
    /// §7 read plan override: (x11 target name, transform). Default: text
    /// candidates for plain text, else the MIME name as the exact target.
    pub plan: Option<(String, Transform)>,
    pub kind: SelKind,
    pub fd: std::os::fd::OwnedFd,
    pub timeout: Option<Duration>,
}

/// X→W reads are serialized: many X11 owners (xclip, older toolkits) are
/// single-threaded, and a second `ConvertSelection` while an INCR dance is in
/// flight can wedge them. History managers make concurrent reads the norm.
static X2W_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Eager X→W: serve a Wayland paste straight from the snapshot.
pub fn spawn_rope_to_fd(
    snapshot: std::sync::Arc<Snapshot>,
    mime: String,
    fd: std::os::fd::OwnedFd,
) {
    std::thread::spawn(move || {
        use std::io::Write as _;
        let mut out = std::fs::File::from(fd);
        let mut reader = SnapshotReader::new(snapshot, mime.clone());
        let mut buf = Zeroizing::new(vec![0_u8; CHUNK_SIZE]);
        while let Ok(n) = std::io::Read::read(&mut reader, &mut buf) {
            if n == 0 {
                break;
            }
            if out.write_all(&buf[..n]).is_err() {
                debug!("event=paste dir=x2w src=snapshot mime={mime:?} result=reader-closed");
                break;
            }
        }
    });
}

/// A Wayland client asked our data source for `request.mime`.
pub fn spawn_x11_read(request: X2wRequest) {
    std::thread::spawn(move || {
        let sel = request.kind.key();
        let mime = request.mime.clone();
        if let Err(e) = serve_x2w(request) {
            // EPIPE is routine: history managers close their read end early.
            let broken_pipe = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe);
            if broken_pipe {
                debug!("event=paste dir=x2w sel={sel} mime={mime:?} result=reader-closed");
            } else {
                warn!(
                    "event=paste dir=x2w sel={sel} mime={mime:?} error={:?}",
                    format!("{e:#}")
                );
            }
        }
    });
}

fn serve_x2w(request: X2wRequest) -> anyhow::Result<()> {
    let X2wRequest {
        mime,
        plan,
        kind,
        fd,
        timeout,
    } = request;
    let mime = mime.as_str();
    let _turn = X2W_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    // §7 read plans: an explicit plan wins; text MIMEs try UTF8_STRING then
    // STRING; anything else converts the exact atom named like the MIME
    // (pass-through — clipferry is a pipe).
    let candidates: Vec<(Atom, Conv, Transform)> = if let Some((target_name, transform)) = plan {
        let atom = conn
            .intern_atom(false, target_name.as_bytes())?
            .reply()
            .context("intern plan target atom")?
            .atom;
        vec![(atom, Conv::None, transform)]
    } else if crate::mime::is_plain_text(mime) {
        let want_utf8 = mime != "STRING";
        vec![
            (atoms.UTF8_STRING, Conv::None, Transform::None),
            (
                Atom::from(AtomEnum::STRING),
                if want_utf8 {
                    Conv::Latin1ToUtf8
                } else {
                    Conv::None
                },
                Transform::None,
            ),
        ]
    } else {
        let atom = conn
            .intern_atom(false, mime.as_bytes())?
            .reply()
            .context("intern MIME atom")?
            .atom;
        vec![(atom, Conv::None, Transform::None)]
    };

    let selection = match kind {
        SelKind::Clipboard => atoms.CLIPBOARD,
        SelKind::Primary => Atom::from(AtomEnum::PRIMARY),
    };
    // Watch the owner window: if it dies mid-transfer we must abort rather
    // than wait forever for INCR chunks — a hung reader would hold the
    // X→W gate and kill the whole direction.
    let owner = conn.get_selection_owner(selection)?.reply()?.owner;
    if owner != x11rb::NONE {
        // Best-effort: the owner may already be gone.
        let _ = conn
            .change_window_attributes(
                owner,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map(x11rb::cookie::VoidCookie::check);
    }
    for (target, conversion, transform) in candidates {
        conn.convert_selection(win, selection, target, atoms.CLIPFERRY, x11rb::CURRENT_TIME)?;
        conn.flush()?;
        match wait_selection_notify(&conn, win, owner)? {
            Some(property) if property != x11rb::NONE => {
                let mut sink = Sink {
                    out: &mut out,
                    conversion,
                    strip_pending: transform == Transform::StripCopyHeader,
                    total: 0,
                    sel: kind.key(),
                    mime,
                };
                return stream_x11_property(&conn, win, owner, &atoms, timeout, &mut sink);
            }
            _ => {} // owner refused this target — try the next candidate
        }
    }
    warn!(
        "event=paste dir=x2w sel={} mime={mime:?} reason=no-target",
        kind.key()
    );
    Ok(())
}

fn wait_selection_notify(
    conn: &RustConnection,
    win: Window,
    owner: Window,
) -> anyhow::Result<Option<Atom>> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if let Some(event) = conn.poll_for_event()? {
            match event {
                Event::SelectionNotify(e) if e.requestor == win => {
                    return Ok(Some(e.property));
                }
                Event::DestroyNotify(e) if e.window == owner && owner != x11rb::NONE => {
                    bail!("X11 owner window destroyed before answering");
                }
                _ => {}
            }
        } else if Instant::now() > deadline {
            bail!("X11 owner did not answer ConvertSelection within {HANDSHAKE_TIMEOUT:?}");
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Streaming sink for X→W transfers: applies the §7 head transform and any
/// charset conversion, tracks totals.
struct Sink<'a> {
    out: &'a mut std::fs::File,
    conversion: Conv,
    /// `StripCopyHeader`: the leading action line has not been consumed yet.
    strip_pending: bool,
    total: usize,
    /// Logging metadata.
    sel: &'static str,
    mime: &'a str,
}

impl Sink<'_> {
    fn write(&mut self, chunk: &[u8]) -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut data = chunk;
        if self.strip_pending {
            match data.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    data = &data[nl + 1..];
                    self.strip_pending = false;
                }
                None => return Ok(()), // header longer than the chunk; keep skipping
            }
        }
        if data.is_empty() {
            return Ok(());
        }
        match self.conversion {
            // Latin-1 → UTF-8 is stateless per byte, so chunk-safe.
            Conv::Latin1ToUtf8 => self.out.write_all(&latin1_to_utf8(data))?,
            _ => self.out.write_all(data)?,
        }
        self.total += data.len();
        Ok(())
    }
}

fn stream_x11_property(
    conn: &RustConnection,
    win: Window,
    owner: Window,
    atoms: &Atoms,
    timeout: Option<Duration>,
    sink: &mut Sink<'_>,
) -> anyhow::Result<()> {
    let head = conn
        .get_property(false, win, atoms.CLIPFERRY, AtomEnum::ANY, 0, 0)?
        .reply()
        .context("probe property type")?;
    if head.type_ == atoms.INCR {
        return stream_x11_incr(conn, win, owner, atoms, timeout, sink);
    }

    let mut offset_units = 0_u32;
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
        sink.write(&chunk)?;
        if reply.bytes_after == 0 {
            break;
        }
        offset_units += u32::try_from(chunk.len() / 4).unwrap_or(u32::MAX);
    }
    conn.delete_property(win, atoms.CLIPFERRY)?;
    conn.flush()?;
    debug!(
        "event=paste dir=x2w sel={} mime={:?} bytes={} mode=single",
        sink.sel, sink.mime, sink.total
    );
    Ok(())
}

/// The INCR protocol, reading side (§6): ack by deleting the INCR property,
/// then read+delete each chunk as the owner posts it; empty chunk ends.
fn stream_x11_incr(
    conn: &RustConnection,
    win: Window,
    owner: Window,
    atoms: &Atoms,
    timeout: Option<Duration>,
    sink: &mut Sink<'_>,
) -> anyhow::Result<()> {
    conn.delete_property(win, atoms.CLIPFERRY)?;
    conn.flush()?;
    let mut writer_gone: Option<std::io::Error> = None;
    loop {
        wait_property_new_value(conn, win, owner, atoms.CLIPFERRY, timeout)?;
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
            match sink.write(&chunk) {
                Ok(()) => {}
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
    debug!(
        "event=paste dir=x2w sel={} mime={:?} bytes={} mode=incr",
        sink.sel, sink.mime, sink.total
    );
    Ok(())
}

fn wait_property_new_value(
    conn: &RustConnection,
    win: Window,
    owner: Window,
    prop: Atom,
    timeout: Option<Duration>,
) -> anyhow::Result<()> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if let Some(event) = conn.poll_for_event()? {
            match event {
                Event::PropertyNotify(e)
                    if e.window == win && e.atom == prop && e.state == Property::NEW_VALUE =>
                {
                    return Ok(());
                }
                Event::DestroyNotify(e) if e.window == owner && owner != x11rb::NONE => {
                    bail!("X11 owner window destroyed mid-INCR transfer");
                }
                _ => {}
            }
        } else if deadline.is_some_and(|d| Instant::now() > d) {
            bail!("transfer idle timeout waiting for INCR chunk");
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
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
