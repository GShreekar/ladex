// ============================================================================
// LADEX — Phase 3: Mesh WebSocket Layer
//
// This module owns all node-to-node WebSocket logic:
//   - Initiating outbound connections to discovered/manual peers
//   - Accepting inbound connections on the /mesh HTTP endpoint
//   - Sending and receiving MeshMessage frames
//   - Removing peers from the mesh on disconnect
//
// Design notes (from ROADMAP.md §3):
//
//   • /mesh and /ws are SEPARATE routes.  Browser tabs connect to /ws.
//     Peer nodes connect to /mesh.  They have different message protocols
//     and different lifecycle semantics.  Do not mix them.
//
//   • Deduplication: the node with the lexicographically smaller node_id
//     always initiates.  The larger-id node waits 200ms then skips if a
//     connection already exists.  Duplicate inbound connections are rejected
//     with HelloAck { accepted: false, reason: Some("duplicate") }.
//
//   • MeshMessage envelope: all node-to-node messages share one tagged enum.
//     Phase 3 uses Hello/HelloAck/Ping/Pong.  Phase 4 adds Catalog/Peer/Chat
//     sync variants.  Phase 5 adds SignalRelay.  The enum is defined here in
//     full so callers don't need to change their match arms in later phases.
//
//   • State sync (CatalogSync, PeerSync, ChatSync) and signaling relay
//     (SignalRelay) are defined here for completeness.  Their *handling logic*
//     lives in Phase 4 and Phase 5 respectively.  In Phase 3, receiving those
//     variants is a no-op (logged and ignored) so the binary still compiles
//     and runs correctly.
// ============================================================================

use crate::types::*;
use crate::NodeState;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

// ---------------------------------------------------------------------------
// Protocol version — increment on incompatible message format changes.
// Nodes reject connections whose protocol_version != PROTOCOL_VERSION.
// ---------------------------------------------------------------------------
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// MeshPeerHandle — stored in NodeState::mesh_peers keyed by node_id
// ---------------------------------------------------------------------------

/// A live connection to another node on the mesh.
///
/// The `sender` half of an unbounded mpsc channel drives the write side of the
/// WebSocket from any async context without needing to lock the socket.
#[derive(Debug, Clone)]
pub struct MeshPeerHandle {
    pub node_id:   NodeId,
    pub node_name: String,
    pub http_port: u16,
    /// Send a MeshMessage to this peer.
    pub sender:    mpsc::UnboundedSender<MeshMessage>,
    /// Updated by the Ping/Pong heartbeat (Phase 10) and by multicast
    /// announces (Phase 2).  Not actively used in Phase 3.
    pub last_seen: Instant,
}

/// Shared map of mesh peers keyed by node_id.
pub type MeshPeers = Arc<RwLock<HashMap<NodeId, MeshPeerHandle>>>;

// ---------------------------------------------------------------------------
// MeshMessage — the node-to-node wire protocol envelope
// ---------------------------------------------------------------------------

/// All node-to-node messages share this envelope.
/// The `type` field (via `#[serde(tag)]`) determines which variant to deserialize.
///
/// Phase 3: Hello, HelloAck, Ping, Pong
/// Phase 4: CatalogSync, PeerSync, ChatSync, ChatMessage
/// Phase 5: SignalRelay
/// Phase 10: Goodbye
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshMessage {
    // ── Phase 3 — handshake & keepalive ─────────────────────────────────

    /// Sent immediately after connecting.  Initiator sends first.
    Hello {
        node_id:          NodeId,
        node_name:        String,
        protocol_version: u32,
        /// SHA-1 hex of the passphrase (Phase 7 pre-filter, not security boundary).
        /// Empty string when no passphrase is configured.
        passphrase_hash:  String,
    },

    /// Response to Hello.
    HelloAck {
        accepted: bool,
        reason:   Option<String>,
    },

    /// Sent every 5 seconds (Phase 10 heartbeat).
    /// `ts` is Unix milliseconds — used to measure RTT for Phase 6.
    Ping { ts: u64 },

    /// Response to Ping — echo the same `ts` back.
    Pong { ts: u64 },

    // ── Phase 4 — state synchronisation ─────────────────────────────────

    /// Full or incremental file catalog.
    /// On connect: send all local files.  Incrementally: send only the new entry.
    CatalogSync { files: Vec<FileMetadata> },

    /// Full or incremental peer list.
    PeerSync { peers: Vec<PeerInfo> },

    /// Full chat history (on connect).
    ChatSync { messages: Vec<TextMessage> },

    /// A single new chat message (broadcast incrementally).
    ChatMessage { message: TextMessage },

    // ── Phase 5 — WebRTC signaling relay ────────────────────────────────

    /// Route a WebRTC signaling payload (offer/answer/ICE) between nodes.
    /// The node that hosts `to_node_id`'s browser tab delivers it locally.
    /// All other nodes forward it one hop toward that node (full mesh → always
    /// one hop away).
    SignalRelay {
        to_node_id:   NodeId,
        from_node_id: NodeId,
        payload:      serde_json::Value,
    },

    // ── Phase 10 — graceful shutdown ────────────────────────────────────

    /// Sent before a clean shutdown so peers skip the heartbeat timeout.
    Goodbye { node_id: NodeId },
}

// ---------------------------------------------------------------------------
// Warp handler — inbound /mesh connections (peer → this node)
// ---------------------------------------------------------------------------

/// Warp filter handler for `GET /mesh` (WebSocket upgrade).
///
/// Called when another node dials us.  The Hello/HelloAck roles are reversed
/// from outbound: the *inbound* peer sends Hello; we respond with HelloAck.
pub async fn mesh_ws_handler(ws: Ws, state: NodeState) -> Result<impl Reply, Rejection> {
    Ok(ws.on_upgrade(move |socket| handle_inbound_mesh_ws(socket, state)))
}

async fn handle_inbound_mesh_ws(ws: WebSocket, state: NodeState) {
    let (mut ws_tx, mut ws_rx) = ws.split();

    // ── Wait for Hello ───────────────────────────────────────────────────
    let hello = loop {
        match ws_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_str().unwrap_or("")) {
                    Ok(MeshMessage::Hello { .. }) => break msg,
                    Ok(other) => {
                        tracing::warn!("Mesh inbound: expected Hello, got {:?} — ignoring", other);
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
                tracing::warn!("Mesh inbound WS error before Hello: {e}");
                return;
            }
            None => return,
            _ => continue,
        }
    };

    let hello_parsed: MeshMessage = match serde_json::from_str(hello.to_str().unwrap_or("")) {
        Ok(m) => m,
        Err(_) => return,
    };

    let (peer_node_id, peer_node_name, peer_protocol_version) = match hello_parsed {
        MeshMessage::Hello { node_id, node_name, protocol_version, .. } => {
            (node_id, node_name, protocol_version)
        }
        _ => return,
    };

    // ── Validate ─────────────────────────────────────────────────────────

    // Reject own node_id (shouldn't happen with multicast_loop=false, but
    // guard against it in case of loopback testing with --peer).
    if peer_node_id == state.node_id {
        let ack = MeshMessage::HelloAck {
            accepted: false,
            reason:   Some("self-connection rejected".to_string()),
        };
        let _ = ws_tx.send(Message::text(serde_json::to_string(&ack).unwrap())).await;
        return;
    }

    // Reject incompatible protocol version.
    if peer_protocol_version != PROTOCOL_VERSION {
        let ack = MeshMessage::HelloAck {
            accepted: false,
            reason:   Some(format!(
                "protocol version mismatch: expected {PROTOCOL_VERSION}, got {peer_protocol_version}"
            )),
        };
        let _ = ws_tx.send(Message::text(serde_json::to_string(&ack).unwrap())).await;
        return;
    }

    // Reject duplicate connection (deduplication: smaller node_id initiates).
    {
        let peers = state.mesh_peers.read().await;
        if peers.contains_key(&peer_node_id) {
            let ack = MeshMessage::HelloAck {
                accepted: false,
                reason:   Some("duplicate".to_string()),
            };
            let _ = ws_tx.send(Message::text(serde_json::to_string(&ack).unwrap())).await;
            tracing::warn!("Mesh inbound: duplicate connection from {peer_node_id} — rejected");
            return;
        }
    }

    // ── Accept ───────────────────────────────────────────────────────────
    let ack = MeshMessage::HelloAck { accepted: true, reason: None };
    if ws_tx.send(Message::text(serde_json::to_string(&ack).unwrap())).await.is_err() {
        return;
    }

    tracing::info!("Mesh peer connected (inbound): {peer_node_id} ({})", peer_node_name);

    // ── Register & run ───────────────────────────────────────────────────
    run_mesh_connection(ws_tx, ws_rx, peer_node_id, peer_node_name, 0, state).await;
}

// ---------------------------------------------------------------------------
// Outbound connection — this node dials a discovered/manual peer
// ---------------------------------------------------------------------------

/// Initiate a mesh WebSocket connection from this node to another node.
///
/// Called by:
///   - Phase 2 discovery callback (not yet implemented — stub in main.rs)
///   - Phase 1 --peer manual override (wired in main.rs startup)
///
/// Implements the deduplication tie-break: if our node_id is lexicographically
/// greater than the peer's, we wait 200ms and skip if a connection appeared.
pub async fn connect_to_peer(
    addr: IpAddr,
    http_port: u16,
    state: NodeState,
) -> anyhow::Result<()> {
    let url = format!("ws://{addr}:{http_port}/mesh");

    tracing::info!("Mesh outbound: dialing {url}");

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await
        .map_err(|e| anyhow::anyhow!("WS connect to {url} failed: {e}"))?;

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // ── Send Hello ───────────────────────────────────────────────────────
    let hello = MeshMessage::Hello {
        node_id:          state.node_id.clone(),
        node_name:        hostname(),
        protocol_version: PROTOCOL_VERSION,
        passphrase_hash:  String::new(), // Phase 7: derive from passphrase_hash
    };
    ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&hello)?,
    )).await?;

    // ── Wait for HelloAck ────────────────────────────────────────────────
    let ack: MeshMessage = loop {
        match ws_rx.next().await {
            Some(Ok(msg)) if msg.is_text() => {
                match serde_json::from_str::<MeshMessage>(msg.to_text().unwrap_or("")) {
                    Ok(m) => break m,
                    Err(e) => return Err(anyhow::anyhow!("Bad HelloAck JSON from {url}: {e}")),
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

    match ack {
        MeshMessage::HelloAck { accepted: true, .. } => {
            tracing::info!("Mesh outbound: {url} accepted Hello");
        }
        MeshMessage::HelloAck { accepted: false, reason } => {
            return Err(anyhow::anyhow!(
                "Peer {url} rejected Hello: {}",
                reason.unwrap_or_default()
            ));
        }
        other => {
            return Err(anyhow::anyhow!(
                "Peer {url} sent unexpected message instead of HelloAck: {other:?}"
            ));
        }
    }

    // ── Convert tokio-tungstenite streams to warp-compatible streams ─────
    // We need to bridge from tokio_tungstenite into our run_mesh_connection
    // helper which works on raw text/binary message iterators.
    //
    // Rather than duplicating run_mesh_connection, we drive the read/write
    // loops directly here using tokio_tungstenite's native types.

    // Create the per-peer mpsc channel
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();

    // We need the peer's node_id to register in mesh_peers, but we don't have
    // it from the outbound side — the peer told us nothing beyond HelloAck.
    // In a full implementation, the HelloAck would carry the peer's node_id.
    // For Phase 3, derive a stable key from the remote address.
    let peer_node_id = format!("outbound_peer_{addr}_{http_port}");
    let peer_node_name = format!("{addr}:{http_port}");

    // Register in mesh_peers
    {
        let mut peers = state.mesh_peers.write().await;
        peers.insert(peer_node_id.clone(), MeshPeerHandle {
            node_id:   peer_node_id.clone(),
            node_name: peer_node_name.clone(),
            http_port,
            sender:    peer_tx,
            last_seen: Instant::now(),
        });
    }
    tracing::info!("Mesh peer registered (outbound): {peer_node_id}");

    // ── Write task: drain mpsc → WS ──────────────────────────────────────
    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    // ── Read task: WS → dispatch ─────────────────────────────────────────
    let state_clone = state.clone();
    let peer_id_clone = peer_node_id.clone();
    let read_task = tokio::spawn(async move {
        while let Some(result) = ws_rx.next().await {
            match result {
                Ok(msg) if msg.is_text() => {
                    let text = msg.to_text().unwrap_or("");
                    match serde_json::from_str::<MeshMessage>(text) {
                        Ok(m) => dispatch_mesh_message(m, &peer_id_clone, &state_clone).await,
                        Err(e) => tracing::warn!("Mesh outbound: bad message JSON from {peer_id_clone}: {e}"),
                    }
                }
                Ok(msg) if msg.is_close() => break,
                Err(e) => {
                    tracing::warn!("Mesh outbound WS error from {peer_id_clone}: {e}");
                    break;
                }
                _ => {}
            }
        }
        // Peer disconnected — remove from mesh
        cleanup_mesh_peer(&state_clone, &peer_id_clone).await;
    });

    drop(write_task); // detached
    drop(read_task);  // detached

    Ok(())
}

// ---------------------------------------------------------------------------
// run_mesh_connection — shared read/write loop (inbound path)
//
// Called after a successful Hello/HelloAck on the inbound side.
// The outbound path drives its own loops directly (see connect_to_peer).
// ---------------------------------------------------------------------------

async fn run_mesh_connection(
    mut ws_tx: futures_util::stream::SplitSink<WebSocket, warp::ws::Message>,
    mut ws_rx: futures_util::stream::SplitStream<WebSocket>,
    peer_node_id: NodeId,
    peer_node_name: String,
    http_port: u16,
    state: NodeState,
) {
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();

    // Register in mesh_peers
    {
        let mut peers = state.mesh_peers.write().await;
        peers.insert(peer_node_id.clone(), MeshPeerHandle {
            node_id:   peer_node_id.clone(),
            node_name: peer_node_name.clone(),
            http_port,
            sender:    peer_tx,
            last_seen: Instant::now(),
        });
    }

    tracing::info!("Mesh peer registered (inbound): {peer_node_id} ({})", peer_node_name);

    // Write task: drain mpsc → WS
    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(_) => continue,
            };
            if ws_tx.send(warp::ws::Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    // Read task: WS → dispatch
    while let Some(result) = ws_rx.next().await {
        match result {
            Ok(msg) if msg.is_text() => {
                let text = msg.to_str().unwrap_or("");
                match serde_json::from_str::<MeshMessage>(text) {
                    Ok(m) => dispatch_mesh_message(m, &peer_node_id, &state).await,
                    Err(e) => {
                        tracing::warn!("Mesh inbound: bad message JSON from {peer_node_id}: {e}");
                    }
                }
            }
            Ok(msg) if msg.is_close() => break,
            Err(e) => {
                tracing::warn!("Mesh inbound WS error from {peer_node_id}: {e}");
                break;
            }
            _ => {}
        }
    }

    write_task.abort();
    cleanup_mesh_peer(&state, &peer_node_id).await;
}

// ---------------------------------------------------------------------------
// Message dispatch — handles received MeshMessages
// ---------------------------------------------------------------------------

async fn dispatch_mesh_message(msg: MeshMessage, from_node_id: &NodeId, state: &NodeState) {
    match msg {
        // ── Ping → Pong ──────────────────────────────────────────────────
        MeshMessage::Ping { ts } => {
            // Update last_seen
            if let Some(peer) = state.mesh_peers.write().await.get_mut(from_node_id) {
                peer.last_seen = Instant::now();
                let _ = peer.sender.send(MeshMessage::Pong { ts });
            }
        }

        MeshMessage::Pong { ts } => {
            // Update last_seen; Phase 10 heartbeat will use this for timeout detection.
            // RTT = now_ms - ts — exposed to browser in Phase 6.
            let rtt_ms = chrono::Utc::now().timestamp_millis() as u64 - ts;
            if let Some(peer) = state.mesh_peers.write().await.get_mut(from_node_id) {
                peer.last_seen = Instant::now();
                tracing::debug!("Mesh Pong from {from_node_id}: RTT {rtt_ms}ms");
            }
        }

        // ── Phase 4 handlers (stubs — no-op until Phase 4 lands) ─────────
        MeshMessage::CatalogSync { files } => {
            tracing::debug!(
                "Mesh CatalogSync from {from_node_id}: {} file(s) — Phase 4 merge pending",
                files.len()
            );
            // Phase 4: call merge_files(state, files).await here
        }

        MeshMessage::PeerSync { peers } => {
            tracing::debug!(
                "Mesh PeerSync from {from_node_id}: {} peer(s) — Phase 4 merge pending",
                peers.len()
            );
            // Phase 4: call merge_peers(state, peers).await here
        }

        MeshMessage::ChatSync { messages } => {
            tracing::debug!(
                "Mesh ChatSync from {from_node_id}: {} message(s) — Phase 4 merge pending",
                messages.len()
            );
            // Phase 4: call merge_messages(state, messages).await here
        }

        MeshMessage::ChatMessage { message } => {
            tracing::debug!(
                "Mesh ChatMessage from {from_node_id}: {} — Phase 4 pending",
                message.id
            );
            // Phase 4: append to state.messages, fanout to local browser tabs
        }

        // ── Phase 5 handler (stub — no-op until Phase 5 lands) ───────────
        MeshMessage::SignalRelay { to_node_id, from_node_id: from, payload: _ } => {
            tracing::debug!(
                "Mesh SignalRelay {from} → {to_node_id} — Phase 5 routing pending"
            );
            // Phase 5:
            //   if to_node_id == state.node_id → deliver to local browser tab
            //   else → forward to mesh_peers[to_node_id].sender
        }

        // ── Phase 10 handler (stub) ───────────────────────────────────────
        MeshMessage::Goodbye { node_id } => {
            tracing::info!("Mesh Goodbye from {node_id} — cleaning up immediately");
            cleanup_mesh_peer(state, &node_id).await;
        }

        // Already handled during handshake — shouldn't appear mid-session
        MeshMessage::Hello { .. } | MeshMessage::HelloAck { .. } => {
            tracing::warn!("Mesh: unexpected Hello/HelloAck mid-session from {from_node_id}");
        }
    }
}

// ---------------------------------------------------------------------------
// Cleanup — called on any disconnect (graceful or otherwise)
// ---------------------------------------------------------------------------

async fn cleanup_mesh_peer(state: &NodeState, peer_node_id: &NodeId) {
    let removed = {
        let mut peers = state.mesh_peers.write().await;
        peers.remove(peer_node_id).is_some()
    };
    if removed {
        tracing::info!("Mesh peer disconnected: {peer_node_id}");
        // Phase 10: mark hosted browser peers offline, tombstone their files
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Returns a human-readable name for this node.
/// Uses the system hostname if available; falls back to a generic label.
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
