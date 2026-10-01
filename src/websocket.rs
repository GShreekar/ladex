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

use crate::sessions::SessionHandle;
use crate::types::*;
use crate::validate;
use crate::NodeState;
use crate::{mesh, state};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::{broadcast::error::RecvError, mpsc};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

// Browser tabs only send small JSON control messages (the biggest are SDP
// blobs of a few KB), so anything near this size is not a legitimate client.
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_FILES_PER_SESSION: usize = 1000;

/// Who currently holds a browser `session_id` on this node, so another
/// connection can't take it over and act as that device.
#[derive(Debug, Clone)]
pub struct ConnOwner {
    pub conn_id: u64,
    /// Login session of the owning connection; None on a node without a passphrase.
    pub auth_id: Option<String>,
}

struct Connection {
    id: u64,
    auth_id: Option<String>,
    /// The `session_id` this connection joined as; every later message must carry it.
    session_id: Option<SessionId>,
    tx: PeerSender,
}

pub async fn websocket_handler(auth: Option<SessionHandle>, ws: Ws, state: NodeState) -> Result<impl Reply, Rejection> {
    let ws = ws.max_message_size(MAX_WS_MESSAGE_BYTES).max_frame_size(MAX_WS_MESSAGE_BYTES);
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state, auth)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// F5: the client already truncates nicknames to 40 chars, but a
/// non-browser client (or a modified one) could send anything — clean and
/// cap it server-side too.
fn cap_nickname(nickname: String) -> String {
    validate::clean_label(&nickname, validate::MAX_NICKNAME_CHARS)
}

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

pub async fn handle_websocket(ws: WebSocket, state: NodeState, auth: Option<SessionHandle>) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut conn = Connection {
        id: state.next_connection_id(),
        auth_id: auth.as_ref().map(|a| a.id.clone()),
        session_id: None,
        tx: peer_tx.clone(),
    };

    let outgoing_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
            if ws_tx.send(Message::text(json)).await.is_err() { break; }
        }
    });

    // The connection ends with its login session: on revocation, on logout,
    // or when the session expires.
    let mut ended = state.sessions.subscribe_ended();
    let expiry = async {
        match &auth {
            Some(a) => tokio::time::sleep_until(tokio::time::Instant::from_std(a.expires_at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(expiry);

    let mut signed_out = false;
    loop {
        tokio::select! {
            incoming = ws_rx.next() => {
                let Some(Ok(msg)) = incoming else { break };
                if !still_owns_session(&state, &conn).await {
                    break; // the same login joined again from a newer connection
                }
                let Ok(text) = msg.to_str() else { continue };
                let Ok(client_msg) = serde_json::from_str::<ClientMessage>(text) else { continue };
                if let Err(e) = handle_client_message(client_msg, &state, &mut conn).await {
                    let _ = peer_tx.send(ServerMessage::Error { message: e.to_string() });
                }
            }
            ended_id = ended.recv(), if conn.auth_id.is_some() => {
                let ours = match &ended_id {
                    Ok(id) => Some(id) == conn.auth_id.as_ref(),
                    Err(RecvError::Lagged(_)) => !state.sessions.is_active(conn.auth_id.as_deref().unwrap_or("")),
                    Err(RecvError::Closed) => true,
                };
                if ours {
                    signed_out = true;
                    break;
                }
            }
            _ = &mut expiry => {
                signed_out = true;
                break;
            }
        }
    }

    if signed_out {
        let _ = peer_tx.send(ServerMessage::Error { message: "This device was signed out.".to_string() });
        tokio::time::sleep(Duration::from_millis(150)).await; // let the notice go out
    }

    if let Some(id) = &conn.session_id {
        // A newer connection may have taken this id over; it keeps it.
        let owned = {
            let mut owners = state.session_owners.write().await;
            let owned = owners.get(id).is_some_and(|o| o.conn_id == conn.id);
            if owned { owners.remove(id); }
            owned
        };
        if owned {
            { let mut s = state.local_senders.write().await; s.remove(id); }
            cleanup_peer(&state, id).await;
        }
    }
    outgoing_task.abort();
}

async fn still_owns_session(state: &NodeState, conn: &Connection) -> bool {
    match &conn.session_id {
        None => true,
        Some(id) => state.session_owners.read().await.get(id).is_some_and(|o| o.conn_id == conn.id),
    }
}

/// The `session_id` a message says it comes from. Every client message carries one.
fn claimed_session(msg: &ClientMessage) -> &SessionId {
    match msg {
        ClientMessage::Join { session_id, .. }
        | ClientMessage::SetNickname { session_id, .. }
        | ClientMessage::FileUpload { session_id, .. }
        | ClientMessage::RequestDownload { session_id, .. }
        | ClientMessage::FileDownloaded { session_id, .. }
        | ClientMessage::WebRTCOffer { session_id, .. }
        | ClientMessage::WebRTCAnswer { session_id, .. }
        | ClientMessage::ICECandidate { session_id, .. }
        | ClientMessage::RequestDownloadFrom { session_id, .. }
        | ClientMessage::TransferDeclined { session_id, .. }
        | ClientMessage::Ping { session_id }
        | ClientMessage::TextMessage { session_id, .. }
        | ClientMessage::FileChecksumUpdate { session_id, .. }
        | ClientMessage::DeleteFile { session_id, .. }
        | ClientMessage::OfferFileTo { session_id, .. }
        | ClientMessage::DeclineFileOffer { session_id, .. } => session_id,
    }
}

// ---------------------------------------------------------------------------
// Message dispatcher
// ---------------------------------------------------------------------------

async fn handle_client_message(
    msg: ClientMessage,
    state: &NodeState,
    conn: &mut Connection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // A connection acts as exactly one device: the one it joined as. Without
    // this, any tab could send messages in another tab's name (delete its
    // files, answer its downloads, ...) just by writing its session id.
    if !matches!(msg, ClientMessage::Join { .. }) {
        match &conn.session_id {
            None => return Err("send join first".into()),
            Some(bound) if bound != claimed_session(&msg) => return Err("session id does not match this connection".into()),
            Some(_) => {}
        }
    }
    let peer_tx = conn.tx.clone();
    let bound = conn.session_id.clone().unwrap_or_default();

    match msg {
        // ── Join ─────────────────────────────────────────────────────────
        ClientMessage::Join { session_id: id, user_agent, nickname } => {
            if !validate::is_valid_id(&id) {
                return Err("invalid session id".into());
            }
            if conn.session_id.as_ref().is_some_and(|bound| *bound != id) {
                return Err("this connection already joined as another device".into());
            }
            {
                // The same login (or, on an open node, anyone) may rejoin a
                // known id, e.g. after a reconnect; a different login may not.
                let mut owners = state.session_owners.write().await;
                if let Some(owner) = owners.get(&id) {
                    let same_login = owner.auth_id == conn.auth_id;
                    if owner.conn_id != conn.id && !same_login {
                        return Err("that device id is already in use".into());
                    }
                }
                owners.insert(id.clone(), ConnOwner { conn_id: conn.id, auth_id: conn.auth_id.clone() });
            }
            conn.session_id = Some(id.clone());
            { let mut s = state.local_senders.write().await; s.insert(id.clone(), peer_tx.clone()); }

            let peer = PeerInfo {
                session_id: id.clone(),
                connected_at: chrono::Utc::now(),
                user_agent: user_agent.map(|ua| validate::clean_label(&ua, validate::MAX_USER_AGENT_CHARS)),
                hosting_node_id: Some(state.node_id.clone()),
                node_rtt_ms: None,
                left: false,
                left_at: None,
                hosting_node_name: Some(state.node_name.clone()),
                nickname: nickname.map(cap_nickname).filter(|n| !n.is_empty()),
                version: state.clock.now(),
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

        // ── F5: nickname change after join ─────────────────────────────────
        ClientMessage::SetNickname { session_id: id, nickname } => {
            let updated = {
                let mut peers = state.local_peers.write().await;
                match peers.get_mut(&id) {
                    Some(peer) => {
                        let nickname = cap_nickname(nickname);
                        peer.nickname = if nickname.is_empty() { None } else { Some(nickname) };
                        // A new version makes this update win the merge on
                        // other mesh nodes.
                        peer.version = state.clock.now();
                        Some(peer.clone())
                    }
                    None => None,
                }
            };
            if let Some(peer) = updated {
                broadcast(state, ServerMessage::PeerSync { peers: vec![peer.clone()] }).await;
                state::push_peer_to_mesh(&state.mesh_peers, peer).await;
            }
        }

        // ── File upload ───────────────────────────────────────────────────
        // The tab only says what the file is; the uploader, host list and
        // timestamps are ours to fill in, so a tab can't claim someone else's
        // file, list other devices as hosts, or backdate an entry.
        //
        // BUG-07 fix: a browser tab resends file_upload for every file it
        // holds after every WS reconnect (see app.js connectWebSocket()),
        // including files it downloaded from others. An id we already know
        // is a re-registration, not a new file: it only adds this device as a
        // host instead of replacing the entry, which would wipe out the other
        // hosts and any checksum computed since.
        ClientMessage::FileUpload { session_id: _, file: request } => {
            let request = validate::file_request(request)?;
            let version = state.clock.now();
            let to_mesh: Option<FileMetadata> = {
                let mut files = state.files.write().await;
                match files.get_mut(&request.id) {
                    Some(existing) if existing.deleted && existing.uploader_id != bound => None,
                    Some(existing) => {
                        existing.hosts.insert(bound.clone());
                        if existing.uploader_id == bound {
                            if request.sha256.is_some() {
                                existing.sha256 = request.sha256;
                            }
                            // Only the one who shared it may share it again after unsharing.
                            existing.deleted = false;
                            existing.deleted_at = 0;
                        }
                        existing.version = version;
                        Some(existing.clone())
                    }
                    None => {
                        state::evict_old_tombstones(&mut files, state::MAX_CATALOG_ENTRIES);
                        if files.len() >= state::MAX_CATALOG_ENTRIES {
                            return Err("The shared file list is full".into());
                        }
                        let shared_by_session = files.values().filter(|f| !f.deleted && f.uploader_id == bound).count();
                        if shared_by_session >= MAX_FILES_PER_SESSION {
                            return Err("This device is sharing too many files".into());
                        }
                        let entry = FileMetadata {
                            id: request.id.clone(),
                            name: request.name,
                            size: request.size,
                            mime_type: request.mime_type,
                            uploader_id: bound.clone(),
                            hosts: [bound.clone()].into(),
                            uploaded_at: chrono::Utc::now(),
                            created_at: version.wall,
                            deleted: false,
                            deleted_at: 0,
                            version,
                            sha256: request.sha256,
                            is_folder: request.is_folder,
                        };
                        files.insert(entry.id.clone(), entry.clone());
                        Some(entry)
                    }
                }
            };

            let files: Vec<FileMetadata> = {
                let files = state.files.read().await;
                files.values().filter(|f| !f.deleted).cloned().collect()
            };
            broadcast(state, ServerMessage::FileListUpdate { files }).await;
            if let Some(file) = to_mesh {
                state::push_file_to_mesh(&state.mesh_peers, file).await;
            }
        }

        // ── Phase 6: Client-side host selection ──────────────────────────
        // Client explicitly names the peer it wants to download from.
        // Node honors the choice without override.  Returns HostUnreachable
        // if that peer is not present in the merged peer list.
        ClientMessage::RequestDownloadFrom { session_id: requester_id, file_id, host_peer_id, resume_from_bytes } => {
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
                    resume_from_bytes,
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
                        resume_from_bytes: None, // legacy path has no host choice to resume against
                    },
                ).await;
            }
        }

        // ── File downloaded → register new host ──────────────────────────
        ClientMessage::FileDownloaded { session_id: _, file_id } => {
            let updated_file = {
                let mut files = state.files.write().await;
                match files.get_mut(&file_id) {
                    Some(file) if !file.deleted => {
                        if file.hosts.insert(bound.clone()) {
                            file.version = state.clock.now();
                            Some(file.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
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
            let content = validate::clean_message(&content).ok_or("message is empty")?;
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
                messages.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
                state::prune_messages(&mut messages); // BUG-13 fix
            }
            broadcast(state, ServerMessage::TextMessage { message: message.clone() }).await;
            state::push_message_to_mesh(&state.mesh_peers, message).await;
        }

        // ── Phase 11.3: checksum update ───────────────────────────────────
        // Only the uploader computes the checksum, and receivers trust it to
        // verify downloads, so nobody else may change it.
        ClientMessage::FileChecksumUpdate { session_id: _, file_id, sha256 } => {
            if !validate::is_valid_sha256(&sha256) {
                return Err("invalid checksum".into());
            }
            let patched: Option<FileMetadata> = {
                let mut files = state.files.write().await;
                match files.get_mut(&file_id) {
                    Some(file) if file.uploader_id == bound && !file.deleted => {
                        file.sha256 = Some(sha256.clone());
                        file.version = state.clock.now();
                        Some(file.clone())
                    }
                    Some(_) => {
                        tracing::warn!("FileChecksumUpdate: {bound} is not the uploader of {file_id}");
                        None
                    }
                    None => None,
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
                tracing::warn!("FileChecksumUpdate: ignored for {file_id}");
            }
        }

        // ── F4: unshare / delete ────────────────────────────────────────
        ClientMessage::DeleteFile { session_id: requester_id, file_id } => {
            let tombstoned: Option<FileMetadata> = {
                let mut files = state.files.write().await;
                match files.get_mut(&file_id) {
                    Some(file) if file.deleted => None, // already gone
                    Some(file) if file.uploader_id != requester_id => {
                        tracing::warn!(
                            "DeleteFile: {requester_id} tried to delete {file_id}, owned by {}",
                            file.uploader_id
                        );
                        None
                    }
                    Some(file) => {
                        file.version = state.clock.now();
                        file.deleted = true;
                        file.deleted_at = file.version.wall;
                        Some(file.clone())
                    }
                    None => None,
                }
            };

            if let Some(file) = tombstoned {
                let files: Vec<FileMetadata> = {
                    let files = state.files.read().await;
                    files.values().filter(|f| !f.deleted).cloned().collect()
                };
                broadcast(state, ServerMessage::FileListUpdate { files }).await;
                broadcast(state, ServerMessage::FileRemoved { file_id: file.id.clone() }).await;
                state::push_file_to_mesh(&state.mesh_peers, file).await;
            } else {
                send_to(state, &requester_id, ServerMessage::Error {
                    message: "Could not delete that file — it may not exist, already be removed, or not be yours".to_string(),
                }).await;
            }
        }

        // ── F3: send-to-person ──────────────────────────────────────────
        // Push a file directly to one peer instead of publishing it to the
        // catalog for anyone to find. Reuses the existing signal-routing
        // path (route_signal already handles same-node vs cross-node
        // delivery) — the target just gets a consent prompt instead of a
        // browsable catalog entry.
        ClientMessage::OfferFileTo { session_id: from, target_session_id, file_id } => {
            mesh::route_signal(
                state, &from, &target_session_id,
                ServerMessage::IncomingFileOffer { file_id, from_session_id: from.clone() },
            ).await;
        }

        ClientMessage::DeclineFileOffer { session_id: from, target_session_id, file_id } => {
            mesh::route_signal(
                state, &from, &target_session_id,
                ServerMessage::FileOfferDeclined { file_id, from_session_id: from.clone() },
            ).await;
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

    // Every file this device hosted loses a host; those left with none are
    // tombstoned. All of them get a new version so other nodes hear about it.
    let mut changed_files: Vec<FileMetadata> = Vec::new();
    {
        let mut files = state.files.write().await;
        for file in files.values_mut() {
            if !file.hosts.remove(session_id) { continue; }
            file.version = state.clock.now();
            if file.hosts.is_empty() && !file.deleted {
                file.deleted = true;
                file.deleted_at = file.version.wall;
            }
            changed_files.push(file.clone());
        }
    }

    broadcast(state, ServerMessage::PeerLeft { session_id: session_id.clone(), total_peers: peers_count }).await;

    let files: Vec<FileMetadata> = {
        let files = state.files.read().await;
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    broadcast(state, ServerMessage::FileListUpdate { files }).await;

    state::push_files_to_mesh(&state.mesh_peers, changed_files).await;
    state::push_peer_left_to_mesh(&state.mesh_peers, session_id.clone(), state.clock.now()).await;
}
