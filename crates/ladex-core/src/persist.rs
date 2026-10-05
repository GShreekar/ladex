//! What a node remembers across restarts besides its files, saved atomically as one private JSON file.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::{digest, pbkdf2};
use serde::{Deserialize, Serialize};

use crate::mesh;
use crate::sessions::SavedSession;
use crate::state::{self, MAX_CATALOG_ENTRIES};
use crate::types::{FileMetadata, NodeId, TextMessage};
use crate::{hlc, validate, NodeState};

const FILE_NAME: &str = "node.state";
const VERSION: u32 = 1;
const SAVE_EVERY: Duration = Duration::from_secs(10);
const PEER_MEMORY_MS: u64 = 30 * 24 * 60 * 60 * 1000;
const MAX_KNOWN_PEERS: usize = 32;
const PBKDF2_ROUNDS: u32 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPeer {
    pub ip: IpAddr,
    pub port: u16,
    pub last_seen_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    v: u32,
    pub node_id: NodeId,
    clock: (u64, u32),
    // Proves the sessions below were issued under the current passphrase.
    login: Option<LoginTag>,
    messages: Vec<TextMessage>,
    tombstones: Vec<FileMetadata>,
    sessions: Vec<SavedSession>,
    pub peers: Vec<KnownPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoginTag {
    salt: String,
    hash: String,
}

fn tag_for(passphrase: &str, salt: &[u8]) -> Vec<u8> {
    let mut out = [0u8; digest::SHA256_OUTPUT_LEN];
    let rounds = std::num::NonZeroU32::new(PBKDF2_ROUNDS).unwrap();
    pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, rounds, salt, passphrase.as_bytes(), &mut out);
    out.to_vec()
}

fn new_login_tag(passphrase: &str) -> LoginTag {
    let salt: [u8; 16] = rand::random();
    LoginTag { salt: hex::encode(salt), hash: hex::encode(tag_for(passphrase, &salt)) }
}

fn login_tag_matches(tag: &LoginTag, passphrase: &str) -> bool {
    let (Ok(salt), Ok(hash)) = (hex::decode(&tag.salt), hex::decode(&tag.hash)) else {
        return false;
    };
    let rounds = std::num::NonZeroU32::new(PBKDF2_ROUNDS).unwrap();
    pbkdf2::verify(pbkdf2::PBKDF2_HMAC_SHA256, rounds, &salt, passphrase.as_bytes(), &hash).is_ok()
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// The saved state, or None on a first run; a damaged file is set aside and the node starts fresh.
pub fn load(dir: &Path) -> Option<Saved> {
    let path = path_in(dir);
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Saved>(&bytes) {
        Ok(saved) if saved.v == VERSION && validate::is_valid_id(&saved.node_id) => Some(saved),
        _ => {
            let aside = path.with_extension("state.bad");
            tracing::warn!("Persist: {} is unreadable; moved to {} and starting fresh", path.display(), aside.display());
            let _ = std::fs::rename(&path, aside);
            None
        }
    }
}

/// Put a loaded snapshot back into a node that has just started.
pub async fn restore(state: &NodeState, saved: &Saved) {
    state.clock.resume_from(saved.clock.0, saved.clock.1);

    {
        let mut messages = state.messages.write().await;
        state::merge_messages(&mut messages, saved.messages.iter().cloned().filter_map(validate::incoming_message).collect());
    }
    {
        let tombstones = saved.tombstones.iter().filter(|f| f.deleted).cloned().filter_map(validate::incoming_file).collect();
        let mut files = state.files.write().await;
        state::merge_files(&mut files, tombstones, MAX_CATALOG_ENTRIES);
    }

    let same_login = match (&state.passphrase, &saved.login) {
        (Some(passphrase), Some(tag)) => login_tag_matches(tag, passphrase),
        _ => false,
    };
    if same_login {
        state.sessions.import(saved.sessions.clone());
    } else if !saved.sessions.is_empty() {
        tracing::info!("Persist: the passphrase changed, so saved logins were dropped");
    }
}

/// Dial the nodes this one was connected to last time (discovery may not reach them).
pub fn redial(state: &NodeState, peers: Vec<KnownPeer>) {
    for peer in peers {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = mesh::connect_to_peer(peer.ip, peer.port, state).await {
                tracing::debug!("Persist: remembered peer {}:{} not reachable: {e}", peer.ip, peer.port);
            }
        });
    }
}

struct Saver {
    path: PathBuf,
    login: Option<LoginTag>,
    peers: Vec<KnownPeer>,
    last_written: Vec<u8>,
}

impl Saver {
    async fn snapshot(&mut self, state: &NodeState) -> Saved {
        let now = hlc::wall_clock_ms();
        let live: Vec<KnownPeer> =
            state.mesh_peers.read().await.values().map(|p| KnownPeer { ip: p.addr.ip(), port: p.http_port, last_seen_ms: now }).collect();
        self.peers.retain(|old| {
            now.saturating_sub(old.last_seen_ms) < PEER_MEMORY_MS && !live.iter().any(|p| p.ip == old.ip && p.port == old.port)
        });
        self.peers.extend(live);
        self.peers.sort_by_key(|p| std::cmp::Reverse(p.last_seen_ms));
        self.peers.truncate(MAX_KNOWN_PEERS);

        Saved {
            v: VERSION,
            node_id: state.node_id.clone(),
            clock: state.clock.latest(),
            login: self.login.clone(),
            messages: state.messages.read().await.clone(),
            tombstones: state.files.read().await.values().filter(|f| f.deleted).cloned().collect(),
            sessions: state.sessions.export(),
            peers: self.peers.clone(),
        }
    }

    async fn save(&mut self, state: &NodeState) {
        let mut snapshot = self.snapshot(state).await;
        let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
        snapshot.clock = (0, 0);
        snapshot.peers.iter_mut().for_each(|p| p.last_seen_ms = 0);
        let fingerprint = serde_json::to_vec(&snapshot).unwrap_or_default();
        if fingerprint == self.last_written {
            return;
        }
        let path = self.path.clone();
        match tokio::task::spawn_blocking(move || write_private(&path, &bytes)).await {
            Ok(Ok(())) => self.last_written = fingerprint,
            Ok(Err(e)) => tracing::warn!("Persist: could not save node state: {e}"),
            Err(e) => tracing::warn!("Persist: save task failed: {e}"),
        }
    }
}

/// Keeps the saved file current until the process ends. Returns a handle for a final save on shutdown.
pub fn spawn_saver(state: NodeState, dir: &Path, saved: Option<&Saved>) -> Flusher {
    let login = match (&state.passphrase, saved.and_then(|s| s.login.clone())) {
        (Some(passphrase), Some(tag)) if login_tag_matches(&tag, passphrase) => Some(tag),
        (Some(passphrase), _) => Some(new_login_tag(passphrase)),
        (None, _) => None,
    };
    let saver = Saver { path: path_in(dir), login, peers: saved.map(|s| s.peers.clone()).unwrap_or_default(), last_written: Vec::new() };
    let saver = std::sync::Arc::new(tokio::sync::Mutex::new(saver));
    let flusher = Flusher { state: state.clone(), saver: saver.clone() };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAVE_EVERY);
        loop {
            tick.tick().await;
            saver.lock().await.save(&state).await;
        }
    });
    flusher
}

pub struct Flusher {
    state: NodeState,
    saver: std::sync::Arc<tokio::sync::Mutex<Saver>>,
}

impl Flusher {
    pub async fn flush(&self) {
        self.saver.lock().await.save(&self.state).await;
    }
}

/// Writes atomically, readable only by this user.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlc::Stamp;
    use crate::types::Holder;

    fn tombstone(id: &str) -> FileMetadata {
        let version = Stamp { wall: hlc::wall_clock_ms(), counter: 0, node: "node_a".into() };
        FileMetadata {
            id: id.into(),
            name: "gone.txt".into(),
            size: 1,
            mime_type: "text/plain".into(),
            uploader_id: "peer_a".into(),
            uploader_node: "node_a".into(),
            holders: [("node_a".to_string(), Holder { since: version.clone(), present: false })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: version.wall,
            deleted_at: version.wall,
            version,
            deleted: true,
            manifest_root: Some("ab".repeat(32)),
            is_folder: false,
            parent: None,
            folder_bytes: 0,
            folder_files: 0,
        }
    }

    fn chat(id: &str) -> TextMessage {
        TextMessage {
            id: id.into(),
            content: "hello".into(),
            sender_id: "peer_a".into(),
            sender_name: Some("A".into()),
            timestamp: chrono::Utc::now(),
            created_at: hlc::wall_clock_ms(),
        }
    }

    async fn saved_by(state: &NodeState, dir: &Path) -> Saved {
        let flusher = spawn_saver(state.clone(), dir, None);
        flusher.flush().await;
        load(dir).expect("state was saved")
    }

    #[tokio::test]
    async fn chat_unshares_and_logins_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let before = NodeState::for_tests_node("node_before", Some("secret"));
        before.messages.write().await.push(chat("m1"));
        before.files.write().await.insert("f1".into(), tombstone("f1"));
        let (token, _) = before.sessions.create("10.0.0.2".parse().unwrap(), None);

        let saved = saved_by(&before, dir.path()).await;
        assert_eq!(saved.node_id, "node_before");

        let after = NodeState::for_tests_node("node_before", Some("secret"));
        restore(&after, &saved).await;
        assert_eq!(after.messages.read().await.len(), 1);
        assert!(after.files.read().await["f1"].deleted);
        assert!(after.sessions.authenticate(&token).is_some());
    }

    #[tokio::test]
    async fn a_changed_passphrase_drops_saved_logins_but_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let before = NodeState::for_tests_node("node_before", Some("old"));
        before.messages.write().await.push(chat("m1"));
        let (token, _) = before.sessions.create("10.0.0.2".parse().unwrap(), None);
        let saved = saved_by(&before, dir.path()).await;

        let after = NodeState::for_tests_node("node_before", Some("new"));
        restore(&after, &saved).await;
        assert!(after.sessions.authenticate(&token).is_none());
        assert_eq!(after.messages.read().await.len(), 1);
    }

    #[tokio::test]
    async fn a_node_without_a_passphrase_saves_no_logins() {
        let dir = tempfile::tempdir().unwrap();
        let open = NodeState::for_tests_node("node_open", None);
        open.sessions.create("10.0.0.2".parse().unwrap(), None);
        let saved = saved_by(&open, dir.path()).await;

        let again = NodeState::for_tests_node("node_open", Some("now-protected"));
        restore(&again, &saved).await;
        assert!(again.sessions.list().is_empty());
    }

    #[tokio::test]
    async fn remembered_peers_are_kept_and_old_ones_age_out() {
        let dir = tempfile::tempdir().unwrap();
        let state = NodeState::for_tests_node("node_a", None);
        let stale = KnownPeer { ip: "10.0.0.9".parse().unwrap(), port: 8080, last_seen_ms: 1 };
        let recent = KnownPeer { ip: "10.0.0.8".parse().unwrap(), port: 8080, last_seen_ms: hlc::wall_clock_ms() };
        let seed = Saved { peers: vec![stale, recent.clone()], ..Default::default() };

        let flusher = spawn_saver(state, dir.path(), Some(&seed));
        flusher.flush().await;
        assert_eq!(load(dir.path()).unwrap().peers, vec![recent]);
    }

    #[test]
    fn a_damaged_file_is_set_aside_and_the_node_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path_in(dir.path()), b"{not json").unwrap();
        assert!(load(dir.path()).is_none());
        assert!(!path_in(dir.path()).exists());
        assert!(dir.path().join("node.state.bad").exists());
    }

    #[test]
    fn a_first_run_has_nothing_to_load() {
        assert!(load(tempfile::tempdir().unwrap().path()).is_none());
    }

    #[tokio::test]
    async fn the_saved_file_is_private_and_unchanged_state_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let state = NodeState::for_tests_node("node_a", None);
        let flusher = spawn_saver(state, dir.path(), None);
        flusher.flush().await;
        let path = path_in(dir.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, b"sentinel").unwrap();
        flusher.flush().await;
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
    }
}
