//! What the person at the machine running LADEX does about guests: invite them, see them, sign them out, accept their uploads.

use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use serde::Deserialize;

use super::api::{refuse, require_same_origin, GuestApi, Outcome};
use super::invites::display_code;
use super::{approve_upload, deny_upload, invite_link, revoke_guest, waiting_uploads, ApprovalError, Permission};
use crate::files_api::json;
use crate::server::{peer_ip, PeerAddr};
use crate::tls;

const MAX_REQUEST_BYTES: usize = 1024;

#[derive(Deserialize, Default)]
struct InviteRequest {
    #[serde(default)]
    permission: Permission,
}

impl GuestApi {
    pub(super) async fn host(&self, parts: &Parts, method: Method, rest: &[&str], body: &mut Option<Incoming>, peer: PeerAddr) -> Outcome {
        // Like pairing and revoking nodes, these are for the machine itself: a guest must not make guests.
        if !peer_ip(Some(peer)).is_loopback() {
            return Err(refuse(StatusCode::FORBIDDEN, "Guests are managed on the machine running LADEX, at http://localhost"));
        }
        if method != Method::GET {
            require_same_origin(&parts.headers)?;
        }
        match (method, rest) {
            (Method::POST, ["invites"]) => self.issue_invite(body).await,
            (Method::GET, ["sessions"]) => Ok(json(StatusCode::OK, &serde_json::json!({ "guests": self.state.guests.sessions.list() }))),
            (Method::DELETE, ["sessions", id]) => self.sign_out(id),
            (Method::GET, ["uploads"]) => Ok(json(StatusCode::OK, &serde_json::json!({ "uploads": waiting_uploads(&self.state) }))),
            (Method::POST, ["uploads", id, "approve"]) => self.approve(id).await,
            (Method::POST, ["uploads", id, "deny"]) => self.deny(id),
            _ => Err(refuse(StatusCode::NOT_FOUND, "not found")),
        }
    }

    async fn issue_invite(&self, body: &mut Option<Incoming>) -> Outcome {
        let bytes = match body.take() {
            Some(body) => Limited::new(body, MAX_REQUEST_BYTES)
                .collect()
                .await
                .map_err(|_| refuse(StatusCode::PAYLOAD_TOO_LARGE, "request too large"))?
                .to_bytes(),
            None => Default::default(),
        };
        let request: InviteRequest = match bytes.is_empty() {
            true => InviteRequest::default(),
            false => serde_json::from_slice(&bytes).map_err(|_| refuse(StatusCode::BAD_REQUEST, "invalid request"))?,
        };

        let state = &self.state;
        let invite = state.guests.invites.issue(request.permission);
        let is_encrypted = !state.tls_fingerprint.is_empty();
        let url = invite_link(state, &invite);
        Ok(json(
            StatusCode::CREATED,
            &serde_json::json!({
                "url": url,
                "code": display_code(&invite.code),
                "permission": invite.permission,
                "expires_in_secs": invite.expires_in.as_secs(),
                "certificate_fingerprint": is_encrypted.then(|| tls::format_fingerprint(&state.tls_fingerprint)),
            }),
        ))
    }

    fn sign_out(&self, id: &str) -> Outcome {
        match revoke_guest(&self.state, id) {
            true => {
                tracing::info!("Guest: {id} signed out by the host");
                Ok(json(StatusCode::OK, &serde_json::json!({ "success": true })))
            }
            false => Err(refuse(StatusCode::NOT_FOUND, "no such guest")),
        }
    }

    async fn approve(&self, id: &str) -> Outcome {
        match approve_upload(&self.state, id).await {
            Ok(file) => Ok(json(StatusCode::OK, &serde_json::json!({ "success": true, "file": file }))),
            Err(ApprovalError::NotWaiting) => Err(refuse(StatusCode::NOT_FOUND, "no such upload is waiting")),
            Err(e) => Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
        }
    }

    fn deny(&self, id: &str) -> Outcome {
        match deny_upload(&self.state, id) {
            true => Ok(json(StatusCode::OK, &serde_json::json!({ "success": true }))),
            false => Err(refuse(StatusCode::NOT_FOUND, "no such upload is waiting")),
        }
    }
}
