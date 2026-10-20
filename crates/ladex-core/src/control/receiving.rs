//! `ladex receive`: the command line as a device others can send files to, which saves what it accepts.

use std::path::Path;

use anyhow::{anyhow, Context};

use super::push::CommandLineDevice;
use super::{acknowledged, device_name, save, with_progress, Event, Events, Offer, Reply};
use crate::push::{self, Refused};
use crate::receive::{Content, ReceiveRoot};
use crate::store::CHUNK_SIZE;
use crate::trust::TrustedVia;
use crate::types::{NodeId, OfferedFile, ServerMessage, SessionId};
use crate::{state, NodeState};

/// Stays in the mesh as a device called `name`, telling the command of each file offered to it, until the command leaves.
pub(super) async fn receive(state: &NodeState, name: &str, events: &Events) -> anyhow::Result<Reply> {
    let mut device = CommandLineDevice::join(state, name).await;
    events.tell(Event::Joined { device: device.id.clone() }).await;
    loop {
        tokio::select! {
            message = device.inbox.recv() => match message {
                Some(ServerMessage::IncomingFileOffer { file_id, from_session_id, pushed }) => {
                    if let Some(offer) = describe(state, file_id, from_session_id, pushed).await {
                        events.tell(Event::Incoming { offer }).await;
                    }
                }
                Some(_) => {}
                None => break,
            },
            () = events.closed() => break,
        }
    }
    device.leave(state).await;
    Ok(acknowledged("Stopped receiving"))
}

async fn describe(state: &NodeState, file_id: String, from: SessionId, pushed: Option<OfferedFile>) -> Option<Offer> {
    let (name, size, is_direct) = match pushed {
        Some(file) => (file.name, file.size, true),
        None => {
            let file = state::file_view(state, &file_id)?;
            let size = if file.is_folder { file.folder_bytes } else { file.size };
            (file.name, size, false)
        }
    };
    let sender = state.local_peers.read().await.get(&from).cloned();
    let from_name = sender.as_ref().map(device_name).unwrap_or_default();
    let is_trusted = sender.and_then(|device| device.hosting_node_id).is_some_and(|node| is_trusted_node(state, &node));
    let is_accepted_without_asking = state.preferences.get().auto_accept.allows(Some(size), is_trusted);
    Some(Offer { file_id, name, size, from, from_name, is_direct, is_accepted_without_asking })
}

// Devices on this node or on a node paired with it, as the app's window counts them.
fn is_trusted_node(state: &NodeState, node: &NodeId) -> bool {
    let is_paired = || matches!(state.trust.get(node), Ok(Some(known)) if known.trusted_via == TrustedVia::Pairing && !known.revoked);
    *node == state.node_id || is_paired()
}

/// Accepts a file offered to `device` and saves it into `into`: one sent to it alone, or a shared one pointed out to it.
pub(super) async fn accept(state: &NodeState, device: &str, file_id: &str, into: &Path, events: &Events) -> anyhow::Result<Reply> {
    if !state.pushes.contains(file_id) {
        return save(state, file_id, into, events).await;
    }
    let root = ReceiveRoot::open(into).with_context(|| format!("could not open {}", into.display()))?;
    push::accept(state, device, file_id).await.map_err(|refused| anyhow!("{refused}"))?;
    let (blob, file) = push::open(state, file_id, device).ok_or_else(|| anyhow!("{}", Refused::NoSuchOffer))?;
    let content = Content { blob: &blob, size: file.size, manifest_root: &file.manifest_root };
    let held = || (blob.bitmap().count() as u64 * CHUNK_SIZE).min(file.size);
    let saving = root.save_file(&file.name, &content, state.transfers.stall_timeout());
    let path = with_progress(saving, held, &file.name, file.size, events).await?;
    push::finish(state, device, file_id).await;
    Ok(Reply::Received { name: file.name, path })
}
