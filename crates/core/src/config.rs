//! Agent configuration (TOML file + CLI overrides; spec 01).

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::media::VideoMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// UDP address the QUIC endpoint listens on.
    pub listen: SocketAddr,
    /// Local admin socket path (UDS path on unix, pipe name on Windows).
    /// `None` = platform default.
    pub control_socket: Option<PathBuf>,
    pub video: VideoConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub mode: VideoMode,
    pub bitrate_bps: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:47420".parse().expect("valid literal"),
            control_socket: None,
            video: VideoConfig::default(),
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        // Mode is the fallback for sources with no native mode (test
        // pattern); real displays stream at native resolution, so the
        // bitrate default targets 1080p60.
        Self {
            mode: VideoMode {
                width: 1280,
                height: 720,
                fps: 60,
            },
            bitrate_bps: 15_000_000,
        }
    }
}

/// Platform-default control socket path/name.
#[must_use]
pub fn default_control_socket() -> PathBuf {
    #[cfg(unix)]
    {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join("gsa-control.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\gsa-control")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.video.mode.fps, 60);
        assert!(cfg.listen.is_ipv4());
    }
}
