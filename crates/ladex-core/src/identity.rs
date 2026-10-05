//! This node's Ed25519 identity, kept in the OS keychain or a private file; the node id is derived from the public key.

use std::path::{Path, PathBuf};

use anyhow::Context;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;

use crate::types::NodeId;

const NODE_ID_CHARS: usize = 20;
const KEY_FILE: &str = "identity.key";
const KEYCHAIN_SERVICE: &str = "ladex";

pub struct Identity {
    key: SigningKey,
    node_id: NodeId,
}

impl Identity {
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        Self::from_secret(&secret)
    }

    pub fn from_secret(secret: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(secret);
        let node_id = node_id_for(&key.verifying_key());
        Self { key, node_id }
    }

    fn from_stored(bytes: &[u8]) -> anyhow::Result<Self> {
        let secret: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("the stored key is {} bytes, not 32", bytes.len()))?;
        Ok(Self::from_secret(&secret))
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.key.sign(message)
    }
}

/// `base32(SHA-256(public key))`, first 20 characters (100 bits), lowercase.
pub fn node_id_for(public_key: &VerifyingKey) -> NodeId {
    let digest = ring::digest::digest(&ring::digest::SHA256, public_key.as_bytes());
    let mut id = data_encoding::BASE32_NOPAD.encode(digest.as_ref());
    id.truncate(NODE_ID_CHARS);
    id.to_ascii_lowercase()
}

pub fn verify(public_key: &VerifyingKey, message: &[u8], signature: &Signature) -> bool {
    public_key.verify_strict(message, signature).is_ok()
}

/// Where the private key is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyLocation {
    Keychain,
    File(PathBuf),
}

impl std::fmt::Display for KeyLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyLocation::Keychain => f.write_str("the OS keychain"),
            KeyLocation::File(path) => write!(f, "{}", path.display()),
        }
    }
}

trait SecretStore {
    /// `Ok(None)` when nothing is stored yet.
    fn read(&self) -> anyhow::Result<Option<Vec<u8>>>;
    fn write(&self, secret: &[u8]) -> anyhow::Result<()>;
}

struct Keychain(keyring::Entry);

impl Keychain {
    /// None when this machine has no usable keychain (e.g. Linux without a Secret Service).
    fn for_dir(dir: &Path) -> Option<Self> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(dir)).ok().map(Keychain)
    }
}

impl SecretStore for Keychain {
    fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        match self.0.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, secret: &[u8]) -> anyhow::Result<()> {
        Ok(self.0.set_secret(secret)?)
    }
}

struct KeyFile(PathBuf);

impl SecretStore for KeyFile {
    fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.0) {
            Ok(secret) => Ok(Some(secret)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, secret: &[u8]) -> anyhow::Result<()> {
        Ok(crate::persist::write_private(&self.0, secret)?)
    }
}

// Each data directory is a separate node, so several nodes on one machine get separate keys.
fn keychain_account(dir: &Path) -> String {
    format!("node key for {}", dir.display())
}

/// Loads this node's identity, or creates one on first run; `use_keychain` false keeps the key in a file.
pub fn load_or_create(dir: &Path, use_keychain: bool) -> anyhow::Result<(Identity, KeyLocation)> {
    crate::tls::create_private_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let keychain = if use_keychain { Keychain::for_dir(&dir) } else { None };
    let file = dir.join(KEY_FILE);
    load_or_create_with(keychain.as_ref().map(|k| k as &dyn SecretStore), &KeyFile(file.clone()), file)
}

fn load_or_create_with(keychain: Option<&dyn SecretStore>, file: &dyn SecretStore, file_path: PathBuf) -> anyhow::Result<(Identity, KeyLocation)> {
    let mut keychain_error = None;
    if let Some(keychain) = keychain {
        match keychain.read() {
            Ok(Some(secret)) => return Ok((Identity::from_stored(&secret).context("the key in the OS keychain is damaged")?, KeyLocation::Keychain)),
            Ok(None) => {}
            Err(e) => keychain_error = Some(e),
        }
    }

    if let Some(secret) = file.read().with_context(|| format!("could not read {}", file_path.display()))? {
        let identity = Identity::from_stored(&secret).with_context(|| format!("{} is damaged", file_path.display()))?;
        return Ok((identity, KeyLocation::File(file_path)));
    }

    // A locked keychain may already hold this node's key; making a new one would split its identity.
    if let Some(e) = keychain_error {
        anyhow::bail!("could not read this node's key from the OS keychain ({e}). Unlock it, or run with --no-keychain to keep the key in a file");
    }

    let identity = Identity::generate();
    let secret = identity.key.to_bytes();
    if let Some(keychain) = keychain {
        // Read back, so a keychain that accepts writes but loses them isn't trusted.
        match keychain.write(&secret).and_then(|_| keychain.read()) {
            Ok(Some(stored)) if stored == secret => return Ok((identity, KeyLocation::Keychain)),
            Ok(_) => tracing::warn!("Identity: the OS keychain did not keep the key; using a file instead"),
            Err(e) => tracing::warn!("Identity: could not save the key in the OS keychain ({e}); using a file instead"),
        }
    }
    file.write(&secret).with_context(|| format!("could not save the key to {}", file_path.display()))?;
    Ok((identity, KeyLocation::File(file_path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeKeychain {
        secret: Mutex<Option<Vec<u8>>>,
        locked: bool,
        forgets_writes: bool,
    }

    impl SecretStore for FakeKeychain {
        fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
            anyhow::ensure!(!self.locked, "locked");
            Ok(self.secret.lock().unwrap().clone())
        }

        fn write(&self, secret: &[u8]) -> anyhow::Result<()> {
            anyhow::ensure!(!self.locked, "locked");
            if !self.forgets_writes {
                *self.secret.lock().unwrap() = Some(secret.to_vec());
            }
            Ok(())
        }
    }

    fn holding(secret: &[u8]) -> FakeKeychain {
        FakeKeychain { secret: Mutex::new(Some(secret.to_vec())), ..Default::default() }
    }

    struct Dir {
        _temp: tempfile::TempDir,
        path: PathBuf,
    }

    fn key_dir() -> Dir {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(KEY_FILE);
        Dir { _temp: temp, path }
    }

    fn load(keychain: Option<&FakeKeychain>, dir: &Dir) -> anyhow::Result<(Identity, KeyLocation)> {
        load_or_create_with(keychain.map(|k| k as &dyn SecretStore), &KeyFile(dir.path.clone()), dir.path.clone())
    }

    #[test]
    fn a_node_id_is_twenty_lowercase_base32_characters() {
        let id = Identity::generate().node_id().to_string();
        assert_eq!(id.len(), 20);
        assert!(id.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)), "{id}");
        assert!(crate::validate::is_valid_id(&id));
    }

    #[test]
    fn the_same_key_always_gives_the_same_node_id() {
        assert_eq!(Identity::from_secret(&[7; 32]).node_id(), Identity::from_secret(&[7; 32]).node_id());
    }

    #[test]
    fn different_keys_give_different_node_ids() {
        assert_ne!(Identity::from_secret(&[7; 32]).node_id(), Identity::from_secret(&[8; 32]).node_id());
    }

    #[test]
    fn the_node_id_comes_from_the_public_key() {
        let identity = Identity::generate();
        assert_eq!(node_id_for(&identity.public_key()), identity.node_id());
    }

    #[test]
    fn a_signature_verifies_with_the_signers_key() {
        let identity = Identity::generate();
        assert!(verify(&identity.public_key(), b"revoke", &identity.sign(b"revoke")));
    }

    #[test]
    fn a_signature_does_not_verify_with_another_key() {
        let signature = Identity::generate().sign(b"revoke");
        assert!(!verify(&Identity::generate().public_key(), b"revoke", &signature));
    }

    #[test]
    fn a_signature_does_not_cover_a_changed_message() {
        let identity = Identity::generate();
        assert!(!verify(&identity.public_key(), b"revoke node b", &identity.sign(b"revoke node a")));
    }

    #[test]
    fn the_first_run_keeps_a_new_key_in_the_keychain() {
        let (keychain, dir) = (FakeKeychain::default(), key_dir());
        let (identity, location) = load(Some(&keychain), &dir).unwrap();
        assert_eq!(location, KeyLocation::Keychain);
        assert_eq!(keychain.secret.lock().unwrap().as_deref(), Some(&identity.key.to_bytes()[..]));
        assert!(!dir.path.exists());
    }

    #[test]
    fn later_runs_load_the_same_key_from_the_keychain() {
        let (keychain, dir) = (FakeKeychain::default(), key_dir());
        let first = load(Some(&keychain), &dir).unwrap().0;
        let second = load(Some(&keychain), &dir).unwrap().0;
        assert_eq!(first.node_id(), second.node_id());
    }

    #[test]
    fn without_a_keychain_the_key_is_kept_in_a_file() {
        let dir = key_dir();
        let (identity, location) = load(None, &dir).unwrap();
        assert_eq!(location, KeyLocation::File(dir.path.clone()));
        assert_eq!(load(None, &dir).unwrap().0.node_id(), identity.node_id());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = key_dir();
        load(None, &dir).unwrap();
        assert_eq!(std::fs::metadata(&dir.path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_key_in_a_file_is_still_used_once_a_keychain_appears() {
        let dir = key_dir();
        let from_file = load(None, &dir).unwrap().0;
        let (identity, location) = load(Some(&FakeKeychain::default()), &dir).unwrap();
        assert_eq!(identity.node_id(), from_file.node_id());
        assert_eq!(location, KeyLocation::File(dir.path.clone()));
    }

    #[test]
    fn the_keychain_key_wins_over_a_file() {
        let dir = key_dir();
        load(None, &dir).unwrap();
        let keychain = holding(&[9; 32]);
        assert_eq!(load(Some(&keychain), &dir).unwrap().0.node_id(), Identity::from_secret(&[9; 32]).node_id());
    }

    #[test]
    fn a_locked_keychain_does_not_lead_to_a_new_key() {
        let keychain = FakeKeychain { locked: true, ..Default::default() };
        let dir = key_dir();
        assert!(load(Some(&keychain), &dir).is_err());
        assert!(!dir.path.exists());
    }

    #[test]
    fn a_locked_keychain_falls_back_to_an_existing_key_file() {
        let dir = key_dir();
        let from_file = load(None, &dir).unwrap().0;
        let keychain = FakeKeychain { locked: true, ..Default::default() };
        assert_eq!(load(Some(&keychain), &dir).unwrap().0.node_id(), from_file.node_id());
    }

    #[test]
    fn a_keychain_that_loses_the_key_is_not_trusted_with_it() {
        let keychain = FakeKeychain { forgets_writes: true, ..Default::default() };
        let dir = key_dir();
        let (identity, location) = load(Some(&keychain), &dir).unwrap();
        assert_eq!(location, KeyLocation::File(dir.path.clone()));
        assert_eq!(load(None, &dir).unwrap().0.node_id(), identity.node_id());
    }

    #[test]
    fn a_damaged_key_is_reported_and_not_replaced() {
        let dir = key_dir();
        std::fs::write(&dir.path, b"too short").unwrap();
        assert!(load(None, &dir).is_err());
        assert_eq!(std::fs::read(&dir.path).unwrap(), b"too short");
    }

    #[test]
    fn each_data_directory_has_its_own_keychain_entry() {
        assert_ne!(keychain_account(Path::new("/data/one")), keychain_account(Path::new("/data/two")));
    }
}
