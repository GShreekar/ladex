//! On-disk storage for the files this node holds: 1 MiB chunks, each checked against its BLAKE3 hash before it is written.
//!
//! Under the store root: `partial/<file_id>.part` (still arriving), `blobs/<file_id>` (complete), `hashes/<file_id>`
//! (32 bytes per chunk) and `store.redb` (one record per file). A file moves from `partial/` to `blobs/` only once every
//! chunk is verified and synced.

mod evict;
mod index;
mod legacy;
mod recover;
mod rehash;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use self::index::{Index, Record};
use crate::bitmap::Bitmap;
use crate::manifest::root_of;
use crate::validate;

pub use self::index::{LegacyEntry, Origin};
pub use crate::manifest::{hash_chunk, root_of as manifest_root, ChunkHash};

pub const CHUNK_SIZE: u64 = crate::manifest::CHUNK_SIZE as u64;
// Always leave this much free on the disk, whatever the quota says.
const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;
const PERSIST_EVERY_WRITES: u32 = 64;

pub fn chunk_count(size: u64) -> u32 {
    size.div_ceil(CHUNK_SIZE) as u32
}

pub fn chunk_len(size: u64, index: u32) -> usize {
    let start = index as u64 * CHUNK_SIZE;
    size.saturating_sub(start).min(CHUNK_SIZE) as usize
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

// ENOSPC and the Windows equivalents
pub(crate) fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(28) | Some(112) | Some(39))
}

/// Whether writing `size` more bytes under `dir` would eat into the free space always kept on its disk.
pub(crate) fn lacks_free_space(dir: &Path, size: u64) -> bool {
    fs4::available_space(dir).is_ok_and(|free| free < size.saturating_add(MIN_FREE_BYTES))
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        if is_disk_full(&e) {
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

// Hashing a 1 MiB chunk is real work, so it runs on the blocking pool and the network tasks never wait for it.
async fn hash_off_thread(data: Vec<u8>) -> Result<(Vec<u8>, ChunkHash), WriteError> {
    tokio::task::spawn_blocking(move || {
        let hash = hash_chunk(&data);
        (data, hash)
    })
    .await
    .map_err(|e| WriteError::Io(e.to_string()))
}

struct State {
    have: Bitmap,
    hashes: Vec<ChunkHash>,
    hash_known: Bitmap,
    expected_root: Option<String>,
    sealed: Option<String>,
    // Saved by an earlier version; handed over once, then no longer kept.
    legacy_entry: Option<LegacyEntry>,
}

pub struct Blob {
    id: String,
    size: u64,
    chunks: u32,
    data: Arc<File>,
    staged_path: PathBuf,
    final_path: PathBuf,
    // Whether the data is still under `partial/`, waiting to be moved into place.
    is_staged: AtomicBool,
    // Held while saving, so an older record is never written over a newer one.
    saving: tokio::sync::Mutex<()>,
    hashes_file: Arc<File>,
    state: Mutex<State>,
    changed: Notify,
    removed: AtomicBool,
    writer: AtomicBool,
    writes_since_persist: AtomicU32,
    last_activity_ms: AtomicU64,
    index: Arc<Index>,
    origin: Origin,
    pinned: AtomicBool,
    created_ms: u64,
    last_access_ms: AtomicU64,
    access_unsaved: AtomicBool,
}

fn now_ms() -> u64 {
    crate::hlc::wall_clock_ms()
}

struct Layout {
    root: PathBuf,
    partial: PathBuf,
    blobs: PathBuf,
    hashes: PathBuf,
}

impl Layout {
    fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf(), partial: root.join("partial"), blobs: root.join("blobs"), hashes: root.join("hashes") }
    }

    fn staged(&self, id: &str) -> PathBuf {
        self.partial.join(format!("{id}.part"))
    }

    fn data(&self, id: &str) -> PathBuf {
        self.blobs.join(id)
    }

    fn hashes(&self, id: &str) -> PathBuf {
        self.hashes.join(id)
    }

    fn index(&self) -> PathBuf {
        self.root.join("store.redb")
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

    pub fn origin(&self) -> Origin {
        self.origin
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned.load(Ordering::Relaxed)
    }

    /// A pinned file is never evicted, whoever published it.
    pub async fn set_pinned(&self, pinned: bool) {
        self.pinned.store(pinned, Ordering::Relaxed);
        self.persist().await;
    }

    /// Complete, fetched from others, not pinned and not being written: safe to evict.
    pub fn is_evictable(&self) -> bool {
        self.origin == Origin::Cached && !self.is_pinned() && !self.is_being_written() && self.is_complete()
    }

    fn touch(&self) {
        self.last_access_ms.store(now_ms(), Ordering::Relaxed);
        self.access_unsaved.store(true, Ordering::Relaxed);
    }

    fn record(&self) -> Record {
        let state = self.state.lock().unwrap();
        Record {
            size: self.size,
            have: state.have.to_hex(),
            hash_known: state.hash_known.to_hex(),
            expected_root: state.expected_root.clone(),
            sealed: state.sealed.clone(),
            entry: state.legacy_entry.clone(),
            origin: self.origin,
            pinned: self.is_pinned(),
            created_ms: self.created_ms,
            last_access_ms: self.last_access_ms.load(Ordering::Relaxed),
        }
    }

    pub fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Relaxed)
    }

    fn data_path(&self) -> &Path {
        if self.is_staged.load(Ordering::Acquire) {
            &self.staged_path
        } else {
            &self.final_path
        }
    }

    /// Held while an upload is writing this file; None if another one is.
    pub fn try_write_lock(self: &Arc<Self>) -> Option<WriteGuard> {
        self.writer.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok()?;
        Some(WriteGuard(self.clone()))
    }

    pub fn is_being_written(&self) -> bool {
        self.writer.load(Ordering::Acquire)
    }

    /// How long since a chunk was last written (or the file opened).
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

    /// The chunk hashes, once every one is known.
    pub fn hashes(&self) -> Option<Vec<ChunkHash>> {
        let state = self.state.lock().unwrap();
        state.hash_known.is_full().then(|| state.hashes.clone())
    }

    /// The entry an earlier version saved with this file, once; the next save forgets it.
    pub fn take_legacy_entry(&self) -> Option<LegacyEntry> {
        self.state.lock().unwrap().legacy_entry.take()
    }

    /// How many chunks from the start are all present: where an interrupted upload resumes.
    pub fn leading_chunks(&self) -> u32 {
        self.state.lock().unwrap().have.leading_ones()
    }

    fn changed(&self) {
        self.changed.notify_waiters();
    }

    /// Waits until `index` is present; false on timeout or if the file is removed.
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
        self.touch();
        let (file, offset, len) = (self.data.clone(), index as u64 * CHUNK_SIZE, chunk_len(self.size, index));
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            read_exact_at(&file, &mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(|e| io::Error::other(e.to_string()))?
    }

    /// Reads a chunk after checking it against its hash; a damaged chunk is dropped so it gets fetched again.
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

    /// Forgets a damaged chunk; the file is no longer complete.
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
        self.touch();
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

    /// Upload path: the sender is the source of truth, so the hash is computed here.
    pub async fn write_chunk_hashing(&self, index: u32, data: Vec<u8>) -> Result<ChunkHash, WriteError> {
        self.check_chunk(index, data.len())?;
        let (data, hash) = hash_off_thread(data).await?;
        self.write_at(index, data, hash).await?;
        Ok(hash)
    }

    /// Download path: the chunk must match the hash the manifest promised.
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
        let (data, hash) = hash_off_thread(data).await?;
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

    /// Accepts a manifest from another node if it matches the root the catalog promised.
    pub fn set_manifest(&self, hashes: Vec<ChunkHash>, root: &str) -> Result<(), &'static str> {
        if hashes.len() != self.chunks as usize {
            return Err("manifest has the wrong number of chunks");
        }
        if root_of(self.size, &hashes) != root {
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

    /// Once every chunk is present, moves the file into place and marks it complete; a crash leaves it either partial or complete.
    pub async fn seal(&self) -> Result<String, &'static str> {
        let _saving = self.saving.lock().await;
        if let Some(root) = self.manifest_root() {
            return Ok(root);
        }
        if self.is_removed() {
            return Err("file was removed");
        }
        let root = self.complete_root()?;
        let mut record = self.record();
        record.sealed = Some(root.clone());
        if let Err(e) = self.finalize(record).await {
            tracing::warn!("Store: could not finish {}: {e}", self.id);
            return Err("could not save the file");
        }
        {
            let mut state = self.state.lock().unwrap();
            // A chunk found damaged while the file was being saved must be fetched again first.
            if !state.have.is_full() {
                return Err("file is not complete");
            }
            state.sealed = Some(root.clone());
        }
        self.changed();
        Ok(root)
    }

    fn complete_root(&self) -> Result<String, &'static str> {
        let state = self.state.lock().unwrap();
        if !state.have.is_full() || !state.hash_known.is_full() {
            return Err("file is not complete");
        }
        let root = root_of(self.size, &state.hashes);
        if state.expected_root.as_ref().is_some_and(|expected| *expected != root) {
            return Err("file does not match the catalog");
        }
        Ok(root)
    }

    // Renamed only after the data is synced, and recorded complete only after the rename.
    async fn finalize(&self, record: Record) -> anyhow::Result<()> {
        if self.is_staged.load(Ordering::Acquire) {
            let (data, hashes) = (self.data.clone(), self.hashes_file.clone());
            let (from, to) = (self.staged_path.clone(), self.final_path.clone());
            tokio::task::spawn_blocking(move || -> io::Result<()> {
                data.sync_all()?;
                hashes.sync_all()?;
                move_into_place(&from, &to)
            })
            .await??;
            self.is_staged.store(false, Ordering::Release);
        }
        self.save(record).await
    }

    pub async fn persist(&self) {
        let _saving = self.saving.lock().await;
        self.writes_since_persist.store(0, Ordering::Relaxed);
        if self.is_removed() {
            return;
        }
        self.access_unsaved.store(false, Ordering::Relaxed);
        if let Err(e) = self.save(self.record()).await {
            tracing::warn!("Store: could not save progress for {}: {e}", self.id);
        }
    }

    async fn save(&self, record: Record) -> anyhow::Result<()> {
        let (index, id) = (self.index.clone(), self.id.clone());
        let (data, hashes) = (self.data.clone(), self.hashes_file.clone());
        tokio::task::spawn_blocking(move || {
            // Data first, so the saved progress never claims chunks that aren't on disk.
            data.sync_data()?;
            hashes.sync_data()?;
            index.put(&id, &record)
        })
        .await?
    }
}

// The rename is what completes a file, so both directories are synced to make it survive a power cut.
fn move_into_place(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)?;
    for dir in [from.parent(), to.parent()].into_iter().flatten() {
        sync_dir(dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

// Windows cannot open a directory to sync it; NTFS journals the rename itself.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

pub struct WriteGuard(Arc<Blob>);

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.0.writer.store(false, Ordering::Release);
    }
}

struct Header {
    size: u64,
    origin: Origin,
    pinned: bool,
    created_ms: u64,
    last_access_ms: u64,
}

struct OpenFiles {
    data: File,
    hashes_file: File,
    is_staged: bool,
}

struct Registry {
    blobs: HashMap<String, Arc<Blob>>,
    // Bytes reserved by every file, complete or not: files are preallocated at their full size.
    used: u64,
}

pub struct Store {
    layout: Layout,
    quota: u64,
    index: Arc<Index>,
    registry: Mutex<Registry>,
}

impl Store {
    /// Opens the store, picking up files and partial files from earlier runs.
    pub fn open(root: &Path, quota: u64) -> io::Result<Self> {
        let layout = Layout::new(root);
        for dir in [root, layout.partial.as_path(), layout.blobs.as_path(), layout.hashes.as_path()] {
            create_private_dir(dir)?;
        }
        let index = Arc::new(Index::open(&layout.index()).map_err(io::Error::other)?);
        let migrated = legacy::migrate(root, &layout, &index, now_ms())?;
        if migrated > 0 {
            tracing::info!("Store: moved {migrated} files into the new layout");
        }
        rehash::migrate(&layout, &index)?;

        let store = Self { layout, quota, index, registry: Mutex::new(Registry { blobs: HashMap::new(), used: 0 }) };
        store.load_records()?;
        store.remove_unrecorded_files()?;
        Ok(store)
    }

    fn load_records(&self) -> io::Result<()> {
        let mut registry = self.registry.lock().unwrap();
        for (id, record) in self.index.load_all().map_err(io::Error::other)? {
            match self.load(&id, record) {
                Ok(blob) => {
                    registry.used += blob.size;
                    registry.blobs.insert(id, blob);
                }
                Err(e) => {
                    tracing::warn!("Store: dropping {id}: {e}");
                    self.delete_files(&id);
                    let _ = self.index.remove(&id);
                }
            }
        }
        Ok(())
    }

    // Files left by a crash between writing them and recording them would otherwise count against the disk forever.
    fn remove_unrecorded_files(&self) -> io::Result<()> {
        let registry = self.registry.lock().unwrap();
        let recorded: HashSet<PathBuf> =
            registry.blobs.values().flat_map(|blob| [blob.data_path().to_path_buf(), self.layout.hashes(&blob.id)]).collect();
        for dir in [&self.layout.partial, &self.layout.blobs, &self.layout.hashes] {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if !recorded.contains(&path) {
                    tracing::warn!("Store: removing {} (not in the index)", path.display());
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(())
    }

    fn load(&self, id: &str, record: Record) -> io::Result<Arc<Blob>> {
        if !validate::is_valid_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad id"));
        }
        let is_staged = record.sealed.is_none();
        if is_staged {
            self.stage_unfinished(id)?;
        }
        let data_path = if is_staged { self.layout.staged(id) } else { self.layout.data(id) };
        let data = OpenOptions::new().read(true).write(true).open(data_path)?;
        if data.metadata()?.len() != record.size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "data file has the wrong size"));
        }
        let hashes_file = OpenOptions::new().read(true).write(true).open(self.layout.hashes(id))?;
        let chunks = chunk_count(record.size);
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad chunk map");
        let have = Bitmap::from_hex(chunks, &record.have).ok_or_else(bad)?;
        let hash_known = Bitmap::from_hex(chunks, &record.hash_known).ok_or_else(bad)?;

        let mut raw = vec![0u8; chunks as usize * 32];
        read_exact_at(&hashes_file, &mut raw, 0)?;
        let hashes = raw.as_chunks::<32>().0.to_vec();

        let header = Header {
            size: record.size,
            origin: record.origin,
            pinned: record.pinned,
            created_ms: record.created_ms,
            last_access_ms: record.last_access_ms,
        };
        let state =
            State { have, hashes, hash_known, expected_root: record.expected_root, sealed: record.sealed, legacy_entry: record.entry };
        let blob = self.new_blob(id, header, OpenFiles { data, hashes_file, is_staged }, state);
        if is_staged {
            self.resume(&blob)?;
        }
        Ok(blob)
    }

    // An unfinished file found in `blobs/` was caught between its rename and its record, or kept there by an older version.
    fn stage_unfinished(&self, id: &str) -> io::Result<()> {
        let (staged, finished) = (self.layout.staged(id), self.layout.data(id));
        if !staged.exists() && finished.exists() {
            std::fs::rename(finished, staged)?;
        }
        Ok(())
    }

    // Picks up what a crash cut short: chunks written after the last save, and a download with every chunk not yet moved into place.
    fn resume(&self, blob: &Blob) -> io::Result<()> {
        let recovered = recover::recover_chunks(&blob.data, blob.size, &mut blob.state.lock().unwrap())?;
        // Only a download is finished here: an upload is listed by the request that completes it.
        let finished_root = blob.expected_root().and_then(|_| blob.complete_root().ok());
        if recovered == 0 && finished_root.is_none() {
            return Ok(());
        }
        if recovered > 0 {
            tracing::info!("Store: found {recovered} chunks of {} on disk that were written but not yet recorded", blob.id);
        }
        blob.data.sync_all()?;
        blob.hashes_file.sync_all()?;
        if let Some(root) = finished_root {
            move_into_place(&blob.staged_path, &blob.final_path)?;
            blob.is_staged.store(false, Ordering::Release);
            blob.state.lock().unwrap().sealed = Some(root);
        }
        self.index.put(&blob.id, &blob.record()).map_err(io::Error::other)
    }

    fn new_blob(&self, id: &str, header: Header, files: OpenFiles, state: State) -> Arc<Blob> {
        Arc::new(Blob {
            id: id.to_string(),
            size: header.size,
            chunks: chunk_count(header.size),
            data: Arc::new(files.data),
            staged_path: self.layout.staged(id),
            final_path: self.layout.data(id),
            is_staged: AtomicBool::new(files.is_staged),
            saving: tokio::sync::Mutex::new(()),
            hashes_file: Arc::new(files.hashes_file),
            state: Mutex::new(state),
            changed: Notify::new(),
            removed: AtomicBool::new(false),
            writer: AtomicBool::new(false),
            writes_since_persist: AtomicU32::new(0),
            last_activity_ms: AtomicU64::new(now_ms()),
            index: self.index.clone(),
            origin: header.origin,
            pinned: AtomicBool::new(header.pinned),
            created_ms: header.created_ms,
            last_access_ms: AtomicU64::new(header.last_access_ms),
            access_unsaved: AtomicBool::new(false),
        })
    }

    pub fn get(&self, id: &str) -> Option<Arc<Blob>> {
        self.registry.lock().unwrap().blobs.get(id).filter(|b| !b.is_removed()).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Blob>> {
        self.registry.lock().unwrap().blobs.values().cloned().collect()
    }

    pub fn used_bytes(&self) -> u64 {
        self.registry.lock().unwrap().used
    }

    pub fn quota(&self) -> u64 {
        self.quota
    }

    /// The existing file with this id and size (to resume), or a new empty one this node published.
    pub fn create(&self, id: &str, size: u64) -> Result<Arc<Blob>, StoreError> {
        self.create_with(id, size, Origin::Published)
    }

    pub fn create_with(&self, id: &str, size: u64, origin: Origin) -> Result<Arc<Blob>, StoreError> {
        if !validate::is_valid_id(id) {
            return Err(StoreError::InvalidId);
        }
        if size > validate::MAX_SAFE_INTEGER {
            return Err(StoreError::QuotaExceeded);
        }
        let mut registry = self.registry.lock().unwrap();
        if let Some(existing) = registry.blobs.get(id) {
            return if existing.size == size { Ok(existing.clone()) } else { Err(StoreError::SizeMismatch) };
        }
        if registry.used.saturating_add(size) > self.quota {
            return Err(StoreError::QuotaExceeded);
        }
        if lacks_free_space(&self.layout.root, size) {
            return Err(StoreError::DiskFull);
        }

        let chunks = chunk_count(size);
        let create = |path: &Path| OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path);
        let data = create(&self.layout.staged(id))?;
        data.set_len(size)?;
        let hashes_file = create(&self.layout.hashes(id))?;
        hashes_file.set_len(chunks as u64 * 32)?;

        let state = State {
            have: Bitmap::new(chunks),
            hashes: vec![[0; 32]; chunks as usize],
            hash_known: Bitmap::new(chunks),
            expected_root: None,
            sealed: None,
            legacy_entry: None,
        };
        let now = now_ms();
        let header = Header { size, origin, pinned: false, created_ms: now, last_access_ms: now };
        let blob = self.new_blob(id, header, OpenFiles { data, hashes_file, is_staged: true }, state);
        if let Err(e) = self.index.put(id, &blob.record()) {
            self.delete_files(id);
            return Err(StoreError::Io(e.to_string()));
        }
        registry.used += size;
        registry.blobs.insert(id.to_string(), blob.clone());
        Ok(blob)
    }

    /// Deletes a file and wakes anything waiting on it.
    pub fn remove(&self, id: &str) {
        let blob = {
            let mut registry = self.registry.lock().unwrap();
            self.detach(&mut registry, id)
        };
        if let Some(blob) = blob {
            self.discard(&blob);
        }
    }

    fn detach(&self, registry: &mut Registry, id: &str) -> Option<Arc<Blob>> {
        let blob = registry.blobs.remove(id)?;
        registry.used = registry.used.saturating_sub(blob.size);
        Some(blob)
    }

    fn discard(&self, blob: &Blob) {
        blob.removed.store(true, Ordering::Relaxed);
        blob.changed();
        self.delete_files(&blob.id);
        if let Err(e) = self.index.remove(&blob.id) {
            tracing::warn!("Store: could not forget {}: {e}", blob.id);
        }
    }

    // The staged copy goes first, so a file being moved into place at this moment is still caught.
    fn delete_files(&self, id: &str) {
        for path in [self.layout.staged(id), self.layout.data(id), self.layout.hashes(id)] {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Evicts least-recently-used cached files until `extra` more bytes fit in the quota; returns the evicted ids.
    pub fn evict_to_fit(&self, extra: u64) -> Vec<String> {
        let evicted: Vec<Arc<Blob>> = {
            let mut registry = self.registry.lock().unwrap();
            let candidates = registry
                .blobs
                .values()
                .filter(|blob| blob.is_evictable())
                .map(|blob| evict::Candidate {
                    id: blob.id.clone(),
                    size: blob.size,
                    last_access_ms: blob.last_access_ms.load(Ordering::Relaxed),
                })
                .collect();
            let victims = evict::pick_victims(candidates, registry.used, self.quota.saturating_sub(extra));
            victims.iter().filter_map(|id| self.detach(&mut registry, id)).collect()
        };
        for blob in &evicted {
            tracing::info!("Store: evicting {} ({} bytes, least recently used)", blob.id, blob.size);
            self.discard(blob);
        }
        evicted.iter().map(|blob| blob.id.clone()).collect()
    }

    /// Saves the last-access times that reads have changed since the last save, so eviction order survives a restart.
    pub async fn save_access_times(&self) {
        let records: Vec<(String, Record)> = self
            .all()
            .into_iter()
            .filter(|blob| !blob.is_removed() && blob.access_unsaved.swap(false, Ordering::Relaxed))
            .map(|blob| (blob.id.clone(), blob.record()))
            .collect();
        if records.is_empty() {
            return;
        }
        let index = self.index.clone();
        match tokio::task::spawn_blocking(move || index.put_all(&records)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("Store: could not save access times: {e}"),
            Err(e) => tracing::warn!("Store: could not save access times: {e}"),
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
        let root = blob.seal().await.unwrap();

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
        let root = source.seal().await.unwrap();

        let (_dir2, other) = store(1 << 40);
        let copy = other.create("file_a", content.len() as u64).unwrap();

        assert_eq!(copy.write_chunk_verified(0, content[..CHUNK_SIZE as usize].to_vec()).await, Err(WriteError::NoManifest));

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
        assert!(copy.seal().await.is_err(), "not complete yet");
        copy.write_chunk_verified(1, content[CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        assert_eq!(copy.seal().await.unwrap(), root);
        assert_eq!(copy.read_chunk(1).await.unwrap(), content[CHUNK_SIZE as usize..]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_does_not_match_the_expected_root_is_not_sealed() {
        let (_dir, store) = store(1 << 40);
        let blob = store.create("file_a", 10).unwrap();
        blob.expect_root(&"0".repeat(64));
        blob.write_chunk_hashing(0, vec![1; 10]).await.unwrap();
        assert!(blob.seal().await.is_err());
        assert!(!blob.is_complete());
    }

    fn reopen(root: &Path) -> (Store, Arc<Blob>) {
        let store = Store::open(root, 1 << 40).unwrap();
        let blob = store.get("file_a").unwrap();
        (store, blob)
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

        let (store, blob) = reopen(&root);
        assert_eq!(blob.leading_chunks(), 2);
        assert!(!blob.is_complete());
        assert_eq!(blob.read_chunk(1).await.unwrap(), content[CHUNK_SIZE as usize..2 * CHUNK_SIZE as usize]);

        blob.write_chunk_hashing(2, content[2 * CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        let root_hash = blob.seal().await.unwrap();
        blob.persist().await;
        drop((store, blob));

        let (_store, blob) = reopen(&root);
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
        assert!(dir.path().join("files/partial/file_a.part").exists());

        store.remove("file_a");
        assert!(blob.is_removed());
        assert!(store.get("file_a").is_none());
        assert!(!dir.path().join("files/partial/file_a.part").exists());
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
        assert_eq!(blob.seal().await.unwrap(), manifest_root(0, &[]));
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
        blob.seal().await.unwrap();
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), content[CHUNK_SIZE as usize..]);

        let path = dir.path().join("files/blobs/file_a");
        let mut raw = std::fs::read(&path).unwrap();
        raw[CHUNK_SIZE as usize + 9] ^= 0xff;
        std::fs::write(&path, raw).unwrap();

        assert_eq!(blob.read_chunk_verified(0).await.unwrap(), content[..CHUNK_SIZE as usize], "intact chunks are unaffected");
        assert!(blob.read_chunk_verified(1).await.is_err(), "the damaged chunk is refused");
        assert!(!blob.has_chunk(1) && !blob.is_complete(), "and the file no longer counts as complete");
        assert!(blob.has_chunk(0));

        blob.write_chunk_verified(1, content[CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        assert_eq!(blob.seal().await.unwrap(), manifest_root(content.len() as u64, &blob.hashes().unwrap()));
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), content[CHUNK_SIZE as usize..]);
    }

    async fn complete(store: &Store, id: &str, len: usize, origin: Origin) -> Arc<Blob> {
        let blob = store.create_with(id, len as u64, origin).unwrap();
        blob.write_chunk_hashing(0, vec![1; len]).await.unwrap();
        blob.seal().await.unwrap();
        tokio::time::sleep(Duration::from_millis(3)).await;
        blob
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_live_under_blobs_and_hashes_with_a_redb_index() {
        let (dir, store) = store(1000);
        complete(&store, "file_a", 10, Origin::Published).await;
        let root = dir.path().join("files");
        assert_eq!(std::fs::metadata(root.join("blobs/file_a")).unwrap().len(), 10);
        assert_eq!(std::fs::metadata(root.join("hashes/file_a")).unwrap().len(), 32);
        assert!(root.join("store.redb").is_file());

        store.remove("file_a");
        assert!(!root.join("blobs/file_a").exists() && !root.join("hashes/file_a").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_stays_in_partial_until_it_is_sealed() {
        let (dir, store) = store(1 << 40);
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize + 10, 1);
        let blob = store.create("file_a", content.len() as u64).unwrap();
        blob.write_chunk_hashing(0, content[..CHUNK_SIZE as usize].to_vec()).await.unwrap();
        assert!(root.join("partial/file_a.part").exists() && !root.join("blobs/file_a").exists());

        blob.write_chunk_hashing(1, content[CHUNK_SIZE as usize..].to_vec()).await.unwrap();
        assert!(!root.join("blobs/file_a").exists(), "every chunk is there, but the file is not sealed yet");

        blob.seal().await.unwrap();
        assert!(!root.join("partial/file_a.part").exists());
        assert_eq!(std::fs::read(root.join("blobs/file_a")).unwrap(), content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sealed_file_is_complete_after_a_restart_without_another_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 2, 2);
        let sealed_root = {
            let store = Store::open(&root, 1 << 40).unwrap();
            let blob = store.create("file_a", content.len() as u64).unwrap();
            upload(&blob, &content).await;
            blob.seal().await.unwrap()
        };

        let (_store, blob) = reopen(&root);
        assert_eq!(blob.manifest_root(), Some(sealed_root));
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), content[CHUNK_SIZE as usize..]);
    }

    // What a download looks like when the node is killed: the manifest was saved, the chunks written after it were not.
    async fn interrupted_download(root: &Path, content: &[u8], written: std::ops::Range<u32>) {
        let (_dir, origin) = store(1 << 40);
        let source = origin.create("file_a", content.len() as u64).unwrap();
        upload(&source, content).await;
        let manifest_root = source.seal().await.unwrap();

        let store = Store::open(root, 1 << 40).unwrap();
        let blob = store.create_with("file_a", content.len() as u64, Origin::Cached).unwrap();
        blob.set_manifest(source.hashes().unwrap(), &manifest_root).unwrap();
        blob.persist().await;
        for index in written {
            let start = index as usize * CHUNK_SIZE as usize;
            let end = (start + CHUNK_SIZE as usize).min(content.len());
            blob.write_chunk_verified(index, content[start..end].to_vec()).await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_written_but_not_saved_before_a_crash_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 4, 3);
        interrupted_download(&root, &content, 1..3).await;

        let (_store, blob) = reopen(&root);
        assert!(!blob.has_chunk(0) && blob.has_chunk(1) && blob.has_chunk(2) && !blob.has_chunk(3));
        assert!(!blob.is_complete());
        assert_eq!(blob.read_chunk_verified(2).await.unwrap(), content[2 * CHUNK_SIZE as usize..3 * CHUNK_SIZE as usize]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chunk_torn_by_a_crash_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 3, 4);
        interrupted_download(&root, &content, 0..2).await;
        let path = root.join("partial/file_a.part");
        let mut raw = std::fs::read(&path).unwrap();
        raw[CHUNK_SIZE as usize + 9] ^= 0xff;
        std::fs::write(&path, raw).unwrap();

        let (_store, blob) = reopen(&root);
        assert!(blob.has_chunk(0) && !blob.has_chunk(1));
        blob.write_chunk_verified(1, content[CHUNK_SIZE as usize..2 * CHUNK_SIZE as usize].to_vec()).await.unwrap();
        assert!(blob.has_chunk(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_recovered_after_a_crash_are_recorded_for_the_next_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 3, 5);
        interrupted_download(&root, &content, 0..2).await;
        drop(reopen(&root));
        // Damage that only a second look at the data would notice.
        std::fs::write(root.join("partial/file_a.part"), vec![0u8; content.len()]).unwrap();

        let (_store, blob) = reopen(&root);
        assert_eq!(blob.bitmap().count(), 2, "the recovered chunks were saved, not found again");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_that_had_every_chunk_is_finished_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 2 + 5, 6);
        interrupted_download(&root, &content, 0..3).await;

        let (_store, blob) = reopen(&root);
        assert!(blob.is_complete());
        assert_eq!(std::fs::read(root.join("blobs/file_a")).unwrap(), content);
        assert!(!root.join("partial/file_a.part").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_killed_between_its_rename_and_its_record_is_still_complete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 2, 7);
        interrupted_download(&root, &content, 0..2).await;
        std::fs::rename(root.join("partial/file_a.part"), root.join("blobs/file_a")).unwrap();

        let (_store, blob) = reopen(&root);
        assert!(blob.is_complete());
        assert_eq!(std::fs::read(root.join("blobs/file_a")).unwrap(), content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unfinished_file_found_in_blobs_goes_back_to_partial() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 2, 8);
        {
            let store = Store::open(&root, 1 << 40).unwrap();
            let blob = store.create("file_a", content.len() as u64).unwrap();
            blob.write_chunk_hashing(0, content[..CHUNK_SIZE as usize].to_vec()).await.unwrap();
            blob.persist().await;
        }
        std::fs::rename(root.join("partial/file_a.part"), root.join("blobs/file_a")).unwrap();

        let (_store, blob) = reopen(&root);
        assert!(!blob.is_complete() && blob.has_chunk(0) && !blob.has_chunk(1));
        assert!(root.join("partial/file_a.part").exists() && !root.join("blobs/file_a").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_upload_with_every_chunk_waits_for_its_request_to_finish_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let content = pattern(CHUNK_SIZE as usize * 2, 9);
        {
            let store = Store::open(&root, 1 << 40).unwrap();
            upload(&store.create("file_a", content.len() as u64).unwrap(), &content).await;
        }

        let (_store, blob) = reopen(&root);
        assert!(blob.bitmap().is_full() && !blob.is_complete());
        assert_eq!(blob.seal().await.unwrap(), manifest_root(content.len() as u64, &blob.hashes().unwrap()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn used_bytes_and_ownership_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        {
            let store = Store::open(&root, 1000).unwrap();
            store.create("file_a", 300).unwrap();
            let cached = store.create_with("file_b", 200, Origin::Cached).unwrap();
            cached.set_pinned(true).await;
        }

        let store = Store::open(&root, 1000).unwrap();
        assert_eq!(store.used_bytes(), 500);
        assert_eq!(store.get("file_a").unwrap().origin(), Origin::Published);
        let cached = store.get("file_b").unwrap();
        assert_eq!(cached.origin(), Origin::Cached);
        assert!(cached.is_pinned());
        assert_eq!(store.create("file_c", 600).err(), Some(StoreError::QuotaExceeded));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_least_recently_used_cached_file_is_evicted_first() {
        let (_dir, store) = store(300);
        complete(&store, "file_old", 100, Origin::Cached).await;
        complete(&store, "file_new", 100, Origin::Cached).await;
        complete(&store, "file_mid", 100, Origin::Cached).await;
        store.get("file_old").unwrap().read_chunk(0).await.unwrap();

        assert_eq!(store.evict_to_fit(100), ["file_new"]);
        assert!(store.get("file_old").is_some() && store.get("file_mid").is_some());
        assert_eq!(store.used_bytes(), 200);
        assert!(store.create("file_fresh", 100).is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn published_pinned_partial_and_busy_files_are_never_evicted() {
        let (_dir, store) = store(500);
        complete(&store, "file_published", 100, Origin::Published).await;
        complete(&store, "file_pinned", 100, Origin::Cached).await.set_pinned(true).await;
        store.create_with("file_partial", 100, Origin::Cached).unwrap();
        let busy = complete(&store, "file_busy", 100, Origin::Cached).await;
        let _writing = busy.try_write_lock().unwrap();

        assert!(store.evict_to_fit(500).is_empty());
        assert_eq!(store.used_bytes(), 400);

        complete(&store, "file_cached", 100, Origin::Cached).await;
        assert_eq!(store.evict_to_fit(500), ["file_cached"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_evicted_file_is_gone_from_disk_and_index_and_stays_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        let evicted = {
            let store = Store::open(&root, 100).unwrap();
            let blob = complete(&store, "file_a", 100, Origin::Cached).await;
            assert_eq!(store.evict_to_fit(1), ["file_a"]);
            assert!(blob.is_removed());
            assert!(!root.join("blobs/file_a").exists());
            blob
        };
        drop(evicted);
        assert!(Store::open(&root, 100).unwrap().get("file_a").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eviction_order_survives_a_restart_once_access_times_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        {
            let store = Store::open(&root, 200).unwrap();
            let first = complete(&store, "file_first", 100, Origin::Cached).await;
            complete(&store, "file_second", 100, Origin::Cached).await;
            first.persist().await;
            first.read_chunk(0).await.unwrap();
            store.save_access_times().await;
        }

        let store = Store::open(&root, 200).unwrap();
        assert_eq!(store.evict_to_fit(100), ["file_second"]);
    }

    #[test]
    fn files_missing_from_the_index_or_the_disk_are_cleaned_up_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        {
            let store = Store::open(&root, 1000).unwrap();
            store.create("file_kept", 10).unwrap();
            store.create("file_lost", 10).unwrap();
        }
        std::fs::remove_file(root.join("partial/file_lost.part")).unwrap();
        std::fs::write(root.join("blobs/file_stray"), b"left by a crash").unwrap();
        std::fs::write(root.join("partial/file_stray.part"), b"left by a crash").unwrap();
        std::fs::write(root.join("hashes/file_stray"), b"").unwrap();

        let store = Store::open(&root, 1000).unwrap();
        assert!(store.get("file_kept").is_some());
        assert!(store.get("file_lost").is_none());
        assert_eq!(store.used_bytes(), 10);
        assert!(!root.join("blobs/file_stray").exists() && !root.join("hashes/file_stray").exists());
        assert!(!root.join("partial/file_stray.part").exists());
        assert!(!root.join("hashes/file_lost").exists());
    }

    #[test]
    fn files_from_the_old_flat_layout_are_moved_in_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("files");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file_old.data"), vec![7u8; 10]).unwrap();
        std::fs::write(root.join("file_old.hashes"), [0u8; 32]).unwrap();
        std::fs::write(
            root.join("file_old.json"),
            r#"{"id":"file_old","size":10,"have":"00","hash_known":"00","expected_root":null,"sealed":null,"entry":null}"#,
        )
        .unwrap();

        let store = Store::open(&root, 1000).unwrap();
        let blob = store.get("file_old").unwrap();
        assert_eq!(blob.origin(), Origin::Published);
        assert_eq!(store.used_bytes(), 10);
        assert_eq!(std::fs::read(root.join("partial/file_old.part")).unwrap(), vec![7u8; 10]);
        assert!(!root.join("file_old.data").exists() && !root.join("file_old.json").exists());
    }
}
