//! Prove controller input reaches the host, by driving a UI that responds to
//! one and watching the picture change.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example verify_gamepad -- 192.168.50.184:47989 1093255277
//! ```
//!
//! Launches the given app (a controller-driven shell), lets it settle, then
//! presses a direction several times, marking the frames to compare. Always
//! cancels, so the host is left as it was found.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};
use gsa_client_core::{ClockSync, StreamSession};
use gsa_protocol::input::{GamepadInput, InputEvent, gamepad};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// Press and release one button, as a real pad would.
fn press(input: &std::sync::Arc<dyn gsa_client_core::InputSink>, buttons: u32) {
    input.send(vec![InputEvent::Gamepad(GamepadInput {
        seat: 0,
        buttons,
        axes: [0; 8],
        ts_us: 0,
    })]);
    std::thread::sleep(std::time::Duration::from_millis(80));
    input.send(vec![InputEvent::Gamepad(GamepadInput {
        seat: 0,
        buttons: 0,
        axes: [0; 8],
        ts_us: 0,
    })]);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: verify_gamepad <host:port> <app-id>");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
    let client_id = "0123456789ABCDEF";

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("gsa-moonlight-dev-key.pem")).expect("identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("gsa-moonlight-host-cert.pem")).expect("host cert");
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
        match gsa_backend_moonlight::start(&mut session, addr.ip(), app_id, mode, 20_000).await {
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

    let path = std::env::temp_dir().join("gsa-moonlight-gamepad.h264");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("recording"));
    let start = std::time::Instant::now();
    let mut frames = 0u32;
    let mut marks: Vec<(u32, String)> = Vec::new();
    // Let the shell finish drawing before touching anything, then move the
    // selection in one direction so any change is unambiguous.
    let script: [(u64, Option<u32>, &str); 5] = [
        (10, None, "settled"),
        (12, Some(gamepad::DPAD_DOWN), "after-down-1"),
        (14, Some(gamepad::DPAD_DOWN), "after-down-2"),
        (16, Some(gamepad::DPAD_UP), "after-up-1"),
        (18, Some(gamepad::A), "after-a"),
    ];
    let mut next = 0usize;
    let mut pending: Option<(std::time::Instant, String)> = None;

    while start.elapsed() < std::time::Duration::from_secs(21) {
        if let Some((at, buttons, label)) = script.get(next)
            && start.elapsed() >= std::time::Duration::from_secs(*at)
        {
            if let Some(buttons) = buttons {
                press(&input, *buttons);
                eprintln!(
                    "[{:>4.1}s] pressed for {label}",
                    start.elapsed().as_secs_f32()
                );
            }
            pending = Some((
                std::time::Instant::now() + std::time::Duration::from_millis(600),
                (*label).to_owned(),
            ));
            next += 1;
        }
        match tokio::time::timeout(std::time::Duration::from_millis(200), core.recv_encoded()).await
        {
            Ok(Ok(Some(frame))) => {
                use std::io::Write;
                let _ = file.write_all(&frame.data);
                core.frame_presented(frame.capture_ts_us);
                frames += 1;
                if let Some((when, label)) = pending.clone()
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
    match session.cancel().await {
        Ok(()) => eprintln!("cancelled — host restored"),
        Err(e) => eprintln!("cancel failed: {e}"),
    }
}
