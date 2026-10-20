//! How `ladex send`, `get` and the other commands reach the node running on this machine: one request, answered with
//! progress events and then a reply.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::guest::sessions::GuestSummary;
use crate::guest::{self, Permission, WaitingUpload};
use crate::receive::{self, ReceiveRoot};
use crate::store::CHUNK_SIZE;
use crate::trust::TrustedNodeView;
use crate::types::{FileView, NodeId, PeerInfo, SessionId, TextMessage};
use crate::{pairing, preferences};
use crate::{state, tls, NodeState};

mod push;
mod receiving;
pub mod server;
mod share;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// The version commands and nodes must share; they are released together, so any difference is a mismatch.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a command or node is told when the other side runs another version; an empty version is one too old to report it.
pub fn version_mismatch_message(node_version: &str, command_version: &str) -> String {
    let named = |version: &str| if version.is_empty() { "an older version".to_string() } else { version.to_string() };
    format!("the running node is {}, this command is {} — update one of them", named(node_version), named(command_version))
}

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
    /// Shows how another device can join, with a new one-time invitation for a browser guest.
    Invite {
        #[serde(default)]
        permission: Permission,
    },
    Status,
    /// Shows the preferences, after applying `update` if there is one.
    Preferences {
        #[serde(default)]
        update: Option<preferences::Update>,
    },
    /// Stops sharing a file or folder this node shared, named by id or by name.
    Unshare {
        file: String,
    },
    /// Shows the room's chat, after posting `message` if there is one.
    Chat {
        message: Option<String>,
    },
    /// Lists the browser guests signed in and their uploads waiting for approval.
    Guests,
    SignOutGuest {
        guest: String,
    },
    /// Accepts a guest's upload that waits for approval, or throws it away.
    AnswerUpload {
        upload: String,
        accepted: bool,
    },
    /// Shows the passphrase the node runs with.
    Passphrase,
    /// Saves a new passphrase for the node's next start.
    ChangePassphrase {
        passphrase: String,
    },
    /// Joins the mesh as a device called `name` and reports each file offered to it, until the command leaves.
    Receive {
        name: String,
    },
    /// Accepts a file offered to the receiving device `device` and saves it into `into`.
    AcceptOffer {
        device: SessionId,
        file: String,
        into: PathBuf,
    },
    /// Turns down a file offered to the receiving device `device` by the device `from`.
    DeclineOffer {
        device: SessionId,
        file: String,
        from: SessionId,
    },
    /// Shows this node's address for pairing and the codes waiting for an answer.
    Pairing,
    /// Accepts pairings from other devices for the next few minutes.
    OpenPairing,
    /// Stops accepting pairings and declines every code still waiting.
    ClosePairing,
    /// Dials the node at `address` to pair with it.
    DialPairing {
        address: String,
    },
    /// Answers whether a waiting code matches the one on the other device.
    AnswerPairing {
        id: u64,
        accepted: bool,
    },
    /// Lists the nodes this one trusts or has revoked.
    Trust,
    /// Revokes a node, named by its id or its name; the rest of the mesh drops it too.
    RevokeNode {
        node: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first thing a node says to a command it accepts, so the command can check they match.
    Welcome {
        version: String,
    },
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
    /// The receiving device joined the mesh under this id.
    Joined {
        device: SessionId,
    },
    /// A file was offered to the receiving device.
    Incoming {
        offer: Offer,
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
    Preferences(Box<preferences::View>),
    Messages { messages: Vec<TextMessage> },
    Guests { guests: Vec<GuestSummary>, uploads: Vec<WaitingUpload> },
    Passphrase { passphrase: Option<String> },
    Received { name: String, path: PathBuf },
    Pairing(Box<pairing::View>),
    Trust { nodes: Vec<TrustedNodeView> },
    Acknowledged { message: String },
}

/// A file offered to a device the command line is receiving as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub file_id: String,
    pub name: String,
    /// For a folder, the size of all its files.
    pub size: u64,
    pub from: SessionId,
    pub from_name: String,
    /// Sent to this device alone, rather than a shared file another device pointed it to.
    pub is_direct: bool,
    /// The node's rule for incoming files lets it be saved without asking.
    pub is_accepted_without_asking: bool,
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
    /// A one-time invitation for a browser guest; none without an address to join at.
    pub guest: Option<GuestInvite>,
}

/// What a browser guest needs to join once: a link for the QR code and a code to type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestInvite {
    /// Carries the token and the certificate fingerprint in its fragment.
    pub join_url: String,
    pub code: String,
    pub permission: Permission,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub node_id: NodeId,
    pub node_name: String,
    pub local_ip: Option<IpAddr>,
    pub http_port: u16,
    // Plain HTTP on 127.0.0.1, which a window on this machine can open without a certificate warning.
    #[serde(default)]
    pub local_http_port: Option<u16>,
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
    pub local_http_port: Option<u16>,
    /// Whether the node's secrets live in the OS keychain, when there is one.
    pub use_keychain: bool,
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

    // Resolves once the command has gone away.
    async fn closed(&self) {
        self.0.closed().await;
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
        Request::Get { file, into } => save(state, &file, &into, events).await,
        Request::List => Ok(Reply::Files { files: state::file_views(state) }),
        Request::Devices => Ok(Reply::Devices { devices: devices(state).await, nodes: nodes(state).await }),
        Request::Invite { permission } => Ok(Reply::Invite(invite(state, permission))),
        Request::Status => Ok(Reply::Status(Box::new(status(state, settings).await))),
        Request::Preferences { update } => {
            if let Some(update) = update {
                preferences::apply(state, &update)?;
            }
            Ok(Reply::Preferences(Box::new(preferences::view(state))))
        }
        Request::Unshare { file } => unshare(state, &file).await,
        Request::Chat { message } => chat(state, message.as_deref()).await,
        Request::Guests => Ok(Reply::Guests { guests: state.guests.sessions.list(), uploads: guest::waiting_uploads(state) }),
        Request::SignOutGuest { guest } => sign_out_guest(state, &guest),
        Request::AnswerUpload { upload, accepted: true } => Ok(Reply::Shared { files: vec![guest::approve_upload(state, &upload).await?] }),
        Request::AnswerUpload { upload, accepted: false } => match guest::deny_upload(state, &upload) {
            true => Ok(acknowledged("The upload was thrown away")),
            false => bail!("no upload {upload:?} is waiting (see `ladex guests`)"),
        },
        Request::Passphrase => Ok(Reply::Passphrase { passphrase: state.passphrase.clone() }),
        Request::ChangePassphrase { passphrase } => {
            preferences::change_passphrase(state, settings.use_keychain, &passphrase)?;
            Ok(acknowledged("Saved; the node uses the new passphrase once it restarts"))
        }
        Request::Receive { name } => receiving::receive(state, &name, events).await,
        Request::AcceptOffer { device, file, into } => receiving::accept(state, &device, &file, &into, events).await,
        Request::DeclineOffer { device, file, from } => {
            crate::push::decline_offer(state, &device, &file, &from).await;
            Ok(acknowledged("Declined"))
        }
        Request::Pairing => Ok(Reply::Pairing(Box::new(pairing::view(state)))),
        Request::OpenPairing => {
            state.pairings.open();
            Ok(Reply::Pairing(Box::new(pairing::view(state))))
        }
        Request::ClosePairing => {
            state.pairings.close();
            Ok(acknowledged("Stopped accepting pairings"))
        }
        Request::DialPairing { address } => {
            let target = pairing::parse_address(&address, state.quic_port)
                .with_context(|| format!("{address:?} is not an address; give the other device's, like 192.168.1.20:8080"))?;
            pairing::dial(state, target);
            Ok(acknowledged(&format!("Dialing {target}")))
        }
        Request::AnswerPairing { id, accepted } => match state.pairings.answer(id, accepted) {
            true => Ok(acknowledged(if accepted { "Confirmed" } else { "Refused" })),
            false => bail!("that pairing is no longer waiting"),
        },
        Request::Trust => Ok(Reply::Trust { nodes: crate::trust::views(state).await? }),
        Request::RevokeNode { node } => revoke(state, &node).await,
    }
}

async fn revoke(state: &NodeState, wanted: &str) -> anyhow::Result<Reply> {
    let nodes = state.trust.list()?;
    let mut named: Vec<_> = nodes.iter().filter(|node| node.node_id == wanted || node.name.eq_ignore_ascii_case(wanted)).collect();
    let node = match named.len() {
        0 => bail!("this node does not know {wanted:?} (see `ladex trust`)"),
        1 => named.remove(0),
        _ => {
            let ids: Vec<&str> = named.iter().map(|node| node.node_id.as_str()).collect();
            bail!("{} nodes are called {wanted:?}; name one by its id: {}", named.len(), ids.join(", "))
        }
    };
    match crate::mesh::revoke_node(state, &node.node_id).await? {
        crate::mesh::Revoked::Now => Ok(acknowledged(&format!("Revoked {}; the rest of the mesh drops it too", node.name))),
        crate::mesh::Revoked::Already => Ok(acknowledged(&format!("{} was already revoked", node.name))),
        crate::mesh::Revoked::UnknownNode => bail!("this node does not know {wanted:?} (see `ladex trust`)"),
    }
}

fn acknowledged(message: &str) -> Reply {
    Reply::Acknowledged { message: message.to_string() }
}

async fn unshare(state: &NodeState, wanted: &str) -> anyhow::Result<Reply> {
    let file = find_file(state, wanted)?;
    if state::unshare(state, &file.id, share::SHARED_BY_THIS_NODE, true).await.is_empty() {
        bail!("{} was shared by another node; only that node can stop sharing it", file.name);
    }
    Ok(acknowledged(&format!("Stopped sharing {}", file.name)))
}

// The command line speaks in chat as this node, under an id of its own so its messages are told apart from a tab's.
async fn chat(state: &NodeState, message: Option<&str>) -> anyhow::Result<Reply> {
    if let Some(text) = message {
        let sender = format!("cli_{}", state.node_id);
        state::post_message(state, sender, Some(state.node_name.clone()), text).await.context("the message is empty")?;
    }
    Ok(Reply::Messages { messages: state.messages.read().await.clone() })
}

fn sign_out_guest(state: &NodeState, wanted: &str) -> anyhow::Result<Reply> {
    if !guest::revoke_guest(state, wanted) {
        bail!("no guest {wanted:?} is signed in (see `ladex guests`)");
    }
    Ok(acknowledged("The guest was signed out"))
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

async fn save(state: &NodeState, wanted: &str, into: &Path, events: &Events) -> anyhow::Result<Reply> {
    let file = find_file(state, wanted)?;
    let root = ReceiveRoot::open(into).with_context(|| format!("could not open {}", into.display()))?;
    let total = if file.is_folder { file.folder_bytes } else { file.size };
    let saving = receive::save_shared(state, &root, &file.id);
    let path = with_progress(saving, || fetched_bytes(state, &file), &file.name, total, events).await?;
    Ok(Reply::Saved { path, file })
}

/// Waits for `work`, telling the command how far it has got now and then.
async fn with_progress<T, E: std::error::Error + Send + Sync + 'static>(
    work: impl std::future::Future<Output = Result<T, E>>,
    done: impl Fn() -> u64,
    name: &str,
    total: u64,
    events: &Events,
) -> anyhow::Result<T> {
    tokio::pin!(work);
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    loop {
        tokio::select! {
            outcome = &mut work => return Ok(outcome?),
            _ = tick.tick() => events.progress(name, done(), total)?,
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

fn invite(state: &NodeState, permission: Permission) -> Invite {
    let issued = state.guests.invites.issue(permission);
    let guest = guest::invite_link(state, &issued).map(|join_url| GuestInvite {
        join_url,
        code: guest::invites::display_code(&issued.code),
        permission,
        expires_in_secs: issued.expires_in.as_secs(),
    });
    let scheme = if is_encrypted(state) { "https" } else { "http" };
    Invite {
        url: state.local_ip.map(|ip| format!("{scheme}://{ip}:{}", state.http_port)),
        certificate_fingerprint: is_encrypted(state).then(|| tls::format_fingerprint(&state.tls_fingerprint)),
        needs_passphrase: state.passphrase.is_some(),
        peer_address: state.local_ip.map(|ip| format!("{ip}:{}", state.quic_port)),
        guest,
    }
}

pub(crate) async fn status(state: &NodeState, settings: &Settings) -> Status {
    Status {
        version: VERSION.to_string(),
        node_id: state.node_id.clone(),
        node_name: state.node_name.clone(),
        local_ip: state.local_ip,
        http_port: state.http_port,
        local_http_port: settings.local_http_port,
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
