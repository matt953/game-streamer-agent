//! Mutual TLS to a paired host.
//!
//! Both certificates here are self-signed, so ordinary web PKI has nothing to
//! say about either of them. Instead we pin: the host is trusted if and only
//! if it presents the exact certificate we received during pairing, and it
//! trusts us because we present the certificate it stored then. That is a
//! stronger guarantee than a CA chain — it names one specific machine.

use crate::http;
use crate::identity::ClientIdentity;
use gsa_core::{Error, Result};
use std::sync::Arc;

/// Accepts exactly one certificate: the one pairing gave us.
#[derive(Debug)]
struct PinnedHost {
    expected: Vec<u8>,
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedHost {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "host presented a different certificate than the one we paired with".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// First DER certificate in a PEM document.
fn cert_der(pem: &str) -> Result<Vec<u8>> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| Error::Session(format!("parse certificate PEM: {e}")))?;
    Ok(block.contents)
}

fn client_config(host_cert_pem: &str, identity: &ClientIdentity) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let supported = provider.signature_verification_algorithms;

    let key = rustls_pemfile::private_key(&mut identity.key_pem().as_bytes())
        .map_err(|e| Error::Session(format!("read client key: {e}")))?
        .ok_or_else(|| Error::Session("client key PEM has no key".into()))?;

    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Transport(format!("tls versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedHost {
            expected: cert_der(host_cert_pem)?,
            supported,
        }))
        .with_client_auth_cert(
            vec![rustls::pki_types::CertificateDer::from(cert_der(
                identity.cert_pem(),
            )?)],
            key,
        )
        .map_err(|e| Error::Transport(format!("client auth cert: {e}")))
}

/// One `GET` against a paired host's TLS port.
pub(crate) async fn get(
    addr: std::net::SocketAddr,
    host_cert_pem: &str,
    identity: &ClientIdentity,
    path_and_query: &str,
) -> Result<Vec<u8>> {
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(client_config(host_cert_pem, identity)?));
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| Error::Transport(format!("connect {addr}: {e}")))?;
    let _ = tcp.set_nodelay(true);
    // The certificate is pinned, so the name is only an SNI formality; hosts
    // are reached by address and their certs carry no useful name anyway.
    let server_name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Transport(format!("tls handshake with {addr}: {e}")))?;
    http::exchange(&mut tls, addr, path_and_query).await
}
