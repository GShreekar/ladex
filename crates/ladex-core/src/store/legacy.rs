//! Moves files stored by earlier versions (`<id>.data`, `.hashes` and `.json` side by side) into the current layout.

use std::io;
use std::path::Path;

use serde::Deserialize;

use super::index::{Index, Origin, Record};
use super::Layout;
use crate::types::FileMetadata;
use crate::validate;

#[derive(Deserialize)]
struct LegacyMeta {
    id: String,
    size: u64,
    have: String,
    hash_known: String,
    expected_root: Option<String>,
    sealed: Option<String>,
    entry: Option<FileMetadata>,
}

/// Imports every old-style file under `root`; returns how many were moved.
pub fn migrate(root: &Path, layout: &Layout, index: &Index, now_ms: u64) -> io::Result<usize> {
    let mut moved = 0;
    for entry in std::fs::read_dir(root)? {
        let meta_path = entry?.path();
        if meta_path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match import(root, &meta_path, layout, index, now_ms) {
            Ok(()) => moved += 1,
            Err(e) => tracing::warn!("Store: could not migrate {}: {e}", meta_path.display()),
        }
    }
    Ok(moved)
}

fn import(root: &Path, meta_path: &Path, layout: &Layout, index: &Index, now_ms: u64) -> io::Result<()> {
    let meta: LegacyMeta = serde_json::from_slice(&std::fs::read(meta_path)?)?;
    if !validate::is_valid_id(&meta.id) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad id"));
    }
    let old_data = root.join(format!("{}.data", meta.id));
    let old_hashes = root.join(format!("{}.hashes", meta.id));
    if !old_data.is_file() || !old_hashes.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "data files are missing"));
    }

    std::fs::rename(&old_data, layout.data(&meta.id))?;
    std::fs::rename(&old_hashes, layout.hashes(&meta.id))?;
    let record = Record {
        size: meta.size,
        have: meta.have,
        hash_known: meta.hash_known,
        expected_root: meta.expected_root,
        sealed: meta.sealed,
        entry: meta.entry,
        // Earlier versions never evicted anything, so keep everything they held.
        origin: Origin::Published,
        pinned: false,
        created_ms: now_ms,
        last_access_ms: now_ms,
    };
    index.put(&meta.id, &record).map_err(io::Error::other)?;
    std::fs::remove_file(meta_path)
}
