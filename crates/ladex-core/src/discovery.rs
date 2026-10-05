//! UDP multicast discovery: nodes announce themselves on the LAN and dial the nodes they hear.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

use crate::mesh;
use crate::NodeState;
use crate::types::hostname;

// After a failed authentication, wait before redialing: 15 s, doubling up to 10 minutes.
const AUTH_BACKOFF_BASE: Duration = Duration::from_secs(15);
const AUTH_BACKOFF_MAX: Duration = Duration::from_secs(600);

type AuthBackoff = Arc<Mutex<HashMap<String, (u32, Instant)>>>;

fn backoff_active(backoff: &AuthBackoff, node_id: &str) -> bool {
    backoff.lock().unwrap().get(node_id).is_some_and(|(_, until)| *until > Instant::now())
}

fn record_auth_failure(backoff: &AuthBackoff, node_id: &str) {
    let mut map = backoff.lock().unwrap();
    let failures = map.get(node_id).map_or(0, |(n, _)| *n) + 1;
    let delay = AUTH_BACKOFF_BASE.saturating_mul(1 << (failures - 1).min(10)).min(AUTH_BACKOFF_MAX);
    map.insert(node_id.to_string(), (failures, Instant::now() + delay));
}

/// Administratively scoped IPv4 multicast address — never forwarded beyond LAN.
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);

/// Announce/listen interval.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);

/// If we haven't heard from a node in this long, mark it stale.
const STALE_THRESHOLD: Duration = Duration::from_secs(10);

/// How often the staleness sweeper runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// JSON payload broadcast to the multicast group every `ANNOUNCE_INTERVAL`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnnouncePacket {
    /// Discriminator — must equal `"ladex_announce"`.
    #[serde(rename = "type")]
    pub packet_type: String,

    /// Unique node identifier.
    pub node_id: String,

    /// Human-readable label for this node (hostname).
    pub node_name: String,

    /// The TCP port this node's HTTP/WS server listens on.
    pub http_port: u16,

    /// Protocol version; packets with another version are ignored.
    pub protocol_version: u32,

    /// Whether this node requires a passphrase.  Not a secret.
    pub secured: bool,
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

/// Owns the multicast UDP socket and runs the announce and listen loops.
pub struct DiscoveryService {
    socket: Arc<UdpSocket>,
    discovery_port: u16,
}

impl DiscoveryService {
    /// Binds the multicast socket, with SO_REUSEADDR so several nodes can share one host.
    pub async fn bind(discovery_port: u16) -> anyhow::Result<Self> {
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

    /// Broadcasts `packet` every `ANNOUNCE_INTERVAL`, forever.
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

    /// Listens for announces and dials newly seen nodes; the smaller node id dials first, to avoid double connects.
    pub async fn listen_loop(
        &self,
        state: NodeState,
    ) -> anyhow::Result<()> {
        let mut buf = [0u8; 2048];

        let seen: Arc<RwLock<HashMap<String, (Instant, SocketAddr)>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let auth_backoff: AuthBackoff = Arc::new(Mutex::new(HashMap::new()));

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

            // Other multicast apps may share the port, so foreign packets are dropped without logging.
            let packet: AnnouncePacket = match serde_json::from_slice(&buf[..len]) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if !packet.is_valid(&state.node_id) {
                continue;
            }

            // Secured and open meshes never mix.
            if packet.secured != state.passphrase.is_some() {
                tracing::debug!("Discovery: ignoring {} — security mode mismatch", packet.node_id);
                continue;
            }

            {
                let mut map = seen.write().await;
                map.insert(packet.node_id.clone(), (Instant::now(), from_addr));
            }

            {
                let peers = state.mesh_peers.read().await;
                if let Some(_handle) = peers.get(&packet.node_id) {
                    // Keeps the staleness sweeper from tearing down a live connection.
                    drop(peers);
                    let mut peers = state.mesh_peers.write().await;
                    if let Some(h) = peers.get_mut(&packet.node_id) {
                        h.last_seen = Instant::now();
                    }
                    continue;
                }
            }

            if backoff_active(&auth_backoff, &packet.node_id) {
                continue;
            }

            let peer_node_id = packet.node_id.clone();
            let peer_ip: IpAddr = from_addr.ip();
            let peer_http_port = packet.http_port;
            let state_spawn = state.clone();
            let my_node_id = state.node_id.clone();
            let auth_backoff = auth_backoff.clone();

            tokio::spawn(async move {
                if my_node_id > peer_node_id {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let already_connected = {
                        let peers = state_spawn.mesh_peers.read().await;
                        peers.contains_key(&peer_node_id)
                    };
                    if already_connected {
                        return;
                    }
                }

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

                match mesh::connect_to_peer(peer_ip, peer_http_port, state_spawn).await {
                    Ok(()) => {
                        auth_backoff.lock().unwrap().remove(&peer_node_id);
                    }
                    Err(e) => {
                        if e.downcast_ref::<mesh::AuthFailure>().is_some() {
                            record_auth_failure(&auth_backoff, &peer_node_id);
                        }
                        tracing::warn!(
                            "Discovery: mesh connect to {}:{} failed: {e}",
                            peer_ip,
                            peer_http_port
                        );
                    }
                }
            });
        }
    }
}

/// Tears down mesh connections to nodes not heard from in `STALE_THRESHOLD`.
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
            seen.write().await.remove(node_id);
        }
    }
}

pub fn build_announce(state: &NodeState, http_port: u16) -> AnnouncePacket {
    AnnouncePacket {
        packet_type: "ladex_announce".to_string(),
        node_id: state.node_id.clone(),
        node_name: hostname(),
        http_port,
        protocol_version: crate::mesh::PROTOCOL_VERSION,
        secured: state.passphrase.is_some(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn backoff_secs(backoff: &AuthBackoff, node_id: &str) -> u64 {
        backoff.lock().unwrap()[node_id].1.duration_since(Instant::now()).as_secs()
    }

    #[test]
    fn auth_backoff_doubles_and_is_capped() {
        let backoff: AuthBackoff = Arc::new(Mutex::new(HashMap::new()));
        assert!(!backoff_active(&backoff, "node"));

        record_auth_failure(&backoff, "node");
        assert!(backoff_active(&backoff, "node"));
        assert!(backoff_secs(&backoff, "node") <= 15);

        record_auth_failure(&backoff, "node");
        assert!(backoff_secs(&backoff, "node") > 15 && backoff_secs(&backoff, "node") <= 30);

        for _ in 0..20 {
            record_auth_failure(&backoff, "node");
        }
        assert!(backoff_secs(&backoff, "node") <= AUTH_BACKOFF_MAX.as_secs());
        assert!(!backoff_active(&backoff, "other-node"));
    }

    const OWN_ID: &str = "node_own";

    fn valid_packet() -> AnnouncePacket {
        AnnouncePacket {
            packet_type: "ladex_announce".into(),
            node_id: "node_other".into(),
            node_name: "laptop".into(),
            http_port: 8080,
            protocol_version: crate::mesh::PROTOCOL_VERSION,
            secured: true,
        }
    }

    fn parse(bytes: &[u8]) -> Option<AnnouncePacket> {
        serde_json::from_slice(bytes).ok()
    }

    #[test]
    fn a_well_formed_packet_from_another_node_is_valid() {
        assert!(valid_packet().is_valid(OWN_ID));
    }

    #[test]
    fn our_own_announce_is_ignored() {
        let packet = AnnouncePacket { node_id: OWN_ID.into(), ..valid_packet() };
        assert!(!packet.is_valid(OWN_ID));
    }

    #[test]
    fn a_packet_of_another_type_is_invalid() {
        let packet = AnnouncePacket { packet_type: "something_else".into(), ..valid_packet() };
        assert!(!packet.is_valid(OWN_ID));
    }

    #[test]
    fn a_packet_from_another_protocol_version_is_invalid() {
        let packet = AnnouncePacket { protocol_version: crate::mesh::PROTOCOL_VERSION + 1, ..valid_packet() };
        assert!(!packet.is_valid(OWN_ID));
    }

    #[test]
    fn a_packet_without_a_node_id_is_invalid() {
        let packet = AnnouncePacket { node_id: String::new(), ..valid_packet() };
        assert!(!packet.is_valid(OWN_ID));
    }

    #[test]
    fn a_packet_announcing_port_zero_is_invalid() {
        let packet = AnnouncePacket { http_port: 0, ..valid_packet() };
        assert!(!packet.is_valid(OWN_ID));
    }

    #[test]
    fn a_valid_packet_round_trips_through_json() {
        let bytes = serde_json::to_vec(&valid_packet()).unwrap();
        assert!(parse(&bytes).unwrap().is_valid(OWN_ID));
    }

    #[test]
    fn garbage_bytes_do_not_parse() {
        assert!(parse(b"").is_none());
        assert!(parse(b"\xff\xfe\x00").is_none());
        assert!(parse(b"not json").is_none());
        assert!(parse(b"[]").is_none());
        assert!(parse(b"null").is_none());
    }

    #[test]
    fn a_packet_missing_a_field_does_not_parse() {
        let without_port = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","protocol_version":4,"secured":false}"#;
        assert!(parse(without_port).is_none());
    }

    #[test]
    fn fields_of_the_wrong_type_do_not_parse() {
        let port_as_string = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","http_port":"80","protocol_version":4,"secured":false}"#;
        let secured_as_number = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","http_port":80,"protocol_version":4,"secured":1}"#;
        assert!(parse(port_as_string).is_none());
        assert!(parse(secured_as_number).is_none());
    }

    #[test]
    fn a_port_outside_the_u16_range_does_not_parse() {
        let port_too_big = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","http_port":70000,"protocol_version":4,"secured":false}"#;
        let negative_port = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","http_port":-1,"protocol_version":4,"secured":false}"#;
        assert!(parse(port_too_big).is_none());
        assert!(parse(negative_port).is_none());
    }

    #[test]
    fn unknown_extra_fields_are_ignored() {
        let with_old_hash = br#"{"type":"ladex_announce","node_id":"n","node_name":"x","http_port":80,"protocol_version":4,"secured":false,"passphrase_hash":"abc"}"#;
        assert!(parse(with_old_hash).is_some());
    }

    #[test]
    fn announce_carries_only_public_fields() {
        let packet = AnnouncePacket {
            packet_type: "ladex_announce".into(),
            node_id: "node_1".into(),
            node_name: "laptop".into(),
            http_port: 8080,
            protocol_version: crate::mesh::PROTOCOL_VERSION,
            secured: true,
        };
        let json = serde_json::to_value(&packet).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["http_port", "node_id", "node_name", "protocol_version", "secured", "type"]);
        assert!(packet.is_valid("node_2"));
    }
}
