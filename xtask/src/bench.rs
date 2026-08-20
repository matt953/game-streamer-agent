//! Run a game's own benchmark over a Moonlight session, unattended.
//!
//! The point is a *real* workload: a 60 fps game rendering for four minutes
//! exercises decode, pacing and presentation in a way a desktop or a film
//! cannot. Doing it by hand takes five minutes of somebody's attention and is
//! not repeatable, so the whole sequence lives here — host readiness, the
//! keystrokes through the menus, the screenshots, and the summary.
//!
//! The menu navigation is not obvious and each mistake costs a five-minute
//! run, so the reasons are recorded next to the steps that need them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Shadow of the Tomb Raider's benchmark, as published by the host.
const SOTR_APP_ID: u32 = 1_825_961_046;

/// Menu navigation by pointer position, because no count of keypresses is
/// reliable here.
///
/// Both menus *wrap*, so over-pressing Up reaches no edge to count from — six
/// Ups over the five selectable main-menu entries is a net one Up, which is a
/// different item every time. And the starting selection follows the mouse, so
/// there is no fixed origin either. A keyboard sequence is right by luck and
/// silently walks into Challenge Tombs the run after; two runs did exactly
/// that and were reported as benchmark results.
///
/// Clicking where an item is drawn depends on neither. `[R] Run Benchmark` is
/// still a footer action of the Display and Graphics page, so it is pressed
/// only once that page is open.
const NAVIGATION: &str = "30s at:0.13,0.41 click 8s at:0.13,0.357 click 10s r";

/// The benchmark itself, then a frame of the results.
const BENCHMARK: &str = "220s shot";

pub fn bench(
    host: std::net::SocketAddr,
    out: &Path,
    codec: &str,
    pacing: &str,
    extra: &[String],
) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;

    ensure_host_free(host)?;

    let script = format!("{NAVIGATION} {BENCHMARK}");
    println!("running the benchmark (about five minutes)…");
    let mut client = Command::new(env!("CARGO"));
    client
        .args(["run", "--quiet", "--release", "-p", "gsa-client-dev", "--"])
        .arg("--moonlight")
        .arg(host.to_string())
        .args(["--moonlight-app", &SOTR_APP_ID.to_string()])
        .args(["--codecs", codec])
        .args(["--moonlight-mode", "1920x1080@60"])
        // Longer than the script, so the session outlives it rather than
        // cutting the results frame off at the end.
        .args(["--moonlight-seconds", "300"])
        .args(["--present-mode", "vsync"])
        .args(["--pacing", pacing])
        .arg("--dump-frame")
        .arg(out.join("base.bmp"))
        .args(["--input-script", &script])
        .args(extra);
    let log = out.join("run.log");
    let handle =
        std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let status = client
        .stdout(handle.try_clone().context("clone log handle")?)
        .stderr(handle)
        .status()
        .context("run gsa-client-dev")?;
    if !status.success() {
        bail!("the client exited with {status}; see {}", log.display());
    }

    report(out, &log)
}

/// Refuse to start against a host that is still holding a session.
///
/// A stranded session is resumed rather than relaunched, which handshakes
/// and then never sends a frame — a grey window that looks like a client
/// fault. Better to say so than to produce five minutes of nothing.
fn ensure_host_free(host: std::net::SocketAddr) -> Result<()> {
    let out = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--release",
            "-p",
            "gsa-backend-moonlight",
            "--example",
            "catalog",
            "--",
        ])
        .arg(host.to_string())
        .output()
        .context("ask the host what it is doing")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("SUNSHINE_SERVER_BUSY") {
        bail!(
            "the host is still holding a session — clear it with:\n  \
             cargo run -q --release -p gsa-backend-moonlight --example launch -- {host} {SOTR_APP_ID}\n\
             {}",
            text.trim()
        );
    }
    Ok(())
}

/// Convert the results frame and print what the run measured.
fn report(out: &Path, log: &Path) -> Result<()> {
    let shots = newest_shot(out)?;
    let png = out.join("results.png");
    // `sips` ships with macOS; a BMP is awkward to look at and the results
    // frame is the artifact a person actually reads.
    let converted = Command::new("sips")
        .args(["-s", "format", "png"])
        .arg(&shots)
        .arg("--out")
        .arg(&png)
        .output();
    match converted {
        Ok(o) if o.status.success() => println!("results frame: {}", png.display()),
        _ => println!("results frame: {} (convert it yourself)", shots.display()),
    }

    // The last stats line says whether the stream itself held up, which is
    // the half of the answer the game's own results panel cannot give.
    let text = std::fs::read_to_string(log).with_context(|| format!("read {}", log.display()))?;
    let stats = text.lines().rev().find(|l| l.contains("stream stats"));
    if let Some(stats) = stats {
        println!("stream: {}", strip_ansi(stats));
    }
    println!("every step: {}", out.display());

    // Deliberately not decided here. Two runs sat in the wrong submenu for
    // five minutes and produced clean statistics and a plausible screenshot;
    // nothing in the stream distinguished them from a benchmark, and every
    // threshold that would have is guesswork until there is a known-good run
    // to calibrate against. Naming the frame to check is honest; an
    // unvalidated verdict would be the same mistake wearing a green tick.
    println!(
        "CHECK before trusting these numbers: the frame after the second click \
         must be the Display and Graphics page, and the last frame must be the \
         benchmark's results panel."
    );
    Ok(())
}

/// The last screenshot the script wrote, which is the results frame.
fn newest_shot(dir: &Path) -> Result<PathBuf> {
    let mut shots: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("step-") && n.ends_with(".bmp"))
        })
        .collect();
    shots.sort();
    shots
        .pop()
        .context("the script wrote no screenshots — see run.log")
}

/// Log lines carry colour codes; they are noise in a summary.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for skip in chars.by_ref() {
                if skip == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The navigation is the part that took five failed runs to get right, and
    /// the rule it encodes is that arrow keys cannot be trusted in these menus
    /// at all — both wrap, and the selection starts wherever the mouse left it.
    #[test]
    fn the_navigation_clicks_rather_than_counting_keypresses() {
        let script = format!("{NAVIGATION} {BENCHMARK}");
        let steps = script.split_whitespace().collect::<Vec<_>>();

        // No arrow keys anywhere. This is the whole fix: two runs walked into
        // Challenge Tombs on a sequence that had worked before, because six
        // Ups over five wrapping entries is a net one Up.
        for arrow in ["up", "down", "left", "right"] {
            assert!(
                !steps.contains(&arrow),
                "{arrow} cannot be relied on in a menu that wraps"
            );
        }

        // Two clicks, each preceded by the position it is aimed at, so neither
        // fires wherever the pointer happened to be left.
        let clicks: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == "click")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(clicks.len(), 2, "Options, then Display and Graphics");
        for click in clicks.iter().copied() {
            assert!(
                steps[click - 1].starts_with("at:"),
                "every click must be aimed first"
            );
        }

        // R is a footer action of the Display and Graphics page, so it comes
        // after the click that opens it.
        let r = steps.iter().position(|s| *s == "r").expect("the r key");
        assert!(r > clicks[1], "R only works once that page is open");

        // And the benchmark's own running time is waited out before the shot.
        assert!(script.contains("220s shot"));
    }

    #[test]
    fn colour_codes_are_stripped_from_the_summary() {
        assert_eq!(
            strip_ansi("\u{1b}[32m INFO\u{1b}[0m frames=120"),
            " INFO frames=120"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
