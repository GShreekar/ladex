//! Starts a node (identity, mesh, store, catalog, discovery, web server, control socket) and hands back a handle to stop it.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;

use crate::types::{self, NodeId, ServerMessage};
use crate::{
    availability, catalog, control, discovery, files_api, guest, hlc, identity, mdns, mesh, pairing, persist, push, ratelimit, server,
    sessions, state, store, tls, transfer, trust, websocket, NodeState,
};

pub mod bridge;
pub mod local;
pub mod lock;
pub mod web;

pub use bridge::{Bridge, Reply, SocketEvent};
pub use local::{Download, LinkError, LocalLink, LocalResponse, LocalUpload, SavedFile, SocketReader, SocketWriter};
pub use lock::DataDirLock;
pub use web::UiAssets;

pub const DEFAULT_HTTP_PORT: u16 = 8080;
pub const DEFAULT_DISCOVERY_PORT: u16 = 7878;
pub const DEFAULT_STORAGE_LIMIT_GIB: u64 = 20;
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_GUEST_SESSION_HOURS: u64 = 8;

pub const DATA_DIR_VARIABLE: &str = "LADEX_DATA_DIR";
const BYTES_PER_GIB: u64 = 1 << 30;
const MESH_GOODBYE_GRACE: Duration = Duration::from_secs(1);
const MANUAL_PEER_DELAY: Duration = Duration::from_millis(500);
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(60);
const ISOLATION_DIAGNOSTIC_DELAY: Duration = Duration::from_secs(10);

/// How a node is set up; `NodeConfig::new` gives the defaults the `ladex` command uses.
pub struct NodeConfig {
    pub data_dir: PathBuf,
    /// Where the guest page's TLS certificate is kept, so guests see the same one each run.
    pub certificate_dir: PathBuf,
    /// The built web UI the node serves to browsers.
    pub ui_assets: Arc<dyn UiAssets>,
    /// Required for browser logins and proven (never sent) in the mesh handshake; None = open node.
    pub passphrase: Option<String>,
    /// The TCP port of the node's web page and guest API; 0 picks a free one.
    pub http_port: u16,
    /// The UDP port other nodes connect to; the same number as `http_port` when None.
    pub quic_port: Option<u16>,
    /// The loopback-only plain-HTTP port, used with TLS; one above `http_port` when None.
    pub local_http_port: Option<u16>,
    /// The UDP multicast port peers are found on; None turns discovery off.
    pub discovery_port: Option<u16>,
    pub manual_peers: Vec<SocketAddr>,
    pub use_tls: bool,
    pub use_keychain: bool,
    pub storage_limit_bytes: u64,
    pub stall_timeout: Duration,
    pub guest_session_ttl: Duration,
    /// The node may vanish at any moment, as an app on a phone does; peers then count on it less.
    pub is_part_time: bool,
}

impl NodeConfig {
    pub fn new(data_dir: PathBuf, ui_assets: Arc<dyn UiAssets>) -> Self {
        Self {
            data_dir,
            certificate_dir: tls::config_dir(),
            ui_assets,
            passphrase: None,
            http_port: DEFAULT_HTTP_PORT,
            quic_port: None,
            local_http_port: None,
            discovery_port: Some(DEFAULT_DISCOVERY_PORT),
            manual_peers: Vec::new(),
            use_tls: true,
            use_keychain: true,
            storage_limit_bytes: DEFAULT_STORAGE_LIMIT_GIB * BYTES_PER_GIB,
            stall_timeout: Duration::from_secs(DEFAULT_STALL_TIMEOUT_SECS),
            guest_session_ttl: Duration::from_secs(DEFAULT_GUEST_SESSION_HOURS * 60 * 60),
            is_part_time: false,
        }
    }
}

impl NodeConfig {
    /// Takes the passphrase saved in the data directory, unless one was given; true when the saved one is used.
    pub fn use_saved_passphrase(&mut self) -> anyhow::Result<bool> {
        if self.passphrase.is_some() {
            return Ok(false);
        }
        self.passphrase = crate::saved_passphrase::load(&self.data_dir, self.use_keychain)?;
        Ok(self.passphrase.is_some())
    }

    /// Takes the storage limit chosen in the app or with `ladex settings`.
    pub fn use_saved_storage_limit(&mut self) {
        self.storage_limit_bytes = crate::preferences::load(&self.data_dir).storage_limit_bytes();
    }
}

/// Why a node could not start; nothing is left running.
#[derive(Debug)]
pub enum StartError {
    EmptyPassphrase,
    /// Another node already owns the data directory; `running` is where it can be reached, when it has said so.
    DataDirInUse {
        data_dir: PathBuf,
        running: Option<control::server::Endpoint>,
    },
    DataDirUnavailable {
        data_dir: PathBuf,
        source: std::io::Error,
    },
    Identity(anyhow::Error),
    Trust(anyhow::Error),
    MeshEndpoint(anyhow::Error),
    Store {
        path: PathBuf,
        source: anyhow::Error,
    },
    Catalog {
        path: PathBuf,
        source: anyhow::Error,
    },
    Listen {
        port: u16,
        source: std::io::Error,
    },
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPassphrase => f.write_str("the passphrase must not be empty"),
            Self::DataDirInUse { data_dir, .. } => {
                write!(f, "another LADEX node (the app or `ladex`) is already running with {}", data_dir.display())
            }
            Self::DataDirUnavailable { data_dir, source } => write!(f, "could not use the data folder {}: {source}", data_dir.display()),
            Self::Identity(e) | Self::Trust(e) | Self::MeshEndpoint(e) => write!(f, "{e:#}"),
            Self::Store { path, source } => write!(f, "could not open the file store at {}: {source}", path.display()),
            Self::Catalog { path, source } => write!(f, "could not open the catalog at {}: {source:#}", path.display()),
            Self::Listen { port, source } => write!(f, "could not listen on port {port}: {source}"),
        }
    }
}

impl std::error::Error for StartError {}

/// Something that went wrong without stopping the node from starting.
#[derive(Debug)]
pub enum Notice {
    /// No certificate could be made, so browsers are served plain HTTP.
    CertificateUnavailable(String),
    /// Commands such as `ladex send` cannot reach this node.
    ControlUnavailable(String),
    /// The loopback listener could not be opened on this port.
    LocalListenerUnavailable { port: u16, reason: String },
}

/// What a started node reports about itself.
#[derive(Debug)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub key_location: String,
    pub data_dir: PathBuf,
    pub http_port: u16,
    pub quic_port: u16,
    /// The loopback plain-HTTP port, when that listener is open.
    pub local_http_port: Option<u16>,
    pub local_ips: Vec<IpAddr>,
    pub primary_ip: Option<IpAddr>,
    pub is_tls_enabled: bool,
    pub tls_fingerprint: Vec<u8>,
    /// Files shared by an earlier version that were listed in the signed catalog on this start.
    pub imported_legacy_files: usize,
    pub notices: Vec<Notice>,
}

/// A running node; `shutdown` ends it cleanly.
pub struct NodeHandle {
    info: NodeInfo,
    state: NodeState,
    mesh_endpoint: Arc<mesh::endpoint::MeshEndpoint>,
    saver: persist::Flusher,
    mdns: Option<mdns::MdnsHandle>,
    web_server: JoinHandle<anyhow::Result<()>>,
    local_web_server: Option<JoinHandle<()>>,
    stop_web: watch::Sender<bool>,
    open_pipe: local::OpenPipe,
    // The runtime the node runs on, for work a caller outside it (such as a GUI thread) starts.
    runtime: tokio::runtime::Handle,
    background_tasks: Vec<JoinHandle<()>>,
    // Kept to the end so the directory is released only after everything that uses it has stopped.
    _data_dir_lock: DataDirLock,
}

impl NodeHandle {
    pub fn info(&self) -> &NodeInfo {
        &self.info
    }

    pub fn state(&self) -> &NodeState {
        &self.state
    }

    /// Stops or restarts serving chunks to other nodes and announcing this one, as when a phone leaves its Wi-Fi and comes back.
    pub fn pause_seeding(&self, is_paused: bool) {
        if self.state.is_seeding_paused.swap(is_paused, Ordering::Relaxed) == is_paused {
            return;
        }
        tracing::info!("Seeding {}", if is_paused { "paused" } else { "resumed" });
        if !is_paused {
            self.refresh_address();
        }
    }

    /// Takes up the address this device has on the local network now, which changes when it moves to another network.
    pub fn refresh_address(&self) {
        let ip = tls::local_ipv4_addresses().first().copied().or_else(guess_local_ip);
        if ip != self.state.local_ip() {
            tracing::info!("Network address is now {}", ip.map_or_else(|| "none".to_string(), |ip| ip.to_string()));
            self.state.set_local_ip(ip);
        }
    }

    /// An in-process way into this node's web server, for a window that lives in the same process.
    pub fn local_link(&self) -> LocalLink {
        LocalLink::new(self.state.clone(), self.open_pipe.clone(), self.stop_web.subscribe(), self.runtime.clone())
    }

    /// Resolves when the web server stops, which it only does on an error.
    pub async fn wait(&mut self) -> anyhow::Result<()> {
        (&mut self.web_server).await?
    }

    /// Tells the mesh goodbye, saves state, removes the control socket's endpoint file and waits for every task to end.
    pub async fn shutdown(self) {
        mesh::broadcast_goodbye(&self.state).await;
        self.mesh_endpoint.close();
        let _ = tokio::time::timeout(MESH_GOODBYE_GRACE, self.mesh_endpoint.wait_idle()).await;
        self.saver.flush().await;
        control::server::close(&self.info.data_dir);
        if let Some(handle) = self.mdns {
            mdns::shutdown(handle).await;
        }
        self.saver.stop().await;
        let _ = self.stop_web.send(true);
        let _ = self.web_server.await;
        if let Some(local) = self.local_web_server {
            let _ = local.await;
        }
        for task in self.background_tasks {
            task.abort();
            let _ = task.await;
        }
    }
}

/// The entry point for running a node.
pub struct Node;

impl Node {
    pub async fn start(config: NodeConfig) -> Result<NodeHandle, StartError> {
        if config.passphrase.as_deref() == Some("") {
            return Err(StartError::EmptyPassphrase);
        }
        let data_dir = config.data_dir.clone();
        let data_dir_lock = claim_data_dir(&data_dir)?;
        let mut notices = Vec::new();

        let (identity, key_location) = identity::load_or_create(&data_dir, config.use_keychain).map_err(StartError::Identity)?;
        let node_id: NodeId = identity.node_id().to_string();
        let trust = Arc::new(trust::TrustStore::open(&data_dir).map_err(StartError::Trust)?);
        let saved = persist::load(&data_dir);
        tracing::info!("Node ID: {node_id} (key kept in {key_location})");

        let local_ips = tls::local_ipv4_addresses();
        let primary_ip = local_ips.first().copied().or_else(guess_local_ip);
        let (tls_config, tls_fingerprint) = prepare_tls(&config, &local_ips, &mut notices);
        let is_tls_enabled = tls_config.is_some();

        let public_listener = bind_public(config.http_port).await?;
        let http_port = public_listener.local_addr().map_err(|source| StartError::Listen { port: config.http_port, source })?.port();
        let local_listener = bind_local(&config, http_port, is_tls_enabled, &mut notices).await;
        let local_http_port = local_listener.as_ref().and_then(|listener| listener.local_addr().ok()).map(|addr| addr.port());

        let quic_port = config.quic_port.unwrap_or(http_port);
        let mesh_endpoint = mesh::endpoint::MeshEndpoint::bind(&identity, SocketAddr::from(([0, 0, 0, 0], quic_port)))
            .map(Arc::new)
            .map_err(StartError::MeshEndpoint)?;

        let (store, catalog) = open_storage(&config)?;
        let state = new_state(
            &config,
            StateParts {
                identity,
                trust,
                mesh_endpoint: mesh_endpoint.clone(),
                store,
                catalog,
                tls_fingerprint,
                http_port,
                quic_port,
                primary_ip,
                local_http_port,
            },
        );

        let imported_legacy_files = restore_state(&state, saved.as_ref(), &node_id).await;
        let saver = persist::spawn_saver(state.clone(), &data_dir, saved.as_ref());

        let mut background_tasks = vec![tokio::spawn(mesh::quic::accept_loop(state.clone(), mesh_endpoint.clone()))];
        background_tasks.extend(start_control_socket(&state, &config, &mut notices).await);
        background_tasks.extend(keep_manual_peers_connected(&state, &peers_by_address(&state)));
        if let Some(saved) = &saved {
            persist::redial(&state, saved.peers.clone());
        }
        match config.discovery_port {
            Some(port) => {
                background_tasks.push(start_discovery(&state, port));
                background_tasks.push(spawn_isolation_diagnostic(state.clone()));
            }
            None => tracing::info!("Discovery: disabled"),
        }
        background_tasks.push(spawn_housekeeping(state.clone()));

        let mdns = mdns::advertise(&local_ips, http_port, is_tls_enabled);
        let (stop_web, stop_signal) = watch::channel(false);
        let listeners = Listeners { public: public_listener, local: local_listener };
        let WebServers { public: web_server, local: local_web_server, open_pipe } =
            serve_web(&state, &config.ui_assets, listeners, tls_config, stop_signal);

        let info = NodeInfo {
            node_id,
            key_location: key_location.to_string(),
            data_dir,
            http_port,
            quic_port,
            local_http_port,
            local_ips,
            primary_ip,
            is_tls_enabled,
            tls_fingerprint: state.tls_fingerprint.clone(),
            imported_legacy_files,
            notices,
        };
        Ok(NodeHandle {
            info,
            state,
            mesh_endpoint,
            saver,
            mdns,
            web_server,
            local_web_server,
            stop_web,
            open_pipe,
            runtime: tokio::runtime::Handle::current(),
            background_tasks,
            _data_dir_lock: data_dir_lock,
        })
    }
}

/// Takes the data directory for this node, or says who has it; the owner is the only one to touch its files.
fn claim_data_dir(data_dir: &std::path::Path) -> Result<DataDirLock, StartError> {
    match DataDirLock::acquire(data_dir) {
        Ok(lock) => {
            // Whatever endpoint is on disk belongs to a node that is gone.
            control::server::close(data_dir);
            Ok(lock)
        }
        Err(lock::LockError::InUse) => {
            Err(StartError::DataDirInUse { data_dir: data_dir.to_path_buf(), running: control::server::Endpoint::load(data_dir) })
        }
        Err(lock::LockError::Io(source)) => Err(StartError::DataDirUnavailable { data_dir: data_dir.to_path_buf(), source }),
    }
}

/// Last-resort guess at the LAN address from the routing table; sends no packet.
fn guess_local_ip() -> Option<IpAddr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

fn prepare_tls(config: &NodeConfig, local_ips: &[IpAddr], notices: &mut Vec<Notice>) -> (Option<Arc<rustls::ServerConfig>>, Vec<u8>) {
    if !config.use_tls {
        tracing::info!("TLS: disabled, serving plain HTTP/WS");
        return (None, Vec::new());
    }
    tls::install_crypto_provider();
    let mut names: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string(), "ladex.local".to_string()];
    names.extend(local_ips.iter().map(IpAddr::to_string));
    match tls::prepare_server_identity(&config.certificate_dir, &names) {
        Ok((server_config, fingerprint)) => (Some(server_config), fingerprint),
        Err(e) => {
            notices.push(Notice::CertificateUnavailable(e.to_string()));
            (None, Vec::new())
        }
    }
}

async fn bind_public(port: u16) -> Result<TcpListener, StartError> {
    TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await.map_err(|source| StartError::Listen { port, source })
}

/// The loopback listener browsers on this machine use without a certificate warning; only open with TLS.
async fn bind_local(config: &NodeConfig, http_port: u16, is_tls_enabled: bool, notices: &mut Vec<Notice>) -> Option<TcpListener> {
    if !is_tls_enabled {
        return None;
    }
    let port = config.local_http_port.unwrap_or(if http_port == u16::MAX { http_port - 1 } else { http_port + 1 });
    match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
        Ok(listener) => Some(listener),
        Err(e) => {
            notices.push(Notice::LocalListenerUnavailable { port, reason: e.to_string() });
            None
        }
    }
}

fn open_storage(config: &NodeConfig) -> Result<(Arc<store::Store>, Arc<catalog::shared::SharedCatalog>), StartError> {
    let path = &config.data_dir;
    let store =
        store::Store::open(path, config.storage_limit_bytes).map_err(|e| StartError::Store { path: path.clone(), source: e.into() })?;
    let catalog = catalog::shared::SharedCatalog::open(path, state::MAX_CATALOG_ENTRIES)
        .map_err(|source| StartError::Catalog { path: path.clone(), source })?;
    Ok((Arc::new(store), Arc::new(catalog)))
}

struct StateParts {
    identity: identity::Identity,
    trust: Arc<trust::TrustStore>,
    mesh_endpoint: Arc<mesh::endpoint::MeshEndpoint>,
    store: Arc<store::Store>,
    catalog: Arc<catalog::shared::SharedCatalog>,
    tls_fingerprint: Vec<u8>,
    http_port: u16,
    quic_port: u16,
    primary_ip: Option<IpAddr>,
    local_http_port: Option<u16>,
}

fn new_state(config: &NodeConfig, parts: StateParts) -> NodeState {
    let node_id: NodeId = parts.identity.node_id().to_string();
    // Mesh joins get no global cap: another mesh on the same network would trip it for everyone.
    let mesh_limiter = ratelimit::Policy {
        free_attempts: 5,
        base_lockout: Duration::from_secs(10),
        max_lockout: Duration::from_secs(600),
        global_cap: None,
    };
    NodeState {
        local_peers: Arc::new(RwLock::new(HashMap::new())),
        local_senders: Arc::new(RwLock::new(HashMap::new())),
        messages: Arc::new(RwLock::new(Vec::new())),
        node_id: node_id.clone(),
        identity: Arc::new(parts.identity),
        trust: parts.trust,
        pairings: Arc::new(pairing::Pairings::new()),
        mesh_peers: Arc::new(RwLock::new(HashMap::new())),
        part_time_peers: Default::default(),
        dial_targets: Default::default(),
        kept_peers: Default::default(),
        mesh_endpoint: Some(parts.mesh_endpoint),
        quic_port: parts.quic_port,
        passphrase: config.passphrase.clone(),
        tls_fingerprint: parts.tls_fingerprint,
        auth_limiter: Arc::new(ratelimit::AttemptLimiter::new(ratelimit::browser_login_policy())),
        mesh_limiter: Arc::new(ratelimit::AttemptLimiter::new(mesh_limiter)),
        availability: Arc::new(availability::Availability::default()),
        store: parts.store,
        catalog: parts.catalog,
        transfers: Arc::new(transfer::Transfers::with_tuning(transfer::Tuning {
            stall_timeout: config.stall_timeout,
            ..Default::default()
        })),
        is_seeding_paused: Arc::new(AtomicBool::new(false)),
        pushes: Arc::new(push::Pushes::default()),
        sessions: Arc::new(sessions::SessionStore::new()),
        catalog_changes: Arc::new(tokio::sync::watch::channel(0).0),
        guests: Arc::new(guest::Guests::new(config.guest_session_ttl)),
        clock: Arc::new(hlc::Clock::new(node_id)),
        session_owners: Arc::new(RwLock::new(HashMap::new())),
        connection_counter: Arc::new(AtomicU64::new(1)),
        clock_alert_at: Arc::new(Mutex::new(None)),
        http_port: parts.http_port,
        local_ip: Arc::new(std::sync::RwLock::new(parts.primary_ip)),
        node_name: types::hostname(),
        is_part_time: config.is_part_time,
        preferences: Arc::new(crate::preferences::PreferencesStore::open(&config.data_dir)),
        started_with: Arc::new(control::Settings {
            data_dir: config.data_dir.clone(),
            discovery_port: config.discovery_port,
            local_http_port: parts.local_http_port,
            use_keychain: config.use_keychain,
            manual_peers: config.manual_peers.clone(),
        }),
    }
}

/// Brings what a previous run saved back into `state`; returns how many legacy files were listed in the catalog.
async fn restore_state(state: &NodeState, saved: Option<&persist::Saved>, node_id: &NodeId) -> usize {
    // Never reissue a stamp this node already signed an op with.
    if let Some(latest) = state.catalog.latest_stamp(node_id) {
        state.clock.resume_from(latest.wall, latest.counter);
    }
    if let Some(saved) = saved {
        persist::restore(state, saved).await;
        if &saved.node_id != node_id {
            tracing::info!("Node ID changed from {} to {node_id}, now derived from the node's key", saved.node_id);
        }
    }
    let imported = state::import_legacy_entries(state).await;
    state::reconcile_store(state).await;
    transfer::remember_interrupted(state);
    imported
}

async fn start_control_socket(state: &NodeState, config: &NodeConfig, notices: &mut Vec<Notice>) -> Option<JoinHandle<()>> {
    match control::server::open(&config.data_dir).await {
        Ok((listener, endpoint)) => {
            let settings = state.started_with.as_ref().clone();
            Some(tokio::spawn(control::server::serve(state.clone(), settings, listener, endpoint)))
        }
        Err(e) => {
            notices.push(Notice::ControlUnavailable(e.to_string()));
            None
        }
    }
}

// The `--peer` addresses and the ones added in the app or with `ladex settings`, each once.
fn peers_by_address(state: &NodeState) -> Vec<SocketAddr> {
    let mut addresses = state.started_with.manual_peers.clone();
    for address in state.preferences.get().peer_addresses {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    addresses
}

fn keep_manual_peers_connected(state: &NodeState, peers: &[SocketAddr]) -> Vec<JoinHandle<()>> {
    peers
        .iter()
        .map(|&addr| {
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(MANUAL_PEER_DELAY).await;
                mesh::dialer::keep_connected(&state, addr).await;
            })
        })
        .collect()
}

fn start_discovery(state: &NodeState, port: u16) -> JoinHandle<()> {
    let announce_packet = discovery::build_announce(state);
    let is_seeding_paused = state.is_seeding_paused.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let service = match discovery::DiscoveryService::bind(port).await {
            Ok(service) => service,
            Err(e) => {
                tracing::warn!("Discovery: failed to bind multicast socket on port {port}: {e}");
                tracing::warn!("Discovery: running without automatic peer discovery; connect to peers by address instead");
                return;
            }
        };
        let announce = async {
            if let Err(e) = service.announce_loop(announce_packet, &is_seeding_paused).await {
                tracing::error!("Discovery announce loop error: {e}");
            }
        };
        let listen = async {
            if let Err(e) = service.listen_loop(state).await {
                tracing::error!("Discovery listen loop error: {e}");
            }
        };
        tokio::join!(announce, listen);
    })
}

/// Expires sessions and old state once a minute.
fn spawn_housekeeping(state: NodeState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HOUSEKEEPING_INTERVAL);
        loop {
            interval.tick().await;
            state.sessions.purge_expired();
            state::collect_garbage(&state).await;
            let mut peers = state.local_peers.write().await;
            let before = peers.len();
            state::prune_peer_tombstones(&mut peers, hlc::wall_clock_ms());
            let pruned = before - peers.len();
            if pruned > 0 {
                tracing::info!("State: pruned {pruned} stale peer-departure tombstone(s)");
            }
        }
    })
}

/// Warns browsers and the log when no other node shows up, the usual sign of a router that isolates clients.
fn spawn_isolation_diagnostic(state: NodeState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(ISOLATION_DIAGNOSTIC_DELAY).await;
        if !state.mesh_peers.read().await.is_empty() {
            return;
        }
        tracing::warn!(
            "AP-ISOLATION-DIAGNOSTIC: No mesh peers discovered after 10 seconds. Ensure all devices are on the same Wi-Fi network, \
             AP/client isolation is disabled on your router, and the discovery and QUIC UDP ports and the HTTP TCP port are not \
             blocked by a firewall."
        );
        let message = "No other LADEX nodes found on this network after 10 seconds. If you expect other devices, check: \
                       (1) all devices on same Wi-Fi, (2) AP/client isolation disabled on router, (3) the UDP and TCP ports \
                       LADEX uses are not blocked by a firewall.";
        websocket::broadcast_all(&state, ServerMessage::NoPeersWarning { message: message.to_string() }).await;
    })
}

struct Listeners {
    public: TcpListener,
    local: Option<TcpListener>,
}

struct WebServers {
    public: JoinHandle<anyhow::Result<()>>,
    local: Option<JoinHandle<()>>,
    open_pipe: local::OpenPipe,
}

/// Serves the node's own page and the guest and file APIs on the public listener, and on the loopback one if it is open.
fn serve_web(
    state: &NodeState,
    ui_assets: &Arc<dyn UiAssets>,
    listeners: Listeners,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    stop: watch::Receiver<bool>,
) -> WebServers {
    let service = web::service(state.clone(), ui_assets);
    let files = files_api::FilesApi::new(state.clone());
    let guest_api = guest::api::GuestApi::new(state.clone(), files.clone(), join_page(ui_assets.as_ref()));
    let api = server::Chain(files, guest_api);

    let local = listeners.local.map(|listener| {
        let port = listener.local_addr().map(|addr| addr.port()).unwrap_or_default();
        let hosts = vec![format!("localhost:{port}"), format!("127.0.0.1:{port}")];
        let (service, api, stop) = (service.clone(), api.clone(), stop.clone());
        tokio::spawn(async move {
            if let Err(e) = server::serve_until(listener, None, Some(hosts), service, api, wait_for_stop(stop)).await {
                tracing::error!("Local HTTP listener exited: {e}");
            }
        })
    });

    // Each in-process connection ends with the node, as a socket would.
    let open_pipe: local::OpenPipe = {
        let (service, api, stop) = (service.clone(), api.clone(), stop.clone());
        Arc::new(move || {
            let (client, served) = tokio::io::duplex(local::PIPE_BYTES);
            let serving = server::serve_connection(served, local::local_peer(), None, service.clone(), api.clone());
            let stop = wait_for_stop(stop.clone());
            tokio::spawn(async move {
                tokio::select! {
                    _ = serving => {}
                    _ = stop => {}
                }
            });
            client
        })
    };

    let acceptor = tls_config.map(tokio_rustls::TlsAcceptor::from);
    let public = tokio::spawn(server::serve_until(listeners.public, acceptor, None, service, api, wait_for_stop(stop)));
    WebServers { public, local, open_pipe }
}

async fn wait_for_stop(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|is_stopped| *is_stopped).await;
}

fn join_page(ui_assets: &dyn UiAssets) -> guest::api::JoinPage {
    let page = ui_assets.file("join.html").and_then(|bytes| std::str::from_utf8(bytes).ok()).unwrap_or_default();
    guest::api::JoinPage::new(page.to_string())
}

/// Where the CLI and app keep a node's files when none is given: `LADEX_DATA_DIR`, else `~/.ladex/files`.
pub fn default_data_dir() -> PathBuf {
    data_dir_from(std::env::var_os(DATA_DIR_VARIABLE))
}

fn data_dir_from(variable: Option<std::ffi::OsString>) -> PathBuf {
    match variable {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => tls::config_dir().join("files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variable_names_the_data_dir_when_set() {
        assert_eq!(data_dir_from(Some("/srv/ladex".into())), PathBuf::from("/srv/ladex"));
    }

    #[test]
    fn an_unset_or_empty_variable_falls_back_to_the_home_folder() {
        assert_eq!(data_dir_from(None), tls::config_dir().join("files"));
        assert_eq!(data_dir_from(Some("".into())), tls::config_dir().join("files"));
    }
}
