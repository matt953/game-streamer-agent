//! QUIC transport (spec 04): one connection per session carrying video
//! datagrams, a reliable input stream, and reliable control streams.
//!
//! M0 trust model: self-signed per-run identities; the client uses a
//! deliberately-loud TOFU verifier (`DevTrustVerifier`) that logs the
//! agent's fingerprint. Real pairing + pinned mTLS replaces it at M2
//! (spec 06) — the verifier type is the seam.

mod identity;
mod stream;

pub use identity::{Identity, fingerprint};
pub use stream::{recv_msg, send_msg};

use std::net::SocketAddr;
use std::sync::Arc;

use gsa_core::{Error, Result};
use quinn::crypto::rustls::QuicClientConfig;

/// ALPN for our protocol; version-gated at the TLS layer.
pub const ALPN: &[u8] = b"gsa/0";

/// Build a listening endpoint from an identity. Returns the endpoint;
/// its local address carries the OS-assigned port when `addr` used :0.
pub fn server_endpoint(addr: SocketAddr, identity: &Identity) -> Result<quinn::Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("tls versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .map_err(|e| Error::Transport(format!("server cert: {e}")))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| Error::Transport(format!("quic server config: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    Arc::get_mut(&mut server_config.transport)
        .expect("fresh config")
        .max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(std::time::Duration::from_secs(30)).expect("valid"),
        ));

    quinn::Endpoint::server(server_config, addr)
        .map_err(|e| Error::Transport(format!("bind {addr}: {e}")))
}

/// Connect to an agent with the dev-TOFU verifier. Returns the endpoint
/// (owns the socket; keep it alive and `wait_idle` it for clean shutdown)
/// and the connection. The peer's fingerprint is logged at `INFO`.
pub async fn client_connect(addr: SocketAddr) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("tls versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(identity::DevTrustVerifier::new()))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = QuicClientConfig::try_from(tls)
        .map_err(|e| Error::Transport(format!("quic client config: {e}")))?;
    let client_config = quinn::ClientConfig::new(Arc::new(quic));

    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().expect("literal")
    } else {
        "[::]:0".parse().expect("literal")
    };
    let mut endpoint =
        quinn::Endpoint::client(bind).map_err(|e| Error::Transport(format!("client bind: {e}")))?;
    endpoint.set_default_client_config(client_config);

    let connecting = endpoint
        .connect(addr, "gsa-agent")
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?;
    let conn = connecting
        .await
        .map_err(|e| Error::Transport(format!("handshake: {e}")))?;
    Ok((endpoint, conn))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsa_protocol::control::{C2A, DecodeCaps, Hello};

    #[tokio::test]
    async fn loopback_control_and_datagrams() {
        let identity = Identity::generate().unwrap();
        let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let server_addr = server.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            // Control echo.
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let msg: C2A = recv_msg(&mut recv).await.unwrap();
            send_msg(&mut send, &msg).await.unwrap();
            // Fire datagrams at the client.
            for i in 0..20u8 {
                conn.send_datagram(bytes::Bytes::from(vec![i; 100]))
                    .unwrap();
            }
            // Hold the connection open until the client is done.
            let _ = conn.accept_uni().await;
        });

        let (endpoint, conn) = client_connect(server_addr).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = C2A::Hello(Hello {
            proto: gsa_protocol::PROTO_VERSION,
            client_name: "loopback".into(),
            decode_caps: DecodeCaps { codecs: vec![] },
        });
        send_msg(&mut send, &hello).await.unwrap();
        let back: C2A = recv_msg(&mut recv).await.unwrap();
        assert!(matches!(back, C2A::Hello(h) if h.client_name == "loopback"));

        let mut got = 0;
        while got < 20 {
            let d = conn.read_datagram().await.unwrap();
            assert_eq!(d.len(), 100);
            got += 1;
        }

        conn.close(0u32.into(), b"done");
        endpoint.wait_idle().await;
        server_task.await.unwrap();
    }
}
