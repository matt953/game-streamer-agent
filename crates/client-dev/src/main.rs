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
use gsa_protocol::control::SourceInfo;

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
    /// Source to stream: a 1-based index from the source list, or a name
    /// substring (e.g. `2` or `"Display 1"`). Default: the first source.
    #[arg(long)]
    source: Option<String>,
    /// Force the software (openh264) decoder instead of platform hardware.
    #[arg(long)]
    sw_decode: bool,
    /// Headless: decode with the platform HARDWARE decoder (VideoToolbox),
    /// zero-copy — fails rather than falling back to software.
    #[arg(long)]
    hw_decode: bool,
    /// Headless: write a per-frame stage ledger (JSONL) to this path.
    #[arg(long)]
    ledger: Option<std::path::PathBuf>,
    /// Enable server-side ABR for the session (headless; used by the chaos rig).
    #[arg(long)]
    abr: bool,
    /// Request a starting bitrate in Mb/s (headless); default: agent config.
    #[arg(long)]
    bitrate: Option<u32>,
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
            cli.hw_decode,
            cli.ledger,
            cli.abr,
            cli.bitrate.map(|m| m.saturating_mul(1_000_000)),
            auth,
        ))
    } else {
        window::run(cli.connect, cli.source, cli.sw_decode, auth)
    }
}

/// Resolve a `--source` selector against the agent's source list. Accepts a
/// 1-based index or a case-insensitive name substring; `None` picks the first.
/// The raw wire id is deliberately not a selector — it's an internal detail.
pub(crate) fn pick_source<'a>(
    sources: &'a [SourceInfo],
    selector: Option<&str>,
) -> Result<&'a SourceInfo> {
    let Some(sel) = selector else {
        return sources.first().context("agent offers no sources");
    };
    if let Ok(n) = sel.parse::<usize>()
        && (1..=sources.len()).contains(&n)
    {
        return Ok(&sources[n - 1]);
    }
    let needle = sel.to_lowercase();
    sources
        .iter()
        .find(|s| s.name.to_lowercase().contains(&needle))
        .with_context(|| format!("no source matches {sel:?}.\n{}", source_list(sources)))
}

/// Human-readable numbered source list for logs and error messages.
pub(crate) fn source_list(sources: &[SourceInfo]) -> String {
    sources
        .iter()
        .enumerate()
        .map(|(i, s)| format!("  {} — {} [{:?}]", i + 1, s.name, s.kind))
        .collect::<Vec<_>>()
        .join("\n")
}
