//! The mesh layer: connections between nodes, heartbeats, reconnects and message dispatch.


use crate::types::*;
use crate::handshake::{self, Reason};
use crate::server::{peer_ip, PeerAddr};
use crate::revocation::{self, Revocation, Verdict};
use crate::{pairing, state, NodeState};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::{Rejection, Reply};

pub const PROTOCOL_VERSION: u32 = 6;

const HEARTBEAT_INTERVAL:  Duration = Duration::from_secs(5);
const HEARTBEAT_TIMEOUT:   Duration = Duration::from_secs(15);
const RECONNECT_GIVE_UP:   Duration = Duration::from_secs(600);
const DATA_QUEUE_FRAMES: usize = 8;
// Mesh messages carry whole catalog and chat snapshots, so they may be far larger than a tab's.
const MAX_MESH_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const CLOCK_SKEW_WARN_MS: i64 = 2 * 60 * 1000;

/// The peer could not be authenticated, so dialing it again soon would fail the same way.
#[derive(Debug)]
pub struct AuthFailure(pub String);

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthFailure {}

#[derive(Debug, Clone)]
pub struct MeshPeerHandle {
    pub node_id:    NodeId,
    pub node_name:  String,
    pub addr:       SocketAddr,
    pub http_port:  u16,
    pub sender:     mpsc::UnboundedSender<MeshMessage>,
    /// Binary chunk frames; bounded so a slow link holds the sender back.
    pub data:       mpsc::Sender<Vec<u8>>,
    /// Limits how many chunk requests from this peer are served at once.
    pub serve_slots: Arc<tokio::sync::Semaphore>,
    pub last_seen:  Instant,
    /// Latest measured RTT to this peer (ms); None until the first Pong.
    pub rtt_ms:     Option<u32>,
    /// Ends the connection, as when the peer is revoked.
    pub hang_up:    Arc<Notify>,
}

pub type MeshPeers = Arc<RwLock<HashMap<NodeId, MeshPeerHandle>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshMessage {
    Ping { ts: u64 },
    Pong { ts: u64 },

    CatalogSync  { files:    Vec<FileMetadata> },
    PeerSync     { peers:    Vec<PeerInfo>     },
    ChatSync     { messages: Vec<TextMessage>  },
    ChatMessage  { message:  TextMessage       },

    /// A node still fetching a file says which chunks it has, so others can fetch from it too.
    ChunkMap { file_id: String, chunks: u32, bitmap: String },
    GetManifest { file_id: String },
    /// The file's chunk hashes, as hex (32 bytes per chunk).
    Manifest { file_id: String, size: u64, hashes: String },
    /// Ask for chunks; they come back as binary frames, not as messages.
    GetChunks { file_id: String, indices: Vec<u32> },
    ChunkError { file_id: String, index: u32, reason: String },

    /// Routes a WebRTC payload between browser tabs on different nodes.
    SignalRelay {
        to_node_id:   NodeId,
        from_node_id: NodeId,
        payload:      serde_json::Value,
    },

    /// Keys no longer trusted, each signed by the node that revoked it.
    Revocations { revocations: Vec<Revocation> },

    Goodbye { node_id: NodeId },
}

/// What each node tells the other during the handshake, besides its name and key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Intro {
    /// The sender's listening port, so the accepting side can redial it.
    #[serde(default)]
    http_port: u16,
    /// The sender's LAN IPv4, or empty; behind TLS the accepting side only sees 127.0.0.1.
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

/// Removes a peer from mesh_peers and cleans up after it.
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

/// Registers a connection unless the peer is already connected, so only one of two racing dials wins.
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

/// Cleans up after a connection; a late-closing duplicate never removes a live peer.
async fn connection_ended(state: &NodeState, peer_node_id: &NodeId, sender: &mpsc::UnboundedSender<MeshMessage>) -> bool {
    let current = is_current_connection(state, peer_node_id, sender).await;
    if current {
        remove_mesh_peer(state, peer_node_id).await;
    }
    current
}

/// Marks the browser tabs a departed node hosted as offline and its files as unavailable.
async fn peer_departed_cleanup(state: &NodeState, departed_node_id: &NodeId) {
    // Marked left with a fresh version, or other nodes' merges would ignore the change.
    let departed_peers: Vec<PeerInfo> = {
        let mut local = state.local_peers.write().await;
        let mut changed = Vec::new();
        let now = chrono::Utc::now();
        for peer in local.values_mut() {
            if peer.hosting_node_id.as_deref() == Some(departed_node_id) {
                peer.hosting_node_id = None;
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
        for peer in departed_peers {
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }
    }

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

/// Redials a lost peer with backoff (2, 4, 8, then every 30 s) for up to 10 minutes.
pub fn spawn_reconnect(addr: IpAddr, http_port: u16, state: NodeState, peer_node_id: NodeId) {
    if addr.is_unspecified() || http_port == 0 {
        tracing::debug!("Reconnect: no dialable address for {peer_node_id} — not attempting");
        return;
    }
    if is_revoked(&state, &peer_node_id) {
        return;
    }
    // The higher id waits a little longer, so two nodes redialing each other don't collide.
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

/// Sends Goodbye to every mesh peer.
pub async fn broadcast_goodbye(state: &NodeState) {
    let peers = state.mesh_peers.read().await;
    let goodbye = MeshMessage::Goodbye { node_id: state.node_id.clone() };
    for handle in peers.values() {
        let _ = handle.sender.send(goodbye.clone());
    }
    drop(peers);
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Pings one peer periodically, and drops and redials it when Pongs stop.
pub fn spawn_heartbeat(
    state: NodeState,
    peer_node_id: NodeId,
    sender: mpsc::UnboundedSender<MeshMessage>,
    peer_addr: IpAddr,
    http_port: u16,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if !is_current_connection(&state, &peer_node_id, &sender).await { return; }

            let ts = chrono::Utc::now().timestamp_millis() as u64;
            let _ = sender.send(MeshMessage::Ping { ts });

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
                spawn_reconnect(peer_addr, http_port, state.clone(), peer_node_id.clone());
                return;
            }
        }
    });
}

pub async fn mesh_ws_handler(ws: Ws, peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    let remote_ip = peer_ip(peer);
    let ws = ws.max_message_size(MAX_MESH_MESSAGE_BYTES).max_frame_size(MAX_MESH_MESSAGE_BYTES);
    Ok(ws.on_upgrade(move |socket| handle_inbound(socket, remote_ip, state)))
}

async fn handle_inbound(ws: WebSocket, remote_ip: IpAddr, state: NodeState) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut transport = InboundTransport { tx: &mut ws_tx, rx: &mut ws_rx };
    let peer = match authenticate_inbound(&mut transport, remote_ip, &state).await {
        Ok(peer) => peer,
        Err(e) => {
            tracing::warn!("Mesh inbound: {remote_ip}: {e}");
            return;
        }
    };
    let intro = Intro::theirs(&peer);
    note_clock_skew(&state, &peer.name, intro.now_ms).await;
    tracing::info!("Mesh: inbound handshake OK — peer {} ({})", peer.node_id, peer.name);

    // Behind TLS every connection comes from our local proxy, so only the self-reported address can be redialed.
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

async fn authenticate_inbound(
    transport: &mut InboundTransport<'_>,
    remote_ip: IpAddr,
    state: &NodeState,
) -> Result<handshake::Peer, handshake::Failure> {
    let incoming = handshake::receive_hello(transport, &state.mesh_limiter, remote_ip).await?;
    let our_intro = Intro::ours(state);
    let local = handshake_local(state, &our_intro, &state.tls_fingerprint);

    // No duplicate check here: a node already connected by passphrase may pair to pin its key.
    if incoming.is_pairing() {
        if !state.pairings.is_open() {
            return Err(incoming.refuse(transport, Reason::NotPairing).await);
        }
        let pairing = incoming.start_pairing(transport, &local).await?;
        let name = pairing.peer_name();
        let result = complete_pairing(state, transport, pairing, &local, false).await;
        state.pairings.record(pairing::Outcome::of(&name, &result));
        return result;
    }

    // Checked before the passphrase so a second connection from a peer isn't counted as proof.
    if state.mesh_peers.read().await.contains_key(&incoming.claimed_node_id()) {
        return Err(incoming.refuse(transport, Reason::Duplicate).await);
    }
    incoming.accept(transport, &local).await
}

async fn complete_pairing<T: handshake::Transport>(
    state: &NodeState,
    transport: &mut T,
    pairing: handshake::Pairing<'_>,
    local: &handshake::Local<'_>,
    dialed: bool,
) -> Result<handshake::Peer, handshake::Failure> {
    let accepted = state.pairings.ask(pairing::Request::for_pairing(&pairing, dialed)).await;
    pairing.finish(transport, local, accepted).await
}

// ws:// and wss:// streams have different types, so both are boxed.
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
    dial(addr, http_port, state, false).await.map(drop)
}

/// Pairs with the node at this address once both people confirm the code; returns its name.
pub async fn pair_with_peer(addr: IpAddr, http_port: u16, state: NodeState) -> anyhow::Result<String> {
    dial(addr, http_port, state, true).await
}

async fn dial(addr: IpAddr, http_port: u16, state: NodeState, pairing: bool) -> anyhow::Result<String> {
    let tls_client_config = state.tls_client_config.clone();
    let url = format!("{}://{addr}:{http_port}/mesh", if tls_client_config.is_some() { "wss" } else { "ws" });
    tracing::info!("Mesh: dialing {url}");

    // Without TLS the fingerprint stays empty, so nothing rules out a man in the middle.
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
    let local = handshake_local(&state, &our_intro, &server_fingerprint);
    let authenticated = if pairing {
        match handshake::start_pairing(&mut transport, &local).await {
            Ok(pending) => complete_pairing(&state, &mut transport, pending, &local, true).await,
            Err(e) => Err(e),
        }
    } else {
        handshake::connect(&mut transport, &local).await
    };
    let peer = authenticated
        .map_err(|e| {
            let message = format!("Peer {url}: {e}");
            if e.is_authentication() { anyhow::Error::new(AuthFailure(message)) } else { anyhow::anyhow!(message) }
        })?;
    tracing::info!("Mesh: {url} authenticated (peer: {})", peer.node_id);
    note_clock_skew(&state, &peer.name, Intro::theirs(&peer).now_ms).await;
    let (peer_node_id, peer_node_name) = (peer.node_id, peer.name);
    let connected_name = peer_node_name.clone();

    let remote_addr: SocketAddr = format!("{addr}:{http_port}").parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());

    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<MeshMessage>();
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(DATA_QUEUE_FRAMES);
    let hang_up = Arc::new(Notify::new());
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
        hang_up:   hang_up.clone(),
    }).await;
    if !registered {
        tracing::warn!("Mesh: duplicate after handshake with {peer_node_id} — dropping");
        return Ok(connected_name);
    }
    tracing::info!("Mesh: registered peer {peer_node_id} ({peer_node_name})");
    crate::transfer::sources_changed(&state);

    post_handshake_sync(&peer_tx, &state).await;

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
    let write_abort = write_task.abort_handle();
    let read_task  = tokio::spawn(async move {
        loop {
            let result = tokio::select! {
                result = tt_rx.next() => result,
                () = hang_up.notified() => break,
            };
            let Some(result) = result else { break };
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
        write_abort.abort();
        if connection_ended(&state_rd, &peer_id_rd, &sender_rd).await {
            spawn_reconnect(addr, http_port, state_rd.clone(), peer_id_rd.clone());
        }
    });

    tokio::spawn(async move { tokio::select! { _ = write_task => {} _ = read_task => {} } });
    Ok(connected_name)
}

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
    let hang_up = Arc::new(Notify::new());
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
        hang_up:   hang_up.clone(),
    }).await;
    if !registered {
        return;
    }
    tracing::info!("Mesh: registered inbound peer {peer_node_id} ({peer_node_name})");
    crate::transfer::sources_changed(&state);

    post_handshake_sync(&peer_tx, &state).await;

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

    loop {
        let result = tokio::select! {
            result = ws_rx.next() => result,
            () = hang_up.notified() => break,
        };
        let Some(result) = result else { break };
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
    if connection_ended(&state, &peer_node_id, &peer_tx).await {
        spawn_reconnect(addr.ip(), http_port, state.clone(), peer_node_id.clone());
    }
}

async fn post_handshake_sync(peer_tx: &mpsc::UnboundedSender<MeshMessage>, state: &NodeState) {
    // First, so a node revoked while this peer was away is dropped before anything else arrives from it.
    match state.trust.revocations() {
        Ok(revocations) if !revocations.is_empty() => {
            for revocations in revocations.chunks(revocation::MAX_PER_MESSAGE) {
                let _ = peer_tx.send(MeshMessage::Revocations { revocations: revocations.to_vec() });
            }
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Mesh: could not read the revocations to pass on: {e:#}"),
    }

    // Tombstones too: a peer that was away must learn what was unshared meanwhile.
    let files: Vec<FileMetadata> = state.files.read().await.values().cloned().collect();
    let _ = peer_tx.send(MeshMessage::CatalogSync { files });

    let peers: Vec<PeerInfo> = state.local_peers.read().await.values().cloned().collect();
    let _ = peer_tx.send(MeshMessage::PeerSync { peers });

    let messages: Vec<TextMessage> = state.messages.read().await.clone();
    let _ = peer_tx.send(MeshMessage::ChatSync { messages });
}

pub(crate) async fn dispatch(msg: &MeshMessage, from_node_id: &NodeId, state: &NodeState) {
    match msg {
        MeshMessage::Ping { ts } => {
            let mut peers = state.mesh_peers.write().await;
            if let Some(h) = peers.get_mut(from_node_id) {
                h.last_seen = Instant::now();
                let _ = h.sender.send(MeshMessage::Pong { ts: *ts });
            }
        }

        MeshMessage::Pong { ts } => {
            let rtt_ms = (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(*ts) as u32;
            tracing::debug!("Mesh Pong from {from_node_id}: RTT {rtt_ms}ms");

            {
                let mut peers = state.mesh_peers.write().await;
                if let Some(h) = peers.get_mut(from_node_id) {
                    h.last_seen = Instant::now();
                    h.rtt_ms = Some(rtt_ms);
                }
            }

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

        MeshMessage::CatalogSync { files } => {
            tracing::debug!("Mesh CatalogSync from {from_node_id}: {} file(s)", files.len());
            state::apply_catalog_sync(state, files.clone(), from_node_id).await;
            state::reassert_holdership(state).await;
            crate::transfer::sources_changed(state);
        }

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

        MeshMessage::SignalRelay { to_node_id, from_node_id: from, payload } => {
            // The mesh is one hop, so a relay always comes from this peer and is addressed to us.
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

        MeshMessage::Revocations { revocations } => receive_revocations(state, from_node_id, revocations).await,

        MeshMessage::Goodbye { node_id } => {
            tracing::info!("Mesh: Goodbye from {node_id}");
            remove_mesh_peer(state, node_id).await;
        }

    }
}

/// Whether this node holds a revocation of that node.
pub fn is_revoked(state: &NodeState, node_id: &str) -> bool {
    state.trust.revocation(node_id).is_ok_and(|revocation| revocation.is_some())
}

/// What came of revoking a node from this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    Now,
    Already,
    UnknownNode,
}

/// Revokes a node this one knows, in this node's name, and tells the rest of the mesh.
pub async fn revoke_node(state: &NodeState, node_id: &str) -> anyhow::Result<Revoked> {
    if state.trust.revocation(node_id)?.is_some() {
        return Ok(Revoked::Already);
    }
    let Some(node) = state.trust.get(node_id)? else {
        return Ok(Revoked::UnknownNode);
    };
    let revocation = Revocation::issue(&state.identity, &node.public_key, crate::hlc::wall_clock_ms());
    if !state.trust.revoke(&revocation)? {
        return Ok(Revoked::Already);
    }
    tracing::warn!("Trust: revoked {} ({node_id})", node.name);
    revocations_applied(state, None, vec![revocation]).await;
    Ok(Revoked::Now)
}

async fn receive_revocations(state: &NodeState, from_node_id: &NodeId, revocations: &[Revocation]) {
    if revocations.len() > revocation::MAX_PER_MESSAGE {
        tracing::warn!("Mesh: {from_node_id} sent {} revocations at once — ignored", revocations.len());
        return;
    }
    let mut applied = Vec::new();
    for revocation in revocations {
        match revocation::receive(&state.trust, &state.identity, from_node_id, revocation) {
            Ok(Verdict::Applied) => {
                tracing::warn!("Trust: {} revoked by {} (via {from_node_id})", revocation.node_id(), revocation.issuer_id());
                applied.push(revocation.clone());
            }
            Ok(Verdict::AlreadyKnown) => {}
            Ok(Verdict::Refused(why)) => {
                tracing::warn!("Trust: ignored a revocation of {} from {from_node_id}: {why}", revocation.node_id());
            }
            Err(e) => tracing::error!("Trust: could not record the revocation of {}: {e:#}", revocation.node_id()),
        }
    }
    revocations_applied(state, Some(from_node_id), applied).await;
}

// Drops the revoked nodes and passes the news on to every other peer; nodes that already know it stop it there.
async fn revocations_applied(state: &NodeState, from_node_id: Option<&NodeId>, revocations: Vec<Revocation>) {
    if revocations.is_empty() {
        return;
    }
    let peers = state.mesh_peers.read().await;
    for revocation in &revocations {
        if let Some(handle) = peers.get(&revocation.node_id()) {
            handle.hang_up.notify_one();
        }
    }
    let message = MeshMessage::Revocations { revocations };
    for (node_id, handle) in peers.iter() {
        if Some(node_id) != from_node_id {
            let _ = handle.sender.send(message.clone());
        }
    }
}

/// A relay may only carry a file offer or its refusal, from a device the sending node really hosts.
async fn relay_is_legitimate(state: &NodeState, msg: &ServerMessage, sender_node: &NodeId) -> bool {
    let claimed_from = match msg {
        ServerMessage::IncomingFileOffer { from_session_id, .. }
        | ServerMessage::FileOfferDeclined { from_session_id, .. } => from_session_id,
        _ => return false,
    };
    let peers = state.local_peers.read().await;
    peers.get(claimed_from).is_some_and(|p| !p.left && p.hosting_node_id.as_ref() == Some(sender_node))
}

/// Routes a WebRTC signaling payload from a local tab to a tab that may be on another node.
pub async fn route_signal(
    state: &NodeState,
    _from_session_id: &str,
    to_session_id: &str,
    srv_msg: ServerMessage,
) {
    let hosting_node_id: Option<NodeId> = {
        let peers = state.local_peers.read().await;
        peers.get(to_session_id).and_then(|p| p.hosting_node_id.clone())
    };

    match hosting_node_id {
        Some(ref hid) if hid == &state.node_id => {
            let senders = state.local_senders.read().await;
            if let Some(sender) = senders.get(to_session_id) {
                let _ = sender.send(srv_msg);
            } else {
                tracing::warn!("route_signal: no local sender for {to_session_id}");
            }
        }

        Some(target_node_id) => {
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

        None => {
            tracing::warn!("route_signal: unknown hosting node for tab {to_session_id} — trying local fallback");
            let senders = state.local_senders.read().await;
            if let Some(sender) = senders.get(to_session_id) {
                let _ = sender.send(srv_msg);
            }
        }
    }
}

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
