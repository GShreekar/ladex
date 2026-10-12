//! The nodes this node has accepted into its mesh, kept across restarts; a revoked key stays rejected.

use std::path::Path;

use anyhow::Context;
use ed25519_dalek::VerifyingKey;
use redb::{Database, ReadableDatabase, ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::identity::node_id_for;
use crate::revocation::Revocation;
use crate::types::NodeId;
use crate::validate;

const FILE_NAME: &str = "trust.redb";
const NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
const REVOCATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("revocations");

/// How a node earned this node's trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustedVia {
    Passphrase,
    // Ordered after Passphrase: pairing checks the key itself, so it is the stronger proof.
    Pairing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedNode {
    pub node_id: NodeId,
    #[serde(with = "hex_public_key")]
    pub public_key: VerifyingKey,
    pub name: String,
    pub trusted_via: TrustedVia,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub revoked: bool,
}

/// What this node thinks of a public key presented to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    Unknown,
    Trusted(Box<TrustedNode>),
    Revoked,
}

pub struct TrustStore {
    db: Database,
}

impl TrustStore {
    /// Opens the trust store in the data directory, creating it on first run.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        crate::tls::create_private_dir(dir).with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(FILE_NAME);
        let db = Database::create(&path).with_context(|| format!("could not open the trust store at {}", path.display()))?;
        let txn = db.begin_write()?;
        txn.open_table(NODES)?;
        txn.open_table(REVOCATIONS)?;
        txn.commit()?;
        Ok(Self { db })
    }

    /// A key revoked by a signed revocation counts as revoked even if this node never trusted it.
    pub fn standing(&self, public_key: &VerifyingKey) -> anyhow::Result<Standing> {
        let node_id = node_id_for(public_key);
        let txn = self.db.begin_read()?;
        if txn.open_table(REVOCATIONS)?.get(node_id.as_str())?.is_some() {
            return Ok(Standing::Revoked);
        }
        let standing = match read(&txn.open_table(NODES)?, &node_id)? {
            None => Standing::Unknown,
            Some(node) if node.revoked => Standing::Revoked,
            Some(node) => Standing::Trusted(Box::new(node)),
        };
        Ok(standing)
    }

    pub fn get(&self, node_id: &str) -> anyhow::Result<Option<TrustedNode>> {
        let txn = self.db.begin_read()?;
        read(&txn.open_table(NODES)?, node_id)
    }

    /// Every node on record, revoked ones included, oldest first.
    pub fn list(&self) -> anyhow::Result<Vec<TrustedNode>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(NODES)?;
        let mut nodes = Vec::new();
        for entry in table.iter()? {
            let (node_id, bytes) = entry?;
            nodes.push(decode(node_id.value(), bytes.value())?);
        }
        nodes.sort_by_key(|node| node.first_seen_ms);
        Ok(nodes)
    }

    /// Records that a node has just proven itself; a revoked node is refused until forgotten.
    pub fn trust(&self, public_key: &VerifyingKey, name: &str, via: TrustedVia, now_ms: u64) -> anyhow::Result<TrustedNode> {
        let node_id = node_id_for(public_key);
        let txn = self.db.begin_write()?;
        if txn.open_table(REVOCATIONS)?.get(node_id.as_str())?.is_some() {
            anyhow::bail!("node {node_id} has been revoked");
        }
        let node = {
            let mut table = txn.open_table(NODES)?;
            let node = match read(&table, &node_id)? {
                Some(node) if node.revoked => anyhow::bail!("node {node_id} has been revoked"),
                Some(known) => TrustedNode {
                    name: validate::clean_label(name, validate::MAX_NODE_NAME_CHARS),
                    trusted_via: known.trusted_via.max(via),
                    last_seen_ms: now_ms,
                    ..known
                },
                None => TrustedNode {
                    node_id: node_id.clone(),
                    public_key: *public_key,
                    name: validate::clean_label(name, validate::MAX_NODE_NAME_CHARS),
                    trusted_via: via,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    revoked: false,
                },
            };
            write(&mut table, &node)?;
            node
        };
        txn.commit()?;
        Ok(node)
    }

    /// Notes that a trusted node reconnected. False when it isn't trusted (unknown or revoked).
    pub fn mark_seen(&self, node_id: &str, name: &str, now_ms: u64) -> anyhow::Result<bool> {
        self.change(node_id, |node| {
            if node.revoked {
                return false;
            }
            node.name = validate::clean_label(name, validate::MAX_NODE_NAME_CHARS);
            node.last_seen_ms = now_ms;
            true
        })
    }

    /// Keeps a signed revocation and stops trusting its key for good. False when the key was already revoked by one.
    pub fn revoke(&self, revocation: &Revocation) -> anyhow::Result<bool> {
        anyhow::ensure!(revocation.is_signed(), "the revocation's signature is invalid");
        let node_id = revocation.node_id();
        let txn = self.db.begin_write()?;
        {
            let mut revocations = txn.open_table(REVOCATIONS)?;
            if revocations.get(node_id.as_str())?.is_some() {
                return Ok(false);
            }
            revocations.insert(node_id.as_str(), serde_json::to_vec(revocation)?.as_slice())?;
            let mut nodes = txn.open_table(NODES)?;
            if let Some(mut node) = read(&nodes, &node_id)? {
                node.revoked = true;
                write(&mut nodes, &node)?;
            }
        }
        txn.commit()?;
        Ok(true)
    }

    pub fn revocation(&self, node_id: &str) -> anyhow::Result<Option<Revocation>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(REVOCATIONS)?;
        let Some(bytes) = table.get(node_id)? else {
            return Ok(None);
        };
        decode_revocation(node_id, bytes.value()).map(Some)
    }

    /// Every revocation this node holds, to pass on to the nodes it meets.
    pub fn revocations(&self) -> anyhow::Result<Vec<Revocation>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(REVOCATIONS)?;
        let mut revocations = Vec::new();
        for entry in table.iter()? {
            let (node_id, bytes) = entry?;
            revocations.push(decode_revocation(node_id.value(), bytes.value())?);
        }
        Ok(revocations)
    }

    /// Removes a node's record and its revocation here; nodes that still hold the revocation will send it again.
    pub fn forget(&self, node_id: &str) -> anyhow::Result<bool> {
        let txn = self.db.begin_write()?;
        let removed = txn.open_table(NODES)?.remove(node_id)?.is_some();
        let unrevoked = txn.open_table(REVOCATIONS)?.remove(node_id)?.is_some();
        txn.commit()?;
        Ok(removed || unrevoked)
    }

    fn change(&self, node_id: &str, edit: impl FnOnce(&mut TrustedNode) -> bool) -> anyhow::Result<bool> {
        let txn = self.db.begin_write()?;
        let changed = {
            let mut table = txn.open_table(NODES)?;
            let Some(mut node) = read(&table, node_id)? else {
                return Ok(false);
            };
            let is_changed = edit(&mut node);
            if is_changed {
                write(&mut table, &node)?;
            }
            is_changed
        };
        txn.commit()?;
        Ok(changed)
    }
}

fn read(table: &impl ReadableTable<&'static str, &'static [u8]>, node_id: &str) -> anyhow::Result<Option<TrustedNode>> {
    match table.get(node_id)? {
        Some(bytes) => decode(node_id, bytes.value()).map(Some),
        None => Ok(None),
    }
}

fn write(table: &mut Table<&str, &[u8]>, node: &TrustedNode) -> anyhow::Result<()> {
    table.insert(node.node_id.as_str(), serde_json::to_vec(node)?.as_slice())?;
    Ok(())
}

// A record whose key doesn't hash to its id was tampered with, so it is never used.
fn decode(node_id: &str, bytes: &[u8]) -> anyhow::Result<TrustedNode> {
    let node: TrustedNode = serde_json::from_slice(bytes).with_context(|| format!("the trust record for {node_id} is damaged"))?;
    anyhow::ensure!(
        node.node_id == node_id && node_id_for(&node.public_key) == node_id,
        "the trust record for {node_id} holds another node's key"
    );
    Ok(node)
}

fn decode_revocation(node_id: &str, bytes: &[u8]) -> anyhow::Result<Revocation> {
    let revocation: Revocation = serde_json::from_slice(bytes).with_context(|| format!("the revocation of {node_id} is damaged"))?;
    anyhow::ensure!(revocation.node_id() == node_id && revocation.is_signed(), "the revocation of {node_id} does not hold up");
    Ok(revocation)
}

pub(crate) mod hex_public_key {
    use ed25519_dalek::VerifyingKey;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(key: &VerifyingKey, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(key.as_bytes()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VerifyingKey, D::Error> {
        let bytes = hex::decode(String::deserialize(deserializer)?).map_err(D::Error::custom)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| D::Error::custom("a public key is 32 bytes"))?;
        VerifyingKey::from_bytes(&bytes).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: TrustStore,
    }

    fn store() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = TrustStore::open(dir.path()).unwrap();
        Fixture { _dir: dir, store }
    }

    fn key(seed: u8) -> VerifyingKey {
        Identity::from_secret(&[seed; 32]).public_key()
    }

    fn revoke(store: &TrustStore, seed: u8) -> bool {
        let issuer = Identity::from_secret(&[0; 32]);
        store.revoke(&Revocation::issue(&issuer, &key(seed), 100)).unwrap()
    }

    #[test]
    fn an_unknown_key_has_no_standing() {
        let fixture = store();
        assert_eq!(fixture.store.standing(&key(1)).unwrap(), Standing::Unknown);
    }

    #[test]
    fn a_trusted_key_is_recognised_by_its_key_alone() {
        let fixture = store();
        let trusted = fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        assert_eq!(fixture.store.standing(&key(1)).unwrap(), Standing::Trusted(Box::new(trusted)));
    }

    #[test]
    fn a_new_record_is_keyed_by_the_id_derived_from_the_key() {
        let fixture = store();
        let trusted = fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        assert_eq!(trusted.node_id, node_id_for(&key(1)));
        assert_eq!((trusted.first_seen_ms, trusted.last_seen_ms), (100, 100));
    }

    #[test]
    fn trusting_again_keeps_first_seen_and_updates_the_rest() {
        let fixture = store();
        fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        let again = fixture.store.trust(&key(1), "work laptop", TrustedVia::Passphrase, 200).unwrap();
        assert_eq!((again.first_seen_ms, again.last_seen_ms, again.name.as_str()), (100, 200, "work laptop"));
    }

    #[test]
    fn a_passphrase_does_not_downgrade_a_paired_node() {
        let fixture = store();
        fixture.store.trust(&key(1), "phone", TrustedVia::Pairing, 100).unwrap();
        let again = fixture.store.trust(&key(1), "phone", TrustedVia::Passphrase, 200).unwrap();
        assert_eq!(again.trusted_via, TrustedVia::Pairing);
    }

    #[test]
    fn pairing_upgrades_a_passphrase_node() {
        let fixture = store();
        fixture.store.trust(&key(1), "phone", TrustedVia::Passphrase, 100).unwrap();
        let again = fixture.store.trust(&key(1), "phone", TrustedVia::Pairing, 200).unwrap();
        assert_eq!(again.trusted_via, TrustedVia::Pairing);
    }

    #[test]
    fn a_revoked_key_is_rejected() {
        let fixture = store();
        fixture.store.trust(&key(1), "lost phone", TrustedVia::Passphrase, 100).unwrap();
        assert!(revoke(&fixture.store, 1));
        assert_eq!(fixture.store.standing(&key(1)).unwrap(), Standing::Revoked);
    }

    #[test]
    fn a_revoked_key_cannot_be_trusted_again_with_the_passphrase() {
        let fixture = store();
        fixture.store.trust(&key(1), "lost phone", TrustedVia::Passphrase, 100).unwrap();
        revoke(&fixture.store, 1);
        assert!(fixture.store.trust(&key(1), "lost phone", TrustedVia::Passphrase, 200).is_err());
        assert_eq!(fixture.store.standing(&key(1)).unwrap(), Standing::Revoked);
    }

    #[test]
    fn a_forgotten_node_can_be_trusted_again() {
        let fixture = store();
        fixture.store.trust(&key(1), "phone", TrustedVia::Passphrase, 100).unwrap();
        revoke(&fixture.store, 1);
        assert!(fixture.store.forget(&node_id_for(&key(1))).unwrap());
        let again = fixture.store.trust(&key(1), "phone", TrustedVia::Pairing, 300).unwrap();
        assert_eq!((again.revoked, again.first_seen_ms), (false, 300));
    }

    #[test]
    fn revoking_twice_reports_it() {
        let fixture = store();
        assert!(revoke(&fixture.store, 1));
        assert!(!revoke(&fixture.store, 1));
    }

    #[test]
    fn forgetting_an_unknown_node_reports_it() {
        assert!(!store().store.forget("nobody").unwrap());
    }

    #[test]
    fn a_key_revoked_before_it_was_ever_trusted_is_refused() {
        let fixture = store();
        revoke(&fixture.store, 1);
        assert_eq!(fixture.store.standing(&key(1)).unwrap(), Standing::Revoked);
        assert!(fixture.store.trust(&key(1), "thief", TrustedVia::Pairing, 200).is_err());
    }

    #[test]
    fn mark_seen_updates_a_trusted_node() {
        let fixture = store();
        fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        assert!(fixture.store.mark_seen(&node_id_for(&key(1)), "renamed", 500).unwrap());
        let node = fixture.store.get(&node_id_for(&key(1))).unwrap().unwrap();
        assert_eq!((node.last_seen_ms, node.name.as_str()), (500, "renamed"));
    }

    #[test]
    fn mark_seen_ignores_unknown_and_revoked_nodes() {
        let fixture = store();
        fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        revoke(&fixture.store, 1);
        assert!(!fixture.store.mark_seen(&node_id_for(&key(1)), "laptop", 500).unwrap());
        assert!(!fixture.store.mark_seen("nobody", "x", 500).unwrap());
        assert_eq!(fixture.store.get(&node_id_for(&key(1))).unwrap().unwrap().last_seen_ms, 100);
    }

    #[test]
    fn names_are_cleaned_and_capped() {
        let fixture = store();
        let long_name = format!("evil\u{202e}{}", "x".repeat(500));
        let node = fixture.store.trust(&key(1), &long_name, TrustedVia::Passphrase, 100).unwrap();
        assert!(!node.name.contains('\u{202e}'));
        assert_eq!(node.name.chars().count(), validate::MAX_NODE_NAME_CHARS);
    }

    #[test]
    fn list_returns_every_node_oldest_first() {
        let fixture = store();
        fixture.store.trust(&key(2), "second", TrustedVia::Passphrase, 200).unwrap();
        fixture.store.trust(&key(1), "first", TrustedVia::Passphrase, 100).unwrap();
        revoke(&fixture.store, 2);
        let names: Vec<String> = fixture.store.list().unwrap().into_iter().map(|node| node.name).collect();
        assert_eq!(names, ["first", "second"]);
    }

    #[test]
    fn trust_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let trusted = TrustStore::open(dir.path()).unwrap().trust(&key(1), "laptop", TrustedVia::Pairing, 100).unwrap();
        let reopened = TrustStore::open(dir.path()).unwrap();
        assert_eq!(reopened.standing(&key(1)).unwrap(), Standing::Trusted(Box::new(trusted)));
    }

    #[test]
    fn a_record_holding_another_nodes_key_is_refused() {
        let fixture = store();
        let mut forged = fixture.store.trust(&key(1), "laptop", TrustedVia::Passphrase, 100).unwrap();
        forged.public_key = key(2);
        let txn = fixture.store.db.begin_write().unwrap();
        write(&mut txn.open_table(NODES).unwrap(), &forged).unwrap();
        txn.commit().unwrap();
        assert!(fixture.store.standing(&key(1)).is_err());
    }
}
