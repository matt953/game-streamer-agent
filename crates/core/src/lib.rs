//! Shared types for the game-streamer-agent workspace.
//!
//! This crate contains no I/O and no platform code. It compiles everywhere
//! and is linked by the agent, clients, and (eventually) the media server.

pub mod config;
pub mod error;
pub mod id;
pub mod media;
pub mod pattern;
pub mod time;

pub use error::{Error, Result};
