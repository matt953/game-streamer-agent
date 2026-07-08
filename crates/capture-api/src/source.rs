//! The `RenderSource` trait (spec 09, authoritative definition trimmed to
//! the M0 surface; `audio()` and `command()` land with their subsystems).

use gsa_core::Result;
use gsa_core::media::VideoMode;
use gsa_protocol::control::{SourceInfo, SourceKind};
use gsa_protocol::input::{InputDisposition, InputEvent};

use crate::sink::FrameSink;

/// Requested configuration for a starting/reconfiguring source.
#[derive(Debug, Clone, Copy)]
pub struct SourceConfig {
    pub mode: VideoMode,
}

/// Rich descriptor; `SourceInfo` is its wire projection.
#[derive(Debug, Clone)]
pub struct SourceDescriptor {
    pub info: SourceInfo,
    /// Modes the source can natively produce; empty = anything (synthetic).
    pub modes: Vec<VideoMode>,
}

impl SourceDescriptor {
    #[must_use]
    pub fn kind(&self) -> SourceKind {
        self.info.kind
    }
}

/// Anything that produces frames: desktop capture, virtual display,
/// emulator, test pattern. The encode pipeline does not care which.
pub trait RenderSource: Send {
    fn descriptor(&self) -> SourceDescriptor;

    /// Begin producing frames into `sink` from whatever thread/callback
    /// the backend uses. Must be non-blocking (spawn internally).
    fn start(&mut self, cfg: SourceConfig, sink: FrameSink) -> Result<()>;

    /// Route an input event. `Consumed` events must never reach the OS.
    fn handle_input(&mut self, event: InputEvent) -> InputDisposition;

    /// Live mode change (client rotation, ABR resolution step, ...).
    fn reconfigure(&mut self, cfg: SourceConfig) -> Result<()>;

    /// Stop producing and release resources. Idempotent.
    fn stop(&mut self) -> Result<()>;
}

/// Virtual display provisioning (spec 02). No implementations at M0;
/// the trait exists so `session` code is written against it from day one.
pub trait VirtualDisplay: Send {
    /// Create a virtual display with the given mode; returns its source.
    fn create(&mut self, mode: VideoMode) -> Result<Box<dyn RenderSource>>;
    /// Tear down the display. Idempotent.
    fn destroy(&mut self) -> Result<()>;
}
