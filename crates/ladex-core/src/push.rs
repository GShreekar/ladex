//! Sending a file to one device: it stays out of the catalog, the receiving device is asked first, and its node then
//! fetches the file from the sending node alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::mesh::MeshMessage;
use crate::store::{Blob, Origin, StoreError};
use crate::types::{NodeId, OfferedFile, ServerMessage, SessionId};
use crate::{state, transfer, validate, websocket, NodeState};

// Offers from other nodes waiting for an answer; more than this are declined, so a node cannot be flooded with prompts.
const MAX_WAITING_OFFERS: usize = 32;
const UNFINISHED_PUSH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A file one device is sending to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedFile {
    pub file_id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    pub manifest_root: String,
    pub from_session: SessionId,
    pub to_session: SessionId,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    NoSuchDevice,
    DeviceUnreachable,
    NoSuchOffer,
    Store(StoreError),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NoSuchDevice => f.write_str("that device is not connected"),
            Refused::DeviceUnreachable => f.write_str("the node that device is connected to cannot be reached"),
            Refused::NoSuchOffer => f.write_str("that file is no longer on offer"),
            Refused::Store(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Clone)]
struct Push {
    file: PushedFile,
    // The node at the other end; this node itself when both devices are connected here.
    other_node: NodeId,
    // Whether this node holds the original, as the sending device uploaded it.
    is_sender: bool,
    is_accepted: bool,
    offered_at: Instant,
}

/// The pushes this node is part of, as the sending or the receiving end. Kept in memory: a restart ends them.
#[derive(Default)]
pub struct Pushes {
    by_file: Mutex<HashMap<String, Push>>,
}

impl Pushes {
    pub fn contains(&self, file_id: &str) -> bool {
        self.by_file.lock().unwrap().contains_key(file_id)
    }

    /// Whether `device` is sending this file, so its upload can be answered again without a second offer.
    pub fn is_sent_by(&self, file_id: &str, device: &str) -> bool {
        self.by_file.lock().unwrap().get(file_id).is_some_and(|push| push.is_sender && push.file.from_session == device)
    }

    /// Whether `node` may fetch this file: only the receiving node, and only once its device has accepted.
    pub fn may_fetch(&self, file_id: &str, node: &NodeId) -> bool {
        self.by_file.lock().unwrap().get(file_id).is_some_and(|push| push.is_sender && push.is_accepted && push.other_node == *node)
    }

    fn add(&self, push: Push) -> bool {
        let mut pushes = self.by_file.lock().unwrap();
        let waiting = pushes.values().filter(|held| !held.is_sender && !held.is_accepted).count();
        if pushes.contains_key(&push.file.file_id) || (!push.is_sender && waiting >= MAX_WAITING_OFFERS) {
            return false;
        }
        pushes.insert(push.file.file_id.clone(), push);
        true
    }

    fn get(&self, file_id: &str) -> Option<Push> {
        self.by_file.lock().unwrap().get(file_id).cloned()
    }

    fn sent_to(&self, file_id: &str, session: &str) -> Option<Push> {
        self.get(file_id).filter(|push| push.file.to_session == session)
    }

    fn mark_accepted(&self, file_id: &str) {
        if let Some(push) = self.by_file.lock().unwrap().get_mut(file_id) {
            push.is_accepted = true;
        }
    }

    fn take_if(&self, file_id: &str, matches: impl Fn(&Push) -> bool) -> Option<Push> {
        let mut pushes = self.by_file.lock().unwrap();
        if !pushes.get(file_id).is_some_and(matches) {
            return None;
        }
        pushes.remove(file_id)
    }

    fn take_older_than(&self, age: Duration) -> Vec<Push> {
        let mut pushes = self.by_file.lock().unwrap();
        let expired: Vec<String> =
            pushes.values().filter(|push| push.offered_at.elapsed() >= age).map(|push| push.file.file_id.clone()).collect();
        expired.iter().filter_map(|file_id| pushes.remove(file_id)).collect()
    }
}

/// The node a device is connected to, if the device is here now.
pub async fn hosting_node(state: &NodeState, session: &str) -> Option<NodeId> {
    let peers = state.local_peers.read().await;
    peers.get(session).filter(|device| !device.left).and_then(|device| device.hosting_node_id.clone())
}

fn prompt(file: &PushedFile) -> ServerMessage {
    ServerMessage::IncomingFileOffer {
        file_id: file.file_id.clone(),
        from_session_id: file.from_session.clone(),
        pushed: Some(OfferedFile { name: file.name.clone(), size: file.size }),
    }
}

async fn send_to_node(state: &NodeState, node: &NodeId, message: MeshMessage) -> bool {
    state.mesh_peers.read().await.get(node).is_some_and(|peer| peer.sender.send(message).is_ok())
}

/// Offers a complete file this node holds to the device it was uploaded for.
pub async fn offer(state: &NodeState, file: PushedFile) -> Result<(), Refused> {
    let to_node = hosting_node(state, &file.to_session).await.ok_or(Refused::NoSuchDevice)?;
    let push = Push { file: file.clone(), other_node: to_node.clone(), is_sender: true, is_accepted: false, offered_at: Instant::now() };
    if !state.pushes.add(push) {
        return Ok(());
    }
    let is_delivered = if to_node == state.node_id {
        let tab = state.local_senders.read().await.get(&file.to_session).cloned();
        tab.is_some_and(|tab| tab.send(prompt(&file)).is_ok())
    } else {
        send_to_node(state, &to_node, MeshMessage::PushOffer { file: file.clone() }).await
    };
    if !is_delivered {
        state.pushes.take_if(&file.file_id, |_| true);
        return Err(if to_node == state.node_id { Refused::NoSuchDevice } else { Refused::DeviceUnreachable });
    }
    tracing::info!("Push: offered {} ({} bytes) to {}", file.name, file.size, file.to_session);
    Ok(())
}

// Nothing a node says about a file is trusted: names are cleaned, and ids, root and size must be well formed.
fn cleaned(file: &PushedFile) -> Option<PushedFile> {
    let is_well_formed = validate::is_valid_id(&file.file_id)
        && validate::is_valid_id(&file.from_session)
        && validate::is_valid_id(&file.to_session)
        && validate::is_valid_root(&file.manifest_root)
        && file.size <= validate::MAX_SAFE_INTEGER;
    is_well_formed.then(|| PushedFile {
        name: validate::sanitize_file_name(&file.name),
        mime_type: validate::sanitize_mime(&file.mime_type),
        ..file.clone()
    })
}

/// Another node offers a file to a device connected here; the device is asked, or the offer is declined at once.
pub async fn on_offer(state: &NodeState, from: &NodeId, file: &PushedFile) {
    let Some(file) = cleaned(file) else {
        tracing::warn!("Push: malformed offer from {from} dropped");
        return;
    };
    if hosting_node(state, &file.from_session).await.as_ref() != Some(from) {
        tracing::warn!("Push: offer from {from} refused, it does not host the device it names");
        return;
    }
    let tab = state.local_senders.read().await.get(&file.to_session).cloned();
    let is_new_id = state.store.get(&file.file_id).is_none() && !state.catalog.contains(&file.file_id);
    let push = Push { file: file.clone(), other_node: from.clone(), is_sender: false, is_accepted: false, offered_at: Instant::now() };
    let is_held = tab.is_some() && is_new_id && state.pushes.add(push);
    if !is_held {
        send_to_node(state, from, MeshMessage::PushAnswer { file_id: file.file_id, accepted: false }).await;
        return;
    }
    if let Some(tab) = tab {
        let _ = tab.send(prompt(&file));
    }
}

/// The receiving device accepts: its node starts fetching the file from the sending node.
pub async fn accept(state: &NodeState, session: &str, file_id: &str) -> Result<(), Refused> {
    let push = state.pushes.sent_to(file_id, session).ok_or(Refused::NoSuchOffer)?;
    if push.is_accepted {
        return Ok(());
    }
    if push.is_sender {
        // Both devices are on this node, so the file is already where the receiving device fetches it from.
        state.pushes.mark_accepted(file_id);
        let delivered = ServerMessage::FilePushDelivered { file_id: file_id.to_string(), to_session_id: session.to_string() };
        websocket::send_to(state, &push.file.from_session, delivered).await;
        return Ok(());
    }
    state::make_room(state, push.file.size).await;
    let blob = match state.store.create_with(file_id, push.file.size, Origin::Pushed) {
        Ok(blob) => blob,
        Err(e) => {
            state.pushes.take_if(file_id, |_| true);
            send_to_node(state, &push.other_node, MeshMessage::PushAnswer { file_id: file_id.to_string(), accepted: false }).await;
            return Err(Refused::Store(e));
        }
    };
    blob.expect_root(&push.file.manifest_root);
    state.pushes.mark_accepted(file_id);
    // The answer goes first, so the sending node already allows the fetch when the first request arrives.
    send_to_node(state, &push.other_node, MeshMessage::PushAnswer { file_id: file_id.to_string(), accepted: true }).await;
    transfer::fetch_from(state, &blob, push.other_node);
    Ok(())
}

/// The receiving device declines, or gives up after accepting; false if this is not a file pushed to it.
pub async fn decline(state: &NodeState, session: &str, file_id: &str) -> bool {
    let Some(push) = state.pushes.take_if(file_id, |push| push.file.to_session == session) else { return false };
    if push.is_sender {
        withdraw(state, &push.file).await;
    } else {
        transfer::cancel(state, file_id);
        state.store.remove(file_id);
        send_to_node(state, &push.other_node, MeshMessage::PushAnswer { file_id: file_id.to_string(), accepted: false }).await;
    }
    true
}

// The sending node's copy only existed for this push.
async fn withdraw(state: &NodeState, file: &PushedFile) {
    state.store.remove(&file.file_id);
    let declined = ServerMessage::FileOfferDeclined { file_id: file.file_id.clone(), from_session_id: file.to_session.clone() };
    websocket::send_to(state, &file.from_session, declined).await;
}

/// The receiving node passes on its device's answer.
pub async fn on_answer(state: &NodeState, from: &NodeId, file_id: &str, accepted: bool) {
    let is_from_receiver = |push: &Push| push.is_sender && push.other_node == *from;
    if accepted {
        if state.pushes.get(file_id).is_some_and(|push| is_from_receiver(&push)) {
            state.pushes.mark_accepted(file_id);
        }
        return;
    }
    if let Some(push) = state.pushes.take_if(file_id, is_from_receiver) {
        withdraw(state, &push.file).await;
    }
}

/// This node now has the whole of a file pushed to one of its devices, and tells the sending node.
pub async fn on_fetched(state: &NodeState, file_id: &str) {
    let Some(push) = state.pushes.get(file_id).filter(|push| !push.is_sender) else { return };
    tracing::info!("Push: received {} ({} bytes) for {}", push.file.name, push.file.size, push.file.to_session);
    send_to_node(state, &push.other_node, MeshMessage::PushReceived { file_id: file_id.to_string() }).await;
}

/// The receiving node has the whole file, so this node's copy is no longer needed.
pub async fn on_received(state: &NodeState, from: &NodeId, file_id: &str) {
    let Some(push) = state.pushes.take_if(file_id, |push| push.is_sender && push.is_accepted && push.other_node == *from) else { return };
    state.store.remove(file_id);
    let delivered = ServerMessage::FilePushDelivered { file_id: file_id.to_string(), to_session_id: push.file.to_session.clone() };
    websocket::send_to(state, &push.file.from_session, delivered).await;
}

/// The file for the device it was pushed to, once accepted; a fetch that stopped is started again.
pub fn open(state: &NodeState, file_id: &str, session: &str) -> Option<(Arc<Blob>, PushedFile)> {
    let push = state.pushes.sent_to(file_id, session).filter(|push| push.is_accepted)?;
    let blob = state.store.get(file_id)?;
    if !push.is_sender {
        transfer::fetch_from(state, &blob, push.other_node);
    }
    Some((blob, push.file))
}

/// Ends pushes nobody finished, and deletes pushed files no push refers to any more (all of them, after a restart).
pub fn collect_garbage(state: &NodeState) {
    for push in state.pushes.take_older_than(UNFINISHED_PUSH_TTL) {
        tracing::info!("Push: {} was never finished; dropping it", push.file.file_id);
        transfer::cancel(state, &push.file.file_id);
        state.store.remove(&push.file.file_id);
    }
    // A pushed file still being uploaded has no push yet, so only finished ones are judged.
    let is_leftover = |blob: &Arc<Blob>| {
        blob.origin() == Origin::Pushed && blob.is_complete() && !blob.is_being_written() && !state.pushes.contains(blob.id())
    };
    for blob in state.store.all().into_iter().filter(is_leftover) {
        state.store.remove(blob.id());
    }
}
