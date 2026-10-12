//! In-process nodes for integration tests, each with a real QUIC endpoint on 127.0.0.1.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::catalog::{CatalogEntry, EntryKind};
use crate::identity::Identity;
use crate::mesh::endpoint::MeshEndpoint;
use crate::store::CHUNK_SIZE;
use crate::types::{FileView, NodeId, PeerInfo, ServerMessage, TextMessage};
use crate::{mesh, ratelimit, state, NodeState};

mod faulty_link;
mod udp_relay;
pub(crate) use faulty_link::detour;
pub use faulty_link::{FaultyLink, LinkedMesh};
pub use udp_relay::{Faults, UdpRelay};

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

static NEXT_MESSAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub struct NodeConfig {
    /// The node's name; its id comes from a key generated for it.
    pub name: String,
    pub passphrase: Option<String>,
    pub peers: Vec<SocketAddr>,
}

impl NodeConfig {
    pub fn named(name: &str) -> Self {
        Self { name: name.to_string(), ..Default::default() }
    }

    pub fn passphrase(mut self, passphrase: &str) -> Self {
        self.passphrase = Some(passphrase.to_string());
        self
    }

    pub fn peer(mut self, addr: SocketAddr) -> Self {
        self.peers.push(addr);
        self
    }
}

pub struct NodeHandle {
    pub state: NodeState,
    /// Where other nodes reach this one over QUIC.
    pub addr: SocketAddr,
    accepting: JoinHandle<()>,
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        self.accepting.abort();
        if let Some(endpoint) = &self.state.mesh_endpoint {
            endpoint.close();
        }
    }
}

/// Starts a node and dials its configured peers; a failed dial is logged, not fatal.
pub async fn spawn_node(config: NodeConfig) -> NodeHandle {
    let identity = Identity::generate();
    let endpoint = Arc::new(MeshEndpoint::bind(&identity, (Ipv4Addr::LOCALHOST, 0).into()).expect("bind a QUIC endpoint"));
    let addr = endpoint.local_addr().expect("read the QUIC address");
    let mut state = NodeState::for_tests_node(identity.node_id(), config.passphrase.as_deref());
    state.identity = Arc::new(identity);
    state.quic_port = addr.port();
    state.mesh_endpoint = Some(endpoint.clone());
    state.node_name = config.name.clone();
    state.local_ip = Some(addr.ip());
    // Every test node dials from 127.0.0.1, so that one address must not look like a guesser.
    state.mesh_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::Policy {
        free_attempts: 1000,
        base_lockout: Duration::from_secs(1),
        max_lockout: Duration::from_secs(1),
        global_cap: None,
    }));

    let accepting = tokio::spawn(mesh::quic::accept_loop(state.clone(), endpoint));
    let node = NodeHandle { state, addr, accepting };
    for peer in &config.peers {
        if let Err(e) = node.connect(*peer).await {
            tracing::warn!("Test node {}: could not join {peer}: {e}", config.name);
        }
    }
    node
}

/// `n` nodes, each connected directly to every other one. Returns once they all see each other.
pub async fn spawn_mesh(n: usize, passphrase: Option<&str>) -> Vec<NodeHandle> {
    let mut nodes: Vec<NodeHandle> = Vec::new();
    for i in 0..n {
        let mut config = NodeConfig::named(&format!("node_{i}"));
        config.passphrase = passphrase.map(String::from);
        config.peers = nodes.iter().map(|node| node.addr).collect();
        nodes.push(spawn_node(config).await);
    }
    assert!(eventually(async || fully_connected(&nodes).await).await, "the {n} nodes never all connected");
    nodes
}

/// Polls `condition` until it holds, or gives up after a few seconds.
pub async fn eventually(condition: impl AsyncFnMut() -> bool) -> bool {
    eventually_within(CONVERGENCE_TIMEOUT, condition).await
}

/// Polls `condition` until it holds, or gives up after `timeout`.
pub async fn eventually_within(timeout: Duration, mut condition: impl AsyncFnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Whether every node sees every other node as a mesh peer.
pub async fn fully_connected(nodes: &[NodeHandle]) -> bool {
    let all: BTreeSet<NodeId> = nodes.iter().map(|node| node.state.node_id.clone()).collect();
    for node in nodes {
        let mut expected = all.clone();
        expected.remove(&node.state.node_id);
        if node.mesh_peers().await != expected {
            return false;
        }
    }
    true
}

/// Whether every node has the same catalog and the same chat history.
pub async fn converged(nodes: &[NodeHandle]) -> bool {
    let Some((first, rest)) = nodes.split_first() else { return true };
    let (catalog, chat) = (first.catalog().await, first.chat().await);
    for node in rest {
        if node.catalog().await != catalog || node.chat().await != chat {
            return false;
        }
    }
    true
}

impl NodeHandle {
    pub fn node_id(&self) -> &str {
        &self.state.node_id
    }

    /// Joins whichever node is at `peer`, as `--peer` does.
    pub async fn connect(&self, peer: SocketAddr) -> anyhow::Result<()> {
        mesh::quic::join(&self.state, peer).await
    }

    /// Joins `peer` by its node id, as discovery would after hearing its announce.
    pub async fn connect_quic(&self, peer: &NodeHandle) -> anyhow::Result<()> {
        mesh::quic::connect(&self.state, peer.addr, &peer.state.node_id).await
    }

    pub async fn mesh_peers(&self) -> BTreeSet<NodeId> {
        self.state.mesh_peers.read().await.keys().cloned().collect()
    }

    /// The whole catalog with each file's hosts, in a form that compares across nodes.
    pub async fn catalog(&self) -> serde_json::Value {
        let files: BTreeMap<String, FileView> = state::file_views(&self.state).into_iter().map(|file| (file.id.clone(), file)).collect();
        serde_json::to_value(files).expect("a catalog serializes")
    }

    /// Ids of the files listed.
    pub async fn live_files(&self) -> BTreeSet<String> {
        state::file_views(&self.state).into_iter().map(|file| file.id).collect()
    }

    /// The nodes this node knows to have the whole file right now.
    pub async fn hosts_of(&self, id: &str) -> Vec<NodeId> {
        state::hosts_of(&self.state, id)
    }

    /// Chat message ids, in the order this node shows them.
    pub async fn chat(&self) -> Vec<String> {
        self.state.messages.read().await.iter().map(|m| m.id.clone()).collect()
    }

    /// Stores `content` on this node and lists it in the mesh catalog.
    pub async fn share_file(&self, id: &str, content: &[u8]) -> FileView {
        let state = &self.state;
        let blob = state.store.create(id, content.len() as u64).expect("create the file");
        for (index, chunk) in content.chunks(CHUNK_SIZE as usize).enumerate() {
            blob.write_chunk_hashing(index as u32, chunk.to_vec()).await.expect("write a chunk");
        }
        let entry = CatalogEntry {
            file_id: id.to_string(),
            name: format!("{id}.bin"),
            size: content.len() as u64,
            mime_type: "application/octet-stream".to_string(),
            created_at_ms: crate::hlc::wall_clock_ms(),
            manifest_root: blob.seal().await.expect("seal the file"),
            uploader_id: self.device_id(),
            kind: EntryKind::File { parent: None },
        };
        state::share_file(state, entry).await.expect("list the file")
    }

    /// Unshares a file, as its uploader would from the browser.
    pub async fn unshare_file(&self, id: &str) {
        let taken_down = state::unshare(&self.state, id, &self.device_id(), false).await;
        assert!(!taken_down.is_empty(), "the file is in the catalog and was shared by this node");
    }

    fn device_id(&self) -> String {
        format!("device_{}", self.state.node_id)
    }

    /// Posts a chat message from a device on this node.
    pub async fn say(&self, text: &str) -> TextMessage {
        let message = TextMessage {
            id: format!("msg_{}", NEXT_MESSAGE.fetch_add(1, Ordering::Relaxed)),
            content: text.to_string(),
            sender_id: format!("device_{}", self.state.node_id),
            sender_name: None,
            timestamp: chrono::Utc::now(),
            created_at: crate::hlc::wall_clock_ms(),
        };
        state::apply_chat_message(&self.state, message.clone()).await;
        state::push_message_to_mesh(&self.state.mesh_peers, message.clone()).await;
        message
    }

    /// Connects a browser device to this node and tells the mesh it is here.
    pub async fn connect_device(&self, session_id: &str) -> PeerInfo {
        let device = PeerInfo {
            session_id: session_id.to_string(),
            connected_at: chrono::Utc::now(),
            user_agent: Some("test browser".to_string()),
            hosting_node_id: Some(self.state.node_id.clone()),
            node_rtt_ms: None,
            left: false,
            left_at: None,
            hosting_node_name: Some(self.state.node_name.clone()),
            nickname: None,
            version: self.state.clock.now(),
        };
        self.state.local_peers.write().await.insert(device.session_id.clone(), device.clone());
        state::push_peer_to_mesh(&self.state.mesh_peers, device.clone()).await;
        device
    }

    /// Connects a browser device and returns what the node sends to its tab.
    pub async fn open_tab(&self, session_id: &str) -> tokio::sync::mpsc::UnboundedReceiver<ServerMessage> {
        let (tab, inbox) = tokio::sync::mpsc::unbounded_channel();
        self.state.local_senders.write().await.insert(session_id.to_string(), tab);
        self.connect_device(session_id).await;
        inbox
    }

    /// Devices this node knows of, wherever they are hosted.
    pub async fn devices(&self) -> BTreeSet<String> {
        self.state.local_peers.read().await.keys().cloned().collect()
    }

    /// The devices known right now and still connected, without waiting for anything.
    pub fn devices_now(&self) -> BTreeSet<String> {
        let devices =
            self.state.local_peers.try_read().map(|peers| peers.values().filter(|d| !d.left).map(|d| d.session_id.clone()).collect());
        devices.unwrap_or_default()
    }

    /// Leaves the mesh the way a node does on Ctrl-C.
    pub async fn say_goodbye(&self) {
        mesh::broadcast_goodbye(&self.state).await;
    }
}
