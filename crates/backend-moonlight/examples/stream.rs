//! Stream from a Moonlight host **through the shared client core**.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example stream -- 192.168.50.184:47989 881448767 15
//! ```
//!
//! The point of this example is the seam: frames leave the backend as neutral
//! access units and everything after that — reference gate, de-jitter, health
//! stats — is the same code the gsa backend uses.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};
use gsa_client_core::{ClockSync, StreamSession};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
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
        &std::fs::read_to_string(store("gsa-moonlight-dev-key.pem")).expect("paired identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("gsa-moonlight-host-cert.pem")).expect("host certificate");
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
    let mut stream =
        match gsa_backend_moonlight::start(&mut session, addr.ip(), app_id, mode, 10_000).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not start: {e}");
                let _ = session.cancel().await;
                return;
            }
        };
    println!("streaming; driving the shared client core for {seconds}s");

    // Everything past this point is backend-agnostic.
    let frames = stream.take_frames().expect("frames not yet taken");
    let mut core = StreamSession::new(
        frames,
        stream.recovery.clone(),
        gsa_core::time::MediaClock::new(),
        ClockSync::default(),
        stream.dropped.clone(),
        stream.recovered.clone(),
    );

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
                if first.is_none() {
                    first = Some(frame.data);
                }
            }
            Ok(Ok(None)) => {
                println!("stream ended");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("stream error: {e}");
                break;
            }
            Err(_) => break, // ran out the clock
        }
    }

    let stats = core.stats();
    let present = core.present_stats();
    println!("frames {frames} ({keyframes} key)");
    println!(
        "  core stats: complete={} decoded={} dropped_incomplete={} recovered={}",
        stats.frames_complete,
        stats.frames_decoded,
        stats.frames_dropped_incomplete,
        stats.frames_recovered
    );
    println!(
        "  presented: {} at {:.1} fps (1% low {:.1}), freezes {}, stutters {}",
        present.presented,
        f64::from(present.fps_x100) / 100.0,
        f64::from(present.low1_fps_x100) / 100.0,
        present.freezes,
        present.stutters
    );
    if let Some(data) = first {
        let path = std::env::temp_dir().join("gsa-moonlight-core-frame.h264");
        std::fs::write(&path, data).ok();
        println!("  first frame written to {}", path.display());
    }

    drop(core);
    // Dropping this stops the receive threads and ends the session.
    drop(stream);
    match session.cancel().await {
        Ok(()) => println!("cancelled"),
        Err(e) => eprintln!("cancel failed: {e}"),
    }
}
