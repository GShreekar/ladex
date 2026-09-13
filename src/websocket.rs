// ============================================================================
// LADEX — Browser-tab WebSocket handler (/ws)
//
// Phase 5: All WebRTC signaling messages now route through mesh::route_signal()
//          which transparently delivers locally or wraps in MeshMessage::SignalRelay.
// Phase 6: RequestDownloadFrom — client names its chosen host explicitly.
//          Node honors the choice without override; returns HostUnreachable if
//          that peer is gone.
// Phase 5: TransferDeclined — receiver signals rejection; routed back to sender.
// ============================================================================

use crate::{mesh, state};
use crate::types::*;
use crate::NodeState;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

pub async fn websocket_handler(ws: Ws, state: NodeState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub async fn broadcast(state: &NodeState, msg: ServerMessage) {
    let senders = state.local_senders.read().await;
    for sender in senders.values() {
        let _ = sender.send(msg.clone());
    }
}

/// Alias for broadcast — used by Phase 10 AP isolation diagnostic.
pub async fn broadcast_all(state: &NodeState, msg: ServerMessage) {
    broadcast(state, msg).await;
}

pub async fn send_to(state: &NodeState, target: &SessionId, msg: ServerMessage) {
    let senders = state.local_senders.read().await;
    if let Some(sender) = senders.get(target) {
        let _ = sender.send(msg);
    }
}

// ---------------------------------------------------------------------------
// WebSocket lifecycle
// ---------------------------------------------------------------------------

pub async fn handle_websocket(ws: WebSocket, state: NodeState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut session_id: Option<SessionId> = None;
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let outgoing_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
            if ws_tx.send(Message::text(json)).await.is_err() { break; }
        }
    });

    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) => {
                if let Ok(text) = msg.to_str() {
                    if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(text) {
                        if let Err(e) = handle_client_message(client_msg, &state, &mut session_id, &peer_tx).await {
                            let _ = peer_tx.send(ServerMessage::Error { message: e.to_string() });
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    if let Some(id) = &session_id {
        { let mut s = state.local_senders.write().await; s.remove(id); }
        cleanup_peer(&state, id).await;
    }
    outgoing_task.abort();
}

// ---------------------------------------------------------------------------
// Message dispatcher
// ---------------------------------------------------------------------------

async fn handle_client_message(
    msg: ClientMessage,
    state: &NodeState,
    session_id: &mut Option<SessionId>,
    peer_tx: &PeerSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match msg {
        // ── Join ─────────────────────────────────────────────────────────
        ClientMessage::Join { session_id: id, user_agent } => {
            *session_id = Some(id.clone());
            { let mut s = state.local_senders.write().await; s.insert(id.clone(), peer_tx.clone()); }

            let peer = PeerInfo {
                session_id: id.clone(),
                connected_at: chrono::Utc::now(),
                user_agent,
                hosting_node_id: Some(state.node_id.clone()),
                node_rtt_ms: None,
                left: false,
                left_at: None,
            };

            let peers_count = {
                let mut peers = state.local_peers.write().await;
                peers.insert(id.clone(), peer.clone());
                peers.len()
            };

            // Send merged catalog to new tab (non-deleted only)
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            let _ = peer_tx.send(ServerMessage::FileListUpdate { files });

            // Send full chat history
            let messages = state.messages.read().await.clone();
            if !messages.is_empty() {
                let _ = peer_tx.send(ServerMessage::MessageHistory { messages });
            }

            broadcast(state, ServerMessage::PeerJoined { peer: peer.clone(), total_peers: peers_count }).await;
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }

        // ── File upload ───────────────────────────────────────────────────
        // BUG-07 fix: a browser tab resends file_upload for every
        // locally-held file after every WS reconnect (see app.js
        // connectWebSocket()), always with `hosts: [own session_id]` since
        // the tab only knows about itself. Treating that as a fresh upload
        // — as a blind `files.insert()` did — replaced the whole catalog
        // entry and wiped out every OTHER peer that had since become a
        // host via file_downloaded, plus any sha256 computed after the
        // original upload. If an id we already know about comes back in,
        // this is a re-registration of the same file, not a new one: merge
        // hosts and sha256 into the existing entry instead of replacing it.
        ClientMessage::FileUpload { session_id: _, mut file } => {
            let file_for_mesh = {
                let mut files = state.files.write().await;
                if let Some(existing) = files.get_mut(&file.id) {
                    existing.hosts.extend(file.hosts.drain());
                    if file.sha256.is_some() {
                        existing.sha256 = file.sha256.take();
                    }
                    // A file id is only ever reused by the peer that
                    // originally minted it re-registering it — never
                    // treat that as resurrecting a deletion made by
                    // someone else in the meantime.
                    if existing.deleted {
                        existing.deleted = false;
                        existing.deleted_at = 0;
                    }
                    existing.clone()
                } else {
                    if file.created_at == 0 { file.created_at = chrono::Utc::now().timestamp_millis() as u64; }
                    file.deleted = false;
                    file.deleted_at = 0;
                    files.insert(file.id.clone(), file.clone());
                    file
                }
            };

            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;
            state::push_file_to_mesh(&state.mesh_peers, file_for_mesh).await;
        }

        // ── Phase 6: Client-side host selection ──────────────────────────
        // Client explicitly names the peer it wants to download from.
        // Node honors the choice without override.  Returns HostUnreachable
        // if that peer is not present in the merged peer list.
        ClientMessage::RequestDownloadFrom { session_id: requester_id, file_id, host_peer_id } => {
            // Verify the file exists and is not tombstoned
            let file_valid = {
                let files = state.files.read().await;
                files.get(&file_id).map(|f| !f.deleted).unwrap_or(false)
            };
            if !file_valid {
                send_to(state, &requester_id, ServerMessage::Error {
                    message: format!("File {file_id} not found or has been removed"),
                }).await;
                return Ok(());
            }

            // Check the chosen host is reachable (present in the merged peer
            // list, and — BUG-08 fix — not a departure tombstone: a
            // departed peer is still a key in the map, just marked `left`,
            // so `contains_key` alone would keep routing to ghosts).
            let host_known = {
                let peers = state.local_peers.read().await;
                peers.get(&host_peer_id).map(|p| !p.left).unwrap_or(false)
            };
            if !host_known {
                // Phase 6: do NOT silently reroute — let client retry
                send_to(state, &requester_id, ServerMessage::HostUnreachable {
                    file_id,
                    host_peer_id,
                }).await;
                return Ok(());
            }

            // Route DownloadRequest to the explicitly chosen host (Phase 5 path)
            mesh::route_signal(
                state,
                &requester_id,
                &host_peer_id,
                ServerMessage::DownloadRequest {
                    file_id,
                    requester_session_id: requester_id.clone(),
                },
            ).await;
        }

        // ── Legacy download request (server picks host — kept for fallback) ─
        ClientMessage::RequestDownload { session_id: requester_id, file_id } => {
            let file_hosts = {
                let files = state.files.read().await;
                files.get(&file_id).filter(|f| !f.deleted).map(|f| f.hosts.clone()).unwrap_or_default()
            };
            let mut available: Vec<SessionId> = file_hosts.into_iter()
                .filter(|h| *h != requester_id).collect();

            if available.is_empty() {
                send_to(state, &requester_id, ServerMessage::Error {
                    message: "No hosts available for this file".to_string(),
                }).await;
            } else {
                available.sort();
                let idx = file_id.bytes().fold(0usize, |acc, b| acc.wrapping_add(b as usize)) % available.len();
                let host_id = available[idx].clone();
                // Phase 5: route through mesh so cross-node hosts work
                mesh::route_signal(
                    state,
                    &requester_id,
                    &host_id,
                    ServerMessage::DownloadRequest {
                        file_id,
                        requester_session_id: requester_id.clone(),
                    },
                ).await;
            }
        }

        // ── File downloaded → register new host ──────────────────────────
        ClientMessage::FileDownloaded { session_id: downloader_id, file_id } => {
            let updated_file = {
                let mut files = state.files.write().await;
                if let Some(file) = files.get_mut(&file_id) {
                    file.hosts.insert(downloader_id);
                    Some(file.clone())
                } else { None }
            };
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;
            if let Some(f) = updated_file { state::push_file_to_mesh(&state.mesh_peers, f).await; }
        }

        // ── Phase 5: WebRTC signaling — routed through mesh ──────────────
        // All three signaling message types follow the same pattern:
        // route_signal() decides local delivery vs. MeshMessage::SignalRelay.

        ClientMessage::WebRTCOffer { session_id: from, target_session_id, sdp } => {
            mesh::route_signal(
                state, &from, &target_session_id,
                ServerMessage::WebRTCOffer { from_session_id: from.clone(), sdp },
            ).await;
        }

        ClientMessage::WebRTCAnswer { session_id: from, target_session_id, sdp } => {
            mesh::route_signal(
                state, &from, &target_session_id,
                ServerMessage::WebRTCAnswer { from_session_id: from.clone(), sdp },
            ).await;
        }

        ClientMessage::ICECandidate { session_id: from, target_session_id, candidate } => {
            mesh::route_signal(
                state, &from, &target_session_id,
                ServerMessage::ICECandidate { from_session_id: from.clone(), candidate },
            ).await;
        }

        // ── Phase 5: Transfer declined ────────────────────────────────────
        // Receiver doesn't want the file.  Route rejection back to sender.
        ClientMessage::TransferDeclined { session_id: from, file_id } => {
            // We don't know the sender's session_id here, but we can broadcast
            // to local tabs that host the file.  The sender tab will recognize
            // its own file_id and surface a toast.
            //
            // Limitation: cross-node rejection routing is handled in Phase 5
            // by embedding from_session_id in the SignalRelay payload.
            // For same-node: broadcast the TransferDeclined to all local tabs.
            broadcast(state, ServerMessage::TransferDeclined {
                file_id,
                from_session_id: from,
            }).await;
        }

        // ── Ping / Pong ───────────────────────────────────────────────────
        ClientMessage::Ping { session_id: _ } => {
            let _ = peer_tx.send(ServerMessage::Pong);
        }

        // ── Text messaging ────────────────────────────────────────────────
        ClientMessage::TextMessage { session_id: sender_id, content } => {
            let now_ms = chrono::Utc::now().timestamp_millis() as u64;
            let message = TextMessage {
                id: format!("msg_{}_{}", sender_id, now_ms),
                content,
                sender_id,
                sender_name: None,
                timestamp: chrono::Utc::now(),
                created_at: now_ms,
            };
            {
                let mut messages = state.messages.write().await;
                messages.push(message.clone());
                messages.sort_by_key(|m| m.created_at);
                state::prune_messages(&mut messages); // BUG-13 fix
            }
            broadcast(state, ServerMessage::TextMessage { message: message.clone() }).await;
            state::push_message_to_mesh(&state.mesh_peers, message).await;
        }

        // ── Phase 11.3: checksum update ───────────────────────────────────
        ClientMessage::FileChecksumUpdate { session_id: _, file_id, sha256 } => {
            // Patch the sha256 field on the existing catalog entry.
            let patched: Option<FileMetadata> = {
                let mut files = state.files.write().await;
                if let Some(file) = files.get_mut(&file_id) {
                    file.sha256 = Some(sha256.clone());
                    Some(file.clone())
                } else {
                    None
                }
            };
            if let Some(file) = patched {
                tracing::info!("Checksum: {file_id} → sha256={sha256:.16}…");
                // Re-broadcast updated file list to local tabs
                let files: Vec<FileMetadata> = {
                    let files = state.files.read().await;
                    files.values().filter(|f| !f.deleted).cloned().collect()
                };
                broadcast(state, ServerMessage::FileListUpdate { files }).await;
                // Propagate updated FileMetadata to mesh peers
                state::push_file_to_mesh(&state.mesh_peers, file).await;
            } else {
                tracing::warn!("FileChecksumUpdate: unknown file_id {file_id}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Peer cleanup on disconnect
// ---------------------------------------------------------------------------

async fn cleanup_peer(state: &NodeState, session_id: &SessionId) {
    let peers_count = {
        let mut peers = state.local_peers.write().await;
        peers.remove(session_id);
        peers.len()
    };

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let mut tombstoned_files: Vec<FileMetadata> = Vec::new();
    {
        let mut files = state.files.write().await;
        for file in files.values_mut() {
            if !file.hosts.contains(session_id) { continue; }
            file.hosts.remove(session_id);
            if file.hosts.is_empty() && !file.deleted {
                file.deleted = true;
                file.deleted_at = now_ms;
                tombstoned_files.push(file.clone());
            }
        }
    }

    broadcast(state, ServerMessage::PeerLeft { session_id: session_id.clone(), total_peers: peers_count }).await;

    let files: Vec<FileMetadata> = {
        let files = state.files.read().await;
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    broadcast(state, ServerMessage::FileListUpdate { files }).await;

    for tombstone in tombstoned_files {
        state::push_file_to_mesh(&state.mesh_peers, tombstone).await;
    }
    state::push_peer_left_to_mesh(&state.mesh_peers, session_id.clone()).await;
}
