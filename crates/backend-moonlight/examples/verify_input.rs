//! Prove input reaches the host by *watching the picture change*.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example verify_input -- 192.168.50.184:47989 881448767
//! ```
//!
//! Sends a scripted, deterministic sequence — park the pointer in one corner,
//! then the opposite one — records the whole session, and reports the frame
//! numbers to extract so the two positions can be compared. No human needs to
//! watch a screen: the evidence is in the recording.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};
use gsa_client_core::{ClockSync, StreamSession};
use gsa_protocol::input::{InputEvent, MouseMove};

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

/// One scripted step: when to fire it, and where to put the pointer.
struct Step {
    at: std::time::Duration,
    x: f32,
    y: f32,
    label: &'static str,
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: verify_input <host:port> <app-id>");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
    let client_id = "0123456789ABCDEF";

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("moonlight-dev-key.pem")).expect("identity"),
    )
    .expect("load identity");
    let host_cert = std::fs::read_to_string(store("moonlight-host-cert.pem")).expect("host cert");
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
        20_000,
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

    let input = stream.input.clone();
    let mut core = StreamSession::with_capture_clock(
        stream.take_frames().expect("frames"),
        stream.recovery.clone(),
        gsa_core::time::MediaClock::new(),
        ClockSync::default(),
        stream.dropped.clone(),
        stream.recovered.clone(),
        gsa_client_core::CaptureClock::StreamPts,
    );

    // Corners, far apart, so the pointer cannot be confused with anything
    // else that happens to move on the desktop.
    let script = [
        Step {
            at: std::time::Duration::from_secs(3),
            x: 0.03,
            y: 0.05,
            label: "top-left",
        },
        Step {
            at: std::time::Duration::from_secs(6),
            x: 0.97,
            y: 0.93,
            label: "bottom-right",
        },
        Step {
            at: std::time::Duration::from_secs(9),
            x: 0.03,
            y: 0.93,
            label: "bottom-left",
        },
    ];

    let path = std::env::temp_dir().join("gsa-moonlight-verify.h264");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("recording"));
    let start = std::time::Instant::now();
    let mut frames = 0u32;
    let mut next_step = 0usize;
    // Frame number captured shortly after each step, for extraction.
    let mut marks: Vec<(u32, &'static str)> = Vec::new();
    let mut pending: Option<(std::time::Instant, &'static str)> = None;

    while start.elapsed() < std::time::Duration::from_secs(12) {
        if let Some(step) = script.get(next_step)
            && start.elapsed() >= step.at
        {
            input.send(vec![InputEvent::MouseMove(MouseMove::Absolute {
                x: step.x,
                y: step.y,
                ts_us: 0,
            })]);
            eprintln!(
                "[{:>5.1}s] pointer -> {}",
                start.elapsed().as_secs_f32(),
                step.label
            );
            // Give the host time to move the cursor and encode it before
            // deciding which frame shows the result.
            pending = Some((
                std::time::Instant::now() + std::time::Duration::from_millis(700),
                step.label,
            ));
            next_step += 1;
        }

        let remaining = std::time::Duration::from_millis(200);
        match tokio::time::timeout(remaining, core.recv_encoded()).await {
            Ok(Ok(Some(frame))) => {
                use std::io::Write;
                let _ = file.write_all(&frame.data);
                core.frame_presented(frame.capture_ts_us);
                frames += 1;
                if let Some((when, label)) = pending
                    && std::time::Instant::now() >= when
                {
                    marks.push((frames - 1, label));
                    pending = None;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => {}
        }
    }
    {
        use std::io::Write;
        let _ = file.flush();
    }

    eprintln!("recorded {frames} frames to {}", path.display());
    for (frame, label) in &marks {
        println!("MARK {frame} {label}");
    }

    drop(core);
    drop(stream);
    let _ = session.cancel().await;
}
