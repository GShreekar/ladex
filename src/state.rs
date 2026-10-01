// ============================================================================
// LADEX — Phase 4: Distributed State Merge
//
// Merges the three distributed state stores: file catalog, peer list, and chat
// messages.
//
//   • Files and peers carry a hybrid-logical-clock `version` (hlc.rs).  On a
//     conflict the entry with the greater version wins; equal versions keep
//     what we have.  Wall-clock time is never compared, so a node with a fast
//     or slow clock can't win conflicts it shouldn't.
//
//   • Everything that comes from another node is validated first (validate.rs)
//     and its version is checked against our clock; entries that fail are
//     dropped, and a clock that is far off is reported to the user.
//
//   • File and peer tombstones: instead of deleting outright, set
//     `deleted`/`left` and propagate.  They are pruned after
//     `TOMBSTONE_TTL_MS`, which must outlast any reasonable disconnection or a
//     stale live entry from a node that was away would bring the file back.
//
//   • After each merge, the updated state is fanned out to all connected
//     local browser tabs via `websocket::broadcast`.
//
//   • Full-state sync is sent immediately after the mesh handshake
//     (see `mesh::post_handshake_sync`).  Incremental deltas are sent
//     per-event (see `push_*` functions below).
// ============================================================================

use std::collections::HashMap;

use crate::hlc::{ClockError, Stamp};
use crate::types::*;
use crate::validate;
use crate::NodeState;
use crate::websocket;

const TOMBSTONE_TTL_MS: u64 = 60 * 60 * 1000;
pub const MAX_CATALOG_ENTRIES: usize = 5000;
const MAX_PEERS: usize = 2000;

// ---------------------------------------------------------------------------
// Merge: file catalog
// ---------------------------------------------------------------------------

/// Merge `incoming` files into `local`; the greater `version` wins.  New ids
/// are skipped once `capacity` is reached, but updates to known ids still apply.
/// Returns how many new entries were skipped for lack of room.
pub fn merge_files(local: &mut HashMap<String, FileMetadata>, incoming: Vec<FileMetadata>, capacity: usize) -> usize {
    let mut skipped = 0;
    for entry in incoming {
        match local.get(&entry.id) {
            Some(existing) if existing.version >= entry.version => {}
            Some(_) => {
                local.insert(entry.id.clone(), entry);
            }
            None if local.len() >= capacity => skipped += 1,
            None => {
                local.insert(entry.id.clone(), entry);
            }
        }
    }
    skipped
}

/// Remove tombstones older than `TOMBSTONE_TTL_MS`.  A stamp from the future
/// (a peer with a fast clock, within the drift limit) is simply not old yet.
pub fn prune_tombstones(local: &mut HashMap<String, FileMetadata>, now_ms: u64) {
    local.retain(|_, f| !f.deleted || now_ms.saturating_sub(f.version.wall) < TOMBSTONE_TTL_MS);
}

/// Same as `prune_tombstones`, for departed peers. Without it a correctly
/// propagated departure would sit in `local_peers` forever.
pub fn prune_peer_tombstones(local: &mut HashMap<SessionId, PeerInfo>, now_ms: u64) {
    local.retain(|_, p| !p.left || now_ms.saturating_sub(p.version.wall) < TOMBSTONE_TTL_MS);
}

/// Make room for new entries by dropping the oldest tombstones first.
pub fn evict_old_tombstones(local: &mut HashMap<String, FileMetadata>, capacity: usize) {
    if local.len() < capacity {
        return;
    }
    let mut tombstones: Vec<(Stamp, String)> =
        local.values().filter(|f| f.deleted).map(|f| (f.version.clone(), f.id.clone())).collect();
    tombstones.sort();
    let excess = local.len() + 1 - capacity;
    for (_, id) in tombstones.into_iter().take(excess) {
        local.remove(&id);
    }
}

// ---------------------------------------------------------------------------
// Merge: chat messages
// ---------------------------------------------------------------------------

/// Merge `incoming` messages into `local`, deduplicating by `message.id`.
/// Messages are immutable, so the first copy we saw is kept.
pub fn merge_messages(local: &mut Vec<TextMessage>, incoming: Vec<TextMessage>) {
    // Build an index for O(1) lookup
    let mut index: HashMap<String, usize> = local
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();

    for msg in incoming {
        if !index.contains_key(&msg.id) {
            index.insert(msg.id.clone(), local.len());
            local.push(msg);
        }
    }

    // Keep messages in chronological order after merge.
    local.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
}

// ---------------------------------------------------------------------------
// Merge: peer list
// ---------------------------------------------------------------------------

/// Merge `incoming` peer infos into `local`; the greater `version` wins.
/// Peers from remote nodes are stored alongside local peers in `local_peers`
/// so the browser tab's peer list is a unified view of the whole mesh.
///
/// Note: this writes to `NodeState::local_peers` (which the browser sees),
/// NOT to `mesh_peers` (which is node handles).  The naming is slightly
/// unfortunate but matches the existing field layout — `local_peers` is the
/// *peer registry visible to the browser*, regardless of where those peers
/// are physically connected.
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

// ---------------------------------------------------------------------------
// High-level apply-and-fanout helpers
// ---------------------------------------------------------------------------

/// Keeps what survives validation and the clock check. Returns the first
/// clock problem seen, if any, so the caller can report it once.
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

/// Apply an incoming catalog sync and fan out the updated list to local tabs.
pub async fn apply_catalog_sync(state: &NodeState, incoming: Vec<FileMetadata>, from: &str) {
    let (accepted, clock_problem) = accept_from_mesh(state, incoming, validate::incoming_file, |f| &f.version);
    let updated: Vec<FileMetadata> = {
        let mut files = state.files.write().await;
        evict_old_tombstones(&mut files, MAX_CATALOG_ENTRIES);
        let skipped = merge_files(&mut files, accepted, MAX_CATALOG_ENTRIES);
        if skipped > 0 {
            tracing::warn!("Catalog full: ignored {skipped} new entries from {from}");
        }
        // Return only non-deleted files for the browser (tombstones are internal)
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    if let Some(problem) = clock_problem {
        report_clock_problem(state, from, &problem).await;
    }
    websocket::broadcast(state, ServerMessage::FileListUpdate { files: updated }).await;
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
    // Phase 6: broadcast incremental PeerSync to browser tabs so client-side
    // host selection can incorporate RTT data from remote nodes.
    if !accepted.is_empty() {
        websocket::broadcast(state, ServerMessage::PeerSync { peers: accepted }).await;
    }
}


/// BUG-13 fix: cap the in-memory chat history so a long-running node
/// doesn't accumulate it forever. Messages are kept sorted ascending by
/// `created_at` everywhere they're inserted, so trimming the front drops
/// the oldest ones. Chat history isn't authoritative state the way the
/// file catalog is — silently aging out old messages is an acceptable
/// tradeoff for bounded memory, same idea as the file tombstone TTL above.
const MAX_CHAT_MESSAGES: usize = 500;
pub fn prune_messages(messages: &mut Vec<TextMessage>) {
    if messages.len() > MAX_CHAT_MESSAGES {
        let excess = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(0..excess);
    }
}

/// Apply an incoming chat sync (a mesh peer's full history, sent once after
/// its handshake — see mesh::post_handshake_sync) and fan out only the
/// messages we didn't already have to local tabs.
///
/// BUG-13 fix: this used to broadcast `ServerMessage::MessageHistory` with
/// the *entire*, ever-growing merged history on every single ChatSync —
/// meaning every new mesh connection cost every local browser tab an
/// O(history size) payload, even though a tab typically already has nearly
/// all of it (it got its own full snapshot on `join`; see
/// websocket.rs). The delta is broadcast the same way a freshly-sent local
/// message already was — one `TextMessage` event per new message, which
/// the client appends incrementally — instead of a snapshot that replaces
/// the client's whole array (see app.js handleMessageHistory).
pub async fn apply_chat_sync(state: &NodeState, incoming: Vec<TextMessage>) {
    let incoming: Vec<TextMessage> = incoming.into_iter().filter_map(validate::incoming_message).collect();
    let new_messages: Vec<TextMessage> = {
        let mut messages = state.messages.write().await;
        let existing_ids: std::collections::HashSet<String> =
            messages.iter().map(|m| m.id.clone()).collect();
        let new_ones: Vec<TextMessage> = incoming.iter()
            .filter(|m| !existing_ids.contains(&m.id))
            .cloned()
            .collect();
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
            prune_messages(&mut messages); // BUG-13 fix
        }
        websocket::broadcast(state, ServerMessage::TextMessage { message }).await;
    }
}

// ---------------------------------------------------------------------------
// Push helpers — propagate local changes to all mesh peers
// ---------------------------------------------------------------------------

use crate::mesh::{MeshMessage, MeshPeers};

/// Broadcast a single file entry to all connected mesh peers.
pub async fn push_file_to_mesh(mesh_peers: &MeshPeers, file: FileMetadata) {
    let peers = mesh_peers.read().await;
    let msg = MeshMessage::CatalogSync {
        files: vec![file],
    };
    for handle in peers.values() {
        let _ = handle.sender.send(msg.clone());
    }
}

/// Broadcast several file entries to all connected mesh peers in one message.
pub async fn push_files_to_mesh(mesh_peers: &MeshPeers, files: Vec<FileMetadata>) {
    if files.is_empty() {
        return;
    }
    let peers = mesh_peers.read().await;
    let msg = MeshMessage::CatalogSync { files };
    for handle in peers.values() {
        let _ = handle.sender.send(msg.clone());
    }
}

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
    let msg = MeshMessage::PeerSync {
        peers: vec![peer],
    };
    for handle in peers.values() {
        let _ = handle.sender.send(msg.clone());
    }
}

/// Broadcast a peer departure tombstone to all mesh peers.
/// We reuse `PeerSync` with a peer marked `left: true` (BUG-08 fix) and
/// `hosting_node_id: None`.  `version` must be a fresh stamp from this node's
/// clock so the tombstone outranks the live record it replaces everywhere.
pub async fn push_peer_left_to_mesh(mesh_peers: &MeshPeers, session_id: SessionId, version: Stamp) {
    let departed = PeerInfo {
        session_id,
        connected_at: chrono::Utc::now(),
        user_agent: None,
        hosting_node_id: None, // None = departed / offline
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
    use crate::hlc::Clock;

    fn stamp(wall: u64, counter: u32, node: &str) -> Stamp {
        Stamp { wall, counter, node: node.into() }
    }

    fn file(id: &str, version: Stamp) -> FileMetadata {
        FileMetadata {
            id: id.into(),
            name: format!("{id}.txt"),
            size: 1,
            mime_type: "text/plain".into(),
            uploader_id: "peer_a".into(),
            hosts: ["peer_a".to_string()].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: version.wall,
            deleted: false,
            deleted_at: 0,
            version,
            sha256: None,
            is_folder: false,
        }
    }

    fn tombstone(id: &str, version: Stamp) -> FileMetadata {
        FileMetadata { deleted: true, deleted_at: version.wall, ..file(id, version) }
    }

    fn catalog(entries: Vec<FileMetadata>) -> HashMap<String, FileMetadata> {
        entries.into_iter().map(|f| (f.id.clone(), f)).collect()
    }

    #[test]
    fn the_greater_version_wins_and_equal_versions_keep_what_we_have() {
        let mut local = catalog(vec![file("f", stamp(10, 0, "a"))]);

        let mut newer = file("f", stamp(11, 0, "b"));
        newer.name = "newer.txt".into();
        merge_files(&mut local, vec![newer], 100);
        assert_eq!(local["f"].name, "newer.txt");

        let mut same = file("f", stamp(11, 0, "b"));
        same.name = "echo.txt".into();
        merge_files(&mut local, vec![same], 100);
        assert_eq!(local["f"].name, "newer.txt");

        let mut older = file("f", stamp(5, 0, "c"));
        older.name = "older.txt".into();
        merge_files(&mut local, vec![older], 100);
        assert_eq!(local["f"].name, "newer.txt");
    }

    #[test]
    fn a_new_host_added_without_changing_the_creation_time_now_propagates() {
        // Before versions, equal timestamps were ignored, so a download on
        // one node never made it into another node's list of hosts.
        let mut local = catalog(vec![file("f", stamp(10, 0, "a"))]);
        let mut with_host = file("f", stamp(10, 1, "a"));
        with_host.hosts.insert("peer_b".into());
        merge_files(&mut local, vec![with_host], 100);
        assert!(local["f"].hosts.contains("peer_b"));
    }

    #[test]
    fn an_older_live_entry_cannot_bring_back_a_newer_deletion() {
        let mut local = catalog(vec![tombstone("f", stamp(20, 0, "a"))]);
        merge_files(&mut local, vec![file("f", stamp(10, 0, "b"))], 100);
        assert!(local["f"].deleted);
    }

    #[test]
    fn a_node_with_a_slow_clock_can_still_delete_what_it_has_seen() {
        let fast = Clock::new("fast".into());
        let slow = Clock::new("slow".into());
        let created = file("f", fast.now());
        // `slow` has seen the file, so its tombstone outranks it however
        // far behind its wall clock is.
        slow.observe(&created.version).unwrap();
        let deletion = tombstone("f", slow.now());

        let mut local = catalog(vec![created]);
        merge_files(&mut local, vec![deletion], 100);
        assert!(local["f"].deleted);
    }

    #[test]
    fn concurrent_changes_resolve_the_same_way_on_every_node() {
        let a = file("f", stamp(10, 0, "node_a"));
        let b = file("f", stamp(10, 0, "node_b"));
        let mut one = catalog(vec![a.clone()]);
        let mut two = catalog(vec![b.clone()]);
        merge_files(&mut one, vec![b], 100);
        merge_files(&mut two, vec![a], 100);
        assert_eq!(one["f"].version, two["f"].version);
    }

    #[test]
    fn a_full_catalog_ignores_new_ids_but_still_applies_updates() {
        let mut local = catalog(vec![file("old", stamp(1, 0, "a"))]);
        let skipped = merge_files(&mut local, vec![file("new", stamp(2, 0, "a")), file("old", stamp(3, 0, "a"))], 1);
        assert_eq!(skipped, 1);
        assert!(!local.contains_key("new"));
        assert_eq!(local["old"].version.wall, 3);
    }

    #[test]
    fn the_oldest_tombstones_are_evicted_first_to_make_room() {
        let mut local = catalog(vec![
            tombstone("t1", stamp(1, 0, "a")),
            tombstone("t2", stamp(2, 0, "a")),
            file("live", stamp(3, 0, "a")),
        ]);
        evict_old_tombstones(&mut local, 3);
        assert!(!local.contains_key("t1") && local.contains_key("t2") && local.contains_key("live"));
    }

    #[test]
    fn pruning_keeps_recent_and_future_tombstones_and_does_not_underflow() {
        let now = 10 * TOMBSTONE_TTL_MS;
        let mut local = catalog(vec![
            tombstone("expired", stamp(now - TOMBSTONE_TTL_MS - 1, 0, "a")),
            tombstone("recent", stamp(now - 1000, 0, "a")),
            tombstone("future", stamp(now + 30 * 60 * 1000, 0, "a")),
            file("live", stamp(1, 0, "a")),
        ]);
        prune_tombstones(&mut local, now);
        let mut kept: Vec<_> = local.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, ["future", "live", "recent"]);
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

    #[tokio::test]
    async fn a_catalog_sync_from_another_node_is_validated_and_advances_our_clock() {
        let state = NodeState::for_tests(None);
        let remote = Stamp { wall: crate::hlc::wall_clock_ms() + 1000, counter: 7, node: "node_remote".into() };
        let mut ok = file("f_ok", remote.clone());
        ok.name = "../evil:name.txt".into();
        let mut bad_id = file("f_bad", remote.clone());
        bad_id.id = "../x".into();

        apply_catalog_sync(&state, vec![ok, bad_id], "node_remote").await;

        let files = state.files.read().await;
        assert_eq!(files.len(), 1);
        assert_eq!(files["f_ok"].name, ".._evil_name.txt");
        // Whatever this node stamps next is ordered after what it just saw.
        assert!(state.clock.now() > remote);
    }

    #[tokio::test]
    async fn entries_stamped_far_in_the_future_are_dropped_and_reported_once() {
        let state = NodeState::for_tests(None);
        let future = Stamp { wall: crate::hlc::wall_clock_ms() + 2 * crate::hlc::MAX_DRIFT_MS, counter: 0, node: "node_fast".into() };
        let sane = Stamp { wall: crate::hlc::wall_clock_ms(), counter: 0, node: "node_fast".into() };

        apply_catalog_sync(&state, vec![file("future", future), file("sane", sane)], "node_fast").await;

        let files = state.files.read().await;
        assert!(files.contains_key("sane") && !files.contains_key("future"));
        assert!(state.clock_alert_at.lock().unwrap().is_some());
        // The bad stamp must not have dragged our clock forward.
        assert!(state.clock.now().wall < crate::hlc::wall_clock_ms() + 60_000);
    }

    #[tokio::test]
    async fn a_peer_sync_without_a_stamp_is_ignored() {
        let state = NodeState::for_tests(None);
        apply_peer_sync(&state, vec![peer("p1", Stamp::default(), false), peer("p2", stamp(crate::hlc::wall_clock_ms(), 0, "a"), false)], "node_a").await;
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
}
