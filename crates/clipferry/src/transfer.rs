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
}
