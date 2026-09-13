//! RTSP negotiation (TCP :48010).
//!
//! Text RTSP/1.0, with three deviations from it:
//!
//! - **One request per TCP connection.** The host closes the socket after every
//!   response, and that close — not a content length — delimits the response.
//!   A reused socket is reset.
//! - **Request targets are not always URIs.** `SETUP` and `ANNOUNCE` address
//!   streams as a bare `streamid=video/0/0`, and `PLAY` targets `/`.
//! - **The host assigns the media ports.** Whatever the client proposes in
//!   `Transport` is ignored; the ports come back in the responses.

use gsa_core::{Error, Result};

/// Carries one RTSP request and returns the whole response.
///
/// The protocol is one request per connection, delimited by the peer closing
/// it; natively that is a fresh TCP connection each time
/// ([`crate::TcpRtsp`]), in the browser a fresh tunnel stream.
pub trait RtspExchange {
    fn exchange(&mut self, request: &[u8]) -> impl std::future::Future<Output = Result<Vec<u8>>>;
}

/// Client protocol generation. Hosts branch on this value.
const CLIENT_VERSION: &str = "14";

/// One parsed RTSP response.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the handshake settled on.
#[derive(Debug, Clone)]
pub struct Negotiated {
    pub video_port: u16,
    pub audio_port: u16,
    pub control_port: u16,
    /// Session ping payload, echoed so the host can bind a stream to this
    /// session rather than trusting the source address.
    ///
    /// **Exactly one socket may use it.** The host issues one payload per
    /// session and binds by payload, so a second socket sending the same value
    /// steals the first stream's binding: pinging audio with it stops video.
    /// It goes to the video socket; audio uses the address-matched legacy ping.
    pub ping_payload: Option<[u8; 16]>,
    /// Passed as ENet connect data, binding the control channel to this
    /// session for the same reason.
    pub connect_data: Option<u32>,
    /// The host's `encryptionSupported` bitmask: 0x01 control v2, 0x02 video,
    /// 0x04 audio. Absent means the host never mentioned it.
    pub encryption_supported: u32,
    /// What the host asks us to actually turn on.
    pub encryption_requested: u32,
    /// The host can repair a broken reference chain without a full keyframe.
    pub reference_invalidation: bool,
    /// The multistream layout the host will encode audio with, when more
    /// than stereo was requested. Parsed from the host's own `surround-params`
    /// lines — the encoder's declaration, not a table of assumptions.
    pub surround: Option<gsa_core::media::SurroundLayout>,
}

impl Negotiated {
    /// The control channel must use the newer nonce construction.
    ///
    /// The two schemes use incompatible IV layouts, so this is read from the
    /// host rather than assumed.
    #[must_use]
    pub fn control_v2(&self) -> bool {
        self.encryption_supported & 0x01 != 0
    }
}

/// What we ask for in `ANNOUNCE`.
#[derive(Debug, Clone, Copy)]
pub struct StreamRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 0 = H.264, 1 = HEVC, 2 = AV1.
    pub bitstream_format: u32,
    /// Ask for high dynamic range.
    ///
    /// Separate from the `hdrMode` in the launch request, and both are
    /// required: launching in HDR and then negotiating a standard-range
    /// stream gets a standard-range stream, with nothing reporting a
    /// contradiction.
    pub hdr: bool,
    pub bitrate_kbps: u32,
    /// Video shard size; hosts use 1024 or 1392.
    pub packet_size: u32,
    pub channels: u8,
}

pub struct Rtsp {
    host_header: String,
    cseq: u32,
    session: Option<String>,
}

impl std::fmt::Debug for Rtsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rtsp")
            .field("host", &self.host_header)
            .finish()
    }
}

impl Rtsp {
    /// `rtsp_url` is the `sessionUrl0` the launch endpoint returned. Hosts
    /// match requests against the host string it carries, so it is reused
    /// verbatim rather than rebuilt from the address.
    pub fn new(rtsp_url: &str) -> Result<Self> {
        let rest = rtsp_url
            .strip_prefix("rtsp://")
            .ok_or_else(|| Error::Session(format!("not an rtsp url: {rtsp_url}")))?;
        let host_header = rest.trim_end_matches('/').to_owned();
        Ok(Self {
            host_header,
            cseq: 0,
            session: None,
        })
    }

    /// The host string the URL carried, `host:port` as the host wrote it.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host_header
    }

    /// The socket address the URL names, for a native exchange.
    pub fn addr(&self) -> Result<std::net::SocketAddr> {
        self.host_header
            .parse::<std::net::SocketAddr>()
            .map_err(|e| Error::Session(format!("bad rtsp address {}: {e}", self.host_header)))
    }

    async fn request<X: RtspExchange>(
        &mut self,
        exchange: &mut X,
        method: &str,
        target: &str,
        extra: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<Response> {
        self.cseq += 1;
        let mut head = format!(
            "{method} {target} RTSP/1.0\r\nCSeq: {}\r\nX-GS-ClientVersion: {CLIENT_VERSION}\r\n\
             Host: {}\r\n",
            self.cseq, self.host_header,
        );
        if let Some(session) = &self.session {
            head.push_str(&format!("Session: {session}\r\n"));
        }
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(body) = body {
            head.push_str(&format!(
                "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        head.push_str("\r\n");
        if let Some(body) = body {
            head.push_str(body);
        }

        let raw = exchange.exchange(head.as_bytes()).await?;
        let response = parse(&raw)?;
        if response.status != 200 {
            return Err(Error::Session(format!(
                "{method} refused with RTSP {}",
                response.status
            )));
        }
        Ok(response)
    }

    /// Run the whole handshake and start the streams.
    pub async fn negotiate<X: RtspExchange>(
        &mut self,
        exchange: &mut X,
        want: StreamRequest,
        modern_control: bool,
    ) -> Result<Negotiated> {
        // The control stream's id is generation-specific, and both SETUP and
        // ANNOUNCE must name the same one. A host that binds its control
        // session to the id it was set up under will not find a session
        // announced against a different one — and says nothing about it, so
        // media flows while every control message lands nowhere.
        let control_stream = if modern_control {
            "streamid=control/13/0"
        } else {
            "streamid=control/1/0"
        };
        self.request(
            exchange,
            "OPTIONS",
            &format!("rtsp://{}", self.host_header),
            &[],
            None,
        )
        .await?;

        let describe = self
            .request(
                exchange,
                "DESCRIBE",
                &format!("rtsp://{}", self.host_header),
                &[("Accept", "application/sdp")],
                None,
            )
            .await?;
        let host_sdp = describe.body;
        // The host's own description of what it can encode — the only
        // legitimate specification for its audio layouts. Kept at debug:
        // it is a page of text, wanted exactly when studying a host.
        tracing::debug!(sdp = %host_sdp, "host DESCRIBE sdp");
        for line in host_sdp.lines() {
            if line.to_ascii_lowercase().contains("audio") {
                tracing::info!(line, "host audio capability");
            }
        }

        // Hosts assign the ports and ignore this proposal, but reference
        // clients send it, so it stays.
        let transport = "unicast;X-GS-ClientPort=50000-50001";
        let audio = self
            .request(
                exchange,
                "SETUP",
                "streamid=audio/0/0",
                &[("Transport", transport)],
                None,
            )
            .await?;
        // The session id appears on the first SETUP and must be echoed on
        // every request after it.
        if let Some(session) = audio.header("Session") {
            self.session = Some(
                session
                    .split(';')
                    .next()
                    .unwrap_or(session)
                    .trim()
                    .to_owned(),
            );
        }
        let video = self
            .request(
                exchange,
                "SETUP",
                "streamid=video/0/0",
                &[("Transport", transport)],
                None,
            )
            .await?;
        let control = self
            .request(
                exchange,
                "SETUP",
                control_stream,
                &[("Transport", transport)],
                None,
            )
            .await?;

        let sdp = announce_sdp(&self.host_header, want, &host_sdp);
        self.request(exchange, "ANNOUNCE", control_stream, &[], Some(&sdp))
            .await?;
        self.request(exchange, "PLAY", "/", &[], None).await?;

        Ok(Negotiated {
            video_port: server_port(&video)?,
            audio_port: server_port(&audio)?,
            control_port: server_port(&control)?,
            ping_payload: video
                .header("X-SS-Ping-Payload")
                .or_else(|| audio.header("X-SS-Ping-Payload"))
                .and_then(|p| <[u8; 16]>::try_from(p.as_bytes()).ok()),
            connect_data: control
                .header("X-SS-Connect-Data")
                .and_then(|d| d.trim().parse().ok()),
            encryption_supported: sdp_int(&host_sdp, "x-ss-general.encryptionSupported")
                .unwrap_or(0),
            encryption_requested: sdp_int(&host_sdp, "x-ss-general.encryptionRequested")
                .unwrap_or(0),
            reference_invalidation: sdp_int(&host_sdp, "x-nv-video[0].refPicInvalidation")
                .is_some_and(|v| v != 0),
            surround: (want.channels > 2)
                .then(|| surround_layout(&host_sdp, want.channels))
                .flatten(),
        })
    }
}

/// The host's stated Opus multistream layout for `channels`, from its
/// `a=fmtp:97 surround-params=` lines.
///
/// Two spellings exist. The digit string packs single digits:
/// `85301245673` reads channels 8, streams 5, coupled 3, mapping 01245673 —
/// which cannot express a channel index past 9, hence the newer
/// comma-separated form `12,8,4,0,1,…`. Hosts list a coupled and an
/// uncoupled variant per count; the first matches AudioQuality 0, which is
/// what this client requests.
fn surround_layout(host_sdp: &str, channels: u8) -> Option<gsa_core::media::SurroundLayout> {
    for line in host_sdp.lines() {
        let Some(params) = line.trim().strip_prefix("a=fmtp:97 surround-params=") else {
            continue;
        };
        let layout = if params.contains(',') {
            let mut fields = params.split(',').map(|f| f.trim().parse::<u8>().ok());
            let ch = fields.next().flatten()?;
            let streams = fields.next().flatten()?;
            let coupled = fields.next().flatten()?;
            let mapping: Option<Vec<u8>> = fields.collect();
            gsa_core::media::SurroundLayout {
                channels: ch,
                streams,
                coupled,
                mapping: mapping?,
            }
        } else {
            let digits: Vec<u8> = params
                .chars()
                .map(|c| c.to_digit(10).map(|d| d as u8))
                .collect::<Option<_>>()?;
            if digits.len() < 3 {
                continue;
            }
            gsa_core::media::SurroundLayout {
                channels: digits[0],
                streams: digits[1],
                coupled: digits[2],
                mapping: digits[3..].to_vec(),
            }
        };
        if layout.channels == channels && layout.mapping.len() == usize::from(channels) {
            return Some(layout);
        }
    }
    None
}

fn server_port(response: &Response) -> Result<u16> {
    response
        .header("Transport")
        .and_then(|t| {
            t.split(';')
                .find_map(|part| part.trim().strip_prefix("server_port="))
        })
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| Error::Session("SETUP response carries no server_port".into()))
}

/// Read an integer `a=<name>:<value>` attribute out of an SDP body.
fn sdp_int(sdp: &str, name: &str) -> Option<u32> {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a="))
        .find_map(|l| {
            let rest = l.strip_prefix(name)?;
            rest.trim_start().strip_prefix(':')?.trim().parse().ok()
        })
}

/// Build the SDP that tells the host what to encode.
fn announce_sdp(host: &str, want: StreamRequest, host_sdp: &str) -> String {
    let ip = host.split(':').next().unwrap_or("0.0.0.0");
    // What we announce here is a promise about how we will then behave, and
    // the control channel reads the *same* attribute to decide whether to
    // encrypt. Deriving the two from different inputs is how a client ends up
    // announcing a plaintext session and then sending ciphertext: the host
    // believes the announcement, reads garbage, and drops every control
    // message without a word while video and audio carry on.
    //
    // So: everything the host says it supports is enabled. A host that also
    // asks for a subset via `encryptionRequested` gets that honoured, but its
    // absence means "no preference", not "none" — the reading that caused
    // exactly the failure above against a host that supports encryption and
    // never mentions requesting it.
    let supported = sdp_int(host_sdp, "x-ss-general.encryptionSupported").unwrap_or(0);
    let encryption = match sdp_int(host_sdp, "x-ss-general.encryptionRequested") {
        Some(requested) => supported & requested,
        None => supported,
    };
    let mask: u32 = match want.channels {
        6 => 0x3f,
        8 => 0x63f,
        // 7.1.4: the 7.1 positions plus four height speakers.
        12 => 0x2d63f,
        _ => 0x3,
    };
    format!(
        "v=0\r\n\
         o=android 0 14 IN IPv4 {ip}\r\n\
         s=NVIDIA Streaming Client\r\n\
         t=0 0\r\n\
         a=x-nv-video[0].clientViewportWd:{}\r\n\
         a=x-nv-video[0].clientViewportHt:{}\r\n\
         a=x-nv-video[0].maxFPS:{}\r\n\
         a=x-nv-video[0].packetSize:{}\r\n\
         a=x-nv-video[0].timeoutLengthMs:7000\r\n\
         a=x-nv-video[0].framesWithInvalidRefThreshold:0\r\n\
         a=x-nv-video[0].maxNumReferenceFrames:1\r\n\
         a=x-nv-video[0].videoEncoderSlicesPerFrame:1\r\n\
         a=x-nv-video[0].encoderCscMode:0\r\n\
         a=x-nv-video[0].dynamicRangeMode:{}\r\n\
         a=x-nv-vqos[0].bitStreamFormat:{}\r\n\
         a=x-nv-vqos[0].fec.enable:1\r\n\
         a=x-nv-vqos[0].fec.minRequiredFecPackets:2\r\n\
         a=x-nv-vqos[0].qosTrafficType:5\r\n\
         a=x-nv-vqos[0].bw.maximumBitrateKbps:{}\r\n\
         a=x-nv-vqos[0].drc.enable:0\r\n\
         a=x-ml-video.configuredBitrateKbps:{}\r\n\
         a=x-nv-aqos.packetDuration:5\r\n\
         a=x-nv-aqos.qosTrafficType:4\r\n\
         a=x-nv-audio.surround.enable:{}\r\n\
         a=x-nv-audio.surround.numChannels:{}\r\n\
         a=x-nv-audio.surround.channelMask:{mask}\r\n\
         a=x-nv-audio.surround.AudioQuality:0\r\n\
         a=x-nv-general.useReliableUdp:13\r\n\
         a=x-nv-general.featureFlags:167\r\n\
         a=x-ss-general.encryptionEnabled:{encryption}\r\n\
         m=video 47998\r\n",
        want.width,
        want.height,
        want.fps,
        want.packet_size,
        u8::from(want.hdr),
        want.bitstream_format,
        want.bitrate_kbps,
        want.bitrate_kbps,
        u8::from(want.channels > 2),
        want.channels.max(2),
    )
}

fn parse(raw: &[u8]) -> Result<Response> {
    let text = String::from_utf8_lossy(raw);
    // Hosts mix bare LF with CRLF within one response; normalise before
    // splitting head from body.
    let text = text.replace("\r\n", "\n");
    let (head, body) = text.split_once("\n\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| Error::Session("empty RTSP response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::Session(format!("bad RTSP status line: {status_line}")))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    Ok(Response {
        status,
        headers,
        body: body.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Response, announce_sdp, parse, sdp_int, server_port};

    /// Captured verbatim from the dev host (Apollo 0.4.6).
    const HOST_SDP: &str = "a=x-ss-general.featureFlags:3\n\
         a=x-ss-general.encryptionSupported:5\n\
         a=x-ss-general.encryptionRequested:1\n\
         a=x-nv-video[0].refPicInvalidation:1\n\
         a=rtpmap:98 AV1/90000\n";

    #[test]
    fn reads_the_hosts_capability_attributes() {
        assert_eq!(
            sdp_int(HOST_SDP, "x-ss-general.encryptionSupported"),
            Some(5)
        );
        assert_eq!(
            sdp_int(HOST_SDP, "x-ss-general.encryptionRequested"),
            Some(1)
        );
        assert_eq!(
            sdp_int(HOST_SDP, "x-nv-video[0].refPicInvalidation"),
            Some(1)
        );
        assert_eq!(sdp_int(HOST_SDP, "x-nv-video[0].absent"), None);
    }

    #[test]
    fn only_enables_encryption_the_host_asked_for() {
        // supported=5 (control v2 + audio), requested=1 (control v2); the
        // announced value is the intersection.
        let sdp = announce_sdp("10.0.0.1:48010", request(), HOST_SDP);
        assert!(sdp.contains("a=x-ss-general.encryptionEnabled:1"), "{sdp}");
    }

    #[test]
    fn announces_what_we_asked_to_encode() {
        let sdp = announce_sdp("10.0.0.1:48010", request(), HOST_SDP);
        assert!(sdp.contains("clientViewportWd:1920"));
        assert!(sdp.contains("clientViewportHt:1080"));
        assert!(sdp.contains("maxFPS:60"));
        assert!(sdp.contains("bitStreamFormat:1"));
        assert!(sdp.contains("configuredBitrateKbps:20000"));
    }

    fn request() -> super::StreamRequest {
        super::StreamRequest {
            width: 1920,
            height: 1080,
            fps: 60,
            bitstream_format: 1,
            hdr: false,
            bitrate_kbps: 20_000,
            packet_size: 1392,
            channels: 2,
        }
    }

    #[test]
    fn parses_a_response_with_either_line_ending() {
        let crlf =
            parse(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nSession: DEADBEEF;timeout = 90\r\n\r\nbody")
                .unwrap();
        assert_eq!(crlf.status, 200);
        assert_eq!(crlf.header("session"), Some("DEADBEEF;timeout = 90"));
        assert_eq!(crlf.body, "body");
        let lf = parse(b"RTSP/1.0 200 OK\nCSeq: 1\n\na=x\n").unwrap();
        assert_eq!(lf.status, 200);
        assert_eq!(lf.body, "a=x\n");
    }

    #[test]
    fn takes_the_port_the_host_assigned() {
        let r: Response =
            parse(b"RTSP/1.0 200 OK\r\nTransport: server_port=48100\r\n\r\n").unwrap();
        assert_eq!(server_port(&r).unwrap(), 48100);
        // A response with no server_port must fail rather than fall back to a
        // default port.
        let missing = parse(b"RTSP/1.0 200 OK\r\nTransport: unicast\r\n\r\n").unwrap();
        assert!(server_port(&missing).is_err());
    }
}
