//! What guests are uploading: who owns each upload, and whether it is still arriving, waiting for the host, or shared.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

// Ids of guest uploads start with this, so leftovers can be told apart from every other file in the store.
const UPLOAD_ID_PREFIX: &str = "guest-";
const MAX_RECEIVING_PER_GUEST: usize = 4;
const MAX_AWAITING_PER_GUEST: usize = 16;
const MAX_AWAITING: usize = 64;

pub fn new_upload_id() -> String {
    format!("{UPLOAD_ID_PREFIX}{}", crate::sessions::random_hex(16))
}

pub fn is_guest_upload_id(id: &str) -> bool {
    id.starts_with(UPLOAD_ID_PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UploadState {
    Receiving,
    AwaitingApproval,
    Shared,
}

impl UploadState {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadState::Receiving => "receiving",
            UploadState::AwaitingApproval => "awaiting-approval",
            UploadState::Shared => "shared",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Upload {
    pub id: String,
    pub guest_id: String,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub state: UploadState,
    started_at: Instant,
}

impl Upload {
    pub fn age(&self) -> Duration {
        self.started_at.elapsed()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    TooManyInProgress,
    TooManyWaiting,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::TooManyInProgress => f.write_str("finish or cancel your other uploads first"),
            Refused::TooManyWaiting => f.write_str("too many uploads are waiting for the host to accept them"),
        }
    }
}

/// The uploads of every guest, in memory: a restart ends the guests they belong to.
#[derive(Default)]
pub struct GuestUploads {
    by_id: Mutex<HashMap<String, Upload>>,
}

impl GuestUploads {
    /// Records a new upload for `guest_id`, unless that guest already has as many as they are allowed.
    pub fn begin(&self, id: &str, guest_id: &str, name: String, mime_type: String, size: u64) -> Result<(), Refused> {
        let mut by_id = self.by_id.lock().unwrap();
        let count = |state: UploadState, guest: Option<&str>| {
            by_id.values().filter(|u| u.state == state && guest.is_none_or(|g| u.guest_id == g)).count()
        };
        if count(UploadState::Receiving, Some(guest_id)) >= MAX_RECEIVING_PER_GUEST {
            return Err(Refused::TooManyInProgress);
        }
        let is_waiting_full = count(UploadState::AwaitingApproval, Some(guest_id)) >= MAX_AWAITING_PER_GUEST
            || count(UploadState::AwaitingApproval, None) >= MAX_AWAITING;
        if is_waiting_full {
            return Err(Refused::TooManyWaiting);
        }
        let upload = Upload {
            id: id.to_string(),
            guest_id: guest_id.to_string(),
            name,
            mime_type,
            size,
            state: UploadState::Receiving,
            started_at: Instant::now(),
        };
        by_id.insert(id.to_string(), upload);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Upload> {
        self.by_id.lock().unwrap().get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.by_id.lock().unwrap().contains_key(id)
    }

    pub fn all(&self) -> Vec<Upload> {
        self.by_id.lock().unwrap().values().cloned().collect()
    }

    /// Moves an upload from `from` to `to`, and returns it; None if it is gone or was not in `from`.
    pub fn transition(&self, id: &str, from: UploadState, to: UploadState) -> Option<Upload> {
        let mut by_id = self.by_id.lock().unwrap();
        let upload = by_id.get_mut(id).filter(|upload| upload.state == from)?;
        upload.state = to;
        Some(upload.clone())
    }

    pub fn remove(&self, id: &str) -> Option<Upload> {
        self.by_id.lock().unwrap().remove(id)
    }

    pub fn remove_of_guest(&self, guest_id: &str) -> Vec<Upload> {
        let mut by_id = self.by_id.lock().unwrap();
        let ids: Vec<String> = by_id.values().filter(|u| u.guest_id == guest_id).map(|u| u.id.clone()).collect();
        ids.iter().filter_map(|id| by_id.remove(id)).collect()
    }

    /// The uploads waiting for the host, oldest first.
    pub fn awaiting_approval(&self) -> Vec<Upload> {
        let mut waiting: Vec<Upload> =
            self.by_id.lock().unwrap().values().filter(|u| u.state == UploadState::AwaitingApproval).cloned().collect();
        waiting.sort_by_key(|u| u.started_at);
        waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(uploads: &GuestUploads, id: &str, guest: &str) -> Result<(), Refused> {
        uploads.begin(id, guest, "a.txt".into(), "text/plain".into(), 10)
    }

    #[test]
    fn a_guest_can_only_have_a_few_uploads_arriving_at_once() {
        let uploads = GuestUploads::default();
        for n in 0..MAX_RECEIVING_PER_GUEST {
            begin(&uploads, &format!("guest-{n}"), "guest_a").unwrap();
        }
        assert_eq!(begin(&uploads, "guest-extra", "guest_a"), Err(Refused::TooManyInProgress));
        assert_eq!(begin(&uploads, "guest-other", "guest_b"), Ok(()));
    }

    #[test]
    fn finished_uploads_free_a_slot() {
        let uploads = GuestUploads::default();
        for n in 0..MAX_RECEIVING_PER_GUEST {
            begin(&uploads, &format!("guest-{n}"), "guest_a").unwrap();
        }
        uploads.transition("guest-0", UploadState::Receiving, UploadState::Shared);
        assert_eq!(begin(&uploads, "guest-new", "guest_a"), Ok(()));
    }

    #[test]
    fn uploads_waiting_for_approval_are_capped_per_guest() {
        let uploads = GuestUploads::default();
        for n in 0..MAX_AWAITING_PER_GUEST {
            begin(&uploads, &format!("guest-{n}"), "guest_a").unwrap();
            uploads.transition(&format!("guest-{n}"), UploadState::Receiving, UploadState::AwaitingApproval);
        }
        assert_eq!(begin(&uploads, "guest-extra", "guest_a"), Err(Refused::TooManyWaiting));
    }

    #[test]
    fn an_upload_only_moves_on_from_the_state_it_is_in() {
        let uploads = GuestUploads::default();
        begin(&uploads, "guest-1", "guest_a").unwrap();
        assert!(uploads.transition("guest-1", UploadState::AwaitingApproval, UploadState::Shared).is_none());
        assert!(uploads.transition("guest-1", UploadState::Receiving, UploadState::AwaitingApproval).is_some());
        assert!(uploads.transition("guest-1", UploadState::Receiving, UploadState::Shared).is_none());
    }

    #[test]
    fn removing_a_guest_removes_all_their_uploads() {
        let uploads = GuestUploads::default();
        begin(&uploads, "guest-1", "guest_a").unwrap();
        begin(&uploads, "guest-2", "guest_b").unwrap();
        let removed = uploads.remove_of_guest("guest_a");
        assert_eq!(removed.len(), 1);
        assert!(!uploads.contains("guest-1") && uploads.contains("guest-2"));
    }

    #[test]
    fn only_guest_upload_ids_are_recognised_as_guest_uploads() {
        assert!(is_guest_upload_id(&new_upload_id()));
        assert!(crate::validate::is_valid_id(&new_upload_id()));
        assert!(!is_guest_upload_id("0123abcd"));
    }
}
