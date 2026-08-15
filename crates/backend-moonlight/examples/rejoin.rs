//! Experiment: does `/resume` restart a session that `/launch` cannot?
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example rejoin -- 192.168.50.184:47989 881448767
//! ```
//!
//! Reports the host's state, then tries each call in turn and says whether
//! media actually arrived. Purely diagnostic; changes no behaviour.

use gsa_backend_moonlight::{ClientIdentity, LaunchedSession, PairedSession, StreamMode};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(addr), Some(app)) = (args.next(), args.next()) else {
        eprintln!("usage: rejoin <host:port> <app-id>");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let app_id: u32 = app.parse().expect("app id");
    let client_id = std::env::var("GSA_CLIENT_ID").unwrap_or_else(|_| "0123456789ABCDEF".into());

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("gsa-moonlight-dev-key.pem")).expect("identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("gsa-moonlight-host-cert.pem")).expect("host cert");
    let info = gsa_backend_moonlight::probe(addr, &client_id)
        .await
        .expect("probe");
    let session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.clone(),
    );

    let before = session.server_info().await.expect("serverinfo");
    println!(
        "host before: state={} current_game={}",
        before.state, before.current_game
    );

    let mode = StreamMode::default();
    if before.current_game == 0 {
        println!("-> nothing running; launching");
        match session.launch(app_id, mode).await {
            Ok(l) => report("launch", &session, addr, l, mode).await,
            Err(e) => println!("   launch failed: {e}"),
        }
    }

    let mid = session.server_info().await.expect("serverinfo");
    println!(
        "host now: state={} current_game={}",
        mid.state, mid.current_game
    );
    if mid.current_game != 0 {
        println!("-> a session exists; resuming it");
        match session.resume(mode).await {
            Ok(l) => report("resume", &session, addr, l, mode).await,
            Err(e) => println!("   resume failed: {e}"),
        }
    }

    match session.cancel().await {
        Ok(()) => println!("cancelled"),
        Err(e) => println!("cancel failed: {e}"),
    }
}

/// Bring the streams up for one launched/resumed session and say whether the
/// host actually sent anything.
async fn report(
    what: &str,
    _session: &PairedSession,
    addr: std::net::SocketAddr,
    launched: LaunchedSession,
    mode: StreamMode,
) {
    let mut rtsp = match gsa_backend_moonlight::Rtsp::new(&launched.rtsp_url) {
        Ok(r) => r,
        Err(e) => {
            println!("   {what}: bad rtsp url: {e}");
            return;
        }
    };
    let negotiated = match rtsp
        .negotiate(gsa_backend_moonlight::StreamRequest {
            width: mode.width,
            height: mode.height,
            fps: mode.fps,
            bitstream_format: 0,
            bitrate_kbps: 10_000,
            packet_size: 1392,
            channels: 2,
        })
        .await
    {
        Ok(n) => n,
        Err(e) => {
            println!("   {what}: rtsp failed: {e}");
            return;
        }
    };

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (evt_tx, _evt_rx) = std::sync::mpsc::channel();
    let crypto = gsa_backend_moonlight::Crypto::new(launched.riaes_key, negotiated.control_v2());
    let control_addr = std::net::SocketAddr::new(addr.ip(), negotiated.control_port);
    let connect_data = negotiated.connect_data.unwrap_or(0);
    let control = std::thread::spawn(move || {
        let _ =
            gsa_backend_moonlight::run_control(control_addr, connect_data, crypto, cmd_rx, evt_tx);
    });

    // One socket, pinging both media ports. The host binds a stream to
    // whichever address pinged it, so two sockets sharing a session let one
    // steal the other's binding; with a single socket there is nothing to
    // steal and both streams land where we are listening.
    let mut video = gsa_backend_moonlight::MediaSocket::bind(
        std::net::SocketAddr::new(addr.ip(), negotiated.video_port),
        negotiated.ping_payload,
    )
    .expect("video socket");
    let audio_port = negotiated.audio_port;

    let mut buf = vec![0u8; 4096];
    let mut on_video = 0usize;
    let mut on_audio = 0usize;
    let start = std::time::Instant::now();
    let mut last_ping = start - std::time::Duration::from_secs(1);
    while start.elapsed() < std::time::Duration::from_secs(6) && on_video < 50 {
        if last_ping.elapsed() >= std::time::Duration::from_millis(400) {
            let _ = video.ping();
            let _ = video.ping_port(audio_port);
            last_ping = std::time::Instant::now();
        }
        if let Ok(Some(_)) = video.recv(&mut buf) {
            on_video += 1;
        }
        let _ = &mut on_audio;
    }
    println!(
        "   {what}: video-socket={on_video} audio-socket={on_audio} in {:.1}s -> {}",
        start.elapsed().as_secs_f32(),
        if on_video > 0 { "MEDIA" } else { "NOTHING" }
    );
    println!("   {what}: local port {}", video.local_port());
    let _ = cmd_tx.send(gsa_backend_moonlight::Command::Stop);
    let _ = control.join();
}
