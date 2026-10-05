//! Signed revocations spreading through real nodes over the /mesh WebSocket.

use ladex_core::mesh::{self, Revoked};
use ladex_core::testing::{eventually, spawn_mesh, spawn_node, NodeConfig, NodeHandle};
use ladex_core::trust::Standing;

fn has_revoked(node: &NodeHandle, other: &NodeHandle) -> bool {
    node.state.trust.standing(&other.state.identity.public_key()).unwrap() == Standing::Revoked
}

async fn is_connected(node: &NodeHandle, other: &NodeHandle) -> bool {
    node.mesh_peers().await.contains(other.node_id())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_node_is_dropped_by_the_whole_mesh() {
    let nodes = spawn_mesh(3, Some("pw")).await;
    let (a, b, stolen) = (&nodes[0], &nodes[1], &nodes[2]);

    assert_eq!(mesh::revoke_node(&a.state, stolen.node_id()).await.unwrap(), Revoked::Now);

    assert!(eventually(async || has_revoked(b, stolen)).await, "the revocation never reached b");
    let dropped =
        eventually(async || !is_connected(a, stolen).await && !is_connected(b, stolen).await && stolen.mesh_peers().await.is_empty()).await;
    assert!(dropped, "the revoked node is still connected");
    assert!(is_connected(a, b).await);
    assert!(stolen.connect(b.addr).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_twice_or_an_unknown_node_is_reported() {
    let nodes = spawn_mesh(2, Some("pw")).await;
    let (a, b) = (&nodes[0], &nodes[1]);
    assert_eq!(mesh::revoke_node(&a.state, "nobody").await.unwrap(), Revoked::UnknownNode);
    mesh::revoke_node(&a.state, b.node_id()).await.unwrap();
    assert_eq!(mesh::revoke_node(&a.state, b.node_id()).await.unwrap(), Revoked::Already);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_joins_later_learns_of_earlier_revocations() {
    let nodes = spawn_mesh(3, Some("pw")).await;
    let (a, b, stolen) = (&nodes[0], &nodes[1], &nodes[2]);
    mesh::revoke_node(&a.state, stolen.node_id()).await.unwrap();
    assert!(eventually(async || has_revoked(b, stolen)).await);

    let newcomer = spawn_node(NodeConfig::named("newcomer").passphrase("pw").peer(b.addr)).await;
    assert!(eventually(async || has_revoked(&newcomer, stolen)).await, "the newcomer never heard of the revocation");
    assert!(stolen.connect(newcomer.addr).await.is_err());
}
