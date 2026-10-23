//! The commands' channel to a running node: sharing from disk, saving to disk, and who may use it.

use std::path::{Path, PathBuf};

use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{self, Event, Events, OpenedFile, Reply, Request, Settings, VERSION};
use ladex_core::guest::uploads::UploadState;
use ladex_core::guest::Permission;
use ladex_core::saved_passphrase;
use ladex_core::store::CHUNK_SIZE;
use ladex_core::testing::{eventually, spawn_mesh, spawn_node, NodeConfig, NodeHandle};
use ladex_core::trust::TrustedVia;
use ladex_core::types::FileView;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Opens a node's control socket the way the node does at start; returns where commands reach it.
async fn control_of(node: &NodeHandle, data_dir: &Path) -> Endpoint {
    std::fs::create_dir_all(data_dir).unwrap();
    let (listener, endpoint) = server::open(data_dir).await.unwrap();
    let settings = Settings {
        data_dir: data_dir.to_path_buf(),
        discovery_port: None,
        local_http_port: None,
        use_keychain: false,
        manual_peers: Vec::new(),
    };
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

fn opened(path: &Path, name: &str, mime_type: &str) -> OpenedFile {
    let file = std::fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    OpenedFile { file, name: name.into(), mime_type: mime_type.into(), size }
}

// An app on a phone hands over files it opened from another app, named by that app rather than by a path.
#[tokio::test]
async fn a_file_handed_over_open_is_shared_under_its_given_name_and_another_node_saves_it() {
    let nodes = spawn_mesh(2, None).await;
    let dir = tempfile::tempdir().unwrap();
    let receiver = control_of(&nodes[1], &dir.path().join("b")).await;
    let source = write(&dir.path().join("1000000123"), &three_chunks());
    let (sender, _events) = mpsc::channel(16);

    let shared = control::share_opened(&nodes[0].state, vec![opened(&source, "beach.jpg", "image/jpeg")], &Events::new(sender))
        .await
        .unwrap()
        .remove(0);
    assert!(eventually(async || !nodes[1].hosts_of(&shared.id).await.is_empty()).await);
    ask(&receiver, Request::Get { file: "beach.jpg".into(), into: dir.path().join("out") }).await.unwrap();

    assert_eq!((shared.name.as_str(), shared.mime_type.as_str()), ("beach.jpg", "image/jpeg"));
    assert_eq!(std::fs::read(dir.path().join("out/beach.jpg")).unwrap(), three_chunks());
}

#[tokio::test]
async fn a_name_and_type_from_another_app_are_held_to_the_network_rules() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let source = write(&dir.path().join("source"), b"note");
    let (sender, _events) = mpsc::channel(16);

    let shared = control::share_opened(&node.state, vec![opened(&source, "../secret.txt", "not a type")], &Events::new(sender))
        .await
        .unwrap()
        .remove(0);

    assert_eq!((shared.name.as_str(), shared.mime_type.as_str()), (".._secret.txt", "application/octet-stream"));
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

#[tokio::test]
async fn unsharing_by_name_takes_the_file_off_every_node() {
    let nodes = spawn_mesh(2, None).await;
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(&nodes[0], &dir.path().join("data")).await;
    let source = write(&dir.path().join("notes.txt"), b"notes");
    let shared = shared_by(&endpoint, Request::Send { paths: vec![source], to: None }).await.remove(0);
    assert!(eventually(async || nodes[1].live_files().await.contains(&shared.id)).await);

    let reply = ask(&endpoint, Request::Unshare { file: "notes.txt".into() }).await.unwrap();

    assert_eq!(reply, Reply::Acknowledged { message: "Stopped sharing notes.txt".into() });
    assert!(eventually(async || !nodes[1].live_files().await.contains(&shared.id)).await);
}

#[tokio::test]
async fn a_file_another_node_shared_cannot_be_unshared_here() {
    let nodes = spawn_mesh(2, None).await;
    let dir = tempfile::tempdir().unwrap();
    let (sharing, other) = (control_of(&nodes[0], &dir.path().join("a")).await, control_of(&nodes[1], &dir.path().join("b")).await);
    let source = write(&dir.path().join("notes.txt"), b"notes");
    let shared = shared_by(&sharing, Request::Send { paths: vec![source], to: None }).await.remove(0);
    assert!(eventually(async || nodes[1].live_files().await.contains(&shared.id)).await);

    let refused = ask(&other, Request::Unshare { file: shared.id.clone() }).await.unwrap_err();

    assert!(refused.to_string().contains("only that node can stop sharing it"), "{refused}");
    assert!(nodes[1].live_files().await.contains(&shared.id));
}

#[tokio::test]
async fn a_chat_message_from_the_command_line_reaches_the_other_node_under_this_nodes_name() {
    let nodes = spawn_mesh(2, None).await;
    let dir = tempfile::tempdir().unwrap();
    let (posting, reading) = (control_of(&nodes[0], &dir.path().join("a")).await, control_of(&nodes[1], &dir.path().join("b")).await);

    ask(&posting, Request::Chat { message: Some("lunch at one?".into()) }).await.unwrap();

    assert!(eventually(async || nodes[1].chat().await.len() == 1).await);
    let Reply::Messages { messages } = ask(&reading, Request::Chat { message: None }).await.unwrap() else { panic!("expected messages") };
    assert_eq!(messages[0].content, "lunch at one?");
    assert_eq!(messages[0].sender_name.as_deref(), Some(nodes[0].state.node_name.as_str()));
}

#[tokio::test]
async fn an_empty_chat_message_is_refused() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    assert!(ask(&endpoint, Request::Chat { message: Some("   ".into()) }).await.is_err());
    assert!(node.chat().await.is_empty());
}

/// A guest on `node` whose upload of a small file waits for approval; returns the guest's id and the upload's.
async fn waiting_guest_upload(node: &NodeHandle) -> (String, String) {
    let guests = &node.state.guests;
    let (_, guest) = guests.sessions.create("192.168.1.40".parse().unwrap(), None, Permission::UploadWithApproval);
    guests.uploads.begin("file_g", &guest.id, "photo.jpg".into(), "image/jpeg".into(), 5).unwrap();
    let blob = node.state.store.create("file_g", 5).unwrap();
    blob.write_chunk_hashing(0, b"photo".to_vec()).await.unwrap();
    blob.seal().await.unwrap();
    guests.uploads.transition("file_g", UploadState::Receiving, UploadState::AwaitingApproval).unwrap();
    (guest.id, "file_g".into())
}

async fn guests_of(endpoint: &Endpoint) -> (Vec<String>, Vec<String>) {
    match ask(endpoint, Request::Guests).await.unwrap() {
        Reply::Guests { guests, uploads } => {
            (guests.into_iter().map(|guest| guest.id).collect(), uploads.into_iter().map(|upload| upload.id).collect())
        }
        other => panic!("expected guests, got {other:?}"),
    }
}

#[tokio::test]
async fn a_waiting_guest_upload_is_listed_and_once_approved_is_shared() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;
    let (guest, upload) = waiting_guest_upload(node).await;
    assert_eq!(guests_of(&endpoint).await, (vec![guest], vec![upload.clone()]));

    let shared = shared_by(&endpoint, Request::AnswerUpload { upload: upload.clone(), accepted: true }).await;

    assert_eq!(shared[0].name, "photo.jpg");
    assert!(node.live_files().await.contains(&upload));
    assert!(guests_of(&endpoint).await.1.is_empty());
}

#[tokio::test]
async fn a_denied_guest_upload_is_thrown_away() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;
    let (_, upload) = waiting_guest_upload(node).await;

    ask(&endpoint, Request::AnswerUpload { upload: upload.clone(), accepted: false }).await.unwrap();

    assert!(node.state.store.get(&upload).is_none());
    assert!(!node.live_files().await.contains(&upload));
    assert!(ask(&endpoint, Request::AnswerUpload { upload, accepted: false }).await.is_err(), "denied twice");
}

#[tokio::test]
async fn a_guest_signed_out_from_the_command_line_is_gone() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;
    let (guest, _) = waiting_guest_upload(node).await;

    ask(&endpoint, Request::SignOutGuest { guest: guest.clone() }).await.unwrap();

    assert_eq!(guests_of(&endpoint).await, (vec![], vec![]));
    assert!(ask(&endpoint, Request::SignOutGuest { guest }).await.is_err(), "signed out twice");
}

#[tokio::test]
async fn a_new_passphrase_is_saved_for_the_next_start_and_the_old_one_is_shown_until_then() {
    let node = &spawn_mesh(1, Some("old-mesh-secret")).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    ask(&endpoint, Request::ChangePassphrase { passphrase: "new-mesh-secret".into() }).await.unwrap();

    let saved = saved_passphrase::load(&node.state.started_with.data_dir, false).unwrap();
    assert_eq!(saved.as_deref(), Some("new-mesh-secret"));
    assert!(node.state.preferences.needs_restart());
    let shown = ask(&endpoint, Request::Passphrase).await.unwrap();
    assert_eq!(shown, Reply::Passphrase { passphrase: Some("old-mesh-secret".into()) });
}

async fn pairing_of(endpoint: &Endpoint) -> ladex_core::pairing::View {
    match ask(endpoint, Request::Pairing).await.unwrap() {
        Reply::Pairing(view) => *view,
        other => panic!("expected pairing, got {other:?}"),
    }
}

/// Answers the code waiting on a node as soon as it shows; returns the words it showed.
async fn confirm_code(endpoint: &Endpoint) -> Vec<String> {
    let mut code = None;
    assert!(
        eventually(async || {
            code = pairing_of(endpoint).await.status.waiting.into_iter().next();
            code.is_some()
        })
        .await,
        "no code to compare ever showed"
    );
    let code = code.unwrap();
    ask(endpoint, Request::AnswerPairing { id: code.id, accepted: true }).await.unwrap();
    code.request.words
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_pair_from_the_command_line_and_one_can_then_revoke_the_other() {
    let phone = spawn_node(NodeConfig::named("phone").passphrase("one")).await;
    let laptop = spawn_node(NodeConfig::named("laptop").passphrase("two")).await;
    let dir = tempfile::tempdir().unwrap();
    let (on_phone, on_laptop) = (control_of(&phone, &dir.path().join("a")).await, control_of(&laptop, &dir.path().join("b")).await);

    ask(&on_laptop, Request::OpenPairing).await.unwrap();
    ask(&on_phone, Request::DialPairing { address: laptop.addr.to_string() }).await.unwrap();
    let (phone_words, laptop_words) = tokio::join!(confirm_code(&on_phone), confirm_code(&on_laptop));

    assert_eq!(phone_words, laptop_words);
    assert!(eventually(async || pairing_of(&on_phone).await.status.recent.first().is_some_and(|outcome| outcome.paired)).await);
    let Reply::Trust { nodes } = ask(&on_phone, Request::Trust).await.unwrap() else { panic!("expected trust") };
    assert_eq!((nodes[0].node.name.as_str(), nodes[0].node.trusted_via), ("laptop", TrustedVia::Pairing));

    ask(&on_phone, Request::RevokeNode { node: "laptop".into() }).await.unwrap();
    let Reply::Trust { nodes } = ask(&on_phone, Request::Trust).await.unwrap() else { panic!("expected trust") };
    assert_eq!(nodes[0].revoked_by.as_deref(), Some("this device"));
}

#[tokio::test]
async fn pairing_with_something_that_is_not_an_address_is_refused() {
    let node = &spawn_mesh(1, None).await[0];
    let dir = tempfile::tempdir().unwrap();
    let endpoint = control_of(node, dir.path()).await;

    let refused = ask(&endpoint, Request::DialPairing { address: "the laptop".into() }).await.unwrap_err();

    assert!(refused.to_string().contains("is not an address"), "{refused}");
}
