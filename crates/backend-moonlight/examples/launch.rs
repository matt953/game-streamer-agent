//! Start a stream on a paired host, print what it negotiated, then cancel.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example launch -- 192.168.50.184:47989 881448767
//! ```
//!
//! Never changes the host's display mode (`sops` off), and always cancels on
//! the way out so a probe cannot leave the host stuck streaming.

use gsa_backend_moonlight::{ClientIdentity, PairedSession, StreamMode};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
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
            let want = gsa_backend_moonlight::StreamRequest {
                width: mode.width,
                height: mode.height,
                fps: mode.fps,
                bitstream_format: 0, // H.264 first; HEVC once decode is wired
                bitrate_kbps: 20_000,
                packet_size: 1392,
                channels: mode.channels,
            };
            match rtsp.negotiate(want).await {
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
                    println!("    ping payload: {:?}", n.ping_payload.is_some());
                    println!("    connect data: {:?}", n.connect_data);
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
