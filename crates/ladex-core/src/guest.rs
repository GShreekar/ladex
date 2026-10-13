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
