//! Session orchestration (specs 01/05/12): assembles source → encode →
//! transport pipelines per connection, speaks the control protocol, and
//! exposes the local admin API (`ControlService`).

pub mod abr;
pub mod admin;
pub mod audio_pipeline;
#[allow(dead_code)] // consumed when the v2 controller replaces v1
pub(crate) mod bwe;
pub mod pairing;
pub mod pipeline;
pub mod service;
pub mod state;

pub use pairing::{PairingPoll, PairingState, serve_pairing};
pub use pipeline::PipelineHandle;
pub use service::{EncoderFactory, SourceFactory, serve_connection};
pub use state::AgentState;
