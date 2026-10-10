//! The node's catalog as shared state: guarded for use from many tasks, and saved to disk as it changes.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use ed25519_dalek::VerifyingKey;

use super::log::OpLog;
use super::{Add, Catalog, CatalogEntry, Op, Outcome, Refusal, Remove, UnpublishError, VersionVector};
use crate::hlc::{Clock, Stamp};
use crate::identity::Identity;

/// What came of a batch of ops from another node.
#[derive(Debug, Default)]
pub struct Received {
    /// The ops that were new here.
    pub applied: Vec<Op>,
    pub refused: Vec<Refusal>,
}

pub struct SharedCatalog {
    catalog: Mutex<Catalog>,
    log: OpLog,
}

impl SharedCatalog {
    /// Opens the catalog saved in `dir`, or an empty one on first run.
    pub fn open(dir: &Path, capacity: usize) -> anyhow::Result<Self> {
        let log = OpLog::open(dir)?;
        let mut catalog = Catalog::new(capacity);
        for op in log.load_all()? {
            if let Err(refusal) = catalog.apply(op) {
                tracing::warn!("Catalog: dropping a saved op because {refusal}");
            }
        }
        Ok(Self { catalog: Mutex::new(catalog), log })
    }

    pub fn publish(&self, identity: &Identity, clock: &Clock, entry: CatalogEntry, authorized: Vec<VerifyingKey>) -> Result<Add, Refusal> {
        let add = self.lock().publish(identity, clock, entry, authorized)?;
        self.save(&[Op::Add(add.clone())]);
        Ok(add)
    }

    pub fn unpublish(&self, identity: &Identity, clock: &Clock, file_id: &str) -> Result<Remove, UnpublishError> {
        let remove = self.lock().unpublish(identity, clock, file_id)?;
        self.save(&[Op::Remove(remove.clone())]);
        Ok(remove)
    }

    /// Verifies and merges ops from another node.
    pub fn receive(&self, ops: Vec<Op>) -> Received {
        let mut received = Received::default();
        {
            let mut catalog = self.lock();
            for op in ops {
                match catalog.apply(op.clone()) {
                    Ok(Outcome::Applied) => received.applied.push(op),
                    Ok(Outcome::AlreadyKnown) => {}
                    Err(refusal) => received.refused.push(refusal),
                }
            }
        }
        self.save(&received.applied);
        received
    }

    pub fn version(&self) -> VersionVector {
        self.lock().version()
    }

    pub fn ops_missing_from(&self, theirs: &VersionVector) -> Vec<Op> {
        self.lock().ops_missing_from(theirs)
    }

    pub fn latest_stamp(&self, origin: &str) -> Option<Stamp> {
        self.lock().latest_stamp(origin).cloned()
    }

    pub fn entries(&self) -> Vec<CatalogEntry> {
        self.lock().entries().into_iter().cloned().collect()
    }

    pub fn contains(&self, file_id: &str) -> bool {
        self.lock().contains(file_id)
    }

    fn lock(&self) -> MutexGuard<'_, Catalog> {
        self.catalog.lock().unwrap()
    }

    // An op that cannot be saved still counts for this run; the failure is loud because a restart would lose it.
    fn save(&self, ops: &[Op]) {
        if let Err(e) = self.log.save(ops) {
            tracing::error!("Catalog: could not save {} op(s) to disk: {e:#}", ops.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: usize = 100;

    fn entry(file_id: &str) -> CatalogEntry {
        CatalogEntry { file_id: file_id.into(), name: format!("{file_id}.txt"), size: 1, mime_type: "text/plain".into(), created_at_ms: 1 }
    }

    fn identity_and_clock() -> (Identity, Clock) {
        let identity = Identity::generate();
        let clock = Clock::new(identity.node_id().to_string());
        (identity, clock)
    }

    #[test]
    fn published_files_are_still_there_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (identity, clock) = identity_and_clock();
        {
            let catalog = SharedCatalog::open(dir.path(), CAPACITY).unwrap();
            catalog.publish(&identity, &clock, entry("f1"), vec![]).unwrap();
        }
        assert!(SharedCatalog::open(dir.path(), CAPACITY).unwrap().contains("f1"));
    }

    #[test]
    fn a_file_taken_down_stays_down_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (identity, clock) = identity_and_clock();
        {
            let catalog = SharedCatalog::open(dir.path(), CAPACITY).unwrap();
            catalog.publish(&identity, &clock, entry("f1"), vec![]).unwrap();
            catalog.unpublish(&identity, &clock, "f1").unwrap();
        }
        assert!(!SharedCatalog::open(dir.path(), CAPACITY).unwrap().contains("f1"));
    }

    #[test]
    fn a_restarted_node_carries_on_its_sequence_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let (identity, clock) = identity_and_clock();
        {
            let catalog = SharedCatalog::open(dir.path(), CAPACITY).unwrap();
            catalog.publish(&identity, &clock, entry("f1"), vec![]).unwrap();
        }
        let catalog = SharedCatalog::open(dir.path(), CAPACITY).unwrap();
        assert_eq!(catalog.publish(&identity, &clock, entry("f2"), vec![]).unwrap().seq, 2);
    }

    #[test]
    fn received_ops_are_saved() {
        let (origin_dir, replica_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (identity, clock) = identity_and_clock();
        let origin = SharedCatalog::open(origin_dir.path(), CAPACITY).unwrap();
        origin.publish(&identity, &clock, entry("f1"), vec![]).unwrap();
        {
            let replica = SharedCatalog::open(replica_dir.path(), CAPACITY).unwrap();
            replica.receive(origin.ops_missing_from(&VersionVector::default()));
        }
        assert!(SharedCatalog::open(replica_dir.path(), CAPACITY).unwrap().contains("f1"));
    }

    #[test]
    fn receiving_reports_what_was_new_and_what_was_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (identity, clock) = identity_and_clock();
        let catalog = SharedCatalog::open(dir.path(), CAPACITY).unwrap();
        let add = Add::sign(&identity, clock.now(), 1, vec![], entry("f1"));
        let reusing = Add::sign(&identity, clock.now(), 1, vec![], entry("f2"));
        let received = catalog.receive(vec![Op::Add(add.clone()), Op::Add(add), Op::Add(reusing)]);
        assert_eq!((received.applied.len(), received.refused), (1, vec![Refusal::SequenceReused]));
    }
}
