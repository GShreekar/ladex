//! Starting and stopping a whole node in-process: what it serves, what it reports, and what a failed start leaves behind.

use std::net::SocketAddr;
use std::sync::Arc;

use ladex_core::node::{Node, NodeConfig, NodeHandle, Notice, StartError, UiAssets, WebUi};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const INDEX_PAGE: &[u8] = b"<!doctype html><title>stub ui</title>";

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        match path {
            "index.html" => Some(INDEX_PAGE),
            "join.html" => Some(b"<!doctype html><title>stub join</title>"),
            _ => None,
        }
    }
}

fn config(data_dir: &std::path::Path) -> NodeConfig {
    let web_ui = WebUi { assets: Arc::new(StubUi), is_legacy: false };
    let mut config = NodeConfig::new(data_dir.to_path_buf(), web_ui);
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    config
}

async fn get(port: u16, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n").expect("a complete response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).and_then(|code| code.parse().ok()).expect("a status code");
    (status, raw[split + 4..].to_vec())
}

async fn started(data_dir: &std::path::Path) -> NodeHandle {
    Node::start(config(data_dir)).await.expect("the node starts")
}

#[tokio::test]
async fn a_started_node_serves_its_page_on_the_port_it_reports() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let port = node.info().http_port;
    assert_ne!(port, 0, "a free port was picked and reported");
    assert_eq!(node.state().http_port, port);
    assert_eq!(node.info().quic_port, port, "the QUIC port follows the HTTP port by default");
    let (status, body) = get(port, "/").await;
    assert_eq!((status, body.as_slice()), (200, INDEX_PAGE));

    node.shutdown().await;
}

#[tokio::test]
async fn a_node_reports_the_identity_it_runs_with() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    assert_eq!(node.info().node_id, node.state().node_id);
    assert_eq!(node.info().data_dir, dir.path());
    assert!(!node.info().is_tls_enabled && node.info().tls_fingerprint.is_empty());

    node.shutdown().await;
}

#[tokio::test]
async fn a_node_keeps_its_identity_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let first = started(dir.path()).await;
    let first_id = first.info().node_id.clone();
    first.shutdown().await;

    let second = started(dir.path()).await;
    assert_eq!(second.info().node_id, first_id);
    second.shutdown().await;
}

#[tokio::test]
async fn the_control_socket_is_published_while_running_and_removed_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    assert!(dir.path().join("control.json").exists());
    assert!(node.info().notices.is_empty());

    node.shutdown().await;
    assert!(!dir.path().join("control.json").exists());
}

#[tokio::test]
async fn a_stopped_node_no_longer_answers() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let port = node.info().http_port;

    node.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).await.is_err());
}

#[tokio::test]
async fn with_a_passphrase_the_page_still_loads_but_the_api_needs_a_login() {
    let dir = tempfile::tempdir().unwrap();
    let mut with_passphrase = config(dir.path());
    with_passphrase.passphrase = Some("correct horse battery".to_string());
    let node = Node::start(with_passphrase).await.expect("the node starts");
    let port = node.info().http_port;

    assert_eq!(get(port, "/").await.0, 200);
    assert_eq!(get(port, "/api/sessions").await.0, 401);

    node.shutdown().await;
}

#[tokio::test]
async fn an_empty_passphrase_is_refused_before_anything_starts() {
    let dir = tempfile::tempdir().unwrap();
    let mut empty = config(dir.path());
    empty.passphrase = Some(String::new());

    let result = Node::start(empty).await;
    assert!(matches!(result, Err(StartError::EmptyPassphrase)));
    assert!(!dir.path().join("control.json").exists());
}

#[tokio::test]
async fn a_port_already_in_use_fails_the_start_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let mut busy = config(dir.path());
    busy.http_port = occupied.local_addr().unwrap().port();

    let result = Node::start(busy).await;
    assert!(matches!(result, Err(StartError::Listen { port, .. }) if port == occupied.local_addr().unwrap().port()));
    assert!(!dir.path().join("control.json").exists());
}

#[tokio::test]
async fn with_tls_the_node_reports_a_fingerprint_and_a_certificate_free_local_port() {
    let dir = tempfile::tempdir().unwrap();
    let mut secured = config(dir.path());
    secured.use_tls = true;
    secured.local_http_port = Some(0);
    let node = Node::start(secured).await.expect("the node starts");

    assert!(node.info().is_tls_enabled);
    assert!(!node.info().tls_fingerprint.is_empty());
    let local_port = node.info().local_http_port.expect("the loopback listener is open");
    assert_eq!(get(local_port, "/").await, (200, INDEX_PAGE.to_vec()));
    assert!(!node.info().notices.iter().any(|notice| matches!(notice, Notice::CertificateUnavailable(_))));

    node.shutdown().await;
}

#[tokio::test]
async fn an_idle_open_connection_does_not_hold_up_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let _idle = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], node.info().http_port))).await.unwrap();

    let shutdown = tokio::time::timeout(std::time::Duration::from_secs(10), node.shutdown()).await;
    assert!(shutdown.is_ok(), "shutdown waited on a connection that never sent a request");
}
