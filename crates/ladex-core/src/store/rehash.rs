//! Re-hashes files stored with SHA-256 chunk hashes, so the store can verify them against BLAKE3 manifests.

use std::fs::{File, OpenOptions};
use std::io;

use super::index::{Index, Record};
use super::{chunk_len, hash_chunk, read_exact_at, root_of, write_all_at, ChunkHash, Layout, CHUNK_SIZE};
use crate::bitmap::Bitmap;

const ALGORITHM: &str = "blake3";

/// Converts every stored file once; a store already on BLAKE3 is left as it is.
pub fn migrate(layout: &Layout, index: &Index) -> io::Result<()> {
    if index.chunk_hash_algorithm().map_err(io::Error::other)?.as_deref() == Some(ALGORITHM) {
        return Ok(());
    }
    let records = index.load_all().map_err(io::Error::other)?;
    if !records.is_empty() {
        tracing::info!("Store: re-hashing {} files with BLAKE3 (done once)", records.len());
    }
    for (id, record) in records {
        match rehash(layout, &id, record) {
            Ok(record) => index.put(&id, &record).map_err(io::Error::other)?,
            Err(e) => {
                tracing::warn!("Store: dropping {id}, it could not be re-hashed: {e}");
                let _ = std::fs::remove_file(layout.data(&id));
                let _ = std::fs::remove_file(layout.hashes(&id));
                index.remove(&id).map_err(io::Error::other)?;
            }
        }
    }
    index.set_chunk_hash_algorithm(ALGORITHM).map_err(io::Error::other)
}

// Only chunks already on disk can be hashed again; the rest wait for a manifest from another node.
fn rehash(layout: &Layout, id: &str, mut record: Record) -> io::Result<Record> {
    let chunks = super::chunk_count(record.size);
    let have = Bitmap::from_hex(chunks, &record.have).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad chunk map"))?;
    let data = File::open(layout.data(id))?;
    let hashes_file = OpenOptions::new().write(true).open(layout.hashes(id))?;

    let mut hashes: Vec<ChunkHash> = vec![[0; 32]; chunks as usize];
    let mut buffer = vec![0u8; CHUNK_SIZE as usize];
    for index in (0..chunks).filter(|index| have.get(*index)) {
        let chunk = &mut buffer[..chunk_len(record.size, index)];
        read_exact_at(&data, chunk, index as u64 * CHUNK_SIZE)?;
        hashes[index as usize] = hash_chunk(chunk);
    }
    write_all_at(&hashes_file, hashes.as_flattened(), 0)?;
    hashes_file.sync_data()?;

    record.hash_known = record.have.clone();
    record.expected_root = None;
    record.sealed = have.is_full().then(|| root_of(record.size, &hashes));
    if let (Some(entry), Some(root)) = (record.entry.as_mut(), &record.sealed) {
        entry.manifest_root = Some(root.clone());
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::super::index::Origin;
    use super::super::Store;
    use super::*;

    const CHUNK: usize = CHUNK_SIZE as usize;

    fn sha256(data: &[u8]) -> ChunkHash {
        ring::digest::digest(&ring::digest::SHA256, data).as_ref().try_into().unwrap()
    }

    // Writes a file the way the SHA-256 store did, with `present` chunks on disk.
    fn write_old_file(root: &std::path::Path, id: &str, content: &[u8], present: &[u32]) {
        let layout = Layout::new(root);
        let size = content.len() as u64;
        let chunks = super::super::chunk_count(size);
        let mut have = Bitmap::new(chunks);
        let mut hashes: Vec<ChunkHash> = vec![[0; 32]; chunks as usize];
        let mut data = vec![0u8; content.len()];
        for &index in present {
            let range = index as usize * CHUNK..(index as usize * CHUNK + chunk_len(size, index));
            data[range.clone()].copy_from_slice(&content[range.clone()]);
            hashes[index as usize] = sha256(&content[range]);
            have.set(index);
        }
        std::fs::write(layout.data(id), &data).unwrap();
        std::fs::write(layout.hashes(id), hashes.as_flattened()).unwrap();
        let record = Record {
            size,
            have: have.to_hex(),
            hash_known: have.to_hex(),
            expected_root: None,
            sealed: have.is_full().then(|| "old-sha256-root".to_string()),
            entry: None,
            origin: Origin::Published,
            pinned: false,
            created_ms: 1,
            last_access_ms: 1,
        };
        Index::open(&layout.index()).unwrap().put(id, &record).unwrap();
    }

    fn prepare_dirs(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::create_dir_all(root.join("hashes")).unwrap();
    }

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn a_complete_file_gets_a_blake3_root_and_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        prepare_dirs(dir.path());
        let bytes = content(CHUNK * 2 + 10);
        write_old_file(dir.path(), "f1", &bytes, &[0, 1, 2]);

        let store = Store::open(dir.path(), 1 << 40).unwrap();
        let blob = store.get("f1").unwrap();
        let expected: Vec<ChunkHash> = bytes.chunks(CHUNK).map(hash_chunk).collect();
        assert_eq!(blob.manifest_root(), Some(root_of(bytes.len() as u64, &expected)));
        assert_eq!(blob.hashes(), Some(expected));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rehashed_file_reads_back_verified() {
        let dir = tempfile::tempdir().unwrap();
        prepare_dirs(dir.path());
        let bytes = content(CHUNK + 5);
        write_old_file(dir.path(), "f1", &bytes, &[0, 1]);

        let blob = Store::open(dir.path(), 1 << 40).unwrap().get("f1").unwrap();
        assert_eq!(blob.read_chunk_verified(1).await.unwrap(), bytes[CHUNK..].to_vec());
    }

    #[test]
    fn a_partial_file_keeps_only_the_chunks_it_has_and_stays_unsealed() {
        let dir = tempfile::tempdir().unwrap();
        prepare_dirs(dir.path());
        write_old_file(dir.path(), "f1", &content(CHUNK * 3), &[0, 2]);

        let blob = Store::open(dir.path(), 1 << 40).unwrap().get("f1").unwrap();
        assert!(!blob.is_complete());
        assert!(!blob.has_manifest());
        assert!(blob.has_chunk(0) && !blob.has_chunk(1) && blob.has_chunk(2));
    }

    #[test]
    fn a_file_whose_data_is_missing_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        prepare_dirs(dir.path());
        write_old_file(dir.path(), "f1", &content(10), &[0]);
        std::fs::remove_file(Layout::new(dir.path()).data("f1")).unwrap();

        assert!(Store::open(dir.path(), 1 << 40).unwrap().get("f1").is_none());
    }

    #[test]
    fn a_store_is_converted_only_once() {
        let dir = tempfile::tempdir().unwrap();
        prepare_dirs(dir.path());
        write_old_file(dir.path(), "f1", &content(10), &[0]);
        let first_root = Store::open(dir.path(), 1 << 40).unwrap().get("f1").unwrap().manifest_root();

        let reopened = Store::open(dir.path(), 1 << 40).unwrap();
        assert_eq!(reopened.get("f1").unwrap().manifest_root(), first_root);
    }
}
