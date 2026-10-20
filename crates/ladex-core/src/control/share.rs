//! Sharing files and folders that are on this machine's disk.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, ensure, Context};

use super::Events;
use crate::catalog::{CatalogEntry, EntryKind};
use crate::files_api::{self, FolderListing, ListingChild};
use crate::store::{chunk_len, Blob, Origin};
use crate::types::FileView;
use crate::{hlc, state, validate, NodeState};

// A device's uploads carry its id; what the node's own user shares carries none, and only this machine can take it down.
pub(super) const SHARED_BY_THIS_NODE: &str = "";

pub(super) fn new_id(kind: &str) -> String {
    format!("{kind}_{:016x}_{}", rand::random::<u64>(), hlc::wall_clock_ms())
}

/// One file on disk, with the path it will have inside a shared folder.
struct LocalFile {
    path: PathBuf,
    relative: String,
    size: u64,
}

enum Source {
    File(LocalFile),
    Folder { dir: PathBuf, files: Vec<LocalFile> },
}

impl Source {
    fn size(&self) -> u64 {
        match self {
            Source::File(file) => file.size,
            Source::Folder { files, .. } => files.iter().map(|file| file.size).sum(),
        }
    }
}

/// Bytes read so far across everything one command is sharing.
pub(super) struct Progress<'a> {
    pub(super) events: &'a Events,
    done: u64,
    total: u64,
}

impl<'a> Progress<'a> {
    pub(super) fn new(events: &'a Events, total: u64) -> Self {
        Self { events, done: 0, total }
    }

    fn advance(&mut self, name: &str, bytes: u64) -> anyhow::Result<()> {
        self.done += bytes;
        self.events.progress(name, self.done, self.total)
    }
}

pub(super) fn name_of(path: &Path) -> String {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    validate::sanitize_file_name(&name)
}

pub(super) fn mime_type_of(path: &Path) -> String {
    validate::sanitize_mime(mime_guess::from_path(path).first_or_octet_stream().essence_str())
}

pub(super) fn regular_file_size(path: &Path) -> anyhow::Result<u64> {
    let metadata = std::fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    ensure!(metadata.is_file(), "{} is not a file", path.display());
    Ok(metadata.len())
}

fn source_at(path: &Path) -> anyhow::Result<Source> {
    let metadata = std::fs::metadata(path).with_context(|| format!("could not read {}", path.display()))?;
    if metadata.is_dir() {
        return Ok(Source::Folder { dir: path.to_path_buf(), files: files_under(path)? });
    }
    ensure!(metadata.is_file(), "{} is neither a file nor a folder", path.display());
    Ok(Source::File(LocalFile { path: path.to_path_buf(), relative: name_of(path), size: metadata.len() }))
}

// Symlinks are left out: one pointing outside the folder would share something the user never named.
fn files_under(dir: &Path) -> anyhow::Result<Vec<LocalFile>> {
    let mut files = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut to_visit = vec![dir.to_path_buf()];
    while let Some(visiting) = to_visit.pop() {
        let entries = std::fs::read_dir(&visiting).with_context(|| format!("could not read {}", visiting.display()))?;
        for entry in entries {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                to_visit.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = relative_path(dir, &path)?;
            ensure!(seen_paths.insert(relative.to_lowercase()), "two files in {} would both be shared as {relative}", dir.display());
            ensure!(
                files.len() < validate::MAX_FOLDER_FILES as usize,
                "{} has more than {} files",
                dir.display(),
                validate::MAX_FOLDER_FILES
            );
            files.push(LocalFile { path, relative, size: metadata.len() });
        }
    }
    ensure!(!files.is_empty(), "{} has no files to share", dir.display());
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

fn relative_path(dir: &Path, path: &Path) -> anyhow::Result<String> {
    let inside = path.strip_prefix(dir)?;
    let parts: Vec<String> = inside.components().map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
    validate::sanitize_relative_path(&parts.join("/"))
        .ok_or_else(|| anyhow!("{} is nested too deeply or its path is too long", path.display()))
}

async fn sources_at(paths: &[PathBuf]) -> anyhow::Result<Vec<Source>> {
    let paths = paths.to_vec();
    tokio::task::spawn_blocking(move || paths.iter().map(|path| source_at(path)).collect()).await?
}

/// Copies a file on disk into the store under `id`; returns its manifest root. Nothing is kept if it fails.
pub(super) async fn import(
    state: &NodeState,
    path: &Path,
    id: &str,
    origin: Origin,
    progress: &mut Progress<'_>,
) -> anyhow::Result<String> {
    let file = Arc::new(File::open(path).with_context(|| format!("could not open {}", path.display()))?);
    let size = file.metadata()?.len();
    state::make_room(state, size).await;
    let blob = state.store.create_with(id, size, origin).map_err(|e| anyhow!("{e}"))?;
    let imported = copy_into(&blob, file, &name_of(path), progress).await;
    if imported.is_err() {
        state.store.remove(id);
    }
    imported.with_context(|| format!("could not share {}", path.display()))
}

async fn copy_into(blob: &Blob, file: Arc<File>, name: &str, progress: &mut Progress<'_>) -> anyhow::Result<String> {
    for index in 0..blob.chunk_count() {
        let (file, length) = (file.clone(), chunk_len(blob.size(), index));
        let chunk = tokio::task::spawn_blocking(move || {
            let mut chunk = vec![0u8; length];
            (&*file).read_exact(&mut chunk).map(|()| chunk)
        });
        let chunk = chunk.await?.context("the file changed while it was being read")?;
        blob.write_chunk_hashing(index, chunk).await.map_err(|e| anyhow!("could not store it: {e:?}"))?;
        progress.advance(name, length as u64)?;
    }
    blob.seal().await.map_err(|why| anyhow!(why))
}

async fn share_file(state: &NodeState, file: &LocalFile, parent: Option<&str>, progress: &mut Progress<'_>) -> anyhow::Result<FileView> {
    let id = new_id("file");
    let manifest_root = import(state, &file.path, &id, Origin::Published, progress).await?;
    let entry = CatalogEntry {
        file_id: id.clone(),
        name: name_of(&file.path),
        size: file.size,
        mime_type: mime_type_of(&file.path),
        created_at_ms: hlc::wall_clock_ms(),
        manifest_root,
        uploader_id: SHARED_BY_THIS_NODE.to_string(),
        kind: EntryKind::File { parent: parent.map(String::from) },
    };
    let listed = state::share_file(state, entry).await;
    if listed.is_err() {
        state.store.remove(&id);
    }
    listed.map_err(|refusal| anyhow!("could not list {}: {refusal}", file.path.display()))
}

async fn share_folder(state: &NodeState, dir: &Path, files: &[LocalFile], progress: &mut Progress<'_>) -> anyhow::Result<FileView> {
    let folder_id = new_id("folder");
    let mut listing = FolderListing { v: 1, name: name_of(dir), children: Vec::new() };
    let mut shared = Ok(());
    for file in files {
        match share_file(state, file, Some(&folder_id), progress).await {
            Ok(view) => listing.children.push(ListingChild { path: file.relative.clone(), file_id: view.id, size: view.size }),
            Err(e) => {
                shared = Err(e);
                break;
            }
        }
    }
    let child_ids: Vec<String> = listing.children.iter().map(|child| child.file_id.clone()).collect();
    let published = match shared {
        Ok(()) => files_api::publish_listing(state, &folder_id, listing, SHARED_BY_THIS_NODE.to_string()).await.map_err(|e| anyhow!("{e}")),
        Err(e) => Err(e),
    };
    if published.is_err() {
        // A folder is shared whole or not at all.
        for id in &child_ids {
            state::unshare(state, id, SHARED_BY_THIS_NODE, true).await;
        }
    }
    published.with_context(|| format!("could not share {}", dir.display()))
}

/// Shares each path with everyone: a file as a file, a directory as a folder.
pub(super) async fn share_paths(state: &NodeState, paths: &[PathBuf], events: &Events) -> anyhow::Result<Vec<FileView>> {
    let sources = sources_at(paths).await?;
    let mut progress = Progress::new(events, sources.iter().map(Source::size).sum());
    let mut shared = Vec::new();
    for source in &sources {
        let view = match source {
            Source::File(file) => share_file(state, file, None, &mut progress).await?,
            Source::Folder { dir, files } => share_folder(state, dir, files, &mut progress).await?,
        };
        tracing::info!("Control: shared {} ({} bytes) from this machine", view.name, source.size());
        shared.push(view);
    }
    Ok(shared)
}

/// Shares a directory as a folder and, once that is done, takes down the folder it replaces.
pub(super) async fn seed(state: &NodeState, dir: &Path, replaces: Option<&str>, events: &Events) -> anyhow::Result<FileView> {
    ensure!(dir.is_dir(), "{} is not a folder", dir.display());
    let shared = share_paths(state, &[dir.to_path_buf()], events).await?.remove(0);
    if let Some(old) = replaces {
        state::unshare(state, old, SHARED_BY_THIS_NODE, true).await;
    }
    Ok(shared)
}
