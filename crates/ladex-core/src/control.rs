//! How `ladex send`, `get` and the other commands reach the node running on this machine: one request, answered with
//! progress events and then a reply.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::receive::{self, ReceiveRoot};
use crate::store::CHUNK_SIZE;
use crate::types::{FileView, NodeId, PeerInfo, SessionId};
use crate::{state, tls, NodeState};

mod push;
pub mod server;
mod share;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// Shares files and folders with everyone, or sends files to one device.
    Send {
        paths: Vec<PathBuf>,
        to: Option<String>,
    },
    /// Saves a shared file or folder, named by id or by name, into a directory.
    Get {
        file: String,
        into: PathBuf,
    },
    /// Shares a directory as a folder; `replaces` is the folder it was shared as before it changed.
    Seed {
        dir: PathBuf,
        replaces: Option<String>,
    },
    List,
    Devices,
    Invite,
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Progress {
        name: String,
        done: u64,
        total: u64,
    },
    /// A file was offered to a device, which has yet to answer.
    Offered {
        name: String,
        device: String,
    },
    Delivered {
        name: String,
    },
    Declined {
        name: String,
    },
    Done {
        reply: Box<Reply>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Shared { files: Vec<FileView> },
    Pushed { delivered: Vec<String>, declined: Vec<String> },
    Saved { path: PathBuf, file: FileView },
    Files { files: Vec<FileView> },
    Devices { devices: Vec<Device>, nodes: Vec<Node> },
    Invite(Invite),
    Status(Box<Status>),
}

/// A browser or command-line device on the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: SessionId,
    pub name: String,
    pub node_id: NodeId,
    pub node_name: String,
    /// Connected to this node, not to another one.
    pub is_here: bool,
}

/// Another node this one is connected to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub name: String,
    pub address: SocketAddr,
    pub rtt_ms: Option<u32>,
}

/// What another device needs to join: where the page is, and how to check it is the right one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub url: Option<String>,
    pub certificate_fingerprint: Option<String>,
    pub needs_passphrase: bool,
    /// What another node passes to `--peer` to connect to this one.
    pub peer_address: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub node_id: NodeId,
    pub node_name: String,
    pub local_ip: Option<IpAddr>,
    pub http_port: u16,
    pub quic_port: u16,
    pub discovery_port: Option<u16>,
    pub is_encrypted: bool,
    pub has_passphrase: bool,
    pub nodes: Vec<Node>,
    pub devices: usize,
    pub shared_files: usize,
    pub downloads_running: usize,
    pub data_dir: PathBuf,
    pub store_used_bytes: u64,
    pub store_quota_bytes: u64,
    pub disk_free_bytes: Option<u64>,
}

/// What the node was started with that its shared state does not record.
#[derive(Debug, Clone)]
pub struct Settings {
    pub data_dir: PathBuf,
    /// None when discovery is switched off.
    pub discovery_port: Option<u16>,
}

/// The command stopped listening, so there is nobody to finish the work for.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the command was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Where a request's events go; sending fails once the command has gone away.
pub struct Events(mpsc::Sender<Event>);

impl Events {
    pub fn new(events: mpsc::Sender<Event>) -> Self {
        Self(events)
    }

    // For news of work that carries on whether or not the command is still there to hear it.
    async fn tell(&self, event: Event) {
        if self.0.send(event).await.is_err() {
            tracing::debug!("Control: the command left before its work was done");
        }
    }

    // Progress is only worth showing now and then; one dropped because the command is busy reading is no loss.
    fn progress(&self, name: &str, done: u64, total: u64) -> anyhow::Result<()> {
        match self.0.try_send(Event::Progress { name: name.to_string(), done, total }) {
            Err(mpsc::error::TrySendError::Closed(_)) => Err(anyhow!(Cancelled)),
            _ => Ok(()),
        }
    }
}

/// Carries out one request and sends its outcome as the last event.
pub async fn answer(state: &NodeState, settings: &Settings, request: Request, events: Events) {
    let outcome = match run(state, settings, request, &events).await {
        Ok(reply) => Event::Done { reply: Box::new(reply) },
        Err(e) => Event::Failed { message: format!("{e:#}") },
    };
    let _ = events.0.send(outcome).await;
}

async fn run(state: &NodeState, settings: &Settings, request: Request, events: &Events) -> anyhow::Result<Reply> {
    match request {
        Request::Send { paths, to: None } => Ok(Reply::Shared { files: share::share_paths(state, &paths, events).await? }),
        Request::Send { paths, to: Some(device) } => push::send_to_device(state, &paths, &device, events).await,
        Request::Seed { dir, replaces } => Ok(Reply::Shared { files: vec![share::seed(state, &dir, replaces.as_deref(), events).await?] }),
        Request::Get { file, into } => save(state, &file, into, events).await,
        Request::List => Ok(Reply::Files { files: state::file_views(state) }),
        Request::Devices => Ok(Reply::Devices { devices: devices(state).await, nodes: nodes(state).await }),
        Request::Invite => Ok(Reply::Invite(invite(state))),
        Request::Status => Ok(Reply::Status(Box::new(status(state, settings).await))),
    }
}

/// The shared file a user means by an id or a name; a name several files share is refused with their ids.
pub fn find_file(state: &NodeState, wanted: &str) -> anyhow::Result<FileView> {
    if let Some(file) = state::file_view(state, wanted) {
        return Ok(file);
    }
    let mut named: Vec<FileView> =
        state::file_views(state).into_iter().filter(|file| file.parent.is_none() && file.name == wanted).collect();
    match named.len() {
        0 => bail!("nothing called {wanted:?} is shared (see `ladex ls`)"),
        1 => Ok(named.remove(0)),
        _ => {
            let ids: Vec<&str> = named.iter().map(|file| file.id.as_str()).collect();
            bail!("{} things are called {wanted:?}; name one by its id: {}", named.len(), ids.join(", "))
        }
    }
}

async fn save(state: &NodeState, wanted: &str, into: PathBuf, events: &Events) -> anyhow::Result<Reply> {
    let file = find_file(state, wanted)?;
    let root = ReceiveRoot::open(&into).with_context(|| format!("could not open {}", into.display()))?;
    let total = if file.is_folder { file.folder_bytes } else { file.size };
    let saving = receive::save_shared(state, &root, &file.id);
    tokio::pin!(saving);
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    loop {
        tokio::select! {
            saved = &mut saving => return Ok(Reply::Saved { path: saved?, file: file.clone() }),
            _ = tick.tick() => events.progress(&file.name, fetched_bytes(state, &file), total)?,
        }
    }
}

// How much of a file, or of a folder's files, this node holds so far.
fn fetched_bytes(state: &NodeState, file: &FileView) -> u64 {
    let held = |id: &str| state.store.get(id).map_or(0, |blob| (blob.bitmap().count() as u64 * CHUNK_SIZE).min(blob.size()));
    if !file.is_folder {
        return held(&file.id);
    }
    let children = state.catalog.listing().into_iter().filter(|listed| listed.entry.parent() == Some(file.id.as_str()));
    children.map(|listed| held(&listed.entry.file_id)).sum()
}

fn device_name(device: &PeerInfo) -> String {
    device.nickname.clone().or_else(|| device.user_agent.clone()).unwrap_or_default()
}

/// Every device connected right now, on this node or another, by name.
pub async fn devices(state: &NodeState) -> Vec<Device> {
    let peers = state.local_peers.read().await;
    let mut devices: Vec<Device> = peers
        .values()
        .filter(|device| !device.left)
        .map(|device| Device {
            id: device.session_id.clone(),
            name: device_name(device),
            node_id: device.hosting_node_id.clone().unwrap_or_default(),
            node_name: device.hosting_node_name.clone().unwrap_or_default(),
            is_here: device.hosting_node_id.as_ref() == Some(&state.node_id),
        })
        .collect();
    devices.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    devices
}

async fn nodes(state: &NodeState) -> Vec<Node> {
    let peers = state.mesh_peers.read().await;
    let mut nodes: Vec<Node> = peers
        .values()
        .map(|peer| Node { id: peer.node_id.clone(), name: peer.node_name.clone(), address: peer.addr, rtt_ms: peer.rtt_ms })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    nodes
}

fn is_encrypted(state: &NodeState) -> bool {
    !state.tls_fingerprint.is_empty()
}

fn invite(state: &NodeState) -> Invite {
    let scheme = if is_encrypted(state) { "https" } else { "http" };
    Invite {
        url: state.local_ip.map(|ip| format!("{scheme}://{ip}:{}", state.http_port)),
        certificate_fingerprint: is_encrypted(state).then(|| tls::format_fingerprint(&state.tls_fingerprint)),
        needs_passphrase: state.passphrase.is_some(),
        peer_address: state.local_ip.map(|ip| format!("{ip}:{}", state.quic_port)),
    }
}

async fn status(state: &NodeState, settings: &Settings) -> Status {
    Status {
        version: env!("CARGO_PKG_VERSION").to_string(),
        node_id: state.node_id.clone(),
        node_name: state.node_name.clone(),
        local_ip: state.local_ip,
        http_port: state.http_port,
        quic_port: state.quic_port,
        discovery_port: settings.discovery_port,
        is_encrypted: is_encrypted(state),
        has_passphrase: state.passphrase.is_some(),
        nodes: nodes(state).await,
        devices: devices(state).await.len(),
        shared_files: state.catalog.listing().len(),
        downloads_running: state.transfers.active(),
        data_dir: settings.data_dir.clone(),
        store_used_bytes: state.store.used_bytes(),
        store_quota_bytes: state.store.quota(),
        disk_free_bytes: fs4::available_space(&settings.data_dir).ok(),
    }
}
