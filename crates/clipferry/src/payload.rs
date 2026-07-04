//! Payload buffers upholding the §8.1 invariant.
//!
//! Clipboard bytes only ever live in fixed-size, zero-on-drop allocations
//! that are never reallocated — a growing `Vec` would return its old blocks
//! to the allocator un-zeroed.

use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::sync::Arc;

use zeroize::Zeroizing;

pub const CHUNK_SIZE: usize = 64 * 1024;

/// A rope of fixed-size zeroizing chunks. The outer `Vec` holds only
/// pointers/lengths (not secret) and may reallocate freely.
#[derive(Default)]
pub struct PayloadRope {
    chunks: Vec<Zeroizing<Vec<u8>>>,
    len: usize,
}

pub enum ReadOutcome {
    /// EOF reached within the cap.
    Complete(PayloadRope),
    /// The running total crossed `cap`. The rope holds what was read so
    /// far (at most cap + one chunk); more remains in the reader. Callers
    /// either continue streaming (INCR) or drop it (zeroed on drop).
    Overflow(PayloadRope),
}

impl PayloadRope {
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read `src` to EOF in `CHUNK_SIZE` steps. Each chunk is allocated once
    /// at full size, filled, then truncated — capacity never changes, so no
    /// payload byte ever transits a reallocation. `cap`: abort as soon as
    /// the running total crosses it.
    pub fn read_to_end(src: &mut impl Read, cap: Option<usize>) -> std::io::Result<ReadOutcome> {
        let mut rope = Self::default();
        loop {
            let mut chunk = Zeroizing::new(vec![0_u8; CHUNK_SIZE]);
            let mut filled = 0;
            while filled < CHUNK_SIZE {
                match src.read(&mut chunk[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if filled == 0 {
                break;
            }
            chunk.truncate(filled);
            rope.len += filled;
            rope.chunks.push(chunk);
            if cap.is_some_and(|c| rope.len > c) {
                return Ok(ReadOutcome::Overflow(rope));
            }
            if filled < CHUNK_SIZE {
                break;
            }
        }
        Ok(ReadOutcome::Complete(rope))
    }

    /// Iterate the chunks in order (for INCR-style chunked serving).
    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.chunks.iter().map(|c| c.as_slice())
    }

    /// The one sanctioned contiguous copy (§8.1): an X11 non-INCR
    /// `ChangeProperty` needs a single slice. Exact-capacity allocation —
    /// never grows.
    pub fn to_contiguous(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(self.len));
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }
}

/// An eager-mode snapshot (§4.2.1): one rope per advertised MIME type,
/// tagged with the claim epoch it belongs to. Held in memory only; replaced
/// on the next claim; ropes zero on drop.
pub struct Snapshot {
    pub epoch: u64,
    pub data: HashMap<String, PayloadRope>,
}

impl Snapshot {
    /// Best-effort §8.1 mlock of every payload chunk (keeps snapshots out
    /// of swap). Degrades silently past `RLIMIT_MEMLOCK`; core-dump
    /// exposure is separately closed by `PR_SET_DUMPABLE=0` (M5).
    #[allow(unsafe_code)] // sole unsafe in the crate; see SAFETY below
    pub fn lock_in_memory(&self) -> bool {
        let mut all_locked = true;
        for rope in self.data.values() {
            for chunk in &rope.chunks {
                // SAFETY: the chunk allocation is live for the duration of
                // the call; mlock does not move or mutate memory.
                let ok = unsafe {
                    rustix::mm::mlock(chunk.as_ptr().cast_mut().cast(), chunk.len()).is_ok()
                };
                if !ok {
                    all_locked = false;
                }
            }
        }
        all_locked
    }
}

/// `Read` over one MIME's rope inside a shared snapshot — lets eager-mode
/// serving reuse the exact same streaming code as lazy pipes.
pub struct SnapshotReader {
    snapshot: Arc<Snapshot>,
    mime: String,
    chunk: usize,
    offset: usize,
}

impl SnapshotReader {
    #[must_use]
    pub const fn new(snapshot: Arc<Snapshot>, mime: String) -> Self {
        Self {
            snapshot,
            mime,
            chunk: 0,
            offset: 0,
        }
    }
}

impl Read for SnapshotReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(rope) = self.snapshot.data.get(&self.mime) else {
            return Ok(0);
        };
        loop {
            let Some(chunk) = rope.chunks.get(self.chunk) else {
                return Ok(0);
            };
            if self.offset >= chunk.len() {
                self.chunk += 1;
                self.offset = 0;
                continue;
            }
            let n = (chunk.len() - self.offset).min(buf.len());
            buf[..n].copy_from_slice(&chunk[self.offset..self.offset + n]);
            self.offset += n;
            return Ok(n);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn read_all(data: &[u8], cap: Option<usize>) -> ReadOutcome {
        PayloadRope::read_to_end(&mut &data[..], cap).unwrap()
    }

    #[test]
    fn empty_input() {
        let ReadOutcome::Complete(rope) = read_all(&[], None) else {
            panic!("expected Complete");
        };
        assert!(rope.is_empty());
        assert_eq!(rope.to_contiguous().len(), 0);
    }

    #[test]
    fn sub_chunk_payload_round_trips() {
        let data = b"hello clipboard".repeat(10);
        let ReadOutcome::Complete(rope) = read_all(&data, None) else {
            panic!("expected Complete");
        };
        assert_eq!(rope.len(), data.len());
        assert_eq!(&*rope.to_contiguous(), &data[..]);
    }

    #[test]
    fn multi_chunk_payload_round_trips() {
        let data: Vec<u8> = (0..CHUNK_SIZE * 2 + 777)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let ReadOutcome::Complete(rope) = read_all(&data, None) else {
            panic!("expected Complete");
        };
        assert_eq!(rope.len(), data.len());
        assert_eq!(&*rope.to_contiguous(), &data[..]);
    }

    #[test]
    fn exact_chunk_boundary() {
        let data = vec![0xAB_u8; CHUNK_SIZE];
        let ReadOutcome::Complete(rope) = read_all(&data, None) else {
            panic!("expected Complete");
        };
        assert_eq!(rope.len(), CHUNK_SIZE);
        assert_eq!(&*rope.to_contiguous(), &data[..]);
    }

    #[test]
    fn cap_is_enforced_while_streaming() {
        let data = vec![1_u8; CHUNK_SIZE * 3];
        let ReadOutcome::Overflow(partial) = read_all(&data, Some(CHUNK_SIZE + 1)) else {
            panic!("expected Overflow");
        };
        assert!(partial.len() > CHUNK_SIZE && partial.len() <= CHUNK_SIZE * 2);
        // At exactly the cap the payload is allowed through.
        let data = vec![1_u8; CHUNK_SIZE];
        assert!(matches!(
            read_all(&data, Some(CHUNK_SIZE)),
            ReadOutcome::Complete(_)
        ));
    }
}
