//! The catalog's ops on disk, so a restarted node still knows its own sequence numbers and what it has seen.

use std::path::Path;

use anyhow::Context;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use super::Op;

const FILE_NAME: &str = "catalog.redb";
// Keyed by origin node and sequence number, so each op is stored once.
const OPS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("ops");

pub struct OpLog {
    db: Database,
}

impl OpLog {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        crate::tls::create_private_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(FILE_NAME);
        let db = Database::create(&path).with_context(|| format!("could not open the catalog log at {}", path.display()))?;
        let txn = db.begin_write()?;
        txn.open_table(OPS)?;
        txn.commit()?;
        Ok(Self { db })
    }

    pub fn load_all(&self) -> anyhow::Result<Vec<Op>> {
        let txn = self.db.begin_read()?;
        let mut ops = Vec::new();
        for entry in txn.open_table(OPS)?.iter()? {
            let (key, bytes) = entry?;
            match postcard::from_bytes(bytes.value()) {
                Ok(op) => ops.push(op),
                Err(e) => tracing::warn!("Catalog: skipping the unreadable op {}#{}: {e}", key.value().0, key.value().1),
            }
        }
        Ok(ops)
    }

    /// Saves the ops together, so a crash leaves all of them or none.
    pub fn save(&self, ops: &[Op]) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(OPS)?;
            for op in ops {
                table.insert((op.origin().as_str(), op.seq()), postcard::to_stdvec(op)?.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }
}
