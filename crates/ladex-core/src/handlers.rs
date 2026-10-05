use crate::auth;
use crate::server::{is_tls, peer_ip, PeerAddr};
use crate::sessions::{SessionHandle, SessionSummary, SESSION_LIFETIME};
use crate::types::*;
use crate::NodeState;
use warp::http::StatusCode;
use warp::{Rejection, Reply};

// The loopback listener can't use `Secure`: Safari refuses Secure cookies on http://localhost.
fn cookie_attributes(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

fn session_cookie(token: &str, secure: bool) -> String {
    format!("auth={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}", SESSION_LIFETIME.as_secs(), cookie_attributes(secure))
}

fn clearing_cookie(secure: bool) -> String {
    format!("auth=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}", cookie_attributes(secure))
}

fn json_error(message: &str, status: StatusCode) -> Box<dyn Reply> {
    Box::new(warp::reply::with_status(warp::reply::json(&AuthResponse { success: false, message: Some(message.to_string()) }), status))
}

pub async fn check_auth_status(auth_cookie: Option<String>, peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    let is_authenticated = match &state.passphrase {
        None => true,
        Some(_) => auth_cookie.as_deref().is_some_and(|token| state.sessions.authenticate(token).is_some()),
    };

    #[derive(serde::Serialize)]
    struct AuthStatusResponse {
        authenticated: bool,
        auth_required: bool,
        /// This node's id, to tell which shared files came through it.
        node_id: String,
        /// The request comes from the machine running this node.
        is_host: bool,
    }

    let response = AuthStatusResponse {
        authenticated: is_authenticated,
        auth_required: state.passphrase.is_some(),
        node_id: state.node_id.clone(),
        is_host: can_manage(peer),
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

    // Counted before the passphrase is checked, so parallel guesses can't slip through before the first failure.
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

/// Ends only this device's session; the others stay signed in.
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

pub async fn list_sessions(session: Option<SessionHandle>, peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    let manage = can_manage(peer);
    let current = session.map(|s| s.id);
    let sessions = if state.passphrase.is_none() {
        Vec::new()
    } else {
        state.sessions.list().into_iter().filter(|s| manage || Some(&s.id) == current.as_ref()).collect()
    };
    Ok(warp::reply::json(&SessionsResponse { auth_required: state.passphrase.is_some(), can_manage: manage, current, sessions }))
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

#[derive(serde::Serialize)]
struct PairingResponse {
    can_pair: bool,
    /// What to type on the other device to dial this one.
    address: Option<String>,
    #[serde(flatten)]
    status: Option<crate::pairing::Status>,
}

pub async fn pairing_status(peer: Option<PeerAddr>, state: NodeState) -> Result<impl Reply, Rejection> {
    let can_pair = can_manage(peer);
    let response = PairingResponse {
        can_pair,
        address: state.local_ip.filter(|_| can_pair).map(|ip| std::net::SocketAddr::new(ip, state.http_port).to_string()),
        status: can_pair.then(|| state.pairings.status()),
    };
    Ok(warp::reply::json(&response))
}

pub async fn open_pairing(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if !can_manage(peer) {
        return Ok(pairing_forbidden());
    }
    state.pairings.open();
    Ok(no_content())
}

pub async fn close_pairing(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if !can_manage(peer) {
        return Ok(pairing_forbidden());
    }
    state.pairings.close();
    Ok(no_content())
}

#[derive(Debug, serde::Deserialize)]
pub struct DialRequest {
    pub address: String,
}

/// Dials another node to pair with it; the outcome shows up in the pairing status.
pub async fn dial_pairing(request: DialRequest, peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if !can_manage(peer) {
        return Ok(pairing_forbidden());
    }
    let Some(target) = parse_node_address(&request.address, state.http_port) else {
        return Ok(json_error("Enter the other device's address, like 192.168.1.20:8080", StatusCode::BAD_REQUEST));
    };
    tokio::spawn(async move {
        let result = crate::mesh::pair_with_peer(target.ip(), target.port(), state.clone()).await;
        let name = result.as_ref().map_or_else(|_| target.to_string(), Clone::clone);
        state.pairings.record(crate::pairing::Outcome::of(&name, &result));
    });
    Ok(Box::new(warp::reply::with_status(warp::reply(), StatusCode::ACCEPTED)))
}

#[derive(Debug, serde::Deserialize)]
pub struct PairingAnswer {
    pub accepted: bool,
}

pub async fn answer_pairing(id: u64, answer: PairingAnswer, peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if !can_manage(peer) {
        return Ok(pairing_forbidden());
    }
    if state.pairings.answer(id, answer.accepted) {
        Ok(no_content())
    } else {
        Ok(json_error("That pairing is no longer waiting", StatusCode::NOT_FOUND))
    }
}

#[derive(serde::Serialize)]
struct TrustResponse {
    can_manage: bool,
    /// False for an open node, which trusts nobody and so has no one to revoke.
    secured: bool,
    nodes: Vec<TrustedNodeView>,
}

#[derive(serde::Serialize)]
struct TrustedNodeView {
    #[serde(flatten)]
    node: crate::trust::TrustedNode,
    connected: bool,
    /// Who revoked it, when it is revoked.
    revoked_by: Option<String>,
}

/// The nodes this one trusts or has revoked, for the person at this machine.
pub async fn trusted_nodes(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    let can_manage = can_manage(peer);
    let mut response = TrustResponse { can_manage, secured: state.passphrase.is_some(), nodes: Vec::new() };
    if !can_manage {
        return Ok(Box::new(warp::reply::json(&response)));
    }
    let nodes = match state.trust.list() {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::error!("Trust: could not list the trusted nodes: {e:#}");
            return Ok(json_error("Could not read the trust store", StatusCode::INTERNAL_SERVER_ERROR));
        }
    };
    let connected = state.mesh_peers.read().await;
    for node in nodes {
        let revoked_by = node.revoked.then(|| revoker_name(&state, &node.node_id));
        response.nodes.push(TrustedNodeView { connected: connected.contains_key(&node.node_id), revoked_by, node });
    }
    Ok(Box::new(warp::reply::json(&response)))
}

fn revoker_name(state: &NodeState, node_id: &str) -> String {
    let Ok(Some(revocation)) = state.trust.revocation(node_id) else {
        return "this device".to_string();
    };
    if revocation.issuer == state.identity.public_key() {
        return "this device".to_string();
    }
    let issuer = revocation.issuer_id();
    match state.trust.get(&issuer) {
        Ok(Some(node)) => node.name,
        _ => issuer,
    }
}

/// Revokes a node in this node's name; the rest of the mesh drops it too.
pub async fn revoke_node(node_id: String, peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if !can_manage(peer) {
        return Ok(json_error("Revoking is only available on the machine running LADEX, at http://localhost", StatusCode::FORBIDDEN));
    }
    match crate::mesh::revoke_node(&state, &node_id).await {
        Ok(crate::mesh::Revoked::Now | crate::mesh::Revoked::Already) => Ok(no_content()),
        Ok(crate::mesh::Revoked::UnknownNode) => Ok(json_error("This device does not know that node", StatusCode::NOT_FOUND)),
        Err(e) => {
            tracing::error!("Trust: could not revoke {node_id}: {e:#}");
            Ok(json_error("Could not record the revocation", StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

// "ip:port", or just "ip" for a node on the same port as this one.
fn parse_node_address(address: &str, default_port: u16) -> Option<std::net::SocketAddr> {
    let address = address.trim();
    address
        .parse::<std::net::SocketAddr>()
        .ok()
        .or_else(|| address.parse::<std::net::IpAddr>().ok().map(|ip| std::net::SocketAddr::new(ip, default_port)))
}

fn pairing_forbidden() -> Box<dyn Reply> {
    json_error("Pairing is only available on the machine running LADEX, at http://localhost", StatusCode::FORBIDDEN)
}

fn no_content() -> Box<dyn Reply> {
    Box::new(warp::reply::with_status(warp::reply(), StatusCode::NO_CONTENT))
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
        authenticate(request(passphrase), peer(addr, tls), Some("Firefox/130 Linux".into()), state.clone()).await.unwrap().into_response()
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
        assert!(
            flags(&over_tls).contains("; Secure") && flags(&over_tls).contains("HttpOnly") && flags(&over_tls).contains("SameSite=Strict")
        );
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
        let status = check_auth_status(Some(token), peer(LAN, true), state).await.unwrap().into_response();
        assert!(body_text(status).await.contains("\"authenticated\":false"));
    }

    #[tokio::test]
    async fn only_the_host_machine_can_sign_out_other_devices() {
        let state = NodeState::for_tests(Some("pw"));
        let (_, device_a) = state.sessions.create("192.168.1.20".parse().unwrap(), None);
        let (_, device_b) = state.sessions.create("192.168.1.21".parse().unwrap(), None);

        let refused = revoke_session(device_b.id.clone(), Some(device_a.clone()), peer(LAN, true), state.clone()).await.unwrap();
        assert_eq!(refused.into_response().status(), StatusCode::FORBIDDEN);
        assert!(state.sessions.is_active(&device_b.id));

        let own = revoke_session(device_a.id.clone(), Some(device_a.clone()), peer(LAN, true), state.clone()).await.unwrap();
        assert_eq!(own.into_response().status(), StatusCode::NO_CONTENT);

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

    fn trust_a_node(state: &NodeState) -> String {
        let other = crate::identity::Identity::generate();
        state.trust.trust(&other.public_key(), "laptop", crate::trust::TrustedVia::Passphrase, 100).unwrap();
        other.node_id().to_string()
    }

    async fn trust_list(state: &NodeState, addr: &str) -> serde_json::Value {
        let reply = trusted_nodes(peer(addr, false), state.clone()).await.unwrap().into_response();
        serde_json::from_str(&body_text(reply).await).unwrap()
    }

    #[tokio::test]
    async fn trusted_nodes_are_hidden_from_other_devices() {
        let state = NodeState::for_tests(Some("pw"));
        trust_a_node(&state);
        let body = trust_list(&state, LAN).await;
        assert_eq!((body["can_manage"].as_bool(), body["nodes"].as_array().map(Vec::len)), (Some(false), Some(0)));
    }

    #[tokio::test]
    async fn only_the_machine_running_ladex_can_revoke_a_node() {
        let state = NodeState::for_tests(Some("pw"));
        let node_id = trust_a_node(&state);
        let refused = revoke_node(node_id.clone(), peer(LAN, true), state.clone()).await.unwrap().into_response();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(state.trust.revocation(&node_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_node_revoked_here_is_listed_as_revoked_by_this_device() {
        let state = NodeState::for_tests(Some("pw"));
        let node_id = trust_a_node(&state);
        let reply = revoke_node(node_id.clone(), peer("127.0.0.1:1", false), state.clone()).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::NO_CONTENT);
        let body = trust_list(&state, "127.0.0.1:1").await;
        let node = &body["nodes"][0];
        assert_eq!((node["node_id"].as_str(), node["revoked"].as_bool()), (Some(node_id.as_str()), Some(true)));
        assert_eq!(node["revoked_by"], "this device");
    }

    #[tokio::test]
    async fn revoking_a_node_this_one_does_not_know_is_not_found() {
        let state = NodeState::for_tests(Some("pw"));
        let reply = revoke_node("nobody".into(), peer("127.0.0.1:1", false), state).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pairing_is_hidden_from_other_devices() {
        let state = NodeState::for_tests(Some("pw"));
        let reply = pairing_status(peer(LAN, true), state).await.unwrap().into_response();
        let body: serde_json::Value = serde_json::from_str(&body_text(reply).await).unwrap();
        assert_eq!(body["can_pair"], false);
        assert!(body.get("waiting").is_none());
    }

    #[tokio::test]
    async fn only_the_machine_running_ladex_can_open_pairing() {
        let state = NodeState::for_tests(Some("pw"));
        let reply = open_pairing(peer(LAN, true), state.clone()).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
        assert!(!state.pairings.is_open());
    }

    #[tokio::test]
    async fn the_machine_running_ladex_can_open_and_close_pairing() {
        let state = NodeState::for_tests(Some("pw"));
        open_pairing(peer("127.0.0.1:1", false), state.clone()).await.unwrap();
        assert!(state.pairings.is_open());
        close_pairing(peer("127.0.0.1:1", false), state.clone()).await.unwrap();
        assert!(!state.pairings.is_open());
    }

    #[tokio::test]
    async fn another_device_cannot_answer_a_pairing() {
        let state = NodeState::for_tests(Some("pw"));
        let reply = answer_pairing(0, PairingAnswer { accepted: true }, peer(LAN, true), state).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn dialing_a_malformed_address_is_rejected() {
        let state = NodeState::for_tests(Some("pw"));
        let request = DialRequest { address: "not an address".into() };
        let reply = dial_pairing(request, peer("127.0.0.1:1", false), state).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_node_address_without_a_port_uses_this_nodes_port() {
        assert_eq!(parse_node_address(" 192.168.1.20 ", 8080), Some("192.168.1.20:8080".parse().unwrap()));
        assert_eq!(parse_node_address("192.168.1.20:9000", 8080), Some("192.168.1.20:9000".parse().unwrap()));
        assert_eq!(parse_node_address("laptop", 8080), None);
    }
}
