//! The app's command surface, driven without a window: ids, events and files as the window would see them.

use std::sync::Arc;
use std::time::Duration;

use ladex_core::node::{Bridge, Node, NodeConfig, NodeHandle, SocketEvent, UiAssets};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const SESSION: &str = "peer_bridge_window";

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

async fn started(data_dir: &std::path::Path) -> NodeHandle {
    let mut config = NodeConfig::new(data_dir.to_path_buf(), Arc::new(StubUi));
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    Node::start(config).await.expect("the node starts")
}

fn headers(size: usize) -> Vec<(String, String)> {
    vec![
        ("X-Ladex-Session".into(), SESSION.into()),
        ("X-Ladex-Size".into(), size.to_string()),
        ("X-Ladex-Name".into(), "note.txt".into()),
        ("Content-Type".into(), "text/plain".into()),
    ]
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<SocketEvent>) -> SocketEvent {
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await;
    event.expect("an event arrives in time").expect("the event channel stays open")
}

async fn next_message_of_type(events: &mut mpsc::UnboundedReceiver<SocketEvent>, wanted: &str) -> Value {
    loop {
        match next_event(events).await {
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

#[tokio::test]
async fn the_window_can_ask_the_node_who_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let bridge = Bridge::new(node.local_link(), dir.path().join("downloads"));

    let reply = bridge.request("GET", "/auth-status", &[], String::new()).await.unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(serde_json::from_str::<Value>(&reply.body).unwrap()["node_id"], node.info().node_id.as_str());
    assert!(bridge.request("NOT A METHOD", "/", &[], String::new()).await.is_err());

    node.shutdown().await;
}

#[tokio::test]
async fn a_shared_file_goes_up_by_id_and_comes_down_into_the_downloads_folder() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let downloads = dir.path().join("downloads");
    let bridge = Bridge::new(node.local_link(), downloads.clone());

    let (sender, mut events) = mpsc::unbounded_channel();
    let socket = bridge.open_socket(move |event| drop(sender.send(event))).await.unwrap();
    let join = json!({ "type": "join", "session_id": SESSION, "user_agent": "test", "nickname": null });
    bridge.send_to_socket(socket, join.to_string()).await.unwrap();
    next_message_of_type(&mut events, "peer_joined").await;

    let upload = bridge.begin_upload("/api/files/file_note", &headers(11), 11);
    bridge.write_upload(upload, b"hello ".to_vec()).await.unwrap();
    bridge.write_upload(upload, b"world".to_vec()).await.unwrap();
    let reply = bridge.finish_upload(upload).await.unwrap();
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(next_message_of_type(&mut events, "file_list_update").await["files"][0]["name"], "note.txt");

    let saved = bridge.save_download("/api/files/file_note", "note.txt").await.unwrap();
    assert!(saved.starts_with(downloads.to_str().unwrap()));
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "hello world");

    node.shutdown().await;
}

#[tokio::test]
async fn an_unknown_upload_id_is_an_error_and_an_aborted_one_is_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let bridge = Bridge::new(node.local_link(), dir.path().join("downloads"));

    assert!(bridge.write_upload(999, vec![1]).await.is_err());
    let upload = bridge.begin_upload("/api/files/file_x", &headers(10), 10);
    bridge.abort_upload(upload);
    assert!(bridge.write_upload(upload, vec![1]).await.is_err());
    assert!(bridge.finish_upload(upload).await.is_err());

    node.shutdown().await;
}

#[tokio::test]
async fn closing_a_socket_stops_its_events_and_forgets_it() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let bridge = Bridge::new(node.local_link(), dir.path().join("downloads"));
    let (sender, mut events) = mpsc::unbounded_channel();
    let socket = bridge.open_socket(move |event| drop(sender.send(event))).await.unwrap();

    bridge.close_socket(socket).await;

    assert!(bridge.send_to_socket(socket, "{}".into()).await.is_err());
    let late = tokio::time::timeout(Duration::from_millis(300), events.recv()).await;
    assert!(!matches!(late, Ok(Some(_))), "no event follows a close the window asked for");

    node.shutdown().await;
}

#[tokio::test]
async fn a_socket_the_node_ends_reports_that_it_closed() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let bridge = Bridge::new(node.local_link(), dir.path().join("downloads"));
    let (sender, mut events) = mpsc::unbounded_channel();
    let socket = bridge.open_socket(move |event| drop(sender.send(event))).await.unwrap();

    node.shutdown().await;

    while next_event(&mut events).await != SocketEvent::Closed {}
    assert!(bridge.send_to_socket(socket, "{}".into()).await.is_err());
}

#[tokio::test]
async fn a_downloads_folder_chosen_later_is_where_the_next_file_goes() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let bridge = Bridge::new(node.local_link(), dir.path().join("downloads")).with_preferences(node.state().preferences.clone());
    assert_eq!(bridge.downloads_folder(), dir.path().join("downloads"));

    let chosen = dir.path().join("chosen");
    let update = ladex_core::preferences::Update { downloads_dir: Some(chosen.clone()), ..Default::default() };
    ladex_core::preferences::apply(node.state(), &update).unwrap();

    assert_eq!(bridge.downloads_folder(), chosen);
    node.shutdown().await;
}
