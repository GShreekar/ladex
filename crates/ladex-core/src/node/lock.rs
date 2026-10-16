//! The exclusive claim a running node holds on its data directory.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs4::fs_std::FileExt;

const LOCK_FILE_NAME: &str = "node.lock";

/// Held for as long as a node runs; the operating system releases it when this is dropped or the process dies.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
}

#[derive(Debug)]
pub enum LockError {
    /// Another node, in this process or another, holds the directory.
    InUse,
    Io(io::Error),
}

impl DataDirLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        crate::tls::create_private_dir(data_dir).map_err(LockError::Io)?;
        let file = open_lock_file(&data_dir.join(LOCK_FILE_NAME)).map_err(LockError::Io)?;
        match file.try_lock_exclusive() {
            Ok(true) => Ok(Self { _file: file }),
            Ok(false) => Err(LockError::InUse),
            Err(e) => Err(LockError::Io(e)),
        }
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_directory_cannot_be_locked_again() {
        let dir = tempfile::tempdir().unwrap();
        let _held = DataDirLock::acquire(dir.path()).unwrap();

        assert!(matches!(DataDirLock::acquire(dir.path()), Err(LockError::InUse)));
    }

    #[test]
    fn a_directory_can_be_locked_again_once_released() {
        let dir = tempfile::tempdir().unwrap();
        drop(DataDirLock::acquire(dir.path()).unwrap());

        assert!(DataDirLock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn different_directories_do_not_share_a_lock() {
        let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let _held = DataDirLock::acquire(first.path()).unwrap();

        assert!(DataDirLock::acquire(second.path()).is_ok());
    }
}
