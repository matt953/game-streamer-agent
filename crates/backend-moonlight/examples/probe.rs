//! Ask a Moonlight host to describe itself — the reachability check before
//! any pairing work.
//!
//! ```text
//! cargo run -p gsa-backend-moonlight --example probe -- 192.168.50.184:47989
//! ```

#[tokio::main]
async fn main() {
    let Some(addr) = std::env::args().nth(1) else {
        eprintln!("usage: probe <host:port>   (the cleartext port, normally 47989)");
        std::process::exit(2);
    };
    let Ok(addr) = addr.parse::<std::net::SocketAddr>() else {
        eprintln!("not a socket address: {addr}");
        std::process::exit(2);
    };
    match gsa_backend_moonlight::probe(addr, "0123456789ABCDEF").await {
        Ok(info) => println!("{info:#?}"),
        Err(e) => {
            eprintln!("probe failed: {e}");
            std::process::exit(1);
        }
    }
}
