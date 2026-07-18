//! Build/CI tooling (spec 01: no shell scripts — CI YAML calls these).
//! `cargo xtask ci-e2e` runs the loopback pipeline and asserts on it; the
//! JSON report it writes is the latency-ledger artifact (spec 13).

mod logs;
mod shaper;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "Repo tooling; run via `cargo xtask <cmd>`")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Storm gate: e2e through the shaper with cellular-style loss bursts;
    /// asserts the recovery ladder (NACK+FEC+hold) keeps frames alive.
    CiStorm {
        /// Frames to stream.
        #[arg(long, default_value_t = 600)]
        frames: u32,
    },
    /// End-to-end loopback test: agent + headless client, assertions,
    /// latency report artifact.
    CiE2e {
        /// Frames to stream.
        #[arg(long, default_value_t = 300)]
        frames: u32,
        /// Where to write the JSON latency report.
        #[arg(long, default_value = "target/e2e-report.json")]
        report: PathBuf,
    },
    /// macOS: code-sign the built debug binaries with a stable identity so
    /// Screen Recording / Accessibility grants survive rebuilds.
    /// Dev log collector: receives the streams pushed by an agent running
    /// with GSA_LOG_SINK (and, automatically, its debug-build clients).
    Logs {
        /// Address to listen on.
        #[arg(long, default_value = "0.0.0.0:9600")]
        listen: String,
    },
    /// Dev link shaper: relay UDP to `--to` with delay/jitter/rate/loss.
    Shape {
        #[arg(long)]
        to: std::net::SocketAddr,
        #[arg(long, default_value_t = 200_000)]
        rate_kbit: u32,
        #[arg(long, default_value_t = 8)]
        delay_ms: u32,
        #[arg(long, default_value_t = 2)]
        jitter_ms: u32,
        #[arg(long, default_value_t = 0.0)]
        loss_pct: f64,
        /// Storm mode: drop everything for this many ms per interval.
        #[arg(long, default_value_t = 0)]
        burst_ms: u64,
        #[arg(long, default_value_t = 2000)]
        burst_interval_ms: u64,
    },
    DevSign {
        /// Signing identity (default: the first Apple Development identity).
        #[arg(long)]
        identity: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::CiE2e { frames, report } => ci_e2e(frames, &report),
        Cmd::CiStorm { frames } => ci_storm(frames),
        Cmd::Logs { listen } => logs::logs(&listen),
        Cmd::Shape {
            to,
            rate_kbit,
            delay_ms,
            jitter_ms,
            loss_pct,
            burst_ms,
            burst_interval_ms,
        } => {
            let shaper = shaper::Shaper::start(
                to,
                &shaper::Shaping {
                    rate_kbit,
                    delay_ms,
                    jitter_ms,
                    loss_pct,
                    burst_ms,
                    burst_interval_ms,
                    buffer_pkts: Some(2048),
                },
                1,
            )?;
            eprintln!("[shape] front {} -> {to}", shaper.front);
            // Grace period so the session connects cleanly, then impair.
            std::thread::sleep(Duration::from_secs(3));
            shaper.activate();
            eprintln!("[shape] impairment active");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        Cmd::DevSign { identity } => dev_sign(identity),
    }
}

/// Code-sign debug binaries with a stable identity (macOS TCC stability).
#[cfg(target_os = "macos")]
fn dev_sign(identity: Option<String>) -> Result<()> {
    let identity = match identity {
        Some(id) => id,
        None => first_apple_dev_identity()?,
    };
    eprintln!("[dev-sign] signing with: {identity}");
    for path in [
        "target/debug/gsa",
        "target/debug/gsa-client-dev",
        "target/release/gsa",
        "target/release/gsa-client-dev",
    ] {
        if !std::path::Path::new(path).exists() {
            eprintln!("[dev-sign] skip {path} (build it first)");
            continue;
        }
        let status = Command::new("codesign")
            .args(["--force", "--sign", &identity, "--timestamp=none", path])
            .status()
            .context("run codesign")?;
        if !status.success() {
            bail!("codesign failed for {path}");
        }
        eprintln!("[dev-sign] signed {path}");
    }
    eprintln!("[dev-sign] done — TCC grants now persist across rebuilds of these binaries");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn dev_sign(_identity: Option<String>) -> Result<()> {
    bail!("dev-sign is macOS-only")
}

#[cfg(target_os = "macos")]
fn first_apple_dev_identity() -> Result<String> {
    let out = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .context("run security find-identity")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find(|l| l.contains("Apple Development") || l.contains("Developer ID"))
        .and_then(|l| l.split('"').nth(1))
        .map(String::from)
        .context("no Apple Development signing identity found (open Xcode → Settings → Accounts)")
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ci_storm(frames: u32) -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    eprintln!("[ci-storm] building agent + client...");
    let status = Command::new(&cargo)
        .args(["build", "-p", "gsa-agent", "-p", "gsa-client-dev"])
        .status()
        .context("cargo build")?;
    if !status.success() {
        bail!("build failed");
    }
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let addr = format!("127.0.0.1:{port}");
    let socket = control_socket_for_test(port);
    let exe = |name: &str| {
        let mut p = PathBuf::from("target/debug").join(name);
        if cfg!(windows) {
            p.set_extension("exe");
        }
        p
    };
    eprintln!("[ci-storm] starting agent on {addr}...");
    let agent = KillOnDrop(
        Command::new(exe("gsa"))
            .args(["run", "--listen", &addr, "--control-socket", &socket])
            .env("RUST_LOG", "info")
            .env("GSA_DEV_OPEN", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn agent")?,
    );
    std::thread::sleep(Duration::from_millis(1500));

    // Cellular-style link: 30 Mb/s, 20 ms, storms of 150 ms every 2 s plus
    // 2% background loss. The recovery ladder must keep frames alive.
    let shaper = shaper::Shaper::start(
        addr.parse()?,
        &shaper::Shaping {
            rate_kbit: 30_000,
            delay_ms: 20,
            // Above the de-jitter engage threshold: the pacing path must be
            // exercised, not just the loss ladder.
            jitter_ms: 15,
            loss_pct: 2.0,
            burst_ms: 150,
            burst_interval_ms: 2_000,
            buffer_pkts: Some(2048),
        },
        7,
    )?;
    let front = shaper.front;
    let sh_activate = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        shaper.activate();
        eprintln!("[ci-storm] storm active");
        shaper
    });

    eprintln!("[ci-storm] streaming {frames} frames through {front}...");
    let out = Command::new(exe("gsa-client-dev"))
        .args([
            "--connect",
            &front.to_string(),
            "--headless",
            "--json",
            "--frames",
            &frames.to_string(),
        ])
        .env("GSA_DEV_OPEN", "1")
        .stderr(Stdio::inherit())
        .output()
        .context("run client")?;
    let _shaper = sh_activate.join().expect("shaper thread");
    drop(agent);
    if !out.status.success() {
        bail!("client failed with {}", out.status);
    }
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parse client JSON")?;
    let decoded = report["frames_decoded"].as_u64().unwrap_or(0);
    let dropped = report["frames_dropped_incomplete"]
        .as_u64()
        .unwrap_or(u64::MAX);
    let recovered = report["frames_recovered"].as_u64().unwrap_or(0);
    let regressions = report["marker_regressions"].as_u64().unwrap_or(u64::MAX);
    eprintln!(
        "[ci-storm] decoded {decoded} dropped {dropped} recovered {recovered} regressions {regressions}"
    );
    if decoded < u64::from(frames) {
        bail!("decoded {decoded} < requested {frames}");
    }
    if dropped > u64::from(frames) / 50 {
        bail!("storm dropped {dropped} frames (> 2%)");
    }
    if regressions > 0 {
        bail!("marker regressions under storm: {regressions}");
    }
    eprintln!("[ci-storm] OK — dropped {dropped}, recovered {recovered}");
    Ok(())
}

fn ci_e2e(frames: u32, report_path: &PathBuf) -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    eprintln!("[ci-e2e] building agent + client...");
    let status = Command::new(&cargo)
        .args(["build", "-p", "gsa-agent", "-p", "gsa-client-dev"])
        .status()
        .context("cargo build")?;
    if !status.success() {
        bail!("build failed");
    }

    // A free UDP port: bind TCP :0 and reuse the number (racy in theory,
    // fine for CI in practice).
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let addr = format!("127.0.0.1:{port}");
    let socket = control_socket_for_test(port);

    let exe = |name: &str| {
        let mut p = PathBuf::from("target/debug").join(name);
        if cfg!(windows) {
            p.set_extension("exe");
        }
        p
    };

    eprintln!("[ci-e2e] starting agent on {addr}...");
    let agent = KillOnDrop(
        Command::new(exe("gsa"))
            .args(["run", "--listen", &addr, "--control-socket", &socket])
            .env("RUST_LOG", "info")
            // Dev-open: skip pairing so the e2e connects anonymously.
            .env("GSA_DEV_OPEN", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn agent")?,
    );
    std::thread::sleep(Duration::from_millis(1500));

    eprintln!("[ci-e2e] streaming {frames} frames...");
    let out = Command::new(exe("gsa-client-dev"))
        .args([
            "--connect",
            &addr,
            "--headless",
            "--json",
            "--frames",
            &frames.to_string(),
        ])
        .env("GSA_DEV_OPEN", "1")
        .stderr(Stdio::inherit())
        .output()
        .context("run client")?;
    if !out.status.success() {
        bail!("client failed with {}", out.status);
    }
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parse client JSON")?;

    // While the session is closing on the agent, also smoke the admin API.
    let status_out = Command::new(exe("gsa"))
        .args(["status", "--json", "--control-socket", &socket])
        .output()
        .context("gsa status")?;
    if !status_out.status.success() {
        bail!("gsa status failed");
    }
    drop(agent);

    // Assertions (spec 13 Tier 1).
    let decoded = report["frames_decoded"].as_u64().unwrap_or(0);
    let complete = report["frames_complete"].as_u64().unwrap_or(0);
    let regressions = report["marker_regressions"].as_u64().unwrap_or(u64::MAX);
    let markers = report["marker_read_ok"].as_u64().unwrap_or(0);
    let p50 = report["latency_ms_p50"].as_f64();

    let min_decoded = u64::from(frames) * 99 / 100;
    if decoded < min_decoded {
        bail!("only {decoded}/{frames} frames decoded (need ≥{min_decoded})");
    }
    if regressions != 0 {
        bail!("{regressions} marker regressions (frames went backwards)");
    }
    if markers < decoded * 95 / 100 {
        bail!("markers read on only {markers}/{decoded} frames");
    }
    if complete < decoded {
        bail!("decoded {decoded} > completed {complete}: accounting bug");
    }
    let Some(p50) = p50 else {
        bail!("no latency measurements")
    };
    // Generous ceiling: catches pipeline queueing bugs (seconds), not
    // slow-runner noise. The trend ledger is the real regression gate.
    if p50 > 500.0 {
        bail!("glass-to-glass p50 {p50} ms — pipeline is queueing somewhere");
    }

    if let Some(dir) = report_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(report_path, serde_json::to_string_pretty(&report)?)?;
    eprintln!(
        "[ci-e2e] OK — {decoded} frames, p50 {p50} ms; report at {}",
        report_path.display()
    );
    Ok(())
}

/// Run each scenario through the userspace shaper and assert the pipeline stays
/// playable with ABR on.
fn control_socket_for_test(port: u16) -> String {
    #[cfg(unix)]
    {
        // Keep it short: UDS paths cap at ~104 bytes on macOS.
        format!("/tmp/gsa-e2e-{port}.sock")
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\gsa-e2e-{port}")
    }
}
