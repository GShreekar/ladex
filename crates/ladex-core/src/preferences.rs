//! What a person chooses about their node in the app or with `ladex settings`: kept in the data directory, so the app
//! and the daemon on one data directory behave the same.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::node::{DEFAULT_HTTP_PORT, DEFAULT_STORAGE_LIMIT_GIB};
use crate::{persist, NodeState};

const FILE_NAME: &str = "preferences.json";
const BYTES_PER_GIB: u64 = 1 << 30;
const BYTES_PER_MIB: u64 = 1 << 20;
const MAX_STORAGE_LIMIT_GIB: u64 = 1 << 20;

/// Whose files this device accepts without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoAcceptFrom {
    #[default]
    Nobody,
    /// Devices on nodes this one has paired with, and this node's own devices.
    #[serde(rename = "paired")]
    PairedDevices,
    /// Anyone in the mesh, which already shares a passphrase.
    Mesh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AutoAccept {
    pub from: AutoAcceptFrom,
    /// Larger files are asked about whoever sends them; None sets no limit.
    pub ask_above_mib: Option<u64>,
}

impl std::str::FromStr for AutoAcceptFrom {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "nobody" => Ok(Self::Nobody),
            "paired" => Ok(Self::PairedDevices),
            "mesh" => Ok(Self::Mesh),
            _ => Err("expected nobody, paired or mesh".to_string()),
        }
    }
}

impl AutoAccept {
    /// Whether a file of this size, from a sender of this standing, is accepted without asking.
    pub fn allows(&self, size_bytes: Option<u64>, is_sender_paired: bool) -> bool {
        let is_trusted = match self.from {
            AutoAcceptFrom::Nobody => false,
            AutoAcceptFrom::PairedDevices => is_sender_paired,
            AutoAcceptFrom::Mesh => true,
        };
        let is_small_enough = match (self.ask_above_mib, size_bytes) {
            (None, _) => true,
            (Some(limit), Some(size)) => size <= limit.saturating_mul(BYTES_PER_MIB),
            // A size nobody stated cannot be shown to be under the limit.
            (Some(_), None) => false,
        };
        is_trusted && is_small_enough
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Whether the first-run wizard has been completed.
    pub is_setup_done: bool,
    /// Where files this device saves go; None is the system's downloads folder.
    pub downloads_dir: Option<PathBuf>,
    pub storage_limit_gib: u64,
    pub auto_accept: AutoAccept,
    /// Other nodes this one keeps connected by address, for networks where discovery cannot find them.
    pub peer_addresses: Vec<SocketAddr>,
    /// On a phone: keep sharing after the app is closed, under a notification. Otherwise it shares while open, or transferring.
    pub keep_sharing_in_background: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            is_setup_done: false,
            downloads_dir: None,
            storage_limit_gib: DEFAULT_STORAGE_LIMIT_GIB,
            auto_accept: AutoAccept::default(),
            peer_addresses: Vec::new(),
            keep_sharing_in_background: false,
        }
    }
}

impl Preferences {
    pub fn storage_limit_bytes(&self) -> u64 {
        self.storage_limit_gib.saturating_mul(BYTES_PER_GIB)
    }
}

/// A change to some preferences; what is left out stays as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Update {
    pub is_setup_done: Option<bool>,
    /// An empty path goes back to the system's downloads folder.
    pub downloads_dir: Option<PathBuf>,
    pub storage_limit_gib: Option<u64>,
    pub auto_accept: Option<AutoAccept>,
    /// The whole list, as typed: `192.168.1.20:8080`, or just the IP for the default port.
    pub peer_addresses: Option<Vec<String>>,
    pub keep_sharing_in_background: Option<bool>,
}

#[derive(Debug)]
pub enum Invalid {
    StorageLimit,
    PeerAddress(String),
    StorageBelowUsed { used_bytes: u64 },
    DownloadsDir { path: PathBuf, reason: String },
    Save(std::io::Error),
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StorageLimit => write!(f, "the storage limit must be between 1 and {MAX_STORAGE_LIMIT_GIB} GiB"),
            Self::StorageBelowUsed { used_bytes } => {
                write!(
                    f,
                    "this node already holds {} MiB of shared files; remove some before lowering the limit that far",
                    used_bytes.div_ceil(BYTES_PER_MIB)
                )
            }
            Self::PeerAddress(text) => write!(f, "{text:?} is not a device address; type its IP, such as 192.168.1.20:{DEFAULT_HTTP_PORT}"),
            Self::DownloadsDir { path, reason } => write!(f, "cannot save files into {}: {reason}", path.display()),
            Self::Save(e) => write!(f, "could not save the preferences: {e}"),
        }
    }
}

impl std::error::Error for Invalid {}

/// The preferences of one data directory, kept in memory and on disk.
pub struct PreferencesStore {
    dir: PathBuf,
    current: RwLock<Preferences>,
    // Set once the passphrase is changed: the running node keeps using the old one until it restarts.
    needs_restart: AtomicBool,
}

impl PreferencesStore {
    /// Reads the saved preferences. A damaged file is set aside and the defaults are used, so a bad edit cannot stop the node.
    pub fn open(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf(), current: RwLock::new(read(dir)), needs_restart: AtomicBool::new(false) }
    }

    pub fn get(&self) -> Preferences {
        self.current.read().unwrap().clone()
    }

    pub fn needs_restart(&self) -> bool {
        self.needs_restart.load(Ordering::Relaxed)
    }

    pub fn mark_needs_restart(&self) {
        self.needs_restart.store(true, Ordering::Relaxed);
    }

    /// Applies a change: checks it, saves it, and returns the preferences as they now are.
    pub fn update(&self, update: &Update, used_bytes: u64) -> Result<Preferences, Invalid> {
        let mut next = self.get();
        if let Some(is_setup_done) = update.is_setup_done {
            next.is_setup_done = is_setup_done;
        }
        if let Some(path) = &update.downloads_dir {
            next.downloads_dir = checked_downloads_dir(path)?;
        }
        if let Some(gib) = update.storage_limit_gib {
            next.storage_limit_gib = checked_storage_limit(gib, used_bytes)?;
        }
        if let Some(auto_accept) = update.auto_accept {
            next.auto_accept = auto_accept;
        }
        if let Some(addresses) = &update.peer_addresses {
            next.peer_addresses = checked_peer_addresses(addresses)?;
        }
        if let Some(keep_sharing) = update.keep_sharing_in_background {
            next.keep_sharing_in_background = keep_sharing;
        }
        self.save(&next)?;
        *self.current.write().unwrap() = next.clone();
        Ok(next)
    }

    fn save(&self, preferences: &Preferences) -> Result<(), Invalid> {
        let bytes = serde_json::to_vec_pretty(preferences).map_err(|e| Invalid::Save(e.into()))?;
        persist::write_private(&self.dir.join(FILE_NAME), &bytes).map_err(Invalid::Save)
    }
}

fn read(dir: &Path) -> Preferences {
    let path = dir.join(FILE_NAME);
    let Ok(bytes) = std::fs::read(&path) else { return Preferences::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!("Preferences: {} is damaged ({e}); it is set aside and the defaults are used", path.display());
        let _ = std::fs::rename(&path, path.with_extension("damaged"));
        Preferences::default()
    })
}

/// The preferences saved in this data directory, without opening a store; for starting a node.
pub fn load(dir: &Path) -> Preferences {
    read(dir)
}

pub fn checked_storage_limit(gib: u64, used_bytes: u64) -> Result<u64, Invalid> {
    if !(1..=MAX_STORAGE_LIMIT_GIB).contains(&gib) {
        return Err(Invalid::StorageLimit);
    }
    if gib * BYTES_PER_GIB < used_bytes {
        return Err(Invalid::StorageBelowUsed { used_bytes });
    }
    Ok(gib)
}

// The folder is created if needed and tried with a file, so a read-only or missing disk is found now and not at the first download.
fn checked_downloads_dir(path: &Path) -> Result<Option<PathBuf>, Invalid> {
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    let refuse = |reason: &str| Invalid::DownloadsDir { path: path.to_path_buf(), reason: reason.to_string() };
    if !path.is_absolute() {
        return Err(refuse("give the full path of the folder"));
    }
    std::fs::create_dir_all(path).map_err(|e| refuse(&e.to_string()))?;
    let probe = path.join(format!(".ladex-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"").map_err(|e| refuse(&e.to_string()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(Some(path.to_path_buf()))
}

/// Reads typed addresses, leaving out repeats; a missing port is the one nodes use unless told otherwise.
pub fn checked_peer_addresses(texts: &[String]) -> Result<Vec<SocketAddr>, Invalid> {
    let mut addresses = Vec::new();
    for text in texts {
        let address = parse_peer_address(text.trim()).ok_or_else(|| Invalid::PeerAddress(text.clone()))?;
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    Ok(addresses)
}

fn parse_peer_address(text: &str) -> Option<SocketAddr> {
    let address = match text.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) => SocketAddr::new(text.parse::<IpAddr>().ok()?, DEFAULT_HTTP_PORT),
    };
    (address.port() != 0 && !address.ip().is_unspecified()).then_some(address)
}

/// The free space on the disk that holds `path`, even when the folder is not there yet.
pub fn free_space_at(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists())?;
    fs4::available_space(existing).ok()
}

/// Applies a change to a running node: saved, and the storage limit and new peer addresses take effect at once.
pub fn apply(state: &NodeState, update: &Update) -> Result<Preferences, Invalid> {
    let before = state.preferences.get();
    let preferences = state.preferences.update(update, state.store.used_bytes())?;
    state.store.set_quota(preferences.storage_limit_bytes());
    for &address in preferences.peer_addresses.iter().filter(|address| !before.peer_addresses.contains(address)) {
        crate::mesh::dialer::keep_connected_in_background(state, address);
    }
    Ok(preferences)
}

/// The preferences with what the page and `ladex settings` need to show beside them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub preferences: Preferences,
    pub data_dir: PathBuf,
    /// Whether the CLI finds this node without being told the data directory.
    pub is_default_data_dir: bool,
    pub has_passphrase: bool,
    pub needs_restart: bool,
    pub store_used_bytes: u64,
    pub disk_free_bytes: Option<u64>,
    /// "windows" or "unix": which shell quoting a copied command needs.
    pub platform: String,
    /// What another device types to add this one by address; None while this node has no network address.
    pub this_address: Option<String>,
}

pub fn view(state: &NodeState) -> View {
    let data_dir = state.started_with.data_dir.clone();
    View {
        preferences: state.preferences.get(),
        is_default_data_dir: data_dir == crate::node::default_data_dir(),
        has_passphrase: state.passphrase.is_some(),
        needs_restart: state.preferences.needs_restart(),
        store_used_bytes: state.store.used_bytes(),
        disk_free_bytes: fs4::available_space(&data_dir).ok(),
        platform: if cfg!(windows) { "windows" } else { "unix" }.to_string(),
        this_address: state.local_ip().map(|ip| SocketAddr::new(ip, state.quic_port).to_string()),
        data_dir,
    }
}

/// Saves a new mesh passphrase for the next start; the running node keeps the one it has.
pub fn change_passphrase(state: &NodeState, use_keychain: bool, passphrase: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!passphrase.is_empty(), "the passphrase must not be empty");
    crate::saved_passphrase::save(&state.started_with.data_dir, use_keychain, passphrase)?;
    state.preferences.mark_needs_restart();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PreferencesStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PreferencesStore::open(dir.path());
        (dir, store)
    }

    fn limit(gib: u64) -> Update {
        Update { storage_limit_gib: Some(gib), ..Default::default() }
    }

    fn addresses(texts: &[&str]) -> Result<Vec<SocketAddr>, Invalid> {
        checked_peer_addresses(&texts.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn a_peer_address_with_a_port_is_kept_as_typed() {
        assert_eq!(addresses(&["192.168.1.20:9000"]).unwrap(), vec!["192.168.1.20:9000".parse().unwrap()]);
    }

    #[test]
    fn a_peer_address_without_a_port_gets_the_default_one() {
        assert_eq!(addresses(&["192.168.1.20"]).unwrap(), vec![SocketAddr::from(([192, 168, 1, 20], DEFAULT_HTTP_PORT))]);
    }

    #[test]
    fn spaces_around_a_peer_address_are_ignored() {
        assert_eq!(addresses(&[" 10.0.0.2:8080 "]).unwrap(), vec!["10.0.0.2:8080".parse().unwrap()]);
    }

    #[test]
    fn the_same_peer_address_twice_is_kept_once() {
        assert_eq!(addresses(&["10.0.0.2", "10.0.0.2:8080"]).unwrap().len(), 1);
    }

    #[test]
    fn a_name_is_not_a_peer_address() {
        assert!(matches!(addresses(&["my-laptop"]), Err(Invalid::PeerAddress(_))));
    }

    #[test]
    fn an_unspecified_ip_is_not_a_peer_address() {
        assert!(matches!(addresses(&["0.0.0.0:8080"]), Err(Invalid::PeerAddress(_))));
    }

    #[test]
    fn port_zero_is_not_a_peer_address() {
        assert!(matches!(addresses(&["10.0.0.2:0"]), Err(Invalid::PeerAddress(_))));
    }

    #[test]
    fn saved_peer_addresses_are_there_after_a_restart() {
        let (dir, store) = store();
        store.update(&Update { peer_addresses: Some(vec!["10.0.0.2".to_string()]), ..Default::default() }, 0).unwrap();
        assert_eq!(PreferencesStore::open(dir.path()).get().peer_addresses.len(), 1);
    }

    #[test]
    fn preferences_saved_before_peer_addresses_existed_still_load() {
        let (dir, _store) = store();
        std::fs::write(dir.path().join(FILE_NAME), br#"{"is_setup_done":true,"storage_limit_gib":5}"#).unwrap();
        assert!(load(dir.path()).peer_addresses.is_empty());
    }

    #[test]
    fn sharing_in_the_background_is_off_until_chosen_and_remembered_once_it_is() {
        let (dir, store) = store();
        assert!(!store.get().keep_sharing_in_background);

        store.update(&Update { keep_sharing_in_background: Some(true), ..Default::default() }, 0).unwrap();
        assert!(PreferencesStore::open(dir.path()).get().keep_sharing_in_background);
    }

    #[test]
    fn a_new_data_directory_has_the_default_preferences() {
        let (_dir, store) = store();
        assert_eq!(store.get(), Preferences::default());
        assert_eq!(store.get().storage_limit_gib, DEFAULT_STORAGE_LIMIT_GIB);
    }

    #[test]
    fn changed_preferences_are_there_after_a_restart() {
        let (dir, store) = store();
        store.update(&Update { is_setup_done: Some(true), storage_limit_gib: Some(50), ..Default::default() }, 0).unwrap();
        let reopened = PreferencesStore::open(dir.path());
        assert_eq!((reopened.get().is_setup_done, reopened.get().storage_limit_gib), (true, 50));
    }

    #[test]
    fn what_an_update_leaves_out_stays_as_it_was() {
        let (_dir, store) = store();
        store.update(&limit(50), 0).unwrap();
        store.update(&Update { is_setup_done: Some(true), ..Default::default() }, 0).unwrap();
        assert_eq!(store.get().storage_limit_gib, 50);
    }

    #[test]
    fn a_storage_limit_of_zero_is_refused_and_nothing_changes() {
        let (_dir, store) = store();
        assert!(matches!(store.update(&limit(0), 0), Err(Invalid::StorageLimit)));
        assert_eq!(store.get(), Preferences::default());
    }

    #[test]
    fn a_storage_limit_below_what_is_stored_is_refused() {
        let (_dir, store) = store();
        let used = 5 * BYTES_PER_GIB;
        assert!(matches!(store.update(&limit(4), used), Err(Invalid::StorageBelowUsed { .. })));
        assert!(store.update(&limit(5), used).is_ok());
    }

    #[test]
    fn a_downloads_folder_is_created_and_remembered() {
        let (dir, store) = store();
        let folder = dir.path().join("received").join("here");
        store.update(&Update { downloads_dir: Some(folder.clone()), ..Default::default() }, 0).unwrap();
        assert!(folder.is_dir());
        assert_eq!(store.get().downloads_dir, Some(folder));
    }

    #[test]
    fn a_relative_downloads_folder_is_refused() {
        let (_dir, store) = store();
        let update = Update { downloads_dir: Some(PathBuf::from("received")), ..Default::default() };
        assert!(matches!(store.update(&update, 0), Err(Invalid::DownloadsDir { .. })));
    }

    #[test]
    fn an_empty_downloads_folder_goes_back_to_the_default() {
        let (dir, store) = store();
        store.update(&Update { downloads_dir: Some(dir.path().join("received")), ..Default::default() }, 0).unwrap();
        store.update(&Update { downloads_dir: Some(PathBuf::new()), ..Default::default() }, 0).unwrap();
        assert_eq!(store.get().downloads_dir, None);
    }

    #[test]
    fn a_damaged_file_is_set_aside_and_the_defaults_are_used() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"{ not json").unwrap();
        assert_eq!(PreferencesStore::open(dir.path()).get(), Preferences::default());
        assert!(dir.path().join("preferences.damaged").exists());
    }

    #[test]
    fn a_file_from_a_later_version_with_extra_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), br#"{"storage_limit_gib": 7, "something_new": true}"#).unwrap();
        assert_eq!(PreferencesStore::open(dir.path()).get().storage_limit_gib, 7);
    }

    #[test]
    fn nobody_is_accepted_without_asking_by_default() {
        assert!(!AutoAccept::default().allows(Some(1), true));
    }

    #[test]
    fn paired_devices_are_accepted_and_other_devices_are_asked_about() {
        let rule = AutoAccept { from: AutoAcceptFrom::PairedDevices, ask_above_mib: None };
        assert!(rule.allows(Some(1), true));
        assert!(!rule.allows(Some(1), false));
    }

    #[test]
    fn the_mesh_rule_accepts_any_sender() {
        let rule = AutoAccept { from: AutoAcceptFrom::Mesh, ask_above_mib: None };
        assert!(rule.allows(Some(1), false));
    }

    #[test]
    fn a_file_over_the_size_limit_is_asked_about_even_from_a_trusted_sender() {
        let rule = AutoAccept { from: AutoAcceptFrom::Mesh, ask_above_mib: Some(100) };
        assert!(rule.allows(Some(100 * BYTES_PER_MIB), true));
        assert!(!rule.allows(Some(100 * BYTES_PER_MIB + 1), true));
    }

    #[test]
    fn a_file_of_unknown_size_is_asked_about_when_there_is_a_limit() {
        let limited = AutoAccept { from: AutoAcceptFrom::Mesh, ask_above_mib: Some(100) };
        let unlimited = AutoAccept { from: AutoAcceptFrom::Mesh, ask_above_mib: None };
        assert!(!limited.allows(None, true));
        assert!(unlimited.allows(None, true));
    }
}
