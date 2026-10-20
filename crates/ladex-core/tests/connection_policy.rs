//! Which node dials, which of two connections stays, and how lost connections come back.

use std::time::Duration;

use ladex_core::mesh;
use ladex_core::testing::{eventually, eventually_within, fully_connected, spawn_node, NodeConfig, NodeHandle, UdpRelay};
use ladex_core::types::NodeId;

// Past the QUIC idle timeout, with room for a busy test machine.
const IDLE_TIMEOUT_WAIT: Duration = Duration::from_secs(25);
// Still inside the idle timeout, so a silent peer must not have been dropped yet.
const SHORT_SILENCE: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_millis(1500);

async fn spawn_pair() -> Vec<NodeHandle> {
    let config = |name: &str| NodeConfig::named(name).passphrase("pw");
    vec![spawn_node(config("a")).await, spawn_node(config("b")).await]
}

/// The two nodes ordered by node id: the lower one is the dialer.
fn by_node_id(nodes: &[NodeHandle]) -> (&NodeHandle, &NodeHandle) {
    if nodes[0].node_id() < nodes[1].node_id() {
        (&nodes[0], &nodes[1])
    } else {
        (&nodes[1], &nodes[0])
    }
}

async fn dialer_of(node: &NodeHandle, peer: &NodeHandle) -> Option<NodeId> {
    node.state.mesh_peers.read().await.get(peer.node_id()).map(|handle| handle.dialer.clone())
}

/// Whether both nodes hold one connection, and it is the one `dialer` opened.
async fn connected_by(nodes: &[NodeHandle], dialer: &NodeHandle) -> bool {
    let (lower, higher) = by_node_id(nodes);
    let expected = Some(dialer.state.node_id.clone());
    fully_connected(nodes).await && dialer_of(lower, higher).await == expected && dialer_of(higher, lower).await == expected
}

async fn apart(a: &NodeHandle, b: &NodeHandle) -> bool {
    !a.mesh_peers().await.contains(b.node_id()) && !b.mesh_peers().await.contains(a.node_id())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovered_peer_is_dialed_by_the_lower_node() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    mesh::dialer::dial(&lower.state, &higher.state.node_id, higher.addr);
    assert!(eventually(async || connected_by(&nodes, lower).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_higher_node_never_dials() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    mesh::dialer::dial(&higher.state, &lower.state.node_id, lower.addr);
    tokio::time::sleep(SETTLE).await;
    assert!(apart(lower, higher).await);
    assert!(higher.state.dial_targets.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_higher_node_dials_a_peer_it_was_given_by_address() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    higher.state.kept_peers.lock().unwrap().insert(lower.state.node_id.clone());
    mesh::dialer::dial(&higher.state, &lower.state.node_id, lower.addr);
    assert!(eventually(async || connected_by(&nodes, higher).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_given_by_address_is_retried_until_it_answers() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    let relay = UdpRelay::start(lower.addr).await;
    relay.go_silent();
    let keeping = tokio::spawn({
        let state = higher.state.clone();
        let addr = relay.addr();
        async move { mesh::dialer::keep_connected(&state, addr).await }
    });
    tokio::time::sleep(SHORT_SILENCE).await;
    assert!(apart(lower, higher).await);

    relay.resume();
    assert!(eventually_within(IDLE_TIMEOUT_WAIT, async || fully_connected(&nodes).await).await, "never retried");
    keeping.await.unwrap();
    assert!(higher.state.kept_peers.lock().unwrap().contains(lower.node_id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_given_by_address_that_is_already_connected_is_kept() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    lower.connect_quic(higher).await.unwrap();
    assert!(eventually(async || connected_by(&nodes, lower).await).await);

    mesh::dialer::keep_connected(&higher.state, lower.addr).await;
    assert!(higher.state.kept_peers.lock().unwrap().contains(lower.node_id()));
    assert!(connected_by(&nodes, lower).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dial_loop_ends_once_connected() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    mesh::dialer::dial(&lower.state, &higher.state.node_id, higher.addr);
    assert!(eventually(async || fully_connected(&nodes).await && lower.state.dial_targets.lock().unwrap().is_empty()).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_dialing_each_other_at_once_keep_the_connection_the_lower_node_opened() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    let _ = tokio::join!(lower.connect_quic(higher), higher.connect_quic(lower));
    assert!(eventually(async || connected_by(&nodes, lower).await).await);
    tokio::time::sleep(SETTLE).await;
    assert!(connected_by(&nodes, lower).await, "the surviving connection did not stay up");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dial_from_the_higher_node_is_turned_away_while_the_lower_nodes_connection_is_open() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    lower.connect_quic(higher).await.unwrap();
    assert!(eventually(async || connected_by(&nodes, lower).await).await);
    assert!(higher.connect_quic(lower).await.is_err());
    assert!(connected_by(&nodes, lower).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redial_from_the_lower_node_replaces_its_open_connection() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    lower.connect_quic(higher).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);
    let old = higher.state.mesh_peers.read().await[lower.node_id()].sender.clone();

    lower.connect_quic(higher).await.unwrap();
    let replaced = async || higher.state.mesh_peers.read().await.get(lower.node_id()).is_some_and(|h| !h.sender.same_channel(&old));
    assert!(eventually(async || fully_connected(&nodes).await && replaced().await).await);
    tokio::time::sleep(SETTLE).await;
    assert!(fully_connected(&nodes).await, "closing the old connection took down the new one");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_hangs_up_is_not_redialed() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    lower.connect_quic(higher).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);

    mesh::remove_mesh_peer(&higher.state, &lower.state.node_id).await;
    assert!(eventually(async || apart(lower, higher).await).await);
    tokio::time::sleep(SETTLE).await;
    assert!(apart(lower, higher).await, "the lower node redialed a peer that hung up on purpose");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_peer_is_kept_until_the_idle_timeout_passes() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    let relay = UdpRelay::start(higher.addr).await;
    mesh::quic::connect(&lower.state, relay.addr(), &higher.state.node_id).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);

    relay.go_silent();
    tokio::time::sleep(SHORT_SILENCE).await;
    assert!(fully_connected(&nodes).await, "dropped before the idle timeout");
    assert!(eventually_within(IDLE_TIMEOUT_WAIT, async || apart(lower, higher).await).await, "never dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_lost_to_silence_is_redialed_by_the_lower_node_when_the_link_returns() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    let relay = UdpRelay::start(higher.addr).await;
    mesh::quic::connect(&lower.state, relay.addr(), &higher.state.node_id).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);

    relay.go_silent();
    assert!(eventually_within(IDLE_TIMEOUT_WAIT, async || apart(lower, higher).await).await, "never dropped");
    relay.resume();
    assert!(eventually_within(IDLE_TIMEOUT_WAIT, async || connected_by(&nodes, lower).await).await, "never redialed");
}

// The idle timeout plus a keep-alive interval is 13 s, so a killed node is gone by then.
const KILL_DETECTION: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_node_is_detected_within_fifteen_seconds() {
    let nodes = spawn_pair().await;
    let (lower, higher) = by_node_id(&nodes);
    let relay = UdpRelay::start(higher.addr).await;
    mesh::quic::connect(&lower.state, relay.addr(), &higher.state.node_id).await.unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await);

    // A killed process says nothing, which the relay's silence reproduces for both ends.
    let killed_at = std::time::Instant::now();
    relay.go_silent();
    assert!(eventually_within(KILL_DETECTION, async || apart(lower, higher).await).await, "not detected within 15 s");
    println!("detected after {:?}", killed_at.elapsed());
}
