use crate::types::*;
use crate::AppState;
use warp::{Rejection, Reply};

pub async fn get_peers(state: AppState) -> Result<impl Reply, Rejection> {
    let peers = {
        let peers = state.peers.read().await;
        peers.values().cloned().collect::<Vec<_>>()
    };

    let stats = PeerStats {
        total_peers: peers.len(),
        peers,
    };

    Ok(warp::reply::json(&stats))
}

pub async fn authenticate(auth_req: AuthRequest, state: AppState) -> Result<Box<dyn Reply>, Rejection> {
    let response = match state.security_code {
        None => AuthResponse {
            success: true,
            message: None,
        },
        Some(required_code) => {
            if auth_req.code == required_code {
                AuthResponse {
                    success: true,
                    message: None,
                }
            } else {
                AuthResponse {
                    success: false,
                    message: Some("Invalid security code".to_string()),
                }
            }
        }
    };

    if response.success {
        let json_reply = warp::reply::json(&response);
        let reply_with_cookie = warp::reply::with_header(
            json_reply,
            "Set-Cookie",
            "auth=authenticated; Path=/; Max-Age=86400; HttpOnly; SameSite=Strict",
        );
        Ok(Box::new(reply_with_cookie) as Box<dyn Reply>)
    } else {
        let json_reply = warp::reply::json(&response);
        let reply_with_status = warp::reply::with_status(
            json_reply,
            warp::http::StatusCode::UNAUTHORIZED,
        );
        Ok(Box::new(reply_with_status) as Box<dyn Reply>)
    }
}