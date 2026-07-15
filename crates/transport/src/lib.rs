//! QUIC transport (spec 04): one connection per session carrying video
//! datagrams, a reliable input stream, and reliable control streams.
//!
//! Trust model (spec 06): persistent self-signed identities pinned by their
//! cert SHA-256. A streaming connection is mutual-TLS — the client pins the
//! agent ([`client_connect_pinned`]) and the agent pins the client
//! ([`server_endpoint`] with a peer store). Pairing runs over an anonymous
//! connection ([`client_connect_anonymous`]) where SPAKE2 provides the
//! authentication before any pins exist.

mod identity;
pub mod logsink;
mod pairing;
mod peers;
mod pinned;
mod stream;

pub use identity::{Identity, fingerprint};
pub use pairing::{AgentPairing, ClientConfirmed, ClientPairing, generate_code};
pub use peers::{Peer, PeerStore};
pub use pinned::{PinnedClientVerifier, PinnedServerVerifier};
pub use stream::{recv_msg, send_msg};

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use gsa_core::{Error, Result};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{EndpointConfig, TransportConfig};
use rustls::pki_types::CertificateDer;
use socket2::{Domain, Protocol, Socket, Type};

/// ALPN for our protocol; version-gated at the TLS layer.
pub const ALPN: &[u8] = b"gsa/0";

/// UDP/datagram buffer target (bytes): large enough to absorb one frame's
/// datagram burst without dropping, small enough to bound queuing latency.
/// The OS clamps the socket buffer to its own max.
const SOCKET_BUFFER_BYTES: usize = 8 * 1024 * 1024;

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

/// Fixed-window congestion controller for the media tunnel: rate control is
/// the application estimator's job; the transport must not second-guess it
/// by pacing datagrams to its own bandwidth opinion. Reliable streams share
/// the window but carry little.
#[derive(Debug, Clone)]
pub struct PermissiveCcConfig {
    /// Fixed congestion window (bytes): sized for the protocol maximum at a
    /// generous rtt (150 Mb/s x 400 ms).
    pub window: u64,
}

impl Default for PermissiveCcConfig {
    fn default() -> Self {
        Self {
            window: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
struct PermissiveCc {
    window: u64,
}

impl quinn::congestion::Controller for PermissiveCc {
    fn on_congestion_event(
        &mut self,
        _now: std::time::Instant,
        _sent: std::time::Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn quinn::congestion::Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.window
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

impl quinn::congestion::ControllerFactory for PermissiveCcConfig {
    fn build(
        self: Arc<Self>,
        _now: std::time::Instant,
        _current_mtu: u16,
    ) -> Box<dyn quinn::congestion::Controller> {
        Box::new(PermissiveCc {
            window: self.window,
        })
    }
}

/// Shared transport tuning for both ends.
fn transport_config() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).expect("valid"),
    ));
    // Match the OS buffer so quinn isn't the ceiling on bursty drains.
    tc.datagram_receive_buffer_size(Some(SOCKET_BUFFER_BYTES));
    // Cap the datagram send queue (~60 ms at 35 Mb/s) so a sender outpacing the
    // path sheds stale datagrams instead of queueing seconds of them.
    tc.datagram_send_buffer_size(256 * 1024);
    // A fixed permissive window: the application estimator owns rate control;
    // a transport controller pacing datagrams to its own bandwidth opinion
    // strangles the estimator's probes (measured on radio links).
    // GSA_TUNNEL_CC=bbr restores the old behavior for A/B diagnosis.
    if std::env::var("GSA_TUNNEL_CC").as_deref() == Ok("bbr") {
        tc.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    } else {
        tc.congestion_controller_factory(Arc::new(PermissiveCcConfig::default()));
    }
    // Pin the UDP payload to the QUIC baseline (1200 B) and disable Path-MTU
    // discovery — discovery overshoots on reduced-MTU paths (VPN/5G) and the
    // oversized datagrams get dropped. (TODO: make this a knob / re-enable
    // discovery once we handle the back-off correctly across platforms.)
    tc.initial_mtu(1200);
    tc.mtu_discovery_config(None);
    tc
}

/// Build a listening endpoint from an identity. Returns the endpoint;
/// its local address carries the OS-assigned port when `addr` used :0.
///
/// With `peers`, the endpoint requires mutual TLS: a client that presents a
/// cert must be a paired peer, while an anonymous client (pairing) is still
/// admitted. Without it (dev-open mode) all clients connect anonymously.
pub fn server_endpoint(
    addr: SocketAddr,
    identity: &Identity,
    peers: Option<Arc<PeerStore>>,
) -> Result<quinn::Endpoint> {
    let base = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("tls versions: {e}")))?;
    let with_auth = match peers {
        Some(store) => base.with_client_cert_verifier(Arc::new(PinnedClientVerifier::new(store))),
        None => base.with_no_client_auth(),
    };
    let mut tls = with_auth
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

/// Connect anonymously (no client cert; accept any agent cert). Used for
/// pairing — where SPAKE2 authenticates and no pins exist yet — and for
/// dev-open streaming. The agent's fingerprint is logged at `INFO`.
pub async fn client_connect_anonymous(
    addr: SocketAddr,
) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let tls = client_tls_base()?
        .with_custom_certificate_verifier(Arc::new(identity::DevTrustVerifier::new()))
        .with_no_client_auth();
    connect_with(addr, tls).await
}

/// Connect with pinned mutual TLS: verify the agent against `agent_pin` and
/// present `identity` as the client cert (its fingerprint is the pin the
/// agent recorded at pairing). Returns the endpoint (keep it alive) + conn.
pub async fn client_connect_pinned(
    addr: SocketAddr,
    agent_pin: &str,
    identity: &Identity,
) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let tls = client_tls_base()?
        .with_custom_certificate_verifier(Arc::new(PinnedServerVerifier::new(
            agent_pin.to_string(),
        )))
        .with_client_auth_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .map_err(|e| Error::Transport(format!("client auth cert: {e}")))?;
    connect_with(addr, tls).await
}

/// The pin (cert SHA-256) of the connected peer, if it presented a client
/// cert. `None` for an anonymous (pairing) connection.
#[must_use]
pub fn peer_pin(conn: &quinn::Connection) -> Option<String> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(identity::fingerprint)
}

fn client_tls_base() -> Result<rustls::client::danger::DangerousClientConfigBuilder> {
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("tls versions: {e}")))?
        .dangerous())
}

/// Bind an ephemeral client endpoint and open a connection with the given TLS.
async fn connect_with(
    addr: SocketAddr,
    mut tls: rustls::ClientConfig,
) -> Result<(quinn::Endpoint, quinn::Connection)> {
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

    let conn = endpoint
        .connect(addr, "gsa-agent")
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?
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
        let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &identity, None).unwrap();
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

        let (endpoint, conn) = client_connect_anonymous(server_addr).await.unwrap();
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

    fn tmp_store(tag: &str) -> Arc<PeerStore> {
        let dir = std::env::temp_dir().join(format!("gsa-mtls-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Arc::new(PeerStore::load_or_empty(&dir).unwrap())
    }

    #[tokio::test]
    async fn pinned_streaming_and_anonymous_pairing() {
        use gsa_protocol::grant::Scope;
        let agent = Identity::generate().unwrap();
        let client = Identity::generate().unwrap();
        let store = tmp_store("ok");
        store
            .add("laptop".into(), client.fingerprint(), Scope::Interact)
            .unwrap();

        let server = server_endpoint("127.0.0.1:0".parse().unwrap(), &agent, Some(store)).unwrap();
        let addr = server.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let c1 = server.accept().await.unwrap().await.unwrap();
            let pinned = peer_pin(&c1).is_some();
            let c2 = server.accept().await.unwrap().await.unwrap();
            let anon = peer_pin(&c2).is_none();
            (pinned, anon)
        });

        // Paired client: pinned mutual TLS presents a client pin.
        let (_e1, _c1) = client_connect_pinned(addr, &agent.fingerprint(), &client)
            .await
            .unwrap();
        // Anonymous client (pairing): admitted with no pin.
        let (_e2, _c2) = client_connect_anonymous(addr).await.unwrap();

        let (pinned, anon) = srv.await.unwrap();
        assert!(pinned, "paired client presents its pin");
        assert!(anon, "anonymous client presents no pin");
    }

    #[tokio::test]
    async fn stranger_client_cert_is_rejected() {
        let agent = Identity::generate().unwrap();
        let stranger = Identity::generate().unwrap();
        let server = server_endpoint(
            "127.0.0.1:0".parse().unwrap(),
            &agent,
            Some(tmp_store("bad")),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            if let Some(inc) = server.accept().await {
                let _ = inc.await; // handshake rejects the stranger cert
            }
        });

        // In TLS 1.3 the client finishes its side before the server validates
        // the client cert, so connect() may resolve; the rejection surfaces as
        // the server closing the connection immediately after.
        let closed = match client_connect_pinned(addr, &agent.fingerprint(), &stranger).await {
            Ok((_ep, conn)) => tokio::time::timeout(Duration::from_secs(5), conn.closed())
                .await
                .is_ok(),
            Err(_) => true, // rejected outright — also acceptable
        };
        assert!(
            closed,
            "an unpaired client must not hold a usable connection"
        );
        srv.abort();
    }
}
