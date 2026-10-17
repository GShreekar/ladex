//! Keeps catalogs in step across the mesh: versions are traded on connect, the missing ops follow, and each new op is pushed as it is made.

use tokio::sync::mpsc;

use super::{Add, CatalogEntry, Op, Refusal, Remove, UnpublishError, VersionVector};
use crate::mesh::MeshMessage;
use crate::types::NodeId;
use crate::{hlc, state, NodeState};
use ed25519_dalek::VerifyingKey;

/// The most ops one mesh message carries, in either direction.
pub const MAX_OPS_PER_MESSAGE: usize = 256;

/// Tells a peer what this node holds, so it can answer with what this node lacks.
pub fn offer_version(peer_tx: &mpsc::UnboundedSender<MeshMessage>, state: &NodeState) {
    let _ = peer_tx.send(MeshMessage::CatalogVersion { version: state.catalog.version() });
}

/// A peer told us what it holds: send it the rest.
pub async fn on_version(state: &NodeState, from: &NodeId, version: &VersionVector) {
    if !version.is_plausible() {
        tracing::warn!("Catalog: ignoring a malformed version vector from {from}");
        return;
    }
    let missing = state.catalog.ops_missing_from(version);
    tracing::debug!("Catalog: sending {from} {} op(s) it lacks", missing.len());
    send_to(state, from, &missing).await;
}

/// Ops arrived from a peer, as a reply to our version or pushed after a change.
pub async fn on_ops(state: &NodeState, from: &NodeId, ops: &[Op]) {
    if ops.len() > MAX_OPS_PER_MESSAGE {
        tracing::warn!("Catalog: ignoring {} ops from {from}, more than a message may carry", ops.len());
        return;
    }
    let mut acceptable = Vec::with_capacity(ops.len());
    for op in ops {
        match hlc::check(op.stamp()) {
            Ok(()) => acceptable.push(op.clone()),
            Err(problem) => state::report_clock_problem(state, from, &problem).await,
        }
    }
    let received = state.catalog.receive(acceptable);
    for op in &received.applied {
        let _ = state.clock.observe(op.stamp());
    }
    if let Some(refusal) = received.refused.first() {
        tracing::warn!("Catalog: refused {} op(s) from {from}, the first because {refusal}", received.refused.len());
    }
    if !received.applied.is_empty() {
        let mut affected: Vec<String> = received.applied.iter().map(|op| op.file_id().to_string()).collect();
        affected.sort();
        affected.dedup();
        state::catalog_changed(state, &affected).await;
    }
}

/// Shares a file in this node's name and tells the mesh.
pub async fn publish(state: &NodeState, entry: CatalogEntry, authorized: Vec<VerifyingKey>) -> Result<Add, Refusal> {
    let add = state.catalog.publish(&state.identity, &state.clock, entry, authorized)?;
    push(state, vec![Op::Add(add.clone())]).await;
    state::catalog_changed(state, std::slice::from_ref(&add.entry.file_id)).await;
    Ok(add)
}

/// Takes down this node's shares of a file and tells the mesh.
pub async fn unpublish(state: &NodeState, file_id: &str) -> Result<Remove, UnpublishError> {
    let remove = state.catalog.unpublish(&state.identity, &state.clock, file_id)?;
    push(state, vec![Op::Remove(remove.clone())]).await;
    state::catalog_changed(state, &[file_id.to_string()]).await;
    Ok(remove)
}

/// Takes down several files at once, telling the mesh and the tabs once; returns the ids taken down.
pub async fn unpublish_all(state: &NodeState, file_ids: &[String]) -> Vec<String> {
    let mut removes = Vec::new();
    let mut taken_down = Vec::new();
    for id in file_ids {
        if let Ok(remove) = state.catalog.unpublish(&state.identity, &state.clock, id) {
            removes.push(Op::Remove(remove));
            taken_down.push(id.clone());
        }
    }
    if taken_down.is_empty() {
        return taken_down;
    }
    for batch in removes.chunks(MAX_OPS_PER_MESSAGE) {
        push(state, batch.to_vec()).await;
    }
    state::catalog_changed(state, &taken_down).await;
    taken_down
}

async fn push(state: &NodeState, ops: Vec<Op>) {
    let peers = state.mesh_peers.read().await;
    for handle in peers.values() {
        let _ = handle.sender.send(MeshMessage::CatalogOps { ops: ops.clone() });
    }
}

async fn send_to(state: &NodeState, node: &NodeId, ops: &[Op]) {
    let peers = state.mesh_peers.read().await;
    let Some(handle) = peers.get(node) else { return };
    for batch in ops.chunks(MAX_OPS_PER_MESSAGE) {
        let _ = handle.sender.send(MeshMessage::CatalogOps { ops: batch.to_vec() });
    }
}
