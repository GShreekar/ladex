//! A file moves between two real nodes whichever way each is driven: through an app window or through `ladex` commands.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Reply, Request};
use ladex_core::node::{Bridge, Node, NodeConfig, NodeHandle, SocketEvent, UiAssets, WebUi};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::mpsc;

const PASSPHRASE: &str = "river-anchor-violet-seven";
const FILE_NAME: &str = "report.bin";
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);
const MESH_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

#[derive(Clone, Copy)]
enum Driver {
    App,
    Cli,
}

struct Window {
    bridge: Bridge,
    events: mpsc::UnboundedReceiver<SocketEvent>,
    session_id: String,
}

struct Peer {
    node: NodeHandle,
    dir: TempDir,
    window: Option<Window>,
}

fn content() -> Vec<u8> {
    (0..1_500_000u32).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
}

async fn start_peer(driver: Driver, join: Option<SocketAddr>) -> Peer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = NodeConfig::new(dir.path().to_path_buf(), WebUi { assets: Arc::new(StubUi), is_legacy: false });
    config.passphrase = Some(PASSPHRASE.to_string());
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    config.manual_peers = join.into_iter().collect();
    let node = Node::start(config).await.expect("the node starts");
    let window = match driver {
        Driver::App => Some(open_window(&node, dir.path()).await),
        Driver::Cli => None,
    };
    Peer { node, dir, window }
}

async fn open_window(node: &NodeHandle, dir: &std::path::Path) -> Window {
    let bridge = Bridge::new(node.local_link(), dir.join("downloads"));
    let (sender, mut events) = mpsc::unbounded_channel();
    let socket = bridge.open_socket(move |event| drop(sender.send(event))).await.unwrap();
    let session_id = format!("peer_window_{}", node.info().node_id);
    let join = json!({ "type": "join", "session_id": session_id, "user_agent": "test", "nickname": null });
    bridge.send_to_socket(socket, join.to_string()).await.unwrap();
    next_message_of_type(&mut events, "peer_joined").await;
    Window { bridge, events, session_id }
}

async fn next_message_of_type(events: &mut mpsc::UnboundedReceiver<SocketEvent>, wanted: &str) -> Value {
    loop {
        let event = tokio::time::timeout(EVENT_TIMEOUT, events.recv()).await;
        match event.expect("an event arrives in time").expect("the event channel stays open") {
            SocketEvent::Message { text } => {
                let message: Value = serde_json::from_str(&text).unwrap();
                if message["type"] == wanted {
                    return message;
                }
            }
            SocketEvent::Closed => panic!("the socket closed while waiting for {wanted}"),
        }
    }
}

async fn command(peer: &Peer, request: Request) -> anyhow::Result<Reply> {
    let endpoint = Endpoint::load(peer.dir.path()).expect("the node publishes its endpoint");
    server::call(&endpoint, &request, |_| {}).await
}

async fn wait_until_meshed(peer: &Peer) {
    let started = tokio::time::Instant::now();
    loop {
        let Reply::Status(status) = command(peer, Request::Status).await.unwrap() else { panic!("expected a status") };
        if !status.nodes.is_empty() {
            return;
        }
        assert!(started.elapsed() < MESH_TIMEOUT, "the nodes never found each other");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn share(peer: &mut Peer, bytes: &[u8]) {
    match &peer.window {
        Some(window) => share_from_window(window, bytes).await,
        None => share_by_command(peer, bytes).await,
    }
}

async fn share_by_command(peer: &Peer, bytes: &[u8]) {
    let path: PathBuf = peer.dir.path().join(FILE_NAME);
    std::fs::write(&path, bytes).unwrap();
    command(peer, Request::Send { paths: vec![path], to: None }).await.expect("send succeeds");
}

async fn share_from_window(window: &Window, bytes: &[u8]) {
    let headers = vec![
        ("X-Ladex-Session".to_string(), window.session_id.clone()),
        ("X-Ladex-Size".to_string(), bytes.len().to_string()),
        ("X-Ladex-Name".to_string(), FILE_NAME.to_string()),
        ("Content-Type".to_string(), "application/octet-stream".to_string()),
    ];
    let upload = window.bridge.begin_upload("/api/files/file_report", &headers, bytes.len() as u64);
    for piece in bytes.chunks(256 * 1024) {
        window.bridge.write_upload(upload, piece.to_vec()).await.unwrap();
    }
    let reply = window.bridge.finish_upload(upload).await.unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
}

async fn fetch(peer: &mut Peer) -> Vec<u8> {
    match peer.window.as_mut() {
        Some(window) => fetch_into_window(window).await,
        None => fetch_by_command(peer).await,
    }
}

async fn fetch_by_command(peer: &Peer) -> Vec<u8> {
    let into = peer.dir.path().join("out");
    let started = tokio::time::Instant::now();
    // The file reaches this node through the mesh, so the first attempts may find nobody hosting it yet.
    let saved = loop {
        match command(peer, Request::Get { file: FILE_NAME.to_string(), into: into.clone() }).await {
            Ok(saved) => break saved,
            Err(error) => assert!(started.elapsed() < MESH_TIMEOUT, "get never succeeded: {error}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let Reply::Saved { path, .. } = saved else { panic!("expected a saved file") };
    std::fs::read(path).unwrap()
}

async fn fetch_into_window(window: &mut Window) -> Vec<u8> {
    let file_id = loop {
        let update = next_message_of_type(&mut window.events, "file_list_update").await;
        let listed = update["files"].as_array().unwrap().iter().find(|file| file["name"] == FILE_NAME).cloned();
        if let Some(file) = listed {
            break file["id"].as_str().unwrap().to_string();
        }
    };
    let saved = window.bridge.save_download(&format!("/api/files/{file_id}"), FILE_NAME).await.expect("the download is saved");
    std::fs::read(saved).unwrap()
}

async fn transfer_between(sharer: Driver, fetcher: Driver) {
    let mut sharing = start_peer(sharer, None).await;
    let join_address = SocketAddr::from(([127, 0, 0, 1], sharing.node.info().quic_port));
    let mut fetching = start_peer(fetcher, Some(join_address)).await;
    wait_until_meshed(&sharing).await;
    wait_until_meshed(&fetching).await;
    let original = content();

    share(&mut sharing, &original).await;
    let received = fetch(&mut fetching).await;

    assert!(received == original, "the received file differs from the shared one");
    fetching.node.shutdown().await;
    sharing.node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_moves_from_one_app_to_another() {
    transfer_between(Driver::App, Driver::App).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_moves_from_one_daemon_to_another() {
    transfer_between(Driver::Cli, Driver::Cli).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_moves_from_an_app_to_a_daemon() {
    transfer_between(Driver::App, Driver::Cli).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_moves_from_a_daemon_to_an_app() {
    transfer_between(Driver::Cli, Driver::App).await;
}
