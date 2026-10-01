// ============================================================================
// LADEX — TLS for the browser UI and the node-to-node mesh.
//
// Plain HTTP means every device that ISN'T localhost — i.e. every other
// phone, tablet, or laptop on the LAN, the entire point of LADEX — loads the
// page as an insecure context. Two things the app depends on are silently
// disabled there:
//   - showSaveFilePicker() (File System Access API): without it, every
//     remote download falls back to buffering the whole file as a Blob in
//     RAM, defeating the O(1)-memory streaming design for large files.
//   - crypto.subtle: without it, SHA-256 integrity verification can't run
//     on either side of a transfer.
//
// The connection handling itself lives in server.rs; this file builds the
// certificate and the rustls configs.
//
// The certificate is self-signed (rcgen) and covers every local IPv4
// address plus localhost, so nodes stay reachable by IP from any browser.
// It's cached under ~/.ladex so a device that already clicked through the
// "not secure" warning once won't be asked again on the next run, as long
// as the machine's IP set hasn't changed.
// ============================================================================

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio_rustls::TlsConnector;

/// Installs the process-wide rustls crypto provider (ring). Must run once
/// before any TLS config is built. Safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Every local, non-loopback IPv4 address this machine currently has.
/// Used both for the printed "access from network" hint and as SAN entries
/// on the self-signed certificate — a device dialing any of these IPs must
/// see a cert that actually covers that IP, or the browser adds a second
/// warning (cert mismatch) on top of the expected self-signed one.
// Common virtual/container interface name prefixes — their addresses are
// only reachable from inside that virtual network, not from other devices
// on the actual LAN, so advertising them (in the TLS cert, the printed
// URLs, or mDNS) would just point people at a dead end. Not exhaustive,
// but covers the tools people are overwhelmingly likely to have running
// alongside LADEX on a dev machine.
const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "docker", "br-", "veth", "virbr", "tun", "tap", "podman", "lxcbr", "vmnet", "vboxnet",
];

fn is_virtual_iface(name: &str) -> bool {
    VIRTUAL_IFACE_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub fn local_ipv4_addresses() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter(|i| !i.is_loopback() && !is_virtual_iface(&i.name))
            .filter_map(|i| match i.ip() {
                IpAddr::V4(v4) => Some(IpAddr::V4(v4)),
                IpAddr::V6(_) => None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("TLS: could not enumerate network interfaces: {e}");
            Vec::new()
        }
    }
}

fn config_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ladex")
}

/// Loads a cached self-signed cert/key pair if it already covers exactly
/// `sans`, else generates a fresh pair and caches it to disk for next run.
pub fn load_or_generate_cert(sans: &[String]) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let dir = config_dir();
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    let sans_path = dir.join("cert.sans");

    if let (Ok(cert_bytes), Ok(key_bytes), Ok(cached_sans)) =
        (std::fs::read(&cert_path), std::fs::read(&key_path), std::fs::read_to_string(&sans_path))
    {
        let mut cached: Vec<&str> = cached_sans.lines().collect();
        let mut wanted: Vec<&str> = sans.iter().map(String::as_str).collect();
        cached.sort_unstable();
        wanted.sort_unstable();
        if cached == wanted {
            tracing::info!("TLS: reusing cached certificate ({} SAN entries)", sans.len());
            return Ok((
                CertificateDer::from(cert_bytes),
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
            ));
        }
        tracing::info!("TLS: local network addresses changed — regenerating certificate");
    }

    tracing::info!("TLS: generating a new self-signed certificate ({} SAN entries)", sans.len());
    let key_pair = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(sans.to_vec())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "LADEX");
    let cert = params.self_signed(&key_pair)?;

    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_bytes = key_pair.serialize_der();

    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&cert_path, &cert_der);
        let _ = std::fs::write(&key_path, &key_bytes);
        let _ = std::fs::write(&sans_path, sans.join("\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
    } else {
        tracing::warn!("TLS: could not create {} — certificate won't be cached across runs", dir.display());
    }

    Ok((cert_der, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes))))
}

pub fn build_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok(Arc::new(config))
}

// ---------------------------------------------------------------------------
// Client side — mesh nodes dial each other over that same TLS-only port, so
// connect_wss() needs a rustls client that can complete a handshake against
// our own self-signed certs. There's no shared CA between nodes (each one
// mints its own on first run), so the TLS layer itself can't tell a real peer
// from an impostor and accepts any certificate.
//
// That is safe only because the mesh handshake (mesh.rs, auth.rs) binds the
// certificate to the passphrase: connect_wss() returns the fingerprint of the
// certificate it actually saw, and the SPAKE2 proofs are computed over it. A
// man in the middle presenting their own certificate can't produce a valid
// proof without the passphrase.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct NoServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
            .map(|_| rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
            .map(|_| rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub fn build_client_config() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoServerVerification(provider)))
        .with_no_client_auth();
    Arc::new(config)
}

/// Dials `addr:port` over TLS and completes the WebSocket upgrade at `path`.
/// tokio-tungstenite's own TLS connectors pull in a different rustls major
/// version than we do here, so we do the TCP + TLS handshake ourselves with
/// our own rustls, then hand the resulting stream to `client_async` — which
/// only needs `AsyncRead + AsyncWrite`, not any particular TLS crate.
/// Also returns the fingerprint of the server certificate the handshake saw.
pub async fn connect_wss(
    addr: IpAddr,
    port: u16,
    path: &str,
    client_config: Arc<rustls::ClientConfig>,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
    Vec<u8>,
)> {
    let tcp = TcpStream::connect((addr, port)).await?;
    let _ = tcp.set_nodelay(true);
    let connector = TlsConnector::from(client_config);
    let server_name = ServerName::IpAddress(addr.into());
    let tls_stream = connector.connect(server_name, tcp).await?;
    let server_fingerprint = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| crate::auth::tls_fingerprint(cert.as_ref()))
        .ok_or_else(|| anyhow::anyhow!("server presented no certificate"))?;
    let url = format!("wss://{addr}:{port}{path}");
    let (ws_stream, response) = tokio_tungstenite::client_async(&url, tls_stream).await?;
    Ok((ws_stream, response, server_fingerprint))
}
