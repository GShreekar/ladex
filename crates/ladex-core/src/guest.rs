//! Browser guests: devices that join through a one-time invite, each with its own session and permission.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::Refusal;
use crate::catalog::{CatalogEntry, EntryKind};
use crate::state;
use crate::store::Blob;
use crate::types::FileView;
use crate::NodeState;

pub mod api;
mod host;
pub mod invites;
pub mod sessions;
mod tus;
pub mod uploads;

pub const DEFAULT_SESSION_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);

/// What a guest may do on the node it joined.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    #[default]
    ReadOnly,
    Upload,
    UploadWithApproval,
}

impl std::str::FromStr for Permission {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "read-only" => Ok(Permission::ReadOnly),
            "upload" => Ok(Permission::Upload),
            "upload-with-approval" => Ok(Permission::UploadWithApproval),
            other => Err(format!("{other:?} is not read-only, upload or upload-with-approval")),
        }
    }
}

impl Permission {
    pub fn can_upload(self) -> bool {
        self != Permission::ReadOnly
    }

    pub fn needs_approval(self) -> bool {
        self == Permission::UploadWithApproval
    }
}

/// The browser guests of one node.
pub struct Guests {
    pub invites: invites::Invites,
    pub sessions: sessions::GuestSessions,
    pub uploads: uploads::GuestUploads,
    /// Throttles wrong invite codes per address; a code is short enough to be worth guessing.
    pub join_limiter: crate::ratelimit::AttemptLimiter,
}

impl Guests {
    pub fn new(session_lifetime: Duration) -> Self {
        let policy = crate::ratelimit::Policy {
            free_attempts: 5,
            base_lockout: Duration::from_secs(60),
            max_lockout: Duration::from_secs(60 * 60),
            global_cap: Some((30, Duration::from_secs(10 * 60))),
        };
        Self {
            invites: invites::Invites::new(),
            sessions: sessions::GuestSessions::new(session_lifetime),
            uploads: uploads::GuestUploads::default(),
            join_limiter: crate::ratelimit::AttemptLimiter::new(policy),
        }
    }
}

/// The address a guest opens, with the invitation token and the certificate fingerprint in the fragment.
/// A browser never sends the fragment to the server or in a Referer, so it stays between the QR code and the page.
pub fn invite_link(state: &NodeState, invite: &invites::IssuedInvite) -> Option<String> {
    let ip = state.local_ip?;
    let is_encrypted = !state.tls_fingerprint.is_empty();
    let scheme = if is_encrypted { "https" } else { "http" };
    let mut fragment = format!("t={}", invite.token);
    if is_encrypted {
        fragment.push_str(&format!("&fp={}", crate::tls::format_fingerprint(&state.tls_fingerprint)));
    }
    Some(format!("{scheme}://{ip}:{}/join#{fragment}", state.http_port))
}

/// Ends a guest's session and throws away what they uploaded but nobody has accepted.
pub fn revoke_guest(state: &NodeState, guest_id: &str) -> bool {
    let was_active = state.guests.sessions.revoke(guest_id);
    discard_uploads_of(state, guest_id);
    was_active
}

fn discard_uploads_of(state: &NodeState, guest_id: &str) {
    for upload in state.guests.uploads.remove_of_guest(guest_id) {
        if upload.state != uploads::UploadState::Shared {
            state.store.remove(&upload.id);
        }
    }
}

/// Lists a finished guest upload in the catalog, as shared by that guest.
pub async fn share_upload(state: &NodeState, upload: &uploads::Upload, blob: &Blob) -> Result<FileView, ShareError> {
    let manifest_root = blob.manifest_root().ok_or(ShareError::Incomplete)?;
    let entry = CatalogEntry {
        file_id: upload.id.clone(),
        name: upload.name.clone(),
        size: upload.size,
        mime_type: upload.mime_type.clone(),
        created_at_ms: crate::hlc::wall_clock_ms(),
        manifest_root,
        uploader_id: upload.guest_id.clone(),
        kind: EntryKind::File { parent: None },
    };
    state::share_file(state, entry).await.map_err(ShareError::Refused)
}

/// A guest's upload waiting for the person at the node's machine to accept it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingUpload {
    pub id: String,
    pub guest_id: String,
    pub guest: Option<String>,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    pub waiting_secs: u64,
}

/// The guests' uploads waiting for approval, oldest first.
pub fn waiting_uploads(state: &NodeState) -> Vec<WaitingUpload> {
    let uploads = state.guests.uploads.awaiting_approval().into_iter();
    uploads
        .map(|upload| WaitingUpload {
            guest: state.guests.sessions.label_of(&upload.guest_id),
            waiting_secs: upload.age().as_secs(),
            id: upload.id,
            guest_id: upload.guest_id,
            name: upload.name,
            size: upload.size,
            mime_type: upload.mime_type,
        })
        .collect()
}

/// Accepts a waiting upload and lists it as shared by its guest.
pub async fn approve_upload(state: &NodeState, id: &str) -> Result<FileView, ApprovalError> {
    // Moved first, so two approvals at once cannot both list the file.
    let upload = state
        .guests
        .uploads
        .transition(id, uploads::UploadState::AwaitingApproval, uploads::UploadState::Shared)
        .ok_or(ApprovalError::NotWaiting)?;
    let Some(blob) = state.store.get(id) else {
        state.guests.uploads.remove(id);
        return Err(ApprovalError::NotWaiting);
    };
    match share_upload(state, &upload, &blob).await {
        Ok(file) => {
            tracing::info!("Guest: {} from {} accepted by the host", file.name, upload.guest_id);
            Ok(file)
        }
        Err(e) => {
            state.guests.uploads.transition(id, uploads::UploadState::Shared, uploads::UploadState::AwaitingApproval);
            Err(ApprovalError::Share(e))
        }
    }
}

/// Throws away a waiting upload; false if no such upload is waiting.
pub fn deny_upload(state: &NodeState, id: &str) -> bool {
    let waiting = state.guests.uploads.get(id).filter(|upload| upload.state == uploads::UploadState::AwaitingApproval);
    let Some(upload) = waiting else { return false };
    state.guests.uploads.remove(&upload.id);
    state.store.remove(&upload.id);
    tracing::info!("Guest: {} from {} declined by the host", upload.name, upload.guest_id);
    true
}

#[derive(Debug)]
pub enum ApprovalError {
    NotWaiting,
    Share(ShareError),
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalError::NotWaiting => f.write_str("no such upload is waiting"),
            ApprovalError::Share(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApprovalError {}

#[derive(Debug)]
pub enum ShareError {
    Incomplete,
    Refused(Refusal),
}

impl std::fmt::Display for ShareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShareError::Incomplete => f.write_str("the upload is not complete"),
            ShareError::Refused(refusal) => write!(f, "could not list the file: {refusal}"),
        }
    }
}

const AWAITING_APPROVAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Forgets ended sessions and uploads nobody is waiting for, and deletes the files they left behind.
pub fn collect_garbage(state: &NodeState) {
    state.guests.sessions.purge_expired();
    for upload in state.guests.uploads.all() {
        let guest_has_left = !state.guests.sessions.is_active(&upload.guest_id);
        let is_stale = match upload.state {
            uploads::UploadState::Receiving => guest_has_left || state.store.get(&upload.id).is_none(),
            uploads::UploadState::AwaitingApproval => upload.age() >= AWAITING_APPROVAL_TTL || state.store.get(&upload.id).is_none(),
            uploads::UploadState::Shared => guest_has_left,
        };
        if !is_stale {
            continue;
        }
        state.guests.uploads.remove(&upload.id);
        if upload.state != uploads::UploadState::Shared {
            state.store.remove(&upload.id);
        }
    }
    // After a restart the records are gone but a finished, unlisted upload is still on disk.
    for blob in state.store.all() {
        let is_leftover = uploads::is_guest_upload_id(blob.id())
            && !blob.is_being_written()
            && !state.guests.uploads.contains(blob.id())
            && !state.catalog.contains(blob.id());
        if is_leftover {
            state.store.remove(blob.id());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_at(ip: &str, fingerprint: &[u8]) -> NodeState {
        let mut state = NodeState::for_tests(None);
        state.local_ip = Some(ip.parse().unwrap());
        state.http_port = 8443;
        state.tls_fingerprint = fingerprint.to_vec();
        state
    }

    #[test]
    fn an_encrypted_nodes_link_carries_the_token_and_the_fingerprint_in_the_fragment() {
        let state = node_at("192.168.1.5", &[0xAB, 0xCD]);
        let invite = state.guests.invites.issue(Permission::Upload);
        let link = invite_link(&state, &invite).unwrap();
        assert_eq!(link, format!("https://192.168.1.5:8443/join#t={}&fp=AB:CD", invite.token));
    }

    #[test]
    fn a_plain_nodes_link_has_no_fingerprint() {
        let state = node_at("192.168.1.5", &[]);
        let invite = state.guests.invites.issue(Permission::ReadOnly);
        let link = invite_link(&state, &invite).unwrap();
        assert_eq!(link, format!("http://192.168.1.5:8443/join#t={}", invite.token));
    }

    #[test]
    fn there_is_no_link_without_an_address() {
        let mut state = node_at("192.168.1.5", &[]);
        state.local_ip = None;
        assert!(invite_link(&state, &state.guests.invites.issue(Permission::ReadOnly)).is_none());
    }

    #[test]
    fn a_permission_is_read_from_its_command_line_name() {
        assert_eq!("upload-with-approval".parse(), Ok(Permission::UploadWithApproval));
        assert!("everything".parse::<Permission>().is_err());
    }
}
