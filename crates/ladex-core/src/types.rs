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

/// Whether one node has the complete file; each node only changes its own record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub since: Stamp,
    pub present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    /// Device that shared the file (empty for files found on disk); only it, or the node's own machine, may unshare it.
    pub uploader_id: SessionId,
    /// Node it was first shared through.
    #[serde(default)]
    pub uploader_node: NodeId,
    /// Nodes that have the complete file (and can serve it).
    #[serde(default)]
    pub holders: HashMap<NodeId, Holder>,
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
    /// Creation time (Unix ms), for display only.
    #[serde(default)]
    pub created_at: u64,

    /// Stamp of the latest change; the greater stamp wins a merge (see hlc.rs).
    #[serde(default)]
    pub version: Stamp,

    /// True once the file has been unshared; the tombstone propagates across the mesh.
    #[serde(default)]
    pub deleted: bool,
    /// Unix-millisecond wall-clock time of deletion, for display only.
    #[serde(default)]
    pub deleted_at: u64,

    /// Identifies the exact contents (size and every chunk hash, see store.rs).
    #[serde(default)]
    pub manifest_root: Option<String>,

    /// A folder is a JSON listing of its children, which are catalog entries with `parent` set.
    #[serde(default)]
    pub is_folder: bool,
    #[serde(default)]
    pub parent: Option<String>,
    /// For a folder: what is inside, for display. `size` is the listing's own size.
    #[serde(default)]
    pub folder_bytes: u64,
    #[serde(default)]
    pub folder_files: u32,
}

impl FileMetadata {
    pub fn is_held_by(&self, node: &str) -> bool {
        self.holders.get(node).is_some_and(|h| h.present)
    }

    pub fn holder_nodes(&self) -> impl Iterator<Item = &NodeId> {
        self.holders.iter().filter(|(_, h)| h.present).map(|(node, _)| node)
    }

    pub fn set_holder(&mut self, node: &str, present: bool, since: Stamp) {
        self.holders.insert(node.to_string(), Holder { since, present });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    FileListUpdate { files: Vec<FileMetadata> },

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
    IncomingFileOffer { file_id: String, from_session_id: SessionId },

    /// The peer we offered a file to declined it.
    #[serde(rename = "file_offer_declined")]
    FileOfferDeclined { file_id: String, from_session_id: SessionId },
}

/// System hostname, or a fallback label if it can't be read.
pub fn hostname() -> String {
    gethostname::gethostname().into_string().unwrap_or_else(|_| "LADEX Node".to_string())
}
