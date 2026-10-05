//! Pairing two real nodes over the /mesh WebSocket.

use ladex_core::mesh;
use ladex_core::pairing::WaitingCode;
use ladex_core::testing::{eventually, fully_connected, spawn_node, NodeConfig, NodeHandle};
use ladex_core::trust::{Standing, TrustedVia};

async fn waiting_code(node: &NodeHandle) -> WaitingCode {
    let mut code = None;
    let shown = eventually(async || {
        code = node.state.pairings.status().waiting.into_iter().next();
        code.is_some()
    })
    .await;
    assert!(shown, "{} never showed a pairing code", node.state.node_name);
    code.unwrap()
}

fn pinned_by_pairing(node: &NodeHandle, other: &NodeHandle) -> bool {
    match node.state.trust.standing(&other.state.identity.public_key()).unwrap() {
        Standing::Trusted(record) => record.trusted_via == TrustedVia::Pairing,
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nodes_with_different_passphrases_pair_when_both_people_confirm() {
    let phone = spawn_node(NodeConfig::named("phone").passphrase("one").tls()).await;
    let laptop = spawn_node(NodeConfig::named("laptop").passphrase("two").tls()).await;
    laptop.state.pairings.open();

    let dialing = tokio::spawn(mesh::pair_with_peer(laptop.addr.ip(), laptop.addr.port(), phone.state.clone()));
    let (on_phone, on_laptop) = (waiting_code(&phone).await, waiting_code(&laptop).await);
    assert_eq!(on_phone.request.words, on_laptop.request.words);
    assert_eq!(on_phone.request.name, "laptop");
    assert!(on_phone.request.dialed && !on_laptop.request.dialed);

    phone.state.pairings.answer(on_phone.id, true);
    laptop.state.pairings.answer(on_laptop.id, true);
    assert_eq!(dialing.await.unwrap().unwrap(), "laptop");

    let nodes = [phone, laptop];
    assert!(eventually(async || fully_connected(&nodes).await).await);
    assert!(pinned_by_pairing(&nodes[0], &nodes[1]) && pinned_by_pairing(&nodes[1], &nodes[0]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_pairing_pins_nobody() {
    let phone = spawn_node(NodeConfig::named("phone").passphrase("pw")).await;
    let laptop = spawn_node(NodeConfig::named("laptop").passphrase("pw")).await;
    laptop.state.pairings.open();

    let dialing = tokio::spawn(mesh::pair_with_peer(laptop.addr.ip(), laptop.addr.port(), phone.state.clone()));
    let (on_phone, on_laptop) = (waiting_code(&phone).await, waiting_code(&laptop).await);
    phone.state.pairings.answer(on_phone.id, true);
    laptop.state.pairings.answer(on_laptop.id, false);

    let failure = dialing.await.unwrap().unwrap_err();
    assert!(failure.to_string().contains("declined on the other device"), "{failure}");
    assert!(!pinned_by_pairing(&phone, &laptop) && !pinned_by_pairing(&laptop, &phone));
    assert!(phone.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_has_not_opened_pairing_refuses_it() {
    let phone = spawn_node(NodeConfig::named("phone").passphrase("pw")).await;
    let laptop = spawn_node(NodeConfig::named("laptop").passphrase("pw")).await;

    let failure = mesh::pair_with_peer(laptop.addr.ip(), laptop.addr.port(), phone.state.clone()).await.unwrap_err();
    assert!(failure.to_string().contains("not accepting pairings"), "{failure}");
    assert!(laptop.state.pairings.status().waiting.is_empty());
}
