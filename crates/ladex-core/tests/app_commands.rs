//! A node started the way the app starts it (`Node::start` with a window on its bridge) answers `ladex` commands.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Reply, Request};
use ladex_core::node::{Bridge, Node, NodeConfig, NodeHandle, SocketEvent, UiAssets};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const WINDOW_SESSION: &str = "peer_app_window";

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

async fn started(data_dir: &Path) -> NodeHandle {
    let mut config = NodeConfig::new(data_dir.to_path_buf(), Arc::new(StubUi));
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    Node::start(config).await.expect("the node starts")
}

async fn command(data_dir: &Path, request: Request) -> anyhow::Result<Reply> {
    let endpoint = Endpoint::load(data_dir).expect("the node publishes its endpoint");
    server::call(&endpoint, &request, |_| {}).await
}

async fn open_window(node: &NodeHandle, downloads: &Path) -> (Bridge, mpsc::UnboundedReceiver<SocketEvent>) {
    let bridge = Bridge::new(node.local_link(), downloads.to_path_buf());
    let (sender, mut events) = mpsc::unbounded_channel();
    let socket = bridge.open_socket(move |event| drop(sender.send(event))).await.unwrap();
    let join = json!({ "type": "join", "session_id": WINDOW_SESSION, "user_agent": "test", "nickname": "window" });
    bridge.send_to_socket(socket, join.to_string()).await.unwrap();
    next_message_of_type(&mut events, "peer_joined").await;
    (bridge, events)
}

async fn next_message_of_type(events: &mut mpsc::UnboundedReceiver<SocketEvent>, wanted: &str) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await;
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

fn write_file(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[tokio::test]
async fn status_comes_from_the_node_the_app_started() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let Reply::Status(status) = command(dir.path(), Request::Status).await.unwrap() else { panic!("expected a status") };

    assert_eq!(status.node_id, node.info().node_id);
    assert_eq!(status.http_port, node.info().http_port);
    node.shutdown().await;
}

#[tokio::test]
async fn a_file_sent_by_command_shows_up_in_the_window_and_in_ls() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let (_window, mut events) = open_window(&node, &dir.path().join("downloads")).await;
    let source = write_file(dir.path(), "note.txt", "hello world");

    command(dir.path(), Request::Send { paths: vec![source], to: None }).await.unwrap();

    assert_eq!(next_message_of_type(&mut events, "file_list_update").await["files"][0]["name"], "note.txt");
    let Reply::Files { files } = command(dir.path(), Request::List).await.unwrap() else { panic!("expected a listing") };
    assert_eq!(files.iter().map(|file| file.name.as_str()).collect::<Vec<_>>(), ["note.txt"]);
    node.shutdown().await;
}

#[tokio::test]
async fn a_file_shared_from_the_window_is_listed_and_fetched_by_command() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let (window, mut events) = open_window(&node, &dir.path().join("downloads")).await;
    let headers = vec![
        ("X-Ladex-Session".to_string(), WINDOW_SESSION.to_string()),
        ("X-Ladex-Size".to_string(), "5".to_string()),
        ("X-Ladex-Name".to_string(), "from-window.txt".to_string()),
        ("Content-Type".to_string(), "text/plain".to_string()),
    ];
    let upload = window.begin_upload("/api/files/file_from_window", &headers, 5);
    window.write_upload(upload, b"hello".to_vec()).await.unwrap();
    window.finish_upload(upload).await.unwrap();
    next_message_of_type(&mut events, "file_list_update").await;

    let saved = command(dir.path(), Request::Get { file: "from-window.txt".into(), into: dir.path().join("out") }).await.unwrap();

    let Reply::Saved { path, .. } = saved else { panic!("expected a saved file") };
    assert_eq!(std::fs::read_to_string(path).unwrap(), "hello");
    node.shutdown().await;
}

#[tokio::test]
async fn the_window_is_listed_as_a_device_here() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let (_window, _events) = open_window(&node, &dir.path().join("downloads")).await;

    let Reply::Devices { devices, .. } = command(dir.path(), Request::Devices).await.unwrap() else { panic!("expected devices") };

    assert!(devices.iter().any(|device| device.id == WINDOW_SESSION && device.is_here));
    node.shutdown().await;
}

#[tokio::test]
async fn an_invitation_names_the_address_other_nodes_join_the_app_at() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let Reply::Invite(invite) = command(dir.path(), Request::Invite { permission: Default::default() }).await.unwrap() else {
        panic!("expected an invitation")
    };

    assert!(invite.peer_address.is_some_and(|address| address.ends_with(&node.info().quic_port.to_string())));
    node.shutdown().await;
}
