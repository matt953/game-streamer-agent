//! Turning video datagrams back into frames.
//!
//! A frame is split into fixed-size shards, grouped into one or more FEC
//! blocks, and each block carries Reed-Solomon parity so a block survives
//! losing some of its shards. Everything needed to place a shard —  which
//! frame, which block, which position, and whether it is data or parity — is
//! in its header, so the stream can be treated as unordered.
//!
//! Two details are easy to get wrong and are load-bearing here:
//!
//! - **Parity covers only the payload columns.** The sender overwrites parts
//!   of a parity shard's header *after* computing parity, so those byte
//!   columns are not recoverable. Recovery therefore runs over the payload
//!   region alone, which is also cheaper.
//! - **The parity count is not transmitted.** It is re-derived from the FEC
//!   percentage, and a shard index beyond that derivation is treated as
//!   evidence the derivation was low rather than as a bad packet.

use gsa_core::{Error, Result};

/// Bytes of framing before the payload: the RTP-like header plus the vendor
/// header. Parity protects everything from here on.
const PAYLOAD_OFFSET: usize = 32;

/// Per-frame prefix carried at the very start of a frame's first shard.
const FRAME_HEADER_LEN: usize = 8;

/// Hosts send at least this many parity shards per block regardless of the
/// percentage; it is what clients ask for during negotiation.
const MIN_PARITY_SHARDS: usize = 2;

/// Frames older than this many behind the newest are given up on, so a lost
/// shard cannot pin memory forever.
const REORDER_DEPTH: u32 = 4;

/// One shard's placement, read from its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardHeader {
    pub sequence: u16,
    /// Host-side stream clock, 90 kHz. Its absolute value has no fixed
    /// relation to our clock, but differences between frames are real
    /// host-side timing — which is what a de-jitter window needs.
    pub timestamp: u32,
    pub frame_index: u32,
    pub flags: u8,
    pub block_index: u8,
    pub last_block_index: u8,
    pub shard_index: u16,
    pub data_shards: u16,
    pub fec_percentage: u8,
}

impl ShardHeader {
    /// Data shards come first in a block; anything past them is parity.
    #[must_use]
    pub fn is_parity(&self) -> bool {
        self.shard_index >= self.data_shards
    }
}

/// Read a shard header, or `None` if the datagram is too small to hold one.
#[must_use]
pub fn parse_header(datagram: &[u8]) -> Option<ShardHeader> {
    if datagram.len() <= PAYLOAD_OFFSET {
        return None;
    }
    let le32 = |o: usize| {
        u32::from_le_bytes([
            datagram[o],
            datagram[o + 1],
            datagram[o + 2],
            datagram[o + 3],
        ])
    };
    let fec = le32(28);
    let blocks = datagram[27];
    Some(ShardHeader {
        sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
        timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
        frame_index: le32(20),
        flags: datagram[24],
        block_index: (blocks >> 4) & 0x3,
        last_block_index: (blocks >> 6) & 0x3,
        shard_index: ((fec >> 12) & 0x3ff) as u16,
        data_shards: ((fec >> 22) & 0x3ff) as u16,
        fec_percentage: ((fec >> 4) & 0xff) as u8,
    })
}

/// How many parity shards a block has.
///
/// The wire carries the percentage, not the count, so this re-derives the
/// sender's arithmetic. `observed_max_index` lets a shard we actually
/// received correct an under-estimate — trusting the formula over the
/// evidence would drop recoverable blocks.
fn parity_shards(data_shards: usize, percentage: u8, observed_max_index: usize) -> usize {
    let from_percentage = data_shards
        .saturating_mul(usize::from(percentage))
        .div_ceil(100);
    let derived = from_percentage
        .max(MIN_PARITY_SHARDS)
        .min(255usize.saturating_sub(data_shards));
    let from_evidence = (observed_max_index + 1).saturating_sub(data_shards);
    derived.max(from_evidence)
}

/// A frame recovered from the wire.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub frame_index: u32,
    /// This frame resets the reference chain.
    pub keyframe: bool,
    /// Annex-B access unit, parameter sets included on a keyframe.
    pub data: Vec<u8>,
    /// The host's own encode-latency estimate, in µs, when it reported one.
    pub host_latency_us: Option<u32>,
    /// Some of this frame was rebuilt from parity — loss that cost nothing
    /// visible. Worth counting separately from loss that did.
    pub recovered: bool,
    /// The host's 90 kHz stream clock for this frame, if any shard carried
    /// one. Relative timing only — see [`ShardHeader::timestamp`].
    pub timestamp: u32,
}

/// Why a frame did not survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLoss {
    /// Too many shards lost for parity to rebuild the frame.
    Unrecoverable { frame_index: u32 },
    /// The host moved on before we saw anything of this frame.
    Skipped { frame_index: u32 },
}

/// What one datagram produced.
#[derive(Debug)]
pub enum Received {
    Nothing,
    Frame(VideoFrame),
    Lost(FrameLoss),
}

#[derive(Debug)]
struct Block {
    data_shards: usize,
    fec_percentage: u8,
    /// Payload region of each shard we hold, by shard index.
    shards: std::collections::BTreeMap<u16, Vec<u8>>,
}

impl Block {
    fn complete(&self) -> bool {
        (0..self.data_shards as u16).all(|i| self.shards.contains_key(&i))
    }

    fn recoverable(&self) -> bool {
        self.shards.len() >= self.data_shards
    }

    /// Data-shard payloads in order, rebuilding any that were lost.
    fn resolve(&self) -> Result<Vec<Vec<u8>>> {
        if self.complete() {
            return Ok((0..self.data_shards as u16)
                .map(|i| self.shards[&i].clone())
                .collect());
        }
        let observed_max = self.shards.keys().copied().max().unwrap_or(0) as usize;
        let parity = parity_shards(self.data_shards, self.fec_percentage, observed_max);
        let total = self.data_shards + parity;
        // Every present shard must be the same length for recovery to work;
        // a host that stops padding would otherwise corrupt silently.
        let width = self
            .shards
            .values()
            .map(Vec::len)
            .max()
            .ok_or_else(|| Error::Session("empty FEC block".into()))?;
        let mut shards: Vec<Option<Vec<u8>>> = (0..total)
            .map(|i| {
                self.shards.get(&(i as u16)).map(|s| {
                    let mut s = s.clone();
                    s.resize(width, 0);
                    s
                })
            })
            .collect();
        let rs = fec_rs::ReedSolomon::new(self.data_shards, parity).map_err(|e| {
            Error::Session(format!("FEC setup ({}+{parity}): {e}", self.data_shards))
        })?;
        rs.reconstruct_data(&mut shards)
            .map_err(|e| Error::Session(format!("FEC recovery: {e}")))?;
        shards
            .into_iter()
            .take(self.data_shards)
            .map(|s| s.ok_or_else(|| Error::Session("FEC left a hole".into())))
            .collect()
    }
}

#[derive(Debug, Default)]
struct Assembly {
    blocks: std::collections::BTreeMap<u8, Block>,
    last_block_index: u8,
    /// Taken from the first shard seen; every shard of a frame carries it.
    timestamp: u32,
    /// True once any shard of this frame arrived with the start flag.
    seen_start: bool,
}

/// Reassembles frames from shards.
///
/// Feed datagrams with [`Depacketizer::push`] and drain results with
/// [`Depacketizer::next_event`]: one datagram can both complete a frame and
/// reveal that an older one will never arrive, so results are queued rather
/// than returned one per call.
#[derive(Debug, Default)]
pub struct Depacketizer {
    frames: std::collections::BTreeMap<u32, Assembly>,
    highest_frame: u32,
    /// Frames already emitted or written off, so late shards cannot revive
    /// them and produce a duplicate.
    finished_below: u32,
    events: std::collections::VecDeque<Received>,
}

impl Depacketizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next completed frame or loss, if any.
    pub fn next_event(&mut self) -> Option<Received> {
        self.events.pop_front()
    }

    /// Feed one datagram. Results are queued for [`Depacketizer::next_event`].
    pub fn push(&mut self, datagram: &[u8]) {
        let Some(header) = parse_header(datagram) else {
            return;
        };
        if header.data_shards == 0 || header.frame_index < self.finished_below {
            return;
        }
        self.highest_frame = self.highest_frame.max(header.frame_index);

        let assembly = self.frames.entry(header.frame_index).or_default();
        assembly.last_block_index = assembly.last_block_index.max(header.last_block_index);
        assembly.seen_start |= header.flags & 0x04 != 0;
        if assembly.timestamp == 0 {
            assembly.timestamp = header.timestamp;
        }
        let block = assembly
            .blocks
            .entry(header.block_index)
            .or_insert_with(|| Block {
                data_shards: usize::from(header.data_shards),
                fec_percentage: header.fec_percentage,
                shards: std::collections::BTreeMap::new(),
            });
        block
            .shards
            .insert(header.shard_index, datagram[PAYLOAD_OFFSET..].to_vec());

        if let Some(finished) = self.try_finish(header.frame_index) {
            self.events.push_back(finished);
        }
        // Independently of that, anything left far behind the newest frame
        // will never complete — a stream that keeps finishing frames must
        // still report the ones it abandoned.
        self.expire();
    }

    fn try_finish(&mut self, frame_index: u32) -> Option<Received> {
        let assembly = self.frames.get(&frame_index)?;
        let wanted = usize::from(assembly.last_block_index) + 1;
        if assembly.blocks.len() < wanted
            || !assembly
                .blocks
                .values()
                .all(|b| b.complete() || b.recoverable())
        {
            return None;
        }
        let assembly = self.frames.remove(&frame_index)?;
        self.finished_below = self.finished_below.max(frame_index + 1);

        let mut payload = Vec::new();
        let mut shard_count = 0usize;
        let mut shard_width = 0usize;
        let recovered = assembly.blocks.values().any(|b| !b.complete());
        for block in assembly.blocks.values() {
            match block.resolve() {
                Ok(shards) => {
                    shard_width = shards.first().map_or(shard_width, Vec::len);
                    shard_count += shards.len();
                    payload.extend(shards.concat());
                }
                Err(e) => {
                    tracing::debug!(frame_index, error = %e, "frame lost in recovery");
                    return Some(Received::Lost(FrameLoss::Unrecoverable { frame_index }));
                }
            }
        }
        Some(
            match parse_frame(
                frame_index,
                &payload,
                shard_width,
                shard_count,
                recovered,
                assembly.timestamp,
            ) {
                Some(frame) => Received::Frame(frame),
                None => Received::Lost(FrameLoss::Unrecoverable { frame_index }),
            },
        )
    }

    /// Give up on frames the host has clearly moved past.
    fn expire(&mut self) {
        let cutoff = self.highest_frame.saturating_sub(REORDER_DEPTH);
        let stale: Vec<u32> = self
            .frames
            .range(..cutoff)
            .map(|(index, _)| *index)
            .collect();
        for index in stale {
            self.frames.remove(&index);
            self.events
                .push_back(Received::Lost(FrameLoss::Unrecoverable {
                    frame_index: index,
                }));
        }
        self.finished_below = self.finished_below.max(cutoff);
    }
}

/// Strip the per-frame prefix and drop the sender's padding.
///
/// Shards are a fixed width, so the last one is zero-padded; the header says
/// how much of it is real. Without that trim the decoder would be handed
/// trailing zeroes as if they were part of the access unit.
fn parse_frame(
    frame_index: u32,
    payload: &[u8],
    shard_width: usize,
    shard_count: usize,
    recovered: bool,
    timestamp: u32,
) -> Option<VideoFrame> {
    if payload.len() <= FRAME_HEADER_LEN {
        return None;
    }
    let latency_units = u16::from_le_bytes([payload[1], payload[2]]);
    let frame_type = payload[3];
    let last_payload_len =
        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;

    let real_len = shard_count
        .saturating_sub(1)
        .saturating_mul(shard_width)
        .saturating_add(last_payload_len);
    // Fall back to everything we assembled if the header's length is absent
    // or implausible — truncating to a wrong value would corrupt the frame
    // more surely than a little trailing padding.
    let end = if (FRAME_HEADER_LEN..=payload.len()).contains(&real_len) {
        real_len
    } else {
        payload.len()
    };
    Some(VideoFrame {
        frame_index,
        // 2 is an IDR; every other type is a predicted frame.
        keyframe: frame_type == 2,
        data: payload[FRAME_HEADER_LEN..end].to_vec(),
        host_latency_us: (latency_units != 0).then(|| u32::from(latency_units) * 100),
        recovered,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::{Depacketizer, Received, parity_shards, parse_header};

    /// Header captured verbatim from the dev host: first shard of frame 1.
    const REAL: [u8; 32] = [
        0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa7, 0x1c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x10, 0x00, 0x40, 0x01,
        0xc0, 0x02,
    ];

    #[test]
    fn reads_a_real_shard_header() {
        let mut datagram = REAL.to_vec();
        datagram.extend_from_slice(&[0u8; 16]);
        let h = parse_header(&datagram).unwrap();
        assert_eq!(h.sequence, 0);
        // Captured value: the host does stamp a real stream clock, unlike
        // some implementations which send zeros.
        assert_eq!(h.timestamp, 0xa71c);
        assert_eq!(h.frame_index, 1);
        assert_eq!(h.shard_index, 0);
        assert_eq!(h.data_shards, 11);
        assert_eq!(h.fec_percentage, 20);
        assert_eq!(h.block_index, 0);
        assert_eq!(h.last_block_index, 0);
        // 0x04 is the start-of-frame bit; the capture's first shard has it.
        assert_eq!(h.flags & 0x04, 0x04);
        assert!(!h.is_parity());
    }

    #[test]
    fn a_shard_past_the_data_count_is_parity() {
        let mut datagram = REAL.to_vec();
        datagram.extend_from_slice(&[0u8; 16]);
        // shard index 11 with 11 data shards is the first parity shard.
        let fec = (20u32 << 4) | (11u32 << 12) | (11u32 << 22);
        datagram[28..32].copy_from_slice(&fec.to_le_bytes());
        assert!(parse_header(&datagram).unwrap().is_parity());
    }

    #[test]
    fn too_small_a_datagram_is_not_a_shard() {
        assert!(parse_header(&[0u8; 32]).is_none());
        assert!(parse_header(&[]).is_none());
    }

    #[test]
    fn parity_count_follows_the_percentage_with_a_floor() {
        // 20% of 11 rounds up to 3.
        assert_eq!(parity_shards(11, 20, 0), 3);
        // Small frames still get the negotiated minimum.
        assert_eq!(parity_shards(2, 20, 0), 2);
        // A shard we actually received outranks the formula: dropping a
        // recoverable block because our arithmetic was low would be worse
        // than trusting the evidence.
        assert_eq!(parity_shards(11, 20, 14), 4);
    }

    /// Build a single-block frame's worth of datagrams.
    fn shards(frame: u32, data_shards: u16, payload: &[u8], width: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for i in 0..data_shards {
            let mut d = REAL.to_vec();
            d[20..24].copy_from_slice(&frame.to_le_bytes());
            d[24] = if i == 0 { 0x05 } else { 0x01 };
            let fec = (20u32 << 4) | (u32::from(i) << 12) | (u32::from(data_shards) << 22);
            d[28..32].copy_from_slice(&fec.to_le_bytes());
            let start = usize::from(i) * width;
            let mut chunk = vec![0u8; width];
            if start < payload.len() {
                let end = (start + width).min(payload.len());
                chunk[..end - start].copy_from_slice(&payload[start..end]);
            }
            d.extend_from_slice(&chunk);
            out.push(d);
        }
        out
    }

    #[test]
    fn assembles_a_frame_from_its_shards() {
        // 8-byte frame header (IDR) then a recognisable access unit.
        let access_unit: Vec<u8> = (0..40u8).collect();
        let (width, count) = (16usize, 3usize);
        // Valid bytes in the final shard: everything real, less the full
        // shards before it.
        let last_len = 8 + access_unit.len() - (count - 1) * width;
        let mut payload = vec![0x01, 0x00, 0x00, 0x02];
        payload.extend_from_slice(&(last_len as u32).to_le_bytes());
        payload.extend_from_slice(&access_unit);

        let mut d = Depacketizer::new();
        let mut got = None;
        for shard in shards(1, count as u16, &payload, width) {
            d.push(&shard);
            while let Some(Received::Frame(f)) = d.next_event() {
                got = Some(f);
            }
        }
        let frame = got.expect("frame completed");
        assert_eq!(frame.frame_index, 1);
        assert!(frame.keyframe);
        assert_eq!(frame.data, access_unit);
    }

    #[test]
    fn a_predicted_frame_is_not_a_keyframe() {
        // One shard: all 16 valid bytes are the header plus 8 of payload.
        let mut payload = vec![0x01, 0x00, 0x00, 0x01];
        payload.extend_from_slice(&16u32.to_le_bytes());
        payload.extend_from_slice(&[7u8; 8]);
        let mut d = Depacketizer::new();
        let mut got = None;
        for shard in shards(1, 1, &payload, 16) {
            d.push(&shard);
            while let Some(Received::Frame(f)) = d.next_event() {
                got = Some(f);
            }
        }
        assert!(!got.expect("frame").keyframe);
    }

    #[test]
    fn duplicate_shards_do_not_produce_a_second_frame() {
        let mut payload = vec![0x01, 0x00, 0x00, 0x02];
        payload.extend_from_slice(&16u32.to_le_bytes());
        payload.extend_from_slice(&[1u8; 8]);
        let built = shards(1, 1, &payload, 16);
        let mut d = Depacketizer::new();
        let mut frames = 0;
        for _ in 0..3 {
            for shard in &built {
                d.push(shard);
                while let Some(event) = d.next_event() {
                    if matches!(event, Received::Frame(_)) {
                        frames += 1;
                    }
                }
            }
        }
        assert_eq!(frames, 1, "a retransmit must not replay a frame");
    }

    #[test]
    fn a_frame_the_host_moved_past_is_reported_lost() {
        let mut d = Depacketizer::new();
        // One lonely shard of frame 1, then frames far ahead of it.
        let mut payload = vec![0x01, 0x00, 0x00, 0x02];
        payload.extend_from_slice(&16u32.to_le_bytes());
        payload.extend_from_slice(&[0u8; 40]);
        d.push(&shards(1, 4, &payload, 16)[0]);
        let mut lost = false;
        for frame in 2..12u32 {
            for shard in shards(frame, 1, &payload, 16) {
                d.push(&shard);
                while let Some(event) = d.next_event() {
                    if matches!(event, Received::Lost(_)) {
                        lost = true;
                    }
                }
            }
        }
        assert!(lost, "an abandoned frame must be reported, not leaked");
    }
}
