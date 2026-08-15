//! Pair with a Moonlight host, persisting the identity so re-runs stay paired.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example pair -- 192.168.50.184:47989
//! ```
//!
//! Prints a PIN and then waits: the host does not answer the first request
//! until the operator enters that PIN in its UI, so the wait is the protocol
//! working, not a hang.

use gsa_backend_moonlight::ClientIdentity;

/// Where the dev identity lives. Real embedders keep this in the keychain.
fn identity_path() -> std::path::PathBuf {
    std::env::var_os("GSA_MOONLIGHT_KEY").map_or_else(
        || {
            let mut p = std::env::temp_dir();
            p.push("gsa-moonlight-dev-key.pem");
            p
        },
        std::path::PathBuf::from,
    )
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let Some(addr) = std::env::args().nth(1) else {
        eprintln!("usage: pair <host:port>   (the cleartext port, normally 47989)");
        std::process::exit(2);
    };
    let addr: std::net::SocketAddr = addr.parse().expect("host:port");

    // Reuse a stored key when there is one: a new key is a new identity, and
    // the host would treat us as a stranger.
    let path = identity_path();
    let identity = match std::fs::read_to_string(&path) {
        Ok(pem) => {
            println!("using stored identity at {}", path.display());
            ClientIdentity::from_key_pem(&pem).expect("load identity")
        }
        Err(_) => {
            println!("minting a new identity at {} …", path.display());
            let id = ClientIdentity::generate().expect("generate identity");
            std::fs::write(&path, id.key_pem()).expect("save identity");
            id
        }
    };

    // Stable per install; the host keys its pairing state on this.
    let client_id = "0123456789ABCDEF";
    // A caller-supplied PIN keeps a scripted run predictable; a real client
    // always generates one so it cannot be guessed ahead of time.
    let pin = std::env::args()
        .nth(2)
        .unwrap_or_else(gsa_backend_moonlight::random_pin);
    println!();
    println!("    Enter this PIN on the host:  {pin}");
    println!();
    println!("waiting for the host to accept (it will not answer until then) …");

    match gsa_backend_moonlight::pair(addr, client_id, "gsa dev client", &pin, &identity).await {
        Ok(paired) => {
            println!("paired with {addr}");
            // Prove the pairing is real by using it: the app list is only
            // reachable over mutual TLS as a client the host now trusts.
            let info = gsa_backend_moonlight::probe(addr, client_id)
                .await
                .expect("probe");
            // Persist the host certificate: it is what pins every future
            // TLS connection to this exact machine.
            let cert_path = std::env::temp_dir().join("gsa-moonlight-host-cert.pem");
            std::fs::write(&cert_path, &paired.host_cert_pem).expect("save host cert");
            println!("host certificate saved to {}", cert_path.display());
            let tls_addr = std::net::SocketAddr::new(addr.ip(), info.https_port);
            let session = gsa_backend_moonlight::PairedSession::new(
                tls_addr,
                paired.host_cert_pem,
                identity,
                client_id.to_owned(),
            );
            let info = session
                .server_info()
                .await
                .expect("authenticated serverinfo");
            println!(
                "host says: paired={} codecs={:#x} current_game={}",
                info.paired, info.codec_mode_support, info.current_game
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
        Err(e) => {
            eprintln!("pairing failed: {e}");
            std::process::exit(1);
        }
    }
}
