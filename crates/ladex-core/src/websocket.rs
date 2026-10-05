//! The browser-tab WebSocket (/ws): joins, chat, file actions and signaling.

use crate::server::PeerAddr;
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

// Tabs only send small JSON control messages, so anything near this size isn't a real client.
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;

/// Who holds a browser `session_id` on this node, so another connection can't act as that device.
#[derive(Debug, Clone)]
pub struct ConnOwner {
    pub conn_id: u64,
    /// Login session of the owning connection; None on a node without a passphrase.
    pub auth_id: Option<String>,
}

struct Connection {
    id: u64,
    /// The request came from the machine running this node.
    is_host: bool,
    auth_id: Option<String>,
    /// The `session_id` this connection joined as; every later message must carry it.
    session_id: Option<SessionId>,
    tx: PeerSender,
}

pub async fn websocket_handler(
    auth: Option<SessionHandle>,
    peer: Option<PeerAddr>,
    ws: Ws,
    state: NodeState,
) -> Result<impl Reply, Rejection> {
    let is_host = peer.is_some_and(|p| p.addr.ip().is_loopback());
    let ws = ws.max_message_size(MAX_WS_MESSAGE_BYTES).max_frame_size(MAX_WS_MESSAGE_BYTES);
    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state, auth, is_host)))
}

fn cap_nickname(nickname: String) -> String {
    validate::clean_label(&nickname, validate::MAX_NICKNAME_CHARS)
}

pub async fn broadcast(state: &NodeState, msg: ServerMessage) {
    let senders = state.local_senders.read().await;
    for sender in senders.values() {
        let _ = sender.send(msg.clone());
    }
}

/// Sends a message to every local tab.
pub async fn broadcast_all(state: &NodeState, msg: ServerMessage) {
    broadcast(state, msg).await;
}

pub async fn send_to(state: &NodeState, target: &SessionId, msg: ServerMessage) {
    let senders = state.local_senders.read().await;
    if let Some(sender) = senders.get(target) {
        let _ = sender.send(msg);
    }
}

pub async fn handle_websocket(ws: WebSocket, state: NodeState, auth: Option<SessionHandle>, is_host: bool) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut conn = Connection {
        id: state.next_connection_id(),
        is_host,
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
                    break;
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
        tokio::time::sleep(Duration::from_millis(150)).await;
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
        | ClientMessage::Ping { session_id }
        | ClientMessage::TextMessage { session_id, .. }
        | ClientMessage::DeleteFile { session_id, .. }
        | ClientMessage::OfferFileTo { session_id, .. }
        | ClientMessage::DeclineFileOffer { session_id, .. } => session_id,
    }
}

async fn handle_client_message(
    msg: ClientMessage,
    state: &NodeState,
    conn: &mut Connection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // A connection acts only as the device it joined as, or any tab could act in another's name.
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
        ClientMessage::Join { session_id: id, user_agent, nickname } => {
            if !validate::is_valid_id(&id) {
                return Err("invalid session id".into());
            }
            if conn.session_id.as_ref().is_some_and(|bound| *bound != id) {
                return Err("this connection already joined as another device".into());
            }
            {
                // The same login (or anyone, on an open node) may rejoin a known id; a different login may not.
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

            let files = state::live_files(&*state.files.read().await);
            let _ = peer_tx.send(ServerMessage::FileListUpdate { files });

            let messages = state.messages.read().await.clone();
            if !messages.is_empty() {
                let _ = peer_tx.send(ServerMessage::MessageHistory { messages });
            }

            broadcast(state, ServerMessage::PeerJoined { peer: peer.clone(), total_peers: peers_count }).await;
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }

        ClientMessage::SetNickname { session_id: id, nickname } => {
            let updated = {
                let mut peers = state.local_peers.write().await;
                match peers.get_mut(&id) {
                    Some(peer) => {
                        let nickname = cap_nickname(nickname);
                        peer.nickname = if nickname.is_empty() { None } else { Some(nickname) };
                        // A new version makes this update win the merge on other nodes.
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

        ClientMessage::Ping { session_id: _ } => {
            let _ = peer_tx.send(ServerMessage::Pong);
        }

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
                state::prune_messages(&mut messages);
            }
            broadcast(state, ServerMessage::TextMessage { message: message.clone() }).await;
            state::push_message_to_mesh(&state.mesh_peers, message).await;
        }

        // The sharing device may unshare a file, and so may the node's own machine; a folder takes its files with it.
        ClientMessage::DeleteFile { session_id: _, file_id } => {
            let tombstones: Vec<FileMetadata> = {
                let mut files = state.files.write().await;
                let allowed = files.get(&file_id).is_some_and(|f| {
                    !f.deleted && (f.uploader_id == bound || (conn.is_host && f.uploader_node == state.node_id))
                });
                if !allowed {
                    Vec::new()
                } else {
                    let ids: Vec<String> = files
                        .values()
                        .filter(|f| !f.deleted && (f.id == file_id || f.parent.as_deref() == Some(file_id.as_str())))
                        .map(|f| f.id.clone())
                        .collect();
                    let mut tombstones = Vec::new();
                    for id in ids {
                        if let Some(file) = files.get_mut(&id) {
                            file.version = state.clock.now();
                            file.deleted = true;
                            file.deleted_at = file.version.wall;
                            tombstones.push(file.clone());
                        }
                    }
                    tombstones
                }
            };

            if tombstones.is_empty() {
                send_to(state, &bound, ServerMessage::Error {
                    message: "Could not delete that file — it may not exist, already be removed, or not be yours".to_string(),
                }).await;
            } else {
                for tombstone in &tombstones {
                    state::forget_file(state, &tombstone.id).await;
                }
                state::broadcast_catalog(state).await;
                broadcast(state, ServerMessage::FileRemoved { file_id: file_id.clone() }).await;
                state::push_files_to_mesh(&state.mesh_peers, tombstones).await;
            }
        }

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

async fn cleanup_peer(state: &NodeState, session_id: &SessionId) {
    let peers_count = {
        let mut peers = state.local_peers.write().await;
        peers.remove(session_id);
        peers.len()
    };
    // Files live on the node, so closing the tab that shared them leaves them shared.
    broadcast(state, ServerMessage::PeerLeft { session_id: session_id.clone(), total_peers: peers_count }).await;
    state::push_peer_left_to_mesh(&state.mesh_peers, session_id.clone(), state.clock.now()).await;
}
