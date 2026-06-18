// ============================================================================
// LADEX — Phases 3/4/5/6/7: Mesh WebSocket Layer
//
// Phase 5: SignalRelay routing — deliver to local tab or forward one-hop.
// Phase 6: RTT tracking via Pong timestamps; update PeerInfo.node_rtt_ms
//           and push incremental PeerSync so browser tabs can rank hosts.
// Phase 7: Passphrase enforcement in Hello/HelloAck (second check after the
//           Phase 2 pre-filter).  Mismatched hashes → HelloAck { accepted:false }.
// ============================================================================

use crate::types::*;
use crate::{auth, state, NodeState};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// MeshPeerHandle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MeshPeerHandle {
    pub node_id:    NodeId,
    pub node_name:  String,
    pub addr:       SocketAddr,
    pub http_port:  u16,
    pub sender:     mpsc::UnboundedSender<MeshMessage>,
    pub last_seen:  Instant,
    /// Latest measured RTT to this peer (ms).  None until first Pong.
    /// Phase 6: exposed to browser tabs via PeerSync.node_rtt_ms.
    pub rtt_ms:     Option<u32>,
}

pub type MeshPeers = Arc<RwLock<HashMap<NodeId, MeshPeerHandle>>>;

// ---------------------------------------------------------------------------
// MeshMessage — wire protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshMessage {
    Hello {
        node_id:          NodeId,
        node_name:        String,
        protocol_version: u32,
        /// PBKDF2-SHA256 hex (Phase 7).  Empty string = no passphrase.
        passphrase_hash:  String,
    },
    HelloAck {
        accepted:  bool,
        reason:    Option<String>,
        node_id:   NodeId,
        node_name: String,
    },
    Ping { ts: u64 },
    Pong { ts: u64 },

    // Phase 4
    CatalogSync  { files:    Vec<FileMetadata> },
    PeerSync     { peers:    Vec<PeerInfo>     },
    ChatSync     { messages: Vec<TextMessage>  },
    ChatMessage  { message:  TextMessage       },

    // Phase 5 — SignalRelay
    /// Routes a WebRTC payload between browser tabs via the mesh node layer.
    /// `payload` is the raw ServerMessage JSON that the destination tab expects
    /// (webrtc_offer / webrtc_answer / ice_candidate).
    SignalRelay {
        to_node_id:   NodeId,
        from_node_id: NodeId,
        payload:      serde_json::Value,
    },

    // Phase 10
    Goodbye { node_id: NodeId },
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

pub async fn remove_mesh_peer(state: &NodeState, peer_node_id: &NodeId) {
    let removed = {
        let mut peers = state.mesh_peers.write().await;
        peers.remove(peer_node_id).is_some()
    };
    if removed {
        tracing::info!("Mesh: peer disconnected/stale: {peer_node_id}");
    }
}

// ---------------------------------------------------------------------------
// Inbound handler — /mesh route
// ---------------------------------------------------------------------------

pub async fn mesh_ws_handler(ws: Ws, state: NodeState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_inbound(socket, state)))
}

async fn handle_inbound(ws: WebSocket, state: NodeState) {
    let (ws_tx, ws_rx) = ws.split();
    let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));
    let mut ws_rx = ws_rx;

    // Wait for Hello
    let raw_hello = loop {
        match ws_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_str().unwrap_or("")) {
                    Ok(MeshMessage::Hello { .. }) => break msg,
                    Ok(_) => continue,
                    Err(e) => { tracing::warn!("Mesh inbound: bad Hello JSON: {e}"); return; }
                }
            }
            Some(Ok(msg)) if msg.is_close() => return,
            Some(Err(e)) => { tracing::warn!("Mesh inbound: WS error before Hello: {e}"); return; }
            None => return,
            _ => continue,
        }
    };

    let (peer_node_id, peer_node_name, peer_protocol_version, peer_passphrase_hash) =
        match serde_json::from_str::<MeshMessage>(raw_hello.to_str().unwrap_or("")) {
            Ok(MeshMessage::Hello { node_id, node_name, protocol_version, passphrase_hash }) =>
                (node_id, node_name, protocol_version, passphrase_hash),
            _ => return,
        };

    // Build send-ack closure
    let send_ack = |accepted: bool, reason: Option<String>| {
        let tx = ws_tx.clone();
        let node_id = state.node_id.clone();
        let node_name = hostname();
        async move {
            let ack = MeshMessage::HelloAck { accepted, reason, node_id, node_name };
            let json = serde_json::to_string(&ack).unwrap_or_default();
            let _ = tx.lock().await.send(Message::text(json)).await;
        }
    };

    // Validate
    if peer_node_id == state.node_id {
        send_ack(false, Some("self-connection rejected".into())).await;
        return;
    }
    if peer_protocol_version != PROTOCOL_VERSION {
        send_ack(false, Some(format!(
            "protocol version mismatch: expected {PROTOCOL_VERSION}, got {peer_protocol_version}"
        ))).await;
        return;
    }
    // Phase 7 check 2: passphrase enforcement
    let our_hash = state.passphrase_hash.as_deref().unwrap_or("");
    if !auth::hashes_match(our_hash, &peer_passphrase_hash) {
        send_ack(false, Some("wrong passphrase".into())).await;
        tracing::warn!("Mesh inbound: passphrase mismatch from {peer_node_id} — rejected");
        return;
    }
    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        send_ack(false, Some("duplicate".into())).await;
        return;
    }

    send_ack(true, None).await;
    tracing::info!("Mesh: inbound handshake OK — peer {peer_node_id} ({peer_node_name})");

    let placeholder_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let ws_tx_inner = Arc::try_unwrap(ws_tx)
        .unwrap_or_else(|_| panic!("BUG: extra ws_tx holders"))
        .into_inner();

    run_connection(ws_tx_inner, ws_rx, peer_node_id, peer_node_name, placeholder_addr, 0, state).await;
}

// ---------------------------------------------------------------------------
// Outbound — dial another node
// ---------------------------------------------------------------------------

pub async fn connect_to_peer(addr: IpAddr, http_port: u16, state: NodeState) -> anyhow::Result<()> {
    let url = format!("ws://{addr}:{http_port}/mesh");
    tracing::info!("Mesh: dialing {url}");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| anyhow::anyhow!("WS connect to {url} failed: {e}"))?;

    let (mut tt_tx, mut tt_rx) = ws_stream.split();

    // Phase 7: include passphrase_hash in Hello
    let hello = MeshMessage::Hello {
        node_id:          state.node_id.clone(),
        node_name:        hostname(),
        protocol_version: PROTOCOL_VERSION,
        passphrase_hash:  state.passphrase_hash.clone().unwrap_or_default(),
    };
    tt_tx.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&hello)?)).await?;

    let (peer_node_id, peer_node_name) = loop {
        match tt_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_text().unwrap_or("")) {
                    Ok(MeshMessage::HelloAck { accepted: true,  node_id, node_name, .. }) => {
                        tracing::info!("Mesh: {url} accepted Hello (peer: {node_id})");
                        break (node_id, node_name);
                    }
                    Ok(MeshMessage::HelloAck { accepted: false, reason, .. }) =>
                        return Err(anyhow::anyhow!("Peer {url} rejected Hello: {}", reason.unwrap_or_default())),
                    Ok(other) =>
                        return Err(anyhow::anyhow!("Peer {url} sent {other:?} instead of HelloAck")),
                    Err(e) =>
                        return Err(anyhow::anyhow!("Bad HelloAck JSON from {url}: {e}")),
                }
            }
            Some(Ok(msg)) if msg.is_close() => return Err(anyhow::anyhow!("Peer {url} closed during handshake")),
            Some(Err(e))                    => return Err(anyhow::anyhow!("WS error from {url}: {e}")),
            None                            => return Err(anyhow::anyhow!("Peer {url} disconnected during handshake")),
            _ => continue,
        }
    };

    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        tracing::warn!("Mesh: duplicate after handshake with {peer_node_id} — dropping");
        return Ok(());
    }

    let remote_addr: SocketAddr = format!("{addr}:{http_port}").parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());

    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();
    {
        let mut peers = state.mesh_peers.write().await;
        peers.insert(peer_node_id.clone(), MeshPeerHandle {
            node_id:   peer_node_id.clone(),
            node_name: peer_node_name.clone(),
            addr:      remote_addr,
            http_port,
            sender:    peer_tx.clone(),
            last_seen: Instant::now(),
            rtt_ms:    None,
        });
    }
    tracing::info!("Mesh: registered peer {peer_node_id} ({peer_node_name})");

    post_handshake_sync(&peer_tx, &state).await;

    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
            if tt_tx.send(tokio_tungstenite::tungstenite::Message::Text(json)).await.is_err() { break; }
        }
    });

    let state_rd  = state.clone();
    let peer_id_rd = peer_node_id.clone();
    let read_task = tokio::spawn(async move {
        while let Some(result) = tt_rx.next().await {
            match result {
                Ok(msg) if msg.is_text() => {
                    let text = msg.to_text().unwrap_or("");
                    match serde_json::from_str::<MeshMessage>(text) {
                        Ok(m)  => dispatch(&m, &peer_id_rd, &state_rd).await,
                        Err(e) => tracing::warn!("Mesh outbound: bad JSON from {peer_id_rd}: {e}"),
                    }
                }
                Ok(msg) if msg.is_close() => break,
                Err(e) => { tracing::warn!("Mesh outbound: WS error from {peer_id_rd}: {e}"); break; }
                _ => {}
            }
        }
        remove_mesh_peer(&state_rd, &peer_id_rd).await;
    });

    tokio::spawn(async move { tokio::select! { _ = write_task => {} _ = read_task => {} } });
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared connection runner — inbound path
// ---------------------------------------------------------------------------

async fn run_connection(
    mut ws_tx: futures_util::stream::SplitSink<WebSocket, Message>,
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    peer_node_id: NodeId,
    peer_node_name: String,
    addr: SocketAddr,
    http_port: u16,
    state: NodeState,
) {
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();
    {
        let mut peers = state.mesh_peers.write().await;
        peers.insert(peer_node_id.clone(), MeshPeerHandle {
            node_id:   peer_node_id.clone(),
            node_name: peer_node_name.clone(),
            addr,
            http_port,
            sender:    peer_tx.clone(),
            last_seen: Instant::now(),
            rtt_ms:    None,
        });
    }
    tracing::info!("Mesh: registered inbound peer {peer_node_id} ({peer_node_name})");

    post_handshake_sync(&peer_tx, &state).await;

    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
            if ws_tx.send(Message::text(json)).await.is_err() { break; }
        }
    });

    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) if msg.is_text() => {
                let text = msg.to_str().unwrap_or("");
                match serde_json::from_str::<MeshMessage>(text) {
                    Ok(m)  => dispatch(&m, &peer_node_id, &state).await,
                    Err(e) => tracing::warn!("Mesh inbound: bad JSON from {peer_node_id}: {e}"),
                }
            }
            Ok(msg) if msg.is_close() => break,
            Err(e) => { tracing::warn!("Mesh inbound: WS error from {peer_node_id}: {e}"); break; }
            _ => {}
        }
    }

    write_task.abort();
    remove_mesh_peer(&state, &peer_node_id).await;
}

// ---------------------------------------------------------------------------
// Full-state sync (Phase 4)
// ---------------------------------------------------------------------------

async fn post_handshake_sync(peer_tx: &mpsc::UnboundedSender<MeshMessage>, state: &NodeState) {
    let files: Vec<FileMetadata> = state.files.read().await
        .values().filter(|f| !f.deleted).cloned().collect();
    let _ = peer_tx.send(MeshMessage::CatalogSync { files });

    let peers: Vec<PeerInfo> = state.local_peers.read().await.values().cloned().collect();
    let _ = peer_tx.send(MeshMessage::PeerSync { peers });

    let messages: Vec<TextMessage> = state.messages.read().await.clone();
    let _ = peer_tx.send(MeshMessage::ChatSync { messages });
}

// ---------------------------------------------------------------------------
// Dispatch (Phases 3–7)
// ---------------------------------------------------------------------------

async fn dispatch(msg: &MeshMessage, from_node_id: &NodeId, state: &NodeState) {
    match msg {
        // ── Phase 3: keepalive ───────────────────────────────────────────
        MeshMessage::Ping { ts } => {
            let mut peers = state.mesh_peers.write().await;
            if let Some(h) = peers.get_mut(from_node_id) {
                h.last_seen = Instant::now();
                let _ = h.sender.send(MeshMessage::Pong { ts: *ts });
            }
        }

        // Phase 6: on Pong, measure RTT and push updated node_rtt_ms to all
        // local browser tabs via a PeerSync containing affected peers.
        MeshMessage::Pong { ts } => {
            let rtt_ms = (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(*ts) as u32;
            tracing::debug!("Mesh Pong from {from_node_id}: RTT {rtt_ms}ms");

            // Update the handle's rtt_ms
            {
                let mut peers = state.mesh_peers.write().await;
                if let Some(h) = peers.get_mut(from_node_id) {
                    h.last_seen = Instant::now();
                    h.rtt_ms = Some(rtt_ms);
                }
            }

            // Phase 6: update node_rtt_ms on every PeerInfo hosted by this node
            // and push incremental PeerSync to browser tabs.
            let updated_peers: Vec<PeerInfo> = {
                let mut local = state.local_peers.write().await;
                let mut updated = Vec::new();
                for peer in local.values_mut() {
                    if peer.hosting_node_id.as_deref() == Some(from_node_id) {
                        peer.node_rtt_ms = Some(rtt_ms);
                        updated.push(peer.clone());
                    }
                }
                updated
            };
            if !updated_peers.is_empty() {
                crate::websocket::broadcast(
                    state,
                    ServerMessage::PeerSync { peers: updated_peers },
                ).await;
            }
        }

        // ── Phase 4: state sync ──────────────────────────────────────────
        MeshMessage::CatalogSync { files } => {
            tracing::debug!("Mesh CatalogSync from {from_node_id}: {} file(s)", files.len());
            state::apply_catalog_sync(state, files.clone()).await;
        }
        MeshMessage::PeerSync { peers } => {
            tracing::debug!("Mesh PeerSync from {from_node_id}: {} peer(s)", peers.len());
            state::apply_peer_sync(state, peers.clone()).await;
        }
        MeshMessage::ChatSync { messages } => {
            tracing::debug!("Mesh ChatSync from {from_node_id}: {} message(s)", messages.len());
            state::apply_chat_sync(state, messages.clone()).await;
        }
        MeshMessage::ChatMessage { message } => {
            tracing::debug!("Mesh ChatMessage from {from_node_id}: {}", message.id);
            state::apply_chat_message(state, message.clone()).await;
        }

        // ── Phase 5: SignalRelay routing ─────────────────────────────────
        //
        // Invariant: only one hop — the mesh is full-mesh, so any node
        // can always reach any other node directly.  No multi-hop routing.
        //
        // Two cases:
        //   A) to_node_id == our node_id  → unwrap payload, deliver to the
        //      local browser tab identified by `to_session_id` inside payload.
        //   B) to_node_id != our node_id  → forward to mesh_peers[to_node_id].
        MeshMessage::SignalRelay { to_node_id, from_node_id: from, payload } => {
            if to_node_id == &state.node_id {
                // Deliver to local browser tab.
                // The payload IS a ServerMessage (webrtc_offer/answer/ice_candidate).
                // Extract the target session_id from the payload's from_session_id
                // field, then route via local_senders.
                //
                // The recipient tab is keyed by its own session_id, NOT from_session_id.
                // We use the `to_session_id` field we embed when building SignalRelay
                // (see websocket.rs route_signal_to_mesh).
                let to_session_id = payload
                    .get("to_session_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(sid) = to_session_id {
                    let senders = state.local_senders.read().await;
                    if let Some(sender) = senders.get(&sid) {
                        // Deliver the inner ServerMessage JSON as-is
                        // by deserializing from the payload value.
                        match serde_json::from_value::<ServerMessage>(payload.clone()) {
                            Ok(srv_msg) => { let _ = sender.send(srv_msg); }
                            Err(e) => tracing::warn!("SignalRelay: bad payload for tab {sid}: {e}"),
                        }
                    } else {
                        tracing::warn!("SignalRelay: no local tab {sid}");
                    }
                } else {
                    tracing::warn!("SignalRelay to us but payload has no to_session_id");
                }
            } else {
                // Forward one hop to the target node
                let peers = state.mesh_peers.read().await;
                if let Some(peer) = peers.get(to_node_id) {
                    let _ = peer.sender.send(MeshMessage::SignalRelay {
                        to_node_id:   to_node_id.clone(),
                        from_node_id: from.clone(),
                        payload:      payload.clone(),
                    });
                } else {
                    tracing::warn!("SignalRelay: no mesh peer {to_node_id} — signal dropped");
                }
            }
        }

        // ── Phase 10: graceful goodbye ───────────────────────────────────
        MeshMessage::Goodbye { node_id } => {
            tracing::info!("Mesh: Goodbye from {node_id}");
            remove_mesh_peer(state, node_id).await;
        }

        MeshMessage::Hello { .. } | MeshMessage::HelloAck { .. } => {
            tracing::warn!("Mesh: unexpected Hello/HelloAck mid-session from {from_node_id}");
        }
    }
}

// ---------------------------------------------------------------------------
// Signal routing helper — called from websocket.rs (Phase 5)
// ---------------------------------------------------------------------------

/// Route a WebRTC signaling payload from a local browser tab to another tab
/// that may be on a different node.
///
/// `from_session_id`: the sender tab's session id.
/// `to_session_id`:   the intended recipient tab's session id.
/// `srv_msg`:         the ServerMessage to deliver (WebRTCOffer/Answer/ICE).
///
/// Routing logic:
///   1. Look up `to_session_id` in `state.local_peers` to find its `hosting_node_id`.
///   2. If `hosting_node_id == state.node_id` → deliver locally via `local_senders`.
///   3. Otherwise → wrap in `MeshMessage::SignalRelay` and send to that node.
pub async fn route_signal(
    state: &NodeState,
    _from_session_id: &str,
    to_session_id: &str,
    srv_msg: ServerMessage,
) {
    // Find hosting node for target session
    let hosting_node_id: Option<NodeId> = {
        let peers = state.local_peers.read().await;
        peers.get(to_session_id).and_then(|p| p.hosting_node_id.clone())
    };

    match hosting_node_id {
        // Same node — direct local delivery
        Some(ref hid) if hid == &state.node_id => {
            let senders = state.local_senders.read().await;
            if let Some(sender) = senders.get(to_session_id) {
                let _ = sender.send(srv_msg);
            } else {
                tracing::warn!("route_signal: no local sender for {to_session_id}");
            }
        }

        // Remote node — wrap in SignalRelay
        Some(target_node_id) => {
            // Embed to_session_id inside payload so the remote node's dispatch()
            // can deliver to the right browser tab.
            let mut payload = serde_json::to_value(&srv_msg).unwrap_or_default();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("to_session_id".into(), serde_json::Value::String(to_session_id.to_string()));
            }

            let peers = state.mesh_peers.read().await;
            if let Some(peer) = peers.get(&target_node_id) {
                let _ = peer.sender.send(MeshMessage::SignalRelay {
                    to_node_id:   target_node_id,
                    from_node_id: state.node_id.clone(),
                    payload,
                });
            } else {
                tracing::warn!("route_signal: no mesh peer {target_node_id} for tab {to_session_id}");
            }
        }

        // Target session unknown — may not have synced yet
        None => {
            tracing::warn!("route_signal: unknown hosting node for tab {to_session_id} — trying local fallback");
            // Fallback: deliver locally (handles single-node mode gracefully)
            let senders = state.local_senders.read().await;
            if let Some(sender) = senders.get(to_session_id) {
                let _ = sender.send(srv_msg);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            use std::process::Command;
            Command::new("hostname")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .unwrap_or_else(|_| "LADEX Node".to_string())
}
