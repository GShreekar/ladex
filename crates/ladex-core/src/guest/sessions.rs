//! Guest sessions: one per joined device, kept in memory only, so a restart ends them all.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::Permission;
use crate::sessions::{device_label, random_hex, token_hash};

const MAX_GUESTS: usize = 256;

/// Who a request comes from, as far as the node needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSession {
    pub id: String,
    pub permission: Permission,
    pub label: String,
    pub expires_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestSummary {
    pub id: String,
    pub label: String,
    pub ip: String,
    pub permission: Permission,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub expires_in_secs: u64,
}

struct Record {
    id: String,
    label: String,
    ip: IpAddr,
    permission: Permission,
    created_at: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
    expires_at: Instant,
}

pub struct GuestSessions {
    lifetime: Duration,
    by_token_hash: Mutex<HashMap<Vec<u8>, Record>>,
    ended: broadcast::Sender<String>,
}

impl GuestSessions {
    pub fn new(lifetime: Duration) -> Self {
        Self { lifetime, by_token_hash: Mutex::new(HashMap::new()), ended: broadcast::channel(64).0 }
    }

    pub fn lifetime(&self) -> Duration {
        self.lifetime
    }

    /// Returns the cookie token (given to the browser once, never stored) and the session.
    pub fn create(&self, ip: IpAddr, user_agent: Option<&str>, permission: Permission) -> (String, GuestSession) {
        let token = random_hex(32);
        let now = chrono::Utc::now();
        let record = Record {
            id: random_hex(8),
            label: device_label(user_agent.unwrap_or("")),
            ip,
            permission,
            created_at: now,
            last_seen: now,
            expires_at: Instant::now() + self.lifetime,
        };
        let session = GuestSession { id: record.id.clone(), permission, label: record.label.clone(), expires_in: self.lifetime };

        let mut sessions = self.by_token_hash.lock().unwrap();
        if sessions.len() >= MAX_GUESTS {
            let oldest = sessions.iter().min_by_key(|(_, r)| r.created_at).map(|(hash, _)| hash.clone());
            if let Some(record) = oldest.and_then(|hash| sessions.remove(&hash)) {
                let _ = self.ended.send(record.id);
            }
        }
        sessions.insert(token_hash(&token), record);
        (token, session)
    }

    /// The session this cookie token belongs to, if it is still valid.
    pub fn authenticate(&self, token: &str) -> Option<GuestSession> {
        let hash = token_hash(token);
        let mut sessions = self.by_token_hash.lock().unwrap();
        let record = sessions.get_mut(&hash)?;
        let now = Instant::now();
        if record.expires_at <= now {
            let record = sessions.remove(&hash)?;
            let _ = self.ended.send(record.id);
            return None;
        }
        record.last_seen = chrono::Utc::now();
        Some(GuestSession {
            id: record.id.clone(),
            permission: record.permission,
            label: record.label.clone(),
            expires_in: record.expires_at - now,
        })
    }

    pub fn revoke(&self, id: &str) -> bool {
        let mut sessions = self.by_token_hash.lock().unwrap();
        let hash = sessions.iter().find(|(_, r)| r.id == id).map(|(hash, _)| hash.clone());
        let removed = hash.and_then(|hash| sessions.remove(&hash)).is_some();
        if removed {
            let _ = self.ended.send(id.to_string());
        }
        removed
    }

    pub fn is_active(&self, id: &str) -> bool {
        let now = Instant::now();
        self.by_token_hash.lock().unwrap().values().any(|r| r.id == id && r.expires_at > now)
    }

    pub fn label_of(&self, id: &str) -> Option<String> {
        self.by_token_hash.lock().unwrap().values().find(|r| r.id == id).map(|r| r.label.clone())
    }

    pub fn list(&self) -> Vec<GuestSummary> {
        let now = Instant::now();
        let sessions = self.by_token_hash.lock().unwrap();
        let mut list: Vec<GuestSummary> = sessions
            .values()
            .filter(|r| r.expires_at > now)
            .map(|r| GuestSummary {
                id: r.id.clone(),
                label: r.label.clone(),
                ip: r.ip.to_string(),
                permission: r.permission,
                created_at: r.created_at,
                last_seen: r.last_seen,
                expires_in_secs: (r.expires_at - now).as_secs(),
            })
            .collect();
        list.sort_by_key(|s| s.created_at);
        list
    }

    pub fn purge_expired(&self) {
        let now = Instant::now();
        let mut sessions = self.by_token_hash.lock().unwrap();
        let expired: Vec<Vec<u8>> = sessions.iter().filter(|(_, r)| r.expires_at <= now).map(|(h, _)| h.clone()).collect();
        for hash in expired {
            if let Some(record) = sessions.remove(&hash) {
                let _ = self.ended.send(record.id);
            }
        }
    }

    /// Ids of sessions as they end, for connections that stream to a guest and must stop.
    pub fn subscribe_ended(&self) -> broadcast::Receiver<String> {
        self.ended.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 30));

    #[test]
    fn a_session_carries_the_permission_it_was_invited_with() {
        let sessions = GuestSessions::new(Duration::from_secs(60));
        let (token, created) = sessions.create(IP, Some("Mozilla/5.0 (iPhone) Safari/604"), Permission::UploadWithApproval);
        let found = sessions.authenticate(&token).unwrap();
        assert_eq!((found.id, found.permission, found.label.as_str()), (created.id, Permission::UploadWithApproval, "Safari on iPhone"));
    }

    #[test]
    fn a_revoked_session_stops_working_and_is_announced() {
        let sessions = GuestSessions::new(Duration::from_secs(60));
        let mut ended = sessions.subscribe_ended();
        let (token, session) = sessions.create(IP, None, Permission::ReadOnly);
        assert!(sessions.revoke(&session.id));
        assert!(sessions.authenticate(&token).is_none());
        assert_eq!(ended.try_recv().unwrap(), session.id);
        assert!(!sessions.revoke(&session.id));
    }

    #[test]
    fn an_expired_session_is_refused() {
        let sessions = GuestSessions::new(Duration::ZERO);
        let (token, session) = sessions.create(IP, None, Permission::ReadOnly);
        assert!(sessions.authenticate(&token).is_none());
        assert!(!sessions.is_active(&session.id));
        assert!(sessions.list().is_empty());
    }

    #[test]
    fn the_cookie_token_is_not_kept_in_the_listing() {
        let sessions = GuestSessions::new(Duration::from_secs(60));
        let (token, _) = sessions.create(IP, None, Permission::Upload);
        let listing = serde_json::to_string(&sessions.list()).unwrap();
        assert!(!listing.contains(&token));
        assert!(listing.contains("\"permission\":\"upload\""));
    }
}
