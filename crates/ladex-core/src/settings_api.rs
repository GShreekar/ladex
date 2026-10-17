//! The routes behind the app's settings and doctor pages. They answer only the person at the machine running the node.

use serde::{Deserialize, Serialize};
use warp::http::StatusCode;
use warp::{Rejection, Reply};

use crate::handlers::{can_manage, json_error};
use crate::server::PeerAddr;
use crate::{auth, control, doctor, preferences, NodeState};

const HOST_ONLY: &str = "Settings are only available on the machine running LADEX, at http://localhost";

#[derive(Deserialize)]
pub struct NewPassphrase {
    passphrase: String,
}

#[derive(Serialize)]
struct Suggestion {
    passphrase: String,
}

#[derive(Serialize)]
struct CurrentPassphrase {
    passphrase: Option<String>,
}

#[derive(Serialize)]
struct Diagnosis {
    checks: Vec<doctor::Check>,
}

fn host_only(peer: Option<PeerAddr>) -> Option<Box<dyn Reply>> {
    (!can_manage(peer)).then(|| json_error(HOST_ONLY, StatusCode::FORBIDDEN))
}

fn reply_with_view(state: &NodeState) -> Box<dyn Reply> {
    Box::new(warp::reply::json(&preferences::view(state)))
}

pub async fn get_preferences(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    Ok(host_only(peer).unwrap_or_else(|| reply_with_view(&state)))
}

pub async fn update_preferences(
    update: preferences::Update,
    peer: Option<PeerAddr>,
    state: NodeState,
) -> Result<Box<dyn Reply>, Rejection> {
    if let Some(refusal) = host_only(peer) {
        return Ok(refusal);
    }
    match preferences::apply(&state, &update) {
        Ok(_) => Ok(reply_with_view(&state)),
        Err(e) => Ok(json_error(&e.to_string(), StatusCode::BAD_REQUEST)),
    }
}

pub async fn change_passphrase(request: NewPassphrase, peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if let Some(refusal) = host_only(peer) {
        return Ok(refusal);
    }
    match preferences::change_passphrase(&state, state.started_with.use_keychain, &request.passphrase) {
        Ok(()) => Ok(reply_with_view(&state)),
        Err(e) => Ok(json_error(&format!("{e:#}"), StatusCode::BAD_REQUEST)),
    }
}

/// The passphrase the running node uses, so the person at its machine can add another computer to the mesh.
pub async fn show_passphrase(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    let current = CurrentPassphrase { passphrase: state.passphrase.as_deref().map(String::from) };
    Ok(host_only(peer).unwrap_or_else(|| Box::new(warp::reply::json(&current))))
}

pub async fn suggest_passphrase(peer: Option<PeerAddr>) -> Result<Box<dyn Reply>, Rejection> {
    Ok(host_only(peer).unwrap_or_else(|| Box::new(warp::reply::json(&Suggestion { passphrase: auth::generate_passphrase() }))))
}

pub async fn diagnose(peer: Option<PeerAddr>, state: NodeState) -> Result<Box<dyn Reply>, Rejection> {
    if let Some(refusal) = host_only(peer) {
        return Ok(refusal);
    }
    let status = control::status(&state, &state.started_with).await;
    Ok(Box::new(warp::reply::json(&Diagnosis { checks: doctor::examine(&status).await })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use warp::http::StatusCode;

    fn lan_peer() -> Option<PeerAddr> {
        Some(PeerAddr { addr: "192.168.1.30:50000".parse().unwrap(), tls: true })
    }

    fn this_machine() -> Option<PeerAddr> {
        Some(PeerAddr { addr: "127.0.0.1:50000".parse().unwrap(), tls: false })
    }

    #[tokio::test]
    async fn another_device_cannot_read_the_preferences() {
        let reply = get_preferences(lan_peer(), NodeState::for_tests(None)).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn another_device_cannot_change_them() {
        let state = NodeState::for_tests(None);
        let update = preferences::Update { storage_limit_gib: Some(1), ..Default::default() };
        let reply = update_preferences(update, lan_peer(), state.clone()).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
        assert_eq!(state.preferences.get().storage_limit_gib, preferences::Preferences::default().storage_limit_gib);
    }

    #[tokio::test]
    async fn another_device_cannot_change_the_passphrase() {
        let state = NodeState::for_tests(None);
        let reply =
            change_passphrase(NewPassphrase { passphrase: "new-one".into() }, lan_peer(), state.clone()).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
        assert!(!state.preferences.needs_restart());
    }

    #[tokio::test]
    async fn only_this_machine_can_see_the_passphrase() {
        let state = NodeState::for_tests(Some("correct-horse"));
        let refused = show_passphrase(lan_peer(), state.clone()).await.unwrap().into_response();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let shown = show_passphrase(this_machine(), state).await.unwrap().into_response();
        let body = http_body_util::BodyExt::collect(shown.into_body()).await.unwrap().to_bytes();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap()["passphrase"], "correct-horse");
    }

    #[tokio::test]
    async fn another_device_cannot_run_the_doctor() {
        let reply = diagnose(lan_peer(), NodeState::for_tests(None)).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn this_machine_can_read_the_preferences() {
        let reply = get_preferences(this_machine(), NodeState::for_tests(None)).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_refused_change_is_a_bad_request_that_says_why() {
        let update = preferences::Update { storage_limit_gib: Some(0), ..Default::default() };
        let reply = update_preferences(update, this_machine(), NodeState::for_tests(None)).await.unwrap().into_response();
        assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
    }
}
