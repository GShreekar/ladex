//! Phones are part-time seeds: they may vanish at any moment, and a download with another source must carry on without them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ladex_core::state;
use ladex_core::store::{Blob, CHUNK_SIZE};
use ladex_core::testing::{eventually, eventually_within, Faults, LinkedMesh, NodeConfig};
use ladex_core::transfer;

const DESKTOP: usize = 0;
const PHONE: usize = 1;
const LAPTOP: usize = 2;

const FILE_BYTES: usize = 128 * 1024 * 1024;
// A slow round trip makes every source keep many chunks in flight, more than the endgame would ask the desktop for again.
const LATENCY: Faults = Faults { drop: 0.0, delay: Duration::from_millis(100), jitter: Duration::ZERO, reorder: 0.0 };
// The phone vanishes with this many chunks left: little enough that the desktop runs out of other work while the phone's are stuck.
const CHUNKS_LEFT_WHEN_VANISHING: u32 = 32;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
// Twice the 3 s test nodes give a part-time source's silent stream, and well under the 10 s a steady source gets.
const LONGEST_ACCEPTABLE_STALL: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn content() -> Vec<u8> {
    (0..FILE_BYTES).map(|i| (i.wrapping_mul(29) % 253) as u8).collect()
}

async fn desktop_phone_and_laptop() -> LinkedMesh {
    LinkedMesh::spawn_with(vec![NodeConfig::named("desktop"), NodeConfig::named("phone").part_time(), NodeConfig::named("laptop")]).await
}

async fn download(mesh: &LinkedMesh, node: usize, id: &str) -> Arc<Blob> {
    transfer::ensure_file(&mesh.nodes[node].state, id).await.expect("the download starts")
}

async fn read_all(blob: &Blob) -> Vec<u8> {
    let mut contents = Vec::new();
    for index in 0..blob.chunk_count() {
        contents.extend(blob.read_chunk(index).await.unwrap());
    }
    contents
}

/// A phone shares the file and the desktop copies it whole, so the laptop then has two seeds; returns the laptop's download, under way.
async fn laptop_downloading_from_desktop_and_phone(mesh: &LinkedMesh, content: &[u8]) -> Arc<Blob> {
    mesh.nodes[PHONE].share_file("photos", content).await;
    assert!(eventually(async || !mesh.nodes[DESKTOP].hosts_of("photos").await.is_empty()).await);
    let at_desktop = download(mesh, DESKTOP, "photos").await;
    assert!(eventually_within(TRANSFER_TIMEOUT, async || at_desktop.is_complete()).await, "the desktop never got the file");
    assert!(eventually(async || mesh.nodes[LAPTOP].hosts_of("photos").await.len() == 2).await, "the laptop never saw two seeds");

    mesh.set_faults_everywhere(LATENCY);
    let at_laptop = download(mesh, LAPTOP, "photos").await;
    let near_the_end = at_laptop.chunk_count() - CHUNKS_LEFT_WHEN_VANISHING;
    assert!(eventually_within(TRANSFER_TIMEOUT, async || at_laptop.bitmap().count() >= near_the_end).await);
    let from_phone = mesh.link(PHONE, LAPTOP).bytes_delivered_to(mesh.nodes[LAPTOP].node_id());
    assert!(from_phone >= CHUNK_SIZE, "the phone sent the laptop only {from_phone} bytes, so it was never a source");
    at_laptop
}

/// The longest the download went without a new chunk until it finished; None if it did not finish in time.
async fn longest_stall_until_complete(blob: &Blob) -> Option<Duration> {
    let deadline = Instant::now() + TRANSFER_TIMEOUT;
    let mut longest = Duration::ZERO;
    let mut last_count = blob.bitmap().count();
    let mut last_progress = Instant::now();
    while !blob.is_complete() {
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        let count = blob.bitmap().count();
        if count != last_count {
            last_count = count;
            last_progress = Instant::now();
        }
        longest = longest.max(last_progress.elapsed());
    }
    Some(longest)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_tells_its_peers_it_is_a_part_time_seed() {
    let mesh = desktop_phone_and_laptop().await;
    let phone = mesh.nodes[PHONE].node_id();
    let desktop = mesh.nodes[DESKTOP].node_id();

    assert!(mesh.nodes[LAPTOP].state.is_part_time_node(phone));
    assert!(!mesh.nodes[LAPTOP].state.is_part_time_node(desktop));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_that_leaves_is_no_longer_counted_as_a_part_time_peer() {
    let mut mesh = desktop_phone_and_laptop().await;
    let phone = mesh.nodes[PHONE].node_id().to_string();

    drop(mesh.nodes.remove(PHONE));

    let laptop = &mesh.nodes[LAPTOP - 1];
    assert!(eventually(async || !laptop.mesh_peers().await.contains(&phone)).await);
    assert!(!laptop.state.is_part_time_node(&phone));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_only_a_phone_has_is_flagged_on_every_node() {
    let mesh = desktop_phone_and_laptop().await;

    mesh.nodes[PHONE].share_file("photo", b"a photo").await;

    let flagged = |node: usize| state::file_view(&mesh.nodes[node].state, "photo").is_some_and(|file| file.only_part_time_hosts);
    assert!(eventually(async || flagged(PHONE) && flagged(DESKTOP) && flagged(LAPTOP)).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_phone_shared_is_no_longer_flagged_once_a_desktop_has_it_too() {
    let mesh = desktop_phone_and_laptop().await;
    mesh.nodes[PHONE].share_file("photo", b"a photo").await;
    assert!(eventually(async || !mesh.nodes[DESKTOP].hosts_of("photo").await.is_empty()).await);

    let at_desktop = download(&mesh, DESKTOP, "photo").await;

    assert!(eventually(async || at_desktop.is_complete()).await);
    let cleared = |node: usize| state::file_view(&mesh.nodes[node].state, "photo").is_some_and(|file| !file.only_part_time_hosts);
    assert!(eventually(async || cleared(PHONE) && cleared(DESKTOP) && cleared(LAPTOP)).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_desktop_shared_is_never_flagged() {
    let mesh = desktop_phone_and_laptop().await;

    let shared = mesh.nodes[DESKTOP].share_file("notes", b"some notes").await;

    assert!(!shared.only_part_time_hosts);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_that_vanishes_mid_transfer_does_not_stall_a_download_with_another_source() {
    let mesh = desktop_phone_and_laptop().await;
    let content = content();
    let at_laptop = laptop_downloading_from_desktop_and_phone(&mesh, &content).await;

    // Silent, as when the phone loses Wi-Fi or its app is killed: nothing says goodbye.
    mesh.isolate(PHONE);

    let longest_stall = longest_stall_until_complete(&at_laptop).await.expect("the download never finished");
    assert!(longest_stall < LONGEST_ACCEPTABLE_STALL, "the download stalled for {longest_stall:?}");
    assert_eq!(read_all(&at_laptop).await, content);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_whose_app_closes_mid_transfer_does_not_stop_a_download_with_another_source() {
    let mut mesh = desktop_phone_and_laptop().await;
    let content = content();
    let at_laptop = laptop_downloading_from_desktop_and_phone(&mesh, &content).await;

    drop(mesh.nodes.remove(PHONE));

    let longest_stall = longest_stall_until_complete(&at_laptop).await.expect("the download never finished");
    assert!(longest_stall < LONGEST_ACCEPTABLE_STALL, "the download stalled for {longest_stall:?}");
    assert_eq!(read_all(&at_laptop).await, content);
}
