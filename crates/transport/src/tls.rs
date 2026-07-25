use std::sync::Arc;

use identity::{LocalIdentity, TrustStore, TrustedPeer};
use quinn::{
    ClientConfig, ServerConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{RootCertStore, server::WebPkiClientVerifier};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use thiserror::Error;

use crate::SessionLimits;

const ALPN: &[u8] = b"tevir/1";

pub(crate) fn server_config(
    identity: &LocalIdentity,
    trust: &TrustStore,
    limits: &SessionLimits,
) -> Result<ServerConfig, TlsError> {
    let roots = roots(trust.peers())?;
    if roots.is_empty() {
        return Err(TlsError::NoTrustedPeers);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|error| TlsError::Configuration(error.to_string()))?;
    let mut crypto = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(TlsError::Rustls)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificate_chain(identity), private_key(identity))
        .map_err(TlsError::Rustls)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicServerConfig::try_from(crypto)
        .map_err(|error| TlsError::Configuration(error.to_string()))?;
    let mut config = ServerConfig::with_crypto(Arc::new(crypto));
    config.transport = limits.transport_config()?;
    Ok(config)
}

pub(crate) fn client_config(
    identity: &LocalIdentity,
    peer: &TrustedPeer,
    limits: &SessionLimits,
) -> Result<ClientConfig, TlsError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(peer.trust_anchor_der().to_vec()))
        .map_err(TlsError::Rustls)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(TlsError::Rustls)?
        .with_root_certificates(roots)
        .with_client_auth_cert(certificate_chain(identity), private_key(identity))
        .map_err(TlsError::Rustls)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(crypto)
        .map_err(|error| TlsError::Configuration(error.to_string()))?;
    let mut config = ClientConfig::new(Arc::new(crypto));
    config.transport_config(limits.transport_config()?);
    Ok(config)
}

fn roots<'a>(peers: impl Iterator<Item = &'a TrustedPeer>) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    for peer in peers {
        roots
            .add(CertificateDer::from(peer.trust_anchor_der().to_vec()))
            .map_err(TlsError::Rustls)?;
    }
    Ok(roots)
}

fn certificate_chain(identity: &LocalIdentity) -> Vec<CertificateDer<'static>> {
    identity
        .certificate_chain()
        .into_iter()
        .map(|certificate| CertificateDer::from(certificate.to_vec()))
        .collect()
}

fn private_key(identity: &LocalIdentity) -> PrivateKeyDer<'static> {
    PrivatePkcs8KeyDer::from(identity.private_key_der().to_vec()).into()
}

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("a secure server requires at least one trusted peer")]
    NoTrustedPeers,
    #[error("TLS configuration failed: {0}")]
    Rustls(rustls::Error),
    #[error("TLS configuration failed: {0}")]
    Configuration(String),
    #[error(transparent)]
    Limits(#[from] crate::LimitsError),
}
