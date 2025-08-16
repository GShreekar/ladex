use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use warp::Filter;

mod types;
mod websocket;
mod handlers;

use types::*;
use include_dir::{include_dir, Dir};
use mime_guess;

// Embed the static directory at compile time
static STATIC_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/static");

type Peers = Arc<RwLock<HashMap<SessionId, PeerInfo>>>;
type Files = Arc<RwLock<HashMap<String, FileMetadata>>>;
type Messages = Arc<RwLock<Vec<types::TextMessage>>>;

#[derive(Clone)]
pub struct AppState {
    pub peers: Peers,
    pub files: Files,
    pub messages: Messages,
    pub tx: broadcast::Sender<ServerMessage>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    
    let (tx, _rx) = broadcast::channel::<ServerMessage>(1000);
    
    let app_state = AppState {
        peers: Arc::new(RwLock::new(HashMap::new())),
        files: Arc::new(RwLock::new(HashMap::new())),
        messages: Arc::new(RwLock::new(Vec::new())),
        tx,
    };

    // Serve embedded static assets under /static/<path>
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
    
    // Serve embedded index.html at root
    let index = warp::path::end().and_then(|| async move {
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

    // WebSocket endpoint
    let app_state_ws = app_state.clone();
    let websocket = warp::path("ws")
        .and(warp::ws())
        .and(warp::any().map(move || app_state_ws.clone()))
        .and_then(websocket::websocket_handler);

    // API endpoints
    let app_state_api = app_state.clone();
    let api = warp::path("api")
        .and(
            warp::path("peers")
                .and(warp::get())
                .and(warp::any().map(move || app_state_api.clone()))
                .and_then(handlers::get_peers)
        );

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);

    // IMPORTANT: More specific routes first
    let routes = websocket
        .or(api)
        .or(static_route)
        .or(index)
        .with(cors);

    let addr: SocketAddr = ([0, 0, 0, 0], 8080).into();
    let local_ip = get_local_ip().unwrap_or_else(|| "YOUR_IP".to_string());
    
    println!("Access locally: http://localhost:8080");
    println!("Access from network: http://{}:8080", local_ip);
    
    warp::serve(routes)
        .run(addr)
        .await;
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
