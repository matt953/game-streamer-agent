//! Developer debug client (spec 01): a thin harness around `client-core`.
//! `--headless` decodes N frames and emits a stats JSON blob (the CI/e2e
//! mode); the default mode opens a window and presents the stream.

mod audio_playback;
mod decoder;
#[cfg(target_os = "macos")]
mod decoder_vt;
mod gamepad_capture;
mod headless;
mod input_capture;
mod pairing;
mod window;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "gsa-client-dev", version, about = "Debug streaming client")]
struct Cli {
    /// Agent address.
    #[arg(long, default_value = "127.0.0.1:47420")]
    connect: std::net::SocketAddr,
    /// Decode N frames headlessly and print stats instead of presenting.
    #[arg(long)]
    headless: bool,
    /// Frame count for --headless.
    #[arg(long, default_value_t = 300)]
    frames: u32,
    /// Emit machine-readable JSON (headless mode).
    #[arg(long)]
    json: bool,
    /// Source id to stream (default: the agent's first source).
    #[arg(long)]
    source: Option<u32>,
    /// Force the software (openh264) decoder instead of platform hardware.
    #[arg(long)]
    sw_decode: bool,
    /// Pair with the agent instead of streaming: enter the code from `gsa pair`.
    #[arg(long)]
    pair: bool,
    /// Pairing code (with --pair).
    #[arg(long)]
    code: Option<String>,
    /// Name recorded on the agent when pairing.
    #[arg(long, default_value = "gsa-client-dev")]
    name: String,
}

fn main() -> Result<()> {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if cli.pair {
        let code = cli.code.as_deref().context("--pair requires --code")?;
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(pairing::run_pair(cli.connect, code, &cli.name));
    }

    let auth = pairing::load_auth()?;
    if cli.headless {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(headless::run(
            cli.connect,
            cli.frames,
            cli.json,
            cli.source,
            cli.sw_decode,
            auth,
        ))
    } else {
        window::run(cli.connect, cli.source, cli.sw_decode, auth)
    }
}
