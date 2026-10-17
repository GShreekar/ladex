//! A file sent to one device: offered to it, fetched from the sending node alone once accepted, and kept from everyone else.

use ladex_core::push::{self, PushedFile, Refused};
use ladex_core::store::{Origin, CHUNK_SIZE};
use ladex_core::testing::{eventually, eventually_within, spawn_mesh, NodeHandle};
use ladex_core::transfer::wire::{ErrorCode, Fetch, GetChunks, Reply};
use ladex_core::types::{OfferedFile, ServerMessage};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver};

// A loaded CI runner can take far longer than a quiet machine to move three chunks.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

type Tab = UnboundedReceiver<ServerMessage>;

fn three_chunks() -> Vec<u8> {
    (0..(2 * CHUNK_SIZE as usize + 4321)).map(|i| (i % 251) as u8).collect()
}

/// Nodes with one device each, `alice` on the first, `bob` on the second and so on, all known to every node.
async fn mesh_with_devices(names: &[&str]) -> (Vec<NodeHandle>, Vec<Tab>) {
    let nodes = spawn_mesh(names.len(), None).await;
    let mut tabs = Vec::new();
    for (node, name) in nodes.iter().zip(names) {
        tabs.push(node.open_tab(name).await);
    }
    for node in &nodes {
        assert!(eventually(async || names.iter().all(|name| node.devices_now().contains(*name))).await, "devices never synced");
    }
    (nodes, tabs)
}

/// Stores `content` on `node` as its device's upload for `to` would, and offers it.
async fn send(node: &NodeHandle, from: &str, to: &str, content: &[u8]) -> Result<PushedFile, Refused> {
    let blob = node.state.store.create_with("file_p", content.len() as u64, Origin::Pushed).unwrap();
    for (index, chunk) in content.chunks(CHUNK_SIZE as usize).enumerate() {
        blob.write_chunk_hashing(index as u32, chunk.to_vec()).await.unwrap();
    }
    let file = PushedFile {
        file_id: "file_p".into(),
        name: "holiday.jpg".into(),
        size: content.len() as u64,
        mime_type: "image/jpeg".into(),
        manifest_root: blob.seal().await.unwrap(),
        from_session: from.into(),
        to_session: to.into(),
    };
    push::offer(&node.state, file.clone()).await.map(|()| file)
}

async fn next_message(tab: &mut Tab) -> ServerMessage {
    let wait = std::time::Duration::from_secs(10);
    loop {
        let message = tokio::time::timeout(wait, tab.recv()).await.expect("the tab was sent nothing").expect("the tab is open");
        // Device and catalog updates reach every tab and are not what these tests are about.
        if !matches!(message, ServerMessage::PeerSync { .. } | ServerMessage::PeerJoined { .. } | ServerMessage::FileListUpdate { .. }) {
            return message;
        }
    }
}

async fn expect_prompt(tab: &mut Tab, file: &PushedFile) {
    match next_message(tab).await {
        ServerMessage::IncomingFileOffer { file_id, from_session_id, pushed } => {
            assert_eq!((file_id, from_session_id), (file.file_id.clone(), file.from_session.clone()));
            assert_eq!(pushed, Some(OfferedFile { name: file.name.clone(), size: file.size }));
        }
        other => panic!("expected the offer, got {other:?}"),
    }
}

async fn contents(node: &NodeHandle, id: &str) -> Vec<u8> {
    let blob = node.state.store.get(id).expect("the file is stored");
    let mut contents = Vec::new();
    for index in 0..blob.chunk_count() {
        contents.extend(blob.read_chunk(index).await.unwrap());
    }
    contents
}

async fn ask(from: &NodeHandle, of: &NodeHandle, request: GetChunks) -> Vec<Reply> {
    let fetches = from.state.mesh_peers.read().await[of.node_id()].fetches.clone();
    let (replies_tx, mut replies) = mpsc::channel(32);
    fetches.send(Fetch { request, replies: replies_tx }).await.unwrap();
    let mut got = Vec::new();
    while let Some(reply) = replies.recv().await {
        got.push(reply);
    }
    got
}

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_file_arrives_whole_and_the_sender_is_told() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let content = three_chunks();
    let file = send(a, "alice", "bob", &content).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;

    push::accept(&b.state, "bob", "file_p").await.unwrap();

    assert!(eventually_within(TRANSFER_TIMEOUT, async || b.state.store.contains_complete("file_p")).await, "the file never arrived");
    assert_eq!(contents(b, "file_p").await, content);
    let delivered = next_message(&mut tabs[0]).await;
    assert!(matches!(&delivered, ServerMessage::FilePushDelivered { to_session_id, .. } if to_session_id == "bob"), "{delivered:?}");
    assert!(eventually_within(TRANSFER_TIMEOUT, async || a.state.store.get("file_p").is_none()).await, "the sender kept its copy");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_receiving_device_gets_the_file_from_its_node_and_no_other_device_does() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let _carol = b.open_tab("carol").await;
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;
    assert!(push::open(&b.state, "file_p", "bob").is_none(), "nothing is given out before it is accepted");

    push::accept(&b.state, "bob", "file_p").await.unwrap();

    let (blob, opened) = push::open(&b.state, "file_p", "bob").expect("the recipient may fetch it");
    assert_eq!((blob.id(), opened.name.as_str()), ("file_p", "holiday.jpg"));
    assert!(push::open(&b.state, "file_p", "carol").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pushed_file_is_never_listed_or_announced() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob", "carol"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;
    push::accept(&b.state, "bob", "file_p").await.unwrap();
    assert!(eventually_within(TRANSFER_TIMEOUT, async || b.state.store.contains_complete("file_p")).await);

    for node in [a, b, c] {
        assert!(node.live_files().await.is_empty());
        assert!(node.hosts_of("file_p").await.iter().all(|host| host == node.node_id()), "a node was told who holds the file");
    }
    assert!(c.state.store.get("file_p").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_fetched_until_the_device_accepts() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;

    assert!(b.state.store.get("file_p").is_none());
    assert_eq!(ask(b, a, GetChunks::for_indices("file_p", &[0])).await, [Reply::Error(ErrorCode::NotFound)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_receiving_node_can_fetch_the_file() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob", "carol"]).await;
    let (a, b, c) = (&nodes[0], &nodes[1], &nodes[2]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;
    push::accept(&b.state, "bob", "file_p").await.unwrap();
    assert!(eventually_within(TRANSFER_TIMEOUT, async || b.state.store.contains_complete("file_p")).await);

    assert_eq!(ask(c, b, GetChunks::for_indices("file_p", &[0])).await, [Reply::Error(ErrorCode::NotFound)]);
    assert_eq!(ask(a, b, GetChunks::for_indices("file_p", &[0])).await, [Reply::Error(ErrorCode::NotFound)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_third_node_cannot_fetch_from_the_sender_while_the_file_is_on_its_way() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob", "carol"]).await;
    let (a, c) = (&nodes[0], &nodes[2]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;
    // Accepted, as the sender sees it, without the receiver fetching and so ending the push.
    push::on_answer(&a.state, &nodes[1].node_id().to_string(), "file_p", true).await;

    assert_eq!(ask(c, a, GetChunks::for_indices("file_p", &[0])).await, [Reply::Error(ErrorCode::NotFound)]);
    assert!(matches!(ask(&nodes[1], a, GetChunks::for_indices("file_p", &[0])).await[..], [Reply::ChunkData { index: 0, .. }]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_file_is_dropped_by_the_sender_and_never_reaches_the_receiver() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;

    assert!(push::decline(&b.state, "bob", "file_p").await);

    let declined = next_message(&mut tabs[0]).await;
    assert!(matches!(&declined, ServerMessage::FileOfferDeclined { from_session_id, .. } if from_session_id == "bob"), "{declined:?}");
    assert!(a.state.store.get("file_p").is_none() && b.state.store.get("file_p").is_none());
    assert_eq!(push::accept(&b.state, "bob", "file_p").await, Err(Refused::NoSuchOffer));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_device_the_file_was_sent_to_can_answer_the_offer() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let b = &nodes[1];
    let _carol = b.open_tab("carol").await;
    let file = send(&nodes[0], "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;

    assert_eq!(push::accept(&b.state, "carol", "file_p").await, Err(Refused::NoSuchOffer));
    assert!(!push::decline(&b.state, "carol", "file_p").await);
    assert!(push::accept(&b.state, "bob", "file_p").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_on_one_node_need_no_transfer() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice"]).await;
    let a = &nodes[0];
    let mut bob = a.open_tab("bob").await;
    let content = three_chunks();
    let file = send(a, "alice", "bob", &content).await.unwrap();
    expect_prompt(&mut bob, &file).await;

    push::accept(&a.state, "bob", "file_p").await.unwrap();

    assert!(matches!(next_message(&mut tabs[0]).await, ServerMessage::FilePushDelivered { .. }));
    let (blob, _) = push::open(&a.state, "file_p", "bob").unwrap();
    assert!(blob.is_complete());
    assert_eq!(contents(a, "file_p").await, content);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_for_a_device_that_is_not_there_is_refused() {
    let (nodes, _tabs) = mesh_with_devices(&["alice"]).await;
    assert_eq!(send(&nodes[0], "alice", "nobody", &three_chunks()).await, Err(Refused::NoSuchDevice));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offer_naming_a_device_the_sending_node_does_not_host_is_ignored() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob", "carol"]).await;
    let (b, c) = (&nodes[1], &nodes[2]);
    let forged = PushedFile {
        file_id: "file_p".into(),
        name: "from-alice.jpg".into(),
        size: 10,
        mime_type: "image/jpeg".into(),
        manifest_root: "ab".repeat(32),
        from_session: "alice".into(),
        to_session: "bob".into(),
    };

    push::on_offer(&b.state, &c.node_id().to_string(), &forged).await;

    assert_eq!(push::accept(&b.state, "bob", "file_p").await, Err(Refused::NoSuchOffer));
    let prompted = std::iter::from_fn(|| tabs[1].try_recv().ok()).any(|message| matches!(message, ServerMessage::IncomingFileOffer { .. }));
    assert!(!prompted, "the device was prompted");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_from_another_node_is_cleaned_before_the_device_sees_it() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let hostile = PushedFile {
        file_id: "file_p".into(),
        name: "../../etc/passwd".into(),
        size: 10,
        mime_type: "text/plain".into(),
        manifest_root: "ab".repeat(32),
        from_session: "alice".into(),
        to_session: "bob".into(),
    };

    push::on_offer(&b.state, &a.node_id().to_string(), &hostile).await;

    let ServerMessage::IncomingFileOffer { pushed: Some(shown), .. } = next_message(&mut tabs[1]).await else { panic!("no offer") };
    assert_eq!(shown.name, ladex_core::validate::sanitize_file_name("../../etc/passwd"));
    assert!(!shown.name.contains('/'));
}

#[tokio::test(flavor = "multi_thread")]
async fn pushed_files_no_push_refers_to_are_cleaned_up() {
    let (nodes, mut tabs) = mesh_with_devices(&["alice", "bob"]).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let file = send(a, "alice", "bob", &three_chunks()).await.unwrap();
    expect_prompt(&mut tabs[1], &file).await;
    push::collect_garbage(&a.state);
    assert!(a.state.store.get("file_p").is_some(), "a file on offer is kept");

    // What a restart leaves: the file on disk, and no memory of the push.
    let leftover = b.state.store.create_with("file_old", 10, Origin::Pushed).unwrap();
    leftover.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
    leftover.seal().await.unwrap();
    push::collect_garbage(&b.state);

    assert!(b.state.store.get("file_old").is_none());
}
