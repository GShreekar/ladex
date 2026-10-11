//! The mesh layer: connections between nodes, heartbeats, reconnects and message dispatch.

use crate::handshake::{self, Reason};
use crate::revocation::{self, Revocation, Verdict};
use crate::types::*;
use crate::{pairing, state, NodeState};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify, RwLock};

pub mod dialer;
pub mod endpoint;
mod frames;
pub mod quic;

// 8: mesh messages on QUIC stream 0 are postcard-encoded instead of JSON.
// 9: nodes trade catalog version vectors and signed ops.
// 10: holders are told with `Have` messages instead of being listed in the catalog.
// 11: chunks travel on a stream per request (see `transfer::wire`), and stream 0 frames are bare postcard messages.
pub const PROTOCOL_VERSION: u32 = 11;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const FETCH_QUEUE: usize = 16;
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
    pub node_id: NodeId,
    pub node_name: String,
    /// Where the peer's QUIC endpoint can be dialed.
    pub addr: SocketAddr,
    pub sender: mpsc::UnboundedSender<MeshMessage>,
    /// Requests for chunks; each is answered on a stream of its own.
    pub fetches: mpsc::Sender<crate::transfer::wire::Fetch>,
    /// Limits how many chunk requests from this peer are served at once.
    pub serve_slots: Arc<tokio::sync::Semaphore>,
    pub last_seen: Instant,
    /// Latest measured RTT to this peer (ms); None until the first Pong.
    pub rtt_ms: Option<u32>,
    /// Ends the connection, as when the peer is revoked.
    pub hang_up: Arc<Notify>,
    /// The node that opened this connection; decides which of two connections with the same peer stays.
    pub dialer: NodeId,
}

pub type MeshPeers = Arc<RwLock<HashMap<NodeId, MeshPeerHandle>>>;

// Postcard identifies variants by position, so new variants go at the end and reordering needs a protocol bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshMessage {
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },

    PeerSync {
        peers: Vec<PeerInfo>,
    },
    ChatSync {
        messages: Vec<TextMessage>,
    },
    ChatMessage {
        message: TextMessage,
    },

    /// A node says which chunks of a file it has, so others can fetch from it, even before it has them all.
    Have {
        file_id: String,
        chunks: u32,
        bitmap: String,
    },
    GetManifest {
        file_id: String,
    },
    /// The file's chunk hashes, as hex (32 bytes per chunk).
    Manifest {
        file_id: String,
        size: u64,
        hashes: String,
    },
    /// Routes a WebRTC signal between browser tabs on different nodes.
    SignalRelay {
        to_node_id: NodeId,
        from_node_id: NodeId,
        to_session_id: SessionId,
        /// The [`ServerMessage`] for the tab, as the JSON the browser reads.
        signal: String,
    },

    /// Keys no longer trusted, each signed by the node that revoked it.
    Revocations {
        revocations: Vec<Revocation>,
    },

    Goodbye {
        node_id: NodeId,
    },

    /// What the sender holds of the catalog; the receiver answers with the ops the sender lacks.
    CatalogVersion {
        version: crate::catalog::VersionVector,
    },
    /// Signed catalog ops: sent in answer to a version, or pushed when the sender makes one.
    CatalogOps {
        ops: Vec<crate::catalog::Op>,
    },

    /// These files are complete on the sender.
    HaveAll {
        file_ids: Vec<String>,
    },
    /// The sender no longer has these files.
    Lost {
        file_ids: Vec<String>,
    },
}

/// What each node tells the other during the handshake, besides its name and key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Intro {
    /// The sender's QUIC port, so the accepting side can redial it.
    #[serde(default)]
    quic_port: u16,
    /// The sender's LAN IPv4, or empty.
    #[serde(default)]
    ip: String,
    /// Sender's wall clock (ms since the epoch), to spot badly set clocks.
    #[serde(default)]
    now_ms: u64,
}

impl Intro {
    fn ours(state: &NodeState) -> String {
        let intro = Intro {
            quic_port: state.quic_port,
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
    let removed = state.mesh_peers.write().await.remove(peer_node_id);
    if let Some(handle) = removed {
        // Without this, QUIC keep-alives would hold the connection open with nothing reading it.
        handle.hang_up.notify_one();
        tracing::info!("Mesh: peer disconnected: {peer_node_id}");
        peer_departed_cleanup(state, peer_node_id).await;
    }
}

/// What a connection's writer drains, and how its reader learns the peer was revoked.
struct Session {
    sender: mpsc::UnboundedSender<MeshMessage>,
    messages: mpsc::UnboundedReceiver<MeshMessage>,
    fetches: mpsc::Receiver<crate::transfer::wire::Fetch>,
    hang_up: Arc<Notify>,
}

/// Registers a newly authenticated peer reachable at `addr`, sends it this node's state and starts its heartbeat; None if it is already connected.
async fn start_session(
    state: &NodeState,
    peer_node_id: &NodeId,
    peer_node_name: &str,
    addr: SocketAddr,
    dialer: NodeId,
) -> Option<Session> {
    let (sender, messages) = mpsc::unbounded_channel::<MeshMessage>();
    let (fetches_tx, fetches) = mpsc::channel(FETCH_QUEUE);
    let hang_up = Arc::new(Notify::new());
    let handle = MeshPeerHandle {
        node_id: peer_node_id.clone(),
        node_name: peer_node_name.to_string(),
        addr,
        sender: sender.clone(),
        fetches: fetches_tx,
        serve_slots: Arc::new(tokio::sync::Semaphore::new(crate::transfer::SERVE_SLOTS)),
        last_seen: Instant::now(),
        rtt_ms: None,
        hang_up: hang_up.clone(),
        dialer,
    };
    if !register_peer(state, handle).await {
        return None;
    }
    tracing::info!("Mesh: registered peer {peer_node_id} ({peer_node_name})");
    crate::transfer::sources_changed(state);
    post_handshake_sync(&sender, state).await;
    spawn_heartbeat(state.clone(), peer_node_id.clone(), sender.clone());
    Some(Session { sender, messages, fetches, hang_up })
}

/// Registers a connection, hanging up the open one with the peer if the new one replaces it; false if the open one stays.
async fn register_peer(state: &NodeState, handle: MeshPeerHandle) -> bool {
    let mut peers = state.mesh_peers.write().await;
    if let Some(open) = peers.get(&handle.node_id) {
        if !replaces(&open.dialer, &handle.dialer) {
            return false;
        }
        tracing::info!("Mesh: a new connection with {} replaces the open one", handle.node_id);
        open.hang_up.notify_one();
    }
    peers.insert(handle.node_id.clone(), handle);
    true
}

/// Whether the peer's open connection stays rather than one `new_dialer` is opening, so the new one can be turned away early.
async fn keeps_open_connection(state: &NodeState, peer_node_id: &NodeId, new_dialer: &NodeId) -> bool {
    state.mesh_peers.read().await.get(peer_node_id).is_some_and(|open| !replaces(&open.dialer, new_dialer))
}

/// The lower node id's connection wins, and a node redialing replaces its own: it only redials once it lost the old one.
fn replaces(open_dialer: &str, new_dialer: &str) -> bool {
    new_dialer <= open_dialer
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

/// Marks the browser tabs a departed node hosted as offline; the files it had are no longer hosted by it.
async fn peer_departed_cleanup(state: &NodeState, departed_node_id: &NodeId) {
    state.availability.drop_node(departed_node_id);
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
        crate::websocket::broadcast(state, ServerMessage::PeerSync { peers: departed_peers.clone() }).await;
        for peer in departed_peers {
            state::push_peer_to_mesh(&state.mesh_peers, peer).await;
        }
    }

    crate::transfer::sources_changed(state);

    state::broadcast_catalog(state).await;
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

/// Pings one peer periodically to measure the round trip; QUIC's idle timeout ends a connection that goes silent.
fn spawn_heartbeat(state: NodeState, peer_node_id: NodeId, sender: mpsc::UnboundedSender<MeshMessage>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if !is_current_connection(&state, &peer_node_id, &sender).await {
                return;
            }
            let ts = chrono::Utc::now().timestamp_millis() as u64;
            let _ = sender.send(MeshMessage::Ping { ts });
        }
    });
}

pub(super) async fn authenticate_inbound<T: handshake::Transport>(
    transport: &mut T,
    remote_ip: IpAddr,
    state: &NodeState,
    channel_binding: &[u8],
) -> Result<handshake::Peer, handshake::Failure> {
    let incoming = handshake::receive_hello(transport, &state.mesh_limiter, remote_ip).await?;
    let our_intro = Intro::ours(state);
    let local = handshake_local(state, &our_intro, channel_binding);

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
    let claimed = incoming.claimed_node_id();
    if keeps_open_connection(state, &claimed, &claimed).await {
        return Err(incoming.refuse(transport, Reason::Duplicate).await);
    }
    incoming.accept(transport, &local).await
}

pub(super) async fn complete_pairing<T: handshake::Transport>(
    state: &NodeState,
    transport: &mut T,
    pairing: handshake::Pairing<'_>,
    local: &handshake::Local<'_>,
    dialed: bool,
) -> Result<handshake::Peer, handshake::Failure> {
    let accepted = state.pairings.ask(pairing::Request::for_pairing(&pairing, dialed)).await;
    pairing.finish(transport, local, accepted).await
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

    crate::catalog::sync::offer_version(peer_tx, state);
    crate::availability::announce_all(peer_tx, state);

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
                crate::websocket::broadcast(state, ServerMessage::PeerSync { peers: updated_peers }).await;
            }
        }

        MeshMessage::GetManifest { file_id } => crate::transfer::serve_manifest(state, from_node_id, file_id).await,
        MeshMessage::Manifest { file_id, hashes, .. } => crate::transfer::on_manifest(state, from_node_id, file_id, hashes),
        MeshMessage::Have { file_id, chunks, bitmap } => crate::availability::on_have(state, from_node_id, file_id, *chunks, bitmap).await,
        MeshMessage::HaveAll { file_ids } => crate::availability::on_have_all(state, from_node_id, file_ids).await,
        MeshMessage::Lost { file_ids } => crate::availability::on_lost(state, from_node_id, file_ids).await,
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

        MeshMessage::SignalRelay { to_node_id, from_node_id: from, to_session_id, signal } => {
            // The mesh is one hop, so a relay always comes from this peer and is addressed to us.
            if from != from_node_id || to_node_id != &state.node_id {
                tracing::warn!("SignalRelay from {from_node_id} with mismatched addressing — dropped");
                return;
            }
            let srv_msg = match serde_json::from_str::<ServerMessage>(signal) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("SignalRelay: bad signal for tab {to_session_id}: {e}");
                    return;
                }
            };
            if !relay_is_legitimate(state, &srv_msg, from_node_id).await {
                tracing::warn!("SignalRelay from {from_node_id} refused: not a signaling message from one of its own devices");
                return;
            }
            let senders = state.local_senders.read().await;
            match senders.get(to_session_id) {
                Some(sender) => {
                    let _ = sender.send(srv_msg);
                }
                None => tracing::warn!("SignalRelay: no local tab {to_session_id}"),
            }
        }

        MeshMessage::Revocations { revocations } => receive_revocations(state, from_node_id, revocations).await,

        MeshMessage::CatalogVersion { version } => crate::catalog::sync::on_version(state, from_node_id, version).await,
        MeshMessage::CatalogOps { ops } => crate::catalog::sync::on_ops(state, from_node_id, ops).await,

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
        ServerMessage::IncomingFileOffer { from_session_id, .. } | ServerMessage::FileOfferDeclined { from_session_id, .. } => {
            from_session_id
        }
        _ => return false,
    };
    let peers = state.local_peers.read().await;
    peers.get(claimed_from).is_some_and(|p| !p.left && p.hosting_node_id.as_ref() == Some(sender_node))
}

/// Routes a WebRTC signaling payload from a local tab to a tab that may be on another node.
pub async fn route_signal(state: &NodeState, _from_session_id: &str, to_session_id: &str, srv_msg: ServerMessage) {
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
            let signal = serde_json::to_string(&srv_msg).expect("a server message always serializes");
            let peers = state.mesh_peers.read().await;
            if let Some(peer) = peers.get(&target_node_id) {
                let _ = peer.sender.send(MeshMessage::SignalRelay {
                    to_node_id: target_node_id,
                    from_node_id: state.node_id.clone(),
                    to_session_id: to_session_id.to_string(),
                    signal,
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
    async fn a_relayed_signal_reaches_the_tab_it_is_addressed_to() {
        let state = node_with_device("peer_x", "node_other", false).await;
        let (tab, mut inbox) = mpsc::unbounded_channel();
        state.local_senders.write().await.insert("tab_1".into(), tab);
        let relay = MeshMessage::SignalRelay {
            to_node_id: state.node_id.clone(),
            from_node_id: "node_other".into(),
            to_session_id: "tab_1".into(),
            signal: serde_json::to_string(&offer("peer_x")).unwrap(),
        };
        dispatch(&relay, &"node_other".to_string(), &state).await;
        assert!(matches!(inbox.try_recv(), Ok(ServerMessage::IncomingFileOffer { .. })));
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
