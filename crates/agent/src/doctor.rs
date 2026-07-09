//! `gsa doctor` — local host-readiness checks (spec 12). Runs without a daemon:
//! reports capture/injection permissions and backend availability so setup
//! problems surface before a session silently misbehaves.

use serde::Serialize;

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

fn collect() -> Vec<Check> {
    let mut checks = Vec::new();
    #[cfg(target_os = "macos")]
    macos_checks(&mut checks);
    #[cfg(not(target_os = "macos"))]
    checks.push(Check {
        name: "host capture",
        level: Level::Warn,
        detail: "platform capture/injection backends land at M4/M5 (spec 11)".into(),
    });
    checks
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
