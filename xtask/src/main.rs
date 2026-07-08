//! Build/CI tooling (spec 01: no shell scripts — CI YAML calls these).
//! `cargo xtask ci-e2e` runs the loopback pipeline and asserts on it; the
//! JSON report it writes is the latency-ledger artifact (spec 13).

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
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Cmd::CiE2e { frames, report } => ci_e2e(frames, &report),
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
