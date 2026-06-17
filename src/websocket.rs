// ============================================================================
// LADEX — Browser-tab WebSocket handler (/ws)
//
// Handles messages from THIS node's own browser tab(s).
// For node-to-node mesh messages, see mesh.rs.
//
// Phase 4 additions:
//   • On FileUpload → push incremental CatalogSync to all mesh peers
//   • On FileDownloaded (new host registered) → push updated file entry to mesh
//   • On TextMessage → push ChatMessage to all mesh peers
//   • On Join → push PeerInfo to all mesh peers
//   • On cleanup (disconnect) → tombstone file entries whose sole host was
//     this session, propagate tombstones to mesh
// ============================================================================

use crate::state;
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
// Helpers: broadcast to all peers / send to a single peer
// ---------------------------------------------------------------------------

/// Send a message to every connected browser tab on THIS node.
pub async fn broadcast(state: &NodeState, msg: ServerMessage) {
    let senders = state.local_senders.read().await;
    for sender in senders.values() {
        let _ = sender.send(msg.clone());
    }
}

/// Send a message to a single browser tab identified by session id.
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

    // Per-peer mpsc channel — sender stored in local_senders on Join.
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Write task: drain mpsc → WS.
    let outgoing_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if ws_tx.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    // Read loop: WS → dispatch.
    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) => {
                if let Ok(text) = msg.to_str() {
                    if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(text) {
                        if let Err(e) =
                            handle_client_message(client_msg, &state, &mut session_id, &peer_tx)
                                .await
                        {
                            let _ = peer_tx.send(ServerMessage::Error {
                                message: e.to_string(),
                            });
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Cleanup on disconnect.
    if let Some(id) = &session_id {
        {
            let mut senders = state.local_senders.write().await;
            senders.remove(id);
        }
        cleanup_peer(&state, id).await;
    }

    outgoing_task.abort();
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

async fn handle_client_message(
    msg: ClientMessage,
    state: &NodeState,
    session_id: &mut Option<SessionId>,
    peer_tx: &PeerSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match msg {
        // ── Join ─────────────────────────────────────────────────────────
        ClientMessage::Join {
            session_id: id,
            user_agent,
        } => {
            *session_id = Some(id.clone());

            {
                let mut senders = state.local_senders.write().await;
                senders.insert(id.clone(), peer_tx.clone());
            }

            let peer = PeerInfo {
                session_id: id.clone(),
                connected_at: chrono::Utc::now(),
                user_agent,
                // Phase 1/Phase 4: tag session with this node so mesh peers can
                // look up which node hosts it (used by Phase 5 signaling relay).
                hosting_node_id: Some(state.node_id.clone()),
            };

            let peers_count = {
                let mut peers = state.local_peers.write().await;
                peers.insert(id.clone(), peer.clone());
                peers.len()
            };

            // Send merged catalog (includes files from remote nodes via Phase 4 sync)
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            let _ = peer_tx.send(ServerMessage::FileListUpdate { files });

            // Send full chat history
            let messages = {
                let messages = state.messages.read().await;
                messages.clone()
            };
            if !messages.is_empty() {
                let _ = peer_tx.send(ServerMessage::MessageHistory { messages });
            }

            // Notify all local tabs
            broadcast(
                state,
                ServerMessage::PeerJoined {
                    peer: peer.clone(),
                    total_peers: peers_count,
                },
            )
            .await;

            // Phase 4: push this new peer to all mesh nodes (incremental PeerSync)
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }

        // ── File catalog ─────────────────────────────────────────────────
        ClientMessage::FileUpload {
            session_id: _,
            mut file,
        } => {
            // Ensure LWW timestamp is populated
            if file.created_at == 0 {
                file.created_at = chrono::Utc::now().timestamp_millis() as u64;
            }
            // Clear tombstone fields (fresh upload is always live)
            file.deleted = false;
            file.deleted_at = 0;

            let file_for_mesh = file.clone();
            {
                let mut files = state.files.write().await;
                files.insert(file.id.clone(), file);
            }

            // Broadcast updated catalog to local tabs (non-deleted only)
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;

            // Phase 4: push single new entry to mesh peers (not the full catalog)
            state::push_file_to_mesh(&state.mesh_peers, file_for_mesh).await;
        }

        // ── Download request → pick a host (round-robin for now) ─────────
        // Phase 6 will move this selection to the client side; for now the node
        // still picks, but only from hosts known locally (merged state).
        ClientMessage::RequestDownload {
            session_id: requester_id,
            file_id,
        } => {
            let file_hosts = {
                let files = state.files.read().await;
                files
                    .get(&file_id)
                    .filter(|f| !f.deleted)
                    .map(|f| f.hosts.clone())
                    .unwrap_or_default()
            };

            let mut available: Vec<SessionId> = file_hosts
                .into_iter()
                .filter(|h| *h != requester_id)
                .collect();

            if available.is_empty() {
                send_to(
                    state,
                    &requester_id,
                    ServerMessage::Error {
                        message: "No hosts available for this file".to_string(),
                    },
                )
                .await;
            } else {
                available.sort();
                let idx = file_id
                    .bytes()
                    .fold(0usize, |acc, b| acc.wrapping_add(b as usize))
                    % available.len();
                let host_id = &available[idx];

                send_to(
                    state,
                    host_id,
                    ServerMessage::DownloadRequest {
                        file_id,
                        requester_session_id: requester_id,
                    },
                )
                .await;
            }
        }

        // ── File downloaded → register new host ──────────────────────────
        ClientMessage::FileDownloaded {
            session_id: downloader_id,
            file_id,
        } => {
            let updated_file = {
                let mut files = state.files.write().await;
                if let Some(file) = files.get_mut(&file_id) {
                    file.hosts.insert(downloader_id);
                    Some(file.clone())
                } else {
                    None
                }
            };

            // Local broadcast
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;

            // Phase 4: push updated file (with new host) to mesh peers
            if let Some(f) = updated_file {
                state::push_file_to_mesh(&state.mesh_peers, f).await;
            }
        }

        // ── WebRTC signaling: Offer ───────────────────────────────────────
        // Phase 5 will intercept these for cross-node routing via SignalRelay.
        // For now (mesh_mode=false or same-node), deliver locally.
        ClientMessage::WebRTCOffer {
            session_id: from,
            target_session_id,
            sdp,
        } => {
            send_to(
                state,
                &target_session_id,
                ServerMessage::WebRTCOffer {
                    from_session_id: from,
                    sdp,
                },
            )
            .await;
        }

        // ── WebRTC signaling: Answer ──────────────────────────────────────
        ClientMessage::WebRTCAnswer {
            session_id: from,
            target_session_id,
            sdp,
        } => {
            send_to(
                state,
                &target_session_id,
                ServerMessage::WebRTCAnswer {
                    from_session_id: from,
                    sdp,
                },
            )
            .await;
        }

        // ── WebRTC signaling: ICE Candidate ──────────────────────────────
        ClientMessage::ICECandidate {
            session_id: from,
            target_session_id,
            candidate,
        } => {
            send_to(
                state,
                &target_session_id,
                ServerMessage::ICECandidate {
                    from_session_id: from,
                    candidate,
                },
            )
            .await;
        }

        // ── Ping / Pong ───────────────────────────────────────────────────
        ClientMessage::Ping { session_id: _ } => {
            let _ = peer_tx.send(ServerMessage::Pong);
        }

        // ── Text messaging ────────────────────────────────────────────────
        ClientMessage::TextMessage {
            session_id: sender_id,
            content,
        } => {
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
            }

            // Local broadcast
            broadcast(state, ServerMessage::TextMessage { message: message.clone() }).await;

            // Phase 4: push to mesh peers so all nodes see the message
            state::push_message_to_mesh(&state.mesh_peers, message).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cleanup on browser tab disconnect
// ---------------------------------------------------------------------------

async fn cleanup_peer(state: &NodeState, session_id: &SessionId) {
    let peers_count = {
        let mut peers = state.local_peers.write().await;
        peers.remove(session_id);
        peers.len()
    };

    // For every file this session was the sole host of:
    //   - Set tombstone (deleted=true, deleted_at=now_ms)
    //   - Propagate to mesh peers (Phase 4)
    // For files with remaining hosts: just remove this session from the host set.
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let mut tombstoned_files: Vec<FileMetadata> = Vec::new();

    {
        let mut files = state.files.write().await;
        for file in files.values_mut() {
            if !file.hosts.contains(session_id) {
                continue;
            }
            file.hosts.remove(session_id);
            if file.hosts.is_empty() && !file.deleted {
                // Tombstone: this node was the only host
                file.deleted = true;
                file.deleted_at = now_ms;
                tombstoned_files.push(file.clone());
            }
        }
    }

    // Broadcast peer departure to local tabs
    broadcast(
        state,
        ServerMessage::PeerLeft {
            session_id: session_id.clone(),
            total_peers: peers_count,
        },
    )
    .await;

    // Broadcast updated catalog (without deleted files) to local tabs
    let files: Vec<FileMetadata> = {
        let files = state.files.read().await;
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    broadcast(state, ServerMessage::FileListUpdate { files }).await;

    // Phase 4: push tombstones to mesh peers
    for tombstone in tombstoned_files {
        state::push_file_to_mesh(&state.mesh_peers, tombstone).await;
    }

    // Phase 4: push peer departure to mesh peers
    state::push_peer_left_to_mesh(&state.mesh_peers, session_id.clone()).await;
}
