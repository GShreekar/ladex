//! Browser login sessions, one per device; only each token's SHA-256 is kept.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;
use ring::digest;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub const SESSION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_SESSIONS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHandle {
    pub id: String,
    pub expires_at: Instant,
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub label: String,
    pub ip: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub expires_in_secs: u64,
}

/// A session as saved to disk, with a wall-clock expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    token_hash: String,
    id: String,
    label: String,
    ip: IpAddr,
    created_at: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

struct Record {
    id: String,
    label: String,
    ip: IpAddr,
    created_at: chrono::DateTime<chrono::Utc>,
    last_seen: chrono::DateTime<chrono::Utc>,
    expires_at: Instant,
}

pub struct SessionStore {
    lifetime: Duration,
    by_token_hash: Mutex<HashMap<Vec<u8>, Record>>,
    ended: broadcast::Sender<String>,
}

fn token_hash(token: &str) -> Vec<u8> {
    digest::digest(&digest::SHA256, token.as_bytes()).as_ref().to_vec()
}

fn random_hex(len_bytes: usize) -> String {
    let bytes: Vec<u8> = (0..len_bytes).map(|_| rand::thread_rng().gen()).collect();
    hex::encode(bytes)
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self::with_lifetime(SESSION_LIFETIME)
    }

    fn with_lifetime(lifetime: Duration) -> Self {
        Self { lifetime, by_token_hash: Mutex::new(HashMap::new()), ended: broadcast::channel(64).0 }
    }

    /// Returns the cookie token (shown to the browser once, never stored) and the session.
    pub fn create(&self, ip: IpAddr, user_agent: Option<&str>) -> (String, SessionHandle) {
        let token = random_hex(32);
        let now = chrono::Utc::now();
        let record = Record {
            id: random_hex(8),
            label: device_label(user_agent.unwrap_or("")),
            ip,
            created_at: now,
            last_seen: now,
            expires_at: Instant::now() + self.lifetime,
        };
        let handle = SessionHandle { id: record.id.clone(), expires_at: record.expires_at };

        let mut sessions = self.by_token_hash.lock().unwrap();
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions.iter().min_by_key(|(_, r)| r.created_at).map(|(hash, _)| hash.clone());
            if let Some(record) = oldest.and_then(|hash| sessions.remove(&hash)) {
                let _ = self.ended.send(record.id);
            }
        }
        sessions.insert(token_hash(&token), record);
        (token, handle)
    }

    /// The session this cookie token belongs to, if it is still valid.
    pub fn authenticate(&self, token: &str) -> Option<SessionHandle> {
        let hash = token_hash(token);
        let mut sessions = self.by_token_hash.lock().unwrap();
        let record = sessions.get_mut(&hash)?;
        if record.expires_at <= Instant::now() {
            let record = sessions.remove(&hash)?;
            let _ = self.ended.send(record.id);
            return None;
        }
        record.last_seen = chrono::Utc::now();
        Some(SessionHandle { id: record.id.clone(), expires_at: record.expires_at })
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

    pub fn list(&self) -> Vec<SessionSummary> {
        let now = Instant::now();
        let sessions = self.by_token_hash.lock().unwrap();
        let mut list: Vec<SessionSummary> = sessions
            .values()
            .filter(|r| r.expires_at > now)
            .map(|r| SessionSummary {
                id: r.id.clone(),
                label: r.label.clone(),
                ip: r.ip.to_string(),
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

    pub fn is_active(&self, id: &str) -> bool {
        let now = Instant::now();
        self.by_token_hash.lock().unwrap().values().any(|r| r.id == id && r.expires_at > now)
    }

    pub fn export(&self) -> Vec<SavedSession> {
        let now = Instant::now();
        let wall_now = chrono::Utc::now();
        let sessions = self.by_token_hash.lock().unwrap();
        sessions
            .iter()
            .filter(|(_, r)| r.expires_at > now)
            .map(|(hash, r)| SavedSession {
                token_hash: hex::encode(hash),
                id: r.id.clone(),
                label: r.label.clone(),
                ip: r.ip,
                created_at: r.created_at,
                last_seen: r.last_seen,
                expires_at: wall_now + chrono::Duration::from_std(r.expires_at - now).unwrap_or_default(),
            })
            .collect()
    }

    /// Restores saved sessions, dropping those that expired while the node was off.
    pub fn import(&self, saved: Vec<SavedSession>) {
        let now = Instant::now();
        let wall_now = chrono::Utc::now();
        let mut sessions = self.by_token_hash.lock().unwrap();
        for s in saved.into_iter().take(MAX_SESSIONS) {
            let (Ok(hash), Ok(left)) = (hex::decode(&s.token_hash), (s.expires_at - wall_now).to_std()) else {
                continue;
            };
            let left = left.min(self.lifetime);
            sessions.insert(
                hash,
                Record { id: s.id, label: s.label, ip: s.ip, created_at: s.created_at, last_seen: s.last_seen, expires_at: now + left },
            );
        }
    }

    pub fn subscribe_ended(&self) -> broadcast::Receiver<String> {
        self.ended.subscribe()
    }
}

/// "Chrome on Android" from a User-Agent string, to tell devices apart in a list.
pub fn device_label(user_agent: &str) -> String {
    let os = [
        ("Android", "Android"),
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Windows", "Windows"),
        ("Macintosh", "macOS"),
        ("CrOS", "ChromeOS"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(needle, _)| user_agent.contains(needle))
    .map(|(_, name)| *name);
    // Order matters: Edge and Opera also say "Chrome", Chrome also says "Safari".
    let browser =
        [("Edg/", "Edge"), ("OPR/", "Opera"), ("Firefox/", "Firefox"), ("Chrome/", "Chrome"), ("CriOS/", "Chrome"), ("Safari/", "Safari")]
            .iter()
            .find(|(needle, _)| user_agent.contains(needle))
            .map(|(_, name)| *name);
    match (browser, os) {
        (Some(browser), Some(os)) => format!("{browser} on {os}"),
        (Some(only), None) | (None, Some(only)) => only.to_string(),
        (None, None) => "Unknown device".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 20));

    #[test]
    fn sessions_survive_a_save_and_load_but_expired_ones_do_not() {
        let old = SessionStore::new();
        let (token, handle) = old.create(IP, Some("Mozilla/5.0 (Linux; Android 14) Chrome/120"));
        let saved = old.export();

        let fresh = SessionStore::new();
        fresh.import(saved.clone());
        assert_eq!(fresh.authenticate(&token).map(|h| h.id), Some(handle.id));
        assert_eq!(fresh.list()[0].label, "Chrome on Android");

        let mut expired = saved;
        expired[0].expires_at = chrono::Utc::now() - chrono::Duration::seconds(5);
        let other = SessionStore::new();
        other.import(expired);
        assert!(other.authenticate(&token).is_none());
    }

    #[test]
    fn a_created_session_authenticates_and_others_do_not() {
        let store = SessionStore::new();
        let (token, handle) = store.create(IP, Some("Mozilla/5.0 Firefox/130.0 Linux"));
        assert_eq!(store.authenticate(&token), Some(handle));
        assert_eq!(store.authenticate("not-a-token"), None);
        assert_eq!(store.authenticate(""), None);
    }

    #[test]
    fn every_login_gets_its_own_token_and_id() {
        let store = SessionStore::new();
        let (token_a, a) = store.create(IP, None);
        let (token_b, b) = store.create(IP, None);
        assert_ne!(token_a, token_b);
        assert_ne!(a.id, b.id);
        assert_eq!(token_a.len(), 64);
    }

    #[test]
    fn revoking_one_session_leaves_the_others_alone() {
        let store = SessionStore::new();
        let (token_a, a) = store.create(IP, None);
        let (token_b, _) = store.create(IP, None);
        assert!(store.revoke(&a.id));
        assert_eq!(store.authenticate(&token_a), None);
        assert!(store.authenticate(&token_b).is_some());
        assert!(!store.revoke(&a.id));
    }

    #[test]
    fn revoking_announces_the_session_id_so_its_websocket_can_close() {
        let store = SessionStore::new();
        let mut ended = store.subscribe_ended();
        let (_, handle) = store.create(IP, None);
        store.revoke(&handle.id);
        assert_eq!(ended.try_recv().unwrap(), handle.id);
    }

    #[test]
    fn expired_sessions_stop_authenticating_and_are_announced() {
        let store = SessionStore::with_lifetime(Duration::ZERO);
        let mut ended = store.subscribe_ended();
        let (token, handle) = store.create(IP, None);
        assert_eq!(store.authenticate(&token), None);
        assert_eq!(ended.try_recv().unwrap(), handle.id);
        assert!(store.list().is_empty());
        assert!(!store.is_active(&handle.id));
    }

    #[test]
    fn purge_removes_only_expired_sessions() {
        let store = SessionStore::with_lifetime(Duration::ZERO);
        store.create(IP, None);
        store.purge_expired();
        assert!(store.by_token_hash.lock().unwrap().is_empty());

        let store = SessionStore::new();
        let (token, _) = store.create(IP, None);
        store.purge_expired();
        assert!(store.authenticate(&token).is_some());
    }

    #[test]
    fn the_token_itself_is_never_stored() {
        let store = SessionStore::new();
        let (token, _) = store.create(IP, None);
        {
            let sessions = store.by_token_hash.lock().unwrap();
            assert!(sessions.keys().all(|k| k != token.as_bytes() && k.len() == 32));
        }
        assert!(!format!("{:?}", store.list()).contains(&token));
    }

    #[test]
    fn the_oldest_session_is_evicted_at_the_cap() {
        let store = SessionStore::new();
        let (first_token, first) = store.create(IP, None);
        let mut ended = store.subscribe_ended();
        for _ in 0..MAX_SESSIONS {
            store.create(IP, None);
        }
        assert_eq!(store.authenticate(&first_token), None);
        assert_eq!(ended.try_recv().unwrap(), first.id);
        assert_eq!(store.list().len(), MAX_SESSIONS);
    }

    #[test]
    fn device_labels() {
        let chrome_android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/126.0 Mobile Safari/537.36";
        assert_eq!(device_label(chrome_android), "Chrome on Android");
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/126.0 Safari/537.36 Edg/126.0";
        assert_eq!(device_label(edge), "Edge on Windows");
        let safari = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) Version/17.5 Mobile Safari/604.1";
        assert_eq!(device_label(safari), "Safari on iPhone");
        assert_eq!(device_label(""), "Unknown device");
    }
}
