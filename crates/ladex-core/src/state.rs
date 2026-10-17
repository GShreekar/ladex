//! Turns the signed catalog, live availability, peers and chat into what local tabs see, and applies changes from the mesh.

use std::collections::HashMap;

use crate::catalog::{self, CatalogEntry, EntryKind, Listed, Refusal};
use crate::hlc::{ClockError, Stamp};
use crate::store::Origin;
use crate::types::*;
use crate::validate;
use crate::websocket;
use crate::NodeState;

const TOMBSTONE_TTL_MS: u64 = 60 * 60 * 1000;
pub const MAX_CATALOG_ENTRIES: usize = 5000;
const MAX_PEERS: usize = 2000;

/// Removes departed-peer tombstones older than `TOMBSTONE_TTL_MS`.
pub fn prune_peer_tombstones(local: &mut HashMap<SessionId, PeerInfo>, now_ms: u64) {
    local.retain(|_, p| !p.left || now_ms.saturating_sub(p.version.wall) < TOMBSTONE_TTL_MS);
}

/// Merges `incoming` messages into `local`, keeping the first copy of each id.
pub fn merge_messages(local: &mut Vec<TextMessage>, incoming: Vec<TextMessage>) {
    let mut index: HashMap<String, usize> = local.iter().enumerate().map(|(i, m)| (m.id.clone(), i)).collect();

    for msg in incoming {
        if !index.contains_key(&msg.id) {
            index.insert(msg.id.clone(), local.len());
            local.push(msg);
        }
    }

    local.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
}

/// Merges `incoming` peers into the browser-visible `local_peers`; the greater `version` wins.
pub fn merge_peers(local: &mut HashMap<SessionId, PeerInfo>, incoming: Vec<PeerInfo>) {
    for peer in incoming {
        match local.get(&peer.session_id) {
            Some(existing) if existing.version >= peer.version => {}
            Some(_) => {
                local.insert(peer.session_id.clone(), peer);
            }
            None if local.len() >= MAX_PEERS => {}
            None => {
                local.insert(peer.session_id.clone(), peer);
            }
        }
    }
}

/// Keeps what passes validation and the clock check; returns the first clock problem seen.
fn accept_from_mesh<T>(
    state: &NodeState,
    incoming: Vec<T>,
    clean: impl Fn(T) -> Option<T>,
    version_of: impl Fn(&T) -> &Stamp,
) -> (Vec<T>, Option<ClockError>) {
    let mut accepted = Vec::with_capacity(incoming.len());
    let mut clock_problem = None;
    for item in incoming {
        let Some(item) = clean(item) else { continue };
        match state.clock.observe(version_of(&item)) {
            Ok(()) => accepted.push(item),
            Err(e) => clock_problem = clock_problem.or(Some(e)),
        }
    }
    (accepted, clock_problem)
}

/// Tell the user a peer's clock is badly off, at most once every 5 minutes.
pub async fn report_clock_problem(state: &NodeState, from: &str, problem: &impl std::fmt::Display) {
    let due = {
        let mut last = state.clock_alert_at.lock().unwrap();
        let due = last.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(300));
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    let message = format!(
        "Clock problem with {from}: {problem}. Set the date and time correctly on both devices; \
         updates stamped more than an hour ahead of a device's clock are ignored."
    );
    tracing::warn!("{message}");
    if due {
        websocket::broadcast(state, ServerMessage::Error { message }).await;
    }
}

/// The size of a shared file, if this node knows of it.
pub fn known_size(state: &NodeState, id: &str) -> Option<u64> {
    state.catalog.listed(id).map(|listed| listed.entry.size)
}

/// The nodes that have the whole file right now, this one included.
pub fn hosts_of(state: &NodeState, id: &str) -> Vec<NodeId> {
    let mut hosts = state.availability.hosts(id);
    if state.store.contains_complete(id) && !hosts.contains(&state.node_id) {
        hosts.push(state.node_id.clone());
        hosts.sort();
    }
    hosts
}

// A node holds a folder only when it has the listing and every file in it.
fn view_of(state: &NodeState, listed: &Listed, children: &[&str]) -> FileView {
    let mut view = FileView::new(listed, hosts_of(state, &listed.entry.file_id));
    if listed.entry.is_folder() {
        for child in children {
            let child_hosts = hosts_of(state, child);
            view.available &= !child_hosts.is_empty();
            view.hosts.retain(|node| child_hosts.contains(node));
        }
    }
    view
}

/// Every shared file as a browser tab shows it.
pub fn file_views(state: &NodeState) -> Vec<FileView> {
    let listing = state.catalog.listing();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for listed in &listing {
        if let Some(parent) = listed.entry.parent() {
            children.entry(parent).or_default().push(&listed.entry.file_id);
        }
    }
    listing.iter().map(|listed| view_of(state, listed, children.get(listed.entry.file_id.as_str()).map_or(&[], Vec::as_slice))).collect()
}

pub fn file_view(state: &NodeState, id: &str) -> Option<FileView> {
    let listed = state.catalog.listed(id)?;
    if !listed.entry.is_folder() {
        return Some(view_of(state, &listed, &[]));
    }
    let listing = state.catalog.listing();
    let children: Vec<&str> = listing.iter().filter(|l| l.entry.parent() == Some(id)).map(|l| l.entry.file_id.as_str()).collect();
    Some(view_of(state, &listed, &children))
}

pub async fn broadcast_catalog(state: &NodeState) {
    state.catalog_changes.send_modify(|count| *count += 1);
    websocket::broadcast(state, ServerMessage::FileListUpdate { files: file_views(state) }).await;
}

/// Acts on the files whose listing just changed: deletes copies of those taken down and tells local tabs.
pub async fn catalog_changed(state: &NodeState, file_ids: &[String]) {
    let taken_down: Vec<&String> = file_ids.iter().filter(|id| state.catalog.was_removed(id)).collect();
    for id in &taken_down {
        forget_file(state, id).await;
    }
    broadcast_catalog(state).await;
    for id in taken_down {
        websocket::broadcast(state, ServerMessage::FileRemoved { file_id: id.clone() }).await;
    }
    crate::transfer::sources_changed(state);
}

/// Delete our copy of a file that was unshared, and stop any transfer of it.
pub async fn forget_file(state: &NodeState, id: &str) {
    crate::transfer::cancel(state, id);
    if state.store.get(id).is_some() {
        tracing::info!("Store: removing {id} (unshared)");
        state.store.remove(id);
        crate::availability::announce_lost(state, &[id.to_string()]).await;
    }
}

/// Lists a file this node already holds, tells the mesh it has it, and returns how tabs will see it.
pub async fn share_file(state: &NodeState, entry: CatalogEntry) -> Result<FileView, Refusal> {
    let id = entry.file_id.clone();
    // Saved before it is listed, so a restart never finds a shared file that is not marked complete.
    if let Some(blob) = state.store.get(&id) {
        blob.persist().await;
    }
    catalog::sync::publish(state, entry, Vec::new()).await?;
    crate::availability::announce_complete(state, std::slice::from_ref(&id)).await;
    Ok(file_view(state, &id).expect("a file just published is listed"))
}

/// Takes down a file and, for a folder, what is in it; returns the ids that were taken down.
/// Only the device that shared it may, or the node's own machine for files shared through this node.
pub async fn unshare(state: &NodeState, file_id: &str, device: &str, is_host: bool) -> Vec<String> {
    let Some(listed) = state.catalog.listed(file_id) else { return Vec::new() };
    let is_ours = listed.publisher == state.node_id;
    let may_unshare = listed.entry.uploader_id == device || (is_host && is_ours);
    if !is_ours || !may_unshare {
        return Vec::new();
    }
    let ids: Vec<String> = state
        .catalog
        .listing()
        .into_iter()
        .filter(|l| l.entry.file_id == file_id || l.entry.parent() == Some(file_id))
        .map(|l| l.entry.file_id)
        .collect();
    catalog::sync::unpublish_all(state, &ids).await
}

/// Evicts least-recently-used cached files so `size` more bytes fit, and tells the mesh they are gone.
pub async fn make_room(state: &NodeState, size: u64) {
    let evicted = state.store.evict_to_fit(size);
    if !evicted.is_empty() {
        crate::availability::announce_lost(state, &evicted).await;
        broadcast_catalog(state).await;
    }
}

const ABANDONED_PARTIAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const UNPUBLISHED_FOLDER_FILES_MS: u64 = 60 * 60 * 1000;

/// Deletes abandoned partial files and uploads for folders that were never published, and evicts cached files over the quota.
pub async fn collect_garbage(state: &NodeState) {
    crate::guest::collect_garbage(state);
    collect_garbage_with(state, ABANDONED_PARTIAL, UNPUBLISHED_FOLDER_FILES_MS).await;
}

async fn collect_garbage_with(state: &NodeState, partial_ttl: std::time::Duration, orphan_ttl_ms: u64) {
    state.store.save_access_times().await;
    make_room(state, 0).await;
    crate::push::collect_garbage(state);
    for blob in state.store.all() {
        let abandoned =
            !blob.is_complete() && !blob.is_being_written() && !state.transfers.is_downloading(blob.id()) && blob.idle_for() >= partial_ttl;
        if abandoned {
            tracing::info!("Store: removing {} (partly received, untouched for a long time)", blob.id());
            state.store.remove(blob.id());
        }
    }

    let now = crate::hlc::wall_clock_ms();
    let listing = state.catalog.listing();
    let orphans = listing
        .iter()
        .filter(|l| l.publisher == state.node_id && now.saturating_sub(l.entry.created_at_ms) >= orphan_ttl_ms)
        .filter(|l| l.entry.parent().is_some_and(|parent| !listing.iter().any(|other| other.entry.file_id == parent)));
    for orphan in orphans {
        // Unpublishing also deletes the copy and tells the tabs.
        let _ = catalog::sync::unpublish(state, &orphan.entry.file_id).await;
    }
}

/// Deletes the copies of files that were taken down while this node was not listening.
pub async fn reconcile_store(state: &NodeState) {
    for blob in state.store.all() {
        if state.catalog.was_removed(blob.id()) {
            forget_file(state, blob.id()).await;
        }
    }
}

/// Lists, in the signed catalog, the files an earlier version of this node shared; returns how many.
pub async fn import_legacy_entries(state: &NodeState) -> usize {
    let mut imported = 0;
    for blob in state.store.all() {
        let Some(old) = blob.take_legacy_entry() else { continue };
        blob.persist().await;
        let (Origin::Published, Some(root)) = (blob.origin(), blob.manifest_root()) else { continue };
        if old.deleted || state.catalog.contains(&old.id) || state.catalog.was_removed(&old.id) {
            continue;
        }
        let kind = match old.is_folder {
            true => EntryKind::Folder { bytes: old.folder_bytes, files: old.folder_files },
            false => EntryKind::File { parent: old.parent },
        };
        let entry = CatalogEntry {
            file_id: old.id,
            name: validate::sanitize_file_name(&old.name),
            size: old.size,
            mime_type: validate::sanitize_mime(&old.mime_type),
            created_at_ms: old.created_at,
            manifest_root: root,
            uploader_id: old.uploader_id,
            kind,
        };
        match state.catalog.publish(&state.identity, &state.clock, entry, Vec::new()) {
            Ok(_) => imported += 1,
            Err(refusal) => tracing::warn!("Catalog: could not list {} from an earlier version because {refusal}", blob.id()),
        }
    }
    imported
}

/// Apply an incoming peer sync and fan out the updated peer list to local tabs.
pub async fn apply_peer_sync(state: &NodeState, incoming: Vec<PeerInfo>, from: &str) {
    let (accepted, clock_problem) = accept_from_mesh(state, incoming, validate::incoming_peer, |p| &p.version);
    {
        let mut peers = state.local_peers.write().await;
        merge_peers(&mut peers, accepted.clone());
    }
    if let Some(problem) = clock_problem {
        report_clock_problem(state, from, &problem).await;
    }
    if !accepted.is_empty() {
        websocket::broadcast(state, ServerMessage::PeerSync { peers: accepted }).await;
    }
}

/// Caps the in-memory chat history, dropping the oldest messages.
const MAX_CHAT_MESSAGES: usize = 500;
pub fn prune_messages(messages: &mut Vec<TextMessage>) {
    if messages.len() > MAX_CHAT_MESSAGES {
        let excess = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(0..excess);
    }
}

/// Applies a peer's full chat history and sends local tabs only the messages that are new.
pub async fn apply_chat_sync(state: &NodeState, incoming: Vec<TextMessage>) {
    let incoming: Vec<TextMessage> = incoming.into_iter().filter_map(validate::incoming_message).collect();
    let new_messages: Vec<TextMessage> = {
        let mut messages = state.messages.write().await;
        let existing_ids: std::collections::HashSet<String> = messages.iter().map(|m| m.id.clone()).collect();
        let new_ones: Vec<TextMessage> = incoming.iter().filter(|m| !existing_ids.contains(&m.id)).cloned().collect();
        merge_messages(&mut messages, incoming);
        prune_messages(&mut messages);
        new_ones
    };
    for message in new_messages {
        websocket::broadcast(state, ServerMessage::TextMessage { message }).await;
    }
}

/// Apply a single incoming chat message and fan it out to local tabs.
pub async fn apply_chat_message(state: &NodeState, message: TextMessage) {
    let Some(message) = validate::incoming_message(message) else { return };
    let already_known = {
        let messages = state.messages.read().await;
        messages.iter().any(|m| m.id == message.id)
    };
    if !already_known {
        {
            let mut messages = state.messages.write().await;
            messages.push(message.clone());
            messages.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
            prune_messages(&mut messages);
        }
        websocket::broadcast(state, ServerMessage::TextMessage { message }).await;
    }
}

use crate::mesh::{MeshMessage, MeshPeers};

/// Broadcast a single chat message to all connected mesh peers.
pub async fn push_message_to_mesh(mesh_peers: &MeshPeers, message: TextMessage) {
    let peers = mesh_peers.read().await;
    let msg = MeshMessage::ChatMessage { message };
    for handle in peers.values() {
        let _ = handle.sender.send(msg.clone());
    }
}

/// Broadcast updated peer info (e.g. on Join) to all connected mesh peers.
pub async fn push_peer_to_mesh(mesh_peers: &MeshPeers, peer: PeerInfo) {
    let peers = mesh_peers.read().await;
    let msg = MeshMessage::PeerSync { peers: vec![peer] };
    for handle in peers.values() {
        let _ = handle.sender.send(msg.clone());
    }
}

/// Broadcasts a peer departure; `version` must be a fresh stamp so it outranks the live record.
pub async fn push_peer_left_to_mesh(mesh_peers: &MeshPeers, session_id: SessionId, version: Stamp) {
    let departed = PeerInfo {
        session_id,
        connected_at: chrono::Utc::now(),
        user_agent: None,
        hosting_node_id: None,
        node_rtt_ms: None,
        left: true,
        left_at: Some(chrono::Utc::now()),
        hosting_node_name: None,
        nickname: None,
        version,
    };
    push_peer_to_mesh(mesh_peers, departed).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(wall: u64, counter: u32, node: &str) -> Stamp {
        Stamp { wall, counter, node: node.into() }
    }

    fn peer(id: &str, version: Stamp, left: bool) -> PeerInfo {
        PeerInfo {
            session_id: id.into(),
            connected_at: chrono::Utc::now(),
            user_agent: None,
            hosting_node_id: if left { None } else { Some("node_a".into()) },
            node_rtt_ms: None,
            left,
            left_at: None,
            hosting_node_name: None,
            nickname: None,
            version,
        }
    }

    #[test]
    fn a_departure_beats_the_join_it_follows_and_a_stale_join_cannot_undo_it() {
        let mut local: HashMap<SessionId, PeerInfo> = HashMap::new();
        merge_peers(&mut local, vec![peer("p", stamp(10, 0, "a"), false)]);
        merge_peers(&mut local, vec![peer("p", stamp(11, 0, "a"), true)]);
        assert!(local["p"].left);
        merge_peers(&mut local, vec![peer("p", stamp(10, 5, "a"), false)]);
        assert!(local["p"].left);
    }

    #[test]
    fn a_local_rtt_survives_an_echo_of_the_same_version() {
        let mut local: HashMap<SessionId, PeerInfo> = HashMap::new();
        let mut known = peer("p", stamp(10, 0, "a"), false);
        known.node_rtt_ms = Some(7);
        local.insert("p".into(), known);
        merge_peers(&mut local, vec![peer("p", stamp(10, 0, "a"), false)]);
        assert_eq!(local["p"].node_rtt_ms, Some(7));
    }

    #[test]
    fn peer_tombstones_are_pruned_by_age() {
        let now = 10 * TOMBSTONE_TTL_MS;
        let mut local: HashMap<SessionId, PeerInfo> = HashMap::new();
        local.insert("old".into(), peer("old", stamp(now - TOMBSTONE_TTL_MS - 1, 0, "a"), true));
        local.insert("new".into(), peer("new", stamp(now, 0, "a"), true));
        local.insert("live".into(), peer("live", stamp(1, 0, "a"), false));
        prune_peer_tombstones(&mut local, now);
        assert!(!local.contains_key("old") && local.contains_key("new") && local.contains_key("live"));
    }

    #[test]
    fn duplicate_chat_messages_are_kept_once_and_in_order() {
        let message = |id: &str, at: u64| TextMessage {
            id: id.into(),
            content: "hi".into(),
            sender_id: "peer_a".into(),
            sender_name: None,
            timestamp: chrono::Utc::now(),
            created_at: at,
        };
        let mut local = vec![message("m2", 20)];
        merge_messages(&mut local, vec![message("m1", 10), message("m2", 99), message("m3", 30)]);
        let order: Vec<_> = local.iter().map(|m| (m.id.as_str(), m.created_at)).collect();
        assert_eq!(order, [("m1", 10), ("m2", 20), ("m3", 30)]);
    }

    fn chat_message(id: &str, created_at: u64) -> TextMessage {
        TextMessage {
            id: id.into(),
            content: "hi".into(),
            sender_id: "peer_a".into(),
            sender_name: None,
            timestamp: chrono::Utc::now(),
            created_at,
        }
    }

    #[test]
    fn a_full_peer_list_ignores_new_peers_but_still_updates_known_ones() {
        let mut local: HashMap<SessionId, PeerInfo> = HashMap::new();
        for i in 0..MAX_PEERS {
            let id = format!("p{i}");
            local.insert(id.clone(), peer(&id, stamp(10, 0, "a"), false));
        }
        merge_peers(&mut local, vec![peer("newcomer", stamp(10, 0, "a"), false)]);
        assert!(!local.contains_key("newcomer"));
        merge_peers(&mut local, vec![peer("p0", stamp(20, 0, "a"), true)]);
        assert!(local["p0"].left);
        assert_eq!(local.len(), MAX_PEERS);
    }

    #[test]
    fn merging_chat_into_nothing_keeps_it_sorted() {
        let mut local = Vec::new();
        merge_messages(&mut local, vec![chat_message("b", 20), chat_message("a", 10)]);
        let order: Vec<_> = local.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, ["a", "b"]);
    }

    #[test]
    fn messages_created_at_the_same_moment_are_ordered_by_id_on_every_node() {
        let mut first = vec![chat_message("b", 10)];
        let mut second = vec![chat_message("a", 10)];
        merge_messages(&mut first, vec![chat_message("a", 10)]);
        merge_messages(&mut second, vec![chat_message("b", 10)]);
        let ids = |messages: &[TextMessage]| messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&first), ids(&second));
    }

    #[test]
    fn pruning_chat_drops_the_oldest_messages_first() {
        let mut messages: Vec<_> = (0..MAX_CHAT_MESSAGES as u64 + 3).map(|i| chat_message(&format!("m{i}"), i)).collect();
        prune_messages(&mut messages);
        assert_eq!(messages.len(), MAX_CHAT_MESSAGES);
        assert_eq!(messages[0].id, "m3");
    }

    #[test]
    fn pruning_a_short_chat_keeps_everything() {
        let mut messages = vec![chat_message("a", 1), chat_message("b", 2)];
        prune_messages(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn a_peer_sync_without_a_stamp_is_ignored() {
        let state = NodeState::for_tests(None);
        apply_peer_sync(
            &state,
            vec![peer("p1", Stamp::default(), false), peer("p2", stamp(crate::hlc::wall_clock_ms(), 0, "a"), false)],
            "node_a",
        )
        .await;
        let peers = state.local_peers.read().await;
        assert!(peers.contains_key("p2") && !peers.contains_key("p1"));
    }

    #[tokio::test]
    async fn a_chat_message_from_another_node_cannot_be_dated_in_the_future() {
        let state = NodeState::for_tests(None);
        let message = TextMessage {
            id: "msg_1".into(),
            content: "hi\u{0}".into(),
            sender_id: "peer_a".into(),
            sender_name: None,
            timestamp: chrono::Utc::now() + chrono::Duration::days(365),
            created_at: crate::hlc::wall_clock_ms() + 365 * 24 * 3_600_000,
        };
        apply_chat_message(&state, message).await;
        let messages = state.messages.read().await;
        assert_eq!(messages[0].content, "hi");
        assert!(messages[0].created_at <= crate::hlc::wall_clock_ms());
        assert!(messages[0].timestamp <= chrono::Utc::now());
    }

    #[tokio::test]
    async fn abandoned_partial_files_are_removed_but_complete_and_active_ones_are_not() {
        let state = NodeState::for_tests(None);
        let partial = state.store.create("file_partial", 100).unwrap();
        let writing = state.store.create("file_writing", 100).unwrap();
        let _guard = writing.try_write_lock().unwrap();
        let complete = state.store.create("file_complete", 10).unwrap();
        complete.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
        complete.seal().await.unwrap();

        collect_garbage_with(&state, std::time::Duration::from_secs(3600), UNPUBLISHED_FOLDER_FILES_MS).await;
        assert!(state.store.get("file_partial").is_some(), "recently touched files stay");

        collect_garbage_with(&state, std::time::Duration::ZERO, UNPUBLISHED_FOLDER_FILES_MS).await;
        assert!(state.store.get("file_partial").is_none());
        assert!(partial.is_removed());
        assert!(state.store.get("file_writing").is_some(), "a file being uploaded right now is never collected");
        assert!(state.store.get("file_complete").is_some());
    }

    fn entry(id: &str, uploader: &str, kind: EntryKind) -> CatalogEntry {
        CatalogEntry {
            file_id: id.into(),
            name: format!("{id}.txt"),
            size: 10,
            mime_type: "text/plain".into(),
            created_at_ms: crate::hlc::wall_clock_ms(),
            manifest_root: "ab".repeat(32),
            uploader_id: uploader.into(),
            kind,
        }
    }

    fn plain(id: &str, uploader: &str) -> CatalogEntry {
        entry(id, uploader, EntryKind::File { parent: None })
    }

    async fn hold(state: &NodeState, id: &str) {
        let blob = state.store.create(id, 10).unwrap();
        blob.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
        blob.seal().await.unwrap();
    }

    #[tokio::test]
    async fn a_shared_file_is_listed_with_this_node_as_its_host() {
        let state = NodeState::for_tests_signing(None);
        hold(&state, "f1").await;
        let view = share_file(&state, plain("f1", "device_1")).await.unwrap();
        assert_eq!((view.uploader_node, view.hosts), (state.node_id.clone(), vec![state.node_id.clone()]));
    }

    #[tokio::test]
    async fn a_shared_file_is_still_complete_after_the_store_is_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let mut state = NodeState::for_tests_signing(None);
        state.store = std::sync::Arc::new(crate::store::Store::open(&root, 1 << 30).unwrap());
        hold(&state, "f1").await;
        share_file(&state, plain("f1", "device_1")).await.unwrap();
        drop(state);

        let reopened = crate::store::Store::open(&root, 1 << 30).unwrap();
        assert!(reopened.contains_complete("f1"));
    }

    #[tokio::test]
    async fn hosts_are_the_nodes_that_said_they_have_the_file_and_leave_with_them() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, plain("f1", "device_1")).await.unwrap();
        state.availability.set_complete("node_b", &["f1".to_string()]);
        assert_eq!(file_view(&state, "f1").unwrap().hosts, vec!["node_b".to_string()]);

        state.availability.drop_node("node_b");
        assert!(!file_view(&state, "f1").unwrap().is_available());
    }

    #[tokio::test]
    async fn a_node_holds_a_folder_only_with_its_listing_and_every_file_in_it() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, entry("folder", "d", EntryKind::Folder { bytes: 20, files: 2 })).await.unwrap();
        share_file(&state, entry("one", "d", EntryKind::File { parent: Some("folder".into()) })).await.unwrap();
        share_file(&state, entry("two", "d", EntryKind::File { parent: Some("folder".into()) })).await.unwrap();
        let folder = || file_view(&state, "folder").unwrap();
        let listed_folder = || file_views(&state).into_iter().find(|v| v.id == "folder").unwrap();

        state.availability.set_complete("node_b", &["folder".to_string(), "one".to_string()]);
        assert!(folder().hosts.is_empty() && !folder().available, "half a folder is not a copy of it");

        state.availability.set_complete("node_c", &["two".to_string()]);
        assert!(folder().hosts.is_empty() && folder().available, "the parts can still be fetched from several nodes");

        state.availability.set_complete("node_b", &["two".to_string()]);
        assert_eq!(folder().hosts, ["node_b"]);
        assert_eq!(listed_folder(), folder());
    }

    #[tokio::test]
    async fn unsharing_a_folder_tells_the_tabs_once() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, entry("folder", "d", EntryKind::Folder { bytes: 20, files: 2 })).await.unwrap();
        share_file(&state, entry("one", "d", EntryKind::File { parent: Some("folder".into()) })).await.unwrap();
        share_file(&state, entry("two", "d", EntryKind::File { parent: Some("folder".into()) })).await.unwrap();
        let before = *state.catalog_changes.borrow();

        assert_eq!(unshare(&state, "folder", "d", false).await.len(), 3);
        assert_eq!(*state.catalog_changes.borrow(), before + 1);
    }

    #[tokio::test]
    async fn the_sharing_device_can_unshare_and_its_folder_contents_go_with_a_folder() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, entry("folder", "device_1", EntryKind::Folder { bytes: 20, files: 2 })).await.unwrap();
        share_file(&state, entry("inside", "device_1", EntryKind::File { parent: Some("folder".into()) })).await.unwrap();
        share_file(&state, plain("elsewhere", "device_1")).await.unwrap();

        let mut taken_down = unshare(&state, "folder", "device_1", false).await;
        taken_down.sort();
        assert_eq!(taken_down, ["folder", "inside"]);
        let left: Vec<String> = file_views(&state).into_iter().map(|v| v.id).collect();
        assert_eq!(left, ["elsewhere"]);
    }

    #[tokio::test]
    async fn another_device_cannot_unshare_but_the_node_s_own_machine_can() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, plain("f1", "device_1")).await.unwrap();
        assert!(unshare(&state, "f1", "device_2", false).await.is_empty());
        assert_eq!(unshare(&state, "f1", "device_2", true).await, ["f1"]);
    }

    #[tokio::test]
    async fn unsharing_deletes_this_node_s_copy_and_tells_the_mesh() {
        let state = NodeState::for_tests_signing(None);
        hold(&state, "f1").await;
        share_file(&state, plain("f1", "device_1")).await.unwrap();

        unshare(&state, "f1", "device_1", false).await;
        assert!(state.store.get("f1").is_none());
    }

    #[tokio::test]
    async fn a_node_cannot_unshare_a_file_another_node_published() {
        let (a, b) = (NodeState::for_tests_signing(None), NodeState::for_tests_signing(None));
        share_file(&a, plain("f1", "device_1")).await.unwrap();
        crate::catalog::sync::on_ops(&b, &"node_a".to_string(), &a.catalog.ops_missing_from(&Default::default())).await;

        assert!(unshare(&b, "f1", "device_1", true).await.is_empty());
        assert!(b.catalog.contains("f1"));
    }

    #[tokio::test]
    async fn a_file_taken_down_while_this_node_was_away_is_deleted_on_reconcile() {
        let state = NodeState::for_tests_signing(None);
        hold(&state, "f1").await;
        hold(&state, "f2").await;
        share_file(&state, plain("f1", "device_1")).await.unwrap();
        state.catalog.unpublish(&state.identity, &state.clock, "f1").unwrap();

        reconcile_store(&state).await;
        assert!(state.store.get("f1").is_none());
        assert!(state.store.get("f2").is_some(), "a copy with no catalog entry yet is kept");
    }

    #[tokio::test]
    async fn an_entry_saved_by_an_earlier_version_is_listed_in_the_signed_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file_old.data"), vec![7u8; 10]).unwrap();
        std::fs::write(root.join("file_old.hashes"), [0u8; 32]).unwrap();
        std::fs::write(
            root.join("file_old.json"),
            r#"{"id":"file_old","size":10,"have":"01","hash_known":"01","expected_root":null,"sealed":"old","entry":
               {"id":"file_old","name":"notes.txt","size":10,"mime_type":"text/plain","uploader_id":"device_1",
                "uploader_node":"node_old","holders":{},"uploaded_at":"2024-01-01T00:00:00Z","created_at":1700000000000,
                "version":{"wall":1,"counter":0,"node":"node_old"},"deleted":false,"deleted_at":0,"manifest_root":"old"}}"#,
        )
        .unwrap();
        let mut state = NodeState::for_tests_signing(None);
        state.store = std::sync::Arc::new(crate::store::Store::open(&root, 1 << 30).unwrap());

        assert_eq!(import_legacy_entries(&state).await, 1);
        let view = file_view(&state, "file_old").unwrap();
        assert_eq!((view.name.as_str(), view.uploader_id.as_str(), view.created_at), ("notes.txt", "device_1", 1_700_000_000_000));
        assert_eq!(Some(view.manifest_root), state.store.get("file_old").unwrap().manifest_root());
        assert_eq!(import_legacy_entries(&state).await, 0, "importing again lists nothing twice");
    }

    #[tokio::test]
    async fn making_room_evicts_cached_files_and_keeps_the_nodes_own_shares() {
        let mut state = NodeState::for_tests_signing(None);
        state.store = std::sync::Arc::new(crate::store::Store::open(&tempfile::tempdir().unwrap().keep(), 100).unwrap());
        for (id, origin) in [("file_cached", Origin::Cached), ("file_mine", Origin::Published)] {
            let blob = state.store.create_with(id, 50, origin).unwrap();
            blob.write_chunk_hashing(0, vec![1; 50]).await.unwrap();
            blob.seal().await.unwrap();
        }

        make_room(&state, 50).await;

        assert!(state.store.get("file_cached").is_none());
        assert!(state.store.get("file_mine").is_some(), "a node's own shares are never evicted");
    }

    #[tokio::test]
    async fn files_uploaded_for_a_folder_that_was_never_published_are_taken_down() {
        let state = NodeState::for_tests_signing(None);
        share_file(&state, entry("orphan", "d", EntryKind::File { parent: Some("folder_never".into()) })).await.unwrap();
        share_file(&state, entry("folder_real", "d", EntryKind::Folder { bytes: 1, files: 1 })).await.unwrap();
        share_file(&state, entry("inside", "d", EntryKind::File { parent: Some("folder_real".into()) })).await.unwrap();
        share_file(&state, plain("loose", "d")).await.unwrap();

        collect_garbage_with(&state, std::time::Duration::from_secs(3600), 0).await;

        let mut left: Vec<String> = file_views(&state).into_iter().map(|v| v.id).collect();
        left.sort();
        assert_eq!(left, ["folder_real", "inside", "loose"]);
    }

    #[tokio::test]
    async fn an_op_stamped_far_in_the_future_is_dropped_and_reported() {
        let state = NodeState::for_tests_signing(None);
        let other = crate::identity::Identity::generate();
        let future =
            Stamp { wall: crate::hlc::wall_clock_ms() + 2 * crate::hlc::MAX_DRIFT_MS, counter: 0, node: other.node_id().to_string() };
        let add = crate::catalog::Add::sign(&other, future, 1, vec![], plain("f1", "d"));

        crate::catalog::sync::on_ops(&state, &"node_fast".to_string(), &[crate::catalog::Op::Add(add)]).await;

        assert!(!state.catalog.contains("f1"));
        assert!(state.clock_alert_at.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn a_node_with_its_clock_a_year_ahead_cannot_override_the_others() {
        const ONE_YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
        let state = NodeState::for_tests_signing(None);
        let fast = crate::identity::Identity::generate();
        let year_ahead =
            |seq: u64| Stamp { wall: crate::hlc::wall_clock_ms() + ONE_YEAR_MS, counter: seq as u32, node: fast.node_id().to_string() };
        let shared = crate::catalog::sync::publish(&state, plain("f1", "d"), vec![fast.public_key()]).await.unwrap();
        let takedown = crate::catalog::Remove::sign(&fast, year_ahead(1), 1, "f1".into(), vec![shared.stamp.clone()]);
        let replacement = crate::catalog::Add::sign(&fast, year_ahead(2), 2, vec![], plain("f2", "d"));

        let ops = [crate::catalog::Op::Remove(takedown), crate::catalog::Op::Add(replacement)];
        crate::catalog::sync::on_ops(&state, &"node_fast".to_string(), &ops).await;

        assert!(state.catalog.contains("f1"));
        assert!(!state.catalog.contains("f2"));
        assert_eq!(state.catalog.latest_stamp(fast.node_id()), None);
    }
}
