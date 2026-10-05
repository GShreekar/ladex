//! TLS for the browser UI and the mesh: a cached self-signed certificate covering every local address.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio_rustls::TlsConnector;

/// Installs the process-wide rustls crypto provider (ring); safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Every local, non-loopback IPv4 address, for the printed URLs and the certificate's names.
// Virtual and container interfaces aren't reachable from other devices, so their addresses are never advertised.
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

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ladex")
}

// Old addresses are kept so a DHCP lease flipping between a few doesn't mint a new certificate each time.
const MAX_REMEMBERED_ADDRESSES: usize = 16;

struct Cached {
    cert: Vec<u8>,
    key: Vec<u8>,
    sans: Vec<String>,
}

fn read_cached(dir: &Path) -> Option<Cached> {
    Some(Cached {
        cert: std::fs::read(dir.join("cert.der")).ok()?,
        key: std::fs::read(dir.join("key.der")).ok()?,
        sans: std::fs::read_to_string(dir.join("cert.sans")).ok()?.lines().map(String::from).collect(),
    })
}

// Created owner-only from the start, never world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // A file from an older version may be too permissive; tighten it before writing key bytes.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)
}

pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// The names the certificate must cover, current ones first, plus recently used addresses.
fn certificate_names(wanted: &[String], previous: &[String]) -> Vec<String> {
    let mut names = wanted.to_vec();
    let remembered = previous
        .iter()
        .filter(|name| name.parse::<IpAddr>().is_ok() && !names.contains(name))
        .take(MAX_REMEMBERED_ADDRESSES)
        .cloned()
        .collect::<Vec<_>>();
    names.extend(remembered);
    names
}

fn generate(names: &[String]) -> anyhow::Result<(CertificateDer<'static>, Vec<u8>)> {
    let key_pair = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, "LADEX");
    let cert = params.self_signed(&key_pair)?;
    Ok((cert.der().clone(), key_pair.serialize_der()))
}

/// The server's TLS config from the cached certificate if it still covers `wanted`, else a new one, plus its SHA-256 fingerprint.
pub fn prepare_server_identity(wanted: &[String]) -> anyhow::Result<(Arc<rustls::ServerConfig>, Vec<u8>)> {
    let dir = config_dir();
    let cached = read_cached(&dir);

    if let Some(c) = &cached {
        if wanted.iter().all(|name| c.sans.contains(name)) {
            let cert = CertificateDer::from(c.cert.clone());
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(c.key.clone()));
            match build_server_config(cert, key) {
                Ok(config) => {
                    tracing::info!("TLS: reusing cached certificate ({} names)", c.sans.len());
                    let _ = write_private(&dir.join("key.der"), &c.key);
                    return Ok((config, crate::auth::tls_fingerprint(&c.cert)));
                }
                Err(e) => tracing::warn!("TLS: cached certificate is unusable ({e}) — generating a new one"),
            }
        } else {
            tracing::info!("TLS: local network addresses changed — generating a new certificate");
        }
    }

    let names = certificate_names(wanted, cached.as_ref().map_or(&[][..], |c| &c.sans));
    tracing::info!("TLS: generating a new self-signed certificate ({} names)", names.len());
    let (cert, key) = generate(&names)?;

    match create_private_dir(&dir)
        .and_then(|_| write_private(&dir.join("key.der"), &key))
        .and_then(|_| std::fs::write(dir.join("cert.der"), &cert))
        .and_then(|_| std::fs::write(dir.join("cert.sans"), names.join("\n")))
    {
        Ok(()) => {}
        Err(e) => tracing::warn!("TLS: could not save the certificate to {} ({e}) — it won't be reused next run", dir.display()),
    }

    let fingerprint = crate::auth::tls_fingerprint(cert.as_ref());
    let config = build_server_config(cert, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))?;
    Ok((config, fingerprint))
}

/// A fresh certificate that is never written to disk, for in-process test nodes.
#[cfg(any(test, feature = "test-support"))]
pub fn ephemeral_server_identity(names: &[String]) -> anyhow::Result<(Arc<rustls::ServerConfig>, Vec<u8>)> {
    let (cert, key) = generate(names)?;
    let fingerprint = crate::auth::tls_fingerprint(cert.as_ref());
    let config = build_server_config(cert, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))?;
    Ok((config, fingerprint))
}

/// "AB:CD:EF:…" — the form browsers show in their certificate details.
pub fn format_fingerprint(fingerprint: &[u8]) -> String {
    fingerprint.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn build_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok(Arc::new(config))
}

// Nodes share no CA, so the client accepts any certificate; the mesh handshake binds its fingerprint, which defeats a man in the middle.

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

/// Dials `addr:port` over TLS with our own rustls and upgrades to a WebSocket at `path`; returns the server certificate's fingerprint.
pub async fn connect_wss(
    addr: IpAddr,
    port: u16,
    path: &str,
    client_config: Arc<rustls::ClientConfig>,
    ws_config: tokio_tungstenite::tungstenite::protocol::WebSocketConfig,
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
    let (ws_stream, response) = tokio_tungstenite::client_async_with_config(&url, tls_stream, Some(ws_config)).await?;
    Ok((ws_stream, response, server_fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn old_addresses_are_remembered_after_the_current_ones() {
        let all = certificate_names(
            &names(&["localhost", "192.168.1.9"]),
            &names(&["localhost", "192.168.1.5", "10.0.0.2", "ladex.local"]),
        );
        assert_eq!(all, names(&["localhost", "192.168.1.9", "192.168.1.5", "10.0.0.2"]));
    }

    #[test]
    fn remembered_addresses_are_bounded() {
        let previous: Vec<String> = (0..100).map(|i| format!("10.0.{i}.1")).collect();
        let all = certificate_names(&names(&["localhost"]), &previous);
        assert_eq!(all.len(), 1 + MAX_REMEMBERED_ADDRESSES);
    }

    #[test]
    fn fingerprints_print_like_a_browser_shows_them() {
        assert_eq!(format_fingerprint(&[0x0a, 0xff, 0x00]), "0A:FF:00");
    }

    #[test]
    fn a_generated_certificate_covers_every_name_and_loads() {
        install_crypto_provider();
        let wanted = names(&["localhost", "127.0.0.1", "ladex.local", "192.168.1.9"]);
        let (cert, key) = generate(&wanted).unwrap();
        assert!(build_server_config(cert, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_never_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ladex-test-{}", std::process::id()));
        create_private_dir(&dir).unwrap();
        let path = dir.join("key.der");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"key").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

        let fresh = dir.join("fresh.der");
        write_private(&fresh, b"key").unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
