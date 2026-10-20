//! The routes of a node's own web page: sign-in, sessions, pairing, trust, the WebSocket and the built UI.

use std::convert::Infallible;
use std::sync::Arc;
use tower_service::Service;
use warp::Filter;

use crate::{handlers, server, sessions, settings_api, websocket, NodeState};

/// The files of a built web UI, looked up by their path in the build.
pub trait UiAssets: Send + Sync {
    fn file(&self, path: &str) -> Option<&[u8]>;
}

#[derive(Debug)]
struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

#[derive(Debug)]
struct InvalidOrigin;
impl warp::reject::Reject for InvalidOrigin {}

/// Resolves the browser's login session; `None` without a passphrase, otherwise a missing session is rejected.
fn with_session(state: NodeState) -> impl Filter<Extract = (Option<sessions::SessionHandle>,), Error = warp::Rejection> + Clone {
    warp::cookie::optional("auth").and(warp::any().map(move || state.clone())).and_then(
        move |token: Option<String>, state: NodeState| async move {
            if state.passphrase.is_none() {
                return Ok(None);
            }
            token.and_then(|t| state.sessions.authenticate(&t)).map(Some).ok_or_else(|| warp::reject::custom(Unauthorized))
        },
    )
}

fn with_api_auth(state: NodeState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    with_session(state).map(|_| ()).untuple_one()
}

// WebSocket upgrades bypass the same-origin policy and CORS, so any web page could open one to this node.
/// The Origin header must name this server's own host:port.
fn require_same_origin() -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("origin")
        .and(warp::header::optional::<String>("host"))
        .and_then(|origin: Option<String>, host: Option<String>| async move {
            let origin_host = origin.as_deref().and_then(|o| o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")));
            match (origin_host, host.as_deref()) {
                (Some(o), Some(h)) if o == h => Ok(()),
                _ => Err(warp::reject::custom(InvalidOrigin)),
            }
        })
        .untuple_one()
}

async fn handle_rejection(err: warp::Rejection) -> Result<Box<dyn warp::Reply>, Infallible> {
    if err.find::<Unauthorized>().is_some() {
        Ok(Box::new(warp::reply::with_status("Unauthorized", warp::http::StatusCode::UNAUTHORIZED)) as Box<dyn warp::Reply>)
    } else if err.find::<InvalidOrigin>().is_some() {
        Ok(Box::new(warp::reply::with_status("Forbidden", warp::http::StatusCode::FORBIDDEN)) as Box<dyn warp::Reply>)
    } else if err.is_not_found() {
        Ok(Box::new(warp::reply::with_status("Not Found", warp::http::StatusCode::NOT_FOUND)) as Box<dyn warp::Reply>)
    } else {
        Ok(Box::new(warp::reply::with_status("Internal Server Error", warp::http::StatusCode::INTERNAL_SERVER_ERROR))
            as Box<dyn warp::Reply>)
    }
}

fn embedded_file(assets: &dyn UiAssets, path: &str) -> Result<Box<dyn warp::Reply>, warp::Rejection> {
    let contents = assets.file(path).ok_or_else(warp::reject::not_found)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
    Ok(Box::new(warp::reply::with_header(warp::reply::html(contents.to_vec()), "content-type", mime)))
}

/// The caller's address and this node's state, for handlers that need both.
fn request_context(state: NodeState) -> impl Filter<Extract = (Option<server::PeerAddr>, NodeState), Error = warp::Rejection> + Clone {
    with_api_auth(state.clone()).and(warp::ext::optional::<server::PeerAddr>()).and(warp::any().map(move || state.clone()))
}

// The page shows its own sign-in form and posts the passphrase here.
fn sign_in_routes(state: NodeState) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let auth_state = state.clone();
    let auth = warp::path("auth")
        .and(warp::post())
        .and(warp::body::content_length_limit(4096))
        .and(warp::body::json())
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::header::optional::<String>("user-agent"))
        .and(warp::any().map(move || auth_state.clone()))
        .and_then(handlers::authenticate);

    let logout_state = state;
    let logout = warp::path("logout")
        .and(warp::post())
        .and(require_same_origin())
        .and(warp::cookie::optional("auth"))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || logout_state.clone()))
        .and_then(handlers::logout);

    auth.or(logout)
}

fn session_routes(state: NodeState) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let list_state = state.clone();
    let list = warp::path!("api" / "sessions")
        .and(warp::get())
        .and(with_session(state.clone()))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || list_state.clone()))
        .and_then(handlers::list_sessions);

    let revoke_state = state.clone();
    let revoke = warp::path!("api" / "sessions" / String)
        .and(warp::delete())
        .and(require_same_origin())
        .and(with_session(state))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || revoke_state.clone()))
        .and_then(handlers::revoke_session);

    list.or(revoke)
}

fn pairing_routes(state: NodeState) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let status = warp::path!("api" / "pairing").and(warp::get()).and(request_context(state.clone())).and_then(handlers::pairing_status);
    let open = warp::path!("api" / "pairing" / "open")
        .and(warp::post())
        .and(require_same_origin())
        .and(request_context(state.clone()))
        .and_then(handlers::open_pairing);
    let close = warp::path!("api" / "pairing" / "close")
        .and(warp::post())
        .and(require_same_origin())
        .and(request_context(state.clone()))
        .and_then(handlers::close_pairing);
    let dial = warp::path!("api" / "pairing" / "dial")
        .and(warp::post())
        .and(require_same_origin())
        .and(warp::body::content_length_limit(1024))
        .and(warp::body::json())
        .and(request_context(state.clone()))
        .and_then(handlers::dial_pairing);
    let answer = warp::path!("api" / "pairing" / u64)
        .and(warp::post())
        .and(require_same_origin())
        .and(warp::body::content_length_limit(1024))
        .and(warp::body::json())
        .and(request_context(state))
        .and_then(handlers::answer_pairing);
    status.or(open).or(close).or(dial).or(answer)
}

fn trust_routes(state: NodeState) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let list = warp::path!("api" / "trust").and(warp::get()).and(request_context(state.clone())).and_then(handlers::trusted_nodes);
    let revoke = warp::path!("api" / "trust" / String / "revoke")
        .and(warp::post())
        .and(require_same_origin())
        .and(request_context(state))
        .and_then(handlers::revoke_node);
    list.or(revoke)
}

fn settings_routes(state: NodeState) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let get =
        warp::path!("api" / "preferences").and(warp::get()).and(request_context(state.clone())).and_then(settings_api::get_preferences);
    let update = warp::path!("api" / "preferences")
        .and(warp::put())
        .and(require_same_origin())
        .and(warp::body::content_length_limit(4096))
        .and(warp::body::json())
        .and(request_context(state.clone()))
        .and_then(settings_api::update_preferences);
    let passphrase = warp::path!("api" / "passphrase")
        .and(warp::put())
        .and(require_same_origin())
        .and(warp::body::content_length_limit(4096))
        .and(warp::body::json())
        .and(request_context(state.clone()))
        .and_then(settings_api::change_passphrase);
    let current_passphrase =
        warp::path!("api" / "passphrase").and(warp::get()).and(request_context(state.clone())).and_then(settings_api::show_passphrase);
    let suggestion = warp::path!("api" / "passphrase" / "suggestion")
        .and(warp::get())
        .and(with_api_auth(state.clone()))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and_then(settings_api::suggest_passphrase);
    let doctor = warp::path!("api" / "doctor").and(warp::get()).and(request_context(state)).and_then(settings_api::diagnose);
    get.or(update).or(passphrase).or(current_passphrase).or(suggestion).or(doctor)
}

// The page itself is public code that signs the person in; what it reads and changes needs a session.
fn page_routes(state: NodeState, ui: &Arc<dyn UiAssets>) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let auth_status_state = state.clone();
    let auth_status = warp::path("auth-status")
        .and(warp::get())
        .and(warp::cookie::optional("auth"))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(warp::any().map(move || auth_status_state.clone()))
        .and_then(handlers::check_auth_status);

    // Built files carry a content hash in their name, so a browser may keep them for good.
    let built_assets = ui.clone();
    let assets = warp::path("assets").and(warp::path::tail()).and_then(move |tail: warp::filters::path::Tail| {
        let assets = built_assets.clone();
        async move {
            let reply = embedded_file(assets.as_ref(), &format!("assets/{}", tail.as_str()))?;
            Ok::<_, warp::Rejection>(
                Box::new(warp::reply::with_header(reply, "cache-control", "public, max-age=31536000, immutable")) as Box<dyn warp::Reply>
            )
        }
    });

    let favicon_assets = ui.clone();
    let favicon = warp::path("favicon.ico").or(warp::path("favicon.svg")).unify().and(warp::get()).and_then(move || {
        let assets = favicon_assets.clone();
        async move { embedded_file(assets.as_ref(), "favicon.svg") }
    });

    let websocket_state = state.clone();
    let websocket = warp::path("ws")
        .and(with_session(state.clone()))
        .and(warp::ext::optional::<server::PeerAddr>())
        .and(require_same_origin())
        .and(warp::ws())
        .and(warp::any().map(move || websocket_state.clone()))
        .and_then(websocket::websocket_handler);

    let index_assets = ui.clone();
    let index = warp::path::end().and_then(move || {
        let assets = index_assets.clone();
        async move { embedded_file(assets.as_ref(), "index.html") }
    });

    auth_status.or(assets).or(favicon).or(websocket).or(index)
}

/// The HTTP service for this node's own page; guests and file transfers are served beside it by `server::Chain`.
pub fn service(
    state: NodeState,
    ui: &Arc<dyn UiAssets>,
) -> impl Service<hyper::Request<hyper::body::Incoming>, Response = warp::reply::Response, Error = Infallible, Future: Send>
       + Clone
       + Send
       + 'static {
    let cors = warp::cors().allow_any_origin().allow_headers(vec!["content-type"]).allow_methods(vec!["GET", "POST", "PUT", "DELETE"]);
    let routes = sign_in_routes(state.clone())
        .or(session_routes(state.clone()))
        .or(pairing_routes(state.clone()))
        .or(trust_routes(state.clone()))
        .or(settings_routes(state.clone()))
        .or(page_routes(state, ui))
        .with(cors)
        .recover(handle_rejection);
    warp::service(routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN_IP: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 20));

    async fn status_for(state: &NodeState, cookie: Option<&str>) -> u16 {
        let protected = with_session(state.clone()).map(|_| "ok").recover(|_| async { Ok::<_, Infallible>("denied") });
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
