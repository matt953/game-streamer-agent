//! Bring a session all the way up and dump raw video datagrams.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example capture -- 192.168.50.184:47989 881448767
//! ```
//!
//! Ground truth for the packet layout: what the host actually sends beats
//! what any document says it sends. Always cancels the session on exit.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: capture <host:port> <app-id>");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
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
    let session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.to_owned(),
    );

    let mode = StreamMode::default();
    let launched = match session.launch(app_id, mode).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("launch failed: {e}");
            return;
        }
    };

    let result = capture(addr, &launched, mode).await;
    if let Err(e) = &result {
        eprintln!("capture failed: {e}");
    }
    match session.cancel().await {
        Ok(()) => println!("cancelled"),
        Err(e) => eprintln!("cancel failed: {e}"),
    }
}

async fn capture(
    addr: std::net::SocketAddr,
    launched: &gsa_backend_moonlight::LaunchedSession,
    mode: StreamMode,
) -> gsa_core::Result<()> {
    let mut rtsp = gsa_backend_moonlight::Rtsp::new(&launched.rtsp_url)?;
    let negotiated = rtsp
        .negotiate(gsa_backend_moonlight::StreamRequest {
            width: mode.width,
            height: mode.height,
            fps: mode.fps,
            bitstream_format: 0,
            bitrate_kbps: 10_000,
            packet_size: 1392,
            channels: 2,
        })
        .await?;
    println!("negotiated video port {}", negotiated.video_port);
    println!(
        "  session ping payload: {}",
        negotiated.ping_payload.is_some()
    );

    // The control channel must be up: some hosts only begin sending media
    // once they have seen the start messages on it.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel();
    let control_addr = std::net::SocketAddr::new(addr.ip(), negotiated.control_port);
    let crypto = gsa_backend_moonlight::Crypto::new(launched.riaes_key, negotiated.control_v2());
    let connect_data = negotiated.connect_data.unwrap_or(0);
    let control = std::thread::spawn(move || {
        gsa_backend_moonlight::run_control(control_addr, connect_data, crypto, cmd_rx, evt_tx)
    });

    let video_addr = std::net::SocketAddr::new(addr.ip(), negotiated.video_port);
    let mut video = gsa_backend_moonlight::MediaSocket::bind(video_addr, negotiated.ping_payload)?;
    // The audio port must be pinged even though this example never reads it:
    // without it the host tears the whole session down after ten seconds.
    // It pings by address — the session payload belongs to video alone.
    let audio_addr = std::net::SocketAddr::new(addr.ip(), negotiated.audio_port);
    let mut audio = gsa_backend_moonlight::MediaSocket::bind(audio_addr, None)?;
    println!(
        "pinging {video_addr} from local port {}",
        video.local_port()
    );

    let mut depacketizer = gsa_backend_moonlight::Depacketizer::new();
    let mut frames = 0usize;
    let mut keyframes = 0usize;
    let mut lost = 0usize;
    let mut frame_bytes = 0usize;
    let mut first_frame: Option<Vec<u8>> = None;
    let mut buf = vec![0u8; 4096];
    let mut seen = 0usize;
    let mut bytes = 0usize;
    let start = std::time::Instant::now();
    let mut last_ping = std::time::Instant::now() - std::time::Duration::from_secs(1);
    // Functional probe: if the host honours a keyframe request, our control
    // messages are genuinely being understood, not merely sent.
    let mut idr_requested = false;

    let run_for = std::time::Duration::from_secs(
        std::env::args()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(12),
    );
    let mut last_report = std::time::Instant::now();
    let mut window = 0usize;
    while start.elapsed() < run_for {
        if last_report.elapsed() >= std::time::Duration::from_secs(2) {
            println!(
                "  [{:>5.1}s] video: {window} datagrams in the last 2s",
                start.elapsed().as_secs_f32()
            );
            window = 0;
            last_report = std::time::Instant::now();
        }
        if last_ping.elapsed() >= std::time::Duration::from_millis(500) {
            video.ping()?;
            audio.ping()?;
            last_ping = std::time::Instant::now();
        }
        if !idr_requested && start.elapsed() >= std::time::Duration::from_secs(4) {
            idr_requested = true;
            println!(
                "  [{:>5.1}s] asking the host for a keyframe",
                start.elapsed().as_secs_f32()
            );
            let _ = cmd_tx.send(gsa_backend_moonlight::Command::RequestIdr);
        }
        while let Ok(m) = evt_rx.try_recv() {
            println!("  [{:>5.1}s] control: {m:?}", start.elapsed().as_secs_f32());
        }
        if let Some(n) = video.recv(&mut buf)? {
            seen += 1;
            window += 1;
            bytes += n;
            depacketizer.push(&buf[..n]);
            while let Some(event) = depacketizer.next_event() {
                match event {
                    gsa_backend_moonlight::Received::Frame(f) => {
                        frames += 1;
                        frame_bytes += f.data.len();
                        if f.keyframe {
                            keyframes += 1;
                            println!(
                                "  [{:>5.1}s] keyframe #{keyframes} (frame {})",
                                start.elapsed().as_secs_f32(),
                                f.frame_index
                            );
                        }
                        if first_frame.is_none() {
                            println!("  [{:>5.1}s] first frame", start.elapsed().as_secs_f32());
                            println!(
                                "first frame: index={} keyframe={} {} bytes, starts:",
                                f.frame_index,
                                f.keyframe,
                                f.data.len()
                            );
                            dump(&f.data[..f.data.len().min(32)]);
                            first_frame = Some(f.data);
                        }
                    }
                    gsa_backend_moonlight::Received::Lost(l) => {
                        lost += 1;
                        if lost <= 3 {
                            println!("lost: {l:?}");
                        }
                    }
                    gsa_backend_moonlight::Received::Nothing => {}
                }
            }
        }
    }

    println!(
        "received {seen} datagrams, {bytes} bytes in {:.1}s",
        start.elapsed().as_secs_f32()
    );
    println!(
        "frames: {frames} ({keyframes} key), {frame_bytes} bytes of access units, {lost} lost"
    );
    if let Some(f) = &first_frame {
        let path = std::env::temp_dir().join("gsa-moonlight-frame.h264");
        std::fs::write(&path, f).ok();
        println!("wrote first frame to {}", path.display());
    }
    let _ = cmd_tx.send(gsa_backend_moonlight::Command::Stop);
    let _ = control.join();
    Ok(())
}

fn dump(bytes: &[u8]) {
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() {
                    char::from(b)
                } else {
                    '.'
                }
            })
            .collect();
        println!("  {:04x}  {:<47}  {ascii}", i * 16, hex.join(" "));
    }
}
