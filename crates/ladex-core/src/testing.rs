// In-process nodes for integration tests. Each node serves the real /mesh
// endpoint on 127.0.0.1 with discovery off, and dials only the peers it is given.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use warp::Filter;

use crate::store::CHUNK_SIZE;
use crate::types::{FileMetadata, Holder, NodeId, TextMessage};
use crate::{files_api, mesh, ratelimit, server, state, tls, NodeState};

mod faulty_link;
pub use faulty_link::{Faults, FaultyLink, LinkedMesh};

// How long `eventually` waits for the mesh to settle.
const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

static NEXT_MESSAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub struct NodeConfig {
    pub node_id: NodeId,
    pub passphrase: Option<String>,
    pub peers: Vec<SocketAddr>,
    pub tls: bool,
}

impl NodeConfig {
    pub fn named(node_id: &str) -> Self {
        Self { node_id: node_id.to_string(), ..Default::default() }
    }

    pub fn passphrase(mut self, passphrase: &str) -> Self {
        self.passphrase = Some(passphrase.to_string());
        self
    }

    pub fn peer(mut self, addr: SocketAddr) -> Self {
        self.peers.push(addr);
        self
    }

    pub fn tls(mut self) -> Self {
        self.tls = true;
        self
    }
}

pub struct NodeHandle {
    pub state: NodeState,
    pub addr: SocketAddr,
    server: JoinHandle<()>,
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Starts a node and dials its configured peers. A failed dial is logged, not
/// fatal, just as for `--peer` on the command line.
pub async fn spawn_node(config: NodeConfig) -> NodeHandle {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the bound address");

    let mut state = NodeState::for_tests_node(&config.node_id, config.passphrase.as_deref());
    state.http_port = addr.port();
    state.local_ip = Some(addr.ip());
    // Every test node dials from 127.0.0.1, so that one address must not look like a guesser.
    state.mesh_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::Policy {
        free_attempts: 1000,
        base_lockout: Duration::from_secs(1),
        max_lockout: Duration::from_secs(1),
        global_cap: None,
    }));

    let acceptor = if config.tls {
        tls::install_crypto_provider();
        let (server_config, fingerprint) =
            tls::ephemeral_server_identity(&["127.0.0.1".to_string()]).expect("generate a certificate");
        state.tls_fingerprint = fingerprint;
        state.tls_client_config = Some(tls::build_client_config());
        Some(TlsAcceptor::from(server_config))
    } else {
        None
    };

    let mesh_state = state.clone();
    let routes = warp::path("mesh")
        .and(warp::ws())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || mesh_state.clone()))
        .and_then(mesh::mesh_ws_handler);
    let service = warp::service(routes);
    let api = files_api::FilesApi::new(state.clone());
    let server = tokio::spawn(async move {
        let _ = server::serve(listener, acceptor, None, service, api).await;
    });

    let node = NodeHandle { state, addr, server };
    for peer in &config.peers {
        if let Err(e) = node.connect(*peer).await {
            tracing::warn!("Test node {}: could not join {peer}: {e}", config.node_id);
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
pub async fn eventually(mut condition: impl AsyncFnMut() -> bool) -> bool {
    let deadline = Instant::now() + CONVERGENCE_TIMEOUT;
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

    pub async fn connect(&self, peer: SocketAddr) -> anyhow::Result<()> {
        mesh::connect_to_peer(peer.ip(), peer.port(), self.state.clone()).await
    }

    pub async fn mesh_peers(&self) -> BTreeSet<NodeId> {
        self.state.mesh_peers.read().await.keys().cloned().collect()
    }

    /// The whole catalog, tombstones and holders included, in a form that compares across nodes.
    pub async fn catalog(&self) -> serde_json::Value {
        let files: BTreeMap<String, FileMetadata> =
            self.state.files.read().await.iter().map(|(id, file)| (id.clone(), file.clone())).collect();
        serde_json::to_value(files).expect("a catalog serializes")
    }

    /// Ids of the files listed and not unshared.
    pub async fn live_files(&self) -> BTreeSet<String> {
        self.state.files.read().await.values().filter(|f| !f.deleted).map(|f| f.id.clone()).collect()
    }

    /// Chat message ids, in the order this node shows them.
    pub async fn chat(&self) -> Vec<String> {
        self.state.messages.read().await.iter().map(|m| m.id.clone()).collect()
    }

    /// Stores `content` on this node and lists it in the mesh catalog.
    pub async fn share_file(&self, id: &str, content: &[u8]) -> FileMetadata {
        let state = &self.state;
        let blob = state.store.create(id, content.len() as u64).expect("create the file");
        for (index, chunk) in content.chunks(CHUNK_SIZE as usize).enumerate() {
            blob.write_chunk_hashing(index as u32, chunk.to_vec()).await.expect("write a chunk");
        }
        let root = blob.seal().expect("seal the file");
        let stamp = state.clock.now();
        let entry = FileMetadata {
            id: id.to_string(),
            name: format!("{id}.bin"),
            size: content.len() as u64,
            mime_type: "application/octet-stream".to_string(),
            uploader_id: format!("device_{}", state.node_id),
            uploader_node: state.node_id.clone(),
            holders: [(state.node_id.clone(), Holder { since: stamp.clone(), present: true })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: stamp.wall,
            version: stamp,
            deleted: false,
            deleted_at: 0,
            manifest_root: Some(root),
            is_folder: false,
            parent: None,
            folder_bytes: 0,
            folder_files: 0,
        };
        state::publish_entry(state, entry.clone()).await;
        entry
    }

    /// Unshares a file, as its uploader would from the browser.
    pub async fn unshare_file(&self, id: &str) {
        let state = &self.state;
        let tombstone = {
            let mut files = state.files.write().await;
            let file = files.get_mut(id).expect("the file is in the catalog");
            file.version = state.clock.now();
            file.deleted = true;
            file.deleted_at = file.version.wall;
            file.clone()
        };
        state::forget_file(state, id).await;
        state::push_files_to_mesh(&state.mesh_peers, vec![tombstone]).await;
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

    /// Leaves the mesh the way a node does on Ctrl-C.
    pub async fn say_goodbye(&self) {
        mesh::broadcast_goodbye(&self.state).await;
    }
}
