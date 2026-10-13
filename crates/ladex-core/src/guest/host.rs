//! What the person at the machine running LADEX does about guests: invite them, see them, sign them out, accept their uploads.

use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use serde::{Deserialize, Serialize};

use super::api::{refuse, require_same_origin, GuestApi, Outcome};
use super::invites::display_code;
use super::uploads::{Upload, UploadState};
use super::{invite_link, revoke_guest, share_upload, Permission};
use crate::files_api::json;
use crate::server::{peer_ip, PeerAddr};
use crate::tls;

const MAX_REQUEST_BYTES: usize = 1024;

#[derive(Deserialize, Default)]
struct InviteRequest {
    #[serde(default)]
    permission: Permission,
}

#[derive(Serialize)]
struct WaitingUpload {
    id: String,
    guest_id: String,
    guest: Option<String>,
    name: String,
    size: u64,
    mime_type: String,
    waiting_secs: u64,
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
            (Method::GET, ["uploads"]) => Ok(self.waiting_uploads()),
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

    fn waiting_uploads(&self) -> hyper::Response<crate::server::ServerBody> {
        let waiting: Vec<WaitingUpload> = self
            .state
            .guests
            .uploads
            .awaiting_approval()
            .into_iter()
            .map(|upload| WaitingUpload {
                guest: self.state.guests.sessions.label_of(&upload.guest_id),
                waiting_secs: upload.age().as_secs(),
                id: upload.id,
                guest_id: upload.guest_id,
                name: upload.name,
                size: upload.size,
                mime_type: upload.mime_type,
            })
            .collect();
        json(StatusCode::OK, &serde_json::json!({ "uploads": waiting }))
    }

    async fn approve(&self, id: &str) -> Outcome {
        let state = &self.state;
        let not_found = || refuse(StatusCode::NOT_FOUND, "no such upload is waiting");
        // Moved first, so two approvals at once cannot both list the file.
        let upload: Upload =
            state.guests.uploads.transition(id, UploadState::AwaitingApproval, UploadState::Shared).ok_or_else(not_found)?;
        let Some(blob) = state.store.get(id) else {
            state.guests.uploads.remove(id);
            return Err(not_found());
        };
        match share_upload(state, &upload, &blob).await {
            Ok(file) => {
                tracing::info!("Guest: {} from {} accepted by the host", file.name, upload.guest_id);
                Ok(json(StatusCode::OK, &serde_json::json!({ "success": true, "file": file })))
            }
            Err(e) => {
                state.guests.uploads.transition(id, UploadState::Shared, UploadState::AwaitingApproval);
                Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
            }
        }
    }

    fn deny(&self, id: &str) -> Outcome {
        let state = &self.state;
        let waiting = state.guests.uploads.get(id).filter(|upload| upload.state == UploadState::AwaitingApproval);
        let Some(upload) = waiting else {
            return Err(refuse(StatusCode::NOT_FOUND, "no such upload is waiting"));
        };
        state.guests.uploads.remove(&upload.id);
        state.store.remove(&upload.id);
        tracing::info!("Guest: {} from {} declined by the host", upload.name, upload.guest_id);
        Ok(json(StatusCode::OK, &serde_json::json!({ "success": true })))
    }
}
