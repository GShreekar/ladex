use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use warp::Filter;
use clap::Parser;
use rand::Rng;
use std::time::Duration;

mod types;
mod websocket;
mod handlers;
mod mesh;
mod discovery;
mod state;
mod auth;
mod tls;
mod hlc;
mod mdns;
mod ratelimit;
mod server;
mod sessions;
mod validate;

use types::*;
use include_dir::{include_dir, Dir};

// Embed the static directory at compile time
static STATIC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");

// ---------------------------------------------------------------------------
// Shared type aliases (kept short for use throughout the crate)
// ---------------------------------------------------------------------------

type LocalPeers = Arc<RwLock<HashMap<SessionId, PeerInfo>>>;
type Files      = Arc<RwLock<HashMap<String, FileMetadata>>>;
type Messages   = Arc<RwLock<Vec<types::TextMessage>>>;

// ---------------------------------------------------------------------------
// Phase 1 — CLI arguments
//
// Defines the full CLI surface upfront.  Arguments used by later phases
// (discovery, manual peers) are parsed now so the surface is stable and
// callers don't need to change when those phases land.
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "ladex")]
#[command(about = "LADEX - Local Area Data Exchange", long_about = None)]
struct Args {
    /// Passphrase required both to log in from a browser and to join the mesh.
    /// If omitted, anyone on the network can do either.
    passphrase: Option<String>,

    /// Generate a random passphrase and print it.
    #[arg(short = 's', long = "secure", conflicts_with = "passphrase")]
    secure: bool,

    /// Local HTTP/WS port for this node's own browser tab.
    #[arg(long, default_value = "8080")]
    port: u16,

    /// UDP multicast discovery port (Phase 2 — not yet used, parsed now for
    /// CLI stability so callers don't break when Phase 2 lands).
    #[arg(long, default_value = "7878")]
    discovery_port: u16,

    /// Disable UDP multicast discovery.
    /// Useful when testing two nodes on localhost or on networks that block
    /// multicast.  Combine with --peer to connect manually.
    #[arg(long)]
    no_discovery: bool,

    /// Manually specify a peer node to connect to, bypassing multicast
    /// discovery.  Format: "<ip>:<http_port>" e.g. "192.168.1.5:8080".
    /// Repeatable: --peer 192.168.1.5:8080 --peer 192.168.1.6:8080
    /// (Phase 3 — consumed by mesh::connect_to_peer)
    #[arg(long = "peer")]
    manual_peers: Vec<String>,

    /// BUG-03 fix: disable TLS and serve plain HTTP/WS, like before.
    /// Remote (non-localhost) browser tabs lose showSaveFilePicker() and
    /// crypto.subtle when this is set — large downloads fall back to
    /// RAM-buffered Blobs and integrity checks can't run. Useful for
    /// environments that reject self-signed certs, or quick localhost-only
    /// testing.
    #[arg(long)]
    no_tls: bool,

    /// Port for a plain-HTTP listener on this machine only (127.0.0.1).
    /// Browsers treat http://localhost as a secure context, so the person
    /// running LADEX can use it without the self-signed certificate warning.
    /// Defaults to the main port + 1. Not used with --no-tls.
    #[arg(long)]
    local_port: Option<u16>,
}

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

    /// Which WebSocket connection holds each browser `session_id`.
    pub session_owners: Arc<RwLock<HashMap<SessionId, websocket::ConnOwner>>>,
    connection_counter: Arc<AtomicU64>,

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
    #[cfg(test)]
    pub fn for_tests(passphrase: Option<&str>) -> Self {
        let policy = || ratelimit::Policy {
            free_attempts: 3,
            base_lockout: Duration::from_secs(30),
            max_lockout: Duration::from_secs(300),
            global_cap: None,
        };
        let node_id: NodeId = "node_test".to_string();
        NodeState {
            local_peers: Arc::new(RwLock::new(HashMap::new())),
            local_senders: Arc::new(RwLock::new(HashMap::new())),
            files: Arc::new(RwLock::new(HashMap::new())),
            messages: Arc::new(RwLock::new(Vec::new())),
            node_id: node_id.clone(),
            mesh_peers: Arc::new(RwLock::new(HashMap::new())),
            passphrase: passphrase.map(String::from),
            tls_fingerprint: Vec::new(),
            auth_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            mesh_limiter: Arc::new(ratelimit::AttemptLimiter::new(policy())),
            sessions: Arc::new(sessions::SessionStore::new()),
            clock: Arc::new(hlc::Clock::new(node_id)),
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

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

/// Resolves the browser's login session from its cookie. `None` on a node
/// without a passphrase; otherwise an invalid or missing session is rejected,
/// as a redirect to the login page, or as a 401 for API calls.
fn with_session(state: NodeState, api: bool) -> impl Filter<Extract = (Option<sessions::SessionHandle>,), Error = warp::Rejection> + Clone {
    warp::cookie::optional("auth")
        .and(warp::any().map(move || state.clone()))
        .and_then(move |token: Option<String>, state: NodeState| async move {
            if state.passphrase.is_none() {
                return Ok(None);
            }
            match token.and_then(|t| state.sessions.authenticate(&t)) {
                Some(session) => Ok(Some(session)),
                None if api => Err(warp::reject::custom(Unauthorized)),
                None => Err(warp::reject::custom(AuthenticationRequired)),
            }
        })
}

fn with_auth(state: NodeState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    with_session(state, false).map(|_| ()).untuple_one()
}

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

#[derive(Debug)]
struct AuthenticationRequired;
impl warp::reject::Reject for AuthenticationRequired {}

// ---------------------------------------------------------------------------
// BUG-06 fix: Cross-Site WebSocket Hijacking (CSWSH) protection.
//
// WebSocket handshakes are exempt from the Same-Origin Policy and from
// warp's `cors()` filter (CORS only ever governs fetch/XHR) — a browser
// will happily let any page open a raw WebSocket to any host:port on the
// LAN. Without a check here, a malicious website the user merely has open
// in another tab could connect straight to this server, and — since the
// product's own "PIN is optional" design makes running with no PIN the
// common case (`passphrase: None`, where `with_auth` allows
// everything through) — freely ride the /ws protocol with no login at all,
// or (with or without a PIN, since /mesh has its own, cookie-independent
// auth) speak the raw mesh protocol on /mesh.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct InvalidOrigin;
impl warp::reject::Reject for InvalidOrigin {}

/// For /ws (browser-tab facing): the Origin header must name exactly this
/// server's own host:port. Our own frontend always satisfies this — the
/// browser sets Origin to the page's own origin automatically, and the
/// page and the WebSocket it opens are served from the same host:port.
fn require_same_origin() -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("origin")
        .and(warp::header::optional::<String>("host"))
        .and_then(|origin: Option<String>, host: Option<String>| async move {
            let origin_host = origin.as_deref()
                .and_then(|o| o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")));
            match (origin_host, host.as_deref()) {
                (Some(o), Some(h)) if o == h => Ok(()),
                _ => Err(warp::reject::custom(InvalidOrigin)),
            }
        })
        .untuple_one()
}

/// For /mesh (node-to-node, not browser-facing): real mesh peers dial each
/// other with tokio-tungstenite directly (mesh::connect_to_peer /
/// tls::connect_wss) and never send an Origin header at all. A browser
/// always sends one on any cross-origin request it initiates, WebSocket
/// upgrades included — so rejecting any request that has one blocks every
/// browser-based attempt to join the mesh while never touching genuine
/// node-to-node traffic.
fn reject_browser_origin() -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("origin")
        .and_then(|origin: Option<String>| async move {
            if origin.is_none() { Ok(()) } else { Err(warp::reject::custom(InvalidOrigin)) }
        })
        .untuple_one()
}

async fn handle_rejection(err: warp::Rejection) -> Result<Box<dyn warp::Reply>, std::convert::Infallible> {
    if err.find::<AuthenticationRequired>().is_some() {
        Ok(Box::new(warp::redirect::temporary(warp::http::Uri::from_static("/login"))) as Box<dyn warp::Reply>)
    } else if err.find::<Unauthorized>().is_some() {
        Ok(Box::new(warp::reply::with_status("Unauthorized", warp::http::StatusCode::UNAUTHORIZED)) as Box<dyn warp::Reply>)
    } else if err.find::<InvalidOrigin>().is_some() {
        Ok(Box::new(warp::reply::with_status("Forbidden", warp::http::StatusCode::FORBIDDEN)) as Box<dyn warp::Reply>)
    } else if err.is_not_found() {
        Ok(Box::new(warp::reply::with_status("Not Found", warp::http::StatusCode::NOT_FOUND)) as Box<dyn warp::Reply>)
    } else {
        Ok(Box::new(warp::reply::with_status("Internal Server Error", warp::http::StatusCode::INTERNAL_SERVER_ERROR)) as Box<dyn warp::Reply>)
    }
}

async fn serve_login_page() -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let lookup = "login.html".to_string();
    if let Some(file) = STATIC_DIR.get_file(&lookup) {
        let mime = mime_guess::from_path(&lookup).first_or_octet_stream().to_string();
        let bytes = file.contents().to_vec();
        Ok(Box::new(warp::reply::with_header(
            warp::reply::html(bytes),
            "content-type",
            mime,
        )) as Box<dyn warp::Reply>)
    } else {
        Err(warp::reject::not_found())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // ── passphrase ───────────────────────────────────────────────────────
    // One passphrase gates both the browser login and the mesh handshake.
    let passphrase: Option<String> = if args.secure {
        let generated = auth::generate_passphrase();
        println!("Generated passphrase: {generated}");
        Some(generated)
    } else {
        args.passphrase.clone()
    };
    match passphrase.as_deref() {
        Some("") => {
            eprintln!("Error: the passphrase must not be empty.");
            std::process::exit(2);
        }
        Some(p) => {
            if let Some(reason) = auth::weakness(p) {
                eprintln!("Warning: this passphrase is weak ({reason}). Prefer `ladex -s` for a generated one.");
            }
        }
        None => eprintln!(
            "Warning: no passphrase set — any device on this network can open the web UI and join the mesh. \
             Use `ladex -s` to generate one."
        ),
    }

    // ── node identity ────────────────────────────────────────────────────
    // node_id identifies THIS machine on the mesh.  Different from a browser
    // tab's session_id.  Generated once per process lifetime.
    let node_id: NodeId = {
        let mut rng = rand::thread_rng();
        format!("node_{:016x}", rng.gen::<u64>())
    };

    tracing::info!("Node ID: {node_id}");

    // ── BUG-03 fix: TLS setup ────────────────────────────────────────────
    // Must happen before `state` is built: connect_to_peer (dialed from the
    // manual-peer, discovery, and reconnect-backoff call sites below) reads
    // state.tls_client_config to decide whether to speak wss:// or ws://.
    let local_ips = tls::local_ipv4_addresses();
    // BUG-11 fix: local_ips comes from if-addrs, which reads local interface
    // configuration directly — no network I/O, no route to anywhere
    // required, so it works with zero connectivity (the whole point of
    // LADEX). get_local_ip()'s 8.8.8.8 route-table trick is kept only as a
    // last-resort fallback for the rare case if-addrs finds nothing.
    let primary_local_ip: Option<IpAddr> = local_ips.first().copied().or_else(get_local_ip);

    let mut tls_fingerprint: Vec<u8> = Vec::new();
    let tls_server_config = if args.no_tls {
        tracing::info!("TLS: disabled via --no-tls — serving plain HTTP/WS");
        None
    } else {
        tls::install_crypto_provider();
        // Every name this node is reachable by, including the mDNS name it advertises.
        let mut names: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string(), "ladex.local".to_string()];
        names.extend(local_ips.iter().map(IpAddr::to_string));
        match tls::prepare_server_identity(&names) {
            Ok((config, fingerprint)) => {
                tls_fingerprint = fingerprint;
                Some(config)
            }
            Err(e) => {
                eprintln!("TLS: could not set up a certificate ({e}) — falling back to plain HTTP");
                None
            }
        }
    };
    let tls_client_config = tls_server_config.as_ref().map(|_| tls::build_client_config());
    if tls_server_config.is_none() && passphrase.is_some() {
        eprintln!(
            "Warning: TLS is off, so traffic is unencrypted and the mesh handshake cannot detect a \
             man in the middle. The passphrase itself is still never sent."
        );
    }

    // Browser logins are the main target for guessing, so they also get a cap
    // across all addresses; mesh joins don't, because a second mesh with a
    // different passphrase on the same network would trip it for everyone.
    let auth_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::Policy {
        free_attempts: 5,
        base_lockout: Duration::from_secs(30),
        max_lockout: Duration::from_secs(3600),
        global_cap: Some((30, Duration::from_secs(600))),
    }));
    let mesh_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::Policy {
        free_attempts: 5,
        base_lockout: Duration::from_secs(10),
        max_lockout: Duration::from_secs(600),
        global_cap: None,
    }));

    let state = NodeState {
        local_peers:          Arc::new(RwLock::new(HashMap::new())),
        local_senders:        Arc::new(RwLock::new(HashMap::new())),
        files:                Arc::new(RwLock::new(HashMap::new())),
        messages:             Arc::new(RwLock::new(Vec::new())),
        node_id:              node_id.clone(),
        mesh_peers:           Arc::new(RwLock::new(HashMap::new())),
        passphrase,
        tls_fingerprint,
        auth_limiter,
        mesh_limiter,
        sessions:             Arc::new(sessions::SessionStore::new()),
        clock:                Arc::new(hlc::Clock::new(node_id.clone())),
        session_owners:       Arc::new(RwLock::new(HashMap::new())),
        connection_counter:   Arc::new(AtomicU64::new(1)),
        clock_alert_at:       Arc::new(Mutex::new(None)),
        tls_client_config,
        http_port: args.port,
        local_ip: primary_local_ip,
        node_name: types::hostname(),
    };

    // ── Phase 3: connect to manually-specified peers ─────────────────────
    // These are processed before the HTTP server starts so the mesh is
    // partially formed by the time the browser tab connects.
    if !args.manual_peers.is_empty() {
        for peer_addr_str in &args.manual_peers {
            match peer_addr_str.parse::<SocketAddr>() {
                Ok(addr) => {
                    let state_clone = state.clone();
                    let addr_clone = addr;
                    tokio::spawn(async move {
                        // Small delay so our own HTTP server is up first.
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        if let Err(e) = mesh::connect_to_peer(addr_clone.ip(), addr_clone.port(), state_clone).await {
                            tracing::warn!("Manual peer connect to {addr_clone} failed: {e}");
                        }
                    });
                }
                Err(_) => {
                    eprintln!("Warning: invalid --peer address '{peer_addr_str}' — expected format: <ip>:<port>");
                }
            }
        }
    }

    // ── Phase 2: UDP multicast discovery ────────────────────────────────
    // Start announce + listen loops unless --no-discovery was passed.
    // Discovery is skipped gracefully (with a warning) if the multicast
    // socket fails to bind (e.g. on systems without a viable NIC at startup).
    if !args.no_discovery {
        let announce_packet = discovery::build_announce(&state, args.port);
        let discovery_port  = args.discovery_port;
        let state_disc      = state.clone();

        tokio::spawn(async move {
            match discovery::DiscoveryService::bind(discovery_port).await {
                Err(e) => {
                    tracing::warn!("Discovery: failed to bind multicast socket on port {discovery_port}: {e}");
                    tracing::warn!("Discovery: running without automatic peer discovery — use --peer <ip:port> to connect manually");
                }
                Ok(svc) => {
                    let svc = std::sync::Arc::new(svc);
                    let svc_listen = svc.clone();
                    let state_listen = state_disc.clone();
                    // Announce loop
                    tokio::spawn(async move {
                        if let Err(e) = svc.announce_loop(announce_packet).await {
                            tracing::error!("Discovery announce loop error: {e}");
                        }
                    });
                    // Listen loop
                    if let Err(e) = svc_listen.listen_loop(state_listen).await {
                        tracing::error!("Discovery listen loop error: {e}");
                    }
                }
            }
        });
    } else {
        tracing::info!("Discovery: disabled via --no-discovery");
    }

    // ── Phase 4 / BUG-08 fix: periodic tombstone pruner ─────────────────
    // Cleans up old tombstoned file entries from the in-memory catalog, and
    // (BUG-08) old peer-departure tombstones, so neither grows unboundedly.
    // Also drops expired login sessions.
    {
        let state_prune = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                state_prune.sessions.purge_expired();
                let mut files = state_prune.files.write().await;
                let before = files.len();
                state::prune_tombstones(&mut files, hlc::wall_clock_ms());
                let pruned = before - files.len();
                if pruned > 0 {
                    tracing::info!("State: pruned {pruned} stale tombstone(s) from file catalog");
                }
                drop(files);

                let mut peers = state_prune.local_peers.write().await;
                let before = peers.len();
                state::prune_peer_tombstones(&mut peers, hlc::wall_clock_ms());
                let pruned = before - peers.len();
                if pruned > 0 {
                    tracing::info!("State: pruned {pruned} stale peer-departure tombstone(s)");
                }
            }
        });
    }

    // ── Phase 10 §10.4: AP isolation diagnostic ──────────────────────────
    // If discovery is enabled but we still have zero mesh peers after 10s,
    // emit a structured warning so the browser tab can surface it.
    if !args.no_discovery {
        let state_diag = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let peer_count = state_diag.mesh_peers.read().await.len();
            if peer_count == 0 {
                tracing::warn!(
                    "AP-ISOLATION-DIAGNOSTIC: No mesh peers discovered after 10 seconds. \
                     Ensure all devices are on the same Wi-Fi network, AP/client isolation \
                     is disabled on your router, and UDP port 7878 / TCP port 8080 are not \
                     blocked by a firewall."
                );
                // Push a diagnostic ServerMessage to all connected browser tabs
                crate::websocket::broadcast_all(
                    &state_diag,
                    ServerMessage::NoPeersWarning {
                        message: "No other LADEX nodes found on this network after 10 seconds. \
                                  If you expect other devices, check: (1) all devices on same \
                                  Wi-Fi, (2) AP/client isolation disabled on router, (3) UDP \
                                  port 7878 and TCP port 8080 not blocked.".to_string(),
                    },
                ).await;
            }
        });
    }

    // ── Routes ───────────────────────────────────────────────────────────

    // Login page — not protected
    let app_state_login = state.clone();
    let login_route = warp::path("login")
        .and(warp::get())
        .and(warp::any().map(move || app_state_login.clone()))
        .and_then(|s: NodeState| async move {
            if s.passphrase.is_some() {
                serve_login_page().await
            } else {
                let redirect = warp::redirect::temporary(warp::http::Uri::from_static("/"));
                Ok::<_, warp::Rejection>(Box::new(redirect) as Box<dyn warp::Reply>)
            }
        });

    // Auth endpoint — not protected
    let app_state_auth = state.clone();
    let auth_route = warp::path("auth")
        .and(warp::post())
        .and(warp::body::content_length_limit(4096))
        .and(warp::body::json())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::header::optional::<String>("user-agent"))
        .and(warp::any().map(move || app_state_auth.clone()))
        .and_then(handlers::authenticate);

    // Logout — ends only the caller's own session
    let app_state_logout = state.clone();
    let logout_route = warp::path("logout")
        .and(warp::post())
        .and(require_same_origin())
        .and(warp::cookie::optional("auth"))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || app_state_logout.clone()))
        .and_then(handlers::logout);

    // Per-device sessions: list them, and sign a device out.
    let app_state_sessions = state.clone();
    let sessions_list_route = warp::path!("api" / "sessions")
        .and(warp::get())
        .and(with_session(state.clone(), true))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || app_state_sessions.clone()))
        .and_then(handlers::list_sessions);

    let app_state_revoke = state.clone();
    let sessions_revoke_route = warp::path!("api" / "sessions" / String)
        .and(warp::delete())
        .and(require_same_origin())
        .and(with_session(state.clone(), true))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || app_state_revoke.clone()))
        .and_then(handlers::revoke_session);

    // Auth-status check — not protected
    let auth_status_route = warp::path("auth-status")
        .and(warp::get())
        .and(warp::cookie::optional("auth"))
        .and(warp::any().map({
            let s = state.clone();
            move || s.clone()
        }))
        .and_then(handlers::check_auth_status);

    // Static assets — not protected
    let static_route = warp::path("static")
        .and(warp::path::tail())
        .and_then(|tail: warp::filters::path::Tail| async move {
            let lookup = tail.as_str().trim_start_matches('/').to_string();
            let lookup = if lookup.is_empty() { "index.html".to_string() } else { lookup };
            if let Some(file) = STATIC_DIR.get_file(&lookup) {
                let mime = mime_guess::from_path(&lookup).first_or_octet_stream().to_string();
                let bytes = file.contents().to_vec();
                Ok::<_, warp::Rejection>(warp::reply::with_header(
                    warp::reply::html(bytes),
                    "content-type",
                    mime,
                ))
            } else {
                Err(warp::reject::not_found())
            }
        });

    // Browsers request /favicon.ico directly regardless of the <link rel="icon"> tag
    let favicon_route = warp::path("favicon.ico")
        .and(warp::get())
        .and_then(|| async move {
            match STATIC_DIR.get_file("favicon.svg") {
                Some(file) => Ok(warp::reply::with_header(
                    warp::reply::html(file.contents().to_vec()),
                    "content-type",
                    "image/svg+xml",
                )),
                None => Err(warp::reject::not_found()),
            }
        });

    // Phase 3 — Mesh WebSocket endpoint /mesh (node-to-node, not browser-facing).
    // Separate from /ws intentionally: mesh peers and browser tabs have
    // different message protocols and different lifecycle semantics.
    let mesh_state = state.clone();
    // The peer's address comes from server.rs (warp has no remote-address
    // filter of its own) and keys the handshake rate limiter. The peer's
    // dialable ip/http_port still come from its self-reported Hello (BUG-10).
    let mesh_route = warp::path("mesh")
        .and(reject_browser_origin())
        .and(warp::ws())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || mesh_state.clone()))
        .and_then(mesh::mesh_ws_handler);

    // Browser-tab WebSocket /ws — protected
    let app_state_ws = state.clone();
    let websocket_route = warp::path("ws")
        .and(with_session(state.clone(), false))
        .and(require_same_origin())
        .and(warp::ws())
        .and(warp::any().map(move || app_state_ws.clone()))
        .and_then(websocket::websocket_handler);

    // Root — protected
    let index = warp::path::end()
        .and(with_auth(state.clone()))
        .and_then(|| async move {
            let lookup = "index.html".to_string();
            if let Some(file) = STATIC_DIR.get_file(&lookup) {
                let mime = mime_guess::from_path(&lookup).first_or_octet_stream().to_string();
                let bytes = file.contents().to_vec();
                Ok::<_, warp::Rejection>(warp::reply::with_header(
                    warp::reply::html(bytes),
                    "content-type",
                    mime,
                ))
            } else {
                Err(warp::reject::not_found())
            }
        });

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);

    // Important: more specific routes first; unprotected before protected.
    // /mesh must come before /ws so the path pattern doesn't shadow it.
    let routes = login_route
        .or(auth_route)
        .or(logout_route)
        .or(sessions_list_route)
        .or(sessions_revoke_route)
        .or(auth_status_route)
        .or(static_route)
        .or(favicon_route)
        .or(mesh_route)
        .or(websocket_route)
        .or(index)
        .with(cors)
        .recover(handle_rejection);

    let local_ip = primary_local_ip.map(|ip| ip.to_string())
        .unwrap_or_else(|| "YOUR_IP".to_string());

    // ── Phase 10 §10.5: graceful shutdown ────────────────────────────────
    // Race the server against a SIGTERM/SIGINT signal. On receiving a
    // signal, broadcast Goodbye to all peers before exiting.
    let state_shutdown = state.clone();
    let tls_enabled = tls_server_config.is_some();

    // F2: advertise ladex.local alongside the IP-based URLs above — a
    // memorable alternative, not a replacement, since .local resolution
    // isn't universally supported.
    let mdns_handle = mdns::advertise(&local_ips, args.port, tls_enabled);

    // The host's own browser can skip the certificate warning by using plain
    // HTTP on loopback: browsers treat http://localhost as a secure context.
    let local_http_port: Option<u16> = if tls_enabled {
        Some(args.local_port.unwrap_or(if args.port == u16::MAX { args.port - 1 } else { args.port + 1 }))
    } else {
        None
    };
    let local_listener = match local_http_port {
        Some(port) => match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
            Ok(listener) => Some(listener),
            Err(e) => {
                eprintln!("Note: could not open the local HTTP listener on 127.0.0.1:{port} ({e}); use the https address below, or pick another port with --local-port.");
                None
            }
        },
        None => None,
    };

    let public_listener = match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], args.port))).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("Error: could not listen on port {}: {e}", args.port);
            std::process::exit(1);
        }
    };

    let scheme = if tls_enabled { "https" } else { "http" };
    match (&local_listener, local_http_port) {
        (Some(_), Some(port)) => println!("Access on this machine: http://localhost:{port}  (no certificate warning)"),
        _ => println!("Access locally: {scheme}://localhost:{}", args.port),
    }
    println!("Access from other devices: {scheme}://{local_ip}:{}", args.port);
    if tls_enabled {
        println!("Certificate fingerprint (SHA-256): {}", tls::format_fingerprint(&state.tls_fingerprint));
        println!(
            "Other devices' browsers will warn that this certificate is self-signed. Before accepting it, \
             check that the fingerprint in the browser's certificate details matches the one above."
        );
    }
    if primary_local_ip.is_some() {
        print_qr_code(&format!("{scheme}://{local_ip}:{}", args.port));
    }

    let service = warp::service(routes);
    if let (Some(listener), Some(port)) = (local_listener, local_http_port) {
        let hosts = vec![format!("localhost:{port}"), format!("127.0.0.1:{port}")];
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(e) = server::serve(listener, None, Some(hosts), service).await {
                tracing::error!("Local HTTP listener exited: {e}");
            }
        });
    }

    let acceptor = tls_server_config.map(tokio_rustls::TlsAcceptor::from);
    tokio::select! {
        result = server::serve(public_listener, acceptor, None, service) => {
            if let Err(e) = result {
                tracing::error!("Server exited: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown: SIGINT received — sending Goodbye to mesh peers");
            mesh::broadcast_goodbye(&state_shutdown).await;
            if let Some(handle) = mdns_handle {
                mdns::shutdown(handle).await;
            }
        }
    }
}

/// BUG-11 fix: last-resort fallback only — see call site. Note this doesn't
/// actually require internet access despite the 8.8.8.8 address: UDP
/// `connect()` just asks the OS to pick a source address via the routing
/// table, it never sends a packet. It only fails with no route at all
/// (e.g. no default gateway), which `local_ipv4_addresses()` above doesn't
/// depend on in the first place.
fn get_local_ip() -> Option<IpAddr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

// F1: scan the URL from a phone instead of typing it in
fn print_qr_code(url: &str) {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(url) {
        Ok(code) => {
            let qr = code.render::<unicode::Dense1x2>()
                .quiet_zone(false)
                .build();
            println!("\nScan to open on your phone:\n{qr}");
        }
        Err(e) => tracing::warn!("QR code: failed to encode {url}: {e}"),
    }
}
