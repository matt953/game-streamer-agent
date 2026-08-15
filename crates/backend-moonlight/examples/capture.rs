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
    let client_id = "0123456789ABCDEF";

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
    println!(
        "pinging {video_addr} from local port {}",
        video.local_port()
    );

    let mut buf = vec![0u8; 4096];
    let mut seen = 0usize;
    let mut bytes = 0usize;
    let start = std::time::Instant::now();
    let mut last_ping = std::time::Instant::now() - std::time::Duration::from_secs(1);

    while start.elapsed() < std::time::Duration::from_secs(12) {
        if last_ping.elapsed() >= std::time::Duration::from_millis(500) {
            video.ping()?;
            last_ping = std::time::Instant::now();
        }
        while let Ok(m) = evt_rx.try_recv() {
            println!("  control: {m:?}");
        }
        if let Some(n) = video.recv(&mut buf)? {
            seen += 1;
            bytes += n;
            // Dump the head of the first few, and one later packet to see
            // how the fields advance.
            if seen <= 4 || seen == 60 {
                println!("--- datagram #{seen}, {n} bytes ---");
                dump(&buf[..n.min(64)]);
            }
        }
    }

    println!(
        "received {seen} datagrams, {bytes} bytes in {:.1}s",
        start.elapsed().as_secs_f32()
    );
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
