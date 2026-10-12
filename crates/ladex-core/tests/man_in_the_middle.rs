//! A relay that terminates QUIC on both sides, with a key of its own, is caught by the handshake's channel binding.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use ladex_core::identity::Identity;
use ladex_core::mesh::endpoint::MeshEndpoint;
use ladex_core::mesh::{self, AuthFailure};
use ladex_core::testing::{eventually, spawn_node, NodeConfig, NodeHandle, UdpRelay};

/// Accepts connections with its own key and bridges each one's first stream to a new connection to `target`.
fn spawn_attacker(target: SocketAddr) -> SocketAddr {
    let endpoint = Arc::new(MeshEndpoint::bind(&Identity::generate(), (Ipv4Addr::LOCALHOST, 0).into()).unwrap());
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(accepting) = endpoint.accept().await {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let _ = bridge(&endpoint, accepting, target).await;
            });
        }
    });
    addr
}

async fn bridge(endpoint: &MeshEndpoint, accepting: ladex_core::mesh::endpoint::Accepting, target: SocketAddr) -> anyhow::Result<()> {
    let from_dialer = accepting.establish().await?;
    let to_server = endpoint.connect_to_address(target).await?;
    let (mut to_dialer, mut dialer_says) = from_dialer.connection.accept_bi().await?;
    let (mut to_target, mut target_says) = to_server.connection.open_bi().await?;
    let _ = tokio::join!(tokio::io::copy(&mut dialer_says, &mut to_target), tokio::io::copy(&mut target_says, &mut to_dialer));
    Ok(())
}

async fn two_nodes() -> (NodeHandle, NodeHandle) {
    let dialer = spawn_node(NodeConfig::named("dialer").passphrase("pw")).await;
    let server = spawn_node(NodeConfig::named("server").passphrase("pw")).await;
    (dialer, server)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_only_forwards_datagrams_lets_the_nodes_connect() {
    let (dialer, server) = two_nodes().await;
    let relay = UdpRelay::start(server.addr).await;
    dialer.connect(relay.addr()).await.unwrap();
    assert!(eventually(async || server.mesh_peers().await.contains(dialer.node_id())).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_terminates_quic_twice_fails_confirmation() {
    let (dialer, server) = two_nodes().await;
    let attacker = spawn_attacker(server.addr);

    let failure = dialer.connect(attacker).await.unwrap_err();
    assert!(failure.downcast_ref::<AuthFailure>().is_some(), "{failure}");
    assert!(dialer.mesh_peers().await.is_empty());
    assert!(server.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_terminates_quic_twice_cannot_get_a_node_found_by_discovery_to_talk() {
    let (dialer, server) = two_nodes().await;
    let attacker = spawn_attacker(server.addr);

    let failure = mesh::quic::connect(&dialer.state, attacker, &server.state.node_id).await.unwrap_err();
    assert!(format!("{failure:#}").contains("dialed") || format!("{failure:#}").to_lowercase().contains("certificate"), "{failure:#}");
    assert!(dialer.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_terminates_quic_twice_gets_no_pairing_code_shown() {
    let (dialer, server) = two_nodes().await;
    server.state.pairings.open();
    let attacker = spawn_attacker(server.addr);

    let failure = mesh::quic::pair(&dialer.state, attacker).await.unwrap_err();
    assert!(failure.downcast_ref::<AuthFailure>().is_some(), "{failure}");
    assert!(dialer.state.pairings.status().waiting.is_empty() && server.state.pairings.status().waiting.is_empty());
    assert!(dialer.mesh_peers().await.is_empty() && server.mesh_peers().await.is_empty());
}
