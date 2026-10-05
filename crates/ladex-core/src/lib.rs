use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use tokio::sync::RwLock;

pub mod types;
pub mod websocket;
pub mod handlers;
pub mod mesh;
pub mod pairing;
pub mod persist;
pub mod discovery;
pub mod state;
pub mod auth;
pub mod tls;
pub mod bitmap;
pub mod files_api;
pub mod handshake;
pub mod hlc;
pub mod identity;
pub mod mdns;
pub mod ratelimit;
pub mod server;
pub mod sessions;
pub mod store;
pub mod transfer;
pub mod trust;
pub mod zip;
pub mod validate;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

use types::*;

pub type LocalPeers = Arc<RwLock<HashMap<SessionId, PeerInfo>>>;
pub type Files      = Arc<RwLock<HashMap<String, FileMetadata>>>;
pub type Messages   = Arc<RwLock<Vec<types::TextMessage>>>;

#[derive(Clone)]
pub struct NodeState {
    pub local_peers:   LocalPeers,
    pub local_senders: types::PeerSenders,

    pub files:    Files,
    pub messages: Messages,

    /// This node's id, derived from its identity key.
    pub node_id: NodeId,

    /// The key pair behind `node_id`; the mesh handshake proves possession of it.
    pub identity: Arc<identity::Identity>,

    /// The nodes this one has accepted into its mesh, and those it has revoked.
    pub trust: Arc<trust::TrustStore>,

    /// Pairings with other nodes: whether they are accepted now, and codes waiting for an answer.
    pub pairings: Arc<pairing::Pairings>,

    /// Connected mesh peers, by node id.
    pub mesh_peers: mesh::MeshPeers,

    /// Required for browser logins and proven (never sent) in the mesh handshake; None = open node.
    pub passphrase: Option<String>,

    /// Fingerprint of this node's TLS certificate, bound into the mesh handshake; empty without TLS.
    pub tls_fingerprint: Vec<u8>,

    /// Throttles wrong guesses at the browser login.
    pub auth_limiter: Arc<ratelimit::AttemptLimiter>,

    /// Throttles wrong guesses in the mesh handshake.
    pub mesh_limiter: Arc<ratelimit::AttemptLimiter>,

    /// Browser login sessions (one per device, individually revocable).
    pub sessions: Arc<sessions::SessionStore>,

    /// Orders catalog and peer updates across nodes without trusting wall clocks.
    pub clock: Arc<hlc::Clock>,

    /// The files this node holds (see store.rs), and the transfers in progress (see transfer.rs).
    pub store: Arc<store::Store>,
    pub transfers: Arc<transfer::Transfers>,

    /// Which WebSocket connection holds each browser `session_id`.
    pub session_owners: Arc<RwLock<HashMap<SessionId, websocket::ConnOwner>>>,
    pub connection_counter: Arc<AtomicU64>,

    /// When the user was last shown a clock warning (they are rate limited).
    pub clock_alert_at: Arc<Mutex<Option<std::time::Instant>>>,

    /// Client config for dialing peers over wss://; None with --no-tls.
    pub tls_client_config: Option<Arc<tokio_rustls::rustls::ClientConfig>>,

    /// This node's public listening port, told to peers so they can redial it.
    pub http_port: u16,

    /// This node's best-guess LAN IPv4, told to peers so they can redial it.
    pub local_ip: Option<IpAddr>,

    /// This machine's hostname, shown to peers.
    pub node_name: String,
}

impl NodeState {
    pub fn next_connection_id(&self) -> u64 {
        self.connection_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// A node with no network, for unit tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(passphrase: Option<&str>) -> Self {
        Self::for_tests_node("node_test", passphrase)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_node(node_id: &str, passphrase: Option<&str>) -> Self {
        let policy = || ratelimit::Policy {
            free_attempts: 3,
            base_lockout: Duration::from_secs(30),
            max_lockout: Duration::from_secs(300),
            global_cap: None,
        };
        let node_id: NodeId = node_id.to_string();
        let data_dir = tempfile::tempdir().unwrap().keep();
        NodeState {
            local_peers: Arc::new(RwLock::new(HashMap::new())),
            local_senders: Arc::new(RwLock::new(HashMap::new())),
            files: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(Vec::new())),
            node_id: node_id.clone(),
            identity: Arc::new(identity::Identity::generate()),
            trust: Arc::new(trust::TrustStore::open(&data_dir).unwrap()),
            pairings: Arc::new(pairing::Pairings::new()),
            mesh_peers: Arc::new(RwLock::new(HashMap::new())),
            passphrase: passphrase.map(String::from),
            tls_fingerprint: Vec::new(),
            auth_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            mesh_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            sessions: Arc::new(sessions::SessionStore::new()),
            clock: Arc::new(hlc::Clock::new(node_id)),
            store: Arc::new(store::Store::open(&data_dir, 1 << 40).unwrap()),
            transfers: Arc::new(transfer::Transfers::with_tuning(transfer::Tuning {
                request_timeout: Duration::from_millis(400),
                manifest_retry: Duration::from_millis(400),
                tick: Duration::from_millis(20),
                idle_exit: Duration::from_secs(30),
                map_interval: Duration::from_millis(100),
                stall_timeout: Duration::from_secs(5),
            })),
            session_owners: Arc::new(RwLock::new(HashMap::new())),
            connection_counter: Arc::new(AtomicU64::new(1)),
            clock_alert_at: Arc::new(Mutex::new(None)),
            tls_client_config: None,
            http_port: 0,
            local_ip: None,
            node_name: "test".to_string(),
        }
    }
}
