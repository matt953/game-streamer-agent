//! Hot-path datagram framing (spec 04). Hand-rolled fixed layouts:
//! zero-alloc parse, no serde on the per-frame path.
//!
//! Layout (big-endian), first byte discriminates the datagram type:
//!
//! ```text
//! Video: | type u8 | seq u32 | epoch u8 | frame_id u32 | kind u8 |
//!        | chunk_index u16 | chunk_count u16 | parity_count u8 |
//!        | frame_len u32 | capture_ts_us u32 | payload... |
//!
//! Shards `0..chunk_count` carry frame bytes; `chunk_count..chunk_count+
//! parity_count` are Reed-Solomon parity over the equal-sized data shards
//! (the last is zero-padded for the field math; `frame_len` trims it).
//! Parity is computed per [`fec::group_layout`] group so FEC cost stays
//! linear in frame size; the layout derives from (`chunk_count`,
//! `parity_count`), so nothing extra rides the wire.
//! Audio: | type u8 | seq u16 | ts_us u32 | payload... |
//! ```

use gsa_core::error::ProtocolError;
use gsa_core::media::FrameKind;

use crate::fec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatagramType {
    Video,
    Audio,
    /// Discardable pacer filler: loads the wire for bandwidth probing; the
    /// receiver records its arrival (per-packet feedback) and drops it.
    Padding,
}

impl DatagramType {
    #[must_use]
    pub fn to_wire(self) -> u8 {
        match self {
            DatagramType::Video => 1,
            DatagramType::Audio => 2,
            DatagramType::Padding => 3,
        }
    }

    pub fn from_wire(b: u8) -> Result<Self, ProtocolError> {
        Ok(match b {
            1 => DatagramType::Video,
            2 => DatagramType::Audio,
            3 => DatagramType::Padding,
            other => return Err(ProtocolError::UnknownDatagramType(other)),
        })
    }
}

/// Transport-wide sequence number: bytes [1..5] of every video and padding
/// datagram, in send order — stamped by the pacer at send time (probe and
/// media packets share one sequence space, like WebRTC's transport-wide CC).
pub const SEQ_OFFSET: usize = 1;

/// Stamp the send-order sequence into an encoded datagram (video or padding).
pub fn stamp_seq(datagram: &mut [u8], seq: u32) {
    datagram[SEQ_OFFSET..SEQ_OFFSET + 4].copy_from_slice(&seq.to_be_bytes());
}

/// Read the transport-wide sequence from a video or padding datagram.
pub fn read_seq(datagram: &[u8]) -> Result<u32, ProtocolError> {
    if datagram.len() < SEQ_OFFSET + 4 {
        return Err(ProtocolError::TooShort {
            got: datagram.len(),
            need: SEQ_OFFSET + 4,
        });
    }
    Ok(u32::from_be_bytes(
        datagram[SEQ_OFFSET..SEQ_OFFSET + 4]
            .try_into()
            .expect("sized"),
    ))
}

/// Build a padding datagram of exactly `len` bytes (≥ header). Sequence is
/// stamped at send time like every datagram.
#[must_use]
pub fn encode_padding(len: usize) -> Vec<u8> {
    let len = len.max(PADDING_HEADER_LEN);
    let mut out = vec![0u8; len];
    out[0] = DatagramType::Padding.to_wire();
    out
}

pub const PADDING_HEADER_LEN: usize = 1 + 4;

/// Parsed header of a video datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoDatagramHeader {
    /// Send-order transport sequence (see [`SEQ_OFFSET`]); 0 until stamped.
    pub seq: u32,
    /// Bumps on encoder reset; receivers discard stale-epoch chunks.
    pub session_epoch: u8,
    /// Truncated frame id (`FrameId::wire`).
    pub frame_id: u32,
    pub kind: FrameKind,
    /// Shard index: `0..chunk_count` = data, then parity.
    pub chunk_index: u16,
    /// Data shard count.
    pub chunk_count: u16,
    /// Reed-Solomon parity shards following the data shards.
    pub parity_count: u8,
    /// Exact encoded frame length (bytes) — trims shard padding.
    pub frame_len: u32,
    /// Agent-clock capture timestamp, truncated (`time::wire_ts`).
    pub capture_ts_us: u32,
}

pub const VIDEO_HEADER_LEN: usize = 1 + 4 + 1 + 4 + 1 + 2 + 2 + 1 + 4 + 4;

/// Total shard count (data + parity) for a frame's header values.
#[must_use]
pub fn total_shards(chunk_count: u16, parity_count: u8) -> usize {
    usize::from(chunk_count) + usize::from(parity_count)
}

impl VideoDatagramHeader {
    /// Serialize the header followed by `payload` into a fresh buffer.
    #[must_use]
    pub fn encode_with_payload(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + payload.len());
        out.push(DatagramType::Video.to_wire());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.push(self.session_epoch);
        out.extend_from_slice(&self.frame_id.to_be_bytes());
        out.push(self.kind.to_wire());
        out.extend_from_slice(&self.chunk_index.to_be_bytes());
        out.extend_from_slice(&self.chunk_count.to_be_bytes());
        out.push(self.parity_count);
        out.extend_from_slice(&self.frame_len.to_be_bytes());
        out.extend_from_slice(&self.capture_ts_us.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Parse a video datagram; returns the header and a borrowed payload.
    /// Zero-alloc; safe on attacker-controlled bytes (fuzzed).
    pub fn parse(datagram: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if datagram.len() < VIDEO_HEADER_LEN {
            return Err(ProtocolError::TooShort {
                got: datagram.len(),
                need: VIDEO_HEADER_LEN,
            });
        }
        let ty = DatagramType::from_wire(datagram[0])?;
        if ty != DatagramType::Video {
            return Err(ProtocolError::UnknownDatagramType(datagram[0]));
        }
        let seq = u32::from_be_bytes(datagram[1..5].try_into().expect("sized"));
        let session_epoch = datagram[5];
        let frame_id = u32::from_be_bytes(datagram[6..10].try_into().expect("sized"));
        let kind = FrameKind::from_wire(datagram[10])?;
        let chunk_index = u16::from_be_bytes(datagram[11..13].try_into().expect("sized"));
        let chunk_count = u16::from_be_bytes(datagram[13..15].try_into().expect("sized"));
        let parity_count = datagram[15];
        let frame_len = u32::from_be_bytes(datagram[16..20].try_into().expect("sized"));
        let capture_ts_us = u32::from_be_bytes(datagram[20..24].try_into().expect("sized"));
        if chunk_count == 0 || usize::from(chunk_index) >= total_shards(chunk_count, parity_count) {
            return Err(ProtocolError::InvalidChunk {
                index: chunk_index,
                count: chunk_count,
            });
        }
        Ok((
            Self {
                seq,
                session_epoch,
                frame_id,
                kind,
                chunk_index,
                chunk_count,
                parity_count,
                frame_len,
                capture_ts_us,
            },
            &datagram[VIDEO_HEADER_LEN..],
        ))
    }
}

/// Split one encoded frame into datagram payload chunks of at most
/// `max_datagram` bytes (header included), plus Reed-Solomon parity shards:
/// `parity_permille`/1000 of the data shard count (rounded up, min 1),
/// encoded per [`fec::group_layout`] group so FEC cost stays linear in frame
/// size. Parity is capped at 255 shards total (`parity_count` is a u8 on the
/// wire); single-group frames also keep the historical GF(2^8) rule of
/// shipping parity-less when `k + m > 255`, so their datagrams stay
/// byte-identical to the ungrouped encoding. Returns ready-to-send buffers.
pub fn chunk_video_frame(
    header_template: VideoDatagramHeader,
    frame_data: &[u8],
    max_datagram: usize,
    parity_permille: u32,
) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let max_payload = max_datagram.saturating_sub(VIDEO_HEADER_LEN);
    if max_payload == 0 {
        return Err(ProtocolError::Serialize(
            "max_datagram smaller than header".into(),
        ));
    }
    let count = frame_data.len().div_ceil(max_payload).max(1);
    if count > u16::MAX as usize {
        return Err(ProtocolError::Serialize(format!(
            "frame needs {count} chunks (> u16::MAX)"
        )));
    }
    let parity = if parity_permille == 0 {
        0
    } else {
        (count * parity_permille as usize).div_ceil(1000).max(1)
    };
    // Wire caps: `parity_count` is a u8 and `chunk_index` a u16.
    let parity = parity.min(255).min(usize::from(u16::MAX) - count);
    // Single-group frames keep the historical GF(2^8) rule (and byte-identical
    // datagrams): ship parity-less when k + m > 255. Multi-group frames encode
    // per group, where k_g + m_g <= 255 always holds (k_g <= MAX_GROUP_DATA,
    // m_g <= ceil(255 / 2) once there are two or more groups).
    let parity = if count <= fec::MAX_GROUP_DATA && count + parity > 255 {
        0
    } else {
        parity
    };

    let hdr = |i: usize| VideoDatagramHeader {
        chunk_index: i as u16,
        chunk_count: count as u16,
        parity_count: parity as u8,
        frame_len: frame_data.len() as u32,
        ..header_template
    };

    let mut out = Vec::with_capacity(count + parity);
    if frame_data.is_empty() {
        // Preserve "a frame always has >= 1 chunk" for receiver simplicity.
        out.push(hdr(0).encode_with_payload(&[]));
    } else {
        for (i, chunk) in frame_data.chunks(max_payload).enumerate() {
            out.push(hdr(i).encode_with_payload(chunk));
        }
    }
    if parity > 0 {
        // Equal-size shards for the field math: the last data shard is
        // zero-padded; `frame_len` trims it back after reconstruction.
        let shard_len = frame_data.len().min(max_payload).max(1);
        let padded_last;
        let mut shards: Vec<&[u8]> = frame_data.chunks(max_payload).collect();
        let last = shards.last().copied().unwrap_or(&[]);
        if last.len() < shard_len {
            let mut s = last.to_vec();
            s.resize(shard_len, 0);
            padded_last = s;
            *shards.last_mut().expect("count >= 1") = &padded_last;
        }
        // Per-group parity: O(k_g * m_g) per group keeps the frame linear.
        for group in fec::group_layout(count, parity) {
            if group.parity_len == 0 {
                continue;
            }
            let data = &shards[group.data_start..group.data_start + group.data_len];
            for (p, shard) in fec::encode_parity(data, group.parity_len)
                .iter()
                .enumerate()
            {
                out.push(hdr(group.parity_start + p).encode_with_payload(shard));
            }
        }
    }
    Ok(out)
}

/// Parsed header of an audio datagram. One Opus frame per datagram (frames are
/// small enough to never need chunking); loss is concealed by Opus PLC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioDatagramHeader {
    /// Monotonic sequence number; wraps. Gaps signal loss for PLC.
    pub seq: u16,
    /// Agent-clock capture timestamp, truncated (`time::wire_ts`).
    pub ts_us: u32,
}

pub const AUDIO_HEADER_LEN: usize = 1 + 2 + 4;

impl AudioDatagramHeader {
    /// Serialize the header followed by the Opus payload into a fresh buffer.
    #[must_use]
    pub fn encode_with_payload(&self, opus: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + opus.len());
        out.push(DatagramType::Audio.to_wire());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.ts_us.to_be_bytes());
        out.extend_from_slice(opus);
        out
    }

    /// Parse an audio datagram; returns the header and a borrowed Opus payload.
    /// Zero-alloc; safe on attacker-controlled bytes (fuzzed).
    pub fn parse(datagram: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if datagram.len() < AUDIO_HEADER_LEN {
            return Err(ProtocolError::TooShort {
                got: datagram.len(),
                need: AUDIO_HEADER_LEN,
            });
        }
        let ty = DatagramType::from_wire(datagram[0])?;
        if ty != DatagramType::Audio {
            return Err(ProtocolError::UnknownDatagramType(datagram[0]));
        }
        let seq = u16::from_be_bytes(datagram[1..3].try_into().expect("sized"));
        let ts_us = u32::from_be_bytes(datagram[3..7].try_into().expect("sized"));
        Ok((Self { seq, ts_us }, &datagram[AUDIO_HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr() -> VideoDatagramHeader {
        VideoDatagramHeader {
            seq: 0,
            session_epoch: 3,
            frame_id: 0xdead_beef,
            kind: FrameKind::P,
            chunk_index: 0,
            chunk_count: 1,
            parity_count: 0,
            frame_len: 0,
            capture_ts_us: 123_456,
        }
    }

    #[test]
    fn header_round_trip() {
        let payload = b"hello pixels";
        let wire = hdr().encode_with_payload(payload);
        let (parsed, body) = VideoDatagramHeader::parse(&wire).unwrap();
        assert_eq!(parsed, hdr());
        assert_eq!(body, payload);
    }

    #[test]
    fn parse_rejects_short_and_bad_type() {
        assert!(VideoDatagramHeader::parse(&[1, 2, 3]).is_err());
        let mut wire = hdr().encode_with_payload(b"x");
        wire[0] = 99;
        assert!(VideoDatagramHeader::parse(&wire).is_err());
    }

    #[test]
    fn chunking_covers_all_bytes_in_order() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let chunks = chunk_video_frame(hdr(), &data, 1200, 0).unwrap();
        let mut reassembled = Vec::new();
        let mut expect_count = None;
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= 1200);
            let (h, body) = VideoDatagramHeader::parse(c).unwrap();
            assert_eq!(h.chunk_index as usize, i);
            assert_eq!(h.frame_len as usize, data.len());
            *expect_count.get_or_insert(h.chunk_count) = h.chunk_count;
            reassembled.extend_from_slice(body);
        }
        assert_eq!(reassembled, data);
        assert_eq!(expect_count.unwrap() as usize, chunks.len());
    }

    #[test]
    fn parity_shards_follow_the_data_shards() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        // 5 data shards at 1180 payload; 200 permille -> 1 parity shard.
        let chunks = chunk_video_frame(hdr(), &data, 1200, 200).unwrap();
        let (last, _) = VideoDatagramHeader::parse(chunks.last().unwrap()).unwrap();
        assert_eq!(last.parity_count, 1);
        assert_eq!(chunks.len(), usize::from(last.chunk_count) + 1);
        assert_eq!(last.chunk_index, last.chunk_count); // first parity slot
    }

    /// Small frames (k <= MAX_GROUP_DATA) are a single FEC group and must be
    /// byte-identical to the historical ungrouped encoding: same data chunks,
    /// parity computed by one `encode_parity` over all (padded) data shards.
    #[test]
    fn small_frame_datagrams_byte_identical_to_ungrouped() {
        for &(frame_len, permille) in &[(5_000usize, 200u32), (9_700, 300), (1, 500), (37_600, 100)]
        {
            let data: Vec<u8> = (0..frame_len).map(|i| (i * 31 % 253) as u8).collect();
            let got = chunk_video_frame(hdr(), &data, 1200, permille).unwrap();

            // Reference: the pre-grouping single-call encoding.
            let max_payload = 1200 - VIDEO_HEADER_LEN;
            let count = data.len().div_ceil(max_payload).max(1);
            assert!(count <= fec::MAX_GROUP_DATA, "test shape must be one group");
            let parity = (count * permille as usize).div_ceil(1000).max(1);
            let shard_len = data.len().min(max_payload);
            let mut shards: Vec<Vec<u8>> = data.chunks(max_payload).map(<[u8]>::to_vec).collect();
            shards.last_mut().unwrap().resize(shard_len, 0);
            let refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
            let mut expect: Vec<Vec<u8>> = data
                .chunks(max_payload)
                .enumerate()
                .map(|(i, c)| {
                    VideoDatagramHeader {
                        chunk_index: i as u16,
                        chunk_count: count as u16,
                        parity_count: parity as u8,
                        frame_len: data.len() as u32,
                        ..hdr()
                    }
                    .encode_with_payload(c)
                })
                .collect();
            for (p, shard) in fec::encode_parity(&refs, parity).iter().enumerate() {
                expect.push(
                    VideoDatagramHeader {
                        chunk_index: (count + p) as u16,
                        chunk_count: count as u16,
                        parity_count: parity as u8,
                        frame_len: data.len() as u32,
                        ..hdr()
                    }
                    .encode_with_payload(shard),
                );
            }
            assert_eq!(got, expect, "frame_len={frame_len} permille={permille}");
        }
    }

    /// Large frames get per-group parity with contiguous global indices and
    /// group shard counts that stay inside GF(2^8).
    #[test]
    fn grouped_parity_indices_are_contiguous_and_complete() {
        let data: Vec<u8> = (0..175 * 100).map(|i| (i % 251) as u8).collect();
        let max_datagram = 100 + VIDEO_HEADER_LEN;
        // 175 data shards; 140 permille -> 25 parity shards.
        let chunks = chunk_video_frame(hdr(), &data, max_datagram, 140).unwrap();
        let (first, _) = VideoDatagramHeader::parse(&chunks[0]).unwrap();
        assert_eq!(first.chunk_count, 175);
        assert_eq!(first.parity_count, 25);
        assert_eq!(chunks.len(), 200);
        for (i, c) in chunks.iter().enumerate() {
            let (h, body) = VideoDatagramHeader::parse(c).unwrap();
            assert_eq!(h.chunk_index as usize, i);
            assert_eq!(body.len(), 100); // parity shards are full-size
        }
    }

    /// Frames beyond 255 data shards used to ship parity-less (GF(2^8) cap);
    /// grouping lifts that — only the u8 `parity_count` field caps parity.
    #[test]
    fn very_large_frames_now_carry_parity() {
        let data: Vec<u8> = vec![7; 300 * 50];
        let max_datagram = 50 + VIDEO_HEADER_LEN;
        let chunks = chunk_video_frame(hdr(), &data, max_datagram, 100).unwrap();
        let (h, _) = VideoDatagramHeader::parse(&chunks[0]).unwrap();
        assert_eq!(h.chunk_count, 300);
        assert_eq!(h.parity_count, 30);
        assert_eq!(chunks.len(), 330);
    }

    #[test]
    fn empty_frame_still_yields_one_chunk() {
        let chunks = chunk_video_frame(hdr(), &[], 1200, 0).unwrap();
        assert_eq!(chunks.len(), 1);
        let (h, body) = VideoDatagramHeader::parse(&chunks[0]).unwrap();
        assert_eq!(h.chunk_count, 1);
        assert!(body.is_empty());
    }

    #[test]
    fn audio_header_round_trip() {
        let h = AudioDatagramHeader {
            seq: 0xbeef,
            ts_us: 987_654,
        };
        let opus = b"\x01\x02\x03 opus frame";
        let wire = h.encode_with_payload(opus);
        let (parsed, body) = AudioDatagramHeader::parse(&wire).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(body, opus);
    }

    #[test]
    fn audio_parse_rejects_short_and_wrong_type() {
        assert!(AudioDatagramHeader::parse(&[2, 0]).is_err());
        // A video datagram must not parse as audio.
        let video = hdr().encode_with_payload(b"x");
        assert!(AudioDatagramHeader::parse(&video).is_err());
    }
}
