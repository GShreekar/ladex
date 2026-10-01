use crate::auth;
use crate::server::{peer_ip, PeerAddr};
use crate::types::*;
use crate::NodeState;
use warp::{Rejection, Reply};

pub async fn check_auth_status(auth_cookie: Option<String>, state: NodeState) -> Result<impl Reply, Rejection> {
    let is_authenticated = match &state.passphrase {
        None => true, // No auth required
        Some(_) => {
            // Exact match against just the `auth` cookie's value — not a
            // substring check against the whole Cookie header, which could
            // be fooled by an unrelated cookie whose value happens to
            // contain this one's expected value as a substring.
            let expected_cookie = format!("authenticated:{}", state.session_token());
            auth_cookie.as_deref().is_some_and(|cookie| auth::secrets_match(&expected_cookie, cookie))
        }
    };

    #[derive(serde::Serialize)]
    struct AuthStatusResponse {
        authenticated: bool,
        auth_required: bool,
    }

    let response = AuthStatusResponse {
        authenticated: is_authenticated,
        auth_required: state.passphrase.is_some(),
    };

    Ok(warp::reply::json(&response))
}

pub async fn authenticate(
    auth_req: AuthRequest,
    peer: Option<PeerAddr>,
    state: NodeState,
) -> Result<Box<dyn Reply>, Rejection> {
    let Some(passphrase) = &state.passphrase else {
        return Ok(Box::new(warp::reply::json(&AuthResponse { success: true, message: None })));
    };

    // Counted before the passphrase is checked, so parallel guesses can't all
    // get through ahead of the first failure being recorded.
    let ip = peer_ip(peer);
    let ticket = match state.auth_limiter.begin(ip) {
        Ok(ticket) => ticket,
        Err(retry_after) => {
            let secs = retry_after.as_secs().max(1);
            tracing::warn!("Login: {ip} is locked out for another {secs}s");
            let reply = warp::reply::with_status(
                warp::reply::json(&AuthResponse {
                    success: false,
                    message: Some(format!("Too many failed attempts. Try again in {secs} seconds.")),
                }),
                warp::http::StatusCode::TOO_MANY_REQUESTS,
            );
            return Ok(Box::new(warp::reply::with_header(reply, "Retry-After", secs.to_string())));
        }
    };

    if !auth::secrets_match(passphrase, &auth_req.passphrase) {
        tracing::warn!("Login: wrong passphrase from {ip}");
        let reply = warp::reply::with_status(
            warp::reply::json(&AuthResponse { success: false, message: Some("Invalid passphrase".to_string()) }),
            warp::http::StatusCode::UNAUTHORIZED,
        );
        return Ok(Box::new(reply));
    }
    state.auth_limiter.succeed(ip, ticket);

    let cookie_header = format!("auth=authenticated:{}; Path=/; Max-Age=86400; HttpOnly; SameSite=Strict", state.session_token());
    let reply = warp::reply::json(&AuthResponse { success: true, message: None });
    Ok(Box::new(warp::reply::with_header(reply, "Set-Cookie", cookie_header)))
}

pub async fn logout() -> Result<impl Reply, Rejection> {
    let response = AuthResponse {
        success: true,
        message: Some("Logged out successfully".to_string()),
    };

    let json_reply = warp::reply::json(&response);
    let reply_with_cookie = warp::reply::with_header(
        json_reply,
        "Set-Cookie",
        "auth=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
    );
    Ok(reply_with_cookie)
}