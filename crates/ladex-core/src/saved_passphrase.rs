//! The mesh passphrase a person chose in the app, kept in the OS keychain (or a private file) for the next start.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::identity::{KeyFile, Keychain, SecretStore};

const FILE_NAME: &str = "passphrase";

fn keychain_account(dir: &Path) -> String {
    format!("mesh passphrase for {}", dir.display())
}

struct Places {
    keychain: Option<Keychain>,
    file: KeyFile,
}

fn places(dir: &Path, use_keychain: bool) -> Places {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let keychain = if use_keychain { Keychain::for_account(&keychain_account(&dir)) } else { None };
    Places { keychain, file: KeyFile(dir.join(FILE_NAME)) }
}

fn text_of(secret: Vec<u8>) -> anyhow::Result<String> {
    String::from_utf8(secret).context("the saved passphrase is damaged")
}

/// The passphrase saved for this data directory, if any. A locked keychain is an error: starting without the passphrase
/// would leave the node open to the network.
pub fn load(dir: &Path, use_keychain: bool) -> anyhow::Result<Option<String>> {
    let places = places(dir, use_keychain);
    let mut keychain_error = None;
    if let Some(keychain) = &places.keychain {
        match keychain.read() {
            Ok(Some(secret)) => return text_of(secret).map(Some),
            Ok(None) => {}
            Err(e) => keychain_error = Some(e),
        }
    }
    match places.file.read().with_context(|| format!("could not read {}", places.file.0.display()))? {
        Some(secret) => text_of(secret).map(Some),
        None => match keychain_error {
            Some(e) => anyhow::bail!("could not read the saved passphrase from the OS keychain ({e}); unlock it and try again"),
            None => Ok(None),
        },
    }
}

/// Saves the passphrase in the keychain, or in a private file when there is no working keychain.
pub fn save(dir: &Path, use_keychain: bool, passphrase: &str) -> anyhow::Result<()> {
    crate::tls::create_private_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let places = places(dir, use_keychain);
    if let Some(keychain) = &places.keychain {
        // Read back, so a keychain that accepts writes but loses them is not trusted.
        let is_kept = keychain
            .write(passphrase.as_bytes())
            .and_then(|()| keychain.read())
            .is_ok_and(|stored| stored.as_deref() == Some(passphrase.as_bytes()));
        if is_kept {
            return places.file.delete();
        }
        tracing::warn!("Passphrase: the OS keychain did not keep it; using a file instead");
    }
    places.file.write(passphrase.as_bytes()).with_context(|| format!("could not save the passphrase to {}", places.file.0.display()))
}

/// Forgets the saved passphrase, so the node starts without one (or with the one given on the command line).
pub fn forget(dir: &Path, use_keychain: bool) -> anyhow::Result<()> {
    let places = places(dir, use_keychain);
    if let Some(keychain) = &places.keychain {
        keychain.delete()?;
    }
    places.file.delete()
}

pub fn file_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_passphrase_is_loaded_back() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), false, "river-stone-lamp-oak").unwrap();
        assert_eq!(load(dir.path(), false).unwrap().as_deref(), Some("river-stone-lamp-oak"));
    }

    #[test]
    fn nothing_is_loaded_before_one_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), false).unwrap(), None);
    }

    #[test]
    fn saving_again_replaces_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), false, "first-one").unwrap();
        save(dir.path(), false, "second-one").unwrap();
        assert_eq!(load(dir.path(), false).unwrap().as_deref(), Some("second-one"));
    }

    #[test]
    fn a_forgotten_passphrase_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), false, "first-one").unwrap();
        forget(dir.path(), false).unwrap();
        assert_eq!(load(dir.path(), false).unwrap(), None);
    }

    #[test]
    fn forgetting_when_nothing_is_saved_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        forget(dir.path(), false).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_passphrase_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), false, "first-one").unwrap();
        let mode = std::fs::metadata(file_in(dir.path())).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
