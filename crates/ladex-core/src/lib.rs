use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use tokio::sync::RwLock;

pub mod auth;
pub mod availability;
pub mod bitmap;
pub mod catalog;
pub mod discovery;
pub mod files_api;
pub mod handlers;
pub mod handshake;
pub mod hlc;
pub mod identity;
pub mod manifest;
pub mod mdns;
pub mod mesh;
pub mod pairing;
pub mod persist;
pub mod push;
pub mod ratelimit;
pub mod receive;
pub mod revocation;
pub mod server;
pub mod sessions;
pub mod state;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
pub mod tls;
pub mod transfer;
pub mod trust;
pub mod types;
pub mod validate;
pub mod websocket;
pub mod zip;

use types::*;

pub type LocalPeers = Arc<RwLock<HashMap<SessionId, PeerInfo>>>;
pub type Messages = Arc<RwLock<Vec<types::TextMessage>>>;

#[derive(Clone)]
pub struct NodeState {
    pub local_peers: LocalPeers,
    pub local_senders: types::PeerSenders,

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

    /// Peers this node is dialing, as the lower node id of the two.
    pub dial_targets: mesh::dialer::DialTargets,

    /// The QUIC endpoint other nodes connect to; None for unit tests that need no network.
    pub mesh_endpoint: Option<Arc<mesh::endpoint::MeshEndpoint>>,

    /// The UDP port of `mesh_endpoint`, announced by discovery.
    pub quic_port: u16,

    /// Required for browser logins and proven (never sent) in the mesh handshake; None = open node.
    pub passphrase: Option<String>,

    /// Fingerprint of the TLS certificate the browser UI is served with; empty without TLS.
    pub tls_fingerprint: Vec<u8>,

    /// Throttles wrong guesses at the browser login.
    pub auth_limiter: Arc<ratelimit::AttemptLimiter>,

    /// Throttles wrong guesses in the mesh handshake.
    pub mesh_limiter: Arc<ratelimit::AttemptLimiter>,

    /// Browser login sessions (one per device, individually revocable).
    pub sessions: Arc<sessions::SessionStore>,

    /// Orders catalog and peer updates across nodes without trusting wall clocks.
    pub clock: Arc<hlc::Clock>,

    /// Which connected nodes have which files, from their `Have` messages (see availability.rs).
    pub availability: Arc<availability::Availability>,

    /// The files this node holds (see store.rs), and the transfers in progress (see transfer.rs).
    pub store: Arc<store::Store>,
    pub transfers: Arc<transfer::Transfers>,

    /// Files being sent to one device, outside the catalog (see push.rs).
    pub pushes: Arc<push::Pushes>,

    /// The signed-op catalog that replaces `files`, and is kept in step across the mesh by version vectors (see catalog.rs).
    pub catalog: Arc<catalog::shared::SharedCatalog>,

    /// Which WebSocket connection holds each browser `session_id`.
    pub session_owners: Arc<RwLock<HashMap<SessionId, websocket::ConnOwner>>>,
    pub connection_counter: Arc<AtomicU64>,

    /// When the user was last shown a clock warning (they are rate limited).
    pub clock_alert_at: Arc<Mutex<Option<std::time::Instant>>>,

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
        Self::for_tests_with(identity::Identity::generate(), node_id.to_string(), passphrase)
    }

    /// A node with no network whose id is derived from its key, so it can sign catalog changes.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_signing(passphrase: Option<&str>) -> Self {
        let identity = identity::Identity::generate();
        let node_id = identity.node_id().to_string();
        Self::for_tests_with(identity, node_id, passphrase)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn for_tests_with(identity: identity::Identity, node_id: NodeId, passphrase: Option<&str>) -> Self {
        let policy = || ratelimit::Policy {
            free_attempts: 3,
            base_lockout: Duration::from_secs(30),
            max_lockout: Duration::from_secs(300),
            global_cap: None,
        };
        let data_dir = tempfile::tempdir().unwrap().keep();
        NodeState {
            local_peers: Arc::new(RwLock::new(HashMap::new())),
            local_senders: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(Vec::new())),
            node_id: node_id.clone(),
            identity: Arc::new(identity),
            trust: Arc::new(trust::TrustStore::open(&data_dir).unwrap()),
            pairings: Arc::new(pairing::Pairings::new()),
            mesh_peers: Arc::new(RwLock::new(HashMap::new())),
            dial_targets: Default::default(),
            mesh_endpoint: None,
            quic_port: 0,
            passphrase: passphrase.map(String::from),
            tls_fingerprint: Vec::new(),
            auth_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            mesh_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            sessions: Arc::new(sessions::SessionStore::new()),
            clock: Arc::new(hlc::Clock::new(node_id)),
            availability: Arc::new(availability::Availability::default()),
            store: Arc::new(store::Store::open(&data_dir, 1 << 40).unwrap()),
            transfers: Arc::new(transfer::Transfers::with_tuning(transfer::Tuning {
                manifest_retry: Duration::from_millis(400),
                tick: Duration::from_millis(20),
                idle_exit: Duration::from_secs(30),
                map_interval: Duration::from_millis(100),
                stall_timeout: Duration::from_secs(5),
                stream_stall_timeout: Duration::from_secs(10),
                upload_slots: transfer::UPLOAD_SLOTS,
            })),
            pushes: Arc::new(push::Pushes::default()),
            catalog: Arc::new(catalog::shared::SharedCatalog::open(&data_dir, state::MAX_CATALOG_ENTRIES).unwrap()),
            session_owners: Arc::new(RwLock::new(HashMap::new())),
            connection_counter: Arc::new(AtomicU64::new(1)),
            clock_alert_at: Arc::new(Mutex::new(None)),
            http_port: 0,
            local_ip: None,
            node_name: "test".to_string(),
        }
    }
}
