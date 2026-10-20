use crate::catalog::{EntryKind, Listed};
use crate::hlc::Stamp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub type SessionId = String;

/// A channel to one connected WebSocket, so the server can address a single peer.
pub type PeerSender = mpsc::UnboundedSender<ServerMessage>;
pub type PeerSenders = Arc<RwLock<HashMap<SessionId, PeerSender>>>;

/// Identifies a node (machine) on the mesh; distinct from a browser tab's `session_id`.
pub type NodeId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub session_id: SessionId,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub user_agent: Option<String>,
    /// Which node hosts this browser session.
    #[serde(default)]
    pub hosting_node_id: Option<NodeId>,
    /// Round-trip time (ms) from this node to the node hosting this peer; None until the first Pong.
    #[serde(default)]
    pub node_rtt_ms: Option<u32>,

    /// True once this peer has disconnected; the tombstone propagates across the mesh.
    #[serde(default)]
    pub left: bool,
    /// When `left` was set.
    #[serde(default)]
    pub left_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Hostname of the machine hosting this session.
    #[serde(default)]
    pub hosting_node_name: Option<String>,
    /// Nickname set by the user in the browser; overrides the User-Agent name.
    #[serde(default)]
    pub nickname: Option<String>,

    /// Orders updates to this peer across nodes (see hlc.rs).
    #[serde(default)]
    pub version: Stamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    pub passphrase: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub success: bool,
    pub message: Option<String>,
}

/// A shared file as a browser tab sees it: the catalog entry, who published it, and which nodes have it right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileView {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    /// The device that shared it (empty for none).
    pub uploader_id: SessionId,
    /// The node that published it.
    pub uploader_node: NodeId,
    /// Nodes that have the whole file and can serve it; for a folder, its listing and every file in it.
    pub hosts: Vec<NodeId>,
    /// Whether it can be downloaded now; a folder can be even when no single node holds all of it.
    #[serde(default)]
    pub available: bool,
    /// When it was shared, for display.
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
    pub created_at: u64,
    pub manifest_root: String,
    pub is_folder: bool,
    pub parent: Option<String>,
    /// For a folder: what is inside, for display. `size` is the listing's own size.
    pub folder_bytes: u64,
    pub folder_files: u32,
}

impl FileView {
    pub fn new(listed: &Listed, hosts: Vec<NodeId>) -> Self {
        let entry = &listed.entry;
        let (folder_bytes, folder_files) = match entry.kind {
            EntryKind::Folder { bytes, files } => (bytes, files),
            EntryKind::File { .. } => (0, 0),
        };
        Self {
            id: entry.file_id.clone(),
            name: entry.name.clone(),
            size: entry.size,
            mime_type: entry.mime_type.clone(),
            uploader_id: entry.uploader_id.clone(),
            uploader_node: listed.publisher.clone(),
            available: !hosts.is_empty(),
            hosts,
            uploaded_at: chrono::DateTime::from_timestamp_millis(entry.created_at_ms as i64).unwrap_or_default(),
            created_at: entry.created_at_ms,
            manifest_root: entry.manifest_root.clone(),
            is_folder: entry.is_folder(),
            parent: entry.parent().map(String::from),
            folder_bytes,
            folder_files,
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessage {
    pub id: String,
    pub content: String,
    pub sender_id: SessionId,
    pub sender_name: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Creation time (Unix ms); orders the chat.
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Peer announces itself to the network
    #[serde(rename = "join")]
    Join {
        session_id: SessionId,
        user_agent: Option<String>,
        #[serde(default)]
        nickname: Option<String>,
    },

    /// The user changed their nickname after joining.
    #[serde(rename = "set_nickname")]
    SetNickname { session_id: SessionId, nickname: String },

    #[serde(rename = "ping")]
    Ping { session_id: SessionId },

    #[serde(rename = "text_message")]
    TextMessage { session_id: SessionId, content: String },

    /// Unshares a file; only its uploader may.
    #[serde(rename = "delete_file")]
    DeleteFile { session_id: SessionId, file_id: String },

    /// Points one peer at a file; the recipient gets a consent prompt.
    #[serde(rename = "offer_file_to")]
    OfferFileTo { session_id: SessionId, target_session_id: SessionId, file_id: String },

    /// The offer's recipient declined it.
    #[serde(rename = "decline_file_offer")]
    DeclineFileOffer { session_id: SessionId, target_session_id: SessionId, file_id: String },

    /// The recipient of a file sent to this device alone accepts it; its node then fetches the file.
    #[serde(rename = "accept_file_offer")]
    AcceptFileOffer { session_id: SessionId, file_id: String },
}

/// What a tab is shown of a file sent to it alone, which is in no catalog it could look the file up in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedFile {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// A new peer joined the network
    #[serde(rename = "peer_joined")]
    PeerJoined { peer: PeerInfo, total_peers: usize },

    /// A peer left the network
    #[serde(rename = "peer_left")]
    PeerLeft { session_id: SessionId, total_peers: usize },

    /// Full file catalog (sent on join and whenever the catalog changes)
    #[serde(rename = "file_list_update")]
    FileListUpdate { files: Vec<FileView> },

    /// A single file was removed (host went offline, no remaining hosts)
    #[serde(rename = "file_removed")]
    FileRemoved { file_id: String },

    /// Incremental peer list update (e.g. an RTT change).
    #[serde(rename = "peer_sync")]
    PeerSync { peers: Vec<PeerInfo> },

    #[serde(rename = "error")]
    Error { message: String },

    #[serde(rename = "pong")]
    Pong,

    #[serde(rename = "text_message")]
    TextMessage { message: TextMessage },

    #[serde(rename = "message_history")]
    MessageHistory { messages: Vec<TextMessage> },

    /// No mesh peers found after 10 s, likely AP isolation; the tab shows a warning.
    #[serde(rename = "no_peers_warning")]
    NoPeersWarning { message: String },

    /// Someone offers to send this file directly; the tab asks for consent.
    #[serde(rename = "incoming_file_offer")]
    IncomingFileOffer {
        file_id: String,
        from_session_id: SessionId,
        /// Set when the file is sent to this device alone, and not when it is one from the catalog.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pushed: Option<OfferedFile>,
    },

    /// The peer we offered a file to declined it.
    #[serde(rename = "file_offer_declined")]
    FileOfferDeclined { file_id: String, from_session_id: SessionId },

    /// A file this tab sent to one device has arrived whole at that device's node.
    #[serde(rename = "file_push_delivered")]
    FilePushDelivered { file_id: String, to_session_id: SessionId },
}

/// System hostname, or a fallback label if it can't be read.
pub fn hostname() -> String {
    gethostname::gethostname().into_string().unwrap_or_else(|_| "LADEX Node".to_string())
}
