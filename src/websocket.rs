use crate::types::*;
use crate::AppState;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

pub async fn websocket_handler(ws: Ws, state: AppState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state)))
}

// ---------------------------------------------------------------------------
// Helpers: broadcast to all peers / send to a single peer
// ---------------------------------------------------------------------------

/// Send a message to every connected peer.
async fn broadcast(state: &AppState, msg: ServerMessage) {
    let senders = state.senders.read().await;
    for sender in senders.values() {
        let _ = sender.send(msg.clone());
    }
}

/// Send a message to a single peer identified by session id.
async fn send_to(state: &AppState, target: &SessionId, msg: ServerMessage) {
    let senders = state.senders.read().await;
    if let Some(sender) = senders.get(target) {
        let _ = sender.send(msg);
    }
}

// ---------------------------------------------------------------------------
// WebSocket lifecycle
// ---------------------------------------------------------------------------

pub async fn handle_websocket(ws: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut session_id: Option<SessionId> = None;

    // Create a per-peer mpsc channel.  The sender half is registered in the
    // shared map once we know the session_id (on Join).  The receiver half
    // drives outgoing messages for THIS connection only.
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Spawn a task that drains the per-peer receiver and writes to the WS.
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

    // Handle incoming messages
    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) => {
                if let Ok(text) = msg.to_str() {
                    if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(text) {
                        match handle_client_message(
                            client_msg,
                            &state,
                            &mut session_id,
                            &peer_tx,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                // Send the error only to THIS peer, not everyone
                                let _ = peer_tx.send(ServerMessage::Error {
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Cleanup when connection closes
    if let Some(id) = &session_id {
        // Remove sender from shared map first
        {
            let mut senders = state.senders.write().await;
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
    state: &AppState,
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

            // Register this peer's sender so others can route messages to it
            {
                let mut senders = state.senders.write().await;
                senders.insert(id.clone(), peer_tx.clone());
            }

            let peer = PeerInfo {
                session_id: id.clone(),
                connected_at: chrono::Utc::now(),
                user_agent,
            };

            let peers_count = {
                let mut peers = state.peers.write().await;
                peers.insert(id.clone(), peer.clone());
                peers.len()
            };

            // Send current file catalog only to the newly joined peer
            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            let _ = peer_tx.send(ServerMessage::FileListUpdate { files });

            // Send message history only to the newly joined peer
            let messages = {
                let messages = state.messages.read().await;
                messages.clone()
            };
            if !messages.is_empty() {
                let _ = peer_tx.send(ServerMessage::MessageHistory { messages });
            }

            // Notify ALL peers about the new peer
            broadcast(
                state,
                ServerMessage::PeerJoined {
                    peer,
                    total_peers: peers_count,
                },
            )
            .await;
        }

        // ── File catalog ─────────────────────────────────────────────────
        ClientMessage::FileUpload {
            session_id: _,
            file,
        } => {
            {
                let mut files = state.files.write().await;
                files.insert(file.id.clone(), file);
            }

            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;
        }

        // ── Download request → pick a host (round-robin) ───────────────────
        ClientMessage::RequestDownload {
            session_id: requester_id,
            file_id,
        } => {
            let file_hosts = {
                let files = state.files.read().await;
                files.get(&file_id).map(|f| f.hosts.clone()).unwrap_or_default()
            };

            // Don't pick the requester as host for its own file
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
                // Round-robin: sort by session_id for determinism, then
                // rotate based on a simple counter derived from the file_id
                // hash so different files spread across different hosts.
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
            {
                let mut files = state.files.write().await;
                if let Some(file) = files.get_mut(&file_id) {
                    file.hosts.insert(downloader_id);
                }
            }

            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;
        }

        // ── WebRTC signaling: Offer ──────────────────────────────────────
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

        // ── WebRTC signaling: Answer ─────────────────────────────────────
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

        // ── Ping / Pong ──────────────────────────────────────────────────
        ClientMessage::Ping { session_id: _ } => {
            let _ = peer_tx.send(ServerMessage::Pong);
        }

        // ── Text messaging ───────────────────────────────────────────────
        ClientMessage::TextMessage {
            session_id: sender_id,
            content,
        } => {
            let message = TextMessage {
                id: format!(
                    "msg_{}_{}",
                    sender_id,
                    chrono::Utc::now().timestamp_millis()
                ),
                content,
                sender_id,
                sender_name: None,
                timestamp: chrono::Utc::now(),
            };
            {
                let mut messages = state.messages.write().await;
                messages.push(message.clone());
            }
            broadcast(state, ServerMessage::TextMessage { message }).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

async fn cleanup_peer(state: &AppState, session_id: &SessionId) {
    let peers_count = {
        let mut peers = state.peers.write().await;
        peers.remove(session_id);
        peers.len()
    };

    // Remove peer from file hosts; drop files with zero remaining hosts
    {
        let mut files = state.files.write().await;
        let mut to_remove = Vec::new();

        for (file_id, file) in files.iter_mut() {
            file.hosts.remove(session_id);
            if file.hosts.is_empty() {
                to_remove.push(file_id.clone());
            }
        }

        for file_id in &to_remove {
            files.remove(file_id);
        }
    }

    // Broadcast peer departure + updated catalog
    broadcast(
        state,
        ServerMessage::PeerLeft {
            session_id: session_id.clone(),
            total_peers: peers_count,
        },
    )
    .await;

    let files: Vec<FileMetadata> = {
        let files = state.files.read().await;
        files.values().cloned().collect()
    };
    broadcast(state, ServerMessage::FileListUpdate { files }).await;
}
