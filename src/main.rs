use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
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
mod auth;
mod tls;
mod mdns;

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

    /// BUG-03 fix: disable TLS and serve plain HTTP/WS, like before.
    /// Remote (non-localhost) browser tabs lose showSaveFilePicker() and
    /// crypto.subtle when this is set — large downloads fall back to
    /// RAM-buffered Blobs and integrity checks can't run. Useful for
    /// environments that reject self-signed certs, or quick localhost-only
    /// testing.
    #[arg(long)]
    no_tls: bool,
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

    /// BUG-04 fix: random secret used ONLY for the browser HTTP auth
    /// cookie. Generated once per process, never transmitted anywhere —
    /// not in discovery announces, not in mesh Hello, not logged. Deliberately
    /// distinct from node_id: node_id is broadcast in the clear over UDP
    /// multicast every 2s (see discovery::build_announce) and is not a
    /// secret, so using it as the cookie value let anyone passively
    /// sniffing the LAN forge `auth=authenticated:{node_id}` and skip the
    /// PIN entirely.
    pub http_auth_secret: String,

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

// Convenience accessor — keeps the auth middleware readable.
impl NodeState {
    /// Returns the cookie-auth session token string.
    /// BUG-04 fix: this is `http_auth_secret`, NOT `node_id` — node_id is
    /// broadcast in the clear over the LAN (UDP discovery, mesh Hello) and
    /// must never be usable to forge the HTTP auth cookie.
    pub fn session_token(&self) -> &str {
        &self.http_auth_secret
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

// ---------------------------------------------------------------------------
// BUG-06 fix: Cross-Site WebSocket Hijacking (CSWSH) protection.
//
// WebSocket handshakes are exempt from the Same-Origin Policy and from
// warp's `cors()` filter (CORS only ever governs fetch/XHR) — a browser
// will happily let any page open a raw WebSocket to any host:port on the
// LAN. Without a check here, a malicious website the user merely has open
// in another tab could connect straight to this server, and — since the
// product's own "PIN is optional" design makes running with no PIN the
// common case (`security_code_legacy: None`, where `with_auth` allows
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

    // BUG-04 fix: separate, never-transmitted secret for the HTTP auth
    // cookie. 256 bits from the OS CSPRNG via `rand`'s default generator —
    // plenty for a cookie value nobody can observe on the wire to begin
    // with, since (unlike node_id) it's never sent anywhere but back to
    // the browser that already proved it knows the PIN.
    let http_auth_secret: String = {
        let mut rng = rand::thread_rng();
        let bytes: [u8; 32] = rng.gen();
        hex::encode(bytes)
    };

    // ── Phase 7: passphrase hash ─────────────────────────────────────────
    // Compute PBKDF2-SHA256 hash now so it can be placed in AnnouncePacket
    // and MeshMessage::Hello.  This replaces the old plain-text comparison.
    // Empty passphrase produces an empty hash → no-passphrase mesh nodes
    // only connect to other no-passphrase nodes.
    let passphrase_for_hash: Option<&str> = args.passphrase.as_deref()
        .filter(|p| !validate_code(p)); // non-numeric = new-style passphrase
    let passphrase_hash_value = auth::derive_hash(passphrase_for_hash);
    if !passphrase_hash_value.is_empty() {
        tracing::info!("Phase 7: passphrase hash computed (PBKDF2-SHA256)");
    }

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

    let tls_server_config = if args.no_tls {
        tracing::info!("TLS: disabled via --no-tls — serving plain HTTP/WS");
        None
    } else {
        tls::install_crypto_provider();
        let mut sans: Vec<String> = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        sans.extend(local_ips.iter().map(IpAddr::to_string));
        match tls::load_or_generate_cert(&sans) {
            Ok((cert, key)) => match tls::build_server_config(cert, key) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    eprintln!("TLS: failed to build server config ({e}) — falling back to plain HTTP");
                    None
                }
            },
            Err(e) => {
                eprintln!("TLS: failed to generate certificate ({e}) — falling back to plain HTTP");
                None
            }
        }
    };
    let tls_client_config = tls_server_config.as_ref().map(|_| tls::build_client_config());

    let state = NodeState {
        local_peers:          Arc::new(RwLock::new(HashMap::new())),
        local_senders:        Arc::new(RwLock::new(HashMap::new())),
        files:                Arc::new(RwLock::new(HashMap::new())),
        messages:             Arc::new(RwLock::new(Vec::new())),
        node_id:              node_id.clone(),
        mesh_peers:           Arc::new(RwLock::new(HashMap::new())),
        security_code_legacy,
        passphrase_hash:      Some(passphrase_hash_value),
        http_auth_secret,
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

    // ── Phase 4 / BUG-08 fix: periodic tombstone pruner ─────────────────
    // Cleans up old tombstoned file entries (>60s old) from the in-memory
    // catalog, and (BUG-08) old peer-departure tombstones, so neither grows
    // unboundedly.
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
                drop(files);

                let mut peers = state_prune.local_peers.write().await;
                let before = peers.len();
                state::prune_peer_tombstones(&mut peers);
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
    // BUG-10 fix note: warp 0.4.1 doesn't expose a remote-address filter at
    // all (its filters::addr module is commented out in the published
    // crate — dead code, like the `tls` feature from the BUG-03 fix), so
    // handle_inbound can't learn a peer's address from the TCP connection
    // itself. It relies entirely on the peer self-reporting its own
    // ip/http_port in the Hello message instead (see MeshMessage::Hello).
    let mesh_route = warp::path("mesh")
        .and(reject_browser_origin())
        .and(warp::ws())
        .and(warp::any().map(move || mesh_state.clone()))
        .and_then(mesh::mesh_ws_handler);

    // Browser-tab WebSocket /ws — protected
    let app_state_ws = state.clone();
    let websocket_route = warp::path("ws")
        .and(with_auth(state.clone()))
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

    // ── Phase 10 §10.5 / BUG-03 fix: graceful shutdown + TLS ─────────────
    // Race the server against a SIGTERM/SIGINT signal. On receiving a
    // signal, broadcast Goodbye to all peers before exiting.
    let state_shutdown = state.clone();
    let tls_enabled = tls_server_config.is_some();

    // F2: advertise ladex.local alongside the IP-based URLs above — a
    // memorable alternative, not a replacement, since .local resolution
    // isn't universally supported.
    let mdns_handle = mdns::advertise(&local_ips, args.port, tls_enabled);

    if let Some(tls_config) = tls_server_config {
        // Public TLS proxy on args.port; warp itself only listens on
        // loopback, one port up, unreachable from the network directly.
        let internal_port = if args.port == u16::MAX { args.port - 1 } else { args.port + 1 };
        let public_addr: SocketAddr = ([0, 0, 0, 0], args.port).into();
        let internal_addr: SocketAddr = ([127, 0, 0, 1], internal_port).into();

        println!("Access locally: https://localhost:{}", args.port);
        println!("Access from network: https://{local_ip}:{}", args.port);
        println!("Note: your browser will warn about the self-signed certificate on first visit — this is expected for a LAN-local tool with no public CA. Click through (\"Advanced\" → \"Proceed\").");
        if primary_local_ip.is_some() {
            print_qr_code(&format!("https://{local_ip}:{}", args.port));
        }

        let internal_server = warp::serve(routes).run(internal_addr);
        tokio::spawn(internal_server);

        tokio::select! {
            result = tls::run_tls_proxy(public_addr, internal_addr, tls_config) => {
                if let Err(e) = result {
                    tracing::error!("TLS: proxy exited: {e}");
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
    } else {
        let addr: SocketAddr = ([0, 0, 0, 0], args.port).into();

        println!("Access locally: http://localhost:{}", args.port);
        println!("Access from network: http://{local_ip}:{}", args.port);
        if primary_local_ip.is_some() {
            print_qr_code(&format!("http://{local_ip}:{}", args.port));
        }

        let server_fut = warp::serve(routes).run(addr);
        tokio::select! {
            _ = server_fut => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutdown: SIGINT received — sending Goodbye to mesh peers");
                mesh::broadcast_goodbye(&state_shutdown).await;
                if let Some(handle) = mdns_handle {
                    mdns::shutdown(handle).await;
                }
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
