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

/// Merge `incoming` peer infos into `local` using LWW on `connected_at`.
/// Peers from remote nodes are stored alongside local peers in `local_peers`
/// so the browser tab's peer list is a unified view of the whole mesh.
///
/// Note: this writes to `NodeState::local_peers` (which the browser sees),
/// NOT to `mesh_peers` (which is node handles).  The naming is slightly
/// unfortunate but matches the existing field layout — `local_peers` is the
/// *peer registry visible to the browser*, regardless of where those peers
/// are physically connected.
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
                // LWW: keep whichever has the newer connected_at timestamp.
                if peer.connected_at > existing.connected_at {
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


/// Apply an incoming chat sync and fan out the full history to local tabs.
pub async fn apply_chat_sync(state: &NodeState, incoming: Vec<TextMessage>) {
    let merged: Vec<TextMessage> = {
        let mut messages = state.messages.write().await;
        merge_messages(&mut messages, incoming);
        messages.clone()
    };
    if !merged.is_empty() {
        websocket::broadcast(state, ServerMessage::MessageHistory { messages: merged }).await;
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
/// We reuse `PeerSync` with a peer whose session is marked offline
/// by setting `hosting_node_id = None` (signals "this session ended").
/// Phase 10 adds explicit tombstone fields; for now the absence of a sender
/// is sufficient for mesh peers to clean up their `local_peers` entry.
pub async fn push_peer_left_to_mesh(mesh_peers: &MeshPeers, session_id: SessionId) {
    // A PeerInfo with no hosting_node_id signals departure.
    let departed = PeerInfo {
        session_id: session_id.clone(),
        connected_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
        user_agent: None,
        hosting_node_id: None, // None = departed / offline
        node_rtt_ms: None,
    };
    push_peer_to_mesh(mesh_peers, departed).await;
}
