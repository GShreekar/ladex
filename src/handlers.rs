use crate::types::*;
use crate::NodeState;
use warp::{Rejection, Reply};

pub async fn check_auth_status(auth_cookie: Option<String>, state: NodeState) -> Result<impl Reply, Rejection> {
    let is_authenticated = match &state.security_code_legacy {
        None => true, // No auth required
        Some(_) => {
            // Exact match against just the `auth` cookie's value — not a
            // substring check against the whole Cookie header, which could
            // be fooled by an unrelated cookie whose value happens to
            // contain this one's expected value as a substring.
            let expected_cookie = format!("authenticated:{}", state.session_token());
            auth_cookie.as_deref() == Some(expected_cookie.as_str())
        }
    };

    #[derive(serde::Serialize)]
    struct AuthStatusResponse {
        authenticated: bool,
        auth_required: bool,
    }

    let response = AuthStatusResponse {
        authenticated: is_authenticated,
        auth_required: state.security_code_legacy.is_some(),
    };

    Ok(warp::reply::json(&response))
}

pub async fn authenticate(auth_req: AuthRequest, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    let response = match &state.security_code_legacy {
        None => AuthResponse {
            success: true,
            message: None,
        },
        Some(required_code) => {
            if auth_req.code == *required_code {
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
        let cookie_value = format!("authenticated:{}", state.session_token());
        let cookie_header = format!("auth={cookie_value}; Path=/; Max-Age=86400; HttpOnly; SameSite=Strict");
        let reply_with_cookie = warp::reply::with_header(
            json_reply,
            "Set-Cookie",
            cookie_header,
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