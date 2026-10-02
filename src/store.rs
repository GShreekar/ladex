// On-disk storage for the files this node holds.
//
// A file is split into 1 MiB chunks, each with a SHA-256 hash. Every chunk is
// verified before it is written, so chunks fetched from different nodes can be
// mixed safely, and a corrupted copy is caught chunk by chunk, not at the end.
// Together the chunk hashes and the size give the file's `manifest root`, which
// the catalog carries; a manifest fetched from any node is checked against it.
//
// Layout, per file `<id>` under the store directory:
//   <id>.data    the file's bytes, preallocated, written at chunk offsets
//   <id>.hashes  32 bytes per chunk: the hash of each chunk we know
//   <id>.json    which chunks we have, the manifest root, and the catalog entry
// A partly received file survives a restart and carries on where it stopped.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ring::digest;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::bitmap::Bitmap;
use crate::types::FileMetadata;
use crate::validate;

pub const CHUNK_SIZE: u64 = 1 << 20;
// Always leave this much free on the disk, whatever the quota says.
const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;
// Progress is saved to disk after this many chunk writes (and when a file completes).
const PERSIST_EVERY_WRITES: u32 = 64;

pub type ChunkHash = [u8; 32];

pub fn chunk_count(size: u64) -> u32 {
    size.div_ceil(CHUNK_SIZE) as u32
}

pub fn chunk_len(size: u64, index: u32) -> usize {
    let start = index as u64 * CHUNK_SIZE;
    size.saturating_sub(start).min(CHUNK_SIZE) as usize
}

pub fn hash_chunk(data: &[u8]) -> ChunkHash {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(digest::digest(&digest::SHA256, data).as_ref());
    hash
}

// Identifies a file's exact contents: its size and every chunk hash.
pub fn manifest_root(size: u64, hashes: &[ChunkHash]) -> String {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"ladex manifest v1");
    context.update(&size.to_be_bytes());
    for hash in hashes {
        context.update(hash);
    }
    hex::encode(context.finish().as_ref())
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    InvalidId,
    SizeMismatch,
    QuotaExceeded,
    DiskFull,
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::InvalidId => f.write_str("invalid file id"),
            StoreError::SizeMismatch => f.write_str("a file with this id exists with a different size"),
            StoreError::QuotaExceeded => f.write_str("this node's storage limit would be exceeded"),
            StoreError::DiskFull => f.write_str("not enough free disk space"),
            StoreError::Io(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        // ENOSPC and the Windows equivalents
        if matches!(e.raw_os_error(), Some(28) | Some(112) | Some(39)) {
            StoreError::DiskFull
        } else {
            StoreError::Io(e.to_string())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    BadIndex,
    BadLength,
    NoManifest,
    HashMismatch,
    Removed,
    Io(String),
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

struct State {
    have: Bitmap,
    hashes: Vec<ChunkHash>,
    hash_known: Bitmap,
    // Set by whoever tells us what the file should be (the catalog), before any chunk is fetched.
    expected_root: Option<String>,
    // Set once every chunk is present and the manifest checked out.
    sealed: Option<String>,
    entry: Option<FileMetadata>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    id: String,
    size: u64,
    have: String,
    hash_known: String,
    expected_root: Option<String>,
    sealed: Option<String>,
    entry: Option<FileMetadata>,
}

pub struct Blob {
    id: String,
    size: u64,
    chunks: u32,
    paths: Paths,
    data: Arc<File>,
    hashes_file: Arc<File>,
    state: Mutex<State>,
    changed: Notify,
    removed: AtomicBool,
    // Only one upload may write a file at a time.
    writer: AtomicBool,
    writes_since_persist: AtomicU32,
    // Milliseconds since the epoch of the last write (or of opening it).
    last_activity_ms: AtomicU64,
}

fn now_ms() -> u64 {
    crate::hlc::wall_clock_ms()
}

#[derive(Clone)]
struct Paths {
    data: PathBuf,
    hashes: PathBuf,
    meta: PathBuf,
}

impl Paths {
    fn new(root: &Path, id: &str) -> Self {
        Self { data: root.join(format!("{id}.data")), hashes: root.join(format!("{id}.hashes")), meta: root.join(format!("{id}.json")) }
    }
}

impl Blob {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunks
    }

    pub fn bitmap(&self) -> Bitmap {
        self.state.lock().unwrap().have.clone()
    }

    pub fn has_chunk(&self, index: u32) -> bool {
        self.state.lock().unwrap().have.get(index)
    }

    pub fn is_complete(&self) -> bool {
        self.state.lock().unwrap().sealed.is_some()
    }

    pub fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    // Held while an upload is writing this file; None if another one is.
    pub fn try_write_lock(self: &Arc<Self>) -> Option<WriteGuard> {
        self.writer.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok()?;
        Some(WriteGuard(self.clone()))
    }

    pub fn is_being_written(&self) -> bool {
        self.writer.load(Ordering::Acquire)
    }

    // How long since a chunk was last written (or the file opened).
    pub fn idle_for(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }

    pub fn manifest_root(&self) -> Option<String> {
        self.state.lock().unwrap().sealed.clone()
    }

    pub fn expected_root(&self) -> Option<String> {
        self.state.lock().unwrap().expected_root.clone()
    }

    pub fn has_manifest(&self) -> bool {
        self.state.lock().unwrap().hash_known.is_full()
    }

    // The chunk hashes, once every one is known.
    pub fn hashes(&self) -> Option<Vec<ChunkHash>> {
        let state = self.state.lock().unwrap();
        state.hash_known.is_full().then(|| state.hashes.clone())
    }

    pub fn entry(&self) -> Option<FileMetadata> {
        self.state.lock().unwrap().entry.clone()
    }

    pub fn set_entry(&self, entry: FileMetadata) {
        self.state.lock().unwrap().entry = Some(entry);
    }

    // Chunks from the start of the file that are all present: where an interrupted upload resumes.
    pub fn leading_chunks(&self) -> u32 {
        self.state.lock().unwrap().have.leading_ones()
    }

    // Wakes anyone waiting on this file (a chunk arrived, it completed, or it was removed).
    fn changed(&self) {
        self.changed.notify_waiters();
    }

    // Waits until `index` is present. False on timeout or if the file is removed.
    pub async fn wait_for_chunk(&self, index: u32, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.has_chunk(index) {
                return true;
            }
            if self.is_removed() || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.has_chunk(index);
            }
        }
    }

    pub async fn read_chunk(&self, index: u32) -> io::Result<Vec<u8>> {
        if !self.has_chunk(index) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "chunk not present"));
        }
        let (file, offset, len) = (self.data.clone(), index as u64 * CHUNK_SIZE, chunk_len(self.size, index));
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            read_exact_at(&file, &mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    // Like `read_chunk`, but checks the bytes against the chunk's hash first.
    // Disks fail quietly, and a browser can't tell, so a chunk that no longer
    // matches is never served: it is dropped from what we have (so it gets
    // fetched again from another node) and reported as an error.
    pub async fn read_chunk_verified(&self, index: u32) -> io::Result<Vec<u8>> {
        let data = self.read_chunk(index).await?;
        let expected = {
            let state = self.state.lock().unwrap();
            state.hash_known.get(index).then(|| state.hashes[index as usize])
        };
        if expected.is_some_and(|hash| hash_chunk(&data) != hash) {
            tracing::error!("Store: chunk {index} of {} no longer matches its hash; dropping it", self.id);
            self.mark_corrupt(index);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk failed verification"));
        }
        Ok(data)
    }

    // Forget a chunk that turned out to be damaged; the file is no longer complete.
    pub fn mark_corrupt(&self, index: u32) {
        {
            let mut state = self.state.lock().unwrap();
            state.have.clear(index);
            state.sealed = None;
        }
        self.changed();
    }

    async fn write_at(&self, index: u32, data: Vec<u8>, hash: ChunkHash) -> Result<(), WriteError> {
        let (data_file, hashes_file) = (self.data.clone(), self.hashes_file.clone());
        let offset = index as u64 * CHUNK_SIZE;
        tokio::task::spawn_blocking(move || {
            write_all_at(&data_file, &data, offset)?;
            write_all_at(&hashes_file, &hash, index as u64 * 32)
        })
        .await
        .map_err(|e| WriteError::Io(e.to_string()))?
        .map_err(|e| WriteError::Io(e.to_string()))?;

        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        let newly_present = {
            let mut state = self.state.lock().unwrap();
            state.hashes[index as usize] = hash;
            state.hash_known.set(index);
            state.have.set(index)
        };
        self.changed();
        if newly_present && self.writes_since_persist.fetch_add(1, Ordering::Relaxed) + 1 >= PERSIST_EVERY_WRITES {
            self.persist().await;
        }
        Ok(())
    }

    // Upload path: the sender is the source of truth, so the hash is computed here.
    pub async fn write_chunk_hashing(&self, index: u32, data: Vec<u8>) -> Result<ChunkHash, WriteError> {
        self.check_chunk(index, data.len())?;
        let hash = hash_chunk(&data);
        self.write_at(index, data, hash).await?;
        Ok(hash)
    }

    // Download path: the chunk must match the hash the manifest promised.
    pub async fn write_chunk_verified(&self, index: u32, data: Vec<u8>) -> Result<(), WriteError> {
        self.check_chunk(index, data.len())?;
        let expected = {
            let state = self.state.lock().unwrap();
            if state.have.get(index) {
                return Ok(());
            }
            if !state.hash_known.get(index) {
                return Err(WriteError::NoManifest);
            }
            state.hashes[index as usize]
        };
        let hash = hash_chunk(&data);
        if hash != expected {
            return Err(WriteError::HashMismatch);
        }
        self.write_at(index, data, hash).await
    }

    fn check_chunk(&self, index: u32, len: usize) -> Result<(), WriteError> {
        if self.is_removed() {
            return Err(WriteError::Removed);
        }
        if index >= self.chunks {
            return Err(WriteError::BadIndex);
        }
        if len != chunk_len(self.size, index) {
            return Err(WriteError::BadLength);
        }
        Ok(())
    }

    // Accepts a manifest from another node if it matches the root the catalog promised.
    pub fn set_manifest(&self, hashes: Vec<ChunkHash>, root: &str) -> Result<(), &'static str> {
        if hashes.len() != self.chunks as usize {
            return Err("manifest has the wrong number of chunks");
        }
        if manifest_root(self.size, &hashes) != root {
            return Err("manifest does not match the catalog");
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.hash_known.is_full() {
                return Ok(());
            }
            for (index, hash) in hashes.iter().enumerate() {
                if !state.hash_known.get(index as u32) {
                    state.hashes[index] = *hash;
                    state.hash_known.set(index as u32);
                }
            }
            state.expected_root = Some(root.to_string());
        }
        let bytes: Vec<u8> = hashes.iter().flatten().copied().collect();
        write_all_at(&self.hashes_file, &bytes, 0).map_err(|_| "could not save the manifest")?;
        Ok(())
    }

    pub fn expect_root(&self, root: &str) {
        self.state.lock().unwrap().expected_root.get_or_insert_with(|| root.to_string());
    }

    // Once every chunk is present: fixes the manifest root and marks the file complete.
    pub fn seal(&self) -> Result<String, &'static str> {
        let mut state = self.state.lock().unwrap();
        if let Some(root) = &state.sealed {
            return Ok(root.clone());
        }
        if !state.have.is_full() || !state.hash_known.is_full() {
            return Err("file is not complete");
        }
        let root = manifest_root(self.size, &state.hashes);
        if state.expected_root.as_ref().is_some_and(|expected| *expected != root) {
            return Err("file does not match the catalog");
        }
        state.sealed = Some(root.clone());
        drop(state);
        self.changed();
        Ok(root)
    }

    pub async fn persist(&self) {
        self.writes_since_persist.store(0, Ordering::Relaxed);
        if self.is_removed() {
            return;
        }
        let meta = {
            let state = self.state.lock().unwrap();
            Meta {
                id: self.id.clone(),
                size: self.size,
                have: state.have.to_hex(),
                hash_known: state.hash_known.to_hex(),
                expected_root: state.expected_root.clone(),
                sealed: state.sealed.clone(),
                entry: state.entry.clone(),
            }
        };
        let (data, hashes, path) = (self.data.clone(), self.hashes_file.clone(), self.paths.meta.clone());
        let result = tokio::task::spawn_blocking(move || -> io::Result<()> {
            // Data first, so the saved progress never claims chunks that aren't on disk.
            data.sync_data()?;
            hashes.sync_data()?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&meta)?)?;
            std::fs::rename(&tmp, &path)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("Store: could not save progress for {}: {e}", self.id),
            Err(e) => tracing::warn!("Store: could not save progress for {}: {e}", self.id),
        }
    }
}

pub struct WriteGuard(Arc<Blob>);

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.0.writer.store(false, Ordering::Release);
    }
}

pub struct Store {
    root: PathBuf,
    quota: u64,
    blobs: Mutex<HashMap<String, Arc<Blob>>>,
}

impl Store {
    // Opens the store, picking up files (and partial files) from earlier runs.
    pub fn open(root: &Path, quota: u64) -> io::Result<Self> {
        create_private_dir(root)?;
        let store = Self { root: root.to_path_buf(), quota, blobs: Mutex::new(HashMap::new()) };
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match store.load(&path) {
                Ok(blob) => {
                    store.blobs.lock().unwrap().insert(blob.id.clone(), blob);
                }
                Err(e) => tracing::warn!("Store: skipping {}: {e}", path.display()),
            }
        }
        Ok(store)
    }

    fn load(&self, meta_path: &Path) -> io::Result<Arc<Blob>> {
        let meta: Meta = serde_json::from_slice(&std::fs::read(meta_path)?)?;
        if !validate::is_valid_id(&meta.id) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad id"));
        }
        let paths = Paths::new(&self.root, &meta.id);
        let data = OpenOptions::new().read(true).write(true).open(&paths.data)?;
        if data.metadata()?.len() != meta.size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "data file has the wrong size"));
        }
        let hashes_file = OpenOptions::new().read(true).write(true).open(&paths.hashes)?;
        let chunks = chunk_count(meta.size);
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad chunk map");
        let have = Bitmap::from_hex(chunks, &meta.have).ok_or_else(bad)?;
        let hash_known = Bitmap::from_hex(chunks, &meta.hash_known).ok_or_else(bad)?;

        let mut raw = vec![0u8; chunks as usize * 32];
        read_exact_at(&hashes_file, &mut raw, 0)?;
        let hashes = raw.chunks_exact(32).map(|c| <ChunkHash>::try_from(c).unwrap()).collect();

        Ok(Arc::new(Blob {
            id: meta.id,
            size: meta.size,
            chunks,
            paths,
            data: Arc::new(data),
            hashes_file: Arc::new(hashes_file),
            state: Mutex::new(State { have, hashes, hash_known, expected_root: meta.expected_root, sealed: meta.sealed, entry: meta.entry }),
            changed: Notify::new(),
            removed: AtomicBool::new(false),
            writer: AtomicBool::new(false),
            writes_since_persist: AtomicU32::new(0),
            last_activity_ms: AtomicU64::new(now_ms()),
        }))
    }

    pub fn get(&self, id: &str) -> Option<Arc<Blob>> {
        self.blobs.lock().unwrap().get(id).filter(|b| !b.is_removed()).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Blob>> {
        self.blobs.lock().unwrap().values().cloned().collect()
    }

    pub fn used_bytes(&self) -> u64 {
        self.blobs.lock().unwrap().values().map(|b| b.size).sum()
    }

    // The existing file with this id and size (to resume), or a new empty one.
    pub fn create(&self, id: &str, size: u64) -> Result<Arc<Blob>, StoreError> {
        if !validate::is_valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        if size > validate::MAX_SAFE_INTEGER {
            return Err(StoreError::QuotaExceeded);
        }
        let mut blobs = self.blobs.lock().unwrap();
        if let Some(existing) = blobs.get(id) {
            return if existing.size == size { Ok(existing.clone()) } else { Err(StoreError::SizeMismatch) };
        }
        let used: u64 = blobs.values().map(|b| b.size).sum();
        if used.saturating_add(size) > self.quota {
            return Err(StoreError::QuotaExceeded);
        }
        if fs4::available_space(&self.root).is_ok_and(|free| free < size.saturating_add(MIN_FREE_BYTES)) {
            return Err(StoreError::DiskFull);
        }

        let paths = Paths::new(&self.root, id);
        let chunks = chunk_count(size);
        let create = |path: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path);
        let data = create(&paths.data)?;
        data.set_len(size)?;
        let hashes_file = create(&paths.hashes)?;
        hashes_file.set_len(chunks as u64 * 32)?;

        let blob = Arc::new(Blob {
            id: id.to_string(),
            size,
            chunks,
            paths,
            data: Arc::new(data),
            hashes_file: Arc::new(hashes_file),
            state: Mutex::new(State {
                have: Bitmap::new(chunks),
                hashes: vec![[0; 32]; chunks as usize],
                hash_known: Bitmap::new(chunks),
                expected_root: None,
                sealed: None,
                entry: None,
            }),
            changed: Notify::new(),
            removed: AtomicBool::new(false),
            writer: AtomicBool::new(false),
            writes_since_persist: AtomicU32::new(0),
            last_activity_ms: AtomicU64::new(now_ms()),
        });
        blobs.insert(id.to_string(), blob.clone());
        Ok(blob)
    }

    // Deletes a file and wakes anything waiting on it.
    pub fn remove(&self, id: &str) {
        let blob = self.blobs.lock().unwrap().remove(id);
        if let Some(blob) = blob {
            blob.removed.store(true, Ordering::Relaxed);
            blob.changed();
            for path in [&blob.paths.data, &blob.paths.hashes, &blob.paths.meta] {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    pub fn contains_complete(&self, id: &str) -> bool {
        self.get(id).is_some_and(|b| b.is_complete())
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(quota: u64) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("files"), quota).unwrap();
        (dir, store)
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    // Uploads `content` chunk by chunk, the way the HTTP upload does.
    async fn upload(blob: &Blob, content: &[u8]) {
        for index in 0..blob.chunk_count() {
            let start = index as usize * CHUNK_SIZE as usize;
            let end = (start + CHUNK_SIZE as usize).min(content.len());
            blob.write_chunk_hashing(index, content[start..end].to_vec()).await.unwrap();
        }
    }

    #[test]
    fn chunk_arithmetic() {
        assert_eq!(chunk_count(0), 0);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_SIZE), 1);
        assert_eq!(chunk_count(CHUNK_SIZE + 1), 2);
        assert_eq!(chunk_len(CHUNK_SIZE + 5, 0), CHUNK_SIZE as usize);
        assert_eq!(chunk_len(CHUNK_SIZE + 5, 1), 5);
        assert_eq!(chunk_len(CHUNK_SIZE + 5, 2), 0);
    }

    #[test]
    fn the_manifest_root_depends_on_size_and_every_chunk() {
        let a = hash_chunk(b"a");
        let b = hash_chunk(b"b");
        let root = manifest_root(10, &[a, b]);
        assert_eq!(root.len(), 64);
        assert_ne!(root, manifest_root(11, &[a, b]));
        assert_ne!(root, manifest_root(10, &[b, a]));
        assert_ne!(root, manifest_root(10, &[a]));
        assert_eq!(root, manifest_root(10, &[a, b]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_uploaded_file_reads_back_and_seals() {
        let (_dir, store) = store(1 << 40);
        let content = pattern(CHUNK_SIZE as usize * 2 + 1234, 7);
        let blob = store.create("file_a", content.len() as u64).unwrap();
        assert!(!blob.is_complete());

        upload(&blob, &content).await;
        let root = blob.seal().unwrap();

        assert!(blob.is_complete());
        assert_eq!(blob.manifest_root(), Some(root.clone()));
        assert_eq!(blob.read_chunk(0).await.unwrap(), content[..CHUNK_SIZE as usize]);
        assert_eq!(blob.read_chunk(2).await.unwrap(), content[2 * CHUNK_SIZE as usize..]);
        assert_eq!(manifest_root(content.len() as u64, &blob.hashes().unwrap()), root);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_are_checked_for_index_and_length() {
        let (_dir, store) = store(1 << 40);
        let blob = store.create("file_a", CHUNK_SIZE + 10).unwrap();
        assert_eq!(blob.write_chunk_hashing(2, vec![0; 10]).await, Err(WriteError::BadIndex));
        assert_eq!(blob.write_chunk_hashing(0, vec![0; 10]).await, Err(WriteError::BadLength));
        assert_eq!(blob.write_chunk_hashing(1, vec![0; CHUNK_SIZE as usize]).await, Err(WriteError::BadLength));
        assert!(blob.write_chunk_hashing(1, vec![0; 10]).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_downloader_accepts_only_chunks_that_match_the_manifest() {
        let (_dir, origin) = store(1 << 40);
        let content = pattern(CHUNK_SIZE as usize + 100, 3);
        let source = origin.create("file_a", content.len() as u64).unwrap();
        upload(&source, &content).await;
        let root = source.seal().unwrap();

        let (_dir2, other) = store(1 << 40);
        let copy = other.create("file_a", content.len() as u64).unwrap();

        // Without a manifest nothing can be verified.
        assert_eq!(copy.write_chunk_verified(0, content[..CHUNK_SIZE as usize].to_vec()).await, Err(WriteError::NoManifest));

        // A manifest that doesn't match the catalog's root is refused.
        let mut forged = source.hashes().unwrap();
        forged[0][0] ^= 1;
        assert!(copy.set_manifest(forged, &root).is_err());
        assert!(copy.set_manifest(source.hashes().unwrap()[..1].to_vec(), &root).is_err());
        copy.set_manifest(source.hashes().unwrap(), &root).unwrap();

        let mut corrupted = content[..CHUNK_SIZE as usize].to_vec();
        corrupted[5] ^= 0xff;
        assert_eq!(copy.write_chunk_verified(0, corrupted).await, Err(WriteError::HashMismatch));
        assert!(!copy.has_chunk(0));

        copy.write_chunk_verified(0, content[..CHUNK_SIZE as usize].to_vec()).await.unwrap();
        assert!(copy.seal().is_err(), "not complete yet");
        copy.write_chunk_verified(1, content[CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        assert_eq!(copy.seal().unwrap(), root);
        assert_eq!(copy.read_chunk(1).await.unwrap(), content[CHUNK_SIZE as usize..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_does_not_match_the_expected_root_is_not_sealed() {
        let (_dir, store) = store(1 << 40);
        let blob = store.create("file_a", 10).unwrap();
        blob.expect_root(&"0".repeat(64));
        blob.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
        assert!(blob.seal().is_err());
        assert!(!blob.is_complete());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn progress_survives_a_restart_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 3, 9);
        {
            let store = Store::open(&root, 1 << 40).unwrap();
            let blob = store.create("file_a", content.len() as u64).unwrap();
            for index in 0..2u32 {
                let start = index as usize * CHUNK_SIZE as usize;
                blob.write_chunk_hashing(index, content[start..start + CHUNK_SIZE as usize].to_vec()).await.unwrap();
            }
            blob.persist().await;
        }

        let store = Store::open(&root, 1 << 40).unwrap();
        let blob = store.get("file_a").unwrap();
        assert_eq!(blob.leading_chunks(), 2);
        assert!(!blob.is_complete());
        assert_eq!(blob.read_chunk(1).await.unwrap(), content[CHUNK_SIZE as usize..2 * CHUNK_SIZE as usize]);

        blob.write_chunk_hashing(2, content[2 * CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        let root_hash = blob.seal().unwrap();
        blob.persist().await;

        let store = Store::open(&root, 1 << 40).unwrap();
        let blob = store.get("file_a").unwrap();
        assert!(blob.is_complete());
        assert_eq!(blob.manifest_root(), Some(root_hash));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn creating_an_existing_file_resumes_it_but_not_with_another_size() {
        let (_dir, store) = store(1 << 40);
        let first = store.create("file_a", 100).unwrap();
        first.write_chunk_hashing(0, vec![1; 100]).await.unwrap();
        let again = store.create("file_a", 100).unwrap();
        assert!(again.has_chunk(0));
        assert_eq!(store.create("file_a", 200).err(), Some(StoreError::SizeMismatch));
    }

    #[test]
    fn the_quota_and_ids_are_enforced() {
        let (_dir, store) = store(1000);
        assert!(store.create("file_a", 600).is_ok());
        assert_eq!(store.create("file_b", 600).err(), Some(StoreError::QuotaExceeded));
        assert!(store.create("file_c", 400).is_ok());
        assert_eq!(store.used_bytes(), 1000);
        assert_eq!(store.create("../evil", 1).err(), Some(StoreError::InvalidId));
        assert_eq!(store.create("a/b", 1).err(), Some(StoreError::InvalidId));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_file_deletes_it_and_releases_its_quota() {
        let (dir, store) = store(1000);
        let blob = store.create("file_a", 600).unwrap();
        blob.write_chunk_hashing(0, vec![1; 600]).await.unwrap();
        blob.persist().await;
        assert!(dir.path().join("files/file_a.data").exists());

        store.remove("file_a");
        assert!(blob.is_removed());
        assert!(store.get("file_a").is_none());
        assert!(!dir.path().join("files/file_a.data").exists());
        assert!(!dir.path().join("files/file_a.json").exists());
        assert!(store.create("file_b", 900).is_ok());
        assert_eq!(blob.write_chunk_hashing(0, vec![1; 600]).await, Err(WriteError::Removed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn waiting_for_a_chunk_wakes_when_it_arrives_and_times_out_otherwise() {
        let (_dir, store) = store(1 << 40);
        let blob = store.create("file_a", 10).unwrap();
        assert!(!blob.wait_for_chunk(0, Duration::from_millis(50)).await);

        let waiter = {
            let blob = blob.clone();
            tokio::spawn(async move { blob.wait_for_chunk(0, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        blob.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
        assert!(waiter.await.unwrap());

        // Removal releases waiters at once instead of leaving them to time out.
        let other = store.create("file_b", 10).unwrap();
        let waiter = {
            let other = other.clone();
            tokio::spawn(async move { other.wait_for_chunk(0, Duration::from_secs(30)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.remove("file_b");
        assert!(!tokio::time::timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_file_seals_immediately() {
        let (_dir, store) = store(1000);
        let blob = store.create("file_empty", 0).unwrap();
        assert_eq!(blob.chunk_count(), 0);
        assert_eq!(blob.seal().unwrap(), manifest_root(0, &[]));
    }

    #[cfg(unix)]
    #[test]
    fn the_store_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _store) = store(1000);
        assert_eq!(std::fs::metadata(dir.path().join("files")).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chunk_damaged_on_disk_is_never_served_and_can_be_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("files"), 1 << 40).unwrap();
        let content = pattern(CHUNK_SIZE as usize * 2, 4);
        let blob = store.create("file_a", content.len() as u64).unwrap();
        upload(&blob, &content).await;
        blob.seal().unwrap();
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), content[CHUNK_SIZE as usize..]);

        // Bit rot: one byte changes in the second chunk.
        let path = dir.path().join("files/file_a.data");
        let mut raw = std::fs::read(&path).unwrap();
        raw[CHUNK_SIZE as usize + 9] ^= 0xff;
        std::fs::write(&path, raw).unwrap();

        assert_eq!(blob.read_chunk_verified(0).await.unwrap(), content[..CHUNK_SIZE as usize], "intact chunks are unaffected");
        assert!(blob.read_chunk_verified(1).await.is_err(), "the damaged chunk is refused");
        assert!(!blob.has_chunk(1) && !blob.is_complete(), "and the file no longer counts as complete");
        assert!(blob.has_chunk(0));

        // Fetching a good copy of just that chunk repairs the file.
        blob.write_chunk_verified(1, content[CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        assert_eq!(blob.seal().unwrap(), manifest_root(content.len() as u64, &blob.hashes().unwrap()));
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), content[CHUNK_SIZE as usize..]);
    }
}
