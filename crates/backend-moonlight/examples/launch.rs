//! Start a stream on a paired host, print what it negotiated, then cancel.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example launch -- 192.168.50.184:47989 881448767
//! ```
//!
//! Never changes the host's display mode (`sops` off), and always cancels on
//! the way out so a probe cannot leave the host stuck streaming.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};

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
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: launch <host:port> <app-id>");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
    let client_id = "0123456789ABCDEF";

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("moonlight-dev-key.pem")).expect("paired identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("moonlight-host-cert.pem")).expect("host certificate");

    let info = gsa_backend_moonlight::probe(addr, client_id)
        .await
        .expect("probe");
    let session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.to_owned(),
    );

    let mode = StreamMode {
        width: 1920,
        height: 1080,
        fps: 60,
        ..StreamMode::default()
    };
    println!(
        "launching app {app_id} at {}x{}x{}",
        mode.width, mode.height, mode.fps
    );
    match session.launch(app_id, mode).await {
        Ok(launched) => {
            println!("  rtsp:      {}", launched.rtsp_url);
            println!("  key id:    {}", launched.riaes_key_id);
            println!("  key bytes: {}", launched.riaes_key.len());
            let info = session.server_info().await.expect("serverinfo");
            println!(
                "  host state: {} current_game={}",
                info.state, info.current_game
            );
            // Negotiate the streams. The host picks the ports and tells us
            // which encryption it wants, so nothing here is assumed.
            let mut rtsp = gsa_backend_moonlight::Rtsp::new(&launched.rtsp_url).expect("rtsp url");
            let mut exchange =
                gsa_backend_moonlight::TcpRtsp::new(rtsp.addr().expect("rtsp address"));
            let want = gsa_backend_moonlight::StreamRequest {
                hdr: false,
                width: mode.width,
                height: mode.height,
                fps: mode.fps,
                bitstream_format: 0, // H.264 first; HEVC once decode is wired
                bitrate_kbps: 20_000,
                packet_size: 1392,
                channels: mode.channels,
            };
            match rtsp.negotiate(&mut exchange, want, true).await {
                Ok(n) => {
                    println!("  negotiated:");
                    println!("    video port:   {}", n.video_port);
                    println!("    audio port:   {}", n.audio_port);
                    println!("    control port: {}", n.control_port);
                    println!(
                        "    encryption:   supported={:#x} requested={:#x} control_v2={}",
                        n.encryption_supported,
                        n.encryption_requested,
                        n.control_v2()
                    );
                    println!("    ref invalidation: {}", n.reference_invalidation);
                    println!(
                        "    ping payload: {:?}",
                        n.ping_payload
                            .map(|p| String::from_utf8_lossy(&p).to_string())
                    );
                    println!("    connect data: {:?}", n.connect_data);

                    // Bring up the control channel. This is the real test of
                    // whether a stock Rust ENet talks to the host's fork.
                    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
                    let (evt_tx, evt_rx) = std::sync::mpsc::channel();
                    let control_addr = std::net::SocketAddr::new(addr.ip(), n.control_port);
                    let crypto =
                        gsa_backend_moonlight::Crypto::new(launched.riaes_key, n.control_v2());
                    let worker = std::thread::spawn(move || {
                        gsa_backend_moonlight::run_control(
                            control_addr,
                            n.connect_data.unwrap_or(0),
                            true,
                            crypto,
                            cmd_rx,
                            evt_tx,
                        )
                    });
                    println!("  control channel: connecting to {control_addr} …");
                    let watch = std::time::Duration::from_secs(8);
                    let start = std::time::Instant::now();
                    let mut heard = 0usize;
                    while start.elapsed() < watch {
                        match evt_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                            Ok(m) => {
                                heard += 1;
                                println!("    host: {m:?}");
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(_) => break,
                        }
                    }
                    let _ = cmd_tx.send(gsa_backend_moonlight::Command::Stop);
                    match worker.join() {
                        Ok(Ok(())) => {
                            println!("  control channel closed cleanly ({heard} messages)")
                        }
                        Ok(Err(e)) => println!("  control channel error: {e}"),
                        Err(_) => println!("  control thread panicked"),
                    }
                }
                Err(e) => eprintln!("  rtsp failed: {e}"),
            }

            // Optionally keep the session up so later stages can be worked
            // on against a live host; still cancelled on the way out.
            if let Some(hold) = std::env::args().nth(3).and_then(|s| s.parse().ok()) {
                println!("holding the session open for {hold}s");
                tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
            }
        }
        Err(e) => eprintln!("launch failed: {e}"),
    }

    // Whatever happened above, do not leave the host streaming.
    match session.cancel().await {
        Ok(()) => println!("cancelled"),
        Err(e) => eprintln!("cancel failed: {e}"),
    }
}
