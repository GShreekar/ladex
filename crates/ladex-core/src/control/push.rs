//! `ladex send --to`: files offered to one device, sent from a device that exists only while they are on offer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use super::share::{self, Progress};
use super::{Device, Event, Events, Reply};
use crate::push::{self, PushedFile};
use crate::store::Origin;
use crate::types::{PeerInfo, ServerMessage, SessionId};
use crate::{state, validate, websocket, NodeState};

const EXPIRY_CHECK_INTERVAL: Duration = Duration::from_secs(1);

async fn find_device(state: &NodeState, wanted: &str) -> anyhow::Result<Device> {
    let devices = super::devices(state).await;
    if let Some(device) = devices.iter().find(|device| device.id == wanted) {
        return Ok(device.clone());
    }
    let mut named: Vec<&Device> = devices.iter().filter(|device| device.name.eq_ignore_ascii_case(wanted)).collect();
    match named.len() {
        0 => bail!("no device called {wanted:?} is connected (see `ladex devices`)"),
        1 => Ok(named.remove(0).clone()),
        _ => {
            let ids: Vec<&str> = named.iter().map(|device| device.id.as_str()).collect();
            bail!("{} devices are called {wanted:?}; name one by its id: {}", named.len(), ids.join(", "))
        }
    }
}

// The command line as a device: a push has to come from one, and its answers are sent to it.
struct CommandLineDevice {
    id: SessionId,
    inbox: UnboundedReceiver<ServerMessage>,
}

impl CommandLineDevice {
    async fn join(state: &NodeState) -> Self {
        let id = share::new_id("cli");
        let (tab, inbox) = mpsc::unbounded_channel();
        state.local_senders.write().await.insert(id.clone(), tab);
        let device = PeerInfo {
            session_id: id.clone(),
            connected_at: chrono::Utc::now(),
            user_agent: Some("LADEX command line".to_string()),
            hosting_node_id: Some(state.node_id.clone()),
            node_rtt_ms: None,
            left: false,
            left_at: None,
            hosting_node_name: Some(state.node_name.clone()),
            nickname: Some(validate::clean_label(&state.node_name, validate::MAX_NICKNAME_CHARS)).filter(|name| !name.is_empty()),
            version: state.clock.now(),
        };
        let total_peers = {
            let mut peers = state.local_peers.write().await;
            peers.insert(id.clone(), device.clone());
            peers.len()
        };
        websocket::broadcast(state, ServerMessage::PeerJoined { peer: device.clone(), total_peers }).await;
        state::push_peer_to_mesh(&state.mesh_peers, device).await;
        Self { id, inbox }
    }

    async fn leave(self, state: &NodeState) {
        state.local_senders.write().await.remove(&self.id);
        websocket::cleanup_peer(state, &self.id).await;
    }
}

/// Offers each file to one device and waits for its answers; returns which were delivered and which were not.
pub(super) async fn send_to_device(state: &NodeState, paths: &[PathBuf], wanted: &str, events: &Events) -> anyhow::Result<Reply> {
    let target = find_device(state, wanted).await?;
    let mut total = 0;
    for path in paths {
        total += share::regular_file_size(path).map_err(|e| anyhow!("{e:#} (only files can be sent to one device)"))?;
    }
    let mut sender = CommandLineDevice::join(state).await;
    let outcome = offer_and_wait(state, &mut sender, paths, &target, &mut Progress::new(events, total)).await;
    sender.leave(state).await;
    outcome
}

async fn offer_and_wait(
    state: &NodeState,
    sender: &mut CommandLineDevice,
    paths: &[PathBuf],
    target: &Device,
    progress: &mut Progress<'_>,
) -> anyhow::Result<Reply> {
    let mut waiting: HashMap<String, String> = HashMap::new();
    for path in paths {
        let file = offer(state, &sender.id, path, target, progress).await?;
        progress.events.tell(Event::Offered { name: file.name.clone(), device: target.name.clone() }).await;
        waiting.insert(file.file_id, file.name);
    }

    let (mut delivered, mut declined) = (Vec::new(), Vec::new());
    let mut seen_gone: HashSet<String> = HashSet::new();
    let mut expiry_check = tokio::time::interval(EXPIRY_CHECK_INTERVAL);
    while !waiting.is_empty() {
        tokio::select! {
            biased;
            message = sender.inbox.recv() => match message {
                Some(ServerMessage::FilePushDelivered { file_id, .. }) => {
                    if let Some(name) = waiting.remove(&file_id) {
                        progress.events.tell(Event::Delivered { name: name.clone() }).await;
                        delivered.push(name);
                    }
                }
                Some(ServerMessage::FileOfferDeclined { file_id, .. }) => {
                    if let Some(name) = waiting.remove(&file_id) {
                        progress.events.tell(Event::Declined { name: name.clone() }).await;
                        declined.push(name);
                    }
                }
                Some(_) => {}
                None => break,
            },
            _ = expiry_check.tick() => {
                // An offer nobody answered is dropped after a day. Its answer may still be on its way the first
                // time it is seen gone, so it only counts as unanswered the second time.
                let gone: Vec<String> = waiting.keys().filter(|id| !state.pushes.contains(id)).cloned().collect();
                for id in gone {
                    if !seen_gone.insert(id.clone()) {
                        declined.extend(waiting.remove(&id));
                    }
                }
            }
        }
    }
    declined.extend(waiting.into_values());
    Ok(Reply::Pushed { delivered, declined })
}

async fn offer(state: &NodeState, from: &str, path: &Path, target: &Device, progress: &mut Progress<'_>) -> anyhow::Result<PushedFile> {
    let id = share::new_id("file");
    let manifest_root = share::import(state, path, &id, Origin::Pushed, progress).await?;
    let size = state.store.get(&id).map_or(0, |blob| blob.size());
    let file = PushedFile {
        file_id: id.clone(),
        name: share::name_of(path),
        size,
        mime_type: share::mime_type_of(path),
        manifest_root,
        from_session: from.to_string(),
        to_session: target.id.clone(),
    };
    if let Err(refused) = push::offer(state, file.clone()).await {
        state.store.remove(&id);
        bail!("could not send {}: {refused}", path.display());
    }
    Ok(file)
}
