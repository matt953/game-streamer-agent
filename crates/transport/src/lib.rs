//! QUIC transport (spec 04): one connection per session carrying video
//! datagrams, a reliable input stream, and reliable control streams.
//!
//! M0 trust model: self-signed per-run identities; the client uses a
//! deliberately-loud TOFU verifier (`DevTrustVerifier`) that logs the
//! agent's fingerprint. Real pairing + pinned mTLS replaces it at M2
//! (spec 06) — the verifier type is the seam.

mod identity;
mod peers;
mod stream;

pub use identity::{Identity, fingerprint};
pub use peers::{Peer, PeerStore};
pub use stream::{recv_msg, send_msg};

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use gsa_core::{Error, Result};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{EndpointConfig, TransportConfig};
use socket2::{Domain, Protocol, Socket, Type};

/// ALPN for our protocol; version-gated at the TLS layer.
pub const ALPN: &[u8] = b"gsa/0";

/// UDP/datagram buffer target (bytes): large enough to absorb one frame's
/// datagram burst without dropping, small enough to bound queuing latency.
/// The OS clamps the socket buffer to its own max.
const SOCKET_BUFFER_BYTES: usize = 1024 * 1024;

/// A non-blocking UDP socket bound to `addr` with enlarged buffers (quinn
/// requires non-blocking).
fn tuned_socket(addr: SocketAddr) -> Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| Error::Transport(format!("socket: {e}")))?;
    // Best-effort: a smaller buffer still helps, so don't fail if the OS says no.
    let _ = sock.set_recv_buffer_size(SOCKET_BUFFER_BYTES);
    let _ = sock.set_send_buffer_size(SOCKET_BUFFER_BYTES);
    tracing::debug!(
        requested = SOCKET_BUFFER_BYTES,
        recv = sock.recv_buffer_size().unwrap_or(0),
        send = sock.send_buffer_size().unwrap_or(0),
        "udp socket buffers (OS may clamp)"
    );
    sock.bind(&addr.into())
        .map_err(|e| Error::Transport(format!("bind {addr}: {e}")))?;
    let sock: UdpSocket = sock.into();
    sock.set_nonblocking(true)
        .map_err(|e| Error::Transport(format!("set_nonblocking: {e}")))?;
    Ok(sock)
}

/// Shared transport tuning for both ends.
fn transport_config() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).expect("valid"),
    ));
    // Match the OS buffer so quinn isn't the ceiling on bursty drains.
    tc.datagram_receive_buffer_size(Some(SOCKET_BUFFER_BYTES));
    tc
}

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
    server_config.transport = Arc::new(transport_config());

    let runtime =
        quinn::default_runtime().ok_or_else(|| Error::Transport("no async runtime".into()))?;
    quinn::Endpoint::new(
        EndpointConfig::default(),
        Some(server_config),
        tuned_socket(addr)?,
        runtime,
    )
    .map_err(|e| Error::Transport(format!("server endpoint {addr}: {e}")))
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
    let mut client_config = quinn::ClientConfig::new(Arc::new(quic));
    client_config.transport_config(Arc::new(transport_config()));

    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().expect("literal")
    } else {
        "[::]:0".parse().expect("literal")
    };
    let runtime =
        quinn::default_runtime().ok_or_else(|| Error::Transport("no async runtime".into()))?;
    let mut endpoint = quinn::Endpoint::new(
        EndpointConfig::default(),
        None,
        tuned_socket(bind)?,
        runtime,
    )
    .map_err(|e| Error::Transport(format!("client bind: {e}")))?;
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
            decode_caps: DecodeCaps {
                codecs: vec![],
                max_h264_profile: gsa_core::media::H264Profile::ConstrainedBaseline,
            },
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
