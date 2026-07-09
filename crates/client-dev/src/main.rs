//! Developer debug client (spec 01): a thin harness around `client-core`.
//! `--headless` decodes N frames and emits a stats JSON blob (the CI/e2e
//! mode); the default mode opens a window and presents the stream.

mod decoder;
mod headless;
mod window;

use anyhow::Result;
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
    if cli.headless {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(headless::run(cli.connect, cli.frames, cli.json, cli.source))
    } else {
        window::run(cli.connect, cli.source)
    }
}
