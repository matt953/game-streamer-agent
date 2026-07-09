//! Headless mode: decode N frames, verify the test-pattern marker, emit a
//! stats report. This is the e2e/CI entry point (spec 13 Tier 1) and the
//! source of the latency ledger.

use anyhow::{Context, Result, bail};
use gsa_client_core::Client;
use gsa_core::id::SourceId;
use serde::Serialize;

use crate::decoder::OpenH264Decoder;

#[derive(Debug, Serialize)]
struct Report {
    connect: String,
    mode: String,
    frames_requested: u32,
    frames_decoded: u64,
    frames_complete: u64,
    frames_dropped_incomplete: u64,
    marker_read_ok: u64,
    marker_regressions: u64,
    latency_ms_p50: Option<f64>,
    latency_ms_p95: Option<f64>,
    latency_ms_p99: Option<f64>,
    decode_ms_p50: Option<f64>,
}

pub async fn run(
    addr: std::net::SocketAddr,
    frames: u32,
    json: bool,
    source_id: Option<u32>,
) -> Result<()> {
    let mut client = Client::connect(addr, "client-dev-headless").await?;

    let sources = client.list_sources().await?;
    for s in &sources {
        tracing::info!(id = s.id.0, name = s.name, kind = ?s.kind, "available source");
    }
    let source = match source_id {
        Some(id) => sources
            .iter()
            .find(|s| s.id.0 == id)
            .with_context(|| format!("agent has no source {id}"))?,
        None => sources.first().context("agent offers no sources")?,
    };
    tracing::info!(source = source.name, "starting session");
    let params = client.start_session(SourceId(source.id.0), None).await?;

    let mut decoder = OpenH264Decoder::new()?;
    let mut decoded = 0u32;
    let mut marker_read_ok = 0u64;
    let mut marker_regressions = 0u64;
    let mut last_marker: Option<u32> = None;

    while decoded < frames {
        let Some(out) = client.recv_frame(&mut decoder).await? else {
            bail!("connection closed after {decoded} frames");
        };
        decoded += 1;

        if let Some(marker) = gsa_core::pattern::read_marker_luma(
            &out.frame.luma,
            out.frame.luma_stride,
            out.frame.width as usize,
        ) {
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

    let stats = client.stats();
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
        marker_read_ok,
        marker_regressions,
        latency_ms_p50: stats.latency_ms_p50,
        latency_ms_p95: stats.latency_ms_p95,
        latency_ms_p99: stats.latency_ms_p99,
        decode_ms_p50: stats.decode_ms_p50,
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
