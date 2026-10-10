//! The shared file catalog: an add-wins OR-Set of signed adds and removes that converge whatever order they arrive in.
//!
//! An add is tagged with its publisher's stamp. A remove lists the add tags it saw, so an add made concurrently
//! survives it. Only the publisher, or a key the publisher named in the add, can remove it; every node checks that.

mod log;
pub mod shared;
pub mod sync;

use std::collections::{BTreeMap, HashMap};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::hlc::{Clock, Stamp};
use crate::identity::{self, Identity};
use crate::revocation::hex_signature;
use crate::trust::hex_public_key;
use crate::types::{NodeId, SessionId};
use crate::validate;

const ADD_LABEL: &[u8] = b"ladex-catalog-add";
const REMOVE_LABEL: &[u8] = b"ladex-catalog-remove";
/// The most keys besides the publisher's that one add may allow to remove it.
pub const MAX_AUTHORIZED: usize = 8;
/// The most add tags one remove may list.
pub const MAX_OBSERVED: usize = 64;
/// The most origin nodes one version vector may name.
pub const MAX_VERSION_ENTRIES: usize = 1024;

/// What a catalog entry is: a file, possibly inside a folder, or a folder's listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File { parent: Option<String> },
    Folder { bytes: u64, files: u32 },
}

/// A shared file as every node lists it. Who has it right now is not here: that is told live (see availability.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub file_id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    pub created_at_ms: u64,
    /// Names the file's exact content: the root of its manifest (see manifest.rs).
    pub manifest_root: String,
    /// The browser device that shared it, or empty.
    pub uploader_id: SessionId,
    pub kind: EntryKind,
}

impl CatalogEntry {
    // Signed content cannot be cleaned up in transit, so anything that sanitizing would change is refused.
    fn is_valid(&self) -> bool {
        validate::is_valid_id(&self.file_id)
            && validate::sanitize_file_name(&self.name) == self.name
            && validate::sanitize_mime(&self.mime_type) == self.mime_type
            && self.size <= validate::MAX_SAFE_INTEGER
            && self.created_at_ms <= validate::MAX_SAFE_INTEGER
            && validate::is_valid_root(&self.manifest_root)
            && (self.uploader_id.is_empty() || validate::is_valid_id(&self.uploader_id))
            && self.kind.is_valid()
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, EntryKind::Folder { .. })
    }

    /// The folder this file is in, if any.
    pub fn parent(&self) -> Option<&str> {
        match &self.kind {
            EntryKind::File { parent } => parent.as_deref(),
            EntryKind::Folder { .. } => None,
        }
    }
}

impl EntryKind {
    fn is_valid(&self) -> bool {
        match self {
            EntryKind::File { parent } => parent.as_deref().is_none_or(validate::is_valid_id),
            EntryKind::Folder { bytes, files } => *bytes <= validate::MAX_SAFE_INTEGER && *files <= validate::MAX_FOLDER_FILES,
        }
    }
}

/// A live entry with the node that published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub entry: CatalogEntry,
    pub publisher: NodeId,
}

/// A node's signed statement that it shares a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Add {
    /// Tags this add; unique because a node's clock never repeats a stamp.
    pub stamp: Stamp,
    /// This op's place in its origin's run of ops: 1, 2, 3 and so on, with no gaps.
    pub seq: u64,
    #[serde(with = "hex_public_key")]
    pub publisher: VerifyingKey,
    #[serde(with = "hex_public_keys")]
    pub authorized: Vec<VerifyingKey>,
    pub entry: CatalogEntry,
    #[serde(with = "hex_signature")]
    signature: Signature,
}

impl Add {
    pub fn sign(identity: &Identity, stamp: Stamp, seq: u64, authorized: Vec<VerifyingKey>, entry: CatalogEntry) -> Self {
        let publisher = identity.public_key();
        let signature = identity.sign(&add_signed_bytes(&stamp, seq, &publisher, &authorized, &entry));
        Self { stamp, seq, publisher, authorized, entry, signature }
    }

    fn may_be_removed_by(&self, key: &VerifyingKey) -> bool {
        self.publisher == *key || self.authorized.contains(key)
    }

    fn check(&self) -> Result<(), Refusal> {
        let signed = add_signed_bytes(&self.stamp, self.seq, &self.publisher, &self.authorized, &self.entry);
        if !identity::verify(&self.publisher, &signed, &self.signature) {
            return Err(Refusal::BadSignature);
        }
        check_stamp(&self.stamp, self.seq, &self.publisher)?;
        if self.authorized.len() > MAX_AUTHORIZED {
            return Err(Refusal::TooManyAuthorized);
        }
        if !self.entry.is_valid() {
            return Err(Refusal::InvalidEntry);
        }
        Ok(())
    }
}

/// A node's signed statement that it saw these adds of a file and took them down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remove {
    pub stamp: Stamp,
    pub seq: u64,
    #[serde(with = "hex_public_key")]
    pub remover: VerifyingKey,
    pub file_id: String,
    pub observed: Vec<Stamp>,
    #[serde(with = "hex_signature")]
    signature: Signature,
}

impl Remove {
    pub fn sign(identity: &Identity, stamp: Stamp, seq: u64, file_id: String, observed: Vec<Stamp>) -> Self {
        let remover = identity.public_key();
        let signature = identity.sign(&remove_signed_bytes(&stamp, seq, &remover, &file_id, &observed));
        Self { stamp, seq, remover, file_id, observed, signature }
    }

    fn removes(&self, add: &Add) -> bool {
        self.file_id == add.entry.file_id && add.may_be_removed_by(&self.remover)
    }

    fn check(&self) -> Result<(), Refusal> {
        let signed = remove_signed_bytes(&self.stamp, self.seq, &self.remover, &self.file_id, &self.observed);
        if !identity::verify(&self.remover, &signed, &self.signature) {
            return Err(Refusal::BadSignature);
        }
        check_stamp(&self.stamp, self.seq, &self.remover)?;
        if !validate::is_valid_id(&self.file_id) || self.observed.is_empty() || self.observed.len() > MAX_OBSERVED {
            return Err(Refusal::InvalidRemove);
        }
        Ok(())
    }
}

/// What nodes send each other to change the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Add(Add),
    Remove(Remove),
}

impl Op {
    pub fn stamp(&self) -> &Stamp {
        match self {
            Op::Add(add) => &add.stamp,
            Op::Remove(remove) => &remove.stamp,
        }
    }

    pub fn seq(&self) -> u64 {
        match self {
            Op::Add(add) => add.seq,
            Op::Remove(remove) => remove.seq,
        }
    }

    /// The file the op is about.
    pub fn file_id(&self) -> &str {
        match self {
            Op::Add(add) => &add.entry.file_id,
            Op::Remove(remove) => &remove.file_id,
        }
    }

    /// The node that made the op.
    pub fn origin(&self) -> &NodeId {
        &self.stamp().node
    }
}

/// For each origin node, how far its run of ops is held with nothing missing before that point.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<NodeId, u64>);

impl VersionVector {
    pub fn get(&self, origin: &str) -> u64 {
        self.0.get(origin).copied().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a vector from another node is small enough and names only well-formed nodes.
    pub fn is_plausible(&self) -> bool {
        self.0.len() <= MAX_VERSION_ENTRIES && self.0.keys().all(|origin| validate::is_valid_id(origin))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    AlreadyKnown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadSignature,
    UnsetStamp,
    UnsetSequence,
    /// The node already used this sequence number for a different op.
    SequenceReused,
    /// The stamp names a different node than the key that signed.
    StampNotFromSigner,
    TooManyAuthorized,
    InvalidEntry,
    InvalidRemove,
    Full,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::BadSignature => "its signature is invalid",
            Refusal::UnsetStamp => "it has no timestamp",
            Refusal::UnsetSequence => "it has no sequence number",
            Refusal::SequenceReused => "its sequence number was already used by another op",
            Refusal::StampNotFromSigner => "its timestamp belongs to a different node than its signer",
            Refusal::TooManyAuthorized => "it authorizes too many nodes",
            Refusal::InvalidEntry => "its file entry is not valid",
            Refusal::InvalidRemove => "its list of removed adds is not valid",
            Refusal::Full => "the catalog is full",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpublishError {
    NotShared,
    /// The file is shared, but not by this node or on its authority.
    NotAllowed,
}

pub struct Catalog {
    adds: HashMap<Stamp, Add>,
    removes: HashMap<Stamp, Remove>,
    // Add tag -> the removes that list it. A remove may arrive before its add, so this is kept apart from `adds`.
    removed_by: HashMap<Stamp, Vec<Stamp>>,
    origins: HashMap<NodeId, OriginRun>,
    capacity: usize,
}

// One origin node's ops by sequence number; the frontier is the last number below which none is missing.
#[derive(Default)]
struct OriginRun {
    stamps: BTreeMap<u64, Stamp>,
    frontier: u64,
}

impl Catalog {
    /// `capacity` bounds the adds and the removes kept, so a misbehaving node cannot grow the catalog without limit.
    pub fn new(capacity: usize) -> Self {
        Self { adds: HashMap::new(), removes: HashMap::new(), removed_by: HashMap::new(), origins: HashMap::new(), capacity }
    }

    /// Shares a file in this node's name.
    pub fn publish(
        &mut self,
        identity: &Identity,
        clock: &Clock,
        entry: CatalogEntry,
        authorized: Vec<VerifyingKey>,
    ) -> Result<Add, Refusal> {
        let add = Add::sign(identity, clock.now(), self.next_seq(identity.node_id()), authorized, entry);
        self.apply_add(add.clone())?;
        Ok(add)
    }

    /// Takes down every add of the file that this node is allowed to remove.
    pub fn unpublish(&mut self, identity: &Identity, clock: &Clock, file_id: &str) -> Result<Remove, UnpublishError> {
        let live = self.live_adds(file_id);
        if live.is_empty() {
            return Err(UnpublishError::NotShared);
        }
        let key = identity.public_key();
        let mut observed: Vec<Stamp> = live.iter().filter(|add| add.may_be_removed_by(&key)).map(|add| add.stamp.clone()).collect();
        if observed.is_empty() {
            return Err(UnpublishError::NotAllowed);
        }
        // A remove that does not fit them all leaves the rest for a second call.
        observed.truncate(MAX_OBSERVED);
        let remove = Remove::sign(identity, clock.now(), self.next_seq(identity.node_id()), file_id.to_string(), observed);
        self.apply_remove(remove.clone()).map_err(|_| UnpublishError::NotAllowed)?;
        Ok(remove)
    }

    /// Verifies an op from any source and merges it; applying the same ops in any order gives the same catalog.
    pub fn apply(&mut self, op: Op) -> Result<Outcome, Refusal> {
        match op {
            Op::Add(add) => self.apply_add(add),
            Op::Remove(remove) => self.apply_remove(remove),
        }
    }

    /// Every op this catalog holds, for saving it or sending it to a node that is missing some.
    pub fn ops(&self) -> impl Iterator<Item = Op> + '_ {
        let adds = self.adds.values().cloned().map(Op::Add);
        adds.chain(self.removes.values().cloned().map(Op::Remove))
    }

    pub fn contains(&self, file_id: &str) -> bool {
        self.get(file_id).is_some()
    }

    /// The entry shown for a file: the newest of its live adds, which is the same on every node.
    pub fn get(&self, file_id: &str) -> Option<&CatalogEntry> {
        self.live_adds(file_id).into_iter().max_by(|a, b| a.stamp.cmp(&b.stamp)).map(|add| &add.entry)
    }

    /// Every shared file once, ordered by id.
    pub fn entries(&self) -> Vec<&CatalogEntry> {
        self.live().into_iter().map(|add| &add.entry).collect()
    }

    /// Every shared file once with the node that published it, ordered by id.
    pub fn listing(&self) -> Vec<Listed> {
        self.live().into_iter().map(|add| Listed { entry: add.entry.clone(), publisher: add.stamp.node.clone() }).collect()
    }

    /// One file as listed, if it is shared.
    pub fn listed(&self, file_id: &str) -> Option<Listed> {
        let add = self.live_adds(file_id).into_iter().max_by(|a, b| a.stamp.cmp(&b.stamp))?;
        Some(Listed { entry: add.entry.clone(), publisher: add.stamp.node.clone() })
    }

    /// Whether the file was shared and every add of it has since been taken down.
    pub fn was_removed(&self, file_id: &str) -> bool {
        let mut adds = self.adds.values().filter(|add| add.entry.file_id == file_id).peekable();
        adds.peek().is_some() && adds.all(|add| !self.is_live(add))
    }

    fn live(&self) -> Vec<&Add> {
        let mut newest: HashMap<&str, &Add> = HashMap::new();
        for add in self.adds.values().filter(|add| self.is_live(add)) {
            let slot = newest.entry(&add.entry.file_id).or_insert(add);
            if add.stamp > slot.stamp {
                *slot = add;
            }
        }
        let mut live: Vec<&Add> = newest.into_values().collect();
        live.sort_by(|a, b| a.entry.file_id.cmp(&b.entry.file_id));
        live
    }

    pub fn op_count(&self) -> usize {
        self.adds.len() + self.removes.len()
    }

    fn apply_add(&mut self, add: Add) -> Result<Outcome, Refusal> {
        if self.adds.contains_key(&add.stamp) {
            return Ok(Outcome::AlreadyKnown);
        }
        add.check()?;
        self.check_sequence(&add.stamp, add.seq)?;
        if self.adds.len() >= self.capacity {
            return Err(Refusal::Full);
        }
        self.record_sequence(&add.stamp, add.seq);
        self.adds.insert(add.stamp.clone(), add);
        Ok(Outcome::Applied)
    }

    fn apply_remove(&mut self, remove: Remove) -> Result<Outcome, Refusal> {
        if self.removes.contains_key(&remove.stamp) {
            return Ok(Outcome::AlreadyKnown);
        }
        remove.check()?;
        self.check_sequence(&remove.stamp, remove.seq)?;
        if self.removes.len() >= self.capacity {
            return Err(Refusal::Full);
        }
        self.record_sequence(&remove.stamp, remove.seq);
        for tag in &remove.observed {
            self.removed_by.entry(tag.clone()).or_default().push(remove.stamp.clone());
        }
        self.removes.insert(remove.stamp.clone(), remove);
        Ok(Outcome::Applied)
    }

    /// What this catalog holds of each origin's run; a node sends this to ask for what it lacks.
    pub fn version(&self) -> VersionVector {
        VersionVector(self.origins.iter().filter(|(_, run)| run.frontier > 0).map(|(origin, run)| (origin.clone(), run.frontier)).collect())
    }

    /// The ops a node with version `theirs` lacks, each origin's in order.
    pub fn ops_missing_from(&self, theirs: &VersionVector) -> Vec<Op> {
        let mut origins: Vec<(&NodeId, &OriginRun)> = self.origins.iter().collect();
        origins.sort_by_key(|(origin, _)| *origin);
        let mut missing = Vec::new();
        for (origin, run) in origins {
            let first = theirs.get(origin).saturating_add(1);
            missing.extend(run.stamps.range(first..).filter_map(|(_, stamp)| self.op_with_stamp(stamp)));
        }
        missing
    }

    /// The newest stamp held from a node, for its clock to resume after a restart.
    pub fn latest_stamp(&self, origin: &str) -> Option<&Stamp> {
        self.origins.get(origin)?.stamps.values().next_back()
    }

    fn next_seq(&self, origin: &str) -> u64 {
        self.origins.get(origin).and_then(|run| run.stamps.keys().next_back()).map_or(1, |last| last + 1)
    }

    fn check_sequence(&self, stamp: &Stamp, seq: u64) -> Result<(), Refusal> {
        match self.origins.get(&stamp.node).and_then(|run| run.stamps.get(&seq)) {
            Some(held) if held != stamp => Err(Refusal::SequenceReused),
            _ => Ok(()),
        }
    }

    fn record_sequence(&mut self, stamp: &Stamp, seq: u64) {
        let run = self.origins.entry(stamp.node.clone()).or_default();
        run.stamps.insert(seq, stamp.clone());
        while run.stamps.contains_key(&(run.frontier + 1)) {
            run.frontier += 1;
        }
    }

    fn op_with_stamp(&self, stamp: &Stamp) -> Option<Op> {
        match self.adds.get(stamp) {
            Some(add) => Some(Op::Add(add.clone())),
            None => self.removes.get(stamp).cloned().map(Op::Remove),
        }
    }

    fn live_adds(&self, file_id: &str) -> Vec<&Add> {
        self.adds.values().filter(|add| add.entry.file_id == file_id && self.is_live(add)).collect()
    }

    // Whether the remove authority is checked against each add, so a remove that arrived first still counts later.
    fn is_live(&self, add: &Add) -> bool {
        let removers = self.removed_by.get(&add.stamp).into_iter().flatten();
        !removers.filter_map(|stamp| self.removes.get(stamp)).any(|remove| remove.removes(add))
    }
}

fn check_stamp(stamp: &Stamp, seq: u64, signer: &VerifyingKey) -> Result<(), Refusal> {
    if !stamp.is_set() {
        return Err(Refusal::UnsetStamp);
    }
    if seq == 0 {
        return Err(Refusal::UnsetSequence);
    }
    if stamp.node != identity::node_id_for(signer) {
        return Err(Refusal::StampNotFromSigner);
    }
    Ok(())
}

fn add_signed_bytes(stamp: &Stamp, seq: u64, publisher: &VerifyingKey, authorized: &[VerifyingKey], entry: &CatalogEntry) -> Vec<u8> {
    let mut bytes = ADD_LABEL.to_vec();
    put_stamp(&mut bytes, stamp);
    bytes.extend_from_slice(&seq.to_be_bytes());
    bytes.extend_from_slice(publisher.as_bytes());
    put_len(&mut bytes, authorized.len());
    for key in authorized {
        bytes.extend_from_slice(key.as_bytes());
    }
    put_text(&mut bytes, &entry.file_id);
    put_text(&mut bytes, &entry.name);
    bytes.extend_from_slice(&entry.size.to_be_bytes());
    put_text(&mut bytes, &entry.mime_type);
    bytes.extend_from_slice(&entry.created_at_ms.to_be_bytes());
    put_text(&mut bytes, &entry.manifest_root);
    put_text(&mut bytes, &entry.uploader_id);
    put_kind(&mut bytes, &entry.kind);
    bytes
}

fn remove_signed_bytes(stamp: &Stamp, seq: u64, remover: &VerifyingKey, file_id: &str, observed: &[Stamp]) -> Vec<u8> {
    let mut bytes = REMOVE_LABEL.to_vec();
    put_stamp(&mut bytes, stamp);
    bytes.extend_from_slice(&seq.to_be_bytes());
    bytes.extend_from_slice(remover.as_bytes());
    put_text(&mut bytes, file_id);
    put_len(&mut bytes, observed.len());
    for tag in observed {
        put_stamp(&mut bytes, tag);
    }
    bytes
}

// Every variable-length field is length-prefixed, so two different ops never share the same signed bytes.
fn put_len(bytes: &mut Vec<u8>, len: usize) {
    bytes.extend_from_slice(&(len as u32).to_be_bytes());
}

fn put_text(bytes: &mut Vec<u8>, text: &str) {
    put_len(bytes, text.len());
    bytes.extend_from_slice(text.as_bytes());
}

fn put_kind(bytes: &mut Vec<u8>, kind: &EntryKind) {
    match kind {
        EntryKind::File { parent } => {
            bytes.push(0);
            bytes.push(parent.is_some() as u8);
            put_text(bytes, parent.as_deref().unwrap_or(""));
        }
        EntryKind::Folder { bytes: size, files } => {
            bytes.push(1);
            bytes.extend_from_slice(&size.to_be_bytes());
            bytes.extend_from_slice(&files.to_be_bytes());
        }
    }
}

fn put_stamp(bytes: &mut Vec<u8>, stamp: &Stamp) {
    bytes.extend_from_slice(&stamp.wall.to_be_bytes());
    bytes.extend_from_slice(&stamp.counter.to_be_bytes());
    put_text(bytes, &stamp.node);
}

mod hex_public_keys {
    use ed25519_dalek::VerifyingKey;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(keys: &[VerifyingKey], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(keys.iter().map(|key| hex::encode(key.as_bytes())))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<VerifyingKey>, D::Error> {
        let encoded = Vec::<String>::deserialize(deserializer)?;
        encoded.iter().map(|key| parse_key(key).map_err(D::Error::custom)).collect()
    }

    fn parse_key(encoded: &str) -> Result<VerifyingKey, String> {
        let bytes: [u8; 32] = hex::decode(encoded).map_err(|e| e.to_string())?.try_into().map_err(|_| "a public key is 32 bytes")?;
        VerifyingKey::from_bytes(&bytes).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CAPACITY: usize = 1000;

    struct TestNode {
        identity: Identity,
        clock: Clock,
    }

    fn node(seed: u8) -> TestNode {
        let identity = Identity::from_secret(&[seed; 32]);
        let clock = Clock::new(identity.node_id().to_string());
        TestNode { identity, clock }
    }

    fn entry(file_id: &str) -> CatalogEntry {
        CatalogEntry {
            file_id: file_id.into(),
            name: format!("{file_id}.txt"),
            size: 10,
            mime_type: "text/plain".into(),
            created_at_ms: 1,
            manifest_root: "ab".repeat(32),
            uploader_id: "device_a".into(),
            kind: EntryKind::File { parent: None },
        }
    }

    fn catalog() -> Catalog {
        Catalog::new(CAPACITY)
    }

    fn publish(catalog: &mut Catalog, node: &TestNode, file_id: &str) -> Add {
        catalog.publish(&node.identity, &node.clock, entry(file_id), vec![]).unwrap()
    }

    fn shared_ids(catalog: &Catalog) -> Vec<String> {
        catalog.entries().iter().map(|entry| entry.file_id.clone()).collect()
    }

    #[test]
    fn a_published_file_appears_in_the_catalog() {
        let mut catalog = catalog();
        publish(&mut catalog, &node(1), "f1");
        assert_eq!(catalog.get("f1"), Some(&entry("f1")));
    }

    #[test]
    fn the_publisher_can_take_its_file_down() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        catalog.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        assert!(!catalog.contains("f1"));
    }

    #[test]
    fn a_file_can_be_shared_again_after_it_was_taken_down() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        catalog.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        publish(&mut catalog, &alice, "f1");
        assert!(catalog.contains("f1"));
    }

    #[test]
    fn another_node_cannot_take_down_a_file_it_did_not_publish() {
        let (mut catalog, alice, mallory) = (catalog(), node(1), node(2));
        publish(&mut catalog, &alice, "f1");
        assert_eq!(catalog.unpublish(&mallory.identity, &mallory.clock, "f1"), Err(UnpublishError::NotAllowed));
    }

    #[test]
    fn a_forged_remove_from_another_node_does_not_hide_the_file() {
        let (mut catalog, alice, mallory) = (catalog(), node(1), node(2));
        let add = publish(&mut catalog, &alice, "f1");
        let remove = Remove::sign(&mallory.identity, mallory.clock.now(), 1, "f1".into(), vec![add.stamp]);
        assert_eq!(catalog.apply(Op::Remove(remove)), Ok(Outcome::Applied));
        assert!(catalog.contains("f1"));
    }

    #[test]
    fn a_node_the_publisher_authorized_can_take_the_file_down() {
        let (mut catalog, alice, bob) = (catalog(), node(1), node(2));
        catalog.publish(&alice.identity, &alice.clock, entry("f1"), vec![bob.identity.public_key()]).unwrap();
        catalog.unpublish(&bob.identity, &bob.clock, "f1").unwrap();
        assert!(!catalog.contains("f1"));
    }

    #[test]
    fn unpublishing_a_file_nobody_shares_is_an_error() {
        let (mut catalog, alice) = (catalog(), node(1));
        assert_eq!(catalog.unpublish(&alice.identity, &alice.clock, "f1"), Err(UnpublishError::NotShared));
    }

    #[test]
    fn an_add_made_while_a_remove_was_in_flight_survives_it() {
        let (mut alice_view, alice, bob) = (catalog(), node(1), node(2));
        publish(&mut alice_view, &alice, "f1");
        let remove = alice_view.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        let concurrent_add = Add::sign(&bob.identity, bob.clock.now(), 1, vec![], entry("f1"));

        let mut replica = catalog();
        replica.apply(Op::Add(concurrent_add)).unwrap();
        replica.apply(Op::Remove(remove)).unwrap();
        assert!(replica.contains("f1"));
    }

    #[test]
    fn taking_down_one_publishers_copy_leaves_another_publishers_copy() {
        let (mut catalog, alice, bob) = (catalog(), node(1), node(2));
        publish(&mut catalog, &alice, "f1");
        publish(&mut catalog, &bob, "f1");
        catalog.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        assert!(catalog.contains("f1"));
    }

    #[test]
    fn a_remove_that_arrives_before_its_add_still_takes_effect() {
        let (mut origin, alice) = (catalog(), node(1));
        let add = publish(&mut origin, &alice, "f1");
        let remove = origin.unpublish(&alice.identity, &alice.clock, "f1").unwrap();

        let mut replica = catalog();
        replica.apply(Op::Remove(remove)).unwrap();
        replica.apply(Op::Add(add)).unwrap();
        assert!(!replica.contains("f1"));
    }

    #[test]
    fn a_remove_does_not_hide_a_different_file_with_the_same_tag() {
        let (mut catalog, alice) = (catalog(), node(1));
        let add = publish(&mut catalog, &alice, "f1");
        let remove = Remove::sign(&alice.identity, alice.clock.now(), 2, "f2".into(), vec![add.stamp]);
        catalog.apply(Op::Remove(remove)).unwrap();
        assert!(catalog.contains("f1"));
    }

    #[test]
    fn applying_the_same_op_twice_changes_nothing() {
        let (mut catalog, alice) = (catalog(), node(1));
        let add = publish(&mut catalog, &alice, "f1");
        assert_eq!(catalog.apply(Op::Add(add)), Ok(Outcome::AlreadyKnown));
        assert_eq!(catalog.op_count(), 1);
    }

    #[test]
    fn an_add_with_a_changed_name_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let mut add = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], entry("f1"));
        add.entry.name = "evil.exe".into();
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::BadSignature));
    }

    #[test]
    fn an_add_with_a_changed_authorized_list_is_refused() {
        let (mut catalog, alice, mallory) = (catalog(), node(1), node(2));
        let mut add = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], entry("f1"));
        add.authorized.push(mallory.identity.public_key());
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::BadSignature));
    }

    #[test]
    fn a_remove_with_a_changed_tag_list_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let add = publish(&mut catalog, &alice, "f1");
        let mut remove = Remove::sign(&alice.identity, alice.clock.now(), 2, "f1".into(), vec![add.stamp]);
        remove.observed.push(Stamp { wall: 1, counter: 0, node: "x".into() });
        assert_eq!(catalog.apply(Op::Remove(remove)), Err(Refusal::BadSignature));
    }

    #[test]
    fn an_add_stamped_as_another_node_is_refused() {
        let (mut catalog, alice, mallory) = (catalog(), node(1), node(2));
        let add = Add::sign(&mallory.identity, alice.clock.now(), 1, vec![], entry("f1"));
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::StampNotFromSigner));
    }

    #[test]
    fn an_add_without_a_stamp_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let add = Add::sign(&alice.identity, Stamp::default(), 1, vec![], entry("f1"));
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::UnsetStamp));
    }

    #[test]
    fn an_add_with_an_unsafe_file_name_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let unsafe_entry = CatalogEntry { name: "../etc/passwd".into(), ..entry("f1") };
        let add = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], unsafe_entry);
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::InvalidEntry));
    }

    fn refused_entry(change: impl FnOnce(&mut CatalogEntry)) -> Result<Outcome, Refusal> {
        let alice = node(1);
        let mut unsafe_entry = entry("f1");
        change(&mut unsafe_entry);
        catalog().apply(Op::Add(Add::sign(&alice.identity, alice.clock.now(), 1, vec![], unsafe_entry)))
    }

    #[test]
    fn an_add_with_a_malformed_file_id_is_refused() {
        assert_eq!(refused_entry(|e| e.file_id = "../x".into()), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn an_add_with_a_size_a_browser_cannot_represent_is_refused() {
        assert_eq!(refused_entry(|e| e.size = u64::MAX), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn an_add_with_a_malformed_manifest_root_is_refused() {
        assert_eq!(refused_entry(|e| e.manifest_root = "not-a-root".into()), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn an_add_with_a_malformed_uploader_is_refused() {
        assert_eq!(refused_entry(|e| e.uploader_id = "bad id".into()), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn an_add_without_an_uploader_is_accepted() {
        assert_eq!(refused_entry(|e| e.uploader_id = String::new()), Ok(Outcome::Applied));
    }

    #[test]
    fn an_add_inside_a_folder_with_a_malformed_id_is_refused() {
        assert_eq!(refused_entry(|e| e.kind = EntryKind::File { parent: Some("bad folder".into()) }), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn a_folder_listing_more_files_than_allowed_is_refused() {
        let too_many = validate::MAX_FOLDER_FILES + 1;
        assert_eq!(refused_entry(|e| e.kind = EntryKind::Folder { bytes: 1, files: too_many }), Err(Refusal::InvalidEntry));
    }

    #[test]
    fn a_changed_manifest_root_breaks_the_signature() {
        let (mut catalog, alice) = (catalog(), node(1));
        let mut add = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], entry("f1"));
        add.entry.manifest_root = "cd".repeat(32);
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::BadSignature));
    }

    #[test]
    fn a_taken_down_file_is_reported_as_removed_and_a_shared_one_is_not() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        publish(&mut catalog, &alice, "f2");
        catalog.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        assert_eq!((catalog.was_removed("f1"), catalog.was_removed("f2"), catalog.was_removed("never_seen")), (true, false, false));
    }

    #[test]
    fn a_listed_file_names_the_node_that_published_it() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        assert_eq!(catalog.listed("f1").unwrap().publisher, alice.identity.node_id());
    }

    #[test]
    fn an_add_authorizing_too_many_nodes_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let keys = vec![node(2).identity.public_key(); MAX_AUTHORIZED + 1];
        let add = Add::sign(&alice.identity, alice.clock.now(), 1, keys, entry("f1"));
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::TooManyAuthorized));
    }

    #[test]
    fn a_remove_listing_no_adds_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let remove = Remove::sign(&alice.identity, alice.clock.now(), 1, "f1".into(), vec![]);
        assert_eq!(catalog.apply(Op::Remove(remove)), Err(Refusal::InvalidRemove));
    }

    #[test]
    fn a_full_catalog_refuses_new_adds() {
        let (mut catalog, alice) = (Catalog::new(1), node(1));
        publish(&mut catalog, &alice, "f1");
        let second = Add::sign(&alice.identity, alice.clock.now(), 2, vec![], entry("f2"));
        assert_eq!(catalog.apply(Op::Add(second)), Err(Refusal::Full));
    }

    #[test]
    fn ops_survive_a_postcard_round_trip_with_their_signatures() {
        let (mut origin, alice, bob) = (catalog(), node(1), node(2));
        origin.publish(&alice.identity, &alice.clock, entry("f1"), vec![bob.identity.public_key()]).unwrap();
        origin.unpublish(&bob.identity, &bob.clock, "f1").unwrap();
        origin.publish(&alice.identity, &alice.clock, entry("f2"), vec![]).unwrap();

        let mut replica = catalog();
        for op in origin.ops() {
            let bytes = postcard::to_stdvec(&op).unwrap();
            replica.apply(postcard::from_bytes(&bytes).unwrap()).unwrap();
        }
        assert_eq!(shared_ids(&replica), vec!["f2".to_string()]);
    }

    #[test]
    fn the_entry_shown_is_the_newest_add_of_the_file() {
        let (mut catalog, alice, bob) = (catalog(), node(1), node(2));
        publish(&mut catalog, &alice, "f1");
        let newer = CatalogEntry { name: "renamed.txt".into(), ..entry("f1") };
        catalog.publish(&bob.identity, &bob.clock, newer.clone(), vec![]).unwrap();
        assert_eq!(catalog.get("f1"), Some(&newer));
    }

    fn replica_with(ops: impl IntoIterator<Item = Op>) -> Catalog {
        let mut replica = catalog();
        for op in ops {
            replica.apply(op).unwrap();
        }
        replica
    }

    fn stamps_of(ops: &[Op]) -> Vec<Stamp> {
        ops.iter().map(|op| op.stamp().clone()).collect()
    }

    #[test]
    fn an_empty_catalog_has_an_empty_version() {
        assert!(catalog().version().is_empty());
    }

    #[test]
    fn the_version_counts_each_origins_ops() {
        let (mut catalog, alice, bob) = (catalog(), node(1), node(2));
        publish(&mut catalog, &alice, "f1");
        publish(&mut catalog, &alice, "f2");
        publish(&mut catalog, &bob, "f3");
        let version = catalog.version();
        assert_eq!((version.get(alice.identity.node_id()), version.get(bob.identity.node_id())), (2, 1));
    }

    #[test]
    fn a_gap_in_an_origins_ops_holds_the_version_back_until_it_is_filled() {
        let (mut origin, alice) = (catalog(), node(1));
        let ops: Vec<Op> = ["f1", "f2", "f3"].iter().map(|id| Op::Add(publish(&mut origin, &alice, id))).collect();

        let mut replica = catalog();
        replica.apply(ops[0].clone()).unwrap();
        replica.apply(ops[2].clone()).unwrap();
        assert_eq!(replica.version().get(alice.identity.node_id()), 1);
        replica.apply(ops[1].clone()).unwrap();
        assert_eq!(replica.version().get(alice.identity.node_id()), 3);
    }

    #[test]
    fn a_node_is_sent_only_the_ops_beyond_its_version() {
        let (mut origin, alice) = (catalog(), node(1));
        let first = publish(&mut origin, &alice, "f1");
        let second = publish(&mut origin, &alice, "f2");
        let behind = replica_with([Op::Add(first)]);
        assert_eq!(stamps_of(&origin.ops_missing_from(&behind.version())), vec![second.stamp]);
    }

    #[test]
    fn a_node_that_is_up_to_date_is_sent_nothing() {
        let (mut origin, alice) = (catalog(), node(1));
        publish(&mut origin, &alice, "f1");
        assert!(origin.ops_missing_from(&origin.version()).is_empty());
    }

    #[test]
    fn a_node_with_an_empty_version_is_sent_everything_in_order() {
        let (mut origin, alice) = (catalog(), node(1));
        publish(&mut origin, &alice, "f1");
        origin.unpublish(&alice.identity, &alice.clock, "f1").unwrap();
        publish(&mut origin, &alice, "f2");
        let sent = origin.ops_missing_from(&VersionVector::default());
        assert_eq!(sent.iter().map(Op::seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn an_op_held_beyond_a_gap_is_still_sent_to_a_node_that_lacks_it() {
        let (mut origin, alice) = (catalog(), node(1));
        let ops: Vec<Op> = ["f1", "f2", "f3"].iter().map(|id| Op::Add(publish(&mut origin, &alice, id))).collect();
        let gappy = replica_with([ops[0].clone(), ops[2].clone()]);
        assert_eq!(stamps_of(&gappy.ops_missing_from(&VersionVector::default())), stamps_of(&[ops[0].clone(), ops[2].clone()]));
    }

    #[test]
    fn a_second_op_with_a_used_sequence_number_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        let reusing = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], entry("f2"));
        assert_eq!(catalog.apply(Op::Add(reusing)), Err(Refusal::SequenceReused));
    }

    #[test]
    fn an_op_with_sequence_zero_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let add = Add::sign(&alice.identity, alice.clock.now(), 0, vec![], entry("f1"));
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::UnsetSequence));
    }

    #[test]
    fn an_add_with_a_changed_sequence_number_is_refused() {
        let (mut catalog, alice) = (catalog(), node(1));
        let mut add = Add::sign(&alice.identity, alice.clock.now(), 1, vec![], entry("f1"));
        add.seq = 5;
        assert_eq!(catalog.apply(Op::Add(add)), Err(Refusal::BadSignature));
    }

    #[test]
    fn a_vector_naming_a_malformed_node_is_not_plausible() {
        let vector: VersionVector =
            postcard::from_bytes(&postcard::to_stdvec(&BTreeMap::from([("../x".to_string(), 1u64)])).unwrap()).unwrap();
        assert!(!vector.is_plausible());
    }

    #[test]
    fn the_latest_stamp_of_a_node_is_that_of_its_last_op() {
        let (mut catalog, alice) = (catalog(), node(1));
        publish(&mut catalog, &alice, "f1");
        let last = publish(&mut catalog, &alice, "f2");
        assert_eq!(catalog.latest_stamp(alice.identity.node_id()), Some(&last.stamp));
    }

    // Builds a history on one catalog that sees everything, so each remove lists the adds it could have seen.
    fn history(actions: &[(u8, u8, bool)]) -> (Vec<Op>, Vec<String>) {
        let nodes = [node(1), node(2), node(3)];
        let mut origin = catalog();
        for &(who, file, is_remove) in actions {
            let node = &nodes[who as usize];
            let file_id = format!("f{file}");
            if is_remove {
                let _ = origin.unpublish(&node.identity, &node.clock, &file_id);
            } else {
                let authorized = vec![nodes[(who as usize + 1) % 3].identity.public_key()];
                origin.publish(&node.identity, &node.clock, entry(&file_id), authorized).unwrap();
            }
        }
        (origin.ops().collect(), shared_ids(&origin))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn replicas_converge_whatever_order_ops_arrive_in(
            (expected, ops) in prop::collection::vec((0u8..3, 0u8..4, any::<bool>()), 0..40).prop_flat_map(|actions| {
                let (ops, expected) = history(&actions);
                (Just(expected), Just(ops).prop_shuffle())
            })
        ) {
            let mut replica = catalog();
            for op in ops {
                replica.apply(op).unwrap();
            }
            prop_assert_eq!(shared_ids(&replica), expected);
        }

        #[test]
        fn two_replicas_converge_after_exchanging_what_each_lacks(
            (ops, holders) in prop::collection::vec((0u8..3, 0u8..4, any::<bool>()), 0..40).prop_flat_map(|actions| {
                let (ops, _) = history(&actions);
                let count = ops.len();
                (Just(ops), prop::collection::vec(0u8..3, count))
            })
        ) {
            // 0: only the first replica holds the op, 1: only the second, 2: both.
            let first_ops: Vec<Op> = ops.iter().zip(&holders).filter(|(_, h)| **h != 1).map(|(op, _)| op.clone()).collect();
            let second_ops: Vec<Op> = ops.iter().zip(&holders).filter(|(_, h)| **h != 0).map(|(op, _)| op.clone()).collect();
            let (mut first, mut second) = (replica_with(first_ops), replica_with(second_ops));

            let for_second = first.ops_missing_from(&second.version());
            let for_first = second.ops_missing_from(&first.version());
            for op in for_second {
                second.apply(op).unwrap();
            }
            for op in for_first {
                first.apply(op).unwrap();
            }
            prop_assert_eq!(shared_ids(&first), shared_ids(&second));
            prop_assert_eq!(first.op_count(), second.op_count());
            prop_assert_eq!(first.version(), second.version());
        }

        #[test]
        fn many_replicas_given_the_same_ops_in_different_orders_agree(
            (ops, orders) in prop::collection::vec((0u8..3, 0u8..4, any::<bool>()), 0..40).prop_flat_map(|actions| {
                let (ops, _) = history(&actions);
                let orders = prop::collection::vec(Just(ops.clone()).prop_shuffle(), 5);
                (Just(ops), orders)
            })
        ) {
            let replicas: Vec<Catalog> = orders.into_iter().map(replica_with).collect();
            for replica in &replicas {
                prop_assert_eq!(shared_ids(replica), shared_ids(&replicas[0]));
                prop_assert_eq!(replica.version(), replicas[0].version());
                prop_assert_eq!(replica.op_count(), ops.len());
            }
        }

        #[test]
        fn delivering_every_op_twice_changes_nothing(
            (expected, ops) in prop::collection::vec((0u8..3, 0u8..4, any::<bool>()), 0..40).prop_flat_map(|actions| {
                let (ops, expected) = history(&actions);
                (Just(expected), Just(ops).prop_shuffle())
            })
        ) {
            let mut replica = catalog();
            for op in ops.iter().chain(ops.iter()) {
                replica.apply(op.clone()).unwrap();
            }
            prop_assert_eq!(shared_ids(&replica), expected);
        }
    }
}
