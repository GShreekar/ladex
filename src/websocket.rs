use crate::types::*;
use crate::AppState;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use warp::ws::{WebSocket, Ws, Message};
use warp::{Rejection, Reply};

pub async fn websocket_handler(ws: Ws, state: AppState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state)))
}

pub async fn handle_websocket(ws: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut session_id: Option<SessionId> = None;
    let mut rx = state.tx.subscribe();

    // Spawn a task to handle outgoing messages
    let outgoing_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            let json = serde_json::to_string(&msg).unwrap();
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
                        match handle_client_message(client_msg, &state, &mut session_id).await {
                            Ok(_) => {}
                            Err(e) => {
                                let error_msg = ServerMessage::Error {
                                    message: e.to_string(),
                                };
                                let _ = state.tx.send(error_msg);
                            }
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Cleanup when connection closes
    if let Some(id) = session_id {
        cleanup_peer(&state, &id).await;
    }

    outgoing_task.abort();
}

async fn handle_client_message(
    msg: ClientMessage,
    state: &AppState,
    session_id: &mut Option<SessionId>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match msg {
        ClientMessage::Join {
            session_id: id,
            user_agent,
        } => {
            *session_id = Some(id.clone());
            
            let peer = PeerInfo {
                session_id: id.clone(),
                connected_at: chrono::Utc::now(),
                user_agent,
            };

            // Add peer to the map
            let peers_count = {
                let mut peers = state.peers.write().await;
                peers.insert(id.clone(), peer.clone());
                peers.len()
            };

            // Send current file list to the new peer
            let files = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            
            let _ = state.tx.send(ServerMessage::FileListUpdate { files });
            
            // Send message history to the new peer
            let messages = {
                let messages = state.messages.read().await;
                messages.clone()
            };
            
            if !messages.is_empty() {
                let _ = state.tx.send(ServerMessage::MessageHistory { messages });
            }

            // Notify all peers about new peer
            let _ = state.tx.send(ServerMessage::PeerJoined {
                peer,
                total_peers: peers_count,
            });
        }
        ClientMessage::FileUpload { session_id: _, file } => {
            // Add file to the registry
            {
                let mut files = state.files.write().await;
                files.insert(file.id.clone(), file.clone());
            }

            // Send updated file list instead of individual file added message
            let files = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            
            let _ = state.tx.send(ServerMessage::FileListUpdate { files });
        }
        ClientMessage::RequestDownload {
            session_id: requester_id,
            file_id,
        } => {
            // Find a host for this file
            let file_hosts = {
                let files = state.files.read().await;
                if let Some(file) = files.get(&file_id) {
                    file.hosts.clone()
                } else {
                    HashSet::new()
                }
            };

            // Pick the first available host (could be improved with load balancing)
            if let Some(host_id) = file_hosts.iter().next() {
                let _ = state.tx.send(ServerMessage::DownloadRequest {
                    from_session_id: host_id.clone(),
                    file_id,
                    requester_session_id: requester_id,
                });
            } else {
                let _ = state.tx.send(ServerMessage::Error {
                    message: "No hosts available for this file".to_string(),
                });
            }
        }
        ClientMessage::FileDownloaded {
            session_id: downloader_id,
            file_id,
        } => {
            // Add downloader as a new host
            {
                let mut files = state.files.write().await;
                if let Some(file) = files.get_mut(&file_id) {
                    file.hosts.insert(downloader_id);
                }
            }

            // Send updated file list
            let files = {
                let files = state.files.read().await;
                files.values().cloned().collect()
            };
            
            let _ = state.tx.send(ServerMessage::FileListUpdate { files });
        }
        ClientMessage::Ping { session_id: _ } => {
            let _ = state.tx.send(ServerMessage::Pong);
        }
        ClientMessage::FileChunk {
            session_id: _,
            file_id,
            chunk_index,
            total_chunks,
            data,
            target_session_id,
        } => {
            // Forward the file chunk to the target session
            let _ = state.tx.send(ServerMessage::FileChunk {
                file_id,
                chunk_index,
                total_chunks,
                data,
                from_session_id: session_id.clone().unwrap_or_default(),
                target_session_id,
            });
        }
        ClientMessage::FileMetadata {
            session_id: _,
            file_id,
            file_name,
            file_size,
            mime_type,
            total_chunks,
            target_session_id,
        } => {
            // Forward the file metadata to the target session
            let _ = state.tx.send(ServerMessage::FileMetadata {
                file_id,
                file_name,
                file_size,
                mime_type,
                total_chunks,
                from_session_id: session_id.clone().unwrap_or_default(),
                target_session_id,
            });
        }
        ClientMessage::TextMessage {
            session_id: sender_id,
            content,
        } => {
            let message = TextMessage {
                id: format!("msg_{}_{}", sender_id, chrono::Utc::now().timestamp_millis()),
                content,
                sender_id: sender_id.clone(),
                sender_name: None,
                timestamp: chrono::Utc::now(),
            };
            {
                let mut messages = state.messages.write().await;
                messages.push(message.clone());
            }
            
            let _ = state.tx.send(ServerMessage::TextMessage { message });
        }
    }
    Ok(())
}

async fn cleanup_peer(state: &AppState, session_id: &SessionId) {
    // Remove peer from peers map
    let peers_count = {
        let mut peers = state.peers.write().await;
        peers.remove(session_id);
        peers.len()
    };

    // Remove peer from file hosts and clean up files with no hosts
    let files_to_remove = {
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
        
        to_remove
    };

    // Notify about peer leaving
    let _ = state.tx.send(ServerMessage::PeerLeft {
        session_id: session_id.clone(),
        total_peers: peers_count,
    });

    // Notify about removed files
    for file_id in files_to_remove {
        let _ = state.tx.send(ServerMessage::FileRemoved { file_id });
    }

    // Send updated file list
    let files = {
        let files = state.files.read().await;
        files.values().cloned().collect()
    };
    
    let _ = state.tx.send(ServerMessage::FileListUpdate { files });
}
