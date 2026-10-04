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

// ---------------------------------------------------------------------------
// Shared pub type aliases (kept short for use throughout the crate)
// ---------------------------------------------------------------------------

pub type LocalPeers = Arc<RwLock<HashMap<SessionId, PeerInfo>>>;
pub type Files      = Arc<RwLock<HashMap<String, FileMetadata>>>;
pub type Messages   = Arc<RwLock<Vec<types::TextMessage>>>;

// ---------------------------------------------------------------------------
// Phase 1 — NodeState
//
// Replaces AppState.  The conceptual split is:
//
//   local_peers / local_senders  — THIS node's own browser tab(s).
//                                  Usually just one, but the design tolerates
//                                  more without breaking.
//
//   files / messages             — Distributed/merged state.  In the current
//                                  phase (MESH_MODE=false) this node is the
//                                  sole source of truth.  Phase 4 adds
//                                  last-write-wins merge across all nodes.
//
//   node_id                      — Identity of THIS node (machine) on the
//                                  mesh.  Distinct from a browser session_id.
//                                  Generated once at startup.
//
//   mesh_peers                   — Other nodes (machines) on the LAN mesh.
//                                  Populated in Phase 3; empty until then.
//
//   passphrase                   — Gates both the browser login and the mesh
//                                  handshake.  None = open node.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct NodeState {
    // ── local browser tab connections (unchanged from legacy AppState) ───
    pub local_peers:   LocalPeers,
    pub local_senders: types::PeerSenders,

    // ── distributed/merged state ─────────────────────────────────────────
    pub files:    Files,
    pub messages: Messages,

    // ── mesh identity & membership ───────────────────────────────────────
    /// Unique identifier for this node (machine).  Stable across browser
    /// reconnects — it lives in the Rust process, not the browser tab.
    pub node_id: NodeId,

    /// The key pair behind `node_id`; the mesh handshake proves possession of it.
    pub identity: Arc<identity::Identity>,

    /// The nodes this one has accepted into its mesh, and those it has revoked.
    pub trust: Arc<trust::TrustStore>,

    /// Connected mesh peer handles keyed by node_id.
    /// Empty until Phase 3 (Mesh WebSocket Layer) is implemented.
    pub mesh_peers: mesh::MeshPeers,

    // ── auth ─────────────────────────────────────────────────────────────
    /// Shared passphrase.  Required for the browser login and proven (never
    /// sent) in the mesh handshake.  None = open node.
    pub passphrase: Option<String>,

    /// Fingerprint of this node's TLS certificate, bound into the mesh
    /// handshake so a man in the middle can't pass for this node.  Empty
    /// when TLS is disabled.
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

    /// BUG-03 fix: rustls client config used by mesh::connect_to_peer to
    /// dial other nodes over wss://. `None` means TLS is disabled
    /// (--no-tls) and peers should be dialed over plain ws:// instead.
    pub tls_client_config: Option<Arc<tokio_rustls::rustls::ClientConfig>>,

    /// BUG-10 fix: this node's own HTTP/mesh listening port (the public
    /// one, i.e. what `args.port` binds — see main() for the TLS-vs-plain
    /// split). Self-reported in mesh::connect_to_peer's Hello so a peer we
    /// dial *into* knows an address to reconnect to if the connection
    /// later drops.
    pub http_port: u16,

    /// BUG-10 fix: this node's own best-guess LAN IPv4 (same one used for
    /// the "Access from network" banner and the TLS cert SANs). Also
    /// self-reported in Hello — needed because, with TLS enabled, warp
    /// only ever sees connections arriving from src/tls.rs's local
    /// TLS-terminating proxy (127.0.0.1), never the real peer, so the
    /// accepting side can't rely on the TCP-level remote address to learn
    /// where a peer that dialed *us* actually is.
    pub local_ip: Option<IpAddr>,

    /// F5: this machine's hostname, shown to peers as the "hosted on"
    /// label for every browser tab connected to this node.
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
