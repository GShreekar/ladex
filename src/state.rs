// ============================================================================
// LADEX — Phase 4: Distributed State Merge
//
// Implements last-write-wins (LWW) merge for the three distributed state
// stores: file catalog, peer list, and chat messages.
//
// Design (from ROADMAP.md §4):
//
//   • LWW on `created_at: u64` (Unix milliseconds) per entry.
//     On conflict (same id, different node), keep the higher timestamp.
//     This is sufficient for human-paced file sharing at LAN scale.
//
//   • File tombstones: instead of deleting outright, set `deleted = true` +
//     `deleted_at: u64` and propagate.  A file is pruned from the in-memory
//     catalog after its tombstone is 60+ seconds old.  This prevents races
//     where a deletion and a late-arriving catalog sync fight.
//
//   • After each merge, the updated state is fanned out to all connected
//     local browser tabs via `websocket::broadcast`.
//
//   • Full-state sync is sent immediately after HelloAck (Phase 3 wires
//     this — see `mesh::post_handshake_sync`).
//     Incremental deltas are sent per-event (see `push_*` functions below).
// ============================================================================

use std::collections::HashMap;

use crate::types::*;
use crate::NodeState;
use crate::websocket;

// ---------------------------------------------------------------------------
// Merge: file catalog
// ---------------------------------------------------------------------------

/// Merge `incoming` files into `local` using LWW on `created_at`.
///
/// Tombstoned entries (deleted=true) win over live entries of the same age
/// because the deletion event always sets a newer `deleted_at`.  Pruning
/// of old tombstones (>60s) is handled separately by `prune_tombstones`.
pub fn merge_files(local: &mut HashMap<String, FileMetadata>, incoming: Vec<FileMetadata>) {
    for entry in incoming {
        match local.get(&entry.id) {
            None => {
                local.insert(entry.id.clone(), entry);
            }
            Some(existing) => {
                // Use whichever timestamp is newer.
                // For tombstones: `deleted_at` is always > `created_at`, so a
                // tombstone always beats a live entry for the same file id.
                let incoming_ts = if entry.deleted {
                    entry.deleted_at
                } else {
                    entry.created_at
                };
                let existing_ts = if existing.deleted {
                    existing.deleted_at
                } else {
                    existing.created_at
                };
                if incoming_ts > existing_ts {
                    local.insert(entry.id.clone(), entry);
                }
            }
        }
    }
}

/// Remove tombstoned entries whose `deleted_at` is more than 60 seconds old.
/// Call this periodically (e.g. every 60s) to keep memory bounded.
pub fn prune_tombstones(local: &mut HashMap<String, FileMetadata>) {
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    const TOMBSTONE_TTL_MS: u64 = 60_000;
    local.retain(|_, f| !f.deleted || (now_ms - f.deleted_at) < TOMBSTONE_TTL_MS);
}

/// BUG-08 fix companion: remove peer-departure tombstones whose `left_at` is
/// more than 60 seconds old, mirroring `prune_tombstones` above. Without
/// this, a correctly-propagated departure tombstone would sit in
/// `local_peers` forever (harmless to correctness — `left: true` is still
/// respected everywhere it's checked — but an unbounded, pointless leak).
pub fn prune_peer_tombstones(local: &mut HashMap<SessionId, PeerInfo>) {
    let now = chrono::Utc::now();
    const TOMBSTONE_TTL: chrono::Duration = chrono::Duration::seconds(60);
    local.retain(|_, p| !p.left || p.left_at.map(|t| now - t < TOMBSTONE_TTL).unwrap_or(false));
}

// ---------------------------------------------------------------------------
// Merge: chat messages
// ---------------------------------------------------------------------------

/// Merge `incoming` messages into `local` using LWW on `created_at`.
/// Deduplicates by `message.id`.
pub fn merge_messages(local: &mut Vec<TextMessage>, incoming: Vec<TextMessage>) {
    // Build an index for O(1) lookup
    let mut index: HashMap<String, usize> = local
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();

    for msg in incoming {
        match index.get(&msg.id) {
            None => {
                index.insert(msg.id.clone(), local.len());
                local.push(msg);
            }
            Some(&idx) => {
                if msg.created_at > local[idx].created_at {
                    local[idx] = msg;
                }
            }
        }
    }

    // Keep messages in chronological order after merge.
    local.sort_by_key(|m| m.created_at);
}

// ---------------------------------------------------------------------------
// Merge: peer list
// ---------------------------------------------------------------------------

/// Merge `incoming` peer infos into `local` using LWW.
/// Peers from remote nodes are stored alongside local peers in `local_peers`
/// so the browser tab's peer list is a unified view of the whole mesh.
///
/// Note: this writes to `NodeState::local_peers` (which the browser sees),
/// NOT to `mesh_peers` (which is node handles).  The naming is slightly
/// unfortunate but matches the existing field layout — `local_peers` is the
/// *peer registry visible to the browser*, regardless of where those peers
/// are physically connected.
///
/// BUG-08 fix: the LWW key is `left_at` for a departure tombstone (`left ==
/// true`) and `connected_at` otherwise — mirrors `merge_files`'s handling
/// of `deleted`/`deleted_at` above. `left_at` is always set to "now" when a
/// departure is pushed (see `push_peer_left_to_mesh`), so it reliably beats
/// whatever `connected_at` it's replacing, unlike the old MIN_UTC sentinel
/// that could never win an LWW comparison.
pub fn merge_peers(
    local: &mut HashMap<SessionId, PeerInfo>,
    incoming: Vec<PeerInfo>,
) {
    for peer in incoming {
        match local.get(&peer.session_id) {
            None => {
                local.insert(peer.session_id.clone(), peer);
            }
            Some(existing) => {
                let incoming_ts = if peer.left { peer.left_at.unwrap_or(peer.connected_at) } else { peer.connected_at };
                let existing_ts = if existing.left { existing.left_at.unwrap_or(existing.connected_at) } else { existing.connected_at };
                if incoming_ts > existing_ts {
                    local.insert(peer.session_id.clone(), peer);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// High-level apply-and-fanout helpers
// ---------------------------------------------------------------------------

/// Apply an incoming catalog sync and fan out the updated list to local tabs.
pub async fn apply_catalog_sync(state: &NodeState, incoming: Vec<FileMetadata>) {
    let updated: Vec<FileMetadata> = {
        let mut files = state.files.write().await;
        merge_files(&mut files, incoming);
        // Return only non-deleted files for the browser (tombstones are internal)
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    websocket::broadcast(state, ServerMessage::FileListUpdate { files: updated }).await;
}

/// Apply an incoming peer sync and fan out the updated peer list to local tabs.
pub async fn apply_peer_sync(state: &NodeState, incoming: Vec<PeerInfo>) {
    let updated_peers: Vec<PeerInfo> = {
        let mut peers = state.local_peers.write().await;
        for peer in &incoming {
            merge_peers(&mut peers, vec![peer.clone()]);
        }
        incoming
    };
    // Phase 6: broadcast incremental PeerSync to browser tabs so client-side
    // host selection can incorporate RTT data from remote nodes.
    if !updated_peers.is_empty() {
        websocket::broadcast(state, ServerMessage::PeerSync { peers: updated_peers }).await;
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
    let already_known = {
        let messages = state.messages.read().await;
        messages.iter().any(|m| m.id == message.id)
    };
    if !already_known {
        {
            let mut messages = state.messages.write().await;
            messages.push(message.clone());
            messages.sort_by_key(|m| m.created_at);
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
/// `hosting_node_id: None`. `left_at` is set to "now" so it reliably wins
/// the LWW comparison in `merge_peers` against whatever `connected_at` the
/// receiving node currently has for this session — unlike the old
/// `connected_at: DateTime::MIN_UTC` sentinel, which could never be
/// greater than a real connection time and so silently lost that
/// comparison forever, leaving every other mesh node believing a
/// long-gone browser tab was still a live, routable peer.
pub async fn push_peer_left_to_mesh(mesh_peers: &MeshPeers, session_id: SessionId) {
    let departed = PeerInfo {
        session_id: session_id.clone(),
        connected_at: chrono::Utc::now(),
        user_agent: None,
        hosting_node_id: None, // None = departed / offline
        node_rtt_ms: None,
        left: true,
        left_at: Some(chrono::Utc::now()),
    };
    push_peer_to_mesh(mesh_peers, departed).await;
}
