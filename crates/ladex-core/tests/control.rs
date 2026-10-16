//! The commands' channel to a running node: sharing from disk, saving to disk, and who may use it.

use std::path::{Path, PathBuf};

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Event, Reply, Request, Settings, VERSION};
use ladex_core::store::CHUNK_SIZE;
use ladex_core::testing::{eventually, spawn_mesh, NodeHandle};
use ladex_core::types::FileView;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Opens a node's control socket the way the node does at start; returns where commands reach it.
async fn control_of(node: &NodeHandle, data_dir: &Path) -> Endpoint {
    std::fs::create_dir_all(data_dir).unwrap();
    let (listener, endpoint) = server::open(data_dir).await.unwrap();
    let settings = Settings { data_dir: data_dir.to_path_buf(), discovery_port: None, local_http_port: None };
    tokio::spawn(server::serve(node.state.clone(), settings, listener, endpoint.clone()));
    endpoint
}

async fn ask(endpoint: &Endpoint, request: Request) -> anyhow::Result<Reply> {
    server::call(endpoint, &request, |_| {}).await
}

async fn shared_by(endpoint: &Endpoint, request: Request) -> Vec<FileView> {
    match ask(endpoint, request).await.unwrap() {
        Reply::Shared { files } => files,
        other => panic!("expected shared files, got {other:?}"),
    }
}

async fn listed_by(endpoint: &Endpoint) -> Vec<FileView> {
    match ask(endpoint, Request::List).await.unwrap() {
        Reply::Files { files } => files,
        other => panic!("expected a listing, got {other:?}"),
    }
}

fn three_chunks() -> Vec<u8> {
    (0..(2 * CHUNK_SIZE as usize + 4321)).map(|i| (i % 251) as u8).collect()
}

fn write(path: &Path, content: &[u8]) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    path.to_path_buf()
}

#[tokio::test]
async fn a_file_on_disk_is_shared_and_another_node_saves_it_intact() {
    let nodes = spawn_mesh(2, None).await;
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = (control_of(&nodes[0], &dir.path().join("a")).await, control_of(&nodes[1], &dir.path().join("b")).await);
    let source = write(&dir.path().join("report.pdf"), &three_chunks());

    let shared = shared_by(&sender, Request::Send { paths: vec![source], to: None }).await.remove(0);
    assert!(eventually(async || !nodes[1].hosts_of(&shared.id).await.is_empty()).await);
    let saved = ask(&receiver, Request::Get { file: "report.pdf".into(), into: dir.path().join("out") }).await.unwrap();

    assert_eq!((shared.name.as_str(), shared.mime_type.as_str()), ("report.pdf", "application/pdf"));
    assert_eq!(saved, Reply::Saved { path: dir.path().canonicalize().unwrap().join("out/report.pdf"), file: saved_file(&saved) });
    assert_eq!(std::fs::read(dir.path().join("out/report.pdf")).unwrap(), three_chunks());
}

fn saved_file(reply: &Reply) -> FileView {
    match reply {
        Reply::Saved { file, .. } => file.clone(),
        other => panic!("expected a saved file, got {other:?}"),
    }
}

#[tokio::test]
async fn a_directory_is_shared_as_a_folder_with_its_files_inside() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    write(&dir.path().join("album/cover.txt"), b"cover");
    write(&dir.path().join("album/disc 1/track.txt"), b"track");

    let folder = shared_by(&endpoint, Request::Send { paths: vec![dir.path().join("album")], to: None }).await.remove(0);
    ask(&endpoint, Request::Get { file: folder.id.clone(), into: dir.path().join("out") }).await.unwrap();

    assert_eq!((folder.name.as_str(), folder.is_folder, folder.folder_files, folder.folder_bytes), ("album", true, 2, 10));
    assert_eq!(std::fs::read(dir.path().join("out/album/disc 1/track.txt")).unwrap(), b"track");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_inside_a_shared_directory_is_left_out() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    let secret = write(&dir.path().join("secret.txt"), b"not for sharing");
    write(&dir.path().join("album/cover.txt"), b"cover");
    std::os::unix::fs::symlink(&secret, dir.path().join("album/link.txt")).unwrap();

    let folder = shared_by(&endpoint, Request::Send { paths: vec![dir.path().join("album")], to: None }).await.remove(0);

    assert_eq!(folder.folder_files, 1);
}

#[tokio::test]
async fn an_empty_directory_is_refused_and_nothing_is_shared() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    std::fs::create_dir(dir.path().join("empty")).unwrap();

    let refused = ask(&endpoint, Request::Send { paths: vec![dir.path().join("empty")], to: None }).await;

    assert!(refused.unwrap_err().to_string().contains("no files to share"));
    assert!(listed_by(&endpoint).await.is_empty());
}

#[tokio::test]
async fn seeding_a_changed_folder_replaces_the_one_shared_before() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    write(&dir.path().join("notes/one.txt"), b"one");
    let first = shared_by(&endpoint, Request::Seed { dir: dir.path().join("notes"), replaces: None }).await.remove(0);
    write(&dir.path().join("notes/two.txt"), b"two");

    let second = shared_by(&endpoint, Request::Seed { dir: dir.path().join("notes"), replaces: Some(first.id) }).await.remove(0);

    let top_level: Vec<String> = listed_by(&endpoint).await.into_iter().filter(|file| file.parent.is_none()).map(|file| file.id).collect();
    assert_eq!(top_level, [second.id]);
    assert_eq!(node.state.store.all().len(), 3);
}

#[tokio::test]
async fn a_name_two_files_share_must_be_given_as_an_id() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    let paths = vec![write(&dir.path().join("a/notes.txt"), b"first"), write(&dir.path().join("b/notes.txt"), b"second")];
    shared_by(&endpoint, Request::Send { paths, to: None }).await;

    let refused = ask(&endpoint, Request::Get { file: "notes.txt".into(), into: dir.path().join("out") }).await;

    assert!(refused.unwrap_err().to_string().contains("2 things are called"));
}

#[tokio::test]
async fn sharing_reports_progress_up_to_the_whole_size() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    let source = write(&dir.path().join("big.bin"), &three_chunks());
    let mut last_progress = None;

    server::call(&endpoint, &Request::Send { paths: vec![source], to: None }, |event| last_progress = Some(event.clone())).await.unwrap();

    let size = three_chunks().len() as u64;
    assert_eq!(last_progress, Some(Event::Progress { name: "big.bin".into(), done: size, total: size }));
}

#[tokio::test]
async fn a_command_without_the_token_is_refused() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;
    let stranger = Endpoint { port: endpoint.port, token: "0".repeat(64) };

    let refused = ask(&stranger, Request::List).await;

    assert!(refused.unwrap_err().to_string().contains("may not control"));
}

#[tokio::test]
async fn the_endpoint_is_found_in_the_data_directory_until_the_node_stops() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    assert_eq!(Endpoint::load(dir.path()), Some(endpoint));
    server::close(dir.path());
    assert_eq!(Endpoint::load(dir.path()), None);
}

#[tokio::test]
async fn a_file_sent_to_a_device_that_is_not_connected_is_refused_and_not_kept() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, &dir.path().join("data")).await;
    let source = write(&dir.path().join("gift.bin"), b"gift");

    let refused = ask(&endpoint, Request::Send { paths: vec![source], to: Some("nobody".into()) }).await;

    assert!(refused.unwrap_err().to_string().contains("no device called"));
    assert!(node.state.store.all().is_empty());
}

/// Sends one raw line to a control socket and returns the first line it answers.
async fn first_reply_to(port: u16, line: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    BufReader::new(stream).lines().next_line().await.unwrap().expect("the node answers")
}

fn hello(endpoint: &Endpoint, version: Option<&str>) -> String {
    let mut hello = serde_json::json!({ "token": endpoint.token, "request": { "command": "status" } });
    if let Some(version) = version {
        hello["version"] = version.into();
    }
    hello.to_string()
}

#[tokio::test]
async fn a_command_of_another_version_is_refused_with_both_versions_named() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    let answer = first_reply_to(endpoint.port, &hello(&endpoint, Some("0.0.1"))).await;

    let expected = format!("the running node is {VERSION}, this command is 0.0.1 — update one of them");
    assert_eq!(serde_json::from_str::<Event>(&answer).unwrap(), Event::Failed { message: expected });
}

#[tokio::test]
async fn a_command_that_reports_no_version_is_refused_as_older() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    let answer = first_reply_to(endpoint.port, &hello(&endpoint, None)).await;

    let expected = format!("the running node is {VERSION}, this command is an older version — update one of them");
    assert_eq!(serde_json::from_str::<Event>(&answer).unwrap(), Event::Failed { message: expected });
}

#[tokio::test]
async fn a_stranger_of_another_version_learns_nothing_about_the_node() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;
    let stranger = Endpoint { port: endpoint.port, token: "0".repeat(64) };

    let answer = first_reply_to(endpoint.port, &hello(&stranger, Some("0.0.1"))).await;

    let refused = Event::Failed { message: "this command may not control the node".into() };
    assert_eq!(serde_json::from_str::<Event>(&answer).unwrap(), refused);
}

#[tokio::test]
async fn a_matching_command_is_welcomed_with_the_nodes_version() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    let answer = first_reply_to(endpoint.port, &hello(&endpoint, Some(VERSION))).await;

    assert_eq!(serde_json::from_str::<Event>(&answer).unwrap(), Event::Welcome { version: VERSION.to_string() });
}

/// A stand-in node that answers whatever it is asked with the given lines.
async fn fake_node_saying(lines: Vec<String>) -> Endpoint {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reading, mut writing) = stream.into_split();
        BufReader::new(reading).lines().next_line().await.unwrap();
        for line in lines {
            writing.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        }
    });
    Endpoint { port, token: "unused".into() }
}

fn event_line(event: &Event) -> String {
    serde_json::to_string(event).unwrap()
}

#[tokio::test]
async fn a_node_of_another_version_is_refused_by_the_command() {
    let welcome = Event::Welcome { version: "0.0.1".into() };
    let endpoint = fake_node_saying(vec![event_line(&welcome)]).await;

    let refused = ask(&endpoint, Request::Status).await.unwrap_err();

    assert_eq!(refused.to_string(), format!("the running node is 0.0.1, this command is {VERSION} — update one of them"));
}

#[tokio::test]
async fn a_node_that_never_reports_its_version_is_refused_as_older() {
    let reply = Event::Done { reply: Box::new(Reply::Files { files: Vec::new() }) };
    let endpoint = fake_node_saying(vec![event_line(&reply)]).await;

    let refused = ask(&endpoint, Request::List).await.unwrap_err();

    assert_eq!(refused.to_string(), format!("the running node is an older version, this command is {VERSION} — update one of them"));
}
