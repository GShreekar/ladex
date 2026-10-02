// Multi-node meshes running in-process over real loopback sockets.

use ladex_core::testing::{converged, eventually, fully_connected, spawn_mesh, spawn_node, NodeConfig};

const PASSPHRASE: &str = "correct horse battery";

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_with_the_same_passphrase_join_over_tls() {
    let a = spawn_node(NodeConfig::named("node_a").passphrase(PASSPHRASE).tls()).await;
    let b = spawn_node(NodeConfig::named("node_b").passphrase(PASSPHRASE).tls().peer(a.addr)).await;
    let nodes = [a, b];
    assert!(eventually(async || fully_connected(&nodes).await).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_the_wrong_passphrase_is_refused() {
    let a = spawn_node(NodeConfig::named("node_a").passphrase(PASSPHRASE)).await;
    let b = spawn_node(NodeConfig::named("node_b").passphrase("a different secret")).await;
    assert!(b.connect(a.addr).await.is_err());
    assert!(a.mesh_peers().await.is_empty());
    assert!(b.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_node_cannot_join_a_secured_mesh() {
    let a = spawn_node(NodeConfig::named("node_a").passphrase(PASSPHRASE)).await;
    let b = spawn_node(NodeConfig::named("node_b")).await;
    assert!(b.connect(a.addr).await.is_err());
    assert!(a.mesh_peers().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn five_nodes_converge_on_the_same_catalog_and_chat() {
    let nodes = spawn_mesh(5, Some(PASSPHRASE)).await;
    for (i, node) in nodes.iter().enumerate() {
        node.share_file(&format!("file_{i}"), format!("contents of file {i}").as_bytes()).await;
        node.say(&format!("hello from node {i}")).await;
    }
    assert!(eventually(async || converged(&nodes).await).await);
    assert_eq!(nodes[0].live_files().await.len(), 5);
    assert_eq!(nodes[0].chat().await.len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_joins_late_receives_everything_shared_before() {
    let mut nodes = spawn_mesh(3, None).await;
    nodes[0].share_file("early_file", b"shared before the fourth node existed").await;
    nodes[1].say("said before the fourth node existed").await;

    let mut late = NodeConfig::named("node_late");
    late.peers = nodes.iter().map(|node| node.addr).collect();
    nodes.push(spawn_node(late).await);

    assert!(eventually(async || converged(&nodes).await).await);
    assert!(nodes[3].live_files().await.contains("early_file"));
    assert_eq!(nodes[3].chat().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsharing_a_file_removes_it_on_every_node() {
    let nodes = spawn_mesh(3, None).await;
    nodes[0].share_file("short_lived", b"soon gone").await;
    assert!(eventually(async || nodes[2].live_files().await.contains("short_lived")).await);

    nodes[0].unshare_file("short_lived").await;
    assert!(eventually(async || converged(&nodes).await).await);
    assert!(nodes[1].live_files().await.is_empty());
    assert!(nodes[2].live_files().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn when_a_node_says_goodbye_the_others_drop_it_and_its_files_become_unavailable() {
    let nodes = spawn_mesh(3, None).await;
    nodes[2].share_file("leaving_file", b"held only by the node that leaves").await;
    assert!(eventually(async || converged(&nodes).await).await);

    nodes[2].say_goodbye().await;
    let leaver = nodes[2].node_id().to_string();
    let remaining = &nodes[..2];
    let dropped_and_unavailable = async || {
        for node in remaining {
            let unavailable = node.state.files.read().await.get("leaving_file").is_some_and(|f| !f.is_held_by(&leaver));
            if node.mesh_peers().await.contains(&leaver) || !unavailable {
                return false;
            }
        }
        true
    };
    assert!(eventually(dropped_and_unavailable).await);
}
