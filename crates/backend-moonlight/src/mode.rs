//! What a session asks for and what a launch hands back — the same on every
//! transport.

use serde::{Deserialize, Serialize};

/// What we ask the host to encode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StreamMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Let the host change its desktop resolution to match. Off by default.
    pub allow_host_mode_change: bool,
    pub hdr: bool,
    /// Speaker count we can render. 2 is stereo.
    pub channels: u8,
    /// Play audio on the host instead of streaming it to this client.
    ///
    /// The wire flag's real meaning, learned the hard way: one host treated
    /// it leniently and streamed audio anyway, which made "keep the host's
    /// speakers too" look like what it did — until a strict host honoured it
    /// and the client went silent. Off by default: a streaming client wants
    /// the audio.
    pub keep_host_audio: bool,
}

impl Default for StreamMode {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            allow_host_mode_change: false,
            hdr: false,
            channels: 2,
            keep_host_audio: false,
        }
    }
}

impl StreamMode {
    /// Channel count in the low half, channel mask in the high half — the
    /// packing the launch endpoint expects.
    #[must_use]
    pub fn surround_audio_info(self) -> u32 {
        let mask: u32 = match self.channels {
            6 => 0x3f,
            8 => 0x63f,
            // 7.1.4: the 7.1 positions plus four height speakers.
            12 => 0x2d63f,
            _ => 0x3,
        };
        (mask << 16) | u32::from(self.channels.max(2))
    }
}

/// A stream the host has started for us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchedSession {
    /// Where to run the RTSP handshake.
    pub rtsp_url: String,
    /// AES key for the control channel, chosen by us at launch.
    pub riaes_key: [u8; 16],
    /// Identifies that key; also feeds the control channel's nonces.
    pub riaes_key_id: i32,
    /// The one-time token that joins a browser's tunnel to this session,
    /// issued by altc's web launch. Absent for a host reached over the
    /// network, where the sockets bind the session themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_token: Option<Vec<u8>>,
}
