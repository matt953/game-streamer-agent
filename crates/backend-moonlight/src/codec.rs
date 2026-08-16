//! Which video codec a session runs, chosen from what both ends support.
//!
//! Both ends have a say and neither can be assumed:
//!
//! - The **host** advertises a bitfield in `/serverinfo`. It withholds detail
//!   from unauthenticated callers, so a host read before pairing looks less
//!   capable than it is — see [`crate::ServerInfo`].
//! - The **client** can only decode what its hardware implements. AV1 needs an
//!   M3 or newer Mac, or an A17 Pro or newer iPhone; on anything older,
//!   offering it produces a stream that never decodes.
//!
//! The client states both in one list — `SessionRequest::decode_codecs`, what
//! it can decode, richest first — so membership is capability and order is
//! preference. "Richest" is not universal: AV1 saves the most bandwidth, HEVC
//! is the most widely accelerated, and on a fast local link H.264 costs
//! nothing worth saving. The ordering is therefore the embedder's to set.
//!
//! H.264 is the floor. Every host and every decoder has it, so a negotiation
//! that agrees on nothing else still produces a picture.

use gsa_core::media::Codec;

/// Bits of `ServerCodecModeSupport` this client relies on.
///
/// The field carries more than these — profile variants for 4:4:4 and higher
/// bit depths — but a bit we do not request is a bit we cannot get wrong, so
/// only the three base profiles are read. A host that sets a profile bit
/// without its base bit would be misread, which no observed host does.
mod bits {
    /// Hosts set this, but its absence proves nothing: an unpaired read
    /// reports an empty field for a host that plainly encodes H.264. Kept
    /// because it documents the layout the other two bits sit in.
    #[allow(dead_code)]
    pub const H264: u32 = 1 << 0;
    pub const HEVC: u32 = 1 << 8;
    pub const AV1: u32 = 1 << 16;
}

/// What a host can encode, as read from its `/serverinfo`.
///
/// The default is a host that reports nothing, which still encodes H.264.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCodecs {
    /// `ServerCodecModeSupport`.
    pub modes: u32,
    /// `MaxLumaPixelsHEVC`. Zero means no HEVC, and hosts that predate the
    /// bitfield report only this.
    pub max_luma_hevc: u64,
}

impl HostCodecs {
    /// Whether the host offers `codec`.
    ///
    /// H.264 is assumed even when the bitfield is empty: a host that speaks
    /// this protocol at all encodes H.264, and an unpaired read reports zero
    /// for everything.
    #[must_use]
    pub fn supports(self, codec: Codec) -> bool {
        match codec {
            Codec::H264 => true,
            // Two independent signals, either sufficient: the bitfield is the
            // modern one, the luma limit is what older hosts set.
            Codec::Hevc => self.modes & bits::HEVC != 0 || self.max_luma_hevc > 0,
            Codec::Av1 => self.modes & bits::AV1 != 0,
            _ => false,
        }
    }
}

/// The codec to ask the host for.
///
/// Walks `client` in order and takes the first the host also has, so the
/// embedder's ordering is the policy and this function holds none of its own.
#[must_use]
pub fn choose(host: HostCodecs, client: &[Codec]) -> Codec {
    let chosen = client
        .iter()
        .copied()
        .find(|&codec| host.supports(codec))
        .unwrap_or(Codec::H264);
    tracing::info!(
        ?chosen,
        ?client,
        host_modes = format!("{:#x}", host.modes),
        host_hevc_luma = host.max_luma_hevc,
        "video codec negotiated"
    );
    chosen
}

/// The `bitstream_format` value the host's RTSP negotiation expects.
#[must_use]
pub fn bitstream_format(codec: Codec) -> u32 {
    match codec {
        Codec::Hevc => 1,
        Codec::Av1 => 2,
        // H.264 and anything this build does not know how to ask for.
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability read from the real Apollo host on the dev network.
    const APOLLO: HostCodecs = HostCodecs {
        modes: 2_032_385,
        max_luma_hevc: 1_869_449_984,
    };

    #[test]
    fn a_hosts_bitfield_names_the_codecs_it_encodes() {
        assert!(APOLLO.supports(Codec::H264));
        assert!(APOLLO.supports(Codec::Hevc));
        assert!(APOLLO.supports(Codec::Av1));
    }

    /// A host that reports nothing still encodes H.264 — that is what an
    /// unpaired read looks like, and refusing to stream to it would be wrong.
    #[test]
    fn an_empty_bitfield_still_offers_the_floor() {
        let bare = HostCodecs {
            modes: 0,
            max_luma_hevc: 0,
        };
        assert!(bare.supports(Codec::H264));
        assert!(!bare.supports(Codec::Hevc));
        assert!(!bare.supports(Codec::Av1));
    }

    /// Hosts that predate the bitfield announce HEVC only by its luma limit.
    #[test]
    fn the_luma_limit_announces_hevc_on_its_own() {
        let older = HostCodecs {
            modes: bits::H264,
            max_luma_hevc: 1_869_449_984,
        };
        assert!(older.supports(Codec::Hevc));
        assert!(!older.supports(Codec::Av1));
    }

    /// A codec the client cannot decode must never be asked for, however
    /// capable the host is: the stream would arrive and never produce a frame.
    #[test]
    fn a_codec_the_client_cannot_decode_is_not_requested() {
        assert_eq!(choose(APOLLO, &[Codec::H264]), Codec::H264);
        assert_eq!(choose(APOLLO, &[Codec::Hevc, Codec::H264]), Codec::Hevc);
    }

    /// Likewise a codec the host cannot encode, however capable the client is.
    #[test]
    fn a_codec_the_host_cannot_encode_is_not_requested() {
        let h264_only = HostCodecs {
            modes: bits::H264,
            max_luma_hevc: 0,
        };
        let all = [Codec::Av1, Codec::Hevc, Codec::H264];
        assert_eq!(choose(h264_only, &all), Codec::H264);
        let hevc_host = HostCodecs {
            modes: bits::H264 | bits::HEVC,
            max_luma_hevc: 1,
        };
        assert_eq!(choose(hevc_host, &all), Codec::Hevc);
    }

    /// The preference order is the policy: the same two ends negotiate a
    /// different codec when the user asks for one.
    #[test]
    fn the_preference_order_decides_between_equals() {
        assert_eq!(choose(APOLLO, &[Codec::Av1, Codec::Hevc]), Codec::Av1);
        assert_eq!(choose(APOLLO, &[Codec::Hevc, Codec::Av1]), Codec::Hevc);
        // An embedder that declares nothing still gets a picture.
        assert_eq!(choose(APOLLO, &[]), Codec::H264);
    }

    #[test]
    fn formats_match_the_hosts_numbering() {
        assert_eq!(bitstream_format(Codec::H264), 0);
        assert_eq!(bitstream_format(Codec::Hevc), 1);
        assert_eq!(bitstream_format(Codec::Av1), 2);
    }
}
