//! The in-process link: what the app's window can do with its node without any socket or browser in between.

use std::sync::Arc;

use bytes::Bytes;
use hyper::Method;
use ladex_core::node::{LocalLink, LocalResponse, Node, NodeConfig, NodeHandle, SocketReader, SocketWriter, UiAssets, WebUi};
use serde_json::{json, Value};

const SESSION: &str = "peer_app_window";
const MIB: usize = 1 << 20;

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        (path == "index.html").then_some(b"<!doctype html><title>stub ui</title>".as_slice())
    }
}

async fn started(data_dir: &std::path::Path, passphrase: Option<&str>) -> NodeHandle {
    let mut config = NodeConfig::new(data_dir.to_path_buf(), WebUi { assets: Arc::new(StubUi), is_legacy: false });
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    config.passphrase = passphrase.map(str::to_string);
    Node::start(config).await.expect("the node starts")
}

fn json_of(response: &LocalResponse) -> Value {
    serde_json::from_slice(&response.body).expect("a JSON answer")
}

async fn get(link: &LocalLink, path: &str) -> LocalResponse {
    link.request(Method::GET, path, &[], Bytes::new()).await.expect("the node answers")
}

async fn next_of_type(reader: &mut SocketReader, wanted: &str) -> Value {
    loop {
        let text = tokio::time::timeout(std::time::Duration::from_secs(10), reader.next())
            .await
            .expect("a message arrives in time")
            .expect("the socket stays open");
        let message: Value = serde_json::from_str(&text).unwrap();
        if message["type"] == wanted {
            return message;
        }
    }
}

async fn join(link: &LocalLink) -> (SocketWriter, SocketReader) {
    let (mut writer, reader) = link.open_socket().await.expect("the socket opens");
    let join = json!({ "type": "join", "session_id": SESSION, "user_agent": "test", "nickname": null });
    writer.send(join.to_string()).await.unwrap();
    (writer, reader)
}

fn upload_headers(name: &str, size: usize) -> Vec<(String, String)> {
    vec![
        ("X-Ladex-Session".into(), SESSION.into()),
        ("X-Ladex-Size".into(), size.to_string()),
        ("X-Ladex-Name".into(), name.into()),
        ("Content-Type".into(), "application/octet-stream".into()),
    ]
}

fn sample(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

#[tokio::test]
async fn the_link_is_signed_in_and_counts_as_the_machine_running_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), Some("correct horse battery")).await;

    let status = json_of(&get(&node.local_link(), "/auth-status").await);
    assert_eq!(status["authenticated"], true);
    assert_eq!(status["auth_required"], true);
    assert_eq!(status["is_host"], true);
    assert_eq!(status["node_id"], node.info().node_id.as_str());

    node.shutdown().await;
}

#[tokio::test]
async fn host_only_calls_work_over_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), None).await;
    let link = node.local_link();

    let invite = link.request(Method::POST, "/api/guests/invites", &[], Bytes::from_static(br#"{"permission":"upload"}"#)).await.unwrap();
    assert_eq!(invite.status, 201);
    assert_eq!(json_of(&invite)["permission"], "upload");
    assert_eq!(json_of(&get(&link, "/api/guests/sessions").await)["guests"], json!([]));

    node.shutdown().await;
}

#[tokio::test]
async fn joining_over_the_socket_lists_the_device_and_the_files() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), Some("correct horse battery")).await;
    let (_writer, mut reader) = join(&node.local_link()).await;

    let files = next_of_type(&mut reader, "file_list_update").await;
    assert_eq!(files["files"], json!([]));
    let joined = next_of_type(&mut reader, "peer_joined").await;
    assert_eq!(joined["peer"]["session_id"], SESSION);

    node.shutdown().await;
}

#[tokio::test]
async fn a_streamed_upload_is_shared_and_can_be_saved_back_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), Some("correct horse battery")).await;
    let link = node.local_link();
    let (_writer, mut reader) = join(&link).await;
    next_of_type(&mut reader, "peer_joined").await;

    let content = sample(3 * MIB + 12_345);
    let upload = link.begin_upload("/api/files/file_one", &upload_headers("report.bin", content.len()), content.len() as u64);
    for piece in content.chunks(MIB) {
        upload.write(Bytes::copy_from_slice(piece)).await.unwrap();
    }
    let answer = upload.finish().await.unwrap();
    assert_eq!(answer.status, 201, "{}", String::from_utf8_lossy(&answer.body));
    assert_eq!(json_of(&answer)["file"]["name"], "report.bin");

    let listed = next_of_type(&mut reader, "file_list_update").await;
    assert_eq!(listed["files"][0]["id"], "file_one");

    let downloads = tempfile::tempdir().unwrap();
    let mut last_progress = 0;
    let saved = link.save_download("/api/files/file_one", downloads.path(), "report.bin", |written| last_progress = written).await.unwrap();
    assert_eq!(std::fs::read(&saved.path).unwrap(), content);
    assert_eq!((saved.size_bytes, last_progress), (content.len() as u64, content.len() as u64));

    let again = link.save_download("/api/files/file_one", downloads.path(), "report.bin", |_| {}).await.unwrap();
    assert_eq!(again.path.file_name().unwrap().to_string_lossy(), "report (2).bin");

    node.shutdown().await;
}

#[tokio::test]
async fn an_upload_the_node_refuses_up_front_reports_why() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), None).await;
    let link = node.local_link();

    // The device never joined, so the node refuses before it reads the body.
    let upload = link.begin_upload("/api/files/file_two", &upload_headers("a.txt", 5), 5);
    let _ = upload.write(Bytes::from_static(b"hello")).await;
    let answer = upload.finish().await.unwrap();
    assert_eq!(answer.status, 403);

    node.shutdown().await;
}

#[tokio::test]
async fn saving_a_file_that_does_not_exist_is_refused_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), None).await;
    let downloads = tempfile::tempdir().unwrap();

    let result = node.local_link().save_download("/api/files/file_missing", downloads.path(), "x.bin", |_| {}).await;
    assert!(matches!(result, Err(ladex_core::node::LinkError::Refused { status: 404, .. })));
    assert_eq!(std::fs::read_dir(downloads.path()).unwrap().count(), 0);

    node.shutdown().await;
}

#[tokio::test]
async fn a_folder_is_published_and_saved_as_an_archive() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path(), None).await;
    let link = node.local_link();
    let (_writer, mut reader) = join(&link).await;
    next_of_type(&mut reader, "peer_joined").await;

    let mut children = Vec::new();
    for (index, name) in ["a.txt", "docs/b.txt"].iter().enumerate() {
        let id = format!("file_child{index}");
        let mut headers = upload_headers(name.rsplit('/').next().unwrap(), 4);
        headers.push(("X-Ladex-Parent".into(), "file_folder".into()));
        let upload = link.begin_upload(&format!("/api/files/{id}"), &headers, 4);
        upload.write(Bytes::from_static(b"data")).await.unwrap();
        assert_eq!(upload.finish().await.unwrap().status, 201);
        children.push(json!({ "path": name, "file_id": id }));
    }
    let listing = json!({ "name": "project", "children": children }).to_string();
    let published = link
        .request(
            Method::PUT,
            "/api/folders/file_folder",
            &[("X-Ladex-Session".into(), SESSION.into()), ("Content-Type".into(), "application/json".into())],
            Bytes::from(listing),
        )
        .await
        .unwrap();
    assert_eq!(published.status, 201, "{}", String::from_utf8_lossy(&published.body));

    let downloads = tempfile::tempdir().unwrap();
    let saved = link.save_download("/api/folders/file_folder.zip", downloads.path(), "project.zip", |_| {}).await.unwrap();
    let archive = std::fs::read(&saved.path).unwrap();
    assert_eq!(&archive[..2], b"PK");
    assert!(archive.windows(5).any(|window| window == b"b.txt"));

    node.shutdown().await;
}
