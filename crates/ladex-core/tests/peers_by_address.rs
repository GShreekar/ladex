//! Nodes added by address, for networks where discovery cannot find them: what the app's Network page and
//! `ladex settings --add-peer` do.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use ladex_core::mesh;
use ladex_core::preferences::{self, Update};
use ladex_core::testing::{eventually, fully_connected, spawn_node, NodeConfig, NodeHandle};

// Long enough for one dial to an address where nothing answers to time out and the retry to notice.
const GIVE_UP_WITHIN: Duration = Duration::from_secs(30);

async fn spawn_pair() -> Vec<NodeHandle> {
    vec![spawn_node(NodeConfig::named("a").passphrase("pw")).await, spawn_node(NodeConfig::named("b").passphrase("pw")).await]
}

fn peers(addresses: &[SocketAddr]) -> Update {
    Update { peer_addresses: Some(addresses.iter().map(ToString::to_string).collect()), ..Default::default() }
}

// A UDP port nobody listens on, found by binding one and letting it go.
fn silent_address() -> SocketAddr {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_an_address_connects_to_the_node_there() {
    let nodes = spawn_pair().await;
    preferences::apply(&nodes[1].state, &peers(&[nodes[0].addr])).unwrap();
    assert!(eventually(async || fully_connected(&nodes).await).await, "the nodes never connected");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_added_address_is_saved_for_the_next_start() {
    let nodes = spawn_pair().await;
    preferences::apply(&nodes[1].state, &peers(&[nodes[0].addr])).unwrap();
    assert_eq!(nodes[1].state.preferences.get().peer_addresses, vec![nodes[0].addr]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_address_is_no_longer_dialed() {
    let node = spawn_node(NodeConfig::named("a").passphrase("pw")).await;
    let address = silent_address();
    preferences::apply(&node.state, &peers(&[address])).unwrap();
    let dialing = tokio::spawn({
        let state = node.state.clone();
        async move { mesh::dialer::keep_connected(&state, address).await }
    });
    preferences::apply(&node.state, &peers(&[])).unwrap();
    assert!(tokio::time::timeout(GIVE_UP_WITHIN, dialing).await.is_ok(), "the removed address was still being dialed");
}
