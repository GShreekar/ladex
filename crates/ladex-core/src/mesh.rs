// ============================================================================
// LADEX — Phases 3/4/5/6/7/10: Mesh WebSocket Layer
//
// Phase 5:  SignalRelay routing — deliver to local tab or forward one-hop.
// Phase 6:  RTT tracking via Pong timestamps; push PeerSync to browser tabs.
// Phase 7:  Passphrase- and key-authenticated handshake (SPAKE2, TLS channel
//           binding, identity signatures, trust store; see handshake.rs).
// Phase 10: Heartbeat Ping/Pong (5s interval, 15s dead-peer timeout),
//           exponential reconnect backoff (2/4/8/30s, give-up at 10 min),
//           graceful Goodbye on shutdown, state cleanup on departure.
// ============================================================================

use crate::types::*;
use crate::handshake::{self, Reason};
use crate::server::{peer_ip, PeerAddr};
use crate::{state, NodeState};

use futures_util::stream::{SplitSink, SplitStream};
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
// v3: catalog and peer entries are ordered by hybrid-logical-clock `version`
// instead of wall-clock time, and the handshake carries each side's clock.
// v4: files are held by nodes and move between them as chunks (ChunkMap,
// GetManifest, Manifest, GetChunks, ChunkError and binary chunk frames); catalog
// entries list `holders` per node instead of hosting browser sessions.
// v5: the handshake (handshake.rs) also proves each node's identity key, and
// node ids are derived from those keys.
pub const PROTOCOL_VERSION: u32 = 5;

// Phase 10 timing constants
const HEARTBEAT_INTERVAL:  Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT:   Duration = Duration::from_secs(15);
const RECONNECT_GIVE_UP:   Duration = Duration::from_secs(600); // 10 min
// Chunk frames waiting to be written to one peer.
const DATA_QUEUE_FRAMES: usize = 8;
// Mesh messages are full catalog / chat snapshots, so allow far more than a
// browser tab may send, but not the library default of 64 MiB.
const MAX_MESH_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
// Clocks further apart than this are reported at handshake time.
const CLOCK_SKEW_WARN_MS: i64 = 2 * 60 * 1000;

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
    /// Binary chunk frames. Bounded, so a slow link holds the sender back
    /// instead of letting chunks pile up in memory; control messages use
    /// `sender` and go first.
    pub data:       mpsc::Sender<Vec<u8>>,
    /// Limits how many chunk requests from this peer are served at once.
    pub serve_slots: Arc<tokio::sync::Semaphore>,
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
    Ping { ts: u64 },
    Pong { ts: u64 },

    // Phase 4
    CatalogSync  { files:    Vec<FileMetadata> },
    PeerSync     { peers:    Vec<PeerInfo>     },
    ChatSync     { messages: Vec<TextMessage>  },
    ChatMessage  { message:  TextMessage       },

    // ── File transfer (see transfer.rs) ───────────────────────────────────
    /// A node still fetching a file says which chunks it has, so others can fetch from it too.
    ChunkMap { file_id: String, chunks: u32, bitmap: String },
    GetManifest { file_id: String },
    /// The file's chunk hashes, as hex (32 bytes per chunk).
    Manifest { file_id: String, size: u64, hashes: String },
    /// Ask for chunks; they come back as binary frames, not as messages.
    GetChunks { file_id: String, indices: Vec<u32> },
    ChunkError { file_id: String, index: u32, reason: String },

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

/// What each node tells the other during the handshake, besides its name and key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Intro {
    /// BUG-10 fix: the sender's own HTTP/mesh listening port. An inbound
    /// connection only reveals the ephemeral source port the peer dialed
    /// from, so without this the accepting side has no address to
    /// reconnect to if the connection later drops.
    #[serde(default)]
    http_port: u16,
    /// BUG-10 fix: the sender's own best-guess LAN IPv4 (empty if unknown).
    /// With TLS enabled the accepting side only sees connections from our
    /// own local TLS-terminating proxy (127.0.0.1), never the real peer.
    #[serde(default)]
    ip: String,
    /// Sender's wall clock (ms since the epoch), to spot badly set clocks.
    #[serde(default)]
    now_ms: u64,
}

impl Intro {
    fn ours(state: &NodeState) -> String {
        let intro = Intro {
            http_port: state.http_port,
            ip: state.local_ip.map(|ip| ip.to_string()).unwrap_or_default(),
            now_ms: crate::hlc::wall_clock_ms(),
        };
        serde_json::to_string(&intro).expect("an intro always serializes")
    }

    // A peer's intro is only advisory, so one that can't be read is treated as empty.
    fn theirs(peer: &handshake::Peer) -> Intro {
        serde_json::from_str(&peer.intro).unwrap_or_default()
    }
}

fn handshake_local<'a>(state: &'a NodeState, intro: &'a str, channel_binding: &'a [u8]) -> handshake::Local<'a> {
    handshake::Local {
        identity: &state.identity,
        name: &state.node_name,
        passphrase: state.passphrase.as_deref(),
        protocol: PROTOCOL_VERSION,
        intro,
        channel_binding,
        trust: &state.trust,
    }
}

// The handshake runs over binary frames on the /mesh WebSocket, before any mesh message.
struct InboundTransport<'a> {
    tx: &'a mut SplitSink<WebSocket, Message>,
    rx: &'a mut SplitStream<WebSocket>,
}

impl handshake::Transport for InboundTransport<'_> {
    async fn send(&mut self, frame: Vec<u8>) -> std::io::Result<()> {
        self.tx.send(Message::binary(frame)).await.map_err(std::io::Error::other)
    }

    async fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        while let Some(message) = self.rx.next().await {
            let message = message.map_err(std::io::Error::other)?;
            if message.is_close() {
                return Ok(None);
            }
            if message.is_binary() || message.is_text() {
                return Ok(Some(message.into_bytes().to_vec()));
            }
        }
        Ok(None)
    }
}

struct OutboundTransport<'a> {
    tx: &'a mut MeshSink,
    rx: &'a mut MeshSource,
}

impl handshake::Transport for OutboundTransport<'_> {
    async fn send(&mut self, frame: Vec<u8>) -> std::io::Result<()> {
        self.tx.send(tokio_tungstenite::tungstenite::Message::Binary(frame)).await.map_err(std::io::Error::other)
    }

    async fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        while let Some(message) = self.rx.next().await {
            let message = message.map_err(std::io::Error::other)?;
            if message.is_close() {
                return Ok(None);
            }
            if message.is_binary() || message.is_text() {
                return Ok(Some(message.into_data()));
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Tell the user when an authenticated peer's clock is far from ours.
async fn note_clock_skew(state: &NodeState, peer_name: &str, peer_now_ms: u64) {
    if peer_now_ms == 0 {
        return;
    }
    let skew_ms = peer_now_ms as i64 - crate::hlc::wall_clock_ms() as i64;
    if skew_ms.abs() > CLOCK_SKEW_WARN_MS {
        let direction = if skew_ms > 0 { "ahead of" } else { "behind" };
        let problem = format!("its clock is {} {direction} this device's", crate::hlc::describe_ms(skew_ms.unsigned_abs()));
        state::report_clock_problem(state, peer_name, &problem).await;
    }
}

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

/// Registers a newly authenticated connection, unless the peer is already
/// connected (both nodes dialed each other at once). Checking and inserting
/// under one lock means only one of two racing connections can win.
async fn register_peer(state: &NodeState, handle: MeshPeerHandle) -> bool {
    let mut peers = state.mesh_peers.write().await;
    if peers.contains_key(&handle.node_id) {
        return false;
    }
    peers.insert(handle.node_id.clone(), handle);
    true
}

/// Whether `sender` belongs to the connection currently registered for the peer.
async fn is_current_connection(state: &NodeState, peer_node_id: &NodeId, sender: &mpsc::UnboundedSender<MeshMessage>) -> bool {
    state.mesh_peers.read().await.get(peer_node_id).is_some_and(|h| h.sender.same_channel(sender))
}

/// Cleanup when a connection ends. A duplicate connection closing late must
/// not remove the peer while its other connection is still live.
async fn connection_ended(state: &NodeState, peer_node_id: &NodeId, sender: &mpsc::UnboundedSender<MeshMessage>) -> bool {
    let current = is_current_connection(state, peer_node_id, sender).await;
    if current {
        remove_mesh_peer(state, peer_node_id).await;
    }
    current
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
                peer.version = state.clock.now();
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

    // The departed node can no longer serve anything. Its files stay listed,
    // marked as unavailable, until it returns (or nobody holds them for a day).
    let changed: Vec<FileMetadata> = {
        let mut files = state.files.write().await;
        files
            .values_mut()
            .filter(|f| !f.deleted && f.is_held_by(departed_node_id))
            .map(|file| {
                file.set_holder(departed_node_id, false, state.clock.now());
                file.clone()
            })
            .collect()
    };
    state::push_files_to_mesh(&state.mesh_peers, changed).await;
    crate::transfer::sources_changed(state);

    state::broadcast_catalog(state).await;
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
    // When both nodes redial each other, the one with the higher id waits a
    // little longer, so the other's dial usually lands first instead of colliding.
    let stagger = if state.node_id > peer_node_id { Duration::from_secs(1) } else { Duration::ZERO };
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
            tokio::time::sleep(Duration::from_secs(delay_secs) + stagger).await;
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
    sender: mpsc::UnboundedSender<MeshMessage>,
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
            // Stop once this connection is no longer the peer's registered one.
            if !is_current_connection(&state, &peer_node_id, &sender).await { return; }

            let ts = chrono::Utc::now().timestamp_millis() as u64;
            let _ = sender.send(MeshMessage::Ping { ts });

            // Wait for up to HEARTBEAT_TIMEOUT for a Pong (last_seen update)
            tokio::time::sleep(HEARTBEAT_TIMEOUT).await;

            let timed_out = {
                let peers = state.mesh_peers.read().await;
                peers.get(&peer_node_id)
                    .filter(|h| h.sender.same_channel(&sender))
                    .is_some_and(|h| h.last_seen.elapsed() > HEARTBEAT_TIMEOUT)
            };
            if timed_out {
                tracing::warn!("Heartbeat: peer {peer_node_id} timed out — removing");
                connection_ended(&state, &peer_node_id, &sender).await;
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
    let ws = ws.max_message_size(MAX_MESH_MESSAGE_BYTES).max_frame_size(MAX_MESH_MESSAGE_BYTES);
    Ok(ws.on_upgrade(move |socket| handle_inbound(socket, remote_ip, state)))
}

async fn handle_inbound(ws: WebSocket, remote_ip: IpAddr, state: NodeState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut transport = InboundTransport { tx: &mut ws_tx, rx: &mut ws_rx };

    let incoming = match handshake::receive_hello(&mut transport, &state.mesh_limiter, remote_ip).await {
        Ok(incoming) => incoming,
        Err(e) => {
            tracing::warn!("Mesh inbound: {remote_ip}: {e}");
            return;
        }
    };
    // Checked before the passphrase so a second connection from a peer isn't counted as proof.
    if state.mesh_peers.read().await.contains_key(&incoming.claimed_node_id()) {
        incoming.refuse(&mut transport, Reason::Duplicate).await;
        return;
    }
    let our_intro = Intro::ours(&state);
    let peer = match incoming.accept(&mut transport, &handshake_local(&state, &our_intro, &state.tls_fingerprint)).await {
        Ok(peer) => peer,
        Err(e) => {
            tracing::warn!("Mesh inbound: {remote_ip}: {e}");
            return;
        }
    };
    let intro = Intro::theirs(&peer);
    note_clock_skew(&state, &peer.name, intro.now_ms).await;
    tracing::info!("Mesh: inbound handshake OK — peer {} ({})", peer.node_id, peer.name);

    // BUG-10 fix: build a real, dialable address for this peer from its
    // self-reported ip/http_port (Intro), so that if the connection later
    // drops, the heartbeat's reconnect attempt has somewhere real to dial
    // instead of the old "0.0.0.0:0" placeholder (which burned the whole
    // 10-minute give-up window failing to connect). warp 0.4.1 exposes no
    // way to read the TCP-level remote address at all (see the route
    // definition in main.rs), so self-reporting is the only option here —
    // conveniently, it's also the only thing that works once TLS is
    // enabled, since every inbound connection warp sees then actually
    // comes from our own local TLS-terminating proxy at 127.0.0.1, not the
    // real peer.
    let addr: SocketAddr = match (intro.ip.parse::<IpAddr>().ok(), intro.http_port) {
        (Some(ip), port) if port > 0 => SocketAddr::new(ip, port),
        _ => {
            tracing::warn!(
                "Mesh inbound: no usable address for {} (ip={:?}, http_port={}) — \
                 reconnect after disconnect will not be possible for this peer",
                peer.node_id, intro.ip, intro.http_port
            );
            "0.0.0.0:0".parse().unwrap()
        }
    };
    run_connection(ws_tx, ws_rx, peer.node_id, peer.name, addr, intro.http_port, state).await;
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

fn mesh_ws_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(MAX_MESH_MESSAGE_BYTES),
        max_frame_size: Some(MAX_MESH_MESSAGE_BYTES),
        ..Default::default()
    }
}

pub async fn connect_to_peer(addr: IpAddr, http_port: u16, state: NodeState) -> anyhow::Result<()> {
    let tls_client_config = state.tls_client_config.clone();
    let url = format!("{}://{addr}:{http_port}/mesh", if tls_client_config.is_some() { "wss" } else { "ws" });
    tracing::info!("Mesh: dialing {url}");

    // The fingerprint stays empty without TLS (--no-tls); the handshake then
    // has nothing to bind to and a man in the middle can't be ruled out.
    let (mut tt_tx, mut tt_rx, server_fingerprint): (MeshSink, MeshSource, Vec<u8>) =
        if let Some(client_config) = tls_client_config {
            let (ws_stream, _, fingerprint) = crate::tls::connect_wss(addr, http_port, "/mesh", client_config, mesh_ws_config())
                .await
                .map_err(|e| anyhow::anyhow!("WSS connect to {url} failed: {e}"))?;
            let (tx, rx) = ws_stream.split();
            (Box::pin(tx), Box::pin(rx), fingerprint)
        } else {
            let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(&url, Some(mesh_ws_config()), false)
                .await
                .map_err(|e| anyhow::anyhow!("WS connect to {url} failed: {e}"))?;
            let (tx, rx) = ws_stream.split();
            (Box::pin(tx), Box::pin(rx), Vec::new())
        };

    let our_intro = Intro::ours(&state);
    let mut transport = OutboundTransport { tx: &mut tt_tx, rx: &mut tt_rx };
    let peer = handshake::connect(&mut transport, &handshake_local(&state, &our_intro, &server_fingerprint))
        .await
        .map_err(|e| {
            let message = format!("Peer {url}: {e}");
            if e.is_authentication() { anyhow::Error::new(AuthFailure(message)) } else { anyhow::anyhow!(message) }
        })?;
    tracing::info!("Mesh: {url} authenticated (peer: {})", peer.node_id);
    note_clock_skew(&state, &peer.name, Intro::theirs(&peer).now_ms).await;
    let (peer_node_id, peer_node_name) = (peer.node_id, peer.name);

    let remote_addr: SocketAddr = format!("{addr}:{http_port}").parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());

    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE_FRAMES);
    let registered = register_peer(&state, MeshPeerHandle {
        node_id:   peer_node_id.clone(),
        node_name: peer_node_name.clone(),
        addr:      remote_addr,
        http_port,
        sender:    peer_tx.clone(),
        data:      data_tx,
        serve_slots: Arc::new(tokio::sync::Semaphore::new(crate::transfer::SERVE_SLOTS)),
        last_seen: Instant::now(),
        rtt_ms:    None,
    }).await;
    if !registered {
        tracing::warn!("Mesh: duplicate after handshake with {peer_node_id} — dropping");
        return Ok(());
    }
    tracing::info!("Mesh: registered peer {peer_node_id} ({peer_node_name})");
    crate::transfer::sources_changed(&state);

    post_handshake_sync(&peer_tx, &state).await;

    // Phase 10: start heartbeat for this connection
    spawn_heartbeat(state.clone(), peer_node_id.clone(), peer_tx.clone(), addr, http_port);

    let write_task = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message as WsMessage;
        loop {
            // Control messages (heartbeats, catalog) always go before chunk data.
            tokio::select! {
                biased;
                msg = peer_rx.recv() => {
                    let Some(msg) = msg else { break };
                    let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
                    if tt_tx.send(WsMessage::Text(json)).await.is_err() { break; }
                }
                frame = data_rx.recv() => {
                    let Some(frame) = frame else { break };
                    if tt_tx.send(WsMessage::Binary(frame)).await.is_err() { break; }
                }
            }
        }
    });

    let state_rd   = state.clone();
    let peer_id_rd = peer_node_id.clone();
    let sender_rd  = peer_tx.clone();
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
                Ok(msg) if msg.is_binary() => crate::transfer::on_binary_frame(&state_rd, &peer_id_rd, &msg.into_data()),
                Ok(msg) if msg.is_close() => break,
                Err(e) => { tracing::warn!("Mesh outbound: WS error from {peer_id_rd}: {e}"); break; }
                _ => {}
            }
        }
        // Phase 10: only run cleanup if not already done by heartbeat
        if connection_ended(&state_rd, &peer_id_rd, &sender_rd).await {
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
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE_FRAMES);
    let registered = register_peer(&state, MeshPeerHandle {
        node_id:   peer_node_id.clone(),
        node_name: peer_node_name.clone(),
        addr,
        http_port,
        sender:    peer_tx.clone(),
        data:      data_tx,
        serve_slots: Arc::new(tokio::sync::Semaphore::new(crate::transfer::SERVE_SLOTS)),
        last_seen: Instant::now(),
        rtt_ms:    None,
    }).await;
    // The peer connected to us in the meantime, from its own dial.
    if !registered {
        return;
    }
    tracing::info!("Mesh: registered inbound peer {peer_node_id} ({peer_node_name})");
    crate::transfer::sources_changed(&state);

    post_handshake_sync(&peer_tx, &state).await;

    // Phase 10: heartbeat for this connection. BUG-10 fix: `addr`/`http_port`
    // are now the peer's real, dialable address (see handle_inbound), so a
    // reconnect *attempt* — whichever path below ends up making one — has
    // somewhere real to dial for inbound connections too, not just outbound.
    spawn_heartbeat(state.clone(), peer_node_id.clone(), peer_tx.clone(), addr.ip(), http_port);

    let write_task = tokio::spawn(async move {
        loop {
            // Control messages (heartbeats, catalog) always go before chunk data.
            tokio::select! {
                biased;
                msg = peer_rx.recv() => {
                    let Some(msg) = msg else { break };
                    let json = match serde_json::to_string(&msg) { Ok(j) => j, Err(_) => continue };
                    if ws_tx.send(Message::text(json)).await.is_err() { break; }
                }
                frame = data_rx.recv() => {
                    let Some(frame) = frame else { break };
                    if ws_tx.send(Message::binary(frame)).await.is_err() { break; }
                }
            }
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
            Ok(msg) if msg.is_binary() => crate::transfer::on_binary_frame(&state, &peer_node_id, msg.as_bytes()),
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
    if connection_ended(&state, &peer_node_id, &peer_tx).await {
        spawn_reconnect(addr.ip(), http_port, state.clone(), peer_node_id.clone());
    }
}

// ---------------------------------------------------------------------------
// Full-state sync (Phase 4)
// ---------------------------------------------------------------------------

async fn post_handshake_sync(peer_tx: &mpsc::UnboundedSender<MeshMessage>, state: &NodeState) {
    // Tombstones too: a peer that was away must learn what was unshared meanwhile.
    let files: Vec<FileMetadata> = state.files.read().await.values().cloned().collect();
    let _ = peer_tx.send(MeshMessage::CatalogSync { files });

    let peers: Vec<PeerInfo> = state.local_peers.read().await.values().cloned().collect();
    let _ = peer_tx.send(MeshMessage::PeerSync { peers });

    let messages: Vec<TextMessage> = state.messages.read().await.clone();
    let _ = peer_tx.send(MeshMessage::ChatSync { messages });
}

// ---------------------------------------------------------------------------
// Dispatch (Phases 3–7)
// ---------------------------------------------------------------------------

pub(crate) async fn dispatch(msg: &MeshMessage, from_node_id: &NodeId, state: &NodeState) {
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
            state::apply_catalog_sync(state, files.clone(), from_node_id).await;
            // The catalog may have lost us as a holder while we were away, or learned of deletions.
            state::reassert_holdership(state).await;
            crate::transfer::sources_changed(state);
        }

        // ── File transfer ────────────────────────────────────────────────
        MeshMessage::GetManifest { file_id } => crate::transfer::serve_manifest(state, from_node_id, file_id).await,
        MeshMessage::Manifest { file_id, hashes, .. } => crate::transfer::on_manifest(state, from_node_id, file_id, hashes),
        MeshMessage::GetChunks { file_id, indices } => crate::transfer::serve_chunks(state, from_node_id, file_id, indices.clone()).await,
        MeshMessage::ChunkError { file_id, index, .. } => crate::transfer::on_chunk_error(state, from_node_id, file_id, *index),
        MeshMessage::ChunkMap { file_id, chunks, bitmap } => crate::transfer::on_chunk_map(state, from_node_id, file_id, *chunks, bitmap),
        MeshMessage::PeerSync { peers } => {
            tracing::debug!("Mesh PeerSync from {from_node_id}: {} peer(s)", peers.len());
            state::apply_peer_sync(state, peers.clone(), from_node_id).await;
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
            // The mesh is one hop, so the relay's sender is the peer on this
            // connection, and it is only ever addressed to us.
            if from != from_node_id || to_node_id != &state.node_id {
                tracing::warn!("SignalRelay from {from_node_id} with mismatched addressing — dropped");
                return;
            }
            let Some(sid) = payload.get("to_session_id").and_then(|v| v.as_str()).map(String::from) else {
                tracing::warn!("SignalRelay to us but payload has no to_session_id");
                return;
            };
            let srv_msg = match serde_json::from_value::<ServerMessage>(payload.clone()) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("SignalRelay: bad payload for tab {sid}: {e}");
                    return;
                }
            };
            if !relay_is_legitimate(state, &srv_msg, from_node_id).await {
                tracing::warn!("SignalRelay from {from_node_id} refused: not a signaling message from one of its own devices");
                return;
            }
            let senders = state.local_senders.read().await;
            match senders.get(&sid) {
                Some(sender) => { let _ = sender.send(srv_msg); }
                None => tracing::warn!("SignalRelay: no local tab {sid}"),
            }
        }

        // ── Phase 10: graceful goodbye ───────────────────────────────────
        MeshMessage::Goodbye { node_id } => {
            tracing::info!("Mesh: Goodbye from {node_id}");
            remove_mesh_peer(state, node_id).await;
        }

    }
}

/// A relay may only carry a file offer (or its refusal), and the device it
/// says it is from must really be hosted by the sending node. Without this a
/// peer could push any message (fake file lists, errors, offers "from"
/// someone else) straight into a user's browser tab.
async fn relay_is_legitimate(state: &NodeState, msg: &ServerMessage, sender_node: &NodeId) -> bool {
    let claimed_from = match msg {
        ServerMessage::IncomingFileOffer { from_session_id, .. }
        | ServerMessage::FileOfferDeclined { from_session_id, .. } => from_session_id,
        _ => return false,
    };
    let peers = state.local_peers.read().await;
    peers.get(claimed_from).is_some_and(|p| !p.left && p.hosting_node_id.as_ref() == Some(sender_node))
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


#[cfg(test)]
mod tests {
    use super::*;

    async fn node_with_device(device: &str, hosted_by: &str, left: bool) -> NodeState {
        let state = NodeState::for_tests(None);
        state.local_peers.write().await.insert(
            device.to_string(),
            PeerInfo {
                session_id: device.to_string(),
                connected_at: chrono::Utc::now(),
                user_agent: None,
                hosting_node_id: Some(hosted_by.to_string()),
                node_rtt_ms: None,
                left,
                left_at: None,
                hosting_node_name: None,
                nickname: None,
                version: Default::default(),
            },
        );
        state
    }

    fn offer(from: &str) -> ServerMessage {
        ServerMessage::IncomingFileOffer { file_id: "f".into(), from_session_id: from.into() }
    }

    #[tokio::test]
    async fn a_relay_from_a_device_the_sending_node_hosts_is_allowed() {
        let state = node_with_device("peer_x", "node_other", false).await;
        let sender = "node_other".to_string();
        assert!(relay_is_legitimate(&state, &offer("peer_x"), &sender).await);
        let declined = ServerMessage::FileOfferDeclined { file_id: "f".into(), from_session_id: "peer_x".into() };
        assert!(relay_is_legitimate(&state, &declined, &sender).await);
    }

    #[tokio::test]
    async fn a_node_cannot_relay_in_the_name_of_a_device_hosted_elsewhere() {
        let state = node_with_device("peer_x", "node_honest", false).await;
        assert!(!relay_is_legitimate(&state, &offer("peer_x"), &"node_evil".to_string()).await);
        assert!(!relay_is_legitimate(&state, &offer("peer_unknown"), &"node_honest".to_string()).await);
    }

    #[tokio::test]
    async fn a_device_that_has_left_cannot_be_relayed_for() {
        let state = node_with_device("peer_x", "node_other", true).await;
        assert!(!relay_is_legitimate(&state, &offer("peer_x"), &"node_other".to_string()).await);
    }

    #[tokio::test]
    async fn only_file_offers_may_be_relayed_into_a_browser_tab() {
        let state = node_with_device("peer_x", "node_other", false).await;
        let sender = "node_other".to_string();
        for forged in [
            ServerMessage::FileListUpdate { files: vec![] },
            ServerMessage::Error { message: "your session expired, log in at evil.example".into() },
            ServerMessage::FileRemoved { file_id: "f".into() },
            ServerMessage::PeerSync { peers: vec![] },
            ServerMessage::MessageHistory { messages: vec![] },
        ] {
            assert!(!relay_is_legitimate(&state, &forged, &sender).await, "{forged:?}");
        }
    }
}
