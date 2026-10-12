//! Saving received files into a downloads folder: nothing a sender says about a file is trusted, and nothing is written
//! outside the folder.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

use crate::catalog::CatalogEntry;
use crate::files_api::{self, ListingError};
use crate::store::{self, hash_chunk, manifest_root, Blob, ChunkHash};
use crate::transfer::{self, FetchError};
use crate::{validate, NodeState};

const PART_PREFIX: &str = ".ladex-";
const PART_SUFFIX: &str = ".part";
// A save cut short by a crash leaves its part file behind; this long after, no save can still be writing it.
const STALE_PART_AGE: Duration = Duration::from_secs(24 * 60 * 60);
// "name (9999).ext" is as far as the search for a free name goes.
const MAX_NAME_ATTEMPTS: u32 = 10_000;

#[derive(Debug, PartialEq, Eq)]
pub enum SaveError {
    NotFound,
    Fetch(FetchError),
    Unavailable,
    SizeMismatch,
    WrongContent,
    DiskFull,
    DamagedFolder,
    NameTaken,
    Io(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::NotFound => f.write_str("no such file"),
            SaveError::Fetch(e) => write!(f, "{e}"),
            SaveError::Unavailable => f.write_str("the file's contents are not available right now"),
            SaveError::SizeMismatch => f.write_str("the file is not the size its sender said"),
            SaveError::WrongContent => f.write_str("the file is not the one its sender described"),
            SaveError::DiskFull => f.write_str("not enough free disk space in the downloads folder"),
            SaveError::DamagedFolder => f.write_str("the folder's listing is damaged or part of it is gone"),
            SaveError::NameTaken => f.write_str("too many files with this name are already there"),
            SaveError::Io(e) => write!(f, "could not save the file: {e}"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        if store::is_disk_full(&e) {
            SaveError::DiskFull
        } else {
            SaveError::Io(e.to_string())
        }
    }
}

/// A file this node holds, with the size and content its sender promised.
pub struct Content<'a> {
    pub blob: &'a Blob,
    pub size: u64,
    pub manifest_root: &'a str,
}

impl<'a> Content<'a> {
    fn listed_as(blob: &'a Blob, entry: &'a CatalogEntry) -> Self {
        Self { blob, size: entry.size, manifest_root: &entry.manifest_root }
    }
}

// Where one file goes: a name inside a directory that already exists below `folder`.
struct Target {
    folder: Arc<Dir>,
    parent: PathBuf,
    name: String,
}

/// The folder received files are saved into. Every write goes through this one handle, so none can land outside it.
pub struct ReceiveRoot {
    dir: Arc<Dir>,
    path: PathBuf,
}

impl ReceiveRoot {
    /// Opens the folder, creating it if needed, and clears out saves an earlier run never finished.
    pub fn open(path: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(path)?;
        let path = path.canonicalize()?;
        let dir = Dir::open_ambient_dir(&path, ambient_authority())?;
        remove_stale_parts(&dir);
        Ok(Self { dir: Arc::new(dir), path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Saves a file under the name its sender gave it, made safe and never over another file; returns where it went.
    pub async fn save_file(&self, name: &str, content: &Content<'_>, chunk_wait: Duration) -> Result<PathBuf, SaveError> {
        let target = Target { folder: self.dir.clone(), parent: PathBuf::new(), name: validate::sanitize_file_name(name) };
        let saved = self.save_to(target, content, chunk_wait).await?;
        Ok(self.path.join(saved))
    }

    fn check_room(&self, size: u64) -> Result<(), SaveError> {
        if store::lacks_free_space(&self.path, size) {
            return Err(SaveError::DiskFull);
        }
        Ok(())
    }

    // Written under a temporary name and only given its real one once every byte is checked and on disk.
    async fn save_to(&self, target: Target, content: &Content<'_>, chunk_wait: Duration) -> Result<PathBuf, SaveError> {
        if content.blob.size() != content.size {
            return Err(SaveError::SizeMismatch);
        }
        self.check_room(content.size)?;
        let part = Part::create(self.dir.clone())?;
        copy_verified(content, &part, chunk_wait).await?;
        tokio::task::spawn_blocking(move || part.move_to(&target)).await.map_err(|e| SaveError::Io(e.to_string()))?
    }
}

// A file being written, under a name no sender chose; it is deleted unless it is moved into place.
struct Part {
    root: Arc<Dir>,
    name: String,
    file: Arc<std::fs::File>,
    is_placed: bool,
}

impl Part {
    fn create(root: Arc<Dir>) -> io::Result<Self> {
        let name = format!("{PART_PREFIX}{:032x}{PART_SUFFIX}", rand::random::<u128>());
        let file = root.open_with(&name, OpenOptions::new().write(true).create_new(true))?.into_std();
        Ok(Self { root, name, file: Arc::new(file), is_placed: false })
    }

    async fn append(&self, data: Vec<u8>) -> Result<ChunkHash, SaveError> {
        let file = self.file.clone();
        let written = tokio::task::spawn_blocking(move || {
            (&*file).write_all(&data)?;
            Ok::<ChunkHash, io::Error>(hash_chunk(&data))
        });
        Ok(written.await.map_err(|e| SaveError::Io(e.to_string()))??)
    }

    fn move_to(mut self, target: &Target) -> Result<PathBuf, SaveError> {
        self.file.sync_all()?;
        let create_empty = |path: &Path| target.folder.open_with(path, OpenOptions::new().write(true).create_new(true)).map(drop);
        let path = claim_free_name(&target.parent, &target.name, create_empty)?;
        // The name is held by an empty file until the finished one replaces it, so two saves can never share a name.
        if let Err(e) = self.root.rename(&self.name, &target.folder, &path) {
            let _ = target.folder.remove_file(&path);
            return Err(e.into());
        }
        self.is_placed = true;
        sync_dir(&target.folder, &target.parent)?;
        Ok(path)
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if !self.is_placed {
            let _ = self.root.remove_file(&self.name);
        }
    }
}

// Exactly the promised number of bytes is written, and what was written must hash to the promised root.
async fn copy_verified(content: &Content<'_>, part: &Part, chunk_wait: Duration) -> Result<(), SaveError> {
    let blob = content.blob;
    let mut hashes = Vec::with_capacity(blob.chunk_count() as usize);
    for index in 0..blob.chunk_count() {
        if !blob.wait_for_chunk(index, chunk_wait).await {
            return Err(SaveError::Unavailable);
        }
        let data = blob.read_chunk_verified(index).await?;
        hashes.push(part.append(data).await?);
    }
    if manifest_root(content.size, &hashes) != content.manifest_root {
        return Err(SaveError::WrongContent);
    }
    Ok(())
}

// Tries "name", then "name (1)", "name (2)" and so on; `create` must fail if the path exists.
fn claim_free_name(parent: &Path, name: &str, create: impl Fn(&Path) -> io::Result<()>) -> Result<PathBuf, SaveError> {
    for number in 0..MAX_NAME_ATTEMPTS {
        let candidate = if number == 0 { name.to_string() } else { validate::numbered_file_name(name, number) };
        let path = parent.join(candidate);
        match create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(SaveError::NameTaken)
}

#[cfg(unix)]
fn sync_dir(folder: &Dir, parent: &Path) -> io::Result<()> {
    let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
    folder.open(parent)?.sync_all()
}

// Windows cannot open a directory to sync it; NTFS journals the rename itself.
#[cfg(not(unix))]
fn sync_dir(_folder: &Dir, _parent: &Path) -> io::Result<()> {
    Ok(())
}

fn remove_stale_parts(dir: &Dir) {
    let Ok(entries) = dir.entries() else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_part = name.starts_with(PART_PREFIX) && name.ends_with(PART_SUFFIX);
        let modified = entry.metadata().and_then(|metadata| metadata.modified());
        let is_stale = modified.is_ok_and(|at| at.into_std().elapsed().is_ok_and(|age| age >= STALE_PART_AGE));
        if !is_part || !is_stale {
            continue;
        }
        if let Err(e) = entry.remove_file() {
            tracing::warn!("Receive: could not remove the unfinished save {name}: {e}");
        }
    }
}

/// Saves a shared file or folder into `root`, fetching what this node does not hold yet; returns where it went.
pub async fn save_shared(state: &NodeState, root: &ReceiveRoot, file_id: &str) -> Result<PathBuf, SaveError> {
    let entry = state.catalog.listed(file_id).ok_or(SaveError::NotFound)?.entry;
    if entry.is_folder() {
        return save_folder(state, root, &entry).await;
    }
    let blob = transfer::ensure_file(state, file_id).await.map_err(SaveError::Fetch)?;
    let saved = root.save_file(&entry.name, &Content::listed_as(&blob, &entry), state.transfers.stall_timeout()).await?;
    tracing::info!("Receive: saved {} ({} bytes) as {}", entry.name, entry.size, saved.display());
    Ok(saved)
}

// A folder arrives as a listing of files, never as an archive to unpack.
async fn save_folder(state: &NodeState, root: &ReceiveRoot, folder: &CatalogEntry) -> Result<PathBuf, SaveError> {
    let children = folder_children(state, folder).await?;
    let total_bytes = children.iter().fold(0u64, |sum, (_, child)| sum.saturating_add(child.size));
    root.check_room(total_bytes)?;

    let name = validate::sanitize_file_name(&folder.name);
    let folder_path = claim_free_name(Path::new(""), &name, |path| root.dir.create_dir(path))?;
    let filled = fill_folder(state, root, &folder_path, &children).await;
    if let Err(e) = filled {
        if let Err(cleanup_error) = root.dir.remove_dir_all(&folder_path) {
            tracing::warn!("Receive: could not remove the unfinished folder {}: {cleanup_error}", folder_path.display());
        }
        return Err(e);
    }
    let saved = root.path.join(folder_path);
    tracing::info!("Receive: saved {} ({} files, {total_bytes} bytes) as {}", folder.name, children.len(), saved.display());
    Ok(saved)
}

async fn folder_children(state: &NodeState, folder: &CatalogEntry) -> Result<Vec<(String, CatalogEntry)>, SaveError> {
    let listing_blob = transfer::ensure_file(state, &folder.file_id).await.map_err(SaveError::Fetch)?;
    let listing = match files_api::read_folder_listing(state, &listing_blob).await {
        Ok(listing) => listing,
        Err(ListingError::Unavailable) => return Err(SaveError::Unavailable),
        Err(ListingError::Unreadable | ListingError::Damaged) => return Err(SaveError::DamagedFolder),
    };
    files_api::listed_children(state, &folder.file_id, &listing).ok_or(SaveError::DamagedFolder)
}

async fn fill_folder(
    state: &NodeState,
    root: &ReceiveRoot,
    folder_path: &Path,
    children: &[(String, CatalogEntry)],
) -> Result<(), SaveError> {
    // The folder gets a handle of its own, so a file in it cannot even land elsewhere in the downloads folder.
    let folder = Arc::new(root.dir.open_dir(folder_path)?);
    for (path, child) in children {
        let path = Path::new(path);
        let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = path.file_name().and_then(|name| name.to_str()).ok_or(SaveError::DamagedFolder)?.to_string();
        if !parent.as_os_str().is_empty() {
            folder.create_dir_all(&parent)?;
        }
        let blob = transfer::ensure_file(state, &child.file_id).await.map_err(SaveError::Fetch)?;
        let target = Target { folder: folder.clone(), parent, name };
        root.save_to(target, &Content::listed_as(&blob, child), state.transfers.stall_timeout()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn downloads() -> (tempfile::TempDir, ReceiveRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = ReceiveRoot::open(&dir.path().join("downloads")).unwrap();
        (dir, root)
    }

    fn part_file(root: &ReceiveRoot, age: Duration) -> PathBuf {
        let path = root.path().join(format!("{PART_PREFIX}abc{PART_SUFFIX}"));
        let file = std::fs::File::create(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age).unwrap();
        path
    }

    #[test]
    fn a_save_larger_than_the_free_space_is_refused() {
        let (_dir, root) = downloads();
        assert_eq!(root.check_room(u64::MAX / 2), Err(SaveError::DiskFull));
        assert_eq!(root.check_room(1), Ok(()));
    }

    #[test]
    fn a_full_disk_is_reported_as_such() {
        assert_eq!(SaveError::from(io::Error::from_raw_os_error(28)), SaveError::DiskFull);
    }

    #[test]
    fn a_taken_name_gets_the_next_free_number() {
        let (_dir, root) = downloads();
        let create = |path: &Path| root.dir.create_dir(path);
        let claimed: Vec<PathBuf> = (0..3).map(|_| claim_free_name(Path::new(""), "notes.txt", create).unwrap()).collect();
        assert_eq!(claimed, ["notes.txt", "notes (1).txt", "notes (2).txt"].map(PathBuf::from));
    }

    #[test]
    fn an_old_unfinished_save_is_removed_when_the_folder_is_opened() {
        let (_dir, root) = downloads();
        let stale = part_file(&root, STALE_PART_AGE + Duration::from_secs(60));
        ReceiveRoot::open(root.path()).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn a_save_that_may_still_be_running_is_left_alone() {
        let (_dir, root) = downloads();
        let recent = part_file(&root, Duration::from_secs(60));
        ReceiveRoot::open(root.path()).unwrap();
        assert!(recent.exists());
    }

    #[test]
    fn the_users_own_files_are_never_cleared_out() {
        let (_dir, root) = downloads();
        let kept = root.path().join("thesis.part");
        std::fs::File::create(&kept).unwrap().set_modified(std::time::SystemTime::UNIX_EPOCH).unwrap();
        ReceiveRoot::open(root.path()).unwrap();
        assert!(kept.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_directory_leading_out_of_the_folder_is_not_followed() {
        let (dir, root) = downloads();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join("link")).unwrap();
        let state = NodeState::for_tests(None);
        let blob = state.store.create("file_1", 7).unwrap();
        blob.write_chunk_hashing(0, b"payload".to_vec()).await.unwrap();
        let manifest_root = blob.seal().await.unwrap();
        let target = Target { folder: root.dir.clone(), parent: PathBuf::from("link"), name: "a.txt".to_string() };

        let refused = root.save_to(target, &Content { blob: &blob, size: 7, manifest_root: &manifest_root }, Duration::ZERO).await;

        assert!(refused.is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
