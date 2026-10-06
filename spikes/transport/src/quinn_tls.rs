//! mTLS for the quinn path: self-signed Ed25519 certificates, pinned by fingerprint instead of a CA.

use std::sync::Arc;

use anyhow::Context;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::wire::ALPN;

pub const SERVER_NAME: &str = "ladex-spike";

/// A node's certificate and key; in v3 the key would be the node's Ed25519 identity.
pub struct NodeCert {
    pub certificate: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl NodeCert {
    pub fn generate() -> anyhow::Result<Self> {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let certificate = rcgen::CertificateParams::new(vec![SERVER_NAME.into()])?.self_signed(&key_pair)?;
        Ok(Self { certificate: certificate.der().clone(), key: PrivatePkcs8KeyDer::from(key_pair.serialize_der()) })
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.certificate)
    }

    fn chain_and_key(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (vec![self.certificate.clone()], PrivateKeyDer::Pkcs8(self.key.clone_key()))
    }
}

pub fn fingerprint(certificate: &CertificateDer<'_>) -> String {
    blake3::hash(certificate).to_hex().to_string()
}

pub fn server_config(cert: &NodeCert) -> anyhow::Result<quinn::ServerConfig> {
    let provider = provider();
    let (chain, key) = cert.chain_and_key();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyClientKey { algorithms: provider.signature_verification_algorithms }))
        .with_single_cert(chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls).context("rustls config is not usable for QUIC")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
}

pub fn client_config(cert: &NodeCert, pinned_server_fingerprint: String) -> anyhow::Result<quinn::ClientConfig> {
    let provider = provider();
    let (chain, key) = cert.chain_and_key();
    let verifier = PinnedServer { fingerprint: pinned_server_fingerprint, algorithms: provider.signature_verification_algorithms };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).context("rustls config is not usable for QUIC")?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts the server only if its certificate matches the fingerprint from the ticket.
#[derive(Debug)]
struct PinnedServer {
    fingerprint: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) != self.fingerprint {
            return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Requires a client certificate and proof of its key, but trusts any key: v3 would check the trust store here.
#[derive(Debug)]
struct AnyClientKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
