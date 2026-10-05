//! Meshes whose nodes talk through a lossy, slow, reordering or cut link.

use std::time::Duration;

use ladex_core::testing::{converged, eventually, Faults, LinkedMesh};
use ladex_core::transfer;

const SLOW_AND_REORDERING: Faults =
    Faults { drop: 0.0, delay: Duration::from_millis(20), jitter: Duration::from_millis(40), reorder: 0.25 };

fn three_chunk_file() -> Vec<u8> {
    (0..(2 * 1024 * 1024 + 4321)).map(|i| (i % 251) as u8).collect()
}

async fn fetched_contents(mesh: &LinkedMesh, node: usize, id: &str) -> Option<Vec<u8>> {
    let blob = transfer::ensure_file(&mesh.nodes[node].state, id).await.ok()?;
    if !eventually(async || blob.is_complete()).await {
        return None;
    }
    let mut contents = Vec::new();
    for index in 0..blob.chunk_count() {
        contents.extend(blob.read_chunk(index).await.ok()?);
    }
    Some(contents)
}

// Longer than the first redial, so a reconnection that went around the link would show up.
const PARTITION_HOLD: Duration = Duration::from_secs(4);

/// Cuts node `i` off and checks it stays cut off, with every node redialing the others meanwhile.
async fn isolate_and_hold(mesh: &LinkedMesh, i: usize) {
    mesh.isolate(i);
    assert!(eventually(async || mesh.nodes[i].mesh_peers().await.is_empty()).await, "node {i} was never cut off");
    tokio::time::sleep(PARTITION_HOLD).await;
    assert!(mesh.nodes[i].mesh_peers().await.is_empty(), "node {i} reconnected around the partition");
}

#[tokio::test(flavor = "multi_thread")]
async fn catalog_and_chat_converge_over_slow_reordering_links() {
    let mesh = LinkedMesh::spawn(3).await;
    mesh.set_faults_everywhere(SLOW_AND_REORDERING);
    for (i, node) in mesh.nodes.iter().enumerate() {
        node.share_file(&format!("file_{i}"), b"contents").await;
        for n in 0..10 {
            node.say(&format!("message {n} from node {i}")).await;
        }
    }
    assert!(eventually(async || converged(&mesh.nodes).await).await);
    assert_eq!(mesh.nodes[0].chat().await.len(), 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_arrives_intact_over_a_slow_reordering_link() {
    let mesh = LinkedMesh::spawn(2).await;
    let content = three_chunk_file();
    mesh.nodes[0].share_file("big_file", &content).await;
    assert!(eventually(async || mesh.nodes[1].live_files().await.contains("big_file")).await);

    mesh.set_faults_everywhere(SLOW_AND_REORDERING);
    assert_eq!(fetched_contents(&mesh, 1, "big_file").await, Some(content));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_arrives_intact_when_the_link_loses_messages() {
    let mesh = LinkedMesh::spawn(2).await;
    let content = three_chunk_file();
    mesh.nodes[0].share_file("big_file", &content).await;
    assert!(eventually(async || mesh.nodes[1].live_files().await.contains("big_file")).await);

    mesh.set_faults_everywhere(Faults { drop: 0.2, ..Default::default() });
    assert_eq!(fetched_contents(&mesh, 1, "big_file").await, Some(content));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_isolated_node_catches_up_once_the_partition_heals() {
    let mesh = LinkedMesh::spawn(3).await;
    isolate_and_hold(&mesh, 2).await;
    mesh.nodes[0].share_file("while_apart", b"shared during the partition").await;
    mesh.nodes[1].say("said during the partition").await;
    assert!(eventually(async || mesh.nodes[1].live_files().await.contains("while_apart")).await);
    assert!(mesh.nodes[2].live_files().await.is_empty());

    mesh.heal_everywhere();
    assert!(eventually(async || converged(&mesh.nodes).await).await);
    assert!(mesh.nodes[2].live_files().await.contains("while_apart"));
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_on_both_sides_of_a_partition_merge_once_it_heals() {
    let mesh = LinkedMesh::spawn(3).await;
    mesh.nodes[0].share_file("before", b"shared before the partition").await;
    assert!(eventually(async || converged(&mesh.nodes).await).await);

    isolate_and_hold(&mesh, 2).await;
    mesh.nodes[0].unshare_file("before").await;
    mesh.nodes[1].share_file("majority_side", b"shared by the larger side").await;
    mesh.nodes[2].share_file("minority_side", b"shared by the cut-off node").await;
    mesh.nodes[2].say("said by the cut-off node").await;

    mesh.heal_everywhere();
    assert!(eventually(async || converged(&mesh.nodes).await).await);
    let expected = ["majority_side", "minority_side"].map(String::from).into();
    assert_eq!(mesh.nodes[2].live_files().await, expected);
    assert_eq!(mesh.nodes[0].chat().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "no anti-entropy yet: an update lost on a live connection is never sent again"]
async fn an_update_lost_on_a_live_connection_still_arrives_eventually() {
    let mesh = LinkedMesh::spawn(2).await;
    mesh.set_faults_everywhere(Faults { drop: 1.0, ..Default::default() });
    mesh.nodes[0].share_file("lost_update", b"its catalog entry is lost").await;
    // Long enough for the update to reach the link and be dropped there.
    tokio::time::sleep(Duration::from_millis(200)).await;
    mesh.set_faults_everywhere(Faults::default());
    assert!(eventually(async || converged(&mesh.nodes).await).await);
}
