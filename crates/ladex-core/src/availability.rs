//! Which nodes hold which files right now, as each node last told us; a node's entries go when it disconnects.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::bitmap::Bitmap;
use crate::mesh::MeshMessage;
use crate::store::chunk_count;
use crate::types::NodeId;
use crate::{state, transfer, validate, NodeState};

/// The most file ids one `HaveAll` or `Lost` message carries.
pub const MAX_IDS_PER_MESSAGE: usize = 512;
// Partial files a node announces on connect; the rest are told as they progress.
const MAX_PARTIALS_ANNOUNCED: usize = 256;

/// The most files one node may claim to have, so a misbehaving peer cannot grow this without limit.
pub const MAX_FILES_PER_NODE: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Have {
    Complete,
    Partial(Bitmap),
}

#[derive(Default)]
pub struct Availability {
    by_node: Mutex<HashMap<NodeId, HashMap<String, Have>>>,
}

impl Availability {
    /// `node` has every chunk of these files.
    pub fn set_complete(&self, node: &str, file_ids: &[String]) {
        let mut by_node = self.by_node.lock().unwrap();
        let files = by_node.entry(node.to_string()).or_default();
        for id in file_ids {
            if files.contains_key(id) || files.len() < MAX_FILES_PER_NODE {
                files.insert(id.clone(), Have::Complete);
            }
        }
    }

    /// `node` has the chunks in `map`; a full map means the whole file and an empty one means nothing.
    pub fn set_partial(&self, node: &str, file_id: &str, map: Bitmap) {
        if map.is_full() {
            return self.set_complete(node, &[file_id.to_string()]);
        }
        if map.count() == 0 {
            return self.lose(node, &[file_id.to_string()]);
        }
        let mut by_node = self.by_node.lock().unwrap();
        let files = by_node.entry(node.to_string()).or_default();
        if files.contains_key(file_id) || files.len() < MAX_FILES_PER_NODE {
            files.insert(file_id.to_string(), Have::Partial(map));
        }
    }

    /// `node` no longer has these files.
    pub fn lose(&self, node: &str, file_ids: &[String]) {
        if let Some(files) = self.by_node.lock().unwrap().get_mut(node) {
            for id in file_ids {
                files.remove(id);
            }
        }
    }

    pub fn drop_node(&self, node: &str) {
        self.by_node.lock().unwrap().remove(node);
    }

    pub fn get(&self, node: &str, file_id: &str) -> Option<Have> {
        self.by_node.lock().unwrap().get(node)?.get(file_id).cloned()
    }

    /// The nodes that have the whole file, in a stable order.
    pub fn hosts(&self, file_id: &str) -> Vec<NodeId> {
        let by_node = self.by_node.lock().unwrap();
        let mut hosts: Vec<NodeId> =
            by_node.iter().filter(|(_, files)| files.get(file_id) == Some(&Have::Complete)).map(|(node, _)| node.clone()).collect();
        hosts.sort();
        hosts
    }
}

/// Tells a newly connected peer what this node holds, whole and in part.
pub fn announce_all(peer_tx: &mpsc::UnboundedSender<MeshMessage>, state: &NodeState) {
    let blobs = state.store.all();
    let complete: Vec<String> = blobs.iter().filter(|blob| blob.is_complete()).map(|blob| blob.id().to_string()).collect();
    for file_ids in complete.chunks(MAX_IDS_PER_MESSAGE) {
        let _ = peer_tx.send(MeshMessage::HaveAll { file_ids: file_ids.to_vec() });
    }
    let partials = blobs.iter().filter(|blob| !blob.is_complete() && blob.bitmap().count() > 0);
    for blob in partials.take(MAX_PARTIALS_ANNOUNCED) {
        let map = blob.bitmap();
        let _ = peer_tx.send(MeshMessage::Have { file_id: blob.id().to_string(), chunks: map.len(), bitmap: map.to_hex() });
    }
}

/// Tells every connected peer that this node now has these files whole.
pub async fn announce_complete(state: &NodeState, file_ids: &[String]) {
    send_to_all(state, file_ids, |file_ids| MeshMessage::HaveAll { file_ids }).await;
}

/// Tells every connected peer that this node no longer has these files.
pub async fn announce_lost(state: &NodeState, file_ids: &[String]) {
    send_to_all(state, file_ids, |file_ids| MeshMessage::Lost { file_ids }).await;
}

async fn send_to_all(state: &NodeState, file_ids: &[String], message: impl Fn(Vec<String>) -> MeshMessage) {
    let peers = state.mesh_peers.read().await;
    for batch in file_ids.chunks(MAX_IDS_PER_MESSAGE) {
        for handle in peers.values() {
            let _ = handle.sender.send(message(batch.to_vec()));
        }
    }
}

/// A peer says it has these files whole.
pub async fn on_have_all(state: &NodeState, from: &NodeId, file_ids: &[String]) {
    let Some(file_ids) = well_formed_ids(file_ids) else { return };
    state.availability.set_complete(from, &file_ids);
    hosts_changed(state).await;
}

/// A peer says it no longer has these files.
pub async fn on_lost(state: &NodeState, from: &NodeId, file_ids: &[String]) {
    let Some(file_ids) = well_formed_ids(file_ids) else { return };
    state.availability.lose(from, &file_ids);
    hosts_changed(state).await;
}

/// A peer says which chunks of one file it has.
pub async fn on_have(state: &NodeState, from: &NodeId, file_id: &str, chunks: u32, bitmap_hex: &str) {
    if !validate::is_valid_id(file_id) {
        return;
    }
    // A map is only believed for a file whose size is known, since that fixes how many chunks there are.
    let Some(size) = state::known_size(state, file_id) else { return };
    if chunks != chunk_count(size) {
        return;
    }
    let Some(map) = Bitmap::from_hex(chunks, bitmap_hex) else { return };
    let was_host = state.availability.get(from, file_id) == Some(Have::Complete);
    state.availability.set_partial(from, file_id, map);
    if was_host != (state.availability.get(from, file_id) == Some(Have::Complete)) {
        hosts_changed(state).await;
    } else {
        transfer::sources_changed(state);
    }
}

async fn hosts_changed(state: &NodeState) {
    transfer::sources_changed(state);
    state::broadcast_catalog(state).await;
}

fn well_formed_ids(file_ids: &[String]) -> Option<Vec<String>> {
    (file_ids.len() <= MAX_IDS_PER_MESSAGE && file_ids.iter().all(|id| validate::is_valid_id(id))).then(|| file_ids.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn map(len: u32, present: &[u32]) -> Bitmap {
        let mut map = Bitmap::new(len);
        for index in present {
            map.set(*index);
        }
        map
    }

    #[test]
    fn a_node_that_announced_a_file_is_a_host_of_it() {
        let availability = Availability::default();
        availability.set_complete("a", &ids(&["f1"]));
        assert_eq!(availability.hosts("f1"), vec!["a".to_string()]);
    }

    #[test]
    fn a_node_with_only_some_chunks_is_not_a_host() {
        let availability = Availability::default();
        availability.set_partial("a", "f1", map(4, &[0, 1]));
        assert!(availability.hosts("f1").is_empty());
        assert_eq!(availability.get("a", "f1"), Some(Have::Partial(map(4, &[0, 1]))));
    }

    #[test]
    fn a_partial_that_fills_up_becomes_a_host() {
        let availability = Availability::default();
        availability.set_partial("a", "f1", map(2, &[0]));
        availability.set_partial("a", "f1", Bitmap::full(2));
        assert_eq!(availability.hosts("f1"), vec!["a".to_string()]);
    }

    #[test]
    fn an_empty_map_forgets_the_file() {
        let availability = Availability::default();
        availability.set_complete("a", &ids(&["f1"]));
        availability.set_partial("a", "f1", Bitmap::new(2));
        assert_eq!(availability.get("a", "f1"), None);
    }

    #[test]
    fn a_lost_file_is_no_longer_hosted() {
        let availability = Availability::default();
        availability.set_complete("a", &ids(&["f1", "f2"]));
        availability.lose("a", &ids(&["f1"]));
        assert_eq!((availability.hosts("f1").len(), availability.hosts("f2").len()), (0, 1));
    }

    #[test]
    fn a_disconnected_node_hosts_nothing() {
        let availability = Availability::default();
        availability.set_complete("a", &ids(&["f1"]));
        availability.drop_node("a");
        assert!(availability.hosts("f1").is_empty());
    }

    #[test]
    fn hosts_are_listed_in_a_stable_order() {
        let availability = Availability::default();
        availability.set_complete("b", &ids(&["f1"]));
        availability.set_complete("a", &ids(&["f1"]));
        assert_eq!(availability.hosts("f1"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_node_cannot_claim_more_files_than_the_limit() {
        let availability = Availability::default();
        let many: Vec<String> = (0..MAX_FILES_PER_NODE + 5).map(|i| format!("f{i}")).collect();
        availability.set_complete("a", &many);
        assert!(availability.get("a", "f0").is_some());
        assert!(availability.get("a", &format!("f{}", MAX_FILES_PER_NODE + 4)).is_none());
    }
}
