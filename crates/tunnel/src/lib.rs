//! The altc Moonlight tunnel: Moonlight's four channels over one
//! authenticated WebTransport session.
//!
//! A browser can open neither the UDP ports nor the ENet channel a Moonlight
//! host expects, so altc terminates a WebTransport session and feeds the
//! same RTSP, control and media machinery the native ports feed. This crate
//! is the wire format both ends agree on, and nothing else: no I/O, no
//! runtime, no dependencies, so it builds identically for `wasm32`, the
//! native test client and the server.
//!
//! # Layout
//!
//! Everything the client opens starts with a one-byte [`StreamKind`] header
//! naming what the stream carries; the server never opens streams.
//!
//! | Channel | Carrier | Header |
//! |---|---|---|
//! | Session hello, then RTSP | the first bidirectional stream | [`StreamKind::Rtsp`] |
//! | Control, reliable (one per ENet channel) | a bidirectional stream | [`StreamKind::Control`] |
//! | Control, unreliable | datagrams | tag [`datagram::CONTROL`] |
//! | Video RTP + FEC | datagrams | tag [`datagram::VIDEO`] |
//! | Audio RTP + FEC | datagrams | tag [`datagram::AUDIO`] |
//! | Voice, both directions | datagrams | tag [`datagram::VOICE`] |
//!
//! Streams carry length-prefixed [`Frame`]s after their header; the RTSP
//! stream's first frame is the [`Hello`], answered by one [`Welcome`] byte,
//! and every frame after that is one RTSP request answered by one RTSP
//! response. A control stream's header is answered by one [`Welcome`] byte
//! too — the tunnel's stand-in for ENet's connect acknowledgement — after
//! which frames are sealed control messages in both directions. Datagrams
//! are the tag byte followed by the payload exactly as it would leave the
//! UDP socket: the host's RTP and FEC framing is untouched, so the shared
//! depacketiser reads it unchanged.
//!
//! Voice is altc's own: [`Voice`] frames of Opus between the people in one
//! game, relayed by the host rather than mixed by it, and carried on no
//! Moonlight channel because Moonlight has none.
//!
//! The session token replaces both the ENet connect data and the media ping
//! payload: the server binds the whole session, streams and datagrams alike,
//! to the Moonlight session the token was minted for, so there is nothing
//! left for a ping to say and the client sends none.

use std::fmt;

/// The wire format version carried in the [`Hello`]. Bumped on any change
/// a peer could misread; the server rejects versions it does not speak.
pub const VERSION: u8 = 1;

/// The largest frame either side will accept on a stream. Generous for an
/// RTSP reply with a long SDP and for any control message, and small enough
/// that a corrupt length cannot make a peer allocate the moon.
pub const MAX_FRAME: usize = 1 << 20;

/// The datagram tags: the first byte of every datagram.
pub mod datagram {
    /// Video RTP and FEC, host → client.
    pub const VIDEO: u8 = 0;
    /// Audio RTP and FEC, host → client.
    pub const AUDIO: u8 = 1;
    /// Unreliable sealed control messages, either direction.
    pub const CONTROL: u8 = 2;
    /// One person's voice, either direction: a [`crate::Voice`] frame.
    pub const VOICE: u8 = 3;
}

/// What a client-opened stream carries, from its first byte(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// The session stream: [`Hello`] first, then RTSP request/response pairs.
    Rtsp,
    /// Reliable control messages for one ENet channel. Moonlight hosts route
    /// on the channel, so each keeps its own stream and its own ordering,
    /// exactly as ENet channels are independent.
    Control { channel: u8 },
}

impl StreamKind {
    const TAG_RTSP: u8 = 0;
    const TAG_CONTROL: u8 = 1;

    /// The header bytes to write before anything else on the stream.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        match self {
            Self::Rtsp => vec![Self::TAG_RTSP],
            Self::Control { channel } => vec![Self::TAG_CONTROL, channel],
        }
    }

    /// Reads a header from the start of `bytes`: the kind and how many
    /// bytes it used. `Incomplete` when more must be read first.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        match bytes {
            [] => Err(Error::Incomplete),
            [Self::TAG_RTSP, ..] => Ok((Self::Rtsp, 1)),
            [Self::TAG_CONTROL] => Err(Error::Incomplete),
            [Self::TAG_CONTROL, channel, ..] => Ok((Self::Control { channel: *channel }, 2)),
            [tag, ..] => Err(Error::UnknownStream(*tag)),
        }
    }
}

/// The first frame on the session stream: who this is and which session
/// it may join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u8,
    /// The one-time stream token the API issued with the launch. Opaque
    /// here; the server knows what it minted.
    pub token: Vec<u8>,
}

impl Hello {
    /// A hello for [`VERSION`] carrying `token`.
    #[must_use]
    pub fn new(token: impl Into<Vec<u8>>) -> Self {
        Self {
            version: VERSION,
            token: token.into(),
        }
    }

    /// The frame payload: version byte, then the token.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.token.len());
        out.push(self.version);
        out.extend_from_slice(&self.token);
        out
    }

    /// The reverse of [`Hello::encode`]; any version is accepted here so the
    /// server can say *which* version it rejects.
    pub fn decode(payload: &[u8]) -> Result<Self, Error> {
        match payload {
            [] => Err(Error::Malformed("empty hello")),
            [version, token @ ..] => Ok(Self {
                version: *version,
                token: token.to_vec(),
            }),
        }
    }
}

/// The server's one-byte answer to a [`Hello`] or a control stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Welcome {
    Accepted,
    Rejected(Reject),
}

/// Why the server turned a stream away. It closes the session after saying
/// so; the client shows the reason rather than retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The hello's version is not one this server speaks.
    Version,
    /// The token is unknown, used already, expired, or minted for someone
    /// else.
    Token,
    /// The session the token was minted for has ended.
    SessionGone,
    /// A stream was opened out of order — control before the hello, a
    /// second session stream — or with a header the server does not know.
    Protocol,
}

impl Welcome {
    const ACCEPTED: u8 = 0;
    const VERSION: u8 = 1;
    const TOKEN: u8 = 2;
    const SESSION_GONE: u8 = 3;
    const PROTOCOL: u8 = 4;

    #[must_use]
    pub fn encode(self) -> u8 {
        match self {
            Self::Accepted => Self::ACCEPTED,
            Self::Rejected(Reject::Version) => Self::VERSION,
            Self::Rejected(Reject::Token) => Self::TOKEN,
            Self::Rejected(Reject::SessionGone) => Self::SESSION_GONE,
            Self::Rejected(Reject::Protocol) => Self::PROTOCOL,
        }
    }

    pub fn decode(byte: u8) -> Result<Self, Error> {
        Ok(match byte {
            Self::ACCEPTED => Self::Accepted,
            Self::VERSION => Self::Rejected(Reject::Version),
            Self::TOKEN => Self::Rejected(Reject::Token),
            Self::SESSION_GONE => Self::Rejected(Reject::SessionGone),
            Self::PROTOCOL => Self::Rejected(Reject::Protocol),
            other => return Err(Error::UnknownWelcome(other)),
        })
    }
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Version => "the server does not speak this tunnel version",
            Self::Token => "the stream token was not accepted",
            Self::SessionGone => "the session has already ended",
            Self::Protocol => "the tunnel was used out of order",
        })
    }
}

/// Length-prefixed framing for streams: a big-endian `u32` length, then
/// that many bytes.
#[derive(Debug)]
pub struct Frame;

impl Frame {
    /// `payload` as it goes on the wire.
    ///
    /// # Panics
    /// If `payload` is longer than [`MAX_FRAME`]: the caller built something
    /// no peer would read, which is a bug rather than a condition.
    #[must_use]
    pub fn encode(payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= MAX_FRAME, "frame exceeds MAX_FRAME");
        let len = u32::try_from(payload.len()).expect("MAX_FRAME fits u32");
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// Reassembles [`Frame`]s from a stream read in arbitrary pieces.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append what the stream delivered.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame's payload, if the buffer holds one.
    ///
    /// A length above [`MAX_FRAME`] is a protocol error: the reader is then
    /// poisoned and returns the error on every call, since nothing after a
    /// corrupt length can be trusted.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(Error::FrameTooLong(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(payload))
    }

    /// Bytes held that do not yet make a frame.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// One datagram: its tag and payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub tag: u8,
    pub payload: &'a [u8],
}

impl<'a> Datagram<'a> {
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.payload.len());
        out.push(self.tag);
        out.extend_from_slice(self.payload);
        out
    }

    /// Splits a received datagram; an empty one carries nothing, not even a
    /// tag, and is malformed.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        match bytes {
            [] => Err(Error::Malformed("empty datagram")),
            [tag, payload @ ..] => Ok(Self { tag: *tag, payload }),
        }
    }
}

/// One 20 ms Opus frame of somebody's voice.
///
/// The same shape both ways, so one encoder and one decoder serve both
/// ends. On the way *to* the host `from` is not read — the host knows whose
/// tunnel the datagram arrived on and stamps the speaker itself, which is
/// also why a client cannot put words in anyone else's mouth — so a client
/// writes 0 there. On the way *out* to a room it names the speaker, and the
/// client shows whoever the lobby says that is.
///
/// `seq` counts frames from one speaker so a receiver can tell a gap from a
/// reordering; it wraps, as a 20 ms counter must.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Voice<'a> {
    /// Who is speaking, as the room numbers them; 0 on the way to the host.
    pub from: u16,
    /// This speaker's frame counter.
    pub seq: u16,
    /// The Opus frame itself: 48 kHz mono, as WebCodecs produces it.
    pub opus: &'a [u8],
}

impl<'a> Voice<'a> {
    /// The whole datagram, tag included, ready to send.
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + self.opus.len());
        out.push(datagram::VOICE);
        out.extend_from_slice(&self.from.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(self.opus);
        out
    }

    /// Reads a voice frame from a datagram's payload — what
    /// [`Datagram::decode`] hands back for [`datagram::VOICE`], the tag
    /// already taken off.
    pub fn decode(payload: &'a [u8]) -> Result<Self, Error> {
        match payload {
            [a, b, c, d, opus @ ..] => Ok(Self {
                from: u16::from_le_bytes([*a, *b]),
                seq: u16::from_le_bytes([*c, *d]),
                opus,
            }),
            _ => Err(Error::Malformed("voice frame shorter than its header")),
        }
    }

    /// The same frame said to come from `from`: what the host sends on,
    /// having decided whose voice this is.
    #[must_use]
    pub fn from_speaker(self, from: u16) -> Self {
        Self { from, ..self }
    }
}

/// What can go wrong reading the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// More bytes are needed before this can be decoded.
    Incomplete,
    /// A stream header tag nobody defined.
    UnknownStream(u8),
    /// A welcome byte nobody defined.
    UnknownWelcome(u8),
    /// A frame length above [`MAX_FRAME`].
    FrameTooLong(usize),
    Malformed(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => f.write_str("incomplete"),
            Self::UnknownStream(tag) => write!(f, "unknown stream header {tag:#04x}"),
            Self::UnknownWelcome(byte) => write!(f, "unknown welcome byte {byte:#04x}"),
            Self::FrameTooLong(len) => write!(f, "frame of {len} bytes exceeds {MAX_FRAME}"),
            Self::Malformed(what) => write!(f, "malformed: {what}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_headers_round_trip_and_report_short_reads() {
        assert_eq!(StreamKind::decode(&[0, 9, 9]), Ok((StreamKind::Rtsp, 1)));
        assert_eq!(
            StreamKind::decode(&StreamKind::Control { channel: 1 }.encode()),
            Ok((StreamKind::Control { channel: 1 }, 2))
        );
        assert_eq!(StreamKind::decode(&[]), Err(Error::Incomplete));
        assert_eq!(StreamKind::decode(&[1]), Err(Error::Incomplete));
        assert_eq!(StreamKind::decode(&[7]), Err(Error::UnknownStream(7)));
    }

    #[test]
    fn hello_carries_version_and_token() {
        let hello = Hello::new(b"tok".to_vec());
        let bytes = hello.encode();
        assert_eq!(bytes, [VERSION, b't', b'o', b'k']);
        assert_eq!(Hello::decode(&bytes), Ok(hello));
        assert_eq!(Hello::decode(&[]), Err(Error::Malformed("empty hello")));
        // A future version decodes so the server can name it in its refusal.
        assert_eq!(Hello::decode(&[9]).unwrap().version, 9);
    }

    #[test]
    fn every_welcome_round_trips_and_unknown_bytes_do_not() {
        for w in [
            Welcome::Accepted,
            Welcome::Rejected(Reject::Version),
            Welcome::Rejected(Reject::Token),
            Welcome::Rejected(Reject::SessionGone),
            Welcome::Rejected(Reject::Protocol),
        ] {
            assert_eq!(Welcome::decode(w.encode()), Ok(w));
        }
        assert_eq!(Welcome::decode(200), Err(Error::UnknownWelcome(200)));
    }

    #[test]
    fn frames_reassemble_from_any_split() {
        let a = Frame::encode(b"first");
        let b = Frame::encode(b"");
        let c = Frame::encode(&[7u8; 300]);
        let wire: Vec<u8> = [a, b, c].concat();
        for chunk in [1usize, 3, 7, wire.len()] {
            let mut reader = FrameReader::new();
            let mut got = Vec::new();
            for piece in wire.chunks(chunk) {
                reader.push(piece);
                while let Some(frame) = reader.next_frame().unwrap() {
                    got.push(frame);
                }
            }
            assert_eq!(
                got,
                vec![b"first".to_vec(), vec![], vec![7u8; 300]],
                "chunk {chunk}"
            );
            assert_eq!(reader.pending(), 0);
        }
    }

    #[test]
    fn a_corrupt_length_poisons_the_reader() {
        let mut reader = FrameReader::new();
        reader.push(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert_eq!(reader.next_frame(), Err(Error::FrameTooLong(MAX_FRAME + 1)));
        reader.push(b"more");
        assert_eq!(reader.next_frame(), Err(Error::FrameTooLong(MAX_FRAME + 1)));
    }

    #[test]
    fn datagrams_are_tag_then_payload() {
        let d = Datagram {
            tag: datagram::AUDIO,
            payload: b"rtp",
        };
        let bytes = d.encode();
        assert_eq!(bytes, [1, b'r', b't', b'p']);
        assert_eq!(Datagram::decode(&bytes), Ok(d));
        assert_eq!(
            Datagram::decode(&[]),
            Err(Error::Malformed("empty datagram"))
        );
        assert_eq!(Datagram::decode(&[2]).unwrap().payload, b"");
    }
    #[test]
    fn a_voice_frame_names_its_speaker_and_survives_the_trip() {
        let out = Voice {
            from: 0,
            seq: 7,
            opus: b"opus",
        };
        let wire = out.encode();
        assert_eq!(wire[0], datagram::VOICE);
        let payload = Datagram::decode(&wire).unwrap().payload;
        // The host stamps the speaker and sends the same frame on.
        let on = Voice::decode(payload).unwrap().from_speaker(513);
        assert_eq!(on.seq, 7);
        let relayed = on.encode();
        let heard = Voice::decode(Datagram::decode(&relayed).unwrap().payload).unwrap();
        assert_eq!(
            heard,
            Voice {
                from: 513,
                seq: 7,
                opus: b"opus"
            }
        );
        // A frame with nothing but a header is silence, not a malformation;
        // anything shorter than the header is.
        assert_eq!(Voice::decode(&[1, 0, 0, 0]).unwrap().opus, b"");
        assert_eq!(
            Voice::decode(&[1, 0, 0]),
            Err(Error::Malformed("voice frame shorter than its header"))
        );
    }
}
