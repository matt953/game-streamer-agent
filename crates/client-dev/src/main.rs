//! Developer debug client (spec 01): a thin harness around `client-core`.
//! `--headless` decodes N frames and emits a stats JSON blob (the CI/e2e
//! mode); the default mode opens a window and presents the stream.

mod audio_playback;
mod av1;
mod decoder;
#[cfg(target_os = "macos")]
mod decoder_vt;
mod frame_dump;
mod gamepad_capture;
#[cfg(target_os = "macos")]
mod gamepad_gc;
#[cfg(target_os = "macos")]
mod haptics;
mod hdr_probe;
mod headless;
mod input_capture;
mod moonlight;
mod netsim;
mod overlay;
mod pairing;
mod present;
mod script;
#[cfg(target_os = "macos")]
mod vt_interop;
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
    /// Stream from a Moonlight host (Sunshine/Apollo) instead of a gsa agent:
    /// its cleartext address, e.g. `192.168.1.10:47989`. Pair first with the
    /// backend's `pair` example.
    #[arg(long)]
    moonlight: Option<std::net::SocketAddr>,
    /// App id to launch on the Moonlight host (see the backend's `catalog`).
    #[arg(long, default_value_t = 881_448_767)]
    moonlight_app: u32,
    /// Bitrate to request from the Moonlight host, Mb/s.
    #[arg(long, default_value_t = 20)]
    moonlight_mbps: u32,
    /// Stop after N seconds (0 = until the window closes). Exiting cleanly
    /// matters: a session abandoned mid-stream leaves the host unable to
    /// start the next one.
    #[arg(long, default_value_t = 0)]
    moonlight_seconds: u64,
    /// Give up if no frame has decoded this many seconds into the session
    /// (0 = wait forever).
    ///
    /// A host holding an abandoned session still accepts the connection and
    /// negotiates; it simply never sends a picture. Without this an unattended
    /// run waits out its whole duration on a grey window and then reports
    /// success, so the failure is only found by someone watching the screen.
    #[arg(long, default_value_t = 15)]
    moonlight_first_frame_s: u64,
    /// Drive a synthetic controller for the session: makes the host plug a
    /// virtual pad so its controller-related messages can be observed without
    /// physical hardware.
    #[arg(long)]
    moonlight_synthetic_pad: bool,
    /// Announce the pad as this family instead of what it really is. Hosts
    /// build a different virtual device per family and support different
    /// feedback on each, so this isolates "the host will not send X" from
    /// "the host will not send X *to this kind of pad*".
    #[arg(long, value_parser = ["auto", "xbox", "dualsense", "dualshock", "generic"])]
    moonlight_pad_kind: Option<String>,
    /// Ask the host for HDR. A request, not a guarantee: what actually
    /// arrives is reported per session, since a host may answer in SDR.
    #[arg(long)]
    moonlight_hdr: bool,
    /// The level, in nits, that an HDR stream's full white is shown at on this
    /// SDR window. Hosts differ in what they encode SDR white as, so a picture
    /// that comes out dim or with blown highlights is tuned here.
    #[arg(long, default_value_t = 203.0)]
    hdr_sdr_white_nits: f32,
    /// Stream mode to ask the host for, as `WIDTHxHEIGHT@FPS`, or `auto` to
    /// match this display. Matching is the useful default: a host that can
    /// create a display for the session then renders at the client's real
    /// geometry instead of scaling into it.
    #[arg(long, default_value = "auto")]
    moonlight_mode: String,
    /// Let the host change its desktop resolution to match the request.
    #[arg(long)]
    moonlight_host_mode_change: bool,
    /// Write the first decoded frame here as a BMP. A frame count proves the
    /// decoder accepted the stream; only the pixels prove it decoded it.
    #[arg(long)]
    dump_frame: Option<std::path::PathBuf>,
    /// Add up to this many milliseconds of delay to each frame, varying per
    /// frame, before the pacing code sees it. A LAN is too clean to exercise
    /// de-jitter at all, so this is how a Wi-Fi hop is reproduced on a desk.
    #[arg(long, default_value_t = 0)]
    jitter_ms: u32,
    /// Seed for the jitter, so two runs impose the same link and an A/B
    /// compares one change rather than two different experiments.
    #[arg(long, default_value_t = 1)]
    jitter_seed: u64,
    /// Redraw on every display refresh rather than only when a frame arrives.
    /// Measures repeats honestly, but makes this look like a max-rate client
    /// to a variable-refresh display, which then never slows to the content.
    #[arg(long)]
    chase_refresh: bool,
    /// Run the window full-screen. Apple requires full-screen for
    /// Adaptive-Sync, so a windowed run on a variable-rate display measures a
    /// fixed one — and reads as a VRR result unless you know that.
    #[arg(long)]
    fullscreen: bool,
    /// Let the window be covered by others. Off by default: presentation can
    /// only be measured while the window is actually on a display, and a
    /// covered one silently measures nothing at all.
    #[arg(long)]
    no_float: bool,
    /// The latency-for-smoothness trade, named as other clients name it:
    /// `lowest-latency` shows each frame the moment it decodes; `balanced`
    /// holds up to one frame to absorb jitter; `balanced-fps-limit` also stays
    /// a frame below the display's rate; `smoothest` never drops a frame and
    /// lets latency grow.
    #[arg(long, default_value = "balanced",
          value_parser = ["lowest-latency", "balanced", "balanced-fps-limit", "smoothest"])]
    pacing: String,
    /// Drive the session with a timed sequence instead of a person: waits,
    /// keypresses and screenshots, e.g.
    /// `"30s down down enter 10s down down down 10s r 220s shot"`.
    /// `shot` writes to `--dump-frame`.
    #[arg(long)]
    input_script: Option<String>,
    /// Turn the de-jitter off. Only useful next to `--jitter-ms`: it is the
    /// control half of the experiment, since a smoothing that cannot be
    /// switched off cannot be shown to have done anything.
    #[arg(long)]
    no_dejitter: bool,
    /// How the window presents: `vsync` waits for the display's refresh, the
    /// way a phone or a TV does; `nosync` shows each frame the moment it is
    /// ready. Only `vsync` reproduces what a user's display actually does, so
    /// it is the one to judge pacing under.
    #[arg(long, default_value = "vsync", value_parser = ["vsync", "nosync"])]
    present_mode: String,
    /// Codecs to offer the host, richest first, instead of everything this
    /// machine can decode. Names the negotiation directly, so a host's
    /// support for one codec can be exercised without changing hardware.
    #[arg(long, value_delimiter = ',', value_parser = ["av1", "hevc", "h264"])]
    codecs: Vec<String>,
    /// What ending the session does to the host: `quit` takes the app down
    /// with the stream; `disconnect` leaves it running, and the next run of
    /// the same app rejoins it mid-session.
    #[arg(long, default_value = "quit", value_parser = ["quit", "disconnect"])]
    moonlight_exit: String,
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

    if let Some(host) = cli.moonlight {
        return window::run_moonlight(
            host,
            cli.moonlight_app,
            cli.moonlight_mbps,
            cli.sw_decode,
            cli.moonlight_seconds,
            cli.moonlight_first_frame_s,
            cli.moonlight_synthetic_pad,
            cli.moonlight_pad_kind.as_deref(),
            cli.dump_frame.clone(),
            &cli.codecs,
            &cli.moonlight_mode,
            cli.moonlight_host_mode_change,
            cli.moonlight_hdr,
            decoder::DisplayMapping::new(cli.hdr_sdr_white_nits)?,
            &cli.present_mode,
            netsim::Jitter::new(cli.jitter_ms, cli.jitter_seed),
            gsa_client_core::PacingMode::from_name(&cli.pacing).unwrap_or_default(),
            match cli.input_script.as_deref() {
                Some(text) => Some(script::parse(text).map_err(|e| anyhow::anyhow!(e))?),
                None => None,
            },
            !cli.no_dejitter,
            !cli.no_float,
            cli.chase_refresh,
            cli.fullscreen,
            cli.moonlight_exit == "disconnect",
        );
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
