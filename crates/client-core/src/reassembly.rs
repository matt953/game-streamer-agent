//! Datagram → frame reassembly (spec 04): frames complete out of nothing
//! but chunk arrival; anything stale is dropped, never waited on (video is
//! newest-wins).

use std::collections::HashMap;

use gsa_protocol::datagram::VideoDatagramHeader;

/// How many in-flight frame ids we track before pruning the oldest.
/// Loopback/LAN never sees more than 2-3 concurrent; the window guards
/// against pathological reordering and id-space garbage.
const MAX_PENDING: usize = 8;

#[derive(Debug)]
struct Pending {
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    epoch: u8,
}

#[derive(Debug, Default)]
pub struct Reassembler {
    pending: HashMap<u32, Pending>,
    /// Frames discarded incomplete (stats / future NACK trigger).
    dropped: u64,
    /// Highest frame id completed (drop-older policy).
    latest_completed: Option<u32>,
}

impl Reassembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one chunk; returns the full frame bytes when it completes.
    pub fn push(&mut self, header: VideoDatagramHeader, payload: &[u8]) -> Option<Vec<u8>> {
        // Frames older than one we already delivered are stale: drop.
        if let Some(latest) = self.latest_completed {
            let age = latest.wrapping_sub(header.frame_id);
            if age < u32::MAX / 2 && age > 0 {
                return None;
            }
        }

        let entry = self
            .pending
            .entry(header.frame_id)
            .or_insert_with(|| Pending {
                chunks: vec![None; header.chunk_count as usize],
                received: 0,
                epoch: header.session_epoch,
            });
        // Epoch bump (encoder reset) or inconsistent chunk_count: restart.
        if entry.epoch != header.session_epoch || entry.chunks.len() != header.chunk_count as usize
        {
            *entry = Pending {
                chunks: vec![None; header.chunk_count as usize],
                received: 0,
                epoch: header.session_epoch,
            };
        }
        let slot = &mut entry.chunks[header.chunk_index as usize];
        if slot.is_none() {
            *slot = Some(payload.to_vec());
            entry.received += 1;
        }

        if entry.received == header.chunk_count {
            let done = self
                .pending
                .remove(&header.frame_id)
                .expect("just inserted");
            self.latest_completed = Some(header.frame_id);
            self.prune_older_than(header.frame_id);
            let mut frame = Vec::with_capacity(
                done.chunks
                    .iter()
                    .map(|c| c.as_ref().map_or(0, Vec::len))
                    .sum(),
            );
            for c in done.chunks {
                frame.extend_from_slice(&c.expect("all chunks received"));
            }
            return Some(frame);
        }

        if self.pending.len() > MAX_PENDING {
            self.prune_oldest();
        }
        None
    }

    #[must_use]
    pub fn frames_dropped(&self) -> u64 {
        self.dropped
    }

    fn prune_older_than(&mut self, completed: u32) {
        let before = self.pending.len();
        self.pending.retain(|id, _| {
            let age = completed.wrapping_sub(*id);
            !(age < u32::MAX / 2 && age > 0)
        });
        self.dropped += (before - self.pending.len()) as u64;
    }

    fn prune_oldest(&mut self) {
        // Oldest = smallest id in wrap-order relative to the newest seen.
        if let Some(&newest) = self.pending.keys().max_by_key(|id| **id)
            && let Some(&oldest) = self
                .pending
                .keys()
                .max_by_key(|id| newest.wrapping_sub(**id))
        {
            self.pending.remove(&oldest);
            self.dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsa_core::media::FrameKind;

    fn hdr(frame_id: u32, index: u16, count: u16) -> VideoDatagramHeader {
        VideoDatagramHeader {
            seq: 0,
            session_epoch: 0,
            frame_id,
            kind: FrameKind::P,
            chunk_index: index,
            chunk_count: count,
            capture_ts_us: 0,
        }
    }

    #[test]
    fn single_chunk_completes_immediately() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(hdr(1, 0, 1), b"abc"), Some(b"abc".to_vec()));
    }

    #[test]
    fn multi_chunk_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(hdr(2, 1, 3), b"BB"), None);
        assert_eq!(r.push(hdr(2, 0, 3), b"AA"), None);
        assert_eq!(r.push(hdr(2, 2, 3), b"CC"), Some(b"AABBCC".to_vec()));
    }

    #[test]
    fn duplicate_chunks_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(hdr(3, 0, 2), b"xx"), None);
        assert_eq!(r.push(hdr(3, 0, 2), b"xx"), None);
        assert_eq!(r.push(hdr(3, 1, 2), b"yy"), Some(b"xxyy".to_vec()));
    }

    #[test]
    fn stale_frames_dropped_after_newer_completes() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(hdr(5, 0, 2), b"old"), None); // incomplete
        assert_eq!(r.push(hdr(6, 0, 1), b"new"), Some(b"new".to_vec()));
        assert_eq!(r.frames_dropped(), 1);
        // Late chunk of the stale frame is ignored.
        assert_eq!(r.push(hdr(5, 1, 2), b"old"), None);
    }

    #[test]
    fn window_pruning_bounds_memory() {
        let mut r = Reassembler::new();
        for id in 0..50u32 {
            let _ = r.push(hdr(id, 0, 2), b"partial"); // never completes
        }
        assert!(r.pending.len() <= MAX_PENDING + 1);
        assert!(r.frames_dropped() > 0);
    }
}
