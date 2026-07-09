//! Capture contracts (specs 01/02/09): `RenderSource` is the central
//! abstraction — desktop capture, virtual displays, and emulators are
//! peers behind it. This crate has **zero platform code** and compiles on
//! every OS; platform crates implement the traits.

mod frame;
mod sink;
mod source;

pub use frame::{CpuFrame, GpuFrame, GpuHandle, PlatformFrame, Rect};
pub use sink::{FrameReceiver, FrameSink, frame_channel};
pub use source::{RenderSource, SourceConfig, SourceDescriptor, VirtualDisplay};

pub use gsa_protocol::input::{InputDisposition, InputEvent};
