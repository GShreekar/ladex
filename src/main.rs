use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use warp::Filter;
use clap::Parser;
use rand::Rng;

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

#[derive(Parser)]
#[command(name = "ladex")]
#[command(about = "LADEX - Local Area Data Exchange", long_about = None)]
struct Args {
    code: Option<String>,
    #[arg(short = 's', long = "secure")]
    secure: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub peers: Peers,
    pub files: Files,
    pub messages: Messages,
    pub tx: broadcast::Sender<ServerMessage>,
    pub security_code: Option<String>,
}

fn generate_random_code() -> String {
    let mut rng = rand::thread_rng();
    format!("{:06}", rng.gen_range(100000..1000000))
}

fn validate_code(code: &str) -> bool {
    code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())
}

fn with_auth(state: AppState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::any()
        .and(warp::cookie::optional("auth"))
        .and(warp::any().map(move || state.clone()))
        .and_then(|auth_cookie: Option<String>, state: AppState| async move {
            match state.security_code {
                None => Ok(()),
                Some(_) => match auth_cookie {
                    Some(cookie) if cookie == "authenticated" => Ok(()),
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    
    let args = Args::parse();
    
    // Handle security code logic
    let security_code = if args.secure {
        let code = generate_random_code();
        println!("Generated security code: {}", code);
        Some(code)
    } else if let Some(code) = args.code {
        if validate_code(&code) {
            Some(code)
        } else {
            eprintln!("Error: Security code must be exactly 6 digits");
            std::process::exit(1);
        }
    } else {
        None
    };
    
    let (tx, _rx) = broadcast::channel::<ServerMessage>(1000);
    
    let app_state = AppState {
        peers: Arc::new(RwLock::new(HashMap::new())),
        files: Arc::new(RwLock::new(HashMap::new())),
        messages: Arc::new(RwLock::new(Vec::new())),
        tx,
        security_code,
    };

    // Login page route - not protected
    let app_state_login = app_state.clone();
    let login_route = warp::path("login")
        .and(warp::get())
        .and(warp::any().map(move || app_state_login.clone()))
        .and_then(|state: AppState| async move {
            if state.security_code.is_some() {
                serve_login_page().await
            } else {
                // No auth required, redirect to main page
                let redirect = warp::redirect::temporary(warp::http::Uri::from_static("/"));
                Ok::<_, warp::Rejection>(Box::new(redirect) as Box<dyn warp::Reply>)
            }
        });

    // Auth endpoint
    let app_state_auth = app_state.clone();
    let auth_route = warp::path("auth")
        .and(warp::post())
        .and(warp::body::json())
        .and(warp::any().map(move || app_state_auth.clone()))
        .and_then(handlers::authenticate);

    // Serve embedded static assets under /static/<path> - not protected
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
    
    // Serve embedded index.html at root - protected
    let index = warp::path::end()
        .and(with_auth(app_state.clone()))
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

    // WebSocket endpoint - protected
    let app_state_ws = app_state.clone();
    let websocket = warp::path("ws")
        .and(with_auth(app_state.clone()))
        .and(warp::ws())
        .and(warp::any().map(move || app_state_ws.clone()))
        .and_then(websocket::websocket_handler);

    // API endpoints - protected
    let app_state_api = app_state.clone();
    let api = warp::path("api")
        .and(with_auth(app_state.clone()))
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

    // IMPORTANT: More specific routes first, unprotected routes before protected ones
    let routes = login_route
        .or(auth_route)
        .or(static_route)
        .or(websocket)
        .or(api)
        .or(index)
        .with(cors)
        .recover(handle_rejection);

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
