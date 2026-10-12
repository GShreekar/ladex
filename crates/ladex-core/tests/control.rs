//! The commands' channel to a running node: sharing from disk, saving to disk, and who may use it.

use std::path::{Path, PathBuf};

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Event, Reply, Request, Settings};
use ladex_core::store::CHUNK_SIZE;
use ladex_core::testing::{eventually, spawn_mesh, NodeHandle};
use ladex_core::types::FileView;

/// Opens a node's control socket the way the node does at start; returns where commands reach it.
async fn control_of(node: &NodeHandle, data_dir: &Path) -> Endpoint {
    std::fs::create_dir_all(data_dir).unwrap();
    let (listener, endpoint) = server::open(data_dir).await.unwrap();
    let settings = Settings { data_dir: data_dir.to_path_buf(), discovery_port: None };
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
