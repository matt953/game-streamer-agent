//! Hot-path datagram framing (spec 04). Hand-rolled fixed layouts:
//! zero-alloc parse, no serde on the per-frame path.
//!
//! Layout (big-endian), first byte discriminates the datagram type:
//!
//! ```text
//! Video: | type u8 | epoch u8 | frame_id u32 | kind u8 |
//!        | chunk_index u16 | chunk_count u16 | capture_ts_us u32 | payload... |
//! Audio: | type u8 | seq u16 | ts_us u32 | payload... |
//! ```

use gsa_core::error::ProtocolError;
use gsa_core::media::FrameKind;

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
    pub chunk_index: u16,
    pub chunk_count: u16,
    /// Agent-clock capture timestamp, truncated (`time::wire_ts`).
    pub capture_ts_us: u32,
}

pub const VIDEO_HEADER_LEN: usize = 1 + 4 + 1 + 4 + 1 + 2 + 2 + 4;

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
        let capture_ts_us = u32::from_be_bytes(datagram[15..19].try_into().expect("sized"));
        if chunk_count == 0 || chunk_index >= chunk_count {
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
                capture_ts_us,
            },
            &datagram[VIDEO_HEADER_LEN..],
        ))
    }
}

/// Split one encoded frame into datagram payload chunks of at most
/// `max_datagram` bytes (header included). Returns ready-to-send buffers.
pub fn chunk_video_frame(
    header_template: VideoDatagramHeader,
    frame_data: &[u8],
    max_datagram: usize,
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
    let mut out = Vec::with_capacity(count);
    for (i, chunk) in frame_data.chunks(max_payload).enumerate() {
        let hdr = VideoDatagramHeader {
            chunk_index: i as u16,
            chunk_count: count as u16,
            ..header_template
        };
        out.push(hdr.encode_with_payload(chunk));
    }
    if frame_data.is_empty() {
        // Preserve "a frame always has >= 1 chunk" for receiver simplicity.
        let hdr = VideoDatagramHeader {
            chunk_index: 0,
            chunk_count: 1,
            ..header_template
        };
        out.push(hdr.encode_with_payload(&[]));
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
            session_epoch: 3,
            frame_id: 0xdead_beef,
            kind: FrameKind::P,
            chunk_index: 0,
            chunk_count: 1,
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
        let chunks = chunk_video_frame(hdr(), &data, 1200).unwrap();
        let mut reassembled = Vec::new();
        let mut expect_count = None;
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= 1200);
            let (h, body) = VideoDatagramHeader::parse(c).unwrap();
            assert_eq!(h.chunk_index as usize, i);
            *expect_count.get_or_insert(h.chunk_count) = h.chunk_count;
            reassembled.extend_from_slice(body);
        }
        assert_eq!(reassembled, data);
        assert_eq!(expect_count.unwrap() as usize, chunks.len());
    }

    #[test]
    fn empty_frame_still_yields_one_chunk() {
        let chunks = chunk_video_frame(hdr(), &[], 1200).unwrap();
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
