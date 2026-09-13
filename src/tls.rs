// ============================================================================
// LADEX — BUG-03 fix: TLS so remote devices get a secure context.
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
// warp 0.4.1 ships no usable TLS integration. Its `.tls()` builder is dead
// code gated on a `tls` Cargo feature that the crate's own Cargo.toml never
// declares (confirmed against the published manifest), and its
// `.incoming(acceptor)` escape hatch takes a private, unnameable `Accept`
// trait, so no external crate can plug a custom acceptor in either. Rather
// than fork warp, we run a small TLS-terminating proxy in front of it:
//
//   0.0.0.0:<port>  --TLS-->  [this proxy]  --plaintext-->  127.0.0.1:<port+1>
//
// warp keeps serving exactly as it does today, just on a loopback-only
// plaintext port instead of the public one. The proxy works at the raw byte
// level (tokio::io::copy_bidirectional), so it passes HTTP upgrades (our
// /ws and /mesh WebSocket routes) through untouched — no HTTP-aware code
// needed here at all.
//
// The certificate is self-signed (rcgen) and covers every local IPv4
// address plus localhost, so nodes stay reachable by IP from any browser.
// It's cached under ~/.ladex so a device that already clicked through the
// "not secure" warning once won't be asked again on the next run, as long
// as the machine's IP set hasn't changed.
// ============================================================================

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio_rustls::{TlsAcceptor, TlsConnector};

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
pub fn local_ipv4_addresses() -> Vec<IpAddr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter(|i| !i.is_loopback())
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

/// Runs the TLS-terminating proxy: accepts TLS on `public_addr`, forwards
/// decrypted bytes to `internal_addr` (warp's real, loopback-only listener).
/// Runs until the process exits or the listener errors out.
pub async fn run_tls_proxy(
    public_addr: SocketAddr,
    internal_addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let listener = TcpListener::bind(public_addr).await?;
    tracing::info!("TLS: terminating on {public_addr}, forwarding to {internal_addr}");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("TLS: accept error: {e}");
                continue;
            }
        };
        let _ = tcp.set_nodelay(true);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let mut tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("TLS: handshake with {peer} failed: {e}");
                    return;
                }
            };
            let mut plain = match TcpStream::connect(internal_addr).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TLS: could not reach internal server at {internal_addr}: {e}");
                    return;
                }
            };
            if let Err(e) = tokio::io::copy_bidirectional(&mut tls_stream, &mut plain).await {
                tracing::debug!("TLS: proxied connection with {peer} ended: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Client side — mesh nodes dial each other over that same TLS-only port, so
// connect_to_peer() needs a rustls client that can complete a handshake
// against our own self-signed certs. There's no shared CA between nodes
// (each one mints its own on first run), so full chain validation is
// impossible by construction; the mesh passphrase (Hello/HelloAck, Phase 7)
// is the actual trust boundary — TLS's job here is making the wire
// unreadable to a passive LAN sniffer, not authenticating the peer.
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
pub async fn connect_wss(
    addr: IpAddr,
    port: u16,
    path: &str,
    client_config: Arc<rustls::ClientConfig>,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let tcp = TcpStream::connect((addr, port)).await?;
    let _ = tcp.set_nodelay(true);
    let connector = TlsConnector::from(client_config);
    let server_name = ServerName::IpAddress(addr.into());
    let tls_stream = connector.connect(server_name, tcp).await?;
    let url = format!("wss://{addr}:{port}{path}");
    let (ws_stream, response) = tokio_tungstenite::client_async(&url, tls_stream).await?;
    Ok((ws_stream, response))
}
