// ============================================================================
// LADEX — Phase 2: UDP Multicast Discovery
//
// Each node broadcasts an AnnouncePacket every 2 seconds to the
// administratively-scoped multicast group 239.255.42.99:7878.  The listen
// loop picks up announces from other nodes and hands them off to the mesh
// layer (Phase 3) to initiate a WebSocket connection.
//
// Design choices (from ROADMAP.md §2):
//
//   • Multicast group 239.255.42.99 is in the IPv4 "administratively scoped"
//     range (239.0.0.0/8).  It will not be forwarded by routers even if
//     IGMP snooping is misconfigured — ideal for LAN confinement.
//
//   • TTL is set to 1 so packets never leave the local segment.
//
//   • SO_REUSEADDR (+ SO_REUSEPORT on Linux/macOS) is required so that two
//     ladex processes on the same machine (integration tests) can both bind
//     the same multicast port.
//
//   • multicast_loop_v4(false) prevents a node from receiving its own
//     announces.  We also double-check node_id in the listen loop as
//     defense-in-depth (some OS/driver combos ignore the loop flag).
//
//   • The passphrase_hash in the announce packet is a pre-filter ONLY —
//     it reduces noise on shared networks.  It is NOT a security boundary.
//     The real auth handshake is in the mesh WebSocket (Phase 7).
//     Until Phase 7 is implemented, this field is always an empty string
//     and the filter is skipped entirely (see listen_loop).
//
//   • Staleness: if we stop receiving announces from a node for 10 seconds
//     (5 missed intervals), we treat it as gone and tear down the mesh
//     connection.  This is independent of the WebSocket-level disconnect
//     signal; whichever fires first wins.
// ============================================================================

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

use crate::mesh;
use crate::auth;
use crate::NodeState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Administratively scoped IPv4 multicast address — never forwarded beyond LAN.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);

/// Announce/listen interval.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);

/// If we haven't heard from a node in this long, mark it stale.
const STALE_THRESHOLD: Duration = Duration::from_secs(10);

/// How often the staleness sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// AnnouncePacket
// ---------------------------------------------------------------------------

/// JSON payload broadcast to the multicast group every `ANNOUNCE_INTERVAL`.
///
/// All fields are required; absent fields cause the packet to be silently
/// dropped (defensive: never panic on a malformed foreign packet).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnnouncePacket {
    /// Discriminator — must equal `"ladex_announce"`.
    #[serde(rename = "type")]
    pub packet_type: String,

    /// Unique node identifier.  On receive, skip if `node_id == self.node_id`.
    pub node_id: String,

    /// Human-readable label for this node (hostname).
    pub node_name: String,

    /// The TCP port this node's HTTP/WS server is listening on.
    /// Used to construct the WebSocket URL for the mesh connection.
    pub http_port: u16,

    /// Protocol version.  Packets with a different version are silently ignored.
    pub protocol_version: u32,

    /// SHA-1 hex of the passphrase (pre-filter only — not a security boundary).
    /// Empty string when no passphrase is configured; until Phase 7 lands,
    /// the listen loop skips this check entirely.
    pub passphrase_hash: String,
}

impl AnnouncePacket {
    pub fn is_valid(&self, own_node_id: &str) -> bool {
        self.packet_type == "ladex_announce"
            && self.protocol_version == crate::mesh::PROTOCOL_VERSION
            && self.node_id != own_node_id
            && !self.node_id.is_empty()
            && self.http_port > 0
    }
}

// ---------------------------------------------------------------------------
// DiscoveryService — wraps the multicast socket
// ---------------------------------------------------------------------------

/// Owns the multicast UDP socket.  Produces announce and listen loops that
/// run as independent Tokio tasks.
pub struct DiscoveryService {
    socket: Arc<UdpSocket>,
    discovery_port: u16,
}

impl DiscoveryService {
    /// Bind and configure the multicast socket.
    ///
    /// Sets SO_REUSEADDR so that two ladex processes on the same host can
    /// both join the multicast group (needed for on-host integration tests).
    pub async fn bind(discovery_port: u16) -> anyhow::Result<Self> {
        // Use socket2 for fine-grained socket options before converting to Tokio.
        use socket2::{Domain, Protocol, Socket, Type};
        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_reuse_address(true)?;
        #[cfg(unix)]
        sock.set_reuse_port(true)?;
        sock.set_nonblocking(true)?;
        sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, discovery_port).into())?;

        let socket = UdpSocket::from_std(sock.into())?;
        socket.join_multicast_v4(MULTICAST_ADDR, Ipv4Addr::UNSPECIFIED)?;
        // Do NOT receive our own announces (defense-in-depth check still in listen_loop)
        socket.set_multicast_loop_v4(false)?;
        // TTL=1: never leave the local segment
        socket.set_multicast_ttl_v4(1)?;

        Ok(Self {
            socket: Arc::new(socket),
            discovery_port,
        })
    }

    // ── Announce loop ─────────────────────────────────────────────────────

    /// Continuously broadcast `packet` every `ANNOUNCE_INTERVAL`.
    /// Runs forever (until the task is cancelled / dropped).
    pub async fn announce_loop(&self, packet: AnnouncePacket) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(&packet)?;
        let target = SocketAddrV4::new(MULTICAST_ADDR, self.discovery_port);
        loop {
            if let Err(e) = self.socket.send_to(&payload, target).await {
                tracing::warn!("Discovery announce failed: {e}");
            }
            tokio::time::sleep(ANNOUNCE_INTERVAL).await;
        }
    }

    // ── Listen loop ───────────────────────────────────────────────────────

    /// Listen for announces from other nodes.
    ///
    /// For each valid, previously-unseen announce:
    ///   1. Update `seen` map with `Instant::now()` (for staleness tracking).
    ///   2. Skip if already present in `mesh_peers` (avoid duplicate connects).
    ///   3. Enforce deduplication tie-break: only the lexicographically
    ///      *smaller* node_id initiates.  The larger waits 200ms then checks
    ///      again — this prevents double-connect races when both nodes discover
    ///      each other simultaneously.
    ///   4. Spawn `mesh::connect_to_peer` as a background task.
    pub async fn listen_loop(
        &self,
        state: NodeState,
        passphrase_hash: String, // Phase 7: empty until implemented
    ) -> anyhow::Result<()> {
        let mut buf = [0u8; 2048];

        // Tracks the last time we heard an announce from each node_id.
        // Keyed by node_id, value is (Instant, SocketAddr) so we can
        // correlate with mesh_peers during the staleness sweep.
        let seen: Arc<RwLock<HashMap<String, (Instant, SocketAddr)>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn the staleness sweeper as a sibling task.
        let seen_clone = seen.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            staleness_sweeper(seen_clone, state_clone).await;
        });

        loop {
            let (len, from_addr) = match self.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Discovery recv error: {e}");
                    continue;
                }
            };

            // Silently drop malformed / foreign packets — do not log at info,
            // this will be noisy if other multicast apps share the port.
            let packet: AnnouncePacket = match serde_json::from_slice(&buf[..len]) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if !packet.is_valid(&state.node_id) {
                continue;
            }

            // Phase 7: passphrase pre-filter (check 1 of 3).
            // Enforces that secured and unsecured meshes never mix:
            //   - If our hash is empty and packet's is not (or vice versa) → skip.
            //   - If both are empty → no-passphrase mesh, allow.
            //   - If both non-empty but different → skip.
            // The Hello/HelloAck check (check 2) enforces this again in the TCP handshake.
            if !auth::hashes_match(
                &passphrase_hash,
                &packet.passphrase_hash,
            ) {
                tracing::debug!(
                    "Discovery: ignoring {} — passphrase mismatch",
                    packet.node_id
                );
                continue;
            }

            // Update seen timestamp (staleness tracking)
            {
                let mut map = seen.write().await;
                map.insert(packet.node_id.clone(), (Instant::now(), from_addr));
            }

            // Already connected to this node?  Update last_seen and skip.
            {
                let peers = state.mesh_peers.read().await;
                if let Some(_handle) = peers.get(&packet.node_id) {
                    // Just bump last_seen so the staleness sweeper doesn't
                    // tear down a live connection.
                    drop(peers);
                    let mut peers = state.mesh_peers.write().await;
                    if let Some(h) = peers.get_mut(&packet.node_id) {
                        h.last_seen = Instant::now();
                    }
                    continue;
                }
            }

            // Deduplication tie-break:
            //   smaller node_id initiates immediately.
            //   larger node_id waits 200ms then checks if a connection appeared.
            let peer_node_id = packet.node_id.clone();
            let peer_ip: IpAddr = from_addr.ip();
            let peer_http_port = packet.http_port;
            let state_spawn = state.clone();
            let my_node_id = state.node_id.clone();

            tokio::spawn(async move {
                if my_node_id > peer_node_id {
                    // We are larger — wait and then check if peer already connected
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let already_connected = {
                        let peers = state_spawn.mesh_peers.read().await;
                        peers.contains_key(&peer_node_id)
                    };
                    if already_connected {
                        return; // Peer beat us to it — skip
                    }
                }

                // Check one more time right before dialing (race window is tiny)
                {
                    let peers = state_spawn.mesh_peers.read().await;
                    if peers.contains_key(&peer_node_id) {
                        return;
                    }
                }

                tracing::info!(
                    "Discovery: new peer {} at {}:{} — connecting",
                    peer_node_id,
                    peer_ip,
                    peer_http_port
                );

                if let Err(e) =
                    mesh::connect_to_peer(peer_ip, peer_http_port, state_spawn).await
                {
                    tracing::warn!(
                        "Discovery: mesh connect to {}:{} failed: {e}",
                        peer_ip,
                        peer_http_port
                    );
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Staleness sweeper
// ---------------------------------------------------------------------------

/// Periodically checks `seen` for nodes we haven't heard from in
/// `STALE_THRESHOLD` and tears down their mesh connections.
///
/// This is complementary to the WebSocket-level disconnect detection in
/// `mesh.rs` — whichever fires first triggers cleanup.  Having both means
/// we handle: (a) clean TCP closes, (b) dead connections where the TCP
/// stack hasn't noticed yet (e.g. Wi-Fi roaming), (c) nodes that stopped
/// sending announces but whose TCP connection is still technically up.
async fn staleness_sweeper(
    seen: Arc<RwLock<HashMap<String, (Instant, SocketAddr)>>>,
    state: NodeState,
) {
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;

        let stale: Vec<String> = {
            let map = seen.read().await;
            map.iter()
                .filter(|(_, (last, _))| last.elapsed() > STALE_THRESHOLD)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for node_id in &stale {
            // Only tear down if the mesh layer considers this peer connected.
            let was_connected = {
                let peers = state.mesh_peers.read().await;
                peers.contains_key(node_id)
            };
            if was_connected {
                tracing::warn!(
                    "Discovery: node {} stale ({:?} since last announce) — tearing down mesh connection",
                    node_id,
                    STALE_THRESHOLD
                );
                mesh::remove_mesh_peer(&state, node_id).await;
            }
            // Remove from seen regardless (prevents log spam for already-cleaned nodes)
            seen.write().await.remove(node_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience: build AnnouncePacket from NodeState
// ---------------------------------------------------------------------------

pub fn build_announce(state: &NodeState, http_port: u16) -> AnnouncePacket {
    AnnouncePacket {
        packet_type: "ladex_announce".to_string(),
        node_id: state.node_id.clone(),
        node_name: hostname(),
        http_port,
        protocol_version: crate::mesh::PROTOCOL_VERSION,
        // Phase 7: compute PBKDF2-SHA256 of passphrase here.
        // Until then, always empty string — the listen loop skips the check.
        passphrase_hash: state
            .passphrase_hash
            .clone()
            .unwrap_or_default(),
    }
}

/// Returns the system hostname, or a fallback label.
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
