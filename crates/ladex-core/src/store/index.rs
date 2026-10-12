//! The redb index of stored files: one record per file, saved together so a crash never leaves them disagreeing.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const BLOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("blobs");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
const HASH_KEY: &str = "chunk_hash";

/// Where a file came from; decides whether the garbage collector may evict it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Shared from this node: never evicted automatically.
    Published,
    /// Fetched from other nodes: evicted least-recently-used first when the quota fills.
    Cached,
    /// Being sent to or received by one device, outside the catalog: deleted when that push ends.
    Pushed,
}

/// The catalog entry that older versions saved with each file; read once to carry it into the signed catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyEntry {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    #[serde(default)]
    pub uploader_id: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub manifest_root: Option<String>,
    #[serde(default)]
    pub is_folder: bool,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub folder_bytes: u64,
    #[serde(default)]
    pub folder_files: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub size: u64,
    pub have: String,
    pub hash_known: String,
    pub expected_root: Option<String>,
    pub sealed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<LegacyEntry>,
    pub origin: Origin,
    pub pinned: bool,
    pub created_ms: u64,
    pub last_access_ms: u64,
}

pub struct Index {
    db: Database,
}

impl Index {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        txn.open_table(BLOBS)?;
        txn.open_table(META)?;
        txn.commit()?;
        Ok(Self { db })
    }

    /// Which hash the chunk hashes on disk were made with; None for a store from before this was recorded.
    pub fn chunk_hash_algorithm(&self) -> anyhow::Result<Option<String>> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(META)?.get(HASH_KEY)?.map(|value| value.value().to_string()))
    }

    pub fn set_chunk_hash_algorithm(&self, algorithm: &str) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(META)?.insert(HASH_KEY, algorithm)?;
        txn.commit()?;
        Ok(())
    }

    pub fn load_all(&self) -> anyhow::Result<Vec<(String, Record)>> {
        let txn = self.db.begin_read()?;
        let mut records = Vec::new();
        for entry in txn.open_table(BLOBS)?.iter()? {
            let (id, bytes) = entry?;
            match serde_json::from_slice(bytes.value()) {
                Ok(record) => records.push((id.value().to_string(), record)),
                Err(e) => tracing::warn!("Store: skipping the unreadable record for {}: {e}", id.value()),
            }
        }
        Ok(records)
    }

    pub fn put(&self, id: &str, record: &Record) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(BLOBS)?.insert(id, serde_json::to_vec(record)?.as_slice())?;
        txn.commit()?;
        Ok(())
    }

    pub fn put_all(&self, records: &[(String, Record)]) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(BLOBS)?;
            for (id, record) in records {
                table.insert(id.as_str(), serde_json::to_vec(record)?.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(BLOBS)?.remove(id)?;
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(size: u64) -> Record {
        Record {
            size,
            have: String::new(),
            hash_known: String::new(),
            expected_root: None,
            sealed: None,
            entry: None,
            origin: Origin::Cached,
            pinned: false,
            created_ms: 1,
            last_access_ms: 2,
        }
    }

    #[test]
    fn records_are_saved_listed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("store.redb")).unwrap();
        index.put("file_a", &record(10)).unwrap();
        index.put("file_b", &record(20)).unwrap();
        index.put("file_a", &record(30)).unwrap();

        let mut records = index.load_all().unwrap();
        records.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(records.iter().map(|(id, r)| (id.as_str(), r.size)).collect::<Vec<_>>(), [("file_a", 30), ("file_b", 20)]);

        index.remove("file_a").unwrap();
        assert_eq!(index.load_all().unwrap().len(), 1);
    }

    #[test]
    fn records_survive_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");
        Index::open(&path).unwrap().put("file_a", &record(10)).unwrap();

        let records = Index::open(&path).unwrap().load_all().unwrap();
        assert_eq!(records[0].1.last_access_ms, 2);
        assert_eq!(records[0].1.origin, Origin::Cached);
    }
}
