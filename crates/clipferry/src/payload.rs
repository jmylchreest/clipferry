//! Payload buffers upholding the §8.1 invariant.
//!
//! Clipboard bytes only ever live in fixed-size, zero-on-drop allocations
//! that are never reallocated — a growing `Vec` would return its old blocks
//! to the allocator un-zeroed.

use std::io::{ErrorKind, Read};

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
    Complete(PayloadRope),
    /// The running total crossed `cap`; whatever was buffered is dropped
    /// (and thereby zeroed) with this value.
    CapExceeded,
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
                return Ok(ReadOutcome::CapExceeded);
            }
            if filled < CHUNK_SIZE {
                break;
            }
        }
        Ok(ReadOutcome::Complete(rope))
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
        assert!(matches!(
            read_all(&data, Some(CHUNK_SIZE + 1)),
            ReadOutcome::CapExceeded
        ));
        // At exactly the cap the payload is allowed through.
        let data = vec![1_u8; CHUNK_SIZE];
        assert!(matches!(
            read_all(&data, Some(CHUNK_SIZE)),
            ReadOutcome::Complete(_)
        ));
    }
}
