//! Inspect the audio stream: packet types, sizes, and whether the payload
//! looks like plaintext Opus.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example audioprobe -- 192.168.50.184:47989 881448767
//! ```

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
        eprintln!("usage: audioprobe <host:port> <app-id>");
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
    let session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id.to_owned(),
    );

    // Experiment: does asking the host to keep playing its own audio stop it
    // sending us any?
    let keep_host_audio = std::env::var("GSA_KEEP_HOST_AUDIO").is_ok();
    let mode = StreamMode {
        keep_host_audio,
        ..StreamMode::default()
    };
    println!("keep_host_audio={keep_host_audio}");
    let launched = match session.launch(app_id, mode).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("launch failed: {e}");
            return;
        }
    };

    let mut rtsp = gsa_backend_moonlight::Rtsp::new(&launched.rtsp_url).expect("rtsp");
    let negotiated = rtsp
        .negotiate(
            gsa_backend_moonlight::StreamRequest {
                hdr: false,
                width: mode.width,
                height: mode.height,
                fps: mode.fps,
                bitstream_format: 0,
                bitrate_kbps: 10_000,
                packet_size: 1392,
                channels: 2,
            },
            true,
        )
        .await
        .expect("negotiate");
    println!(
        "encryption supported={:#x} requested={:#x}",
        negotiated.encryption_supported, negotiated.encryption_requested
    );

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (evt_tx, _evt_rx) = std::sync::mpsc::channel();
    let crypto = gsa_backend_moonlight::Crypto::new(launched.riaes_key, negotiated.control_v2());
    let control_addr = std::net::SocketAddr::new(addr.ip(), negotiated.control_port);
    let connect_data = negotiated.connect_data.unwrap_or(0);
    let control = std::thread::spawn(move || {
        let _ = gsa_backend_moonlight::run_control(
            control_addr,
            connect_data,
            true,
            crypto,
            cmd_rx,
            evt_tx,
        );
    });

    let mut media = gsa_backend_moonlight::MediaSocket::bind(
        std::net::SocketAddr::new(addr.ip(), negotiated.video_port),
        negotiated.ping_payload,
    )
    .expect("socket");
    let audio_port = negotiated.audio_port;

    // After video is established on the first socket, claim the audio port
    // from a second one. If audio appears there the host was willing all
    // along and the binding was the problem; if nothing appears, the host is
    // not producing audio for this session at all.
    let mut second: Option<gsa_backend_moonlight::MediaSocket> = None;
    let mut on_second = 0usize;
    // Feed what arrives through the real decode path and measure the result:
    // decoding without checking the samples would pass on silence.
    let (mut audio_rx, pcm_out) = gsa_backend_moonlight::AudioReceive::new(None).expect("audio");
    let mut pcm_frames = 0usize;
    let mut samples = 0usize;
    let mut peak = 0i32;
    let mut energy = 0f64;
    let mut wav: Vec<i16> = Vec::new();
    let mut buf = vec![0u8; 4096];
    let mut by_type: std::collections::BTreeMap<u8, (usize, usize)> =
        std::collections::BTreeMap::new();
    let mut shown = 0usize;
    let start = std::time::Instant::now();
    let mut last_ping = start - std::time::Duration::from_secs(1);
    while start.elapsed() < std::time::Duration::from_secs(10) {
        if start.elapsed() >= std::time::Duration::from_secs(4) && second.is_none() {
            let sock = gsa_backend_moonlight::MediaSocket::bind(
                std::net::SocketAddr::new(addr.ip(), audio_port),
                negotiated.ping_payload,
            )
            .expect("second socket");
            println!("[4s] claiming the audio port from a second socket");
            second = Some(sock);
        }
        if last_ping.elapsed() >= std::time::Duration::from_millis(400) {
            let _ = media.ping();
            match second.as_mut() {
                Some(sock) => {
                    let _ = sock.ping();
                }
                None => {
                    let _ = media.ping_port(audio_port);
                }
            }
            last_ping = std::time::Instant::now();
        }
        if let Some(sock) = second.as_ref()
            && let Ok(Some(n)) = sock.recv(&mut buf)
        {
            on_second += 1;
            if on_second == 1 {
                println!("  second socket got {n} bytes, packet type {}", buf[1]);
            }
        }
        if let Ok(Some(n)) = media.recv(&mut buf) {
            if n < 2 {
                continue;
            }
            let packet_type = buf[1];
            if gsa_backend_moonlight::AudioReceive::owns(&buf[..n]) {
                audio_rx.handle(&buf[..n]);
                while let Ok(pcm) = pcm_out.try_recv() {
                    pcm_frames += 1;
                    samples += pcm.len();
                    for s in &pcm {
                        peak = peak.max(i32::from(s.abs()));
                        energy += f64::from(*s) * f64::from(*s);
                    }
                    wav.extend_from_slice(&pcm);
                }
            }
            let entry = by_type.entry(packet_type).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += n;
            // Video is type 0; anything else is what we came to look at.
            if packet_type != 0 && shown < 3 {
                shown += 1;
                println!("--- non-video packet type {packet_type}, {n} bytes ---");
                let head = &buf[..n.min(40)];
                let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
                println!("  {}", hex.join(" "));
            }
        }
    }

    let rms = if samples > 0 {
        (energy / samples as f64).sqrt()
    } else {
        0.0
    };
    println!("\ndecoded {pcm_frames} PCM frames, {samples} samples, peak {peak}, rms {rms:.0}");
    println!("second socket received {on_second} datagrams");
    if !wav.is_empty() {
        let path = std::env::temp_dir().join("gsa-moonlight-audio.wav");
        if let Err(e) = write_wav(&path, &wav, 48_000, 2) {
            println!("could not write audio: {e}");
        } else {
            println!("wrote {} ({} samples)", path.display(), wav.len());
        }
    }
    println!("\npacket types seen (type: count, bytes):");
    for (kind, (count, bytes)) in &by_type {
        let label = match kind {
            0 => " (video)",
            97 => " (audio data)",
            127 => " (audio FEC)",
            _ => "",
        };
        println!("  {kind}{label}: {count} packets, {bytes} bytes");
    }

    let _ = cmd_tx.send(gsa_backend_moonlight::Command::Stop);
    let _ = control.join();
    let _ = session.cancel().await;
}

/// Write interleaved PCM as a WAV so a human can listen to what we decoded.
fn write_wav(
    path: &std::path::Path,
    pcm: &[i16],
    sample_rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = (pcm.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&(channels * 2).to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits per sample
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in pcm {
        f.write_all(&s.to_le_bytes())?;
    }
    f.flush()
}
