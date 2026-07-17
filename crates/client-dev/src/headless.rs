//! Headless mode: decode N frames, verify the test-pattern marker, emit a
//! stats report. This is the e2e/CI entry point (spec 13 Tier 1) and the
//! source of the latency ledger.

use anyhow::{Result, bail};
use gsa_client_core::Client;
use gsa_core::id::SourceId;
use serde::Serialize;

use crate::decoder::{decoder_max_profile, make_decoder};

#[derive(Debug, Serialize)]
struct Report {
    connect: String,
    mode: String,
    frames_requested: u32,
    frames_decoded: u64,
    frames_complete: u64,
    frames_dropped_incomplete: u64,
    frames_recovered: u64,
    marker_read_ok: u64,
    marker_regressions: u64,
    latency_ms_p50: Option<f64>,
    latency_ms_p95: Option<f64>,
    latency_ms_p99: Option<f64>,
    decode_ms_p50: Option<f64>,
    /// Rolling received video goodput (Mb/s) — used by the chaos rig to check
    /// ABR converged under a shaped link.
    recv_mbps: Option<f64>,
    /// Presentation health (fed at decode-complete in this harness).
    present: gsa_client_core::stats::PresentSummary,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    addr: std::net::SocketAddr,
    frames: u32,
    json: bool,
    source: Option<String>,
    force_sw: bool,
    hw_decode: bool,
    ledger: Option<std::path::PathBuf>,
    abr: bool,
    bitrate_bps: Option<u32>,
    auth: crate::pairing::Auth,
) -> Result<()> {
    let mut client = Client::connect(
        addr,
        "client-dev-headless",
        decoder_max_profile(force_sw),
        if hw_decode {
            &[gsa_core::media::Codec::Hevc, gsa_core::media::Codec::H264][..]
        } else {
            &[gsa_core::media::Codec::H264][..]
        },
        auth.server_auth(),
    )
    .await?;

    let sources = client.list_sources().await?;
    tracing::info!("available sources:\n{}", crate::source_list(&sources));
    let source = crate::pick_source(&sources, source.as_deref())?;
    tracing::info!(source = source.name, "starting session");
    // Marker verification only means something for the synthetic pattern.
    let check_markers = source.kind == gsa_protocol::control::SourceKind::TestPattern;
    let params = client
        .start_session(SourceId(source.id.0), None, bitrate_bps, abr)
        .await?;
    // Arms the background control writer that carries stats reports + keyframe requests.
    let _input = client.take_input_sender();

    let mut ledger_file = match &ledger {
        Some(path) => Some(std::io::BufWriter::new(std::fs::File::create(path)?)),
        None => None,
    };
    let mut decoded = 0u32;
    let mut marker_read_ok = 0u64;
    let mut marker_regressions = 0u64;
    let mut last_marker: Option<u32> = None;

    if hw_decode {
        // Hardware path: encoded passthrough (the apps' path) into the
        // zero-copy VideoToolbox decoder. No software fallback: any decode
        // error fails the run.
        let codec = client
            .negotiated_codec()
            .ok_or_else(|| anyhow::anyhow!("no negotiated codec"))?;
        let mut vt = gsa_decode_videotoolbox::VtDecoder::new(codec)
            .map_err(|e| anyhow::anyhow!("hw decoder: {e}"))?;
        while decoded < frames {
            let Some(enc) = client.recv_encoded().await? else {
                bail!("connection closed after {decoded} frames");
            };
            let t0 = std::time::Instant::now();
            let surface = vt
                .decode(&enc.data)
                .map_err(|e| anyhow::anyhow!("hw decode: {e}"))?;
            let Some(surface) = surface else { continue };
            decoded += 1;
            client.frame_presented(enc.capture_ts_us);
            if let Some(w) = &mut ledger_file {
                use std::io::Write as _;
                let recv_us = enc.latency_us.unwrap_or(0);
                let dec_us = u64::from(recv_us) + t0.elapsed().as_micros() as u64;
                writeln!(
                    w,
                    "{{\"f\":{},\"recv\":{recv_us},\"dec\":{dec_us}}}",
                    enc.frame_id
                )?;
                if decoded.is_multiple_of(16) {
                    w.flush()?;
                }
            }
            if decoded.is_multiple_of(60) {
                let s = client.stats();
                tracing::info!(
                    decoded,
                    dropped = s.frames_dropped_incomplete,
                    recv_mbps = s.recv_mbps,
                    "headless progress (hw)"
                );
            }
            // Sampled integrity check: a small luma readback, after timing.
            if check_markers && decoded.is_multiple_of(30) {
                let w = gsa_core::pattern::MIN_WIDTH as u32;
                let h = gsa_core::pattern::BLOCK as u32;
                if surface.width() >= w
                    && surface.height() >= h
                    && let Some(marker) = gsa_core::pattern::read_marker_luma(
                        &surface
                            .read_luma_region(0, 0, w, h)
                            .map_err(|e| anyhow::anyhow!("marker readback: {e}"))?,
                        w as usize,
                        w as usize,
                    )
                {
                    marker_read_ok += 1;
                    if let Some(prev) = last_marker
                        && marker.wrapping_sub(prev) > u32::MAX / 2
                    {
                        marker_regressions += 1;
                    }
                    last_marker = Some(marker);
                }
            }
        }
    } else {
        let mut decoder = make_decoder(force_sw)?;
        while decoded < frames {
            let Some(out) = client.recv_frame(decoder.as_mut()).await? else {
                bail!("connection closed after {decoded} frames");
            };
            decoded += 1;
            client.frame_presented(out.capture_ts_us);
            // Ledger row: stage times as µs-from-capture, joined by frame id.
            if let Some(w) = &mut ledger_file {
                use std::io::Write as _;
                let lat = out.latency_us.unwrap_or(0);
                writeln!(w, "{{\"f\":{},\"recv\":{lat},\"dec\":{lat}}}", out.frame_id)?;
                if decoded.is_multiple_of(16) {
                    w.flush()?;
                }
            }
            // Progress heartbeat: a failing CI scenario shows where flow stopped.
            if decoded.is_multiple_of(60) {
                let s = client.stats();
                tracing::info!(
                    decoded,
                    dropped = s.frames_dropped_incomplete,
                    recv_mbps = s.recv_mbps,
                    "headless progress"
                );
            }

            if let Some(marker) = check_markers
                .then(|| {
                    gsa_core::pattern::read_marker_rgba(&out.frame.pixels, out.frame.width as usize)
                })
                .flatten()
            {
                marker_read_ok += 1;
                if let Some(prev) = last_marker {
                    // Markers may skip (dropped frames) but must never go back.
                    if marker.wrapping_sub(prev) > u32::MAX / 2 {
                        marker_regressions += 1;
                    }
                }
                last_marker = Some(marker);
            }
        }
    }

    let stats = client.stats();
    let present = client.present_stats();
    let report = Report {
        connect: addr.to_string(),
        mode: format!(
            "{}x{}@{}",
            params.mode.width, params.mode.height, params.mode.fps
        ),
        frames_requested: frames,
        frames_decoded: stats.frames_decoded,
        frames_complete: stats.frames_complete,
        frames_dropped_incomplete: stats.frames_dropped_incomplete,
        frames_recovered: stats.frames_recovered,
        marker_read_ok,
        marker_regressions,
        latency_ms_p50: stats.latency_ms_p50,
        latency_ms_p95: stats.latency_ms_p95,
        latency_ms_p99: stats.latency_ms_p99,
        decode_ms_p50: stats.decode_ms_p50,
        recv_mbps: stats.recv_mbps,
        present,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{} frames decoded @ {} — glass-to-glass p50 {:?} ms, p99 {:?} ms, decode p50 {:?} ms",
            report.frames_decoded,
            report.mode,
            report.latency_ms_p50,
            report.latency_ms_p99,
            report.decode_ms_p50
        );
        println!(
            "markers: {} read, {} regressions; {} incomplete frames dropped",
            report.marker_read_ok, report.marker_regressions, report.frames_dropped_incomplete
        );
    }

    client.close().await;
    if report.frames_decoded == 0 {
        bail!("no frames decoded");
    }
    Ok(())
}
