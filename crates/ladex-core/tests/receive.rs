//! Saving received files to disk: hostile names and paths stay inside the downloads folder, nothing is overwritten, and a
//! file that is not what its sender promised is never left behind.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ladex_core::catalog::{CatalogEntry, EntryKind};
use ladex_core::files_api::{FolderListing, ListingChild};
use ladex_core::receive::{self, Content, ReceiveRoot, SaveError};
use ladex_core::store::{Blob, CHUNK_SIZE};
use ladex_core::testing::{eventually, spawn_mesh, NodeHandle};
use ladex_core::{state, NodeState};

const NO_WAIT: Duration = Duration::ZERO;
const DEVICE: &str = "device_sender";

fn three_chunks() -> Vec<u8> {
    (0..(2 * CHUNK_SIZE as usize + 4321)).map(|i| (i % 251) as u8).collect()
}

fn downloads() -> (tempfile::TempDir, ReceiveRoot) {
    let dir = tempfile::tempdir().unwrap();
    let root = ReceiveRoot::open(&dir.path().join("downloads")).unwrap();
    (dir, root)
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

async fn store_file(state: &NodeState, id: &str, content: &[u8]) -> (Arc<Blob>, String) {
    let blob = state.store.create(id, content.len() as u64).unwrap();
    for (index, chunk) in content.chunks(CHUNK_SIZE as usize).enumerate() {
        blob.write_chunk_hashing(index as u32, chunk.to_vec()).await.unwrap();
    }
    let root = blob.seal().await.unwrap();
    (blob, root)
}

/// Stores `content` on `node` and lists it in the catalog under `name`, as a file or as what `kind` says.
async fn share(node: &NodeHandle, id: &str, name: &str, content: &[u8], kind: EntryKind) {
    let (_, manifest_root) = store_file(&node.state, id, content).await;
    let entry = CatalogEntry {
        file_id: id.to_string(),
        name: name.to_string(),
        size: content.len() as u64,
        mime_type: "application/octet-stream".to_string(),
        created_at_ms: 1,
        manifest_root,
        uploader_id: DEVICE.to_string(),
        kind,
    };
    state::share_file(&node.state, entry).await.unwrap();
}

/// Shares a folder whose listing says exactly what `paths` says, each path with its own small file.
async fn share_folder(node: &NodeHandle, name: &str, paths: &[&str]) {
    let mut children = Vec::new();
    for (number, path) in paths.iter().enumerate() {
        let (file_id, content) = (format!("child_{number}"), format!("contents of file {number}"));
        share(node, &file_id, "child.txt", content.as_bytes(), EntryKind::File { parent: Some("folder_1".into()) }).await;
        children.push(ListingChild { path: path.to_string(), file_id, size: content.len() as u64 });
    }
    let kind = EntryKind::Folder { bytes: children.iter().map(|child| child.size).sum(), files: children.len() as u32 };
    let listing = FolderListing { v: 1, name: name.to_string(), children };
    share(node, "folder_1", name, &serde_json::to_vec(&listing).unwrap(), kind).await;
}

async fn knows_where_to_get(node: &NodeHandle, ids: &[&str]) -> bool {
    eventually(async || {
        for id in ids {
            if node.hosts_of(id).await.is_empty() {
                return false;
            }
        }
        true
    })
    .await
}

#[tokio::test]
async fn a_shared_file_is_fetched_from_another_node_and_saved_under_its_name() {
    let nodes = spawn_mesh(2, None).await;
    let (_dir, root) = downloads();
    share(&nodes[0], "file_1", "holiday.jpg", &three_chunks(), EntryKind::File { parent: None }).await;
    assert!(knows_where_to_get(&nodes[1], &["file_1"]).await);

    let saved = receive::save_shared(&nodes[1].state, &root, "file_1").await.unwrap();

    assert_eq!(saved, root.path().join("holiday.jpg"));
    assert_eq!(std::fs::read(saved).unwrap(), three_chunks());
}

#[tokio::test]
async fn saving_the_same_name_again_never_overwrites_the_first_file() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();
    let (first, first_root) = store_file(&state, "file_1", b"the first file").await;
    let (second, second_root) = store_file(&state, "file_2", b"a different file").await;

    let kept = root.save_file("notes.txt", &Content { blob: &first, size: 14, manifest_root: &first_root }, NO_WAIT).await.unwrap();
    let added = root.save_file("notes.txt", &Content { blob: &second, size: 16, manifest_root: &second_root }, NO_WAIT).await.unwrap();

    assert_eq!(added, root.path().join("notes (1).txt"));
    assert_eq!(std::fs::read(kept).unwrap(), b"the first file");
    assert_eq!(std::fs::read(added).unwrap(), b"a different file");
}

#[tokio::test]
async fn a_name_that_tries_to_climb_out_is_saved_inside_the_folder() {
    let state = NodeState::for_tests(None);
    let (dir, root) = downloads();
    let (blob, manifest_root) = store_file(&state, "file_1", b"payload").await;
    let content = Content { blob: &blob, size: 7, manifest_root: &manifest_root };

    let saved = root.save_file("../../../etc/cron.d/evil", &content, NO_WAIT).await.unwrap();

    assert_eq!(saved.parent(), Some(root.path()));
    assert_eq!(names_in(dir.path()), ["downloads"]);
}

#[tokio::test]
async fn an_empty_file_is_saved_empty() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();
    let (blob, manifest_root) = store_file(&state, "file_1", b"").await;

    let saved = root.save_file("empty.txt", &Content { blob: &blob, size: 0, manifest_root: &manifest_root }, NO_WAIT).await.unwrap();

    assert_eq!(std::fs::read(saved).unwrap(), b"");
}

#[tokio::test]
async fn a_file_that_is_not_the_promised_size_is_refused_and_leaves_nothing_behind() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();
    let (blob, manifest_root) = store_file(&state, "file_1", b"payload").await;

    let refused = root.save_file("a.bin", &Content { blob: &blob, size: 8, manifest_root: &manifest_root }, NO_WAIT).await;

    assert_eq!(refused, Err(SaveError::SizeMismatch));
    assert!(names_in(root.path()).is_empty());
}

#[tokio::test]
async fn a_file_that_is_not_the_promised_content_is_refused_and_leaves_nothing_behind() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();
    let (blob, _) = store_file(&state, "file_1", &three_chunks()).await;
    let (_, another_root) = store_file(&state, "file_2", &three_chunks()[1..]).await;
    let size = three_chunks().len() as u64;

    let refused = root.save_file("a.bin", &Content { blob: &blob, size, manifest_root: &another_root }, NO_WAIT).await;

    assert_eq!(refused, Err(SaveError::WrongContent));
    assert!(names_in(root.path()).is_empty());
}

#[tokio::test]
async fn a_file_with_a_part_missing_is_refused_and_leaves_nothing_behind() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();
    let (blob, manifest_root) = store_file(&state, "file_1", &three_chunks()).await;
    blob.mark_corrupt(1);
    let size = three_chunks().len() as u64;

    let refused = root.save_file("a.bin", &Content { blob: &blob, size, manifest_root: &manifest_root }, NO_WAIT).await;

    assert_eq!(refused, Err(SaveError::Unavailable));
    assert!(names_in(root.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_with_the_files_name_is_not_written_through() {
    let state = NodeState::for_tests(None);
    let (dir, root) = downloads();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, b"the user's own file").unwrap();
    std::os::unix::fs::symlink(&outside, root.path().join("report.txt")).unwrap();
    let (blob, manifest_root) = store_file(&state, "file_1", b"payload").await;

    let saved = root.save_file("report.txt", &Content { blob: &blob, size: 7, manifest_root: &manifest_root }, NO_WAIT).await.unwrap();

    assert_eq!(saved, root.path().join("report (1).txt"));
    assert_eq!(std::fs::read(outside).unwrap(), b"the user's own file");
}

#[tokio::test]
async fn a_folder_is_saved_as_real_directories_and_files() {
    let nodes = spawn_mesh(2, None).await;
    let (_dir, root) = downloads();
    share_folder(&nodes[0], "album", &["cover.txt", "disc 1/track.txt"]).await;
    assert!(knows_where_to_get(&nodes[1], &["folder_1", "child_0", "child_1"]).await);

    let saved = receive::save_shared(&nodes[1].state, &root, "folder_1").await.unwrap();

    assert_eq!(saved, root.path().join("album"));
    assert_eq!(std::fs::read(saved.join("cover.txt")).unwrap(), b"contents of file 0");
    assert_eq!(std::fs::read(saved.join("disc 1/track.txt")).unwrap(), b"contents of file 1");
}

#[tokio::test]
async fn a_folder_listing_cannot_place_files_outside_the_folder() {
    let node = &spawn_mesh(1, None).await[0];
    let (dir, root) = downloads();
    share_folder(node, "album", &["../../escaped.txt", "/etc/absolute.txt", "deep/../../../climb.txt", "C:\\windows\\style.txt"]).await;

    let saved = receive::save_shared(&node.state, &root, "folder_1").await.unwrap();

    assert_eq!(names_in(dir.path()), ["downloads"]);
    assert_eq!(names_in(root.path()), ["album"]);
    assert_eq!(names_in(&saved), ["C_", "deep", "escaped.txt", "etc"]);
}

#[tokio::test]
async fn two_listed_files_with_one_path_are_both_kept() {
    let node = &spawn_mesh(1, None).await[0];
    let (_dir, root) = downloads();
    share_folder(node, "album", &["track.txt", "./track.txt"]).await;

    let saved = receive::save_shared(&node.state, &root, "folder_1").await.unwrap();

    assert_eq!(names_in(&saved), ["track (1).txt", "track.txt"]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_with_the_folders_name_is_not_written_into() {
    let node = &spawn_mesh(1, None).await[0];
    let (dir, root) = downloads();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.path().join("album")).unwrap();
    share_folder(node, "album", &["cover.txt"]).await;

    let saved = receive::save_shared(&node.state, &root, "folder_1").await.unwrap();

    assert_eq!(saved, root.path().join("album (1)"));
    assert!(names_in(&outside).is_empty());
}

#[tokio::test]
async fn a_folder_already_there_is_left_alone_and_the_new_one_is_numbered() {
    let node = &spawn_mesh(1, None).await[0];
    let (_dir, root) = downloads();
    std::fs::create_dir(root.path().join("album")).unwrap();
    share_folder(node, "album", &["cover.txt"]).await;

    let saved = receive::save_shared(&node.state, &root, "folder_1").await.unwrap();

    assert_eq!(saved, root.path().join("album (1)"));
    assert!(names_in(&root.path().join("album")).is_empty());
}

#[tokio::test]
async fn a_folder_with_a_file_gone_from_the_catalog_is_refused_and_leaves_nothing_behind() {
    let node = &spawn_mesh(1, None).await[0];
    let (_dir, root) = downloads();
    share_folder(node, "album", &["cover.txt", "track.txt"]).await;
    node.state.catalog.listed("child_1").expect("the file is listed");
    assert_eq!(state::unshare(&node.state, "child_1", DEVICE, false).await, ["child_1"]);

    let refused = receive::save_shared(&node.state, &root, "folder_1").await;

    assert_eq!(refused, Err(SaveError::DamagedFolder));
    assert!(names_in(root.path()).is_empty());
}

#[tokio::test]
async fn a_file_nobody_shared_cannot_be_saved() {
    let state = NodeState::for_tests(None);
    let (_dir, root) = downloads();

    assert_eq!(receive::save_shared(&state, &root, "file_unknown").await, Err(SaveError::NotFound));
}

fn saved_path_of(name: &str) -> (PathBuf, PathBuf) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    runtime.block_on(async {
        let state = NodeState::for_tests(None);
        let (_dir, root) = downloads();
        let (blob, manifest_root) = store_file(&state, "file_1", b"payload").await;
        let saved = root.save_file(name, &Content { blob: &blob, size: 7, manifest_root: &manifest_root }, NO_WAIT).await.unwrap();
        (saved, root.path().to_path_buf())
    })
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

    #[test]
    fn whatever_the_name_the_file_lands_directly_in_the_downloads_folder(name in "(\\PC|[/\\\\.\u{0} ]){0,40}") {
        let (saved, root) = saved_path_of(&name);
        proptest::prop_assert_eq!(saved.parent(), Some(root.as_path()), "{:?} was saved as {:?}", name, saved);
    }
}
