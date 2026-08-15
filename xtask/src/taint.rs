//! Taint audit for shipped artifacts (spec 16, clean-room rules).
//!
//! Scans binaries for strings naming a GPL/AGPL project we may not derive
//! from. A hit is not proof of infringement, but it is the first thing an
//! adversary greps for, and log strings carrying another project's file paths
//! are the usual source.
//!
//! The protocol *name* is not taint: `moonlight` alone matches our own crate
//! and symbol names, so the needles are project identifiers with no innocent
//! reason to appear.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Project identifiers that must never appear in a shipped artifact.
///
/// Lowercase; matching is case-insensitive.
const NEEDLES: &[&str] = &[
    "moonlight-common",
    "moonlight-ios",
    "moonlight-android",
    "moonlight-qt",
    "moonlight-docs",
    "chiaki",
    "gnu general public license",
    "gnu affero",
];

/// Shortest run of printable bytes treated as a string, matching `strings`.
const MIN_RUN: usize = 4;

/// A needle found in an artifact, with enough context to judge it.
struct Hit {
    needle: &'static str,
    text: String,
}

pub fn taint_audit(paths: Vec<PathBuf>) -> Result<()> {
    let paths = if paths.is_empty() {
        default_targets()?
    } else {
        paths
    };
    if paths.is_empty() {
        bail!(
            "no artifacts to scan: build first (`cargo build --release -p gsa-client-ffi`) \
             or pass --path"
        );
    }

    let mut total = 0usize;
    for path in &paths {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let hits = scan(&bytes);
        println!(
            "{}: {} bytes, {} hit(s)",
            path.display(),
            bytes.len(),
            hits.len()
        );
        for hit in &hits {
            println!("  [{}] {}", hit.needle, hit.text);
        }
        total += hits.len();
    }

    if total > 0 {
        bail!("{total} taint string(s) found; a release must have none");
    }
    println!(
        "clean: no banned project names in {} artifact(s)",
        paths.len()
    );
    Ok(())
}

/// Extract printable runs and report those containing a needle.
fn scan(bytes: &[u8]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut run = Vec::new();
    // A trailing run at EOF still has to be checked, hence the chained None.
    for byte in bytes.iter().copied().map(Some).chain(std::iter::once(None)) {
        match byte {
            Some(b) if (0x20..0x7f).contains(&b) => run.push(b),
            _ => {
                if run.len() >= MIN_RUN {
                    let text = String::from_utf8_lossy(&run).to_lowercase();
                    for needle in NEEDLES {
                        if text.contains(needle) {
                            hits.push(Hit {
                                needle,
                                text: String::from_utf8_lossy(&run).into_owned(),
                            });
                        }
                    }
                }
                run.clear();
            }
        }
    }
    hits
}

/// Release artifacts that ship inside an app, if they have been built.
fn default_targets() -> Result<Vec<PathBuf>> {
    let release = Path::new("target/release");
    if !release.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(release)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // The staticlib and cdylib are what an embedding app links; other
        // release output (rlibs, build scripts) never reaches a device.
        if name.starts_with("libgsa_client_ffi.")
            && (name.ends_with(".a") || name.ends_with(".dylib") || name.ends_with(".so"))
        {
            found.push(path);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::scan;

    #[test]
    fn a_banned_project_name_is_found_in_a_printable_run() {
        let hits = scan(b"\x00\x01/home/x/chiaki-ng/src/video.c\x00");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].needle, "chiaki");
    }

    #[test]
    fn the_protocol_name_alone_is_not_taint() {
        // Our own crate and symbol names contain it; flagging them would make
        // the audit useless.
        assert!(scan(b"gsa_backend_moonlight::video::depacketize").is_empty());
        assert!(scan(b"moonlight host").is_empty());
    }

    #[test]
    fn matching_ignores_case_and_short_runs_are_skipped() {
        assert_eq!(scan(b"MOONLIGHT-COMMON-C").len(), 1);
        // Below the printable-run threshold, so never considered.
        assert!(scan(b"\x00chi\x00").is_empty());
    }
}
