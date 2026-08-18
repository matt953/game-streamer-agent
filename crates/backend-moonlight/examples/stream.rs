//! Stream from a Moonlight host **through the shared client core**.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example stream -- 192.168.50.184:47989 881448767 15
//! ```
//!
//! The point of this example is the seam: frames leave the backend as neutral
//! access units and everything after that — reference gate, de-jitter, health
//! stats — is the same code the gsa backend uses.
//!
//! To **watch** the stream, pipe the elementary stream into a player:
//!
//! ```text
//! GSA_STDOUT=1 cargo run -q -p gsa-backend-moonlight --example stream -- \
//!     192.168.50.184:47989 881448767 60 | ffplay -fflags nobuffer -f h264 -i -
//! ```
//!
//! Or record it and open the file afterwards with `GSA_RECORD=/path/out.h264`.
//! Progress and stats always go to stderr, so they never corrupt the video.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};
use gsa_client_core::{ClockSync, StreamSession};

/// Where the paired dev credentials live, shared with `pair` and the harness.
///
/// Deliberately not the OS temp directory: macOS purges it, and losing the
/// pairing costs a PIN round-trip with whoever owns the host.
fn store(name: &str) -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("GSA_MOONLIGHT_DIR") {
        return std::path::PathBuf::from(dir).join(name);
    }
    let base = std::env::var_os("HOME").map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    base.join(".local/share/gsa").join(name)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();

    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: stream <host:port> <app-id> [seconds]");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);
    // Overridable so a wedged host can be probed with a fresh session slot.
    let client_id_owned =
        std::env::var("GSA_CLIENT_ID").unwrap_or_else(|_| "0123456789ABCDEF".into());
    let client_id: &str = &client_id_owned;

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("moonlight-dev-key.pem")).expect("paired identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("moonlight-host-cert.pem")).expect("host certificate");
    let info = gsa_backend_moonlight::probe(addr, client_id)
        .await
        .expect("probe");
    let mut session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.to_owned(),
    );

    let mode = StreamMode::default();
    let mut stream = match gsa_backend_moonlight::start(
        &mut session,
        addr.ip(),
        app_id,
        mode,
        10_000,
        &[gsa_core::media::Codec::H264],
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not start: {e}");
            let _ = session.cancel().await;
            return;
        }
    };
    eprintln!("streaming; driving the shared client core for {seconds}s");

    // Everything past this point is backend-agnostic.
    // Audio goes through the same injected-loss path as video, so its
    // behaviour under stress can be measured rather than assumed.
    let audio = stream.audio_channel();
    let frames = stream.take_frames().expect("frames not yet taken");
    let mut core = StreamSession::with_capture_clock(
        frames,
        stream.recovery.clone(),
        gsa_core::time::MediaClock::new(),
        ClockSync::default(),
        stream.dropped.clone(),
        stream.recovered.clone(),
        gsa_client_core::CaptureClock::StreamPts,
    );

    // Where the pictures go. stdout is for piping into a player, so every
    // human-readable line in this example goes to stderr.
    let to_stdout = std::env::var("GSA_STDOUT").is_ok();
    let mut recording = std::env::var("GSA_RECORD")
        .ok()
        .map(|path| std::io::BufWriter::new(std::fs::File::create(path).expect("record file")));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut frames = 0usize;
    let mut keyframes = 0usize;
    let mut first: Option<Vec<u8>> = None;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, core.recv_encoded()).await {
            Ok(Ok(Some(frame))) => {
                frames += 1;
                if frame.keyframe {
                    keyframes += 1;
                }
                // Report it as presented: without this the health stats have
                // no display truth and every frame looks dropped.
                core.frame_presented(frame.capture_ts_us);
                if to_stdout {
                    use std::io::Write;
                    let mut out = std::io::stdout().lock();
                    if out.write_all(&frame.data).is_err() {
                        break; // player went away
                    }
                    let _ = out.flush();
                }
                if let Some(file) = recording.as_mut() {
                    use std::io::Write;
                    let _ = file.write_all(&frame.data);
                }
                if first.is_none() {
                    first = Some(frame.data);
                }
            }
            Ok(Ok(None)) => {
                eprintln!("stream ended");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("stream error: {e}");
                break;
            }
            Err(_) => break, // ran out the clock
        }
    }

    let mut audio_frames = 0usize;
    let mut audio_samples = 0usize;
    let mut audio_peak = 0i32;
    while let Ok(pcm) = audio.try_recv() {
        audio_frames += 1;
        audio_samples += pcm.len();
        for s in &pcm {
            audio_peak = audio_peak.max(i32::from(s.abs()));
        }
    }
    eprintln!("  audio: {audio_frames} frames, {audio_samples} samples, peak {audio_peak}");

    let (invalidations, keyframe_requests) = stream.repairs.requests();
    eprintln!(
        "  repairs asked for: {invalidations} reference invalidations, {keyframe_requests} keyframes"
    );

    let stats = core.stats();
    let present = core.present_stats();
    eprintln!("frames {frames} ({keyframes} key)");
    eprintln!(
        "  core stats: complete={} decoded={} dropped_incomplete={} recovered={}",
        stats.frames_complete,
        stats.frames_decoded,
        stats.frames_dropped_incomplete,
        stats.frames_recovered
    );
    eprintln!(
        "  presented: {} at {:.1} fps (1% low {:.1}), freezes {}, stutters {}",
        present.presented,
        f64::from(present.fps_x100) / 100.0,
        f64::from(present.low1_fps_x100) / 100.0,
        present.freezes,
        present.stutters
    );
    if let Some(file) = recording.as_mut() {
        use std::io::Write;
        let _ = file.flush();
    }
    if let Some(data) = first {
        let path = std::env::temp_dir().join("gsa-moonlight-core-frame.h264");
        std::fs::write(&path, data).ok();
        eprintln!("  first frame written to {}", path.display());
    }

    drop(core);
    // Dropping this stops the receive threads and ends the session.
    drop(stream);
    match session.cancel().await {
        Ok(()) => eprintln!("cancelled"),
        Err(e) => eprintln!("cancel failed: {e}"),
    }
}
