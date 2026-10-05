use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use warp::Filter;
use clap::Parser;
use std::time::Duration;

use ladex_core::types::{self, *};
use ladex_core::{auth, discovery, files_api, handlers, hlc, identity, mdns, mesh, pairing, persist, ratelimit, server, sessions, state, store, tls, transfer, trust, websocket, NodeState};
use include_dir::{include_dir, Dir};

static STATIC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../static");

#[derive(Parser)]
#[command(name = "ladex")]
#[command(about = "LADEX - Local Area Data Exchange", long_about = None)]
struct Args {
    /// Passphrase for browser logins and for joining the mesh; without one, anyone on the network can do both.
    passphrase: Option<String>,

    /// Generate a random passphrase and print it.
    #[arg(short = 's', long = "secure", conflicts_with = "passphrase")]
    secure: bool,

    /// Local HTTP/WS port for this node's own browser tab.
    #[arg(long, default_value = "8080")]
    port: u16,

    /// UDP multicast discovery port.
    #[arg(long, default_value = "7878")]
    discovery_port: u16,

    /// Disable UDP multicast discovery (combine with --peer to connect manually).
    #[arg(long)]
    no_discovery: bool,

    /// Peer node to connect to directly, as <ip>:<http_port>; repeatable.
    #[arg(long = "peer")]
    manual_peers: Vec<String>,

    /// Serve plain HTTP/WS instead of TLS; remote browsers then lose streamed downloads and integrity checks.
    #[arg(long)]
    no_tls: bool,

    /// Port for a plain-HTTP listener on 127.0.0.1, free of certificate warnings (default: main port + 1; unused with --no-tls).
    #[arg(long)]
    local_port: Option<u16>,

    /// Where shared files are stored on this machine. Default: ~/.ladex/files
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,

    /// How long a download may wait for the next piece of a file before giving up.
    #[arg(long, default_value = "60")]
    stall_timeout_secs: u64,

    /// Most disk space, in GiB, that shared files may use on this node.
    #[arg(long, default_value = "20")]
    storage_limit_gb: u64,

    /// Keep this node's private key in a file in the data directory instead of the OS keychain.
    #[arg(long)]
    no_keychain: bool,
}


/// Resolves the browser's login session; `None` without a passphrase, otherwise a missing session is rejected.
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

fn with_api_auth(state: NodeState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    with_session(state, true).map(|_| ()).untuple_one()
}

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

#[derive(Debug)]
struct AuthenticationRequired;
impl warp::reject::Reject for AuthenticationRequired {}

// WebSocket upgrades bypass the same-origin policy and CORS, so any web page could open one to this node; the Origin checks below stop that.

#[derive(Debug)]
struct InvalidOrigin;
impl warp::reject::Reject for InvalidOrigin {}

/// For /ws: the Origin header must name this server's own host:port.
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

/// For /mesh: nodes never send an Origin header and browsers always do, so any request with one is refused.
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

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

    let data_dir = args.data_dir.clone().unwrap_or_else(|| tls::config_dir().join("files"));
    let (identity, key_location) = match identity::load_or_create(&data_dir, !args.no_keychain) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    };
    let node_id: NodeId = identity.node_id().to_string();
    let trust = match trust::TrustStore::open(&data_dir) {
        Ok(trust) => Arc::new(trust),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    };
    let saved = persist::load(&data_dir);

    tracing::info!("Node ID: {node_id} (key kept in {key_location})");

    // Before `state` is built: dialing reads state.tls_client_config.
    let local_ips = tls::local_ipv4_addresses();
    let primary_local_ip: Option<IpAddr> = local_ips.first().copied().or_else(get_local_ip);

    let mut tls_fingerprint: Vec<u8> = Vec::new();
    let tls_server_config = if args.no_tls {
        tracing::info!("TLS: disabled via --no-tls — serving plain HTTP/WS");
        None
    } else {
        tls::install_crypto_provider();
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

    // Mesh joins get no global cap: another mesh on the same network would trip it for everyone.
    let auth_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::browser_login_policy()));
    let mesh_limiter = Arc::new(ratelimit::AttemptLimiter::new(ratelimit::Policy {
        free_attempts: 5,
        base_lockout: Duration::from_secs(10),
        max_lockout: Duration::from_secs(600),
        global_cap: None,
    }));

    let store = match store::Store::open(&data_dir, args.storage_limit_gb.saturating_mul(1 << 30)) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!("Error: could not open the file store at {}: {e}", data_dir.display());
            std::process::exit(1);
        }
    };
    println!("Shared files are kept in {} (up to {} GiB)", data_dir.display(), args.storage_limit_gb);

    let state = NodeState {
        local_peers:          Arc::new(RwLock::new(HashMap::new())),
        local_senders:        Arc::new(RwLock::new(HashMap::new())),
        files:                Arc::new(RwLock::new(HashMap::new())),
        messages:             Arc::new(RwLock::new(Vec::new())),
        node_id:              node_id.clone(),
        identity:             Arc::new(identity),
        trust,
        pairings:             Arc::new(pairing::Pairings::new()),
        mesh_peers:           Arc::new(RwLock::new(HashMap::new())),
        passphrase,
        tls_fingerprint,
        auth_limiter,
        mesh_limiter,
        store:                store.clone(),
        transfers:            Arc::new(transfer::Transfers::with_tuning(transfer::Tuning {
            stall_timeout: Duration::from_secs(args.stall_timeout_secs),
            ..Default::default()
        })),
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

    state::load_catalog_from_store(&state).await;
    if let Some(saved) = &saved {
        persist::restore(&state, saved).await;
        if saved.node_id != node_id {
            tracing::info!("Node ID changed from {} to {node_id}, now derived from the node's key", saved.node_id);
            state::adopt_node_id(&state, &saved.node_id).await;
        }
    }
    let saver = persist::spawn_saver(state.clone(), &data_dir, saved.as_ref());

    if !args.manual_peers.is_empty() {
        for peer_addr_str in &args.manual_peers {
            match peer_addr_str.parse::<SocketAddr>() {
                Ok(addr) => {
                    let state_clone = state.clone();
                    let addr_clone = addr;
                    tokio::spawn(async move {
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

    if let Some(saved) = &saved {
        persist::redial(&state, saved.peers.clone());
    }

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
                    tokio::spawn(async move {
                        if let Err(e) = svc.announce_loop(announce_packet).await {
                            tracing::error!("Discovery announce loop error: {e}");
                        }
                    });
                    if let Err(e) = svc_listen.listen_loop(state_listen).await {
                        tracing::error!("Discovery listen loop error: {e}");
                    }
                }
            }
        });
    } else {
        tracing::info!("Discovery: disabled via --no-discovery");
    }

    {
        let state_prune = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                state_prune.sessions.purge_expired();
                state::collect_garbage(&state_prune).await;
                let mut files = state_prune.files.write().await;
                let before = files.len();
                state::prune_tombstones(&mut files, hlc::wall_clock_ms());
                state::prune_unheld(&mut files, hlc::wall_clock_ms());
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

    let app_state_auth = state.clone();
    let auth_route = warp::path("auth")
        .and(warp::post())
        .and(warp::body::content_length_limit(4096))
        .and(warp::body::json())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::header::optional::<String>("user-agent"))
        .and(warp::any().map(move || app_state_auth.clone()))
        .and_then(handlers::authenticate);

    let app_state_logout = state.clone();
    let logout_route = warp::path("logout")
        .and(warp::post())
        .and(require_same_origin())
        .and(warp::cookie::optional("auth"))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || app_state_logout.clone()))
        .and_then(handlers::logout);

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

    let request = || {
        with_api_auth(state.clone())
            .and(warp::ext::optional::<server::PeerAddr>())
            .and(warp::any().map({
                let state = state.clone();
                move || state.clone()
            }))
    };
    let pairing_routes = {
        let status = warp::path!("api" / "pairing").and(warp::get()).and(request()).and_then(handlers::pairing_status);
        let open = warp::path!("api" / "pairing" / "open")
            .and(warp::post())
            .and(require_same_origin())
            .and(request())
            .and_then(handlers::open_pairing);
        let close = warp::path!("api" / "pairing" / "close")
            .and(warp::post())
            .and(require_same_origin())
            .and(request())
            .and_then(handlers::close_pairing);
        let dial = warp::path!("api" / "pairing" / "dial")
            .and(warp::post())
            .and(require_same_origin())
            .and(warp::body::content_length_limit(1024))
            .and(warp::body::json())
            .and(request())
            .and_then(handlers::dial_pairing);
        let answer = warp::path!("api" / "pairing" / u64)
            .and(warp::post())
            .and(require_same_origin())
            .and(warp::body::content_length_limit(1024))
            .and(warp::body::json())
            .and(request())
            .and_then(handlers::answer_pairing);
        status.or(open).or(close).or(dial).or(answer)
    };

    let trust_routes = {
        let list = warp::path!("api" / "trust").and(warp::get()).and(request()).and_then(handlers::trusted_nodes);
        let revoke = warp::path!("api" / "trust" / String / "revoke")
            .and(warp::post())
            .and(require_same_origin())
            .and(request())
            .and_then(handlers::revoke_node);
        list.or(revoke)
    };

    let auth_status_route = warp::path("auth-status")
        .and(warp::get())
        .and(warp::cookie::optional("auth"))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map({
            let s = state.clone();
            move || s.clone()
        }))
        .and_then(handlers::check_auth_status);

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

    let mesh_state = state.clone();
    let mesh_route = warp::path("mesh")
        .and(reject_browser_origin())
        .and(warp::ws())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || mesh_state.clone()))
        .and_then(mesh::mesh_ws_handler);

    let app_state_ws = state.clone();
    let websocket_route = warp::path("ws")
        .and(with_session(state.clone(), false))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(require_same_origin())
        .and(warp::ws())
        .and(warp::any().map(move || app_state_ws.clone()))
        .and_then(websocket::websocket_handler);

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

    // More specific routes first; /mesh must come before /ws.
    let routes = login_route
        .or(auth_route)
        .or(logout_route)
        .or(sessions_list_route)
        .or(sessions_revoke_route)
        .or(pairing_routes)
        .or(trust_routes)
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

    let state_shutdown = state.clone();
    let tls_enabled = tls_server_config.is_some();

    let mdns_handle = mdns::advertise(&local_ips, args.port, tls_enabled);

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
    let api = files_api::FilesApi::new(state.clone());
    if let (Some(listener), Some(port)) = (local_listener, local_http_port) {
        let hosts = vec![format!("localhost:{port}"), format!("127.0.0.1:{port}")];
        let service = service.clone();
        let api = api.clone();
        tokio::spawn(async move {
            if let Err(e) = server::serve(listener, None, Some(hosts), service, api).await {
                tracing::error!("Local HTTP listener exited: {e}");
            }
        });
    }

    let acceptor = tls_server_config.map(tokio_rustls::TlsAcceptor::from);
    tokio::select! {
        result = server::serve(public_listener, acceptor, None, service, api) => {
            if let Err(e) = result {
                tracing::error!("Server exited: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown: SIGINT received — sending Goodbye to mesh peers");
            mesh::broadcast_goodbye(&state_shutdown).await;
            saver.flush().await;
            if let Some(handle) = mdns_handle {
                mdns::shutdown(handle).await;
            }
        }
    }
}

/// Last-resort guess at the LAN address from the routing table; sends no packet.
fn get_local_ip() -> Option<IpAddr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    const LAN_IP: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 20));

    async fn status_for(state: &NodeState, cookie: Option<&str>) -> u16 {
        let protected = with_session(state.clone(), true)
            .map(|_| "ok")
            .recover(|_| async { Ok::<_, std::convert::Infallible>("denied") });
        let mut request = warp::test::request().path("/api");
        if let Some(token) = cookie {
            request = request.header("cookie", format!("auth={token}"));
        }
        match request.reply(&protected).await.body() {
            body if body.as_ref() == b"ok" => 200,
            _ => 401,
        }
    }

    #[tokio::test]
    async fn a_numeric_passphrase_still_requires_login() {
        let state = NodeState::for_tests(Some("123456"));
        assert_eq!(status_for(&state, None).await, 401);
    }

    #[tokio::test]
    async fn a_made_up_cookie_is_not_a_login() {
        let state = NodeState::for_tests(Some("123456"));
        assert_eq!(status_for(&state, Some("not-a-real-token")).await, 401);
    }

    #[tokio::test]
    async fn a_real_session_cookie_gets_through() {
        let state = NodeState::for_tests(Some("123456"));
        let (token, _) = state.sessions.create(LAN_IP, None);
        assert_eq!(status_for(&state, Some(&token)).await, 200);
    }

    #[tokio::test]
    async fn a_node_without_a_passphrase_needs_no_login() {
        let state = NodeState::for_tests(None);
        assert_eq!(status_for(&state, None).await, 200);
    }
}
