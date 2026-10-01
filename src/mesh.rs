// ============================================================================
// LADEX — Phases 3/4/5/6/7/10: Mesh WebSocket Layer
//
// Phase 5:  SignalRelay routing — deliver to local tab or forward one-hop.
// Phase 6:  RTT tracking via Pong timestamps; push PeerSync to browser tabs.
// Phase 7:  Passphrase-authenticated handshake (SPAKE2 + TLS channel binding,
//           see auth.rs): Hello -> HelloAck -> HelloConfirm.
// Phase 10: Heartbeat Ping/Pong (5s interval, 15s dead-peer timeout),
//           exponential reconnect backoff (2/4/8/30s, give-up at 10 min),
//           graceful Goodbye on shutdown, state cleanup on departure.
// ============================================================================

use crate::types::*;
use crate::auth::{Pake, Role, Transcript};
use crate::server::{peer_ip, PeerAddr};
use crate::{state, NodeState};

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

// v2: the passphrase hash is gone from Hello and discovery; peers authenticate
// with a SPAKE2 exchange instead.
pub const PROTOCOL_VERSION: u32 = 2;

// Phase 10 timing constants
const HEARTBEAT_INTERVAL:  Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT:   Duration = Duration::from_secs(15);
const RECONNECT_GIVE_UP:   Duration = Duration::from_secs(600); // 10 min
const HANDSHAKE_TIMEOUT:   Duration = Duration::from_secs(10);

// Rejection reasons the dialing side treats as authentication failures and
// backs off from, rather than retrying every few seconds.
const REASON_RATE_LIMITED: &str = "too many failed attempts";
const REASON_SECURITY_MODE: &str = "passphrase mismatch: one side is secured and the other is not";

/// The peer could not be authenticated (wrong passphrase, mismatched security
/// mode, a possible man in the middle, or we are being rate limited). Retrying
/// immediately will not help.
#[derive(Debug)]
pub struct AuthFailure(pub String);

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthFailure {}

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
        /// Whether the sender requires a passphrase. Secured and open
        /// meshes never mix.
        secured:          bool,
        /// Hex SPAKE2 message (client side).
        pake:             String,
        /// BUG-10 fix: the sender's own HTTP/mesh listening port. An
        /// inbound connection only reveals the *ephemeral* source port of
        /// the TCP connection the peer dialed with — not the port its own
        /// server listens on — so without this, the accepting side has no
        /// address to reconnect to if the connection later drops, and
        /// spent up to RECONNECT_GIVE_UP retrying 0.0.0.0:0.
        /// `#[serde(default)]` so an old peer that predates this field
        /// degrades to the previous (broken) behavior instead of failing
        /// the handshake outright.
        #[serde(default)]
        http_port: u16,
        /// BUG-10 fix: the sender's own best-guess LAN IPv4 (empty string
        /// if unknown), self-reported for the same reason as `http_port`
        /// — and, with TLS enabled, for the additional reason that the
        /// accepting side only ever sees connections arriving from our own
        /// local TLS-terminating proxy (127.0.0.1), never the real peer,
        /// so the TCP-level remote address can't be trusted here either.
        #[serde(default)]
        ip: String,
    },
    HelloAck {
        accepted:  bool,
        reason:    Option<String>,
        node_id:   NodeId,
        node_name: String,
        /// Hex SPAKE2 message (server side); empty when rejected.
        #[serde(default)]
        pake:      String,
        /// Hex server proof over the handshake transcript; empty when rejected.
        #[serde(default)]
        proof:     String,
    },
    /// Client's proof that it derived the same key; the server registers the
    /// peer only after verifying it.
    HelloConfirm { proof: String },
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

/// Remove a peer from mesh_peers and run Phase 10 state cleanup.
pub async fn remove_mesh_peer(state: &NodeState, peer_node_id: &NodeId) {
    let removed = {
        let mut peers = state.mesh_peers.write().await;
        peers.remove(peer_node_id).is_some()
    };
    if removed {
        tracing::info!("Mesh: peer disconnected: {peer_node_id}");
        peer_departed_cleanup(state, peer_node_id).await;
    }
}

/// Phase 10 §10.2 — mark hosted peers offline and tombstone their files.
/// Does NOT delete catalog entries; marks them unavailable so the UI shows
/// "[offline]" rather than silently hiding the file.
async fn peer_departed_cleanup(state: &NodeState, departed_node_id: &NodeId) {
    // Mark browser peers hosted by the departed node as offline.
    // BUG-08 fix: also set left/left_at, not just hosting_node_id = None —
    // this list gets pushed to the rest of the mesh below, and without an
    // LWW timestamp newer than the connected_at these peers joined with,
    // merge_peers on every other node would just discard the update (same
    // "MIN_UTC never wins" class of bug that push_peer_left_to_mesh had).
    let departed_peers: Vec<PeerInfo> = {
        let mut local = state.local_peers.write().await;
        let mut changed = Vec::new();
        let now = chrono::Utc::now();
        for peer in local.values_mut() {
            if peer.hosting_node_id.as_deref() == Some(departed_node_id) {
                peer.hosting_node_id = None; // None = offline
                peer.node_rtt_ms = None;
                peer.left = true;
                peer.left_at = Some(now);
                changed.push(peer.clone());
            }
        }
        changed
    };
    if !departed_peers.is_empty() {
        crate::websocket::broadcast(
            state,
            ServerMessage::PeerSync { peers: departed_peers.clone() },
        ).await;
        // Propagate to the rest of the mesh too — otherwise a third node
        // only ever learns about this departure if it happens to be the
        // one that detects the disconnect itself.
        for peer in departed_peers {
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }
    }

    // Tombstone files whose only remaining host session belongs to the departed node.
    // We can't perfectly know which sessions belonged to which node here (sessions
    // are purged on WS disconnect), but we mark files whose host set is now empty.
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let tombstoned: Vec<FileMetadata> = {
        let mut files = state.files.write().await;
        let mut ts = Vec::new();
        for file in files.values_mut() {
            if file.deleted { continue; }
            // If every host session is now gone from local_peers, tombstone.
            // (In a real multi-node scenario sessions for the departed node
            //  were already cleaned up by cleanup_peer in websocket.rs.)
            if file.hosts.is_empty() {
                file.deleted = true;
                file.deleted_at = now_ms;
                ts.push(file.clone());
            }
        }
        ts
    };
    for ts_file in tombstoned {
        state::push_file_to_mesh(&state.mesh_peers, ts_file).await;
    }

    // Refresh local tab file list
    let files: Vec<FileMetadata> = {
        let files = state.files.read().await;
        files.values().filter(|f| !f.deleted).cloned().collect()
    };
    crate::websocket::broadcast(state, ServerMessage::FileListUpdate { files }).await;
}

/// Phase 10 §10.3 — Reconnect with exponential backoff.
/// Attempts: 2s, 4s, 8s, 30s, 30s, … until RECONNECT_GIVE_UP (10 min).
/// Stops early if multicast discovery already re-established the connection.
///
/// BUG-10 fix: guard against the residual case where `addr`/`http_port`
/// couldn't be determined at all (see handle_inbound's fallback) — dialing
/// 0.0.0.0:0 can never succeed, so don't burn the whole 10-minute give-up
/// window finding that out.
pub fn spawn_reconnect(addr: IpAddr, http_port: u16, state: NodeState, peer_node_id: NodeId) {
    if addr.is_unspecified() || http_port == 0 {
        tracing::debug!("Reconnect: no dialable address for {peer_node_id} — not attempting");
        return;
    }
    tokio::spawn(async move {
        let started = Instant::now();
        let delays_secs = [2u64, 4, 8, 30];
        let mut attempt = 0usize;
        loop {
            if started.elapsed() > RECONNECT_GIVE_UP {
                tracing::info!("Reconnect: giving up on {peer_node_id} after 10 min");
                return;
            }
            // Stop if already reconnected (discovery may have beaten us)
            if state.mesh_peers.read().await.contains_key(&peer_node_id) {
                return;
            }
            let delay_secs = if attempt < delays_secs.len() {
                delays_secs[attempt]
            } else {
                30
            };
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            attempt += 1;
            if state.mesh_peers.read().await.contains_key(&peer_node_id) {
                return;
            }
            tracing::info!("Reconnect: attempt {attempt} to {addr}:{http_port} ({peer_node_id})");
            match connect_to_peer(addr, http_port, state.clone()).await {
                Ok(()) => { tracing::info!("Reconnect: success to {peer_node_id}"); return; }
                Err(e) if e.downcast_ref::<AuthFailure>().is_some() => {
                    tracing::warn!("Reconnect: giving up on {peer_node_id}: {e}");
                    return;
                }
                Err(e) => tracing::warn!("Reconnect: failed: {e}"),
            }
        }
    });
}

/// Phase 10 §10.5 — Graceful shutdown: send Goodbye to all mesh peers.
pub async fn broadcast_goodbye(state: &NodeState) {
    let peers = state.mesh_peers.read().await;
    let goodbye = MeshMessage::Goodbye { node_id: state.node_id.clone() };
    for handle in peers.values() {
        let _ = handle.sender.send(goodbye.clone());
    }
    // Brief yield so write tasks can flush before the process exits
    drop(peers);
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Phase 10 §10.1 — Spawn a heartbeat task for one peer connection.
/// Sends Ping every HEARTBEAT_INTERVAL; if no Pong for HEARTBEAT_TIMEOUT,
/// removes the peer and optionally spawns reconnect.
pub fn spawn_heartbeat(
    state: NodeState,
    peer_node_id: NodeId,
    peer_addr: IpAddr,
    http_port: u16,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        // the loop body sleeps longer than one interval period; avoid firing
        // the missed ticks back-to-back once it catches up
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // skip first immediate tick
        loop {
            interval.tick().await;
            // Check if this peer is still in mesh_peers
            let still_connected = {
                let peers = state.mesh_peers.read().await;
                peers.contains_key(&peer_node_id)
            };
            if !still_connected { return; }

            // Send Ping
            let ts = chrono::Utc::now().timestamp_millis() as u64;
            {
                let peers = state.mesh_peers.read().await;
                if let Some(h) = peers.get(&peer_node_id) {
                    let _ = h.sender.send(MeshMessage::Ping { ts });
                }
            }

            // Wait for up to HEARTBEAT_TIMEOUT for a Pong (last_seen update)
            tokio::time::sleep(HEARTBEAT_TIMEOUT).await;

            let timed_out = {
                let peers = state.mesh_peers.read().await;
                peers.get(&peer_node_id)
                    .map(|h| h.last_seen.elapsed() > HEARTBEAT_TIMEOUT)
                    .unwrap_or(false)
            };
            if timed_out {
                tracing::warn!("Heartbeat: peer {peer_node_id} timed out — removing");
                remove_mesh_peer(&state, &peer_node_id).await;
                // Attempt to reconnect
                spawn_reconnect(peer_addr, http_port, state.clone(), peer_node_id.clone());
                return;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Inbound handler — /mesh route
// ---------------------------------------------------------------------------

pub async fn mesh_ws_handler(ws: Ws, peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    let remote_ip = peer_ip(peer);
    Ok(ws.on_upgrade(move |socket| handle_inbound(socket, remote_ip, state)))
}

/// Next text message from the peer, or None on timeout, close, error or bad JSON.
async fn recv_handshake_message(rx: &mut SplitStream<WebSocket>) -> Option<MeshMessage> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match rx.next().await? {
                Ok(msg) if msg.is_text() => return serde_json::from_str(msg.to_str().unwrap_or("")).ok(),
                Ok(msg) if msg.is_close() => return None,
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

async fn handle_inbound(ws: WebSocket, remote_ip: IpAddr, state: NodeState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let our_node_name = hostname();

    async fn send_ack(
        ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        state: &NodeState,
        node_name: String,
        outcome: Result<(String, String), String>,
    ) {
        let (accepted, reason, pake, proof) = match outcome {
            Ok((pake, proof)) => (true, None, pake, proof),
            Err(reason) => (false, Some(reason), String::new(), String::new()),
        };
        let ack = MeshMessage::HelloAck { accepted, reason, node_id: state.node_id.clone(), node_name, pake, proof };
        let json = serde_json::to_string(&ack).unwrap_or_default();
        let _ = ws_tx.send(Message::text(json)).await;
    }

    // Every connection counts as a passphrase guess until it proves otherwise.
    let ticket = match state.mesh_limiter.begin(remote_ip) {
        Ok(ticket) => ticket,
        Err(retry_after) => {
            tracing::warn!("Mesh inbound: {remote_ip} is locked out for another {}s", retry_after.as_secs());
            send_ack(&mut ws_tx, &state, our_node_name, Err(REASON_RATE_LIMITED.into())).await;
            return;
        }
    };

    let (peer_node_id, peer_node_name, peer_protocol_version, peer_secured, peer_pake, peer_http_port, peer_ip) =
        match recv_handshake_message(&mut ws_rx).await {
            Some(MeshMessage::Hello { node_id, node_name, protocol_version, secured, pake, http_port, ip }) =>
                (node_id, node_name, protocol_version, secured, pake, http_port, ip),
            _ => return,
        };

    if peer_node_id == state.node_id {
        send_ack(&mut ws_tx, &state, our_node_name, Err("self-connection rejected".into())).await;
        return;
    }
    if peer_protocol_version != PROTOCOL_VERSION {
        send_ack(&mut ws_tx, &state, our_node_name, Err(format!(
            "protocol version mismatch: expected {PROTOCOL_VERSION}, got {peer_protocol_version}"
        ))).await;
        return;
    }
    if peer_secured != state.passphrase.is_some() {
        send_ack(&mut ws_tx, &state, our_node_name, Err(REASON_SECURITY_MODE.into())).await;
        return;
    }
    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        send_ack(&mut ws_tx, &state, our_node_name, Err("duplicate".into())).await;
        return;
    }

    let pake = Pake::start(Role::Server, state.passphrase.as_deref().unwrap_or(""));
    let server_pake = pake.message().to_vec();
    let client_pake = hex::decode(&peer_pake).unwrap_or_default();
    let Some(key) = pake.finish(&client_pake) else {
        send_ack(&mut ws_tx, &state, our_node_name, Err("malformed handshake message".into())).await;
        return;
    };
    let transcript = Transcript::new(&peer_node_id, &state.node_id, &client_pake, &server_pake, &state.tls_fingerprint);

    let ack = (hex::encode(&server_pake), hex::encode(key.prove(Role::Server, &transcript)));
    send_ack(&mut ws_tx, &state, our_node_name, Ok(ack)).await;

    // Registering the peer waits for its proof; a wrong passphrase ends here
    // and stays counted against the IP.
    let proven = match recv_handshake_message(&mut ws_rx).await {
        Some(MeshMessage::HelloConfirm { proof }) => {
            hex::decode(&proof).is_ok_and(|proof| key.verify(Role::Client, &transcript, &proof))
        }
        _ => false,
    };
    if !proven {
        tracing::warn!("Mesh inbound: {peer_node_id} at {remote_ip} failed to prove the passphrase — rejected");
        return;
    }
    state.mesh_limiter.succeed(remote_ip, ticket);

    // The peer may have connected to us in the meantime, from its own dial.
    if state.mesh_peers.read().await.contains_key(&peer_node_id) {
        return;
    }
    tracing::info!("Mesh: inbound handshake OK — peer {peer_node_id} ({peer_node_name})");

    // BUG-10 fix: build a real, dialable address for this peer from its
    // self-reported ip/http_port (Hello), so that if the connection later
    // drops, the heartbeat's reconnect attempt has somewhere real to dial
    // instead of the old "0.0.0.0:0" placeholder (which burned the whole
    // 10-minute give-up window failing to connect). warp 0.4.1 exposes no
    // way to read the TCP-level remote address at all (see the route
    // definition in main.rs), so self-reporting is the only option here —
    // conveniently, it's also the only thing that works once TLS is
    // enabled, since every inbound connection warp sees then actually
    // comes from our own local TLS-terminating proxy at 127.0.0.1, not the
    // real peer.
    let addr: SocketAddr = match (peer_ip.parse::<IpAddr>().ok(), peer_http_port) {
        (Some(ip), port) if port > 0 => SocketAddr::new(ip, port),
        _ => {
            tracing::warn!(
                "Mesh inbound: no usable address for {peer_node_id} \
                 (peer_ip={peer_ip:?}, peer_http_port={peer_http_port}) — \
                 reconnect after disconnect will not be possible for this peer"
            );
            "0.0.0.0:0".parse().unwrap()
        }
    };
    run_connection(ws_tx, ws_rx, peer_node_id, peer_node_name, addr, peer_http_port, state).await;
}

// ---------------------------------------------------------------------------
// Outbound — dial another node
// ---------------------------------------------------------------------------

// BUG-03 fix: the public HTTP port speaks TLS-only once TLS is enabled (see
// src/tls.rs), so dialing a peer must go over wss:// in that case. The two
// paths (plain ws:// via tokio-tungstenite's own TCP connect, vs wss:// over
// our own rustls handshake) produce different concrete `WebSocketStream<S>`
// types, so both are boxed into the same trait objects here — the rest of
// this function only needs Sink<Message>/Stream<Item = Result<Message, _>>.
type MeshSink = std::pin::Pin<Box<dyn futures_util::Sink<
    tokio_tungstenite::tungstenite::Message,
    Error = tokio_tungstenite::tungstenite::Error,
> + Send>>;
type MeshSource = std::pin::Pin<Box<dyn futures_util::Stream<
    Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
> + Send>>;

pub async fn connect_to_peer(addr: IpAddr, http_port: u16, state: NodeState) -> anyhow::Result<()> {
    let tls_client_config = state.tls_client_config.clone();
    let url = format!("{}://{addr}:{http_port}/mesh", if tls_client_config.is_some() { "wss" } else { "ws" });
    tracing::info!("Mesh: dialing {url}");

    // The fingerprint stays empty without TLS (--no-tls); the handshake then
    // has nothing to bind to and a man in the middle can't be ruled out.
    let (mut tt_tx, mut tt_rx, server_fingerprint): (MeshSink, MeshSource, Vec<u8>) =
        if let Some(client_config) = tls_client_config {
            let (ws_stream, _, fingerprint) = crate::tls::connect_wss(addr, http_port, "/mesh", client_config)
                .await
                .map_err(|e| anyhow::anyhow!("WSS connect to {url} failed: {e}"))?;
            let (tx, rx) = ws_stream.split();
            (Box::pin(tx), Box::pin(rx), fingerprint)
        } else {
            let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|e| anyhow::anyhow!("WS connect to {url} failed: {e}"))?;
            let (tx, rx) = ws_stream.split();
            (Box::pin(tx), Box::pin(rx), Vec::new())
        };

    let pake = Pake::start(Role::Client, state.passphrase.as_deref().unwrap_or(""));
    let client_pake = pake.message().to_vec();
    // BUG-10 fix: include our own http_port so the peer can reconnect to us
    // if this connection later drops (see MeshMessage::Hello's doc comment).
    let hello = MeshMessage::Hello {
        node_id:          state.node_id.clone(),
        node_name:        hostname(),
        protocol_version: PROTOCOL_VERSION,
        secured:          state.passphrase.is_some(),
        pake:             hex::encode(&client_pake),
        http_port:        state.http_port,
        ip:               state.local_ip.map(|ip| ip.to_string()).unwrap_or_default(),
    };
    tt_tx.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&hello)?)).await?;

    let (peer_node_id, peer_node_name, server_pake, server_proof) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match tt_rx.next().await {
                Some(Ok(msg)) if msg.is_text() => {
                    return match serde_json::from_str::<MeshMessage>(msg.to_text().unwrap_or("")) {
                        Ok(MeshMessage::HelloAck { accepted: true, node_id, node_name, pake, proof, .. }) =>
                            Ok((node_id, node_name, pake, proof)),
                        Ok(MeshMessage::HelloAck { accepted: false, reason, .. }) => {
                            let reason = reason.unwrap_or_default();
                            let msg = format!("Peer {url} rejected Hello: {reason}");
                            if reason == REASON_RATE_LIMITED || reason == REASON_SECURITY_MODE {
                                Err(anyhow::Error::new(AuthFailure(msg)))
                            } else {
                                Err(anyhow::anyhow!(msg))
                            }
                        }
                        Ok(other) => Err(anyhow::anyhow!("Peer {url} sent {other:?} instead of HelloAck")),
                        Err(e) => Err(anyhow::anyhow!("Bad HelloAck JSON from {url}: {e}")),
                    };
                }
                Some(Ok(msg)) if msg.is_close() => return Err(anyhow::anyhow!("Peer {url} closed during handshake")),
                Some(Err(e))                    => return Err(anyhow::anyhow!("WS error from {url}: {e}")),
                None                            => return Err(anyhow::anyhow!("Peer {url} disconnected during handshake")),
                _ => continue,
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Peer {url} did not answer Hello in time"))??;

    // The server must prove it knows the passphrase over a transcript that
    // includes the certificate we actually connected to.
    let server_pake = hex::decode(&server_pake).unwrap_or_default();
    let transcript = Transcript::new(&state.node_id, &peer_node_id, &client_pake, &server_pake, &server_fingerprint);
    let server_proven = pake.finish(&server_pake).and_then(|key| {
        let proof = hex::decode(&server_proof).ok()?;
        key.verify(Role::Server, &transcript, &proof).then_some(key)
    });
    let Some(key) = server_proven else {
        return Err(anyhow::Error::new(AuthFailure(format!(
            "Peer {url} failed to prove the passphrase: wrong passphrase, or the connection is being intercepted"
        ))));
    };
    let confirm = MeshMessage::HelloConfirm { proof: hex::encode(key.prove(Role::Client, &transcript)) };
    tt_tx.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&confirm)?)).await?;
    tracing::info!("Mesh: {url} authenticated (peer: {peer_node_id})");

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

    // Phase 10: start heartbeat for this connection
    spawn_heartbeat(state.clone(), peer_node_id.clone(), addr, http_port);

    let write_task = tokio::spawn(async move {
        while let Some(msg) = peer_rx.recv().await {
            let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
            if tt_tx.send(tokio_tungstenite::tungstenite::Message::Text(json)).await.is_err() { break; }
        }
    });

    let state_rd   = state.clone();
    let peer_id_rd = peer_node_id.clone();
    let read_task  = tokio::spawn(async move {
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
        // Phase 10: only run cleanup if not already done by heartbeat
        let still_present = state_rd.mesh_peers.read().await.contains_key(&peer_id_rd);
        if still_present {
            remove_mesh_peer(&state_rd, &peer_id_rd).await;
            // Reconnect backoff — we know the addr/port from the handle stored before we lost it,
            // but the handle is now gone. The heartbeat task handles reconnect for timeout cases;
            // this branch handles clean-close cases where heartbeat didn't fire.
            spawn_reconnect(addr, http_port, state_rd.clone(), peer_id_rd.clone());
        }
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

    // Phase 10: heartbeat for this connection. BUG-10 fix: `addr`/`http_port`
    // are now the peer's real, dialable address (see handle_inbound), so a
    // reconnect *attempt* — whichever path below ends up making one — has
    // somewhere real to dial for inbound connections too, not just outbound.
    spawn_heartbeat(state.clone(), peer_node_id.clone(), addr.ip(), http_port);

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
    // BUG-10 fix: this branch handles a connection that errors/closes
    // immediately (e.g. the peer process was killed — a TCP reset arrives
    // as a WS read error right away) — well before the heartbeat's own
    // ~15-20s timeout window would notice anything wrong. Previously only
    // the heartbeat's timeout path attempted a reconnect, so this faster,
    // far more common disconnect shape left inbound connections with no
    // reconnect attempt at all, silently, regardless of the address fix
    // above. Mirrors the equivalent cleanup in connect_to_peer's read_task.
    let still_present = state.mesh_peers.read().await.contains_key(&peer_node_id);
    if still_present {
        remove_mesh_peer(&state, &peer_node_id).await;
        spawn_reconnect(addr.ip(), http_port, state.clone(), peer_node_id.clone());
    }
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

        MeshMessage::Hello { .. } | MeshMessage::HelloAck { .. } | MeshMessage::HelloConfirm { .. } => {
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

