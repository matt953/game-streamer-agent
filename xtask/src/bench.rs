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

/// Menu navigation by pointer position, with real time between the steps.
///
/// Both menus *wrap*, so over-pressing an arrow key reaches no edge to count
/// from — six Ups over five selectable entries is a net one Up, which walked
/// two runs into Challenge Tombs. And the starting selection follows the
/// mouse, so counting from it has no fixed origin either. Clicking where an
/// item is drawn depends on neither.
///
/// The gaps are generous on purpose. A click sent too soon after a pointer
/// move lands at the *old* position: one run opened MOUSE instead of DISPLAY
/// AND GRAPHICS because y=0.41 is OPTIONS in the main menu and MOUSE in the
/// submenu, and the move had not been applied yet. That race is why an
/// identical script worked minutes earlier and then did not.
///
/// `[R] RUN BENCHMARK` is a footer action of the Display and Graphics page and
/// does nothing until that page is open. The pause before it is the window for
/// setting resolution and refresh by hand, which the game keeps rather than
/// taking from the stream.
const NAVIGATION: &str = "50s at:0.13,0.41 click 20s at:0.13,0.357 click 30s r";

/// The benchmark itself, then a frame of the results.
///
/// The benchmark's own running time, no longer than it needs to be. It was
/// raised to 320 s while the harness was capped at 85 fps by a CPU colour
/// conversion and everything ran late; with that gone the run is back to its
/// normal length and the extra wait was dead time at the end of every run.
const BENCHMARK: &str = "200s shot";

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
        // The geometry of the *stream*, which is not the geometry the game
        // renders at: the game keeps its own resolution setting and is
        // downscaled into whatever is asked for here. So this does not lower
        // the host's load — only changing the game's own setting does. Asking
        // for less than the game renders just adds a downscale.
        .args(["--moonlight-mode", "1920x1080@120"])
        // Enough headroom that the encoder, not the ceiling, decides the
        // bitrate. At 1440p120 a 20 Mb/s cap is the thing being measured.
        .args(["--moonlight-mbps", "100"])
        // A request, not a guarantee — what actually arrives is reported per
        // session and is part of what this run is checking.
        .arg("--moonlight-hdr")
        // Longer than the script, so the session outlives it rather than
        // cutting the results frame off at the end.
        .args(["--moonlight-seconds", "400"])
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

    // A host that reports itself free can still be winding the previous
    // session down, and one that is still holding it accepts the connection
    // and then sends nothing. The client gives up on that in seconds, so the
    // cheap answer is to clear the host properly and go again rather than
    // hand a person a grey screen and ask them to run a command.
    if std::fs::read_to_string(&log).is_ok_and(|t| t.contains("no video")) {
        println!("no video — clearing the host and retrying once…");
        clear_host(host)?;
        let mut handle =
            std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
        let status = client
            .stdout(handle.try_clone().context("clone log handle")?)
            .stderr(handle.try_clone().context("clone log handle")?)
            .status()
            .context("run gsa-client-dev")?;
        let _ = &mut handle;
        if !status.success() {
            bail!("the client exited with {status}; see {}", log.display());
        }
    }

    report(out, &log)
}

/// Resume the host's stranded session and quit it cleanly.
///
/// Killing it is what leaves the next attempt with no picture, so this goes
/// through the same path a real client would.
fn clear_host(host: std::net::SocketAddr) -> Result<()> {
    let _ = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--release",
            "-p",
            "gsa-backend-moonlight",
            "--example",
            "launch",
            "--",
        ])
        .arg(host.to_string())
        .arg(SOTR_APP_ID.to_string())
        .output()
        .context("clear the host's session")?;
    // The host reports itself free before it actually is, so the wait is not
    // optional — this is the race that produced the grey screens.
    std::thread::sleep(std::time::Duration::from_secs(20));
    Ok(())
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

        // No arrow keys. Both menus wrap, so a count of Downs has no edge to
        // start from and lands somewhere different every run.
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
        assert!(script.contains("200s shot"));
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
