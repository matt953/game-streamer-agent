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
    /// Host bitrate cap (bits/s); `None` = unlimited up to the protocol max.
    pub bitrate_bps: Option<u32>,
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
        // pattern); real displays stream at native resolution. Default
        // bitrate targets 1080p60 desktop/text (sharp text wants generous
        // bits; tune with `--bitrate`).
        Self {
            mode: VideoMode {
                width: 1280,
                height: 720,
                fps: 60,
            },
            bitrate_bps: None,
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

/// Platform data directory for persistent agent state (identity, peer store).
/// `~/Library/Application Support/gsa` (macOS), `%APPDATA%\gsa` (Windows),
/// `$XDG_DATA_HOME`/`~/.local/share/gsa` (Linux).
#[must_use]
pub fn data_dir() -> PathBuf {
    data_base_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("gsa")
}

#[cfg(target_os = "macos")]
fn data_base_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"))
}

#[cfg(target_os = "windows")]
fn data_base_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn data_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
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
