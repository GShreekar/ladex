use crate::hlc::Stamp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub type SessionId = String;

/// Per-peer sender channel — each connected WebSocket gets its own mpsc sender
/// so the server can route messages to a specific peer instead of broadcasting.
pub type PeerSender = mpsc::UnboundedSender<ServerMessage>;
pub type PeerSenders = Arc<RwLock<HashMap<SessionId, PeerSender>>>;

// ---------------------------------------------------------------------------
// Phase 1 — node identity
// ---------------------------------------------------------------------------

/// Identifies this node (machine) on the mesh.  Distinct from a browser tab's
/// `session_id` — every node generates one `node_id` on startup, regardless
/// of how many browser tabs connect to it locally.
///
/// In the current single-server phase (MESH_MODE=false) this is only used for
/// cookie auth invalidation; the mesh layer (Phase 3) uses it to route messages.
pub type NodeId = String;

// ---------------------------------------------------------------------------
// Peer / file / message data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub session_id: SessionId,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub user_agent: Option<String>,
    /// Which node (machine) hosts this browser session.
    /// Set to the local node_id when registering a local peer.
    /// Used in Phase 5 (decentralized signaling) to route WebRTC signals
    /// to the correct node without a central server.
    #[serde(default)]
    pub hosting_node_id: Option<NodeId>,
    /// Round-trip latency (ms) from THIS node to the node hosting this peer.
    /// Populated / updated by the Phase 6 Ping/Pong loop.
    /// `None` until at least one Pong is received.
    /// Exposed to browser tabs via PeerSync so clients can pick the fastest host.
    #[serde(default)]
    pub node_rtt_ms: Option<u32>,

    // ── BUG-08 fix: explicit departure tombstone ─────────────────────────
    // Mirrors FileMetadata's deleted/deleted_at (see below). Departure used
    // to be signalled by a PeerInfo with `hosting_node_id: None` and
    // `connected_at` set to `DateTime::MIN_UTC`, on the theory that a
    // "sentinel" PeerInfo would merge in like any other update — but
    // `merge_peers`'s LWW rule was `incoming.connected_at >
    // existing.connected_at`, and MIN_UTC can never be greater than a real
    // connection time. The departure marker silently lost that comparison
    // on every other mesh node forever, so those nodes kept treating a
    // long-gone browser tab as a live, routable peer (ghost peers).
    /// True when this peer has disconnected. Tombstones propagate across
    /// the mesh so all nodes stop treating this session as routable.
    #[serde(default)]
    pub left: bool,
    /// When `left` was set, used as the LWW key for departures instead of
    /// `connected_at` (always newer than the `connected_at` it's replacing,
    /// so it actually wins the merge).
    #[serde(default)]
    pub left_at: Option<chrono::DateTime<chrono::Utc>>,

    // ── F5: device names ─────────────────────────────────────────────────
    /// Hostname of the machine hosting this session (NodeState::node_name).
    /// Set by the hosting node itself; other nodes just carry it along.
    #[serde(default)]
    pub hosting_node_name: Option<String>,
    /// User-editable nickname, set client-side and persisted in
    /// localStorage. Overrides the User-Agent-derived name in the UI.
    #[serde(default)]
    pub nickname: Option<String>,

    /// Orders updates to this peer across nodes (see hlc.rs). Replaces the
    /// wall-clock `connected_at`/`left_at` as the merge key.
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

/// Whether one node has the complete file. Each node only ever changes its
/// own record, with a stamp from its own clock, so concurrent changes by
/// different nodes merge instead of overwriting each other.
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
    /// Device (browser session) that shared it; empty for files found on disk
    /// after a restart. Only that device, or the machine running the node it
    /// was shared through, may unshare it.
    pub uploader_id: SessionId,
    /// Node it was first shared through.
    #[serde(default)]
    pub uploader_node: NodeId,
    /// Nodes that have the complete file (and can serve it).
    #[serde(default)]
    pub holders: HashMap<NodeId, Holder>,
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
    /// Unix-millisecond wall-clock time of creation, for display only.
    /// Merging uses `version`.
    #[serde(default)]
    pub created_at: u64,

    /// Stamp of the latest change to this entry (create, delete). The entry
    /// with the greater stamp wins a merge; see hlc.rs. Holders merge
    /// separately, node by node.
    #[serde(default)]
    pub version: Stamp,

    // ── tombstone support ──────────────────────────────────────────────────
    /// True when this file has been deleted.  Tombstones propagate across the
    /// mesh so all nodes stop advertising the file and delete their copy.  The
    /// entry is pruned from memory after a while (see `state::prune_tombstones`).
    #[serde(default)]
    pub deleted: bool,
    /// Unix-millisecond wall-clock time of deletion, for display only.
    #[serde(default)]
    pub deleted_at: u64,

    /// Identifies the exact contents (size and every chunk hash, see store.rs).
    /// A node that fetches the file checks what it receives against it.
    #[serde(default)]
    pub manifest_root: Option<String>,

    /// A folder is a small JSON file listing its children (stored like any
    /// other file), which are themselves catalog entries with `parent` set.
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
    /// Unix-millisecond timestamp for LWW merge (Phase 4).
    #[serde(default)]
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------
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

    /// F5: user changed their nickname after already joining
    #[serde(rename = "set_nickname")]
    SetNickname {
        session_id: SessionId,
        nickname: String,
    },

    #[serde(rename = "ping")]
    Ping {
        session_id: SessionId,
    },

    #[serde(rename = "text_message")]
    TextMessage {
        session_id: SessionId,
        content: String,
    },

    /// F4: unshare a file. Only the original uploader may do this.
    #[serde(rename = "delete_file")]
    DeleteFile {
        session_id: SessionId,
        file_id: String,
    },

    /// F3: point one peer at a file it may want. Routed to
    /// `target_session_id`, who gets an IncomingFileOffer consent prompt.
    #[serde(rename = "offer_file_to")]
    OfferFileTo {
        session_id: SessionId,
        target_session_id: SessionId,
        file_id: String,
    },

    /// F3: the offer's recipient declined it — routed back to the sender.
    #[serde(rename = "decline_file_offer")]
    DeclineFileOffer {
        session_id: SessionId,
        target_session_id: SessionId,
        file_id: String,
    },
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// A new peer joined the network
    #[serde(rename = "peer_joined")]
    PeerJoined {
        peer: PeerInfo,
        total_peers: usize,
    },

    /// A peer left the network
    #[serde(rename = "peer_left")]
    PeerLeft {
        session_id: SessionId,
        total_peers: usize,
    },

    /// Full file catalog (sent on join and whenever the catalog changes)
    #[serde(rename = "file_list_update")]
    FileListUpdate {
        files: Vec<FileMetadata>,
    },

    /// A single file was removed (host went offline, no remaining hosts)
    #[serde(rename = "file_removed")]
    FileRemoved {
        file_id: String,
    },

    /// Phase 6: Incremental peer list update (e.g. RTT change).
    /// Browser tab merges these into its local peer map for host selection.
    #[serde(rename = "peer_sync")]
    PeerSync {
        peers: Vec<PeerInfo>,
    },

    // ── Misc ─────────────────────────────────────────────────────────────
    #[serde(rename = "error")]
    Error {
        message: String,
    },

    #[serde(rename = "pong")]
    Pong,

    #[serde(rename = "text_message")]
    TextMessage {
        message: TextMessage,
    },

    #[serde(rename = "message_history")]
    MessageHistory {
        messages: Vec<TextMessage>,
    },

    /// Phase 10 §10.4: AP isolation diagnostic — no mesh peers found after 10s.
    /// Browser tab surfaces a non-dismissible warning banner.
    #[serde(rename = "no_peers_warning")]
    NoPeersWarning {
        message: String,
    },

    /// F3: someone is offering to send this file directly — show a consent
    /// prompt. The client resolves file name/size/mime from its own
    /// already-synced catalog by `file_id`.
    #[serde(rename = "incoming_file_offer")]
    IncomingFileOffer {
        file_id: String,
        from_session_id: SessionId,
    },

    /// F3: the peer we offered a file to declined it.
    #[serde(rename = "file_offer_declined")]
    FileOfferDeclined {
        file_id: String,
        from_session_id: SessionId,
    },
}

/// System hostname, or a fallback label if it can't be read.
pub fn hostname() -> String {
    gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "LADEX Node".to_string())
}
