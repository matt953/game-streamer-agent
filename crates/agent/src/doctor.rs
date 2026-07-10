//! `gsa doctor` — local host-readiness checks (spec 12). Runs without a daemon:
//! reports capture/injection permissions and backend availability so setup
//! problems surface before a session silently misbehaves.

use serde::Serialize;

// `Ok` is only constructed by the macOS/Windows checks; elsewhere host
// support isn't implemented yet, so allow the variant to be unused there.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    level: Level,
    detail: String,
}

/// Run the checks and print a report. Returns the process exit code: non-zero
/// if any check `Fail`ed (scripting-friendly).
#[must_use]
pub fn run(json: bool) -> i32 {
    let checks = collect();
    let failed = checks.iter().any(|c| c.level == Level::Fail);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&checks).unwrap_or_else(|_| "[]".into())
        );
    } else {
        print_human(&checks, failed);
    }
    i32::from(failed)
}

#[cfg(target_os = "macos")]
fn collect() -> Vec<Check> {
    let mut checks = Vec::new();
    macos_checks(&mut checks);
    checks
}

#[cfg(target_os = "windows")]
fn collect() -> Vec<Check> {
    let mut checks = Vec::new();
    windows_checks(&mut checks);
    checks
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn collect() -> Vec<Check> {
    vec![Check {
        name: "host capture",
        level: Level::Warn,
        detail: "platform capture/injection backends land at M4/M5 (spec 11)".into(),
    }]
}

#[cfg(target_os = "macos")]
fn macos_checks(checks: &mut Vec<Check>) {
    // Screen Recording — capture can't run without it.
    if gsa_capture_macos::screen_recording_authorized() {
        checks.push(Check {
            name: "screen recording",
            level: Level::Ok,
            detail: "granted".into(),
        });
    } else {
        checks.push(Check {
            name: "screen recording",
            level: Level::Fail,
            detail: "missing — capture won't produce frames. Grant in System Settings › \
                     Privacy & Security › Screen Recording (see `cargo xtask dev-sign`)"
                .into(),
        });
    }

    // Accessibility — CGEventPost silently no-ops without it.
    match gsa_input::injection_authorized() {
        Some(true) => checks.push(Check {
            name: "accessibility",
            level: Level::Ok,
            detail: "granted".into(),
        }),
        Some(false) => checks.push(Check {
            name: "accessibility",
            level: Level::Fail,
            detail: "missing — input injection silently no-ops. Grant in System Settings › \
                     Privacy & Security › Accessibility (see `cargo xtask dev-sign`)"
                .into(),
        }),
        None => {}
    }

    // Display enumeration — confirms the capture backend actually works.
    match gsa_capture_macos::list_displays() {
        Ok(displays) if !displays.is_empty() => checks.push(Check {
            name: "display capture",
            level: Level::Ok,
            detail: format!("{} display(s) available", displays.len()),
        }),
        Ok(_) => checks.push(Check {
            name: "display capture",
            level: Level::Warn,
            detail: "no displays enumerated".into(),
        }),
        Err(e) => checks.push(Check {
            name: "display capture",
            level: Level::Fail,
            detail: format!("enumeration failed: {e}"),
        }),
    }
}

#[cfg(target_os = "windows")]
fn windows_checks(checks: &mut Vec<Check>) {
    // Windows Graphics Capture — the capture API itself, present since 1903.
    if gsa_capture_windows::capture_supported() {
        checks.push(Check {
            name: "graphics capture",
            level: Level::Ok,
            detail: "supported".into(),
        });
    } else {
        checks.push(Check {
            name: "graphics capture",
            level: Level::Fail,
            detail: "Windows.Graphics.Capture unavailable — needs Windows 10 1903 or later".into(),
        });
    }

    // SendInput needs no grant, but UIPI silently drops injection into a
    // window owned by a higher-integrity process.
    if gsa_input::injection_authorized() == Some(true) {
        checks.push(Check {
            name: "input injection",
            level: Level::Ok,
            detail: "SendInput available; run elevated to control elevated apps".into(),
        });
    }

    // Display enumeration — confirms the capture backend actually works.
    match gsa_capture_windows::list_displays() {
        Ok(displays) if !displays.is_empty() => checks.push(Check {
            name: "display capture",
            level: Level::Ok,
            detail: format!("{} display(s) available", displays.len()),
        }),
        Ok(_) => checks.push(Check {
            name: "display capture",
            level: Level::Warn,
            detail: "no displays enumerated".into(),
        }),
        Err(e) => checks.push(Check {
            name: "display capture",
            level: Level::Fail,
            detail: format!("enumeration failed: {e}"),
        }),
    }

    // System audio. Loopback needs no grant, but a host with no render
    // endpoint (a headless box, some RDP sessions) simply has nothing to tap;
    // the stream stays up, silent.
    match gsa_capture_windows::loopback_mix_format() {
        Ok((rate, channels)) => checks.push(Check {
            name: "system audio",
            level: Level::Ok,
            detail: format!("WASAPI loopback: {rate} Hz, {channels} ch"),
        }),
        Err(e) => checks.push(Check {
            name: "system audio",
            level: Level::Warn,
            detail: format!("no loopback capture — the stream will be silent: {e}"),
        }),
    }

    // Hardware encode. Its absence is not a failure — the software encoder
    // still streams — but it is the difference between ~4 ms and ~83 ms.
    match gsa_encode_nvenc::probe() {
        Some(_) => checks.push(Check {
            name: "hardware encode",
            level: Level::Ok,
            detail: "NVENC available (zero-copy from the capture texture)".into(),
        }),
        None => checks.push(Check {
            name: "hardware encode",
            level: Level::Warn,
            detail: "no NVENC — falling back to the software encoder, which is much slower".into(),
        }),
    }
}

fn print_human(checks: &[Check], failed: bool) {
    println!("gsa doctor — host readiness\n");
    for c in checks {
        let mark = match c.level {
            Level::Ok => "✓",
            Level::Warn => "~",
            Level::Fail => "✗",
        };
        println!("  {mark}  {:<18}{}", c.name, c.detail);
    }
    println!();
    if failed {
        println!("✗ not ready — resolve the failing check(s) above and re-run.");
    } else {
        println!("✓ ready.");
    }
}
