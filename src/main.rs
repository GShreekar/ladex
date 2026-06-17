use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use warp::Filter;
use clap::Parser;
use rand::Rng;

mod types;
mod websocket;
mod handlers;
mod mesh;
mod discovery;
mod state;

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
    /// Optional shared passphrase for joining the mesh.
    /// Replaces the old numeric security_code.  If omitted, no auth is required.
    passphrase: Option<String>,

    /// Auto-generate a random passphrase and print it (replaces --secure).
    /// Equivalent to the old --secure flag for backward compat.
    #[arg(short = 's', long = "secure")]
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
}

fn generate_random_code() -> String {
    let mut rng = rand::thread_rng();
    format!("{:06}", rng.gen_range(100000..1000000))
}

fn validate_code(code: &str) -> bool {
    code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())
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
//   passphrase_hash              — Replaces security_code in Phase 7.
//                                  Until then, security_code_legacy carries
//                                  the old numeric code so auth still works.
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
    /// Legacy numeric security code (Phase 0 / pre-Phase-7 auth).
    /// Kept alongside passphrase_hash so auth continues to work before
    /// Phase 7 replaces the whole auth system.
    pub security_code_legacy: Option<String>,

    /// Phase 7 passphrase hash (PBKDF2-SHA256 hex).
    /// None until Phase 7 is implemented.
    pub passphrase_hash: Option<String>,
}

// Convenience accessor — keeps the auth middleware readable.
impl NodeState {
    /// Returns the cookie-auth session token string.
    /// During the pre-Phase-7 period this is the node_id (was server_session_id).
    pub fn session_token(&self) -> &str {
        &self.node_id
    }
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

fn with_auth(state: NodeState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::any()
        .and(warp::cookie::optional("auth"))
        .and(warp::any().map(move || state.clone()))
        .and_then(|auth_cookie: Option<String>, state: NodeState| async move {
            match &state.security_code_legacy {
                None => Ok(()),
                Some(_) => match auth_cookie {
                    Some(cookie) => {
                        let expected_cookie = format!("authenticated:{}", state.session_token());
                        if cookie == expected_cookie {
                            Ok(())
                        } else {
                            Err(warp::reject::custom(AuthenticationRequired))
                        }
                    },
                    _ => Err(warp::reject::custom(AuthenticationRequired)),
                }
            }
        })
        .untuple_one()
}

#[derive(Debug)]
struct AuthenticationRequired;
impl warp::reject::Reject for AuthenticationRequired {}

async fn handle_rejection(err: warp::Rejection) -> Result<Box<dyn warp::Reply>, std::convert::Infallible> {
    if err.find::<AuthenticationRequired>().is_some() {
        Ok(Box::new(warp::redirect::temporary(warp::http::Uri::from_static("/login"))) as Box<dyn warp::Reply>)
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

    // ── passphrase / security code ───────────────────────────────────────
    // Backward-compat: accept the old 6-digit numeric code on positional arg.
    // Phase 7 will replace this with proper PBKDF2 hashing; for now we keep
    // the same cookie-auth behaviour as before.
    let security_code_legacy = if args.secure {
        let code = generate_random_code();
        println!("Generated security code: {code}");
        Some(code)
    } else if let Some(ref p) = args.passphrase {
        // If it looks like the old 6-digit code, accept it as-is.
        if validate_code(p) {
            Some(p.clone())
        } else {
            // Non-numeric passphrase: store for Phase 7 hash; no legacy cookie auth.
            // Until Phase 7 lands, just print a warning and skip auth.
            eprintln!("Note: non-numeric passphrase provided — mesh auth will be enforced in Phase 7.  Running without HTTP auth for now.");
            None
        }
    } else {
        None
    };

    // ── node identity ────────────────────────────────────────────────────
    // node_id identifies THIS machine on the mesh.  Different from a browser
    // tab's session_id.  Generated once per process lifetime.
    let node_id: NodeId = {
        let mut rng = rand::thread_rng();
        format!("node_{:016x}", rng.gen::<u64>())
    };

    tracing::info!("Node ID: {node_id}");

    let state = NodeState {
        local_peers:          Arc::new(RwLock::new(HashMap::new())),
        local_senders:        Arc::new(RwLock::new(HashMap::new())),
        files:                Arc::new(RwLock::new(HashMap::new())),
        messages:             Arc::new(RwLock::new(Vec::new())),
        node_id:              node_id.clone(),
        mesh_peers:           Arc::new(RwLock::new(HashMap::new())),
        security_code_legacy,
        passphrase_hash:      None, // Phase 7
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
        let passphrase_hash = state.passphrase_hash.clone().unwrap_or_default();

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
                    if let Err(e) = svc_listen.listen_loop(state_listen, passphrase_hash).await {
                        tracing::error!("Discovery listen loop error: {e}");
                    }
                }
            }
        });
    } else {
        tracing::info!("Discovery: disabled via --no-discovery");
    }

    // ── Phase 4: periodic tombstone pruner ──────────────────────────────
    // Cleans up old tombstoned file entries (>60s old) from the in-memory
    // catalog so it doesn't grow unboundedly.
    {
        let state_prune = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut files = state_prune.files.write().await;
                let before = files.len();
                state::prune_tombstones(&mut files);
                let pruned = before - files.len();
                if pruned > 0 {
                    tracing::info!("State: pruned {pruned} stale tombstone(s) from file catalog");
                }
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
            if s.security_code_legacy.is_some() {
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
        .and(warp::body::json())
        .and(warp::any().map(move || app_state_auth.clone()))
        .and_then(handlers::authenticate);

    // Logout — not protected
    let logout_route = warp::path("logout")
        .and(warp::post())
        .and_then(handlers::logout);

    // Auth-status check — not protected
    let auth_status_route = warp::path("auth-status")
        .and(warp::get())
        .and(warp::header::optional::<String>("cookie"))
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

    // Phase 3 — Mesh WebSocket endpoint /mesh (node-to-node, not browser-facing).
    // Separate from /ws intentionally: mesh peers and browser tabs have
    // different message protocols and different lifecycle semantics.
    let mesh_state = state.clone();
    let mesh_route = warp::path("mesh")
        .and(warp::ws())
        .and(warp::any().map(move || mesh_state.clone()))
        .and_then(mesh::mesh_ws_handler);

    // Browser-tab WebSocket /ws — protected
    let app_state_ws = state.clone();
    let websocket_route = warp::path("ws")
        .and(with_auth(state.clone()))
        .and(warp::ws())
        .and(warp::any().map(move || app_state_ws.clone()))
        .and_then(websocket::websocket_handler);

    // API — protected
    let app_state_api = state.clone();
    let api = warp::path("api")
        .and(with_auth(state.clone()))
        .and(
            warp::path("peers")
                .and(warp::get())
                .and(warp::any().map(move || app_state_api.clone()))
                .and_then(handlers::get_peers)
        );

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
        .or(auth_status_route)
        .or(static_route)
        .or(mesh_route)
        .or(websocket_route)
        .or(api)
        .or(index)
        .with(cors)
        .recover(handle_rejection);

    let addr: SocketAddr = ([0, 0, 0, 0], args.port).into();
    let local_ip = get_local_ip().unwrap_or_else(|| "YOUR_IP".to_string());

    println!("Access locally: http://localhost:{}", args.port);
    println!("Access from network: http://{local_ip}:{}", args.port);

    warp::serve(routes).run(addr).await;
}

fn get_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                return Some(addr.ip().to_string());
            }
        }
    }
    None
}
