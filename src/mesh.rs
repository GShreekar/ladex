// ============================================================================
// LADEX — Phase 3 + Phase 4: Mesh WebSocket Layer & State Dispatch
//
// Owns all node-to-node WebSocket logic:
//   - Accepting inbound /mesh connections (Phase 3)
//   - Initiating outbound connections to discovered/manual peers (Phase 3)
//   - Full-state sync immediately after HelloAck (Phase 4)
//   - Incremental state dispatch for CatalogSync/PeerSync/ChatSync/ChatMessage
//   - Removing peers from mesh_peers on disconnect
//
// Architectural invariants:
//
//   • /mesh and /ws are SEPARATE routes.  Browser tabs → /ws (ServerMessage).
//     Peer nodes → /mesh (MeshMessage).  Never mix them.
//
//   • Deduplication: smaller node_id always initiates.
//     Duplicate inbound connections → HelloAck { accepted: false, reason: "duplicate" }.
//
//   • Post-HelloAck, both sides immediately exchange full state:
//     CatalogSync + PeerSync + ChatSync.  After that, only deltas are sent.
//
//   • All MeshMessage variants are defined here in full so callers never need
//     to change their match arms in later phases.  Phase 4/5 stubs are no-ops
//     that log and forward to state.rs helpers.
// ============================================================================

use crate::types::*;
use crate::{state, NodeState};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/// Increment on any incompatible wire-format change.
/// Nodes reject connections whose protocol_version != PROTOCOL_VERSION.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// MeshPeerHandle
// ---------------------------------------------------------------------------

/// A live connection to one other node on the mesh.
///
/// Stored in `NodeState::mesh_peers` keyed by `node_id`.
/// The `sender` half of an unbounded mpsc channel drives the outgoing WS
/// write task without needing a lock on the socket.
#[derive(Debug, Clone)]
pub struct MeshPeerHandle {
    /// Stable identifier for the remote node (from its Hello message).
    pub node_id: NodeId,
    /// Human-readable label (hostname) of the remote node.
    pub node_name: String,
    /// Remote IP address (as seen from this node).
    pub addr: SocketAddr,
    /// HTTP port the remote node is listening on (for reconnection, Phase 10).
    pub http_port: u16,
    /// Send a MeshMessage to this peer.
    pub sender: mpsc::UnboundedSender<MeshMessage>,
    /// Updated by Ping/Pong (Phase 10) and by multicast announces (Phase 2).
    pub last_seen: Instant,
}

/// Shared map of active mesh peer handles keyed by node_id.
pub type MeshPeers = Arc<RwLock<HashMap<NodeId, MeshPeerHandle>>>;

// ---------------------------------------------------------------------------
// MeshMessage — node-to-node wire protocol
// ---------------------------------------------------------------------------

/// All node-to-node messages share this tagged enum.
///
/// Phases:
///   3  — Hello, HelloAck, Ping, Pong
///   4  — CatalogSync, PeerSync, ChatSync, ChatMessage
///   5  — SignalRelay
///   10 — Goodbye
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshMessage {
    // ── Phase 3 — handshake & keepalive ─────────────────────────────────

    /// Sent immediately after connecting.  Initiator sends first.
    Hello {
        node_id: NodeId,
        node_name: String,
        protocol_version: u32,
        /// SHA-1 of passphrase (pre-filter, NOT a security boundary).
        /// Empty string when no passphrase configured.
        passphrase_hash: String,
    },

    /// Response to Hello.
    HelloAck {
        accepted: bool,
        reason: Option<String>,
        /// Responder's node_id — allows the initiator to key the handle correctly.
        node_id: NodeId,
        node_name: String,
    },

    /// Heartbeat (Phase 10).  `ts` is Unix-ms so RTT can be measured.
    Ping { ts: u64 },

    /// Echo of Ping — same `ts` returned verbatim.
    Pong { ts: u64 },

    // ── Phase 4 — state synchronisation ─────────────────────────────────

    /// Full-catalog or incremental catalog update.
    CatalogSync { files: Vec<FileMetadata> },

    /// Full-peer-list or incremental peer update.
    PeerSync { peers: Vec<PeerInfo> },

    /// Full chat history (on connect).
    ChatSync { messages: Vec<TextMessage> },

    /// Single new chat message (incremental).
    ChatMessage { message: TextMessage },

    // ── Phase 5 — WebRTC signaling relay ────────────────────────────────

    /// Route a WebRTC payload between browser tabs via the node mesh.
    SignalRelay {
        to_node_id: NodeId,
        from_node_id: NodeId,
        payload: serde_json::Value,
    },

    // ── Phase 10 — graceful shutdown ────────────────────────────────────

    /// Sent before clean shutdown so peers can skip the heartbeat timeout.
    Goodbye { node_id: NodeId },
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Remove a peer from mesh_peers and log the event.
/// Called by both the WS read loop (on disconnect) and discovery.rs (on stale).
pub async fn remove_mesh_peer(state: &NodeState, peer_node_id: &NodeId) {
    let removed = {
        let mut peers = state.mesh_peers.write().await;
        peers.remove(peer_node_id).is_some()
    };
    if removed {
        tracing::info!("Mesh: peer disconnected/stale: {peer_node_id}");
        // Phase 10: mark hosted browser peers offline, tombstone files
    }
}

// ---------------------------------------------------------------------------
// Warp handler — inbound /mesh (peer → this node)
// ---------------------------------------------------------------------------

/// Warp filter handler for `GET /mesh` (WebSocket upgrade).
pub async fn mesh_ws_handler(ws: Ws, state: NodeState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_inbound(socket, state)))
}

async fn handle_inbound(ws: WebSocket, state: NodeState) {
    let (ws_tx, ws_rx) = ws.split();

    // Wrap the sink in an Arc<Mutex> so we can share it between the handshake
    // and the write task without moving it prematurely.
    let ws_tx = Arc::new(tokio::sync::Mutex::new(ws_tx));
    let mut ws_rx = ws_rx;

    // ── Wait for Hello ───────────────────────────────────────────────────
    let hello = loop {
        match ws_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_str().unwrap_or("")) {
                    Ok(MeshMessage::Hello { .. }) => break msg,
                    Ok(other) => {
                        tracing::warn!("Mesh inbound: expected Hello, got {other:?} — ignoring");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("Mesh inbound: bad Hello JSON: {e}");
                        return;
                    }
                }
            }
            Some(Ok(msg)) if msg.is_close() => return,
            Some(Err(e)) => {
                tracing::warn!("Mesh inbound: WS error before Hello: {e}");
                return;
            }
            None => return,
            _ => continue,
        }
    };

    let (peer_node_id, peer_node_name, peer_protocol_version) =
        match serde_json::from_str::<MeshMessage>(hello.to_str().unwrap_or("")) {
            Ok(MeshMessage::Hello {
                node_id,
                node_name,
                protocol_version,
                ..
            }) => (node_id, node_name, protocol_version),
            _ => return,
        };

    // ── Validate ─────────────────────────────────────────────────────────

    let send_ack = |accepted: bool, reason: Option<String>| {
        let tx = ws_tx.clone();
        let node_id = state.node_id.clone();
        let node_name = hostname();
        async move {
            let ack = MeshMessage::HelloAck {
                accepted,
                reason,
                node_id,
                node_name,
            };
            let json = serde_json::to_string(&ack).unwrap_or_default();
            let _ = tx.lock().await.send(Message::text(json)).await;
        }
    };

    if peer_node_id == state.node_id {
        send_ack(false, Some("self-connection rejected".into())).await;
        return;
    }

    if peer_protocol_version != PROTOCOL_VERSION {
        send_ack(
            false,
            Some(format!(
                "protocol version mismatch: expected {PROTOCOL_VERSION}, got {peer_protocol_version}"
            )),
        )
        .await;
        return;
    }

    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        send_ack(false, Some("duplicate".into())).await;
        tracing::warn!("Mesh inbound: duplicate connection from {peer_node_id} — rejected");
        return;
    }

    // ── Accept ───────────────────────────────────────────────────────────
    send_ack(true, None).await;
    tracing::info!(
        "Mesh: inbound handshake complete — peer {peer_node_id} ({})",
        peer_node_name
    );

    // Use a placeholder addr (warp doesn't expose the remote addr without extra filter).
    // Phase 10 reconnection will use the addr from discovery.
    let placeholder_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let peer_http_port: u16 = 0; // inbound: we learn http_port from discovery, not the WS

    // ── Register + run ───────────────────────────────────────────────────
    // Unwrap the ws_tx Arc – we're the only holder now.
    let ws_tx_inner = Arc::try_unwrap(ws_tx)
        .unwrap_or_else(|_a| panic!("BUG: extra ws_tx holders"))
        .into_inner();

    run_connection(
        ws_tx_inner,
        ws_rx,
        peer_node_id,
        peer_node_name,
        placeholder_addr,
        peer_http_port,
        state,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Outbound connection — this node dials another
// ---------------------------------------------------------------------------

/// Initiate a mesh WebSocket connection to another node.
///
/// Called by:
///   - Phase 2 discovery listen_loop (on new peer)
///   - Phase 1 --peer CLI flag (on startup)
pub async fn connect_to_peer(
    addr: IpAddr,
    http_port: u16,
    state: NodeState,
) -> anyhow::Result<()> {
    let url = format!("ws://{addr}:{http_port}/mesh");
    tracing::info!("Mesh: dialing {url}");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| anyhow::anyhow!("WS connect to {url} failed: {e}"))?;

    let (mut tt_tx, mut tt_rx) = ws_stream.split();

    // ── Send Hello ───────────────────────────────────────────────────────
    let hello = MeshMessage::Hello {
        node_id: state.node_id.clone(),
        node_name: hostname(),
        protocol_version: PROTOCOL_VERSION,
        passphrase_hash: state.passphrase_hash.clone().unwrap_or_default(),
    };
    tt_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&hello)?,
        ))
        .await?;

    // ── Wait for HelloAck ────────────────────────────────────────────────
    let (peer_node_id, peer_node_name) = loop {
        match tt_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_text().unwrap_or("")) {
                    Ok(MeshMessage::HelloAck {
                        accepted: true,
                        node_id,
                        node_name,
                        ..
                    }) => {
                        tracing::info!("Mesh: {url} accepted Hello (peer: {node_id})");
                        break (node_id, node_name);
                    }
                    Ok(MeshMessage::HelloAck {
                        accepted: false,
                        reason,
                        ..
                    }) => {
                        return Err(anyhow::anyhow!(
                            "Peer {url} rejected Hello: {}",
                            reason.unwrap_or_default()
                        ));
                    }
                    Ok(other) => {
                        return Err(anyhow::anyhow!(
                            "Peer {url} sent {other:?} instead of HelloAck"
                        ));
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("Bad HelloAck JSON from {url}: {e}"));
                    }
                }
            }
            Some(Ok(msg)) if msg.is_close() => {
                return Err(anyhow::anyhow!("Peer {url} closed during handshake"));
            }
            Some(Err(e)) => return Err(anyhow::anyhow!("WS error from {url}: {e}")),
            None => return Err(anyhow::anyhow!("Peer {url} disconnected during handshake")),
            _ => continue,
        }
    };

    // Check for duplicate after handshake (race window between connect and HelloAck)
    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        tracing::warn!("Mesh: duplicate after handshake with {peer_node_id} — dropping");
        return Ok(());
    }

    let remote_addr: SocketAddr = format!("{addr}:{http_port}").parse().unwrap_or_else(|_| {
        format!("0.0.0.0:{http_port}").parse().unwrap()
    });

    // ── Register peer & run loops ─────────────────────────────────────────
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();

    {
        let mut peers = state.mesh_peers.write().await;
        peers.insert(
            peer_node_id.clone(),
            MeshPeerHandle {
                node_id: peer_node_id.clone(),
                node_name: peer_node_name.clone(),
                addr: remote_addr,
                http_port,
                sender: peer_tx.clone(),
                last_seen: Instant::now(),
            },
        );
    }
    tracing::info!("Mesh: registered peer {peer_node_id} ({peer_node_name})");

    // ── Full-state sync (Phase 4) ─────────────────────────────────────────
    post_handshake_sync(&peer_tx, &state).await;

    // ── Write task: mpsc → tokio-tungstenite WS ───────────────────────────
    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if tt_tx
                .send(tokio_tungstenite::tungstenite::Message::Text(json))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // ── Read task: tokio-tungstenite WS → dispatch ────────────────────────
    let state_rd = state.clone();
    let peer_id_rd = peer_node_id.clone();
    let read_task = tokio::spawn(async move {
        while let Some(result) = tt_rx.next().await {
            match result {
                Ok(msg) if msg.is_text() => {
                    let text = msg.to_text().unwrap_or("");
                    match serde_json::from_str::<MeshMessage>(text) {
                        Ok(m) => dispatch(&m, &peer_id_rd, &state_rd).await,
                        Err(e) => {
                            tracing::warn!("Mesh outbound: bad JSON from {peer_id_rd}: {e}")
                        }
                    }
                }
                Ok(msg) if msg.is_close() => break,
                Err(e) => {
                    tracing::warn!("Mesh outbound: WS error from {peer_id_rd}: {e}");
                    break;
                }
                _ => {}
            }
        }
        remove_mesh_peer(&state_rd, &peer_id_rd).await;
    });

    // Detach both tasks (they run until the connection closes)
    tokio::spawn(async move {
        tokio::select! {
            _ = write_task => {}
            _ = read_task => {}
        }
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared read/write loop — used by the inbound path
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
        peers.insert(
            peer_node_id.clone(),
            MeshPeerHandle {
                node_id: peer_node_id.clone(),
                node_name: peer_node_name.clone(),
                addr,
                http_port,
                sender: peer_tx.clone(),
                last_seen: Instant::now(),
            },
        );
    }
    tracing::info!("Mesh: registered inbound peer {peer_node_id} ({peer_node_name})");

    // ── Full-state sync (Phase 4) ─────────────────────────────────────────
    post_handshake_sync(&peer_tx, &state).await;

    // ── Write task ────────────────────────────────────────────────────────
    let write_task = tokio::spawn(async move {
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

    // ── Read task (inline) ────────────────────────────────────────────────
    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) if msg.is_text() => {
                let text = msg.to_str().unwrap_or("");
                match serde_json::from_str::<MeshMessage>(text) {
                    Ok(m) => dispatch(&m, &peer_node_id, &state).await,
                    Err(e) => {
                        tracing::warn!("Mesh inbound: bad JSON from {peer_node_id}: {e}");
                    }
                }
            }
            Ok(msg) if msg.is_close() => break,
            Err(e) => {
                tracing::warn!("Mesh inbound: WS error from {peer_node_id}: {e}");
                break;
            }
            _ => {}
        }
    }

    write_task.abort();
    remove_mesh_peer(&state, &peer_node_id).await;
}

// ---------------------------------------------------------------------------
// Phase 4 — full-state sync immediately after HelloAck
// ---------------------------------------------------------------------------

/// Immediately after a successful Hello/HelloAck, both sides send their full
/// local state so the new peer can merge it.
async fn post_handshake_sync(
    peer_tx: &mpsc::UnboundedSender<MeshMessage>,
    state: &NodeState,
) {
    // Catalog (non-deleted files only — tombstones are internal bookkeeping)
    let files: Vec<FileMetadata> = {
        state
            .files
            .read()
            .await
            .values()
            .filter(|f| !f.deleted)
            .cloned()
            .collect()
    };
    let _ = peer_tx.send(MeshMessage::CatalogSync { files });

    // Peer list (only local peers — remote peers will supply their own)
    let peers: Vec<PeerInfo> = {
        state
            .local_peers
            .read()
            .await
            .values()
            .cloned()
            .collect()
    };
    let _ = peer_tx.send(MeshMessage::PeerSync { peers });

    // Chat history
    let messages: Vec<TextMessage> = {
        state.messages.read().await.clone()
    };
    let _ = peer_tx.send(MeshMessage::ChatSync { messages });
}

// ---------------------------------------------------------------------------
// Phase 4 — message dispatch
// ---------------------------------------------------------------------------

/// Handle a received MeshMessage.
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

        MeshMessage::Pong { ts } => {
            let rtt_ms = chrono::Utc::now().timestamp_millis() as u64 - ts;
            let mut peers = state.mesh_peers.write().await;
            if let Some(h) = peers.get_mut(from_node_id) {
                h.last_seen = Instant::now();
                tracing::debug!("Mesh Pong from {from_node_id}: RTT {rtt_ms}ms");
            }
        }

        // ── Phase 4: state sync ──────────────────────────────────────────
        MeshMessage::CatalogSync { files } => {
            tracing::debug!(
                "Mesh CatalogSync from {from_node_id}: {} file(s)",
                files.len()
            );
            state::apply_catalog_sync(state, files.clone()).await;
        }

        MeshMessage::PeerSync { peers } => {
            tracing::debug!(
                "Mesh PeerSync from {from_node_id}: {} peer(s)",
                peers.len()
            );
            state::apply_peer_sync(state, peers.clone()).await;
        }

        MeshMessage::ChatSync { messages } => {
            tracing::debug!(
                "Mesh ChatSync from {from_node_id}: {} message(s)",
                messages.len()
            );
            state::apply_chat_sync(state, messages.clone()).await;
        }

        MeshMessage::ChatMessage { message } => {
            tracing::debug!(
                "Mesh ChatMessage from {from_node_id}: {}",
                message.id
            );
            state::apply_chat_message(state, message.clone()).await;
        }

        // ── Phase 5: signaling relay (stub) ─────────────────────────────
        MeshMessage::SignalRelay {
            to_node_id,
            from_node_id: from,
            payload: _,
        } => {
            tracing::debug!(
                "Mesh SignalRelay {from} → {to_node_id} — Phase 5 routing pending"
            );
            // Phase 5:
            //   if to_node_id == state.node_id → deliver to local browser tab
            //   else → forward to mesh_peers[to_node_id].sender
        }

        // ── Phase 10: graceful goodbye ───────────────────────────────────
        MeshMessage::Goodbye { node_id } => {
            tracing::info!("Mesh: Goodbye from {node_id}");
            remove_mesh_peer(state, node_id).await;
        }

        // ── Shouldn't appear mid-session ─────────────────────────────────
        MeshMessage::Hello { .. } | MeshMessage::HelloAck { .. } => {
            tracing::warn!("Mesh: unexpected Hello/HelloAck mid-session from {from_node_id}");
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
