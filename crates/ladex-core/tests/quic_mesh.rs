//! Real nodes joining each other over QUIC, the way discovery connects them.

use ladex_core::mesh::{self, AuthFailure};
use ladex_core::store::{hash_chunk, CHUNK_SIZE};
use ladex_core::testing::{converged, eventually, fully_connected, spawn_node, NodeConfig, NodeHandle};
use ladex_core::transfer::wire::{ErrorCode, Fetch, GetChunks, Reply};
use ladex_core::{transfer, NodeState};
use tokio::sync::mpsc;

async fn spawn_pair(passphrase: Option<&str>) -> Vec<NodeHandle> {
    let config = |name: &str| NodeConfig { passphrase: passphrase.map(String::from), ..NodeConfig::named(name) };
    vec![spawn_node(config("a")).await, spawn_node(config("b")).await]
}

async fn spawn_joined_pair(passphrase: &str) -> Vec<NodeHandle> {
    let nodes = spawn_pair(Some(passphrase)).await;
    nodes[0].connect_quic(&nodes[1]).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await, "the nodes never connected");
    nodes
}

/// Ends the connection between `a` and `b` and waits until neither lists the other.
async fn disconnect(a: &NodeHandle, b: &NodeHandle) {
    let hang_up = a.state.mesh_peers.read().await[b.node_id()].hang_up.clone();
    hang_up.notify_one();
    let apart = eventually(async || !a.mesh_peers().await.contains(b.node_id()) && !b.mesh_peers().await.contains(a.node_id())).await;
    assert!(apart, "the nodes never disconnected");
}

/// `node` with a different passphrase, sharing everything else with it.
fn with_passphrase(node: &NodeHandle, passphrase: &str) -> NodeState {
    NodeState { passphrase: Some(passphrase.to_string()), ..node.state.clone() }
}

async fn dial(state: &NodeState, to: &NodeHandle) -> anyhow::Result<()> {
    mesh::quic::connect(state, to.addr, &to.state.node_id).await
}

#[tokio::test(flavor = "multi_thread")]
async fn two_open_nodes_join_over_quic() {
    let nodes = spawn_pair(None).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    a.connect_quic(b).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_right_passphrase_joins_a_secured_mesh_over_quic() {
    let nodes = spawn_pair(Some("pw")).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    a.connect_quic(b).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_passphrase_is_refused_as_an_authentication_failure() {
    let nodes = spawn_pair(Some("pw")).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let error = dial(&with_passphrase(a, "wrong"), b).await.unwrap_err();
    assert!(error.downcast_ref::<AuthFailure>().is_some(), "{error:#}");
    assert!(b.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn catalog_and_chat_reach_a_peer_over_quic() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    a.share_file("shared", b"hello over quic").await;
    b.say("hi").await;
    assert!(eventually(async || converged(&nodes).await && b.live_files().await.contains("shared")).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_joining_one_node_is_seen_by_its_peer_over_quic() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    a.connect_device("device_on_a").await;
    assert!(eventually(async || b.devices().await.contains("device_on_a")).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_over_quic_measure_the_round_trip() {
    let nodes = spawn_joined_pair("pw").await;
    let a = &nodes[0];
    let b_node_id = nodes[1].node_id();
    let measured = eventually(async || a.state.mesh_peers.read().await[b_node_id].rtt_ms.is_some()).await;
    assert!(measured, "no pong ever came back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_goodbye_over_quic_drops_the_departing_node() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    b.say_goodbye().await;
    assert!(eventually(async || a.mesh_peers().await.is_empty()).await, "the node that said goodbye is still listed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_of_several_chunks_downloads_over_quic() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let content: Vec<u8> = (0..(2 * 1024 * 1024 + 4321)).map(|i| (i % 251) as u8).collect();
    a.share_file("big", &content).await;
    assert!(eventually(async || !b.hosts_of("big").await.is_empty()).await);

    let blob = transfer::ensure_file(&b.state, "big").await.unwrap();
    assert!(eventually(async || blob.is_complete()).await, "the download never finished");
    let mut received = Vec::new();
    for index in 0..blob.chunk_count() {
        received.extend(blob.read_chunk(index).await.unwrap());
    }
    assert_eq!(received, content);
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_nodes_reconnect_without_proving_the_passphrase_again() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    disconnect(a, b).await;
    dial(&with_passphrase(a, "no longer known"), b).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_forgot_the_dialer_makes_it_prove_the_passphrase_again() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    disconnect(a, b).await;
    b.state.trust.forget(a.node_id()).unwrap();
    assert!(dial(&with_passphrase(a, "wrong"), b).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forgotten_dialer_that_knows_the_passphrase_rejoins_through_the_handshake() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    disconnect(a, b).await;
    b.state.trust.forget(a.node_id()).unwrap();
    a.connect_quic(b).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_node_cannot_reconnect_over_quic() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    mesh::revoke_node(&a.state, b.node_id()).await.unwrap();
    assert!(eventually(async || a.mesh_peers().await.is_empty()).await, "the revoked node was never dropped");
    assert!(b.connect_quic(a).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_shuts_down_is_dropped_by_its_peer() {
    let mut nodes = spawn_joined_pair("pw").await;
    drop(nodes.pop());
    let a = &nodes[0];
    assert!(eventually(async || a.mesh_peers().await.is_empty()).await, "the node that shut down is still listed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_told_to_join_its_own_address_refuses() {
    let node = spawn_node(NodeConfig::named("alone")).await;
    let failure = node.connect(node.addr).await.unwrap_err();
    assert!(failure.to_string().contains("itself"), "{failure}");
    assert!(node.mesh_peers().await.is_empty());
}

/// Sends `request` to `of` over its own QUIC stream and collects every reply.
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
async fn chunks_asked_for_over_a_stream_arrive_intact() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let content: Vec<u8> = (0..(3 * CHUNK_SIZE as usize + 99)).map(|i| (i % 253) as u8).collect();
    a.share_file("big", &content).await;

    let replies = ask(b, a, GetChunks::for_indices("big", &[3, 0, 1])).await;

    let [Reply::ChunkData { index: 0, bytes: first }, Reply::ChunkData { index: 1, .. }, Reply::ChunkData { index: 3, bytes: last }] =
        replies.as_slice()
    else {
        panic!("unexpected replies: {} of them", replies.len());
    };
    assert_eq!(first[..], content[..CHUNK_SIZE as usize]);
    assert_eq!(hash_chunk(last), hash_chunk(&content[3 * CHUNK_SIZE as usize..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_for_a_file_the_peer_lacks_is_refused_over_a_stream() {
    let nodes = spawn_joined_pair("pw").await;
    let replies = ask(&nodes[1], &nodes[0], GetChunks::for_indices("file_nope", &[0])).await;
    assert_eq!(replies, [Reply::Error(ErrorCode::NotFound)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn many_requests_in_flight_at_once_are_each_answered_on_their_own_stream() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let content: Vec<u8> = (0..(6 * CHUNK_SIZE as usize)).map(|i| (i % 241) as u8).collect();
    a.share_file("big", &content).await;

    let requests = (0..6).map(|index| ask(b, a, GetChunks::for_indices("big", &[index])));
    let answers = futures_util::future::join_all(requests).await;

    for (index, replies) in answers.iter().enumerate() {
        let start = index * CHUNK_SIZE as usize;
        let expected = Reply::ChunkData { index: index as u32, bytes: content[start..start + CHUNK_SIZE as usize].to_vec() };
        assert_eq!(replies, &[expected]);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_beyond_the_upload_slots_wait_their_turn_instead_of_failing() {
    let nodes = spawn_joined_pair("pw").await;
    let (a, b) = (&nodes[0], &nodes[1]);
    let content: Vec<u8> = (0..(2 * CHUNK_SIZE as usize)).map(|i| (i % 239) as u8).collect();
    a.share_file("big", &content).await;

    let requests = (0..24).map(|_| ask(b, a, GetChunks::for_indices("big", &[0, 1])));
    let answers = futures_util::future::join_all(requests).await;

    assert!(answers.iter().all(|replies| replies.len() == 2 && replies.iter().all(|r| matches!(r, Reply::ChunkData { .. }))));
}
