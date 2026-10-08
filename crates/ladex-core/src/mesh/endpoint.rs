//! This node's QUIC endpoint: it accepts and dials mesh connections, mutually authenticated with each node's Ed25519 key.
//!
//! TLS carries the bare identity key (RFC 7250 raw public keys), so a peer's node id is `node_id_for` the key it proved
//! in the handshake. Whether that node is trusted or revoked is the mesh handshake's call, not this layer's.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use ed25519_dalek::VerifyingKey;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::AlwaysResolvesClientRawPublicKeys;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::AlwaysResolvesServerRawPublicKeys;
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, PeerIncompatible, SignatureScheme};

use crate::identity::{node_id_for, Identity};
use crate::types::NodeId;

pub const ALPN: &[u8] = b"ladex-mesh";

/// A peer stays connected through brief silences. A vanished one is noticed within the idle timeout plus one keep-alive interval,
/// since sending a keep-alive restarts the idle timer; that is 13 s, inside the 15 s the mesh promises.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(3);

/// Close code for a connection a node ended on purpose, so the other node doesn't redial it.
pub const HUNG_UP: quinn::VarInt = quinn::VarInt::from_u32(1);

// Peers are addressed by key, not by name; SNI just needs a valid DNS name.
const SERVER_NAME: &str = "ladex";

const CHANNEL_BINDING_LABEL: &[u8] = b"EXPORTER-ladex-mesh-channel-binding";
const CHANNEL_BINDING_LEN: usize = 32;

// DER prefixes of an Ed25519 SubjectPublicKeyInfo and PKCS#8 v1 private key (RFC 8410); the 32 key bytes follow.
const ED25519_SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
const ED25519_PKCS8_PREFIX: [u8; 16] = [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];

/// The one QUIC endpoint of a node, serving incoming connections and dialing out from the same UDP socket.
pub struct MeshEndpoint {
    endpoint: quinn::Endpoint,
    provider: Arc<CryptoProvider>,
    our_key: Arc<CertifiedKey>,
}

/// A mesh connection whose far end has proven it holds the key behind `node_id`.
#[derive(Debug, Clone)]
pub struct PeerConnection {
    pub connection: quinn::Connection,
    pub node_id: NodeId,
    pub public_key: VerifyingKey,
}

impl PeerConnection {
    /// The same bytes on both ends of this connection, and different on each leg of a relayed one; feeds the mesh handshake.
    pub fn channel_binding(&self) -> anyhow::Result<Vec<u8>> {
        let mut binding = vec![0u8; CHANNEL_BINDING_LEN];
        self.connection
            .export_keying_material(&mut binding, CHANNEL_BINDING_LABEL, &[])
            .map_err(|_| anyhow!("the TLS session cannot export keying material"))?;
        Ok(binding)
    }
}

/// A connection attempt from a peer that has not finished its TLS handshake yet.
pub struct Accepting(quinn::Incoming);

impl Accepting {
    pub fn remote_address(&self) -> SocketAddr {
        self.0.remote_address()
    }

    /// Completes the handshake; fails unless the peer proved an Ed25519 key.
    pub async fn establish(self) -> anyhow::Result<PeerConnection> {
        let connection = self.0.await?;
        peer_connection(connection)
    }
}

impl MeshEndpoint {
    /// Binds the node's UDP socket; port 0 picks a free one.
    pub fn bind(identity: &Identity, addr: SocketAddr) -> anyhow::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let our_key = certified_key(identity, &provider)?;
        let server_config = server_config(&provider, &our_key)?;
        let endpoint =
            quinn::Endpoint::server(server_config, addr).with_context(|| format!("could not bind the mesh endpoint on {addr}"))?;
        Ok(Self { endpoint, provider, our_key })
    }

    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Dials `addr` and succeeds only if the node there holds the key behind `expected`.
    pub async fn connect(&self, addr: SocketAddr, expected: &NodeId) -> anyhow::Result<PeerConnection> {
        let peer = self.dial(addr, Some(expected.clone())).await?;
        // The verifier already checked this; a mismatch here would mean the TLS layer was bypassed.
        if &peer.node_id != expected {
            bail!("dialed node {expected} but connected to {}", peer.node_id);
        }
        Ok(peer)
    }

    /// Dials `addr` without knowing who is there; the node proves the key it holds, and the mesh handshake decides whether to trust it.
    pub async fn connect_to_address(&self, addr: SocketAddr) -> anyhow::Result<PeerConnection> {
        self.dial(addr, None).await
    }

    async fn dial(&self, addr: SocketAddr, expected: Option<NodeId>) -> anyhow::Result<PeerConnection> {
        let client_config = client_config(&self.provider, &self.our_key, expected)?;
        let connection = self.endpoint.connect_with(client_config, addr, SERVER_NAME)?.await?;
        peer_connection(connection)
    }

    /// The next connection attempt, or None once the endpoint is closed.
    pub async fn accept(&self) -> Option<Accepting> {
        self.endpoint.accept().await.map(Accepting)
    }

    /// Closes every connection at once; peers are told, but nothing waits for them to hear it.
    pub fn close(&self) {
        self.endpoint.close(HUNG_UP, b"node shutting down");
    }

    /// Waits until every closed connection has told its peer.
    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

fn certified_key(identity: &Identity, provider: &CryptoProvider) -> anyhow::Result<Arc<CertifiedKey>> {
    let pkcs8 = [&ED25519_PKCS8_PREFIX[..], &identity.secret_bytes()].concat();
    let signing_key = provider.key_provider.load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8)))?;
    let public_key = CertificateDer::from(encode_public_key(&identity.public_key()));
    Ok(Arc::new(CertifiedKey::new(vec![public_key], signing_key)))
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("10 s fits a QUIC idle timeout")));
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    Arc::new(transport)
}

fn server_config(provider: &Arc<CryptoProvider>, our_key: &Arc<CertifiedKey>) -> anyhow::Result<quinn::ServerConfig> {
    let verifier = AnyNodeKey { algorithms: provider.signature_verification_algorithms };
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(our_key.clone())));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport_config());
    Ok(config)
}

fn client_config(
    provider: &Arc<CryptoProvider>,
    our_key: &Arc<CertifiedKey>,
    expected: Option<NodeId>,
) -> anyhow::Result<quinn::ClientConfig> {
    let verifier = ExpectedNode { node_id: expected, algorithms: provider.signature_verification_algorithms };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(our_key.clone())));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport_config());
    Ok(config)
}

fn peer_connection(connection: quinn::Connection) -> anyhow::Result<PeerConnection> {
    let presented = connection
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|keys| keys.into_iter().next())
        .context("the peer presented no key")?;
    let public_key = decode_public_key(&presented).context("the peer's key is not an Ed25519 key")?;
    Ok(PeerConnection { connection, node_id: node_id_for(&public_key), public_key })
}

fn encode_public_key(public_key: &VerifyingKey) -> Vec<u8> {
    [&ED25519_SPKI_PREFIX[..], public_key.as_bytes()].concat()
}

/// None for anything but a well-formed Ed25519 SubjectPublicKeyInfo.
fn decode_public_key(spki: &[u8]) -> Option<VerifyingKey> {
    let key_bytes: &[u8; 32] = spki.strip_prefix(&ED25519_SPKI_PREFIX)?.try_into().ok()?;
    VerifyingKey::from_bytes(key_bytes).ok()
}

fn verify_raw_key_signature(
    message: &[u8],
    presented: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
    algorithms: &WebPkiSupportedAlgorithms,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    let spki = SubjectPublicKeyInfoDer::from(presented.as_ref());
    rustls::crypto::verify_tls13_signature_with_raw_key(message, &spki, signature, algorithms)
}

fn bad_key() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::BadEncoding)
}

/// The dialer's check: the server must hold the key of the node it meant to reach, or any Ed25519 key when it dialed an address.
#[derive(Debug)]
struct ExpectedNode {
    node_id: Option<NodeId>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ExpectedNode {
    fn verify_server_cert(
        &self,
        presented: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let public_key = decode_public_key(presented).ok_or_else(bad_key)?;
        if self.node_id.as_ref().is_some_and(|expected| &node_id_for(&public_key) != expected) {
            return Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _presented: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(PeerIncompatible::Tls12NotOffered.into())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        presented: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_raw_key_signature(message, presented, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The listener's check: any node may connect if it proves an Ed25519 key; the mesh handshake decides whether to trust it.
#[derive(Debug)]
struct AnyNodeKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyNodeKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        presented: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        decode_public_key(presented).ok_or_else(bad_key)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _presented: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(PeerIncompatible::Tls12NotOffered.into())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        presented: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_raw_key_signature(message, presented, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    struct Node {
        identity: Identity,
        endpoint: MeshEndpoint,
    }

    fn node() -> Node {
        let identity = Identity::generate();
        let endpoint = MeshEndpoint::bind(&identity, loopback()).unwrap();
        Node { identity, endpoint }
    }

    fn node_id(node: &Node) -> NodeId {
        node.identity.node_id().to_string()
    }

    /// Dials `to` as `expected` while `to` accepts; returns both ends' results.
    async fn dial(from: &Node, to: &Node, expected: &NodeId) -> (anyhow::Result<PeerConnection>, anyhow::Result<PeerConnection>) {
        let addr = to.endpoint.local_addr().unwrap();
        let accepting = async { to.endpoint.accept().await.unwrap().establish().await };
        tokio::join!(from.endpoint.connect(addr, expected), accepting)
    }

    #[tokio::test]
    async fn the_dialer_learns_the_node_id_it_asked_for() {
        let (a, b) = (node(), node());
        let (dialed, _) = dial(&a, &b, &node_id(&b)).await;
        assert_eq!(dialed.unwrap().node_id, node_id(&b));
    }

    #[tokio::test]
    async fn the_listener_learns_the_dialers_node_id_from_its_key() {
        let (a, b) = (node(), node());
        let (_, accepted) = dial(&a, &b, &node_id(&b)).await;
        let accepted = accepted.unwrap();
        assert_eq!(accepted.node_id, node_id(&a));
        assert_eq!(accepted.public_key, a.identity.public_key());
    }

    #[tokio::test]
    async fn a_node_answering_for_another_node_id_is_refused() {
        let (a, impostor, real) = (node(), node(), node());
        let (dialed, _) = dial(&a, &impostor, &node_id(&real)).await;
        assert!(dialed.is_err());
    }

    #[tokio::test]
    async fn the_listener_also_fails_when_the_dialer_refuses_its_key() {
        let (a, impostor, real) = (node(), node(), node());
        let (_, accepted) = dial(&a, &impostor, &node_id(&real)).await;
        assert!(accepted.is_err());
    }

    #[tokio::test]
    async fn a_node_presenting_a_key_it_does_not_hold_is_refused() {
        let (a, victim) = (node(), Identity::generate());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let impostor_signer = certified_key(&Identity::generate(), &provider).unwrap().key.clone();
        let forged = Arc::new(CertifiedKey::new(vec![CertificateDer::from(encode_public_key(&victim.public_key()))], impostor_signer));
        let impostor = quinn::Endpoint::server(server_config(&provider, &forged).unwrap(), loopback()).unwrap();
        let victim_id = victim.node_id().to_string();
        let accepting = async { impostor.accept().await.unwrap().await };
        let (dialed, _) = tokio::join!(a.endpoint.connect(impostor.local_addr().unwrap(), &victim_id), accepting);
        assert!(dialed.is_err());
    }

    #[tokio::test]
    async fn a_dialer_without_a_key_cannot_complete_the_tls_handshake() {
        let b = node();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = ExpectedNode { node_id: Some(node_id(&b)), algorithms: provider.signature_verification_algorithms };
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let anonymous = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
        let client = quinn::Endpoint::client(loopback()).unwrap();
        let dialing = async { client.connect_with(anonymous, b.endpoint.local_addr().unwrap(), SERVER_NAME).unwrap().await };
        let accepting = async { b.endpoint.accept().await.unwrap().establish().await };
        let (dialed, accepted) = tokio::join!(dialing, accepting);
        assert!(dialed.is_err() && accepted.is_err());
    }

    #[tokio::test]
    async fn dialing_an_address_learns_whichever_node_answers() {
        let (a, b) = (node(), node());
        let addr = b.endpoint.local_addr().unwrap();
        let accepting = async { b.endpoint.accept().await.unwrap().establish().await };
        let (dialed, _) = tokio::join!(a.endpoint.connect_to_address(addr), accepting);
        assert_eq!(dialed.unwrap().node_id, node_id(&b));
    }

    #[tokio::test]
    async fn both_ends_derive_the_same_channel_binding() {
        let (a, b) = (node(), node());
        let (dialed, accepted) = dial(&a, &b, &node_id(&b)).await;
        assert_eq!(dialed.unwrap().channel_binding().unwrap(), accepted.unwrap().channel_binding().unwrap());
    }

    #[tokio::test]
    async fn each_connection_has_its_own_channel_binding() {
        let (a, b) = (node(), node());
        let first = dial(&a, &b, &node_id(&b)).await.0.unwrap();
        let second = dial(&a, &b, &node_id(&b)).await.0.unwrap();
        assert_ne!(first.channel_binding().unwrap(), second.channel_binding().unwrap());
    }

    #[tokio::test]
    async fn streams_carry_data_between_authenticated_peers() {
        let (a, b) = (node(), node());
        let (dialed, accepted) = dial(&a, &b, &node_id(&b)).await;
        let (dialed, accepted) = (dialed.unwrap(), accepted.unwrap());
        let mut send = dialed.connection.open_uni().await.unwrap();
        send.write_all(b"hello").await.unwrap();
        send.finish().unwrap();
        let mut recv = accepted.connection.accept_uni().await.unwrap();
        assert_eq!(recv.read_to_end(64).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn accept_ends_once_the_endpoint_is_closed() {
        let a = node();
        a.endpoint.close();
        assert!(a.endpoint.accept().await.is_none());
    }

    #[test]
    fn a_public_key_survives_encoding_and_decoding() {
        let public_key = Identity::generate().public_key();
        assert_eq!(decode_public_key(&encode_public_key(&public_key)), Some(public_key));
    }

    #[test]
    fn a_key_of_another_algorithm_is_not_accepted() {
        let mut spki = encode_public_key(&Identity::generate().public_key());
        spki[8] = 0x71; // Ed448's OID instead of Ed25519's
        assert_eq!(decode_public_key(&spki), None);
    }

    #[test]
    fn a_truncated_key_is_not_accepted() {
        let spki = encode_public_key(&Identity::generate().public_key());
        assert_eq!(decode_public_key(&spki[..spki.len() - 1]), None);
    }

    #[test]
    fn the_listener_rejects_a_presented_key_that_is_not_ed25519() {
        let verifier = AnyNodeKey { algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms };
        let not_a_key = CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        assert!(verifier.verify_client_cert(&not_a_key, &[], UnixTime::now()).is_err());
    }

    #[test]
    fn the_node_key_loads_as_a_tls_signing_key() {
        let provider = rustls::crypto::ring::default_provider();
        assert!(certified_key(&Identity::generate(), &provider).is_ok());
    }
}
