//! Session orchestration (specs 01/05/12): assembles source → encode →
//! transport pipelines per connection, speaks the control protocol, and
//! exposes the local admin API (`ControlService`).

pub mod admin;
pub mod pipeline;
pub mod service;
pub mod state;

pub use pipeline::PipelineHandle;
pub use service::{EncoderFactory, SourceFactory, serve_connection};
pub use state::AgentState;
