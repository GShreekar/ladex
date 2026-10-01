use crate::auth;
use crate::server::{is_tls, peer_ip, PeerAddr};
use crate::sessions::{SessionHandle, SessionSummary, SESSION_LIFETIME};
use crate::types::*;
use crate::NodeState;
use warp::http::StatusCode;
use warp::{Rejection, Reply};

// `Secure` keeps the cookie off plain HTTP; the loopback listener has to go
// without it because Safari refuses Secure cookies on http://localhost.
fn cookie_attributes(secure: bool) -> &'static str {
    if secure { "; Secure" } else { "" }
}

fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "auth={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
        SESSION_LIFETIME.as_secs(),
        cookie_attributes(secure)
    )
}

fn clearing_cookie(secure: bool) -> String {
    format!("auth=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}", cookie_attributes(secure))
}

fn json_error(message: &str, status: StatusCode) -> Box<dyn Reply> {
    Box::new(warp::reply::with_status(
        warp::reply::json(&AuthResponse { success: false, message: Some(message.to_string()) }),
        status,
    ))
}

pub async fn check_auth_status(auth_cookie: Option<String>, state: NodeState) -> Result<impl Reply, Rejection> {
    let is_authenticated = match &state.passphrase {
        None => true, // No auth required
        Some(_) => auth_cookie.as_deref().is_some_and(|token| state.sessions.authenticate(token).is_some()),
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
    user_agent: Option<String>,
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
            let reply = json_error(&format!("Too many failed attempts. Try again in {secs} seconds."), StatusCode::TOO_MANY_REQUESTS);
            return Ok(Box::new(warp::reply::with_header(reply, "Retry-After", secs.to_string())));
        }
    };

    if !auth::secrets_match(passphrase, &auth_req.passphrase) {
        tracing::warn!("Login: wrong passphrase from {ip}");
        return Ok(json_error("Invalid passphrase", StatusCode::UNAUTHORIZED));
    }
    state.auth_limiter.succeed(ip, ticket);

    let (token, session) = state.sessions.create(ip, user_agent.as_deref());
    tracing::info!("Login: new session {} from {ip}", session.id);
    let reply = warp::reply::json(&AuthResponse { success: true, message: None });
    Ok(Box::new(warp::reply::with_header(reply, "Set-Cookie", session_cookie(&token, is_tls(peer)))))
}

// Ends only this device's session; the others stay signed in.
pub async fn logout(auth_cookie: Option<String>, peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    if let Some(session) = auth_cookie.as_deref().and_then(|token| state.sessions.authenticate(token)) {
        state.sessions.revoke(&session.id);
        tracing::info!("Logout: session {} ended", session.id);
    }
    let reply = warp::reply::json(&AuthResponse { success: true, message: Some("Logged out successfully".to_string()) });
    Ok(warp::reply::with_header(reply, "Set-Cookie", clearing_cookie(is_tls(peer))))
}

// The machine running LADEX manages every device; any other device only its own session.
fn can_manage(peer: Option<PeerAddr>) -> bool {
    peer_ip(peer).is_loopback()
}

#[derive(serde::Serialize)]
struct SessionsResponse {
    auth_required: bool,
    can_manage: bool,
    current: Option<String>,
    sessions: Vec<SessionSummary>,
}

pub async fn list_sessions(
    session: Option<SessionHandle>,
    peer: Option<PeerAddr>,
    state: NodeState,
) -> Result<impl Reply, Rejection> {
    let manage = can_manage(peer);
    let current = session.map(|s| s.id);
    let sessions = if state.passphrase.is_none() {
        Vec::new()
    } else {
        state.sessions.list().into_iter().filter(|s| manage || Some(&s.id) == current.as_ref()).collect()
    };
    Ok(warp::reply::json(&SessionsResponse {
        auth_required: state.passphrase.is_some(),
        can_manage: manage,
        current,
        sessions,
    }))
}

pub async fn revoke_session(
    id: String,
    session: Option<SessionHandle>,
    peer: Option<PeerAddr>,
    state: NodeState,
) -> Result<Box<dyn Reply>, Rejection> {
    let Some(me) = session else {
        return Ok(json_error("This node has no sessions", StatusCode::NOT_FOUND));
    };
    if id != me.id && !can_manage(peer) {
        return Ok(json_error("Only the device running LADEX can sign out other devices", StatusCode::FORBIDDEN));
    }
    if state.sessions.revoke(&id) {
        tracing::info!("Session {id} revoked by {}", me.id);
        Ok(Box::new(warp::reply::with_status(warp::reply(), StatusCode::NO_CONTENT)))
    } else {
        Ok(json_error("No such session", StatusCode::NOT_FOUND))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: &str = "192.168.1.20:50000";

    fn peer(addr: &str, tls: bool) -> Option<PeerAddr> {
        Some(PeerAddr { addr: addr.parse().unwrap(), tls })
    }

    fn request(passphrase: &str) -> AuthRequest {
        AuthRequest { passphrase: passphrase.into() }
    }

    async fn login(state: &NodeState, passphrase: &str, addr: &str, tls: bool) -> warp::reply::Response {
        authenticate(request(passphrase), peer(addr, tls), Some("Firefox/130 Linux".into()), state.clone())
            .await
            .unwrap()
            .into_response()
    }

    async fn body_text(response: warp::reply::Response) -> String {
        use http_body_util::BodyExt;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn token_of(response: &warp::reply::Response) -> String {
        let cookie = response.headers().get("set-cookie").unwrap().to_str().unwrap();
        cookie.trim_start_matches("auth=").split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_correct_login_creates_a_session_with_its_own_token() {
        let state = NodeState::for_tests(Some("pw"));
        let first = login(&state, "pw", LAN, true).await;
        let second = login(&state, "pw", LAN, true).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_ne!(token_of(&first), token_of(&second));
        assert_eq!(state.sessions.list().len(), 2);
        assert!(state.sessions.authenticate(&token_of(&first)).is_some());
    }

    #[tokio::test]
    async fn the_cookie_is_secure_only_over_tls() {
        let state = NodeState::for_tests(Some("pw"));
        let over_tls = login(&state, "pw", LAN, true).await;
        let plain = login(&state, "pw", "127.0.0.1:50000", false).await;
        let flags = |r: &warp::reply::Response| r.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        assert!(flags(&over_tls).contains("; Secure") && flags(&over_tls).contains("HttpOnly") && flags(&over_tls).contains("SameSite=Strict"));
        assert!(!flags(&plain).contains("Secure") && flags(&plain).contains("HttpOnly"));
    }

    #[tokio::test]
    async fn a_wrong_passphrase_is_rejected_and_creates_no_session() {
        let state = NodeState::for_tests(Some("pw"));
        let response = login(&state, "nope", LAN, true).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get("set-cookie").is_none());
        assert!(state.sessions.list().is_empty());
    }

    #[tokio::test]
    async fn repeated_wrong_guesses_lock_the_address_out_even_for_the_right_passphrase() {
        let state = NodeState::for_tests(Some("pw"));
        for _ in 0..4 {
            login(&state, "wrong", LAN, true).await;
        }
        let locked = login(&state, "pw", LAN, true).await;
        assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(locked.headers().get("retry-after").is_some());
        assert!(state.sessions.list().is_empty());
        // Another address is unaffected.
        assert_eq!(login(&state, "pw", "192.168.1.21:50000", true).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logging_out_ends_only_that_session() {
        let state = NodeState::for_tests(Some("pw"));
        let mine = token_of(&login(&state, "pw", LAN, true).await);
        let theirs = token_of(&login(&state, "pw", "192.168.1.21:50000", true).await);

        logout(Some(mine.clone()), peer(LAN, true), state.clone()).await.unwrap();
        assert!(state.sessions.authenticate(&mine).is_none());
        assert!(state.sessions.authenticate(&theirs).is_some());
    }

    #[tokio::test]
    async fn a_logged_out_cookie_cannot_be_replayed() {
        let state = NodeState::for_tests(Some("pw"));
        let token = token_of(&login(&state, "pw", LAN, true).await);
        logout(Some(token.clone()), peer(LAN, true), state.clone()).await.unwrap();
        let status = check_auth_status(Some(token), state).await.unwrap().into_response();
        assert!(body_text(status).await.contains("\"authenticated\":false"));
    }

    #[tokio::test]
    async fn only_the_host_machine_can_sign_out_other_devices() {
        let state = NodeState::for_tests(Some("pw"));
        let (_, device_a) = state.sessions.create("192.168.1.20".parse().unwrap(), None);
        let (_, device_b) = state.sessions.create("192.168.1.21".parse().unwrap(), None);

        // Device A may not end B's session...
        let refused = revoke_session(device_b.id.clone(), Some(device_a.clone()), peer(LAN, true), state.clone()).await.unwrap();
        assert_eq!(refused.into_response().status(), StatusCode::FORBIDDEN);
        assert!(state.sessions.is_active(&device_b.id));

        // ...but may end its own.
        let own = revoke_session(device_a.id.clone(), Some(device_a.clone()), peer(LAN, true), state.clone()).await.unwrap();
        assert_eq!(own.into_response().status(), StatusCode::NO_CONTENT);

        // The host machine (loopback) may end anyone's.
        let host = revoke_session(device_b.id.clone(), Some(device_a), peer("127.0.0.1:40000", false), state.clone()).await.unwrap();
        assert_eq!(host.into_response().status(), StatusCode::NO_CONTENT);
        assert!(!state.sessions.is_active(&device_b.id));
    }

    #[tokio::test]
    async fn an_open_node_has_no_sessions_to_list() {
        let state = NodeState::for_tests(None);
        let reply = list_sessions(None, peer("127.0.0.1:1", false), state).await.unwrap().into_response();
        let text = body_text(reply).await;
        assert!(text.contains("\"auth_required\":false") && text.contains("\"sessions\":[]"));
    }
}
