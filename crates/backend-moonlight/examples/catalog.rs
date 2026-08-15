//! List a paired host's library over mutual TLS.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example catalog -- 192.168.50.184:47989
//! ```
//!
//! Reads the identity and the host certificate saved by the `pair` example.

use gsa_backend_moonlight::{ClientIdentity, PairedSession};

fn store(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

#[tokio::main]
async fn main() {
    let Some(addr) = std::env::args().nth(1) else {
        eprintln!("usage: catalog <host:port>   (the cleartext port, normally 47989)");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");
    let client_id = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "0123456789ABCDEF".to_owned());

    let identity = ClientIdentity::from_key_pem(
        &std::fs::read_to_string(store("gsa-moonlight-dev-key.pem")).expect("paired identity"),
    )
    .expect("load identity");
    let host_cert =
        std::fs::read_to_string(store("gsa-moonlight-host-cert.pem")).expect("host certificate");

    // The TLS port comes from the host rather than the default, since a host
    // can be moved off it.
    let info = gsa_backend_moonlight::probe(addr, &client_id)
        .await
        .expect("probe");
    let session = PairedSession::new(
        std::net::SocketAddr::new(addr.ip(), info.https_port),
        host_cert,
        identity,
        client_id,
    );

    let info = session
        .server_info()
        .await
        .expect("authenticated serverinfo");
    println!(
        "{} — paired={} codecs={:#x} state={}",
        info.hostname, info.paired, info.codec_mode_support, info.state
    );
    for entry in session.catalog().await.expect("catalog") {
        println!(
            "  [{:>10}] {:<20} {:?}{}",
            entry.id,
            entry.title,
            entry.kind,
            if entry.running { "  (running)" } else { "" }
        );
    }
}
